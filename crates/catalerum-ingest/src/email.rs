//! Email ingestion (SOUL §28/§10) — turn an ingested message's body into
//! catalogued, searchable text.
//!
//! An [`Email`](catalerum_core::model::Email) already lives in Postgres (the
//! sync §28 landed it); this pipeline projects its **text** (subject + body) into
//! the **`documents` catalogue** keyed by `SourceRef::Email`, so mail joins notes
//! and files in the document corpus, and — when an [`EmbedContext`] is present —
//! chunks + embeds it into Qdrant (derived) so mail is **semantically searchable**
//! (the substrate for `search_emails`, §7). Unlike object ingestion there is no
//! blob read: the body is already extracted (the Maildir provider parsed it).
//!
//! Idempotent + reconciling (SOUL §3.1/§10): a re-ingest re-projects by
//! `SourceRef::Email`; an email found **deleted** purges its document (and
//! vectors) — the same contract as notes/objects.

use serde::{Deserialize, Serialize};
use tracing::debug;
use uuid::Uuid;

use catalerum_core::id::{EmailId, WorkspaceId};
use catalerum_core::model::{Email, SourceRef};
use catalerum_store::{Store, StoreError};

use crate::embed::{EmbedContext, IngestReport};
use crate::error::Result;

/// The `job_queue.kind` token for an email-ingest job (SOUL §28/§10).
pub const JOB_KIND_INGEST_EMAIL: &str = "ingest_email";

/// The JSON payload of a [`JOB_KIND_INGEST_EMAIL`] job: which email to ingest,
/// and optionally which workspace (resolved from the job row's `workspace_id`
/// column when absent — the same shape as the other ingest payloads).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestEmailPayload {
    /// The workspace that owns the email. Optional on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// The email to ingest.
    pub email_id: EmailId,
}

impl IngestEmailPayload {
    /// A payload carrying an explicit workspace scope.
    #[must_use]
    pub fn new(workspace_id: WorkspaceId, email_id: EmailId) -> Self {
        Self {
            workspace_id: Some(workspace_id),
            email_id,
        }
    }

    /// A payload that defers its scope to the job row's `workspace_id` column.
    #[must_use]
    pub fn for_email(email_id: EmailId) -> Self {
        Self {
            workspace_id: None,
            email_id,
        }
    }
}

/// Enqueue a durable [`JOB_KIND_INGEST_EMAIL`] job for `email_id` (SOUL §6.2/§10).
/// Returns the enqueued job's id. Idempotent at the data level: each run
/// re-projects the email's current text, so a duplicate job is at worst a
/// redundant re-projection.
pub async fn enqueue_ingest_email(
    store: &Store,
    workspace_id: WorkspaceId,
    email_id: EmailId,
) -> Result<Uuid> {
    let payload = IngestEmailPayload::new(workspace_id, email_id);
    let job = store
        .job_queue()
        .enqueue(
            Some(workspace_id),
            JOB_KIND_INGEST_EMAIL,
            serde_json::to_value(payload)?,
            None,
        )
        .await?;
    debug!(job = %job.id, %email_id, "enqueued ingest_email job");
    Ok(job.id)
}

/// The embeddable text for an email: its subject + body, with the sender for
/// context (mirrors how a note's title gives its body context). Prefers the
/// plain-text body; falls back to the HTML body verbatim when that's all there is
/// (a future slice can strip HTML — for now the raw markup is still better signal
/// than nothing).
#[must_use]
pub fn email_text(email: &Email) -> String {
    let from = email
        .from
        .as_ref()
        .map(|a| a.address.as_str())
        .unwrap_or("");
    let body = email
        .body_text
        .as_deref()
        .filter(|b| !b.trim().is_empty())
        .or(email.body_html.as_deref())
        .unwrap_or("");
    // A compact header keeps subject + sender in the embedded text without a
    // separate metadata channel.
    format!("Subject: {}\nFrom: {}\n\n{}", email.subject, from, body)
        .trim()
        .to_string()
}

/// Ingest one email (SOUL §28/§10). Reconciles to the email's *current* state: a
/// present email (re-)projects its text into the `documents` catalogue keyed by
/// `SourceRef::Email`, and — when `embed` is `Some` — chunks + embeds it into
/// Qdrant. An email found **deleted** purges its document (and vectors).
pub async fn ingest_email(
    store: &Store,
    embed: Option<&EmbedContext>,
    workspace_id: WorkspaceId,
    email_id: EmailId,
) -> Result<IngestReport> {
    let source = SourceRef::Email { id: email_id };

    let email = match store.emails().get(workspace_id, email_id).await {
        Ok(e) => e,
        Err(StoreError::NotFound) => {
            // Deleted: drop the derived projection (vectors + document).
            let report = match embed {
                Some(e) => e.purge(store, workspace_id, &source).await?,
                None => {
                    store
                        .documents()
                        .delete_by_source(workspace_id, &source)
                        .await?;
                    IngestReport {
                        document_id: None,
                        chunks: 0,
                    }
                }
            };
            debug!(%email_id, "email deleted; purged its document projection");
            return Ok(report);
        }
        Err(e) => return Err(e.into()),
    };

    let text = email_text(&email);

    // Truth first: catalogue the document. With an embed context the same upsert
    // runs inside the derived pipeline (idempotent), so we pick one path.
    let report = match embed {
        Some(e) => {
            let created_at = email.received_at.unwrap_or_else(chrono::Utc::now);
            e.ingest_text(store, workspace_id, &source, &text, None, created_at)
                .await?
        }
        None => {
            let doc = store
                .documents()
                .upsert_by_source(workspace_id, &source, &text, Some(&email.subject))
                .await?;
            IngestReport {
                document_id: Some(doc.id),
                chunks: 0,
            }
        }
    };
    debug!(%email_id, document = ?report.document_id, chunks = report.chunks, "ingest_email done");
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::model::EmailAddress;
    use catalerum_core::MailboxId;

    fn sample(subject: &str, body_text: Option<&str>, body_html: Option<&str>) -> Email {
        Email {
            id: EmailId::new(),
            workspace_id: WorkspaceId::new(),
            mailbox_id: MailboxId::new(),
            uid: "u".into(),
            message_id: None,
            from: Some(EmailAddress::new("ada@example.com")),
            to: vec![],
            cc: vec![],
            subject: subject.to_string(),
            received_at: None,
            body_text: body_text.map(str::to_string),
            body_html: body_html.map(str::to_string),
            has_attachments: false,
            flags: vec![],
            labels: vec![],
            raw_ref: None,
            attachments: Vec::new(),
            raw: None,
        }
    }

    #[test]
    fn email_text_prefers_plain_body_and_includes_subject_sender() {
        let t = email_text(&sample("Hello", Some("the body"), Some("<p>html</p>")));
        assert!(t.contains("Subject: Hello"));
        assert!(t.contains("From: ada@example.com"));
        assert!(t.contains("the body"));
        assert!(!t.contains("<p>"), "plain body wins over html");
    }

    #[test]
    fn email_text_falls_back_to_html_when_no_plain() {
        let t = email_text(&sample("H", None, Some("<p>html only</p>")));
        assert!(t.contains("<p>html only</p>"));
    }

    #[test]
    fn payload_round_trips_and_accepts_email_only_shape() {
        let p = IngestEmailPayload::new(WorkspaceId::new(), EmailId::new());
        let json = serde_json::to_value(p).unwrap();
        assert!(json.get("workspace_id").is_some());
        let back: IngestEmailPayload = serde_json::from_value(json).unwrap();
        assert_eq!(p, back);

        let eid = EmailId::new();
        let only = serde_json::json!({ "email_id": eid });
        let p2: IngestEmailPayload = serde_json::from_value(only).unwrap();
        assert_eq!(p2.workspace_id, None);
        assert_eq!(p2.email_id, eid);
        assert_eq!(IngestEmailPayload::for_email(eid), p2);
    }

    #[test]
    fn job_kind_token_is_stable() {
        assert_eq!(JOB_KIND_INGEST_EMAIL, "ingest_email");
    }
}
