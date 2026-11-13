//! The collect-write actions (`WriteEmail`, `WriteEvent`, ...) and their
//! calendar helpers (SOUL §10/§28).

use catalerum_automation::{Action, ActionOutcome};
use catalerum_core::capability::{Action as CapAction, Capability, Resource};
use catalerum_core::model::{Calendar, Email, Event, Grant};
use catalerum_core::{CalendarId, MailboxId, WorkspaceId};
use catalerum_store::Store;
use serde_json::{json, Value};

use super::templating::render_params;
use super::{cap_allows, ToolActionRunner};

/// The collect-write actions (SOUL §10/§28).
impl ToolActionRunner {
    /// `WriteEmail` — persist a collected message (idempotent upsert by
    /// `(mailbox_id, uid)`) and enqueue the §10 chunk→embed→project pipeline. The
    /// message rides on the firing `CollectEmail` trigger. Gated `email:write`; the
    /// output carries the stored email's id/uid so the collect finalizer can key its
    /// cursor commit on this node.
    pub(super) async fn run_write_email(
        &self,
        workspace_id: WorkspaceId,
        _action: &Action,
        trigger: Option<&Value>,
        grant: Option<&Grant>,
    ) -> ActionOutcome {
        let Some(store) = self.store.clone() else {
            return ActionOutcome::failed("write_email needs a store-backed runner");
        };
        let ctx = match self.resolve_context(workspace_id, grant).await {
            Ok(c) => c,
            Err(e) => return ActionOutcome::failed(e),
        };
        if !cap_allows(
            &ctx,
            &Capability::new(CapAction::Write, Resource::domain("email")),
        ) {
            return ActionOutcome::failed("write_email denied: requires email:write authority");
        }
        let Some(item) = collect_item(trigger) else {
            return ActionOutcome::failed(
                "write_email: no email on the trigger — place it downstream of a CollectEmail trigger",
            );
        };
        let mut email: Email = match serde_json::from_value(item.clone()) {
            Ok(e) => e,
            Err(e) => {
                return ActionOutcome::failed(format!(
                    "write_email: trigger item is not an email: {e}"
                ))
            }
        };
        // The write target is governed by the **action's** authority scope, never the
        // (deserialized) item's — force the workspace so a crafted item can't land in
        // another tenant (defense-in-depth; the collect job already scopes correctly).
        email.workspace_id = workspace_id;
        if ctx.dry_run {
            return ActionOutcome::succeeded(Some(json!({ "dry_run": true, "uid": email.uid })));
        }
        // Idempotent-redelivery signal (SOUL §11/§29): probe whether this
        // `(mailbox_id, uid)` is **already stored** before the upsert. `newly_written`
        // is `false` when the message was already persisted — an at-least-once
        // REDELIVERY (the crash window between an earlier run's write and its ledger
        // commit, or a snapshot provider re-emitting). The DAG executor latches this
        // into a per-run `redelivery` flag, so the non-idempotent nodes downstream of
        // this write (a classifying `LlmAgent`, a `LabelEmail`) auto-**skip** instead of
        // double-firing (no double-spent tokens, no re-label) — while this *idempotent*
        // write still runs so its success advances the collect cursor (`commit_on`).
        // Read the store, not the ledger: the row survives a crash the ledger doesn't.
        let newly_written = match store
            .emails()
            .get_by_uid(workspace_id, email.mailbox_id, &email.uid)
            .await
        {
            Ok(_) => false,
            Err(catalerum_store::StoreError::NotFound) => true,
            // A probe error is not fatal — treat it as "assume new" and let the upsert
            // (which hits the same store) surface any real failure, rather than
            // wrongly suppressing a first-delivery classify.
            Err(e) => {
                tracing::warn!(error = %e, uid = %email.uid, "write_email: newly-written probe failed; assuming new");
                true
            }
        };
        let stored = match store.emails().upsert_by_uid(&email).await {
            Ok(e) => e,
            Err(e) => return ActionOutcome::failed(format!("write_email upsert: {e}")),
        };
        // The §10 projection is derived/rebuildable, so a failed enqueue is logged,
        // not fatal — the email is already in Postgres truth and re-projects on a
        // later write of the same uid.
        if let Err(e) =
            catalerum_ingest::enqueue_ingest_email(&store, workspace_id, stored.id).await
        {
            tracing::warn!(error = %e, email = %stored.id, "write_email: failed to enqueue ingest_email projection");
        }
        ActionOutcome::succeeded(Some(json!({
            "email_id": stored.id,
            "mailbox_id": stored.mailbox_id,
            "uid": stored.uid,
            "newly_written": newly_written,
        })))
    }

    /// `WriteEvent` — the calendar twin of [`run_write_email`](Self::run_write_email):
    /// idempotent upsert of a collected event by `(calendar_id, uid)` + enqueue the
    /// event graph projection. An optional `calendar_id` param redirects the write
    /// into a specific **local** calendar instead of the collect source's mirror.
    /// Gated `calendar:write`.
    pub(super) async fn run_write_event(
        &self,
        workspace_id: WorkspaceId,
        action: &Action,
        trigger: Option<&Value>,
        grant: Option<&Grant>,
    ) -> ActionOutcome {
        let Some(store) = self.store.clone() else {
            return ActionOutcome::failed("write_event needs a store-backed runner");
        };
        let ctx = match self.resolve_context(workspace_id, grant).await {
            Ok(c) => c,
            Err(e) => return ActionOutcome::failed(e),
        };
        if !cap_allows(
            &ctx,
            &Capability::new(CapAction::Write, Resource::domain("calendar")),
        ) {
            return ActionOutcome::failed("write_event denied: requires calendar:write authority");
        }
        let Some(item) = collect_item(trigger) else {
            return ActionOutcome::failed(
                "write_event: no event on the trigger — place it downstream of a CollectCalendar trigger",
            );
        };
        let mut event: Event = match serde_json::from_value(item.clone()) {
            Ok(e) => e,
            Err(e) => {
                return ActionOutcome::failed(format!(
                    "write_event: trigger item is not an event: {e}"
                ))
            }
        };
        // Scope the write to the action's workspace, not the item's (defense-in-depth).
        event.workspace_id = workspace_id;
        // A dry run must not touch the store — return before the redirect below
        // resolves (and may create) a calendar or the probe reads for redelivery.
        if ctx.dry_run {
            return ActionOutcome::succeeded(Some(json!({ "dry_run": true, "uid": event.uid })));
        }
        // Optional (templatable) redirect into a specific **local** calendar,
        // overriding the collect source's mirror. Two param spellings, both
        // accepting a name-or-id: `calendar_id` (historically a UUID) and
        // `calendar` (a calendar name). A name is get-or-created idempotently, so
        // an automation authored against a not-yet-existing calendar — e.g.
        // "Feiertage Bayern" for a Bavaria-holidays ICS collect — writes into a
        // freshly-made local calendar of that name instead of silently landing in
        // the collect source's mirror. Local calendars only: WriteEvent is a
        // store-mirror write that never loops back to a provider (SOUL §8), so a
        // row planted in a provider calendar's mirror would silently vanish on its
        // next snapshot sync.
        let rendered = render_params(&action.params, trigger);
        let target = rendered
            .get("calendar_id")
            .or_else(|| rendered.get("calendar"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(arg) = target {
            match resolve_write_calendar(&store, workspace_id, arg).await {
                Ok(calendar) => event.calendar_id = calendar.id,
                Err(e) => return ActionOutcome::failed(format!("write_event: {e}")),
            }
        }
        // Idempotent-redelivery signal (SOUL §11/§29) — the calendar twin of the
        // `WriteEmail` probe: read the store for this `(calendar_id, uid)` **before** the
        // upsert. `newly_written` is `false` when the event was already persisted (an
        // at-least-once redelivery), which the DAG executor latches into a per-run
        // `redelivery` flag so any downstream non-idempotent node (a `CollectCalendar →
        // LlmAgent` classify) auto-skips instead of double-firing — while this idempotent
        // write still runs so its success advances the collect cursor (`commit_on`). Read
        // the store, not the ledger: the row survives a crash the ledger doesn't.
        let newly_written = match store
            .events()
            .get_by_uid(workspace_id, event.calendar_id, &event.uid)
            .await
        {
            Ok(_) => false,
            Err(catalerum_store::StoreError::NotFound) => true,
            // A probe error is not fatal — assume new and let the upsert (same store)
            // surface any real failure, rather than wrongly suppressing a first classify.
            Err(e) => {
                tracing::warn!(error = %e, uid = %event.uid, "write_event: newly-written probe failed; assuming new");
                true
            }
        };
        let upsert = catalerum_ingest::event_to_upsert(&event);
        let stored = match store.events().upsert_by_uid(&upsert).await {
            Ok(e) => e,
            Err(e) => return ActionOutcome::failed(format!("write_event upsert: {e}")),
        };
        if let Err(e) =
            catalerum_ingest::enqueue_project_event(&store, workspace_id, stored.id).await
        {
            tracing::warn!(error = %e, event = %stored.id, "write_event: failed to enqueue project_event");
        }
        ActionOutcome::succeeded(Some(json!({
            "event_id": stored.id,
            "calendar_id": stored.calendar_id,
            "uid": stored.uid,
            "newly_written": newly_written,
        })))
    }

    /// `LabelEmail` — record a classifier verdict on a **stored** email (SOUL
    /// §11/§28): a full replace of its free-text `labels`. The target is resolved
    /// from the firing trigger's `(mailbox_id, uid)` (so it labels the message an
    /// upstream `WriteEmail` persisted); `labels` come from the action `params`
    /// (templatable, e.g. `{ "labels": "{{ inputs.classify.labels }}" }`). Idempotent;
    /// gated `email:write`.
    pub(super) async fn run_label_email(
        &self,
        workspace_id: WorkspaceId,
        action: &Action,
        trigger: Option<&Value>,
        grant: Option<&Grant>,
    ) -> ActionOutcome {
        let Some(store) = self.store.clone() else {
            return ActionOutcome::failed("label_email needs a store-backed runner");
        };
        let ctx = match self.resolve_context(workspace_id, grant).await {
            Ok(c) => c,
            Err(e) => return ActionOutcome::failed(e),
        };
        if !cap_allows(
            &ctx,
            &Capability::new(CapAction::Write, Resource::domain("email")),
        ) {
            return ActionOutcome::failed("label_email denied: requires email:write authority");
        }
        // `labels` may be templated from an upstream classifier's output.
        let rendered = render_params(&action.params, trigger);
        let mut seen = std::collections::HashSet::new();
        let labels: Vec<String> = rendered
            .get("labels")
            .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            // Order-preserving de-dup of the whole set (Vec::dedup only drops
            // consecutive repeats — a classifier may emit ["a","b","a"]).
            .filter(|s| seen.insert(s.clone()))
            .collect();
        if labels.is_empty() {
            return ActionOutcome::failed("label_email: `labels` must be a non-empty string array");
        }
        let Some(item) = collect_item(trigger) else {
            return ActionOutcome::failed(
                "label_email: no email on the trigger — it labels the message an upstream WriteEmail wrote",
            );
        };
        let Some(mailbox_id) = item
            .get("mailbox_id")
            .and_then(|v| serde_json::from_value::<MailboxId>(v.clone()).ok())
        else {
            return ActionOutcome::failed("label_email: trigger item is missing a mailbox_id");
        };
        let Some(uid) = item.get("uid").and_then(Value::as_str) else {
            return ActionOutcome::failed("label_email: trigger item is missing a uid");
        };
        if ctx.dry_run {
            return ActionOutcome::succeeded(Some(json!({ "dry_run": true, "labels": labels })));
        }
        let email = match store
            .emails()
            .get_by_uid(workspace_id, mailbox_id, uid)
            .await
        {
            Ok(e) => e,
            Err(e) => {
                return ActionOutcome::failed(format!(
                    "label_email: target email not found (write it first with WriteEmail): {e}"
                ))
            }
        };
        match store
            .emails()
            .set_labels(workspace_id, email.id, &labels)
            .await
        {
            Ok(updated) => ActionOutcome::succeeded(Some(json!({
                "email_id": updated.id,
                "labels": updated.labels,
            }))),
            Err(e) => ActionOutcome::failed(format!("label_email set_labels: {e}")),
        }
    }

    /// `MarkEmailRead` — set a **stored** email's local `seen` flag (SOUL
    /// §11/§28), or clear it with `"unread": true`. The target is resolved from
    /// the firing trigger's `(mailbox_id, uid)` like `LabelEmail`. **Local
    /// only** (§14): the provider's mailbox is never written, so a provider
    /// re-sync may overwrite it. Idempotent; gated `email:write`.
    pub(super) async fn run_mark_email_read(
        &self,
        workspace_id: WorkspaceId,
        action: &Action,
        trigger: Option<&Value>,
        grant: Option<&Grant>,
    ) -> ActionOutcome {
        let Some(store) = self.store.clone() else {
            return ActionOutcome::failed("mark_email_read needs a store-backed runner");
        };
        let ctx = match self.resolve_context(workspace_id, grant).await {
            Ok(c) => c,
            Err(e) => return ActionOutcome::failed(e),
        };
        if !cap_allows(
            &ctx,
            &Capability::new(CapAction::Write, Resource::domain("email")),
        ) {
            return ActionOutcome::failed("mark_email_read denied: requires email:write authority");
        }
        // `unread` may be templated (e.g. from an upstream classifier's verdict).
        let rendered = render_params(&action.params, trigger);
        let unread = rendered
            .get("unread")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let Some(item) = collect_item(trigger) else {
            return ActionOutcome::failed(
                "mark_email_read: no email on the trigger — it marks the message an upstream WriteEmail wrote",
            );
        };
        let Some(mailbox_id) = item
            .get("mailbox_id")
            .and_then(|v| serde_json::from_value::<MailboxId>(v.clone()).ok())
        else {
            return ActionOutcome::failed("mark_email_read: trigger item is missing a mailbox_id");
        };
        let Some(uid) = item.get("uid").and_then(Value::as_str) else {
            return ActionOutcome::failed("mark_email_read: trigger item is missing a uid");
        };
        if ctx.dry_run {
            return ActionOutcome::succeeded(Some(json!({ "dry_run": true, "unread": unread })));
        }
        let email = match store
            .emails()
            .get_by_uid(workspace_id, mailbox_id, uid)
            .await
        {
            Ok(e) => e,
            Err(e) => {
                return ActionOutcome::failed(format!(
                    "mark_email_read: target email not found (write it first with WriteEmail): {e}"
                ))
            }
        };
        match store
            .emails()
            .set_seen(workspace_id, email.id, !unread)
            .await
        {
            Ok(updated) => ActionOutcome::succeeded(Some(json!({
                "email_id": updated.id,
                "unread": unread,
            }))),
            Err(e) => ActionOutcome::failed(format!("mark_email_read set_seen: {e}")),
        }
    }
}

/// The collected item carried on a collect trigger's firing event: in a graph run
/// the runtime ctx is `{ "trigger": <event>, "inputs": … }`, so the item is at
/// `trigger.item`; in the linear loop the ctx *is* the event, so it is at `item`.
fn collect_item(trigger: Option<&Value>) -> Option<&Value> {
    let t = trigger?;
    if let Some(item) = t.get("trigger").and_then(|e| e.get("item")) {
        return Some(item);
    }
    t.get("item")
}

/// A stable, workspace-unique `external_id` for a *named* local calendar (SOUL
/// §8) — a slug of the name so [`CalendarRepo::upsert_local`] get-or-creates the
/// same row on every `WriteEvent` run for the same name (its local unique index
/// keys on `external_id`). The `named-` prefix keeps it out of the reserved
/// `default` key (the auto-provisioned default local calendar), so asking to
/// write to a calendar literally named "default" never renames that one.
pub(super) fn local_calendar_external_id(name: &str) -> String {
    let slug: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    format!("named-{}", if slug.is_empty() { "cal" } else { slug })
}

/// Get-or-create a writable **local** calendar by name (case-insensitive, SOUL
/// §8). An existing local calendar of that name is reused; otherwise a new one
/// is created, keyed on [`local_calendar_external_id`]. This is the single
/// get-or-create path shared by the `create_calendar` tool, `POST /calendars`,
/// and a `WriteEvent` name redirect — so asking for the same calendar name more
/// than once (a re-run, a double-submit, an agent retry) converges on one row
/// instead of minting differently-keyed duplicate calendars.
pub(crate) async fn get_or_create_local_calendar_by_name(
    store: &Store,
    workspace_id: WorkspaceId,
    name: &str,
) -> std::result::Result<Calendar, String> {
    let name = name.trim();
    let existing = store
        .calendars()
        .list_by_workspace(workspace_id)
        .await
        .map_err(|e| format!("listing calendars: {e}"))?;
    if let Some(cal) = existing
        .into_iter()
        .find(|c| c.is_local() && c.name.trim().eq_ignore_ascii_case(name))
    {
        return Ok(cal);
    }
    store
        .calendars()
        .upsert_local(workspace_id, &local_calendar_external_id(name), name)
        .await
        .map_err(|e| format!("creating local calendar `{name}`: {e}"))
}

/// Resolve a `WriteEvent` redirect target to a writable **local** calendar,
/// accepting either a calendar id (UUID) or a calendar **name**. A bare UUID is
/// looked up and must be local (a provider calendar is refused — its mirror is
/// overwritten by the next sync, SOUL §8). Any other string is a name: an
/// existing local calendar of that name (case-insensitive) is reused, else one
/// is created (get-or-create via [`local_calendar_external_id`]), so an
/// automation can target a calendar that does not exist yet.
async fn resolve_write_calendar(
    store: &Store,
    workspace_id: WorkspaceId,
    arg: &str,
) -> std::result::Result<Calendar, String> {
    // A well-formed UUID is always an id, never a name.
    if let Ok(id) = arg.parse::<CalendarId>() {
        let calendar = store
            .calendars()
            .get(workspace_id, id)
            .await
            .map_err(|e| format!("calendar {id} not found in this workspace: {e}"))?;
        if !calendar.is_local() {
            return Err(
                "`calendar_id` must name a local calendar — a provider calendar's mirror is \
                 overwritten by its next sync"
                    .to_string(),
            );
        }
        return Ok(calendar);
    }
    // A name: get-or-create a local calendar of that name, so a calendar the
    // user made via `create_calendar` (or a prior WriteEvent) isn't duplicated
    // by a differently-keyed twin.
    get_or_create_local_calendar_by_name(store, workspace_id, arg).await
}
