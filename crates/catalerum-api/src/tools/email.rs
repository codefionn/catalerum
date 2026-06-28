//! Email read tools: `get_emails` / `read_email` (SOUL §28).

use super::*;

/// How many emails to scan from the store before client-side filtering
/// (by-sender / unread). Recent-first, so the most relevant are covered; a
/// dedicated indexed query can replace this if mailboxes grow huge.
pub(crate) const EMAIL_SCAN_CAP: i64 = 500;

/// `get_emails` — typed, **read-only**, workspace-scoped email lookups (SOUL
/// §7/§28): the structured complement to `search_emails` (semantic). The model
/// picks a named `operation`; results carry the mailbox name + sender + subject,
/// never the raw row. Gated on `email:read` (deny-by-default, §19).
pub(crate) struct GetEmailsTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for GetEmailsTool {
    fn name(&self) -> &str {
        "get_emails"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "email")
    }

    fn description(&self) -> &str {
        "List ingested email messages (read-only). operation = 'recent_emails' \
         (most recently received); 'emails_by_sender' (from address contains \
         `sender`); 'unread_emails' (not yet flagged seen); 'untagged_emails' \
         (no labels yet — filtered server-side, so old untagged mail is \
         reachable; feed a classify-and-LabelEmail sweep from this). Results \
         carry the mailbox name + id, uid, sender, subject, received time, \
         unread flag, labels, and whether the message has attachments."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["recent_emails", "emails_by_sender", "unread_emails", "untagged_emails"],
                    "description": "Which email lookup to run."
                },
                "sender": {
                    "type": "string",
                    "description": "Substring to match against the From address (required for emails_by_sender)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results (1-50, default 10).",
                    "minimum": 1,
                    "maximum": 50
                }
            },
            "required": ["operation"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let limit = opt_clamped_u64(&args, "limit", 10, 50) as usize;
        let operation = required_str(&args, "operation")?;

        // Index mailbox_id → name so each email carries where it lives, not an id.
        let mailboxes = self
            .store
            .mailboxes()
            .list_by_workspace(ws)
            .await
            .map_err(query_err)?;
        let mailbox_index: std::collections::HashMap<MailboxId, String> =
            mailboxes.into_iter().map(|m| (m.id, m.name)).collect();

        // `untagged_emails` filters in SQL rather than scanning a recent window:
        // a backlog sweep must reach old untagged mail even when the newest
        // EMAIL_SCAN_CAP messages are all labelled already.
        if operation == "untagged_emails" {
            let results: Vec<Json> = self
                .store
                .emails()
                .list_untagged_by_workspace(ws, limit as i64)
                .await
                .map_err(query_err)?
                .into_iter()
                .map(|e| email_summary(e, &mailbox_index))
                .collect();
            return Ok(json!({ "operation": operation, "results": results }));
        }

        // `recent_emails` needs only the top `limit`; the filtered ops scan a
        // larger recent window then filter + take.
        let fetch = if operation == "recent_emails" {
            limit as i64
        } else {
            EMAIL_SCAN_CAP
        };
        let emails = self
            .store
            .emails()
            .list_by_workspace(ws, fetch)
            .await
            .map_err(query_err)?;

        let results: Vec<Json> = match operation.as_str() {
            "recent_emails" => emails
                .into_iter()
                .take(limit)
                .map(|e| email_summary(e, &mailbox_index))
                .collect(),
            "emails_by_sender" => {
                let sender = required_str(&args, "sender")?.to_ascii_lowercase();
                emails
                    .into_iter()
                    .filter(|e| {
                        e.from.as_ref().is_some_and(|a| {
                            a.address.to_ascii_lowercase().contains(&sender)
                                || a.name
                                    .as_ref()
                                    .is_some_and(|n| n.to_ascii_lowercase().contains(&sender))
                        })
                    })
                    .take(limit)
                    .map(|e| email_summary(e, &mailbox_index))
                    .collect()
            }
            "unread_emails" => emails
                .into_iter()
                .filter(|e| !e.flags.iter().any(|f| f.eq_ignore_ascii_case("seen")))
                .take(limit)
                .map(|e| email_summary(e, &mailbox_index))
                .collect(),
            other => {
                return Err(Error::invalid(format!(
                    "unknown get_emails operation `{other}` (expected \
                     recent_emails | emails_by_sender | unread_emails | untagged_emails)"
                )))
            }
        };
        Ok(json!({ "operation": operation, "results": results }))
    }
}

/// A compact email view for tool results: the message plus its mailbox name
/// (resolved via `mailbox_index`); the body is omitted to save tokens (the model
/// asks for it explicitly / via search). `unread` = the `seen` flag is absent.
/// `mailbox_id` + `uid` are carried so an automation Code node can iterate these
/// summaries and hand each one to `LabelEmail`/`MarkEmailRead` (which target by
/// `(mailbox_id, uid)`); `labels` so a Condition can gate on classification state.
pub(crate) fn email_summary(
    e: catalerum_core::model::Email,
    mailbox_index: &std::collections::HashMap<MailboxId, String>,
) -> Json {
    let mailbox = mailbox_index
        .get(&e.mailbox_id)
        .cloned()
        .unwrap_or_default();
    let from = e.from.as_ref().map(|a| match &a.name {
        Some(n) => format!("{n} <{}>", a.address),
        None => a.address.clone(),
    });
    let unread = !e.flags.iter().any(|f| f.eq_ignore_ascii_case("seen"));
    json!({
        "id": e.id,
        "mailbox": mailbox,
        "mailbox_id": e.mailbox_id,
        "uid": e.uid,
        "from": from,
        "subject": e.subject,
        "received_at": e.received_at,
        "unread": unread,
        "labels": e.labels,
        "has_attachments": e.has_attachments,
    })
}

/// Format an [`EmailAddress`](catalerum_core::model::EmailAddress) as
/// `Name <addr>` (or just `addr` when unnamed) — the readable form `email_summary`
/// uses for `from`, reused for `read_email`'s recipients.
pub(crate) fn fmt_addr(a: &catalerum_core::model::EmailAddress) -> String {
    match &a.name {
        Some(n) => format!("{n} <{}>", a.address),
        None => a.address.clone(),
    }
}

/// `read_email` — read one email's **full body** + sender/recipients by id (SOUL
/// §7/§28). The id comes from `get_emails` / `search_emails`, which return only
/// subject/sender summaries + matched snippets. The email counterpart to
/// `read_object` / `read_note`; gated `email:read` (deny-by-default, §19).
/// NotFound never leaks another tenant's mail.
pub(crate) struct ReadEmailTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for ReadEmailTool {
    fn name(&self) -> &str {
        "read_email"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "email")
    }
    fn description(&self) -> &str {
        "Read one email's full body, sender, and recipients by its id (the `id` from \
         get_emails or search_emails). Use to read a specific message in full; use \
         search_emails to find relevant mail across the mailbox."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Email id (a UUID from get_emails/search_emails)." }
            },
            "required": ["id"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id: EmailId = parse_id(&args, "id")?;
        let email = self.store.emails().get(ws, id).await?;
        // Best-effort mailbox name (a missing mailbox degrades to "").
        let mailbox = self
            .store
            .mailboxes()
            .get(ws, email.mailbox_id)
            .await
            .map(|m| m.name)
            .unwrap_or_default();
        let unread = !email.flags.iter().any(|f| f.eq_ignore_ascii_case("seen"));
        let (body, truncated) = cap_read_text(email.body_text.as_deref().unwrap_or(""));
        Ok(json!({
            "id": email.id,
            "mailbox": mailbox,
            "from": email.from.as_ref().map(fmt_addr),
            "to": email.to.iter().map(fmt_addr).collect::<Vec<_>>(),
            "cc": email.cc.iter().map(fmt_addr).collect::<Vec<_>>(),
            "subject": email.subject,
            "received_at": email.received_at,
            "unread": unread,
            "has_attachments": email.has_attachments,
            "body": body,
            "truncated": truncated,
            "has_html": email.body_html.is_some(),
        }))
    }
}

// ---------------------------------------------------------------------------
// Source connections (email / calendar) — SOUL §8/§10/§28
// ---------------------------------------------------------------------------
