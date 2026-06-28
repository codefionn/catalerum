//! The organisation → workspace switcher and org-management surface (SOUL §18, §12).
//!
//! The header control switches the active workspace and, above it, exposes the
//! organisation that groups workspaces. Behaviour follows the deployment mode
//! (`GET /status.mode`) — presentation only; the server enforces every action:
//!
//! - The `<select>` lists the caller's workspaces (`GET /workspaces`) grouped by
//!   organisation (`GET /organisations` resolves the group names; a 403/error
//!   degrades to a single flat list). Selecting a different workspace mints a
//!   session bound to it (`POST /auth/switch`), adopts the new bearer, and reloads
//!   so every panel re-fetches under the new workspace. It renders only when there
//!   is more than one workspace to switch between.
//! - An org-management button opens the **organisations manager** — a two-pane
//!   modal with an organisation rail on the left (plus a "New organisation" entry)
//!   and the selected org's detail on the right: its workspaces (switch / archive /
//!   restore), the create-workspace flow, and — in `multi_user`, for an org the
//!   caller administers — the members/policy admin panel and the owner-only danger
//!   zone. In `single_user` creation is shown plainly (any member); in `multi_user`
//!   the manager is visible only when the caller administers some organisation
//!   (owner/admin). Creation POSTs are attempted and a `403` is surfaced as a
//!   friendly "not permitted" notice — the client never predicts policy.

use leptos::prelude::*;
use leptos::task::spawn_local;

use super::dialogs::{use_dialogs, ConfirmSpec};
use crate::api::{
    AddOrgMember, CreateOrg, CreateOrgWorkspace, MyOrganisation, MyWorkspace, OrgMemberView,
    SetOrgPolicy, WorkspaceMembership, WorkspaceShell,
};
use crate::auth;
use crate::components::icons::{Icon, MdIcon};
use crate::rest;
use crate::rest::RestError;

// ---------------------------------------------------------------------------
// Pure presentation helpers (unit-tested below — no reactive/DOM dependency).
// ---------------------------------------------------------------------------

/// A group of workspaces under one organisation, for the grouped switcher. A
/// group whose `name` is empty renders as bare (ungrouped) options — the flat
/// fallback when org names are unavailable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrgGroup {
    /// Organisation id (empty for the fallback bucket).
    pub org_id: String,
    /// Display name, or empty to render bare options with no `<optgroup>` header.
    pub name: String,
    /// The workspaces in this group, in their input order.
    pub workspaces: Vec<WorkspaceMembership>,
}

/// Group the caller's `workspaces` by organisation, resolving each org's display
/// name from `orgs` (the `/organisations` listing). Named groups come first, in
/// `orgs` order (stable switcher grouping), and any workspace whose organisation
/// is absent/unknown falls into a trailing unnamed bucket. An empty `orgs` (a
/// 403 / error) collapses everything into that one unnamed bucket — i.e. a flat
/// list, the graceful fallback.
pub fn group_workspaces_by_org(
    workspaces: &[WorkspaceMembership],
    orgs: &[MyOrganisation],
) -> Vec<OrgGroup> {
    let mut groups: Vec<OrgGroup> = Vec::new();

    // Named groups, in org order, keeping only orgs that actually hold a visible
    // workspace (so an org with nothing to switch to adds no empty header).
    for org in orgs {
        let mine: Vec<WorkspaceMembership> = workspaces
            .iter()
            .filter(|w| w.organisation_id == org.id)
            .cloned()
            .collect();
        if mine.is_empty() {
            continue;
        }
        let name = if org.name.trim().is_empty() {
            org.slug.clone()
        } else {
            org.name.clone()
        };
        groups.push(OrgGroup {
            org_id: org.id.clone(),
            name,
            workspaces: mine,
        });
    }

    // The trailing fallback bucket: workspaces whose org id matched no named group.
    let known: std::collections::HashSet<&str> = orgs.iter().map(|o| o.id.as_str()).collect();
    let orphans: Vec<WorkspaceMembership> = workspaces
        .iter()
        .filter(|w| !known.contains(w.organisation_id.as_str()))
        .cloned()
        .collect();
    if !orphans.is_empty() {
        groups.push(OrgGroup {
            org_id: String::new(),
            name: String::new(),
            workspaces: orphans,
        });
    }

    groups
}

/// The multi-user deployment mode (`multi_user`). Anything else (incl. an absent
/// mode defaulted to `single_user`) is the leaner single-user presentation.
pub fn is_multi_user(mode: &str) -> bool {
    mode.trim() == "multi_user"
}

/// Whether an org role token grants administration (owner/admin). Presentation
/// only — the server re-checks every org action.
pub fn is_org_admin_role(role: &str) -> bool {
    matches!(role.trim(), "owner" | "admin")
}

/// Whether an org role token is **Owner** (the stricter gate for structural
/// actions like deleting the organisation). Presentation only — the server
/// re-checks and is the sole authority (`DELETE /organisations/{id}` is
/// owner-only, SOUL §18).
pub fn is_org_owner_role(role: &str) -> bool {
    role.trim() == "owner"
}

/// A workspace-created confirmation, tagged with the org it belongs to. The notice
/// is held **above** the per-org panel (which remounts when the post-create list
/// refresh refetches the org list) so it survives that refresh — yet it renders
/// only under the org it refers to, so switching orgs hides a stale notice without
/// discarding it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceNotice {
    /// The org the created workspace lives in.
    pub org_id: String,
    /// The confirmation text to show.
    pub message: String,
}

/// The notice text to show under `org_id`'s panel, given the lifted notice (held by
/// a parent that survives the panel remount). `None` unless the held notice targets
/// this org — so switching to a different org hides it without clearing it.
pub fn notice_for_org<'a>(notice: &'a Option<WorkspaceNotice>, org_id: &str) -> Option<&'a str> {
    notice
        .as_ref()
        .filter(|n| n.org_id == org_id)
        .map(|n| n.message.as_str())
}

/// Whether the caller administers **any** organisation — the multi-user gate for
/// the org admin surface (fail-closed to hidden if they administer none).
pub fn is_org_admin_somewhere(orgs: &[MyOrganisation]) -> bool {
    orgs.iter().any(|o| is_org_admin_role(&o.role))
}

/// How the "Delete organisation" affordance should present. Deletion is owner-only
/// and the server 409s if the org holds *any* workspace — live **or archived**. The
/// org-admin **shell** listing (which includes archived shells) is the only
/// client-side view that can see archived ones, so the affordance keys off it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrgDeleteAffordance {
    /// Don't offer deletion (not an owner, or — when the shell listing isn't
    /// available — the caller-visible workspaces show the org is non-empty).
    Hidden,
    /// Offer an enabled Delete button (the server re-checks and stays authoritative).
    Enabled,
    /// Offer the button **disabled**: the shell listing shows the org still holds a
    /// workspace (archived ones count), which can only be archived, never deleted.
    DisabledHasWorkspaces,
}

/// Decide the delete-organisation affordance from the owner check, the
/// caller-visible (live) workspace emptiness, and the admin **shell** count
/// (`Some(n)` once that listing — which includes archived shells — has loaded,
/// `None` while it is unavailable: a non-admin owner, a `403`, an error, or before
/// the first fetch). When the listing is available it is authoritative (any
/// workspace ⇒ disabled, empty ⇒ enabled). When it isn't we fall back to the
/// caller-visible view and let the server stay the authority: an org that still
/// looks non-empty hides the button, an empty-looking one offers it and the server
/// 409s if an archived shell lurks.
#[must_use]
pub fn org_delete_affordance(
    is_owner: bool,
    visible_workspaces_empty: bool,
    shell_count: Option<usize>,
) -> OrgDeleteAffordance {
    if !is_owner {
        return OrgDeleteAffordance::Hidden;
    }
    match shell_count {
        Some(0) => OrgDeleteAffordance::Enabled,
        Some(_) => OrgDeleteAffordance::DisabledHasWorkspaces,
        None if visible_workspaces_empty => OrgDeleteAffordance::Enabled,
        None => OrgDeleteAffordance::Hidden,
    }
}

/// Whether to surface the org-management button in the header. In `single_user`
/// the create affordances are shown plainly (always). In `multi_user` they are
/// tucked behind the org admin surface — visible only to an org admin/owner.
pub fn show_org_button(mode: &str, orgs: &[MyOrganisation]) -> bool {
    if is_multi_user(mode) {
        is_org_admin_somewhere(orgs)
    } else {
        true
    }
}

/// How an add-member input resolves. A canonical UUID is used as a user id
/// directly; anything else is treated as an email to resolve through the org-gated
/// `user-lookup` route before adding. Blank input never reaches here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemberInput {
    /// A user id typed / pasted directly.
    UserId(String),
    /// An email address to resolve to a user id.
    Email(String),
}

/// Classify raw add-member input (the user's typed text). `None` for blank input;
/// a canonical `8-4-4-4-12` UUID → [`MemberInput::UserId`]; anything else →
/// [`MemberInput::Email`]. Purely syntactic — the server still authorises the add
/// and is the sole resolver of the email → user id mapping.
pub fn classify_member_input(raw: &str) -> Option<MemberInput> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if is_canonical_uuid(s) {
        Some(MemberInput::UserId(s.to_string()))
    } else {
        Some(MemberInput::Email(s.to_string()))
    }
}

/// Whether `s` is a canonical hyphenated UUID (`8-4-4-4-12` lowercase-or-upper
/// hex). Kept dependency-free (no `uuid` crate) — this is only a syntactic split
/// between "looks like an id" and "treat as an email".
fn is_canonical_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    b.iter().enumerate().all(|(i, &c)| match i {
        8 | 13 | 18 | 23 => c == b'-',
        _ => c.is_ascii_hexdigit(),
    })
}

/// Suggest a URL-safe slug from a display name: lowercased, ASCII alphanumerics
/// kept, every other run of characters collapsed to a single `-`, trimmed. Only a
/// convenience prefill — the field stays editable and the server remains the slug
/// authority (uniqueness, casing).
pub fn suggest_slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending_dash = false;
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c);
        } else {
            pending_dash = true;
        }
    }
    out
}

/// How a workspace row's switch affordance presents, from the caller's switcher
/// memberships (`GET /workspaces`, which carries the `active` flag).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WsSwitch {
    /// This is the session's current workspace — show a "current" badge, no button.
    Current,
    /// The caller is a member and can switch into it.
    Available,
    /// The caller is not a member (an org admin administering a shell they can't
    /// enter) — no switch affordance; org roles confer no data access (SOUL §18).
    NotMember,
}

/// Decide a workspace row's switch affordance by looking the workspace up in the
/// caller's memberships. Presentation only — `POST /auth/switch` re-checks
/// membership (and refuses archived workspaces) server-side.
pub fn ws_switch_state(memberships: &[WorkspaceMembership], ws_id: &str) -> WsSwitch {
    match memberships.iter().find(|m| m.id == ws_id) {
        Some(m) if m.active => WsSwitch::Current,
        Some(_) => WsSwitch::Available,
        None => WsSwitch::NotMember,
    }
}

/// How many workspace rows the shell panel renders per bucket (live / archived)
/// before truncating with a "…N more" note. An org can legitimately hold very
/// many shells (the shared dev org accumulates thousands from test runs); the
/// filter box narrows, the cap keeps the DOM sane.
pub const WS_ROW_CAP: usize = 50;

/// How many shells (total) it takes for the filter box to appear.
pub const WS_FILTER_THRESHOLD: usize = 8;

/// Whether a shell matches the filter `query` — case-insensitive substring on
/// the name or the slug; a blank query matches everything.
pub fn shell_matches(shell: &WorkspaceShell, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    shell.name.to_lowercase().contains(&q) || shell.slug.to_lowercase().contains(&q)
}

/// Truncate `rows` to the render cap, returning the shown rows and how many were
/// hidden (0 when everything fits).
pub fn cap_rows<T>(mut rows: Vec<T>) -> (Vec<T>, usize) {
    let hidden = rows.len().saturating_sub(WS_ROW_CAP);
    rows.truncate(WS_ROW_CAP);
    (rows, hidden)
}

/// Order live shell rows so the workspaces that matter to the caller surface
/// first — the session's current workspace, then other workspaces they are a
/// member of, then shells they merely administer — stable (server order) within
/// each group. Without this, a large org (the shared dev org holds thousands of
/// test shells) buries the caller's own workspaces past the render cap.
pub fn order_live_shells(
    mut rows: Vec<WorkspaceShell>,
    memberships: &[WorkspaceMembership],
) -> Vec<WorkspaceShell> {
    rows.sort_by_key(|s| match ws_switch_state(memberships, &s.id) {
        WsSwitch::Current => 0_u8,
        WsSwitch::Available => 1,
        WsSwitch::NotMember => 2,
    });
    rows
}

/// The one-character avatar initial for a member row: the first alphanumeric of
/// the display name, falling back to the email, falling back to `?` — uppercased.
pub fn member_initial(display_name: &str, email: &str) -> String {
    display_name
        .chars()
        .chain(email.chars())
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// Render a REST error as a friendly notice. A `403` surfaces the server's own
/// (already friendly) verdict when present, else a generic not-permitted line —
/// the client never predicts policy, it only relays the server's refusal.
fn friendly_error(err: &RestError, action: &str) -> String {
    match err {
        RestError::Status {
            status: 403,
            message,
        } if !message.trim().is_empty() => message.clone(),
        RestError::Status { status: 403, .. } => {
            format!("{action} is not permitted on this instance or organisation.")
        }
        // A 409 carries the server's own precondition verdict (e.g. "the default
        // organisation cannot be deleted"); surface it verbatim, no HTTP-code suffix.
        RestError::Status {
            status: 409,
            message,
        } if !message.trim().is_empty() => message.clone(),
        other => other.to_string(),
    }
}

/// The org roles offered in the add-member picker (`token`, label).
const ORG_ROLES: [(&str, &str); 3] = [("member", "Member"), ("admin", "Admin"), ("owner", "Owner")];

/// The workspace-creation policies offered in the policy picker (`token`, label).
const WS_POLICIES: [(&str, &str); 3] = [
    ("members", "Any member"),
    ("admins", "Admins only"),
    ("disabled", "Disabled"),
];

/// Switch the session into `ws_id`: mint a workspace-bound session and adopt it
/// (the page reloads and every panel re-fetches under the new scope). On failure
/// the busy latch is released and the error surfaced through `on_err` (empty
/// closure for the header switcher, which just re-enables).
fn spawn_switch(ws_id: String, busy: RwSignal<bool>, on_err: impl Fn(String) + 'static) {
    busy.set(true);
    spawn_local(async move {
        let token = auth::resolve_token();
        match rest::switch_workspace(token.as_deref(), &ws_id).await {
            // Adopt the new bearer + reload; the page navigates away here.
            Ok(resp) => auth::adopt_token_and_reload(&resp.token),
            Err(e) => {
                busy.set(false);
                on_err(friendly_error(&e, "Switching workspace"));
            }
        }
    });
}

// ---------------------------------------------------------------------------
// The switcher component
// ---------------------------------------------------------------------------

/// The organisation → workspace switcher (a grouped `<select>` plus an
/// org-management button) in the workbench header.
#[component]
pub fn WorkspaceSwitcher() -> impl IntoView {
    let workspaces = RwSignal::new(Vec::<WorkspaceMembership>::new());
    let orgs = RwSignal::new(Vec::<MyOrganisation>::new());
    let mode = RwSignal::new(String::from("single_user"));
    let switching = RwSignal::new(false);
    let modal_open = RwSignal::new(false);
    // Bumping this re-runs the loader (after a create/policy change refreshes the
    // switcher + org list in place).
    let reload = RwSignal::new(0_u32);

    // Load workspaces + organisations + deployment mode; re-run on `reload`.
    Effect::new(move |_| {
        reload.get();
        spawn_local(async move {
            let token = auth::resolve_token();
            if let Ok(list) = rest::list_workspaces(token.as_deref()).await {
                workspaces.set(list);
            }
            // The org listing resolves group names + admin visibility; a 403/error
            // just leaves it empty → a flat switcher, no org button in multi-user.
            match rest::list_organisations(token.as_deref()).await {
                Ok(list) => orgs.set(list),
                Err(_) => orgs.set(Vec::new()),
            }
            if let Ok(st) = rest::get_status(token.as_deref()).await {
                mode.set(st.mode);
            }
        });
    });

    let on_change = move |ev: leptos::ev::Event| {
        let id = event_target_value(&ev);
        if id.is_empty() || switching.get_untracked() {
            return;
        }
        // No-op if the chosen workspace is already the active one.
        let already_active = workspaces
            .get_untracked()
            .iter()
            .any(|w| w.id == id && w.active);
        if already_active {
            return;
        }
        spawn_switch(id, switching, |_| ());
    };

    view! {
        <span class="wb-switcher">
            // The grouped switcher — only meaningful with more than one workspace.
            <Show
                when=move || workspaces.with(|w| w.len() > 1)
                fallback=|| ().into_view()
            >
                <select
                    class="wb-workspace"
                    disabled=move || switching.get()
                    on:change=on_change
                >
                    {move || {
                        let ws = workspaces.get();
                        let groups = group_workspaces_by_org(&ws, &orgs.get());
                        groups
                            .into_iter()
                            .map(|g| {
                                let opts = g
                                    .workspaces
                                    .into_iter()
                                    .map(|w| {
                                        let label = if w.name.trim().is_empty() {
                                            w.slug.clone()
                                        } else {
                                            w.name.clone()
                                        };
                                        view! {
                                            <option value=w.id.clone() selected=w.active>
                                                {label}
                                            </option>
                                        }
                                    })
                                    .collect::<Vec<_>>();
                                if g.name.is_empty() {
                                    opts.into_any()
                                } else {
                                    view! { <optgroup label=g.name>{opts}</optgroup> }.into_any()
                                }
                            })
                            .collect::<Vec<_>>()
                    }}
                </select>
            </Show>

            // The org-management entry point (create flows + multi-user admin).
            <Show
                when=move || show_org_button(&mode.get(), &orgs.get())
                fallback=|| ().into_view()
            >
                <button
                    class="wb-org-btn"
                    title="Organisations & workspaces"
                    on:click=move |_| modal_open.set(true)
                >
                    "Organisations"
                </button>
            </Show>

            <OrgModal open=modal_open orgs mode reload memberships=workspaces />
        </span>
    }
}

// ---------------------------------------------------------------------------
// The organisations manager modal (two-pane: org rail + detail)
// ---------------------------------------------------------------------------

/// What the manager's detail pane shows: an organisation, or the create-org flow
/// (the rail's "New organisation" entry).
#[derive(Clone, Debug, PartialEq, Eq)]
enum OrgView {
    /// The organisation with this id.
    Org(String),
    /// The "create a new organisation" flow.
    Create,
}

/// The organisations manager. A left rail lists the caller's organisations (with
/// their role) plus a "New organisation" entry; the right pane shows the selected
/// org's detail — workspaces (switch / archive / restore), the create-workspace
/// flow, and, in `multi_user` for an administered org, members + policy + the
/// owner-only danger zone. Reuses the shared `settings-*` modal styling with the
/// wider two-pane `org-*` layout.
#[component]
fn OrgModal(
    open: RwSignal<bool>,
    orgs: RwSignal<Vec<MyOrganisation>>,
    mode: RwSignal<String>,
    reload: RwSignal<u32>,
    /// The caller's switcher memberships (`GET /workspaces`) — decorates workspace
    /// rows with the current/switch affordance.
    memberships: RwSignal<Vec<WorkspaceMembership>>,
) -> impl IntoView {
    // The workspace-created confirmation lives here — above `OrgPanel`, which
    // remounts when a create's `reload` refetches the org list. Held here it
    // survives that refresh (the flash-and-vanish bug); it is tagged with its org
    // (`notice_for_org`) so it only renders under the org it refers to. Cleared on
    // close so a fresh open starts clean (mirroring the org-created notice, which
    // resets when `CreateOrgForm` remounts on reopen).
    let ws_notice = RwSignal::new(Option::<WorkspaceNotice>::None);
    // The user's explicit choice; `None` means "auto", which `resolved` maps to
    // the first org (or the create flow when the caller sees no orgs at all). Auto
    // is a separate state — not an eager pin — because the org list loads async:
    // pinning before it arrives would land (and stick) on the create flow.
    let view_sel = RwSignal::new(Option::<OrgView>::None);
    let resolved = move || -> OrgView {
        if let Some(v) = view_sel.get() {
            return v;
        }
        match orgs.with(|l| l.first().map(|o| o.id.clone())) {
            Some(id) => OrgView::Org(id),
            None => OrgView::Create,
        }
    };
    let close = move || {
        open.set(false);
        ws_notice.set(None);
    };
    // Drop a selection whose org vanished (deleted / list refetch) back to auto.
    Effect::new(move |_| {
        let list = orgs.get();
        view_sel.update(|s| {
            if let Some(OrgView::Org(id)) = s {
                if !list.iter().any(|o| &o.id == id) {
                    *s = None;
                }
            }
        });
    });

    view! {
        <Show when=move || open.get() fallback=|| ().into_view()>
            <div class="settings-overlay" on:click=move |_| close()>
                <div
                    class="settings-modal org-modal"
                    on:click=move |ev| ev.stop_propagation()
                >
                    <header class="settings-header">
                        <div class="settings-header-titles">
                            <h2 class="settings-title">"Organisations"</h2>
                            <span class="settings-subtitle">
                                {move || {
                                    if is_multi_user(&mode.get()) {
                                        "manage members, policy & workspaces"
                                    } else {
                                        "create workspaces & organisations"
                                    }
                                }}
                            </span>
                        </div>
                        <button
                            class="settings-close"
                            title="Close"
                            on:click=move |_| close()
                        >
                            <Icon icon=MdIcon::Close />
                        </button>
                    </header>

                    <div class="org-layout">
                        // The organisation rail: one entry per org (name + the
                        // caller's role) and the create-org entry at the bottom.
                        <nav class="org-rail">
                            <span class="org-rail-label">"Your organisations"</span>
                            <For
                                each=move || orgs.get()
                                key=|o| o.id.clone()
                                children=move |o: MyOrganisation| {
                                    let id = o.id.clone();
                                    let sel_id = o.id.clone();
                                    let label = if o.name.trim().is_empty() {
                                        o.slug.clone()
                                    } else {
                                        o.name.clone()
                                    };
                                    let role = o.role.clone();
                                    let ws_count = o.workspaces.len();
                                    view! {
                                        <button
                                            class="org-rail-item"
                                            class:org-rail-item-active=move || {
                                                matches!(resolved(), OrgView::Org(i) if i == sel_id)
                                            }
                                            on:click=move |_| {
                                                view_sel.set(Some(OrgView::Org(id.clone())));
                                            }
                                        >
                                            <span class="org-rail-name">{label}</span>
                                            {(ws_count > 0)
                                                .then(|| view! {
                                                    <span class="org-rail-count">
                                                        {ws_count.to_string()}
                                                    </span>
                                                })}
                                            {(!role.trim().is_empty())
                                                .then(|| view! { <RoleBadge role=role.clone() /> })}
                                        </button>
                                    }
                                }
                            />
                            <button
                                class="org-rail-new"
                                class:org-rail-new-active=move || resolved() == OrgView::Create
                                on:click=move |_| view_sel.set(Some(OrgView::Create))
                            >
                                "+ New organisation"
                            </button>
                        </nav>

                        // The detail pane: the selected org, or the create flow.
                        <div class="org-detail">
                            // The create flow is mounted behind a boolean `Show`
                            // (memoized) rather than inside the org-panel closure:
                            // the post-create `reload` refetches `orgs`, and a
                            // closure rebuild would remount the form and wipe its
                            // "Created organisation …" notice — the same
                            // flash-and-vanish bug the lifted workspace notice
                            // fixes for `OrgPanel`.
                            <Show
                                when=move || resolved() == OrgView::Create
                                fallback=|| ().into_view()
                            >
                                <CreateOrgForm reload />
                            </Show>
                            {move || {
                                let OrgView::Org(id) = resolved() else {
                                    return ().into_any();
                                };
                                let multi = is_multi_user(&mode.get());
                                match orgs.get().iter().find(|o| o.id == id).cloned() {
                                    Some(org) => view! {
                                        <OrgPanel org multi reload ws_notice memberships />
                                    }
                                        .into_any(),
                                    None => ().into_any(),
                                }
                            }}
                        </div>
                    </div>
                </div>
            </div>
        </Show>
    }
}

/// A small role badge (`owner` / `admin` / `member`, org or workspace flavour).
#[component]
fn RoleBadge(role: String) -> impl IntoView {
    let token = role.trim().to_lowercase();
    let class = format!("org-badge org-badge-{token}");
    view! { <span class=class>{token}</span> }
}

/// One organisation's detail pane: header (name, slug, the caller's role), its
/// workspaces (switch / archive / restore + the create flow), and — in
/// `multi_user` when the caller administers it — members + policy, plus the
/// owner-only danger zone. In `single_user` the member/role chrome is hidden
/// entirely.
#[component]
fn OrgPanel(
    org: MyOrganisation,
    multi: bool,
    reload: RwSignal<u32>,
    /// The workspace-created notice, held by the parent so it survives this panel's
    /// remount on the post-create list refresh (tagged with its org id).
    ws_notice: RwSignal<Option<WorkspaceNotice>>,
    /// The caller's switcher memberships — drive the current/switch affordances.
    memberships: RwSignal<Vec<WorkspaceMembership>>,
) -> impl IntoView {
    // The shared confirm dialog (replaces the native delete-org confirm).
    let dialogs = use_dialogs();
    let org_id = org.id.clone();
    let role = org.role.clone();
    let is_admin = is_org_admin_role(&role);
    let policy = org.workspace_creation.clone();
    // Deletion is owner-only; the enabled/disabled verdict is decided reactively
    // (below) from the shell listing, which — unlike the caller-visible
    // `org.workspaces` — includes archived shells the server also 409s on.
    let is_owner = is_org_owner_role(&role);
    let visible_workspaces_empty = org.workspaces.is_empty();
    // The org-admin shell listing's total count (live + archived), lifted here from
    // `OrgWorkspacesPanel` so the delete affordance can disable itself while an
    // archived-only org would be refused. `None` until that panel fetches it (or if
    // it can't — a 403 / error / non-admin), where the affordance falls back to the
    // caller-visible view and lets the server stay authoritative.
    let shell_count = RwSignal::new(Option::<usize>::None);
    // A `Copy` handle to this org's id for the notice's render closures (which must
    // stay `Fn`), read without a per-call clone that would demote them to `FnOnce`.
    let notice_org = StoredValue::new(org_id.clone());

    // --- Create a workspace in this org -----------------------------------
    let ws_name = RwSignal::new(String::new());
    let ws_slug = RwSignal::new(String::new());
    // Whether the slug was hand-edited; until then it tracks the name
    // (`suggest_slug`), so typing a name prefills a sensible handle.
    let ws_slug_touched = RwSignal::new(false);
    let ws_err = RwSignal::new(Option::<String>::None);
    let ws_busy = RwSignal::new(false);
    let create_ws = {
        let org_id = org_id.clone();
        move |_| {
            if ws_busy.get_untracked() {
                return;
            }
            let name = ws_name.get_untracked().trim().to_string();
            let slug = ws_slug.get_untracked().trim().to_string();
            ws_err.set(None);
            ws_notice.set(None);
            if name.is_empty() || slug.is_empty() {
                ws_err.set(Some("Name and slug are required.".into()));
                return;
            }
            ws_busy.set(true);
            let org_id = org_id.clone();
            spawn_local(async move {
                let token = auth::resolve_token();
                let body = CreateOrgWorkspace { name, slug };
                match rest::create_org_workspace(token.as_deref(), &org_id, &body).await {
                    Ok(ws) => {
                        let label = if ws.name.trim().is_empty() {
                            ws.slug
                        } else {
                            ws.name
                        };
                        // Set on the lifted, org-tagged signal so it survives the
                        // remount triggered by the `reload` below (the flash-and-
                        // vanish bug) and renders only under this org.
                        ws_notice.set(Some(WorkspaceNotice {
                            org_id: org_id.clone(),
                            message: format!(
                                "Created workspace “{label}”. Choose it from the switcher to enter."
                            ),
                        }));
                        ws_name.set(String::new());
                        ws_slug.set(String::new());
                        ws_slug_touched.set(false);
                        reload.update(|n| *n += 1);
                    }
                    Err(e) => ws_err.set(Some(friendly_error(&e, "Creating a workspace"))),
                }
                ws_busy.set(false);
            });
        }
    };

    // --- Delete this organisation (owner-only, empty orgs; server re-checks) ---
    let del_err = RwSignal::new(Option::<String>::None);
    let del_busy = RwSignal::new(false);
    let delete_org = {
        let org_id = org_id.clone();
        move |_| {
            if del_busy.get_untracked() {
                return;
            }
            let org_id = org_id.clone();
            dialogs.confirm(
                ConfirmSpec::danger(
                    "Delete organisation?",
                    "Delete this organisation permanently? This cannot be undone. Only an \
                     organisation that has never held a workspace can be deleted.",
                    "Delete",
                ),
                move || {
                    del_err.set(None);
                    del_busy.set(true);
                    let org_id = org_id.clone();
                    spawn_local(async move {
                        let token = auth::resolve_token();
                        match rest::delete_organisation(token.as_deref(), &org_id).await {
                            // Gone — refresh the switcher + org list (this org drops out).
                            Ok(()) => reload.update(|n| *n += 1),
                            Err(e) => {
                                del_err.set(Some(friendly_error(&e, "Deleting an organisation")))
                            }
                        }
                        del_busy.set(false);
                    });
                },
            );
        }
    };

    let org_name = if org.name.trim().is_empty() {
        org.slug.clone()
    } else {
        org.name.clone()
    };
    let org_slug = org.slug.clone();
    let member_workspaces = org.workspaces.clone();

    view! {
        // --- Org header: name, slug, the caller's role ------------------------
        <header class="org-head">
            <h3 class="org-head-title">{org_name}</h3>
            <span class="org-chip">{org_slug}</span>
            {(!role.trim().is_empty()).then(|| view! { <RoleBadge role=role.clone() /> })}
        </header>

        // --- Workspaces: rows (switch / archive / restore) + the create flow --
        <section class="settings-section">
            <h4 class="settings-section-title">"Workspaces"</h4>

            // Admins see the shell listing (live + archived, with archive/restore);
            // everyone else sees the workspaces they are a member of, with switch.
            // Archive/restore is shell administration, not member chrome, so the
            // admin panel shows in both modes; the listing itself is org-admin
            // gated server-side, so a stale gate fails closed (panel hides itself
            // and this member view takes over via the shell fetch's 403).
            {if is_admin {
                view! {
                    <OrgWorkspacesPanel
                        org_id=org_id.clone()
                        reload
                        shell_count
                        memberships
                    />
                }
                    .into_any()
            } else {
                view! {
                    <MemberWorkspaceList workspaces=member_workspaces memberships />
                }
                    .into_any()
            }}

            <label class="settings-label">"New workspace"</label>
            <div class="settings-form settings-form-row">
                <div class="settings-field">
                    <input
                        class="settings-input"
                        placeholder="Name"
                        prop:value=move || ws_name.get()
                        on:input=move |ev| {
                            let v = event_target_value(&ev);
                            if !ws_slug_touched.get_untracked() {
                                ws_slug.set(suggest_slug(&v));
                            }
                            ws_name.set(v);
                        }
                        disabled=move || ws_busy.get()
                    />
                </div>
                <div class="settings-field">
                    <input
                        class="settings-input settings-input-narrow"
                        placeholder="slug"
                        prop:value=move || ws_slug.get()
                        on:input=move |ev| {
                            ws_slug_touched.set(true);
                            ws_slug.set(event_target_value(&ev));
                        }
                        disabled=move || ws_busy.get()
                    />
                </div>
                <button
                    class="settings-btn settings-btn-primary"
                    on:click=create_ws
                    disabled=move || ws_busy.get()
                >
                    "Create"
                </button>
            </div>
            <Show
                when=move || ws_err.with(Option::is_some)
                fallback=|| ().into_view()
            >
                <div class="settings-form-error">
                    {move || ws_err.get().unwrap_or_default()}
                </div>
            </Show>
            <Show
                when=move || {
                    ws_notice.with(|n| notice_for_org(n, &notice_org.get_value()).is_some())
                }
                fallback=|| ().into_view()
            >
                <div class="settings-form-notice">
                    {move || {
                        ws_notice.with(|n| {
                            notice_for_org(n, &notice_org.get_value())
                                .unwrap_or_default()
                                .to_string()
                        })
                    }}
                </div>
            </Show>
        </section>

        // Members + policy — multi-user only, and only for an org the caller
        // administers. Data is never gated client-side (the server enforces);
        // this is the presentation default.
        {(multi && is_admin)
            .then(|| view! { <OrgMembersPanel org_id=org_id.clone() policy reload /> })}

        // Delete organisation — owner-only. The affordance keys off the shell
        // listing (archived workspaces count against deletion): disabled with an
        // explanatory tooltip while any workspace remains, enabled only once
        // empty. When the listing isn't available it falls back to the
        // caller-visible view and lets the server (which re-checks + 409s) stay
        // authoritative.
        {move || {
            let aff = org_delete_affordance(
                is_owner,
                visible_workspaces_empty,
                shell_count.get(),
            );
            if aff == OrgDeleteAffordance::Hidden {
                return ().into_any();
            }
            let blocked = aff == OrgDeleteAffordance::DisabledHasWorkspaces;
            let title = if blocked {
                "This organisation still has workspaces (archived ones count); \
                 workspaces can only be archived, not deleted."
            } else {
                "Delete this organisation permanently"
            };
            let delete_org = delete_org.clone();
            view! {
                <section class="org-danger">
                    <span class="org-danger-title">"Danger zone"</span>
                    <p class="settings-blurb">
                        "Deleting an organisation is permanent. Only an organisation \
                         with no workspaces — archived ones count — can be deleted."
                    </p>
                    <div class="settings-form">
                        <button
                            class="settings-btn settings-btn-danger"
                            title=title
                            on:click=delete_org
                            disabled=move || blocked || del_busy.get()
                        >
                            "Delete organisation"
                        </button>
                    </div>
                    <Show
                        when=move || del_err.with(Option::is_some)
                        fallback=|| ().into_view()
                    >
                        <div class="settings-form-error">
                            {move || del_err.get().unwrap_or_default()}
                        </div>
                    </Show>
                </section>
            }
                .into_any()
        }}
    }
}

/// One workspace row's static display data (shared by the admin shell rows and
/// the member rows).
struct WsRowInfo {
    id: String,
    name: String,
    slug: String,
    archived: bool,
}

/// A single workspace row: status dot, name + slug, and — from the caller's
/// memberships — a "current" badge or a Switch action. `extra` renders the
/// row-specific admin action (archive / restore), if any.
fn ws_row(
    info: WsRowInfo,
    memberships: RwSignal<Vec<WorkspaceMembership>>,
    switch_busy: RwSignal<bool>,
    switch_err: RwSignal<Option<String>>,
    extra: impl IntoView + 'static,
) -> impl IntoView {
    let WsRowInfo {
        id,
        name,
        slug,
        archived,
    } = info;
    let display = if name.trim().is_empty() {
        slug.clone()
    } else {
        name
    };
    let state_id = id.clone();
    let state = move || ws_switch_state(&memberships.get(), &state_id);
    let sw_state = state.clone();
    let is_current = move || state() == WsSwitch::Current;
    let switch_id = id.clone();
    view! {
        <li class="org-ws-row" class:org-ws-row-active=is_current.clone()>
            <span
                class="org-ws-dot"
                class:org-ws-dot-archived=archived
            ></span>
            <span class="org-ws-meta">
                <span class="org-ws-name">{display}</span>
                <span class="org-ws-slug">{slug}</span>
            </span>
            {archived.then(|| view! { <span class="org-badge org-badge-archived">"archived"</span> })}
            <span class="org-ws-actions">
                {move || match (archived, sw_state()) {
                    (true, _) | (false, WsSwitch::NotMember) => ().into_any(),
                    (false, WsSwitch::Current) => {
                        view! { <span class="org-badge org-badge-current">"current"</span> }
                            .into_any()
                    }
                    (false, WsSwitch::Available) => {
                        let id = switch_id.clone();
                        view! {
                            <button
                                class="settings-btn settings-btn-mini"
                                title="Switch into this workspace"
                                disabled=move || switch_busy.get()
                                on:click=move |_| {
                                    spawn_switch(id.clone(), switch_busy, move |e| {
                                        switch_err.set(Some(e));
                                    });
                                }
                            >
                                "Switch"
                            </button>
                        }
                            .into_any()
                    }
                }}
                {extra}
            </span>
        </li>
    }
}

/// The workspaces a non-admin member sees: the org's caller-visible workspaces
/// with the current/switch affordance (no shell administration).
#[component]
fn MemberWorkspaceList(
    workspaces: Vec<MyWorkspace>,
    memberships: RwSignal<Vec<WorkspaceMembership>>,
) -> impl IntoView {
    let switch_busy = RwSignal::new(false);
    let switch_err = RwSignal::new(Option::<String>::None);
    let empty = workspaces.is_empty();
    view! {
        {empty.then(|| view! {
            <p class="settings-empty">"No workspaces here yet — create one below."</p>
        })}
        <ul class="org-ws-list">
            {workspaces
                .into_iter()
                .map(|w| {
                    ws_row(
                        WsRowInfo {
                            id: w.id,
                            name: w.name,
                            slug: w.slug,
                            archived: false,
                        },
                        memberships,
                        switch_busy,
                        switch_err,
                        (),
                    )
                })
                .collect::<Vec<_>>()}
        </ul>
        <Show
            when=move || switch_err.with(Option::is_some)
            fallback=|| ().into_view()
        >
            <div class="settings-form-error">
                {move || switch_err.get().unwrap_or_default()}
            </div>
        </Show>
    }
}

/// The org-admin workspace panel: every shell in the org (live + archived) with
/// switch (when the admin is also a member), a reversible **Archive** (hides the
/// workspace from the switcher) and **Restore**. The shell listing is org-admin
/// gated; a `403` fails closed (the panel yields to the member view), so a stale
/// presentation gate never surfaces actions the server would reject. After
/// either action the switcher + lists are refreshed (parent `reload`).
#[component]
fn OrgWorkspacesPanel(
    org_id: String,
    reload: RwSignal<u32>,
    /// The shell listing's total count (live + archived), lifted to the parent so
    /// the delete-organisation affordance can disable itself while any workspace —
    /// archived ones included — remains. Set on each successful fetch; left/reset to
    /// `None` (unavailable) on a `403` / error, where the parent falls back to the
    /// caller-visible view and lets the server stay authoritative.
    shell_count: RwSignal<Option<usize>>,
    /// The caller's switcher memberships — drive the current/switch affordances.
    memberships: RwSignal<Vec<WorkspaceMembership>>,
) -> impl IntoView {
    // The shared confirm dialog (replaces the native archive-workspace confirm).
    let dialogs = use_dialogs();
    let org_sv = StoredValue::new(org_id);
    let shells = RwSignal::new(Vec::<WorkspaceShell>::new());
    let denied = RwSignal::new(false);
    let load_err = RwSignal::new(Option::<String>::None);
    let act_err = RwSignal::new(Option::<String>::None);
    let switch_busy = RwSignal::new(false);
    // The name/slug filter — an org can hold very many shells; rendering is also
    // capped at `WS_ROW_CAP` per bucket with an explicit "…N more" note.
    let filter = RwSignal::new(String::new());
    let live_rows = move || {
        let q = filter.get();
        let rows = shells
            .get()
            .into_iter()
            .filter(|s| !s.is_archived() && shell_matches(s, &q))
            .collect::<Vec<_>>();
        cap_rows(memberships.with(|m| order_live_shells(rows, m)))
    };
    let archived_rows = move || {
        let q = filter.get();
        cap_rows(
            shells
                .get()
                .into_iter()
                .filter(|s| s.is_archived() && shell_matches(s, &q))
                .collect::<Vec<_>>(),
        )
    };
    // Bumping this re-fetches the shell list (after archive / restore).
    let shells_reload = RwSignal::new(0_u32);
    Effect::new(move |_| {
        shells_reload.get();
        let org_id = org_sv.get_value();
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_org_workspaces(token.as_deref(), &org_id).await {
                Ok(list) => {
                    // Total (live + archived) drives the delete affordance — archived
                    // shells count against deletion just like the server does.
                    shell_count.set(Some(list.len()));
                    shells.set(list);
                    denied.set(false);
                    load_err.set(None);
                }
                Err(RestError::Status { status: 403, .. }) => {
                    denied.set(true);
                    shells.set(Vec::new());
                    shell_count.set(None);
                }
                Err(e) => {
                    load_err.set(Some(e.to_string()));
                    shell_count.set(None);
                }
            }
        });
    });

    // Archive (with a confirm that spells out the reversible-hide semantics), then
    // refresh both the shell list and the switcher. `Copy`-only capture keeps the
    // per-row `<For>` button `Fn`.
    let archive = move |ws_id: String| {
        dialogs.confirm(
            ConfirmSpec::danger(
                "Archive workspace?",
                "Archive this workspace? It disappears from the workspace switcher and \
                 can no longer be opened. This is reversible — you can restore it here.",
                "Archive",
            ),
            move || {
                act_err.set(None);
                let org_id = org_sv.get_value();
                let ws_id = ws_id.clone();
                spawn_local(async move {
                    let token = auth::resolve_token();
                    match rest::archive_org_workspace(token.as_deref(), &org_id, &ws_id).await {
                        Ok(()) => {
                            shells_reload.update(|n| *n += 1);
                            reload.update(|n| *n += 1);
                        }
                        Err(e) => act_err.set(Some(friendly_error(&e, "Archiving a workspace"))),
                    }
                });
            },
        );
    };
    let restore = move |ws_id: String| {
        act_err.set(None);
        let org_id = org_sv.get_value();
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::restore_org_workspace(token.as_deref(), &org_id, &ws_id).await {
                Ok(_) => {
                    shells_reload.update(|n| *n += 1);
                    reload.update(|n| *n += 1);
                }
                Err(e) => act_err.set(Some(friendly_error(&e, "Restoring a workspace"))),
            }
        });
    };

    view! {
        <Show when=move || !denied.get() fallback=|| ().into_view()>
            <Show
                when=move || load_err.with(Option::is_some)
                fallback=|| ().into_view()
            >
                <div class="settings-form-error">
                    {move || load_err.get().unwrap_or_default()}
                </div>
            </Show>

            <Show
                when=move || shells.with(|l| !l.iter().any(|s| !s.is_archived()))
                fallback=|| ().into_view()
            >
                <p class="settings-empty">"No live workspaces — create one below."</p>
            </Show>

            // The filter box — only once the org holds enough shells to need it.
            <Show
                when=move || shells.with(|l| l.len() > WS_FILTER_THRESHOLD)
                fallback=|| ().into_view()
            >
                <input
                    class="settings-input"
                    placeholder="Filter workspaces…"
                    prop:value=move || filter.get()
                    on:input=move |ev| filter.set(event_target_value(&ev))
                />
            </Show>

            // Live workspaces: current/switch affordance + a reversible Archive.
            <ul class="org-ws-list">
                <For
                    each=move || live_rows().0
                    key=|s| s.id.clone()
                    children=move |s: WorkspaceShell| {
                        let id = s.id.clone();
                        ws_row(
                            WsRowInfo {
                                id: s.id.clone(),
                                name: s.name.clone(),
                                slug: s.slug.clone(),
                                archived: false,
                            },
                            memberships,
                            switch_busy,
                            act_err,
                            view! {
                                <button
                                    class="settings-btn settings-btn-mini"
                                    title="Archive workspace (reversible — hides it from the switcher)"
                                    on:click=move |_| archive(id.clone())
                                >
                                    "Archive"
                                </button>
                            },
                        )
                    }
                />
            </ul>
            {move || {
                let hidden = live_rows().1;
                (hidden > 0).then(|| view! {
                    <p class="settings-empty">
                        {format!("…and {hidden} more — filter to narrow the list.")}
                    </p>
                })
            }}

            // Archived workspaces (restorable) — shown only when any exist.
            <Show
                when=move || shells.with(|l| l.iter().any(WorkspaceShell::is_archived))
                fallback=|| ().into_view()
            >
                <label class="settings-label">"Archived"</label>
                <ul class="org-ws-list">
                    <For
                        each=move || archived_rows().0
                        key=|s| s.id.clone()
                        children=move |s: WorkspaceShell| {
                            let id = s.id.clone();
                            ws_row(
                                WsRowInfo {
                                    id: s.id.clone(),
                                    name: s.name.clone(),
                                    slug: s.slug.clone(),
                                    archived: true,
                                },
                                memberships,
                                switch_busy,
                                act_err,
                                view! {
                                    <button
                                        class="settings-btn settings-btn-mini"
                                        title="Restore workspace (returns it to the switcher)"
                                        on:click=move |_| restore(id.clone())
                                    >
                                        "Restore"
                                    </button>
                                },
                            )
                        }
                    />
                </ul>
                {move || {
                    let hidden = archived_rows().1;
                    (hidden > 0).then(|| view! {
                        <p class="settings-empty">
                            {format!("…and {hidden} more — filter to narrow the list.")}
                        </p>
                    })
                }}
            </Show>

            <Show
                when=move || act_err.with(Option::is_some)
                fallback=|| ().into_view()
            >
                <div class="settings-form-error">
                    {move || act_err.get().unwrap_or_default()}
                </div>
            </Show>
        </Show>
    }
}

/// The org members + policy admin panel (multi-user, org admin/owner only). The
/// member list is fetched on mount; a `403` fails closed (controls hidden), so a
/// stale presentation gate never exposes admin actions the server would reject.
#[component]
fn OrgMembersPanel(org_id: String, policy: String, reload: RwSignal<u32>) -> impl IntoView {
    // Hold the org id behind a `Copy` handle so every nested closure (incl. the
    // per-row `<For>` remove button, which must stay `Fn`) can read it without a
    // move-per-clone that would demote the reactive children to `FnOnce`.
    let org_sv = StoredValue::new(org_id);
    let members = RwSignal::new(Vec::<OrgMemberView>::new());
    let denied = RwSignal::new(false);
    let load_err = RwSignal::new(Option::<String>::None);
    // Bumping this re-fetches the member list (after add/remove).
    let members_reload = RwSignal::new(0_u32);
    Effect::new(move |_| {
        members_reload.get();
        let org_id = org_sv.get_value();
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_org_members(token.as_deref(), &org_id).await {
                Ok(list) => {
                    members.set(list);
                    denied.set(false);
                    load_err.set(None);
                }
                Err(RestError::Status { status: 403, .. }) => {
                    denied.set(true);
                    members.set(Vec::new());
                }
                Err(e) => load_err.set(Some(e.to_string())),
            }
        });
    });

    // --- Add / re-role a member (by email or user id) ---------------------
    let add_user = RwSignal::new(String::new());
    let add_role = RwSignal::new(String::from("member"));
    let add_err = RwSignal::new(Option::<String>::None);
    let add_busy = RwSignal::new(false);
    let add_member = move |_| {
        if add_busy.get_untracked() {
            return;
        }
        add_err.set(None);
        // Accept either a raw user id or an email — a UUID is used directly, an
        // email is resolved through the org-gated lookup first.
        let Some(input) = classify_member_input(&add_user.get_untracked()) else {
            add_err.set(Some("A user id or email is required.".into()));
            return;
        };
        add_busy.set(true);
        let org_id = org_sv.get_value();
        let role = add_role.get_untracked();
        spawn_local(async move {
            let token = auth::resolve_token();
            // Resolve the target user id: an email goes through `user-lookup`
            // (a 404 there means "no user with that email"), a UUID is used as-is.
            let resolved = match input {
                MemberInput::UserId(id) => Ok(id),
                MemberInput::Email(email) => {
                    match rest::user_lookup(token.as_deref(), &org_id, &email).await {
                        Ok(u) => Ok(u.user_id),
                        Err(RestError::Status { status: 404, .. }) => {
                            Err("No user with that email.".to_string())
                        }
                        Err(e) => Err(friendly_error(&e, "Looking up a user")),
                    }
                }
            };
            match resolved {
                Ok(user_id) => {
                    let body = AddOrgMember { user_id, role };
                    match rest::add_org_member(token.as_deref(), &org_id, &body).await {
                        Ok(_) => {
                            add_user.set(String::new());
                            members_reload.update(|n| *n += 1);
                        }
                        Err(e) => add_err.set(Some(friendly_error(&e, "Adding a member"))),
                    }
                }
                Err(msg) => add_err.set(Some(msg)),
            }
            add_busy.set(false);
        });
    };

    // --- Change the workspace-creation policy -----------------------------
    let policy_val = RwSignal::new(policy);
    let policy_err = RwSignal::new(Option::<String>::None);
    let set_policy = move |ev: leptos::ev::Event| {
        let next = event_target_value(&ev);
        policy_val.set(next.clone());
        policy_err.set(None);
        let org_id = org_sv.get_value();
        spawn_local(async move {
            let token = auth::resolve_token();
            let body = SetOrgPolicy {
                workspace_creation: next,
            };
            match rest::set_org_policy(token.as_deref(), &org_id, &body).await {
                Ok(o) => {
                    policy_val.set(o.workspace_creation);
                    reload.update(|n| *n += 1);
                }
                Err(e) => policy_err.set(Some(friendly_error(&e, "Changing the policy"))),
            }
        });
    };

    // A `Copy` remove handler (only `Copy` handles captured), so the per-row
    // `<For>` button stays `Fn` — mirrors the `Fn + Copy` callback idiom used by
    // the tasks board.
    let remove_member = move |user_id: String| {
        let org_id = org_sv.get_value();
        spawn_local(async move {
            let token = auth::resolve_token();
            if rest::remove_org_member(token.as_deref(), &org_id, &user_id)
                .await
                .is_ok()
            {
                members_reload.update(|n| *n += 1);
            }
        });
    };

    view! {
        <Show
            when=move || !denied.get()
            fallback=|| view! {
                <p class="settings-empty">
                    "You do not administer this organisation."
                </p>
            }
        >
            <section class="settings-section">
                <h4 class="settings-section-title">
                    "Members"
                    <span class="org-count">
                        {move || {
                            let n = members.with(Vec::len);
                            if n > 0 { format!(" · {n}") } else { String::new() }
                        }}
                    </span>
                </h4>
                <Show
                    when=move || load_err.with(Option::is_some)
                    fallback=|| ().into_view()
                >
                    <div class="settings-form-error">
                        {move || load_err.get().unwrap_or_default()}
                    </div>
                </Show>
                <ul class="org-member-list">
                    <For
                        each=move || members.get()
                        key=|m| (m.user_id.clone(), m.role.clone())
                        children=move |m: OrgMemberView| {
                            let uid = m.user_id.clone();
                            let initial = member_initial(&m.display_name, &m.email);
                            let name = if m.display_name.trim().is_empty() {
                                m.email.clone()
                            } else {
                                m.display_name.clone()
                            };
                            // The email line only adds signal when it isn't already
                            // the headline.
                            let mail =
                                (!m.email.trim().is_empty() && m.email != name)
                                    .then(|| m.email.clone());
                            view! {
                                <li class="org-member-row">
                                    <span class="org-avatar">{initial}</span>
                                    <span class="org-member-meta">
                                        <span class="org-member-name">{name}</span>
                                        {mail.map(|e| view! {
                                            <span class="org-member-mail">{e}</span>
                                        })}
                                    </span>
                                    <RoleBadge role=m.role.clone() />
                                    <button
                                        class="settings-conn-del"
                                        title="Remove member"
                                        on:click=move |_| remove_member(uid.clone())
                                    >
                                        <Icon icon=MdIcon::Delete />
                                    </button>
                                </li>
                            }
                        }
                    />
                </ul>

                <label class="settings-label">"Add member (by email or user id)"</label>
                <div class="settings-form settings-form-row">
                    <div class="settings-field">
                        <input
                            class="settings-input"
                            placeholder="email or user id"
                            prop:value=move || add_user.get()
                            on:input=move |ev| add_user.set(event_target_value(&ev))
                            disabled=move || add_busy.get()
                        />
                    </div>
                    <div class="settings-field">
                        <select
                            class="settings-input settings-input-narrow"
                            on:change=move |ev| add_role.set(event_target_value(&ev))
                            disabled=move || add_busy.get()
                        >
                            {ORG_ROLES
                                .iter()
                                .map(|(val, label)| {
                                    view! {
                                        <option value=*val selected=*val == "member">{*label}</option>
                                    }
                                })
                                .collect::<Vec<_>>()}
                        </select>
                    </div>
                    <button
                        class="settings-btn settings-btn-primary"
                        on:click=add_member
                        disabled=move || add_busy.get()
                    >
                        "Add"
                    </button>
                </div>
                <Show
                    when=move || add_err.with(Option::is_some)
                    fallback=|| ().into_view()
                >
                    <div class="settings-form-error">
                        {move || add_err.get().unwrap_or_default()}
                    </div>
                </Show>

                <label class="settings-label">"Workspace creation policy"</label>
                <select class="settings-input" on:change=set_policy>
                    {move || {
                        let cur = policy_val.get();
                        WS_POLICIES
                            .iter()
                            .map(|(val, label)| {
                                view! {
                                    <option value=*val selected=*val == cur>{*label}</option>
                                }
                            })
                            .collect::<Vec<_>>()
                    }}
                </select>
                <Show
                    when=move || policy_err.with(Option::is_some)
                    fallback=|| ().into_view()
                >
                    <div class="settings-form-error">
                        {move || policy_err.get().unwrap_or_default()}
                    </div>
                </Show>
            </section>
        </Show>
    }
}

/// The "create a new organisation" form (instance-policy gated server-side),
/// shown when the rail's "New organisation" entry is selected. Available in both
/// modes — the manager is the org admin surface in `multi_user`, and in
/// `single_user` creation is plainly available.
#[component]
fn CreateOrgForm(reload: RwSignal<u32>) -> impl IntoView {
    let name = RwSignal::new(String::new());
    let slug = RwSignal::new(String::new());
    // Slug tracks the name (`suggest_slug`) until hand-edited.
    let slug_touched = RwSignal::new(false);
    let err = RwSignal::new(Option::<String>::None);
    let notice = RwSignal::new(Option::<String>::None);
    let busy = RwSignal::new(false);

    let create = move |_| {
        if busy.get_untracked() {
            return;
        }
        let n = name.get_untracked().trim().to_string();
        let s = slug.get_untracked().trim().to_string();
        err.set(None);
        notice.set(None);
        if n.is_empty() || s.is_empty() {
            err.set(Some("Name and slug are required.".into()));
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let body = CreateOrg { name: n, slug: s };
            match rest::create_organisation(token.as_deref(), &body).await {
                Ok(o) => {
                    let label = if o.name.trim().is_empty() {
                        o.slug
                    } else {
                        o.name
                    };
                    notice.set(Some(format!("Created organisation “{label}”.")));
                    name.set(String::new());
                    slug.set(String::new());
                    slug_touched.set(false);
                    reload.update(|c| *c += 1);
                }
                Err(e) => err.set(Some(friendly_error(&e, "Creating an organisation"))),
            }
            busy.set(false);
        });
    };

    view! {
        <section class="settings-section">
            <h3 class="settings-section-title">"New organisation"</h3>
            <p class="settings-blurb">
                "An organisation groups workspaces for administration — its roles \
                 grant no data access. You become its Owner."
            </p>
            <div class="settings-form settings-form-row">
                <div class="settings-field">
                    <input
                        class="settings-input"
                        placeholder="Name"
                        prop:value=move || name.get()
                        on:input=move |ev| {
                            let v = event_target_value(&ev);
                            if !slug_touched.get_untracked() {
                                slug.set(suggest_slug(&v));
                            }
                            name.set(v);
                        }
                        disabled=move || busy.get()
                    />
                </div>
                <div class="settings-field">
                    <input
                        class="settings-input settings-input-narrow"
                        placeholder="slug"
                        prop:value=move || slug.get()
                        on:input=move |ev| {
                            slug_touched.set(true);
                            slug.set(event_target_value(&ev));
                        }
                        disabled=move || busy.get()
                    />
                </div>
                <button
                    class="settings-btn settings-btn-primary"
                    on:click=create
                    disabled=move || busy.get()
                >
                    "Create"
                </button>
            </div>
            <Show
                when=move || err.with(Option::is_some)
                fallback=|| ().into_view()
            >
                <div class="settings-form-error">{move || err.get().unwrap_or_default()}</div>
            </Show>
            <Show
                when=move || notice.with(Option::is_some)
                fallback=|| ().into_view()
            >
                <div class="settings-form-notice">{move || notice.get().unwrap_or_default()}</div>
            </Show>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(id: &str, org: &str, active: bool) -> WorkspaceMembership {
        WorkspaceMembership {
            id: id.to_string(),
            name: format!("ws-{id}"),
            slug: id.to_string(),
            role: "owner".to_string(),
            organisation_id: org.to_string(),
            active,
        }
    }

    fn org(id: &str, name: &str, role: &str) -> MyOrganisation {
        MyOrganisation {
            id: id.to_string(),
            name: name.to_string(),
            slug: name.to_lowercase(),
            role: role.to_string(),
            workspace_creation: "members".to_string(),
            can_create_workspace: true,
            workspaces: Vec::new(),
        }
    }

    #[test]
    fn groups_by_org_in_org_order_with_names() {
        let workspaces = vec![
            ws("w2", "o2", false),
            ws("w1", "o1", true),
            ws("w3", "o1", false),
        ];
        let orgs = vec![org("o1", "Acme", "owner"), org("o2", "Beta", "member")];
        let groups = group_workspaces_by_org(&workspaces, &orgs);
        assert_eq!(groups.len(), 2);
        // Org order preserved (o1 then o2), workspaces bucketed under each.
        assert_eq!(groups[0].name, "Acme");
        let ids: Vec<&str> = groups[0].workspaces.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["w1", "w3"]);
        assert_eq!(groups[1].name, "Beta");
        assert_eq!(groups[1].workspaces.len(), 1);
    }

    #[test]
    fn unknown_org_falls_into_trailing_unnamed_bucket() {
        let workspaces = vec![
            ws("w1", "o1", true),
            ws("w9", "gone", false),
            ws("w8", "", false),
        ];
        let orgs = vec![org("o1", "Acme", "owner")];
        let groups = group_workspaces_by_org(&workspaces, &orgs);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "Acme");
        // The orphan bucket is last, unnamed (renders as bare options), and holds
        // both the unknown-org and the empty-org workspaces.
        assert_eq!(groups[1].name, "");
        let ids: Vec<&str> = groups[1].workspaces.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["w9", "w8"]);
    }

    #[test]
    fn empty_orgs_collapse_to_one_flat_bucket() {
        let workspaces = vec![ws("w1", "o1", true), ws("w2", "o2", false)];
        let groups = group_workspaces_by_org(&workspaces, &[]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, ""); // flat: no <optgroup> header
        assert_eq!(groups[0].workspaces.len(), 2);
    }

    #[test]
    fn org_with_no_visible_workspace_adds_no_empty_group() {
        let workspaces = vec![ws("w1", "o1", true)];
        let orgs = vec![org("o1", "Acme", "owner"), org("o2", "Empty", "member")];
        let groups = group_workspaces_by_org(&workspaces, &orgs);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "Acme");
    }

    #[test]
    fn mode_and_admin_gates() {
        assert!(is_multi_user("multi_user"));
        assert!(!is_multi_user("single_user"));
        assert!(!is_multi_user("")); // absent ⇒ defaulted single-user

        assert!(is_org_admin_role("owner"));
        assert!(is_org_admin_role("admin"));
        assert!(!is_org_admin_role("member"));
        assert!(!is_org_admin_role(""));
    }

    #[test]
    fn org_button_visibility_by_mode() {
        let admin = vec![org("o1", "Acme", "admin")];
        let member = vec![org("o1", "Acme", "member")];
        // single-user: always shown (creation plainly available).
        assert!(show_org_button("single_user", &member));
        assert!(show_org_button("single_user", &[]));
        // multi-user: only when the caller administers some org (fail closed).
        assert!(show_org_button("multi_user", &admin));
        assert!(!show_org_button("multi_user", &member));
        assert!(!show_org_button("multi_user", &[]));
    }

    #[test]
    fn classify_member_input_splits_uuid_from_email() {
        // A canonical UUID (either case) is used directly as a user id.
        assert_eq!(
            classify_member_input("11111111-1111-1111-1111-111111111111"),
            Some(MemberInput::UserId(
                "11111111-1111-1111-1111-111111111111".into()
            ))
        );
        assert_eq!(
            classify_member_input("  AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE  "),
            Some(MemberInput::UserId(
                "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE".into()
            )),
            "trimmed + case-insensitive hex still classifies as an id"
        );

        // Anything else is treated as an email to resolve via lookup.
        assert_eq!(
            classify_member_input("ada@example.com"),
            Some(MemberInput::Email("ada@example.com".into()))
        );
        // A near-UUID that isn't canonical (wrong length / non-hex / bad dashes)
        // falls through to the email branch rather than being sent as an id.
        assert_eq!(
            classify_member_input("11111111-1111-1111-1111-11111111111"),
            Some(MemberInput::Email(
                "11111111-1111-1111-1111-11111111111".into()
            )),
            "35 chars is not a canonical UUID"
        );
        assert_eq!(
            classify_member_input("zzzzzzzz-1111-1111-1111-111111111111"),
            Some(MemberInput::Email(
                "zzzzzzzz-1111-1111-1111-111111111111".into()
            )),
            "non-hex is not a canonical UUID"
        );

        // Blank input is rejected.
        assert_eq!(classify_member_input("   "), None);
        assert_eq!(classify_member_input(""), None);
    }

    #[test]
    fn friendly_error_relays_403() {
        let denied = RestError::Status {
            status: 403,
            message: "workspace creation is not permitted".into(),
        };
        // A 403 with a server message relays it verbatim.
        assert_eq!(
            friendly_error(&denied, "Creating a workspace"),
            "workspace creation is not permitted"
        );
        // A 403 without a message gets the generic not-permitted line.
        let bare = RestError::Status {
            status: 403,
            message: String::new(),
        };
        assert!(friendly_error(&bare, "Creating a workspace").contains("not permitted"));

        // A 409 (a delete precondition) is relayed verbatim — no "(HTTP 409)" suffix
        // — so the org modal shows the server's exact reason.
        let conflict = RestError::Status {
            status: 409,
            message: "the default organisation cannot be deleted".into(),
        };
        assert_eq!(
            friendly_error(&conflict, "Deleting an organisation"),
            "the default organisation cannot be deleted"
        );
    }

    #[test]
    fn is_org_owner_role_is_owner_only() {
        assert!(is_org_owner_role("owner"));
        assert!(is_org_owner_role("  owner  "));
        // Admin administers but is NOT an owner — deletion is owner-only.
        assert!(!is_org_owner_role("admin"));
        assert!(!is_org_owner_role("member"));
        assert!(!is_org_owner_role(""));
    }

    #[test]
    fn org_delete_affordance_keys_off_shell_listing() {
        use OrgDeleteAffordance::{DisabledHasWorkspaces, Enabled, Hidden};
        // Not an owner ⇒ never offered, whatever the counts say.
        assert_eq!(org_delete_affordance(false, true, Some(0)), Hidden);
        assert_eq!(org_delete_affordance(false, true, None), Hidden);

        // Shell listing available + authoritative: any workspace (archived shells
        // included) ⇒ disabled; empty ⇒ enabled.
        assert_eq!(org_delete_affordance(true, true, Some(0)), Enabled);
        assert_eq!(
            org_delete_affordance(true, true, Some(1)),
            DisabledHasWorkspaces,
            "an archived-only org (no visible workspaces, but a shell exists) disables"
        );
        assert_eq!(
            org_delete_affordance(true, false, Some(2)),
            DisabledHasWorkspaces
        );

        // Listing unavailable (non-admin owner / 403 / error / pre-fetch): fall back
        // to the caller-visible view — offer it when empty (server stays the
        // authority and 409s if an archived shell lurks), hide it when non-empty.
        assert_eq!(org_delete_affordance(true, true, None), Enabled);
        assert_eq!(org_delete_affordance(true, false, None), Hidden);
    }

    #[test]
    fn notice_for_org_scopes_notice_to_its_org() {
        let held = Some(WorkspaceNotice {
            org_id: "o1".into(),
            message: "Created workspace “W”. Choose it from the switcher to enter.".into(),
        });
        // Shows under its own org (survives the panel remount because the parent
        // holds it).
        assert_eq!(
            notice_for_org(&held, "o1"),
            Some("Created workspace “W”. Choose it from the switcher to enter.")
        );
        // Hidden under a different org (switching orgs doesn't discard it).
        assert_eq!(notice_for_org(&held, "o2"), None);
        // Nothing held ⇒ nothing shown.
        assert_eq!(notice_for_org(&None, "o1"), None);
    }

    #[test]
    fn suggest_slug_kebab_cases_names() {
        assert_eq!(suggest_slug("My Cool Workspace"), "my-cool-workspace");
        assert_eq!(suggest_slug("  Acme,  Inc.  "), "acme-inc");
        assert_eq!(suggest_slug("R&D — 2026!"), "r-d-2026");
        // Uppercase folds down; digits survive.
        assert_eq!(suggest_slug("Team42"), "team42");
        // Non-ASCII characters are separators, never emitted.
        assert_eq!(suggest_slug("Ümlaut Space"), "mlaut-space");
        // Degenerate input yields an empty suggestion (the field stays editable,
        // and the create handler still requires a non-empty slug).
        assert_eq!(suggest_slug("!!!"), "");
        assert_eq!(suggest_slug(""), "");
    }

    #[test]
    fn ws_switch_state_from_memberships() {
        let memberships = vec![ws("w1", "o1", true), ws("w2", "o1", false)];
        // The active membership is the session's current workspace.
        assert_eq!(ws_switch_state(&memberships, "w1"), WsSwitch::Current);
        // A non-active membership can be switched into.
        assert_eq!(ws_switch_state(&memberships, "w2"), WsSwitch::Available);
        // A shell the caller doesn't belong to (org-admin view) offers no switch —
        // org roles confer no data access.
        assert_eq!(ws_switch_state(&memberships, "w9"), WsSwitch::NotMember);
        assert_eq!(ws_switch_state(&[], "w1"), WsSwitch::NotMember);
    }

    #[test]
    fn shell_matches_filters_by_name_or_slug() {
        let shell = WorkspaceShell {
            id: "w1".into(),
            name: "Research Lab".into(),
            slug: "research-lab".into(),
            archived_at: None,
        };
        // Blank matches everything; matching is case-insensitive substring.
        assert!(shell_matches(&shell, ""));
        assert!(shell_matches(&shell, "   "));
        assert!(shell_matches(&shell, "LAB"));
        assert!(shell_matches(&shell, "research-"));
        assert!(!shell_matches(&shell, "kitchen"));
    }

    #[test]
    fn order_live_shells_surfaces_current_then_member_workspaces() {
        let shell = |id: &str| WorkspaceShell {
            id: id.into(),
            name: format!("ws-{id}"),
            slug: id.into(),
            archived_at: None,
        };
        // Server order: two admin-only shells, a member workspace, the current one.
        let rows = vec![shell("s1"), shell("s2"), shell("w2"), shell("w1")];
        let memberships = vec![ws("w1", "o1", true), ws("w2", "o1", false)];
        let ordered: Vec<String> = order_live_shells(rows, &memberships)
            .into_iter()
            .map(|s| s.id)
            .collect();
        // Current first, then the other membership, then the rest in server order.
        assert_eq!(ordered, ["w1", "w2", "s1", "s2"]);
    }

    #[test]
    fn cap_rows_truncates_and_counts_hidden() {
        let (shown, hidden) = cap_rows((0..3).collect::<Vec<_>>());
        assert_eq!((shown.len(), hidden), (3, 0), "under the cap nothing hides");
        let (shown, hidden) = cap_rows((0..WS_ROW_CAP + 7).collect::<Vec<_>>());
        assert_eq!(shown.len(), WS_ROW_CAP);
        assert_eq!(hidden, 7);
    }

    #[test]
    fn member_initial_prefers_name_then_email() {
        assert_eq!(member_initial("Ada Lovelace", "ada@example.com"), "A");
        // Falls back to the email when the display name has no alphanumerics.
        assert_eq!(member_initial("", "grace@example.com"), "G");
        assert_eq!(member_initial("··", "grace@example.com"), "G");
        // Nothing usable at all yields the placeholder.
        assert_eq!(member_initial("", ""), "?");
    }
}
