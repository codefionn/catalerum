//! Email read REST surface (SOUL §28/§12) — the **inbox view** over ingested
//! mail. Read-only w.r.t. the **provider** (catalerum reads mail, it never
//! sends/replies, §14) — the only mutations here touch catalerum's own copy
//! (the local `seen` flag) and the source registrations. Every route is
//! authenticated + workspace-scoped (§18); reads are `email:read`-gated,
//! mutations `email:write`-gated (§19):
//!
//! - `GET /mailboxes` — list the workspace's mailboxes, each annotated with its
//!   **unread count** + owning connection (the sidebar's account grouping)
//! - `GET /emails?…` — list emails as compact rows, with optional
//!   `mailbox`/`mailbox_id`/`sender`/`unread` filters + `limit`
//! - `GET /emails/{id}` — one email with its body + recipients (the detail view)
//! - `PATCH /emails/{id}` — set the email's read/unread state (`{"unread": bool}`).
//!   **Local only**: it flips the stored `seen` flag, never the provider's (§14),
//!   so a later provider re-sync may overwrite it. `email:write`-gated.
//! - `GET /email/connections` — list this workspace's configured email sources
//! - `GET /email/connections/{id}` — one email source **with its non-secret
//!   settings** (the edit form's prefill; secrets never cross the wire)
//! - `PUT /email/connections/{id}` — update an email source's name/settings.
//!   A blank/omitted secret keeps the stored one (so editing a host never
//!   forces re-entering the password).
//! - `DELETE /email/connections/{id}` — remove an email source (+ its synced mail, `204`)
//! - `POST /email/connections` — register a read-only email source (SOUL §28),
//!   so mail reading can be set up from the workbench (the header "Settings"
//!   surface). Registering a connection provisions **nothing** — it is dormant
//!   until a user-authored automation headed by a `CollectEmail` trigger (filled
//!   with this connection) pulls it on a cadence and a downstream `WriteEmail`
//!   action persists each message (§10/§11). catalerum reads mail, it never
//!   sends/replies (§14).
//!
//! Reads Postgres truth (`MailboxRepo`/`EmailRepo`) — the HTTP sibling of the
//! `get_emails` LLM tool (§7). Bodies are omitted from the listing to keep it
//! light; the detail route carries them. The stored HTML body is the raw
//! provider payload and is **never served as-is**: the detail route passes it
//! through [`sanitize_email_html`] so the workbench only ever receives
//! script-free allowlisted markup.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use catalerum_core::capability::Action;
use catalerum_core::model::{Connection, ConnectionKind, EmailAddress};
use catalerum_core::{ConnectionId, EmailId, MailboxId};
use catalerum_store::StoreError;

use crate::auth::Auth;
use crate::connection_status::ConnectionView;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Mount the email read routes + the email-source (connection) management routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/mailboxes", get(list_mailboxes))
        .route("/emails", get(list_emails))
        .route("/emails/{id}", get(get_email).patch(set_email_read))
        // Singular `/email/connections` (not `/emails/...`) so it never collides
        // with the `/emails/{id}` detail route.
        .route(
            "/email/connections",
            get(list_email_connections).post(create_email_connection),
        )
        .route(
            "/email/connections/{id}",
            get(get_email_connection)
                .put(update_email_connection)
                .delete(delete_email_connection),
        )
}

/// Query for `GET /emails` — optional filters; `limit` clamps to `[1, 200]`.
#[derive(Debug, Default, Deserialize)]
pub struct EmailQuery {
    /// Restrict to a mailbox by name (case-insensitive).
    pub mailbox: Option<String>,
    /// Restrict to a mailbox by **id** — the precise filter the sidebar uses,
    /// since mailbox *names* collide across accounts (every account has an
    /// `INBOX`). Wins over `mailbox` when both are given.
    pub mailbox_id: Option<MailboxId>,
    /// Substring (case-insensitive) the `From` address/name must contain.
    pub sender: Option<String>,
    /// Substring (case-insensitive) the subject or body text must contain.
    pub q: Option<String>,
    /// `true` → only unread, `false` → only read, absent → both.
    pub unread: Option<bool>,
    /// Max results (default 50).
    pub limit: Option<u32>,
}

/// Cap on `also_in` folder names carried per row/detail — enough to orient in the
/// list-row badge tooltip without unbounding the wire (SOUL §29). `folder_count`
/// stays exact regardless of this cap.
const ALSO_IN_CAP: usize = 5;

/// serde `skip_serializing_if` for the `folder_count` default (single-filed → omitted
/// from the wire, so an older client / one that ignores it treats a message as `1`).
fn is_one(n: &usize) -> bool {
    *n == 1
}

/// A compact email list row (no body, to keep listings light).
#[derive(Debug, Serialize)]
pub struct EmailView {
    pub id: EmailId,
    pub mailbox: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub received_at: Option<chrono::DateTime<chrono::Utc>>,
    pub unread: bool,
    pub has_attachments: bool,
    /// How many distinct folders (mailboxes) this message — keyed by its RFC 5322
    /// `Message-ID` — is filed under across the workspace (SOUL §29 cross-folder
    /// dedup). `1` = single-filed (also the case for a row with no `Message-ID`);
    /// `>1` = cross-filed. Omitted on the wire when `1` to keep the listing light.
    #[serde(skip_serializing_if = "is_one")]
    pub folder_count: usize,
    /// The OTHER folders this same message is also filed in — every folder in its
    /// cross-folder set except this row's own mailbox — capped to [`ALSO_IN_CAP`]
    /// for the badge tooltip. Empty (and omitted) for a single-filed message.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub also_in: Vec<String>,
}

/// A single email with its body + recipients (the detail view).
#[derive(Debug, Serialize)]
pub struct EmailDetail {
    pub id: EmailId,
    pub mailbox: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<EmailAddress>,
    pub to: Vec<EmailAddress>,
    pub cc: Vec<EmailAddress>,
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub received_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_html: Option<String>,
    pub unread: bool,
    pub has_attachments: bool,
    /// Archived attachment references (objects in the default files store,
    /// SOUL §28) — empty when nothing was archived. Additive; the web renders
    /// download affordances from these.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<catalerum_core::model::Attachment>,
    /// The archived raw `.eml` object key, when the message was archived.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_ref: Option<String>,
    /// Cross-folder dedup (SOUL §29): how many distinct folders this message is filed
    /// under across the workspace (`1` = single-filed; omitted on the wire when `1`).
    #[serde(skip_serializing_if = "is_one")]
    pub folder_count: usize,
    /// The OTHER folders this message is also filed in (this mailbox removed), capped
    /// to [`ALSO_IN_CAP`]. Empty (and omitted) for a single-filed message.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub also_in: Vec<String>,
}

/// `unread` = the provider `seen` flag is absent.
fn is_unread(flags: &[String]) -> bool {
    !flags.iter().any(|f| f.eq_ignore_ascii_case("seen"))
}

/// Render an optional sender as `"Name <addr>"` / `"addr"`.
fn fmt_from(from: Option<&EmailAddress>) -> Option<String> {
    from.map(|a| match &a.name {
        Some(n) => format!("{n} <{}>", a.address),
        None => a.address.clone(),
    })
}

/// Sanitize a raw ingested HTML body for rendering in the workbench (SOUL §28).
///
/// An allowlist pass (ammonia): scripts, event handlers, forms, frames, and
/// comments are dropped; URLs are constrained to `http(s)`/`mailto`/`tel`/`cid`
/// plus `data:image/*` on `<img src>` **only** — so an inline image survives but
/// a `data:text/html` link can't smuggle a document. Inline `style` and the
/// legacy table/font presentation attributes are kept for fidelity: HTML mail
/// is styled almost entirely through them, and they carry no script surface.
///
/// This is one of three independent layers — the web client renders the result
/// inside a fully sandboxed `<iframe>` (no scripts, opaque origin) whose
/// document carries a CSP denying all remote loads by default, so even markup
/// that slipped an allowlist gap could neither run nor phone home.
fn sanitize_email_html(html: &str) -> String {
    let mut b = ammonia::Builder::default();
    b.add_tags(["center", "font"])
        .add_tag_attributes("font", ["color", "face", "size"])
        // Presentation attributes (inert layout hints) HTML mail still leans on.
        .add_generic_attributes([
            "style",
            "align",
            "valign",
            "width",
            "height",
            "bgcolor",
            "border",
            "cellpadding",
            "cellspacing",
        ])
        .url_schemes(std::collections::HashSet::from([
            "http", "https", "mailto", "tel", "cid", "data",
        ]))
        // `data:` is in the scheme set solely so inline images can survive; this
        // filter drops every other `data:` use. It parses with the same URL
        // parser as ammonia's scheme gate, so a smuggled spelling (tabs/controls
        // inside the scheme) can't pass one check and dodge the other.
        .attribute_filter(|element, attribute, value| {
            if let Ok(url) = ammonia::Url::parse(value) {
                if url.scheme() == "data" {
                    let inline_image = element == "img"
                        && attribute == "src"
                        && url.path().to_ascii_lowercase().starts_with("image/");
                    if !inline_image {
                        return None;
                    }
                }
            }
            Some(value.into())
        })
        .link_rel(Some("noopener noreferrer nofollow"));
    b.clean(html).to_string()
}

/// The cross-folder dedup annotation for one listed/opened message, read out of the
/// page-scoped [`folders_by_message_id`](catalerum_store::EmailRepo::folders_by_message_id)
/// group (SOUL §29): `folder_count` is how many distinct folders share its `Message-ID`
/// (`1` when it has none, or is filed once), and `also_in` is the OTHER folder names (its
/// own `mailbox` removed), capped to [`ALSO_IN_CAP`] for the badge tooltip. A message with
/// no `Message-ID` — or one the group query didn't return — is single-filed (`1`, empty).
fn cross_folder_context(
    message_id: Option<&str>,
    mailbox: &str,
    folders_by_mid: &HashMap<String, Vec<String>>,
) -> (usize, Vec<String>) {
    let Some(folders) = message_id.and_then(|mid| folders_by_mid.get(mid)) else {
        return (1, Vec::new());
    };
    let folder_count = folders.len().max(1);
    let mut also_in: Vec<String> = folders
        .iter()
        .filter(|name| name.as_str() != mailbox)
        .cloned()
        .collect();
    also_in.truncate(ALSO_IN_CAP);
    (folder_count, also_in)
}

fn map_email_err(e: StoreError) -> ApiError {
    match e {
        StoreError::NotFound => ApiError::NotFound,
        other => ApiError::internal(format!("email lookup: {other}")),
    }
}

/// A mailbox as the sidebar consumes it (SOUL §28): the core [`Mailbox`] fields
/// (same names, so an older client keeps parsing) plus its **unread count** and
/// the owning connection's display name — everything the account-grouped
/// sidebar needs in one round trip.
#[derive(Debug, Serialize)]
pub struct MailboxView {
    pub id: MailboxId,
    pub workspace_id: catalerum_core::WorkspaceId,
    pub connection_id: ConnectionId,
    /// The owning email source's display name — the sidebar's account header.
    /// Empty when the connection row is gone mid-delete (cascade in flight).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub connection_name: String,
    pub external_id: String,
    pub name: String,
    pub read_only: bool,
    /// How many stored emails in this mailbox lack the `seen` flag.
    pub unread_count: i64,
}

async fn list_mailboxes(
    State(state): State<AppState>,
    auth: Auth,
) -> ApiResult<Json<Vec<MailboxView>>> {
    let p = auth.principal();
    auth.require(Action::Read, "email")?;
    let store = state.store();
    let mailboxes = store
        .mailboxes()
        .list_by_workspace(p.workspace_id)
        .await
        .map_err(|e| ApiError::internal(format!("listing mailboxes: {e}")))?;
    // ONE grouped query for every badge number (never a count per mailbox).
    let unread = store
        .emails()
        .unread_counts_by_mailbox(p.workspace_id)
        .await
        .map_err(|e| ApiError::internal(format!("counting unread email: {e}")))?;
    // Resolve owning-connection names so the sidebar can group by account.
    let conn_names: HashMap<ConnectionId, String> = store
        .connections()
        .list_by_workspace(p.workspace_id)
        .await
        .map_err(|e| ApiError::internal(format!("listing email connections: {e}")))?
        .into_iter()
        .filter(|c| c.kind == ConnectionKind::Email)
        .map(|c| (c.id, c.name))
        .collect();
    let views = mailboxes
        .into_iter()
        .map(|m| MailboxView {
            unread_count: unread.get(&m.id).copied().unwrap_or(0),
            connection_name: conn_names
                .get(&m.connection_id)
                .cloned()
                .unwrap_or_default(),
            id: m.id,
            workspace_id: m.workspace_id,
            connection_id: m.connection_id,
            external_id: m.external_id,
            name: m.name,
            read_only: m.read_only,
        })
        .collect::<Vec<_>>();
    Ok(Json(views))
}

async fn list_emails(
    State(state): State<AppState>,
    auth: Auth,
    Query(q): Query<EmailQuery>,
) -> ApiResult<Json<Vec<EmailView>>> {
    let p = auth.principal();
    auth.require(Action::Read, "email")?;
    let store = state.store();
    let ws = p.workspace_id;

    // Index mailbox_id → name so each row carries where it lives, not an id.
    let mailboxes = store
        .mailboxes()
        .list_by_workspace(ws)
        .await
        .map_err(|e| ApiError::internal(format!("listing mailboxes: {e}")))?;
    let mb_index: HashMap<MailboxId, String> =
        mailboxes.iter().map(|m| (m.id, m.name.clone())).collect();

    let limit = q.limit.unwrap_or(50).clamp(1, 200);

    // Resolve the mailbox filter: an explicit `mailbox_id` wins (names collide
    // across accounts — every account has an `INBOX`), else an optional `mailbox`
    // name (an unknown name → empty, as before). The sender/content/unread
    // predicates run **in SQL** so the limit bounds *matching* rows — a match in
    // mail older than the limit is still found, not silently dropped by a
    // scan-then-filter window (SOUL §28).
    let mailbox_id = match (q.mailbox_id, &q.mailbox) {
        (Some(id), _) => Some(id),
        (None, Some(name)) => match mailboxes.iter().find(|m| m.name.eq_ignore_ascii_case(name)) {
            Some(mb) => Some(mb.id),
            None => return Ok(Json(Vec::new())),
        },
        (None, None) => None,
    };
    let sender = q.sender.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let content = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty());

    let emails = store
        .emails()
        .search_in_workspace(ws, mailbox_id, content, sender, q.unread, limit as i64)
        .await
        .map_err(|e| ApiError::internal(format!("searching emails: {e}")))?;

    // Cross-folder dedup annotation (SOUL §29): ONE grouped query over just this page's
    // distinct non-null `Message-ID`s (never one-per-row) tells us, per message, every
    // folder it's filed under workspace-wide — so a cross-filed row can show "also in N
    // other folders". Rows with no `Message-ID` can't group → `folder_count` stays 1.
    let mut page_mids: Vec<String> = emails.iter().filter_map(|e| e.message_id.clone()).collect();
    page_mids.sort();
    page_mids.dedup();
    let folders_by_mid = store
        .emails()
        .folders_by_message_id(ws, &page_mids)
        .await
        .map_err(|e| ApiError::internal(format!("grouping cross-folder mail: {e}")))?;

    let views: Vec<EmailView> = emails
        .into_iter()
        .map(|e| {
            let mailbox = mb_index.get(&e.mailbox_id).cloned().unwrap_or_default();
            let (folder_count, also_in) =
                cross_folder_context(e.message_id.as_deref(), &mailbox, &folders_by_mid);
            EmailView {
                id: e.id,
                mailbox,
                from: fmt_from(e.from.as_ref()),
                subject: e.subject,
                received_at: e.received_at,
                unread: is_unread(&e.flags),
                has_attachments: e.has_attachments,
                folder_count,
                also_in,
            }
        })
        .collect();
    Ok(Json(views))
}

async fn get_email(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<Json<EmailDetail>> {
    let p = auth.principal();
    auth.require(Action::Read, "email")?;
    let email_id: EmailId = id
        .parse()
        .map_err(|_| ApiError::bad_request("invalid email id"))?;
    let store = state.store();
    let email = store
        .emails()
        .get(p.workspace_id, email_id)
        .await
        .map_err(map_email_err)?;
    // Resolve the mailbox name (best-effort — a missing mailbox degrades to "").
    let mailbox = store
        .mailboxes()
        .get(p.workspace_id, email.mailbox_id)
        .await
        .map(|m| m.name)
        .unwrap_or_default();
    let unread = is_unread(&email.flags);
    // Cross-folder dedup annotation (SOUL §29): the same message may be filed in several
    // folders — surface the OTHER ones next to the mailbox name. One grouped query keyed
    // by this message's `Message-ID` (none → single-filed).
    let (folder_count, also_in) = match email.message_id.clone() {
        Some(mid) => {
            let folders_by_mid = store
                .emails()
                .folders_by_message_id(p.workspace_id, std::slice::from_ref(&mid))
                .await
                .map_err(map_email_err)?;
            cross_folder_context(Some(&mid), &mailbox, &folders_by_mid)
        }
        None => (1, Vec::new()),
    };
    Ok(Json(EmailDetail {
        id: email.id,
        mailbox,
        message_id: email.message_id,
        from: email.from,
        to: email.to,
        cc: email.cc,
        subject: email.subject,
        received_at: email.received_at,
        body_text: email.body_text,
        // Raw provider HTML never crosses the wire — see `sanitize_email_html`.
        body_html: email.body_html.as_deref().map(sanitize_email_html),
        unread,
        has_attachments: email.has_attachments,
        attachments: email.attachments,
        raw_ref: email.raw_ref,
        folder_count,
        also_in,
    }))
}

/// Body for `PATCH /emails/{id}` — set the read/unread state. Field-named after
/// [`EmailView::unread`] so the client toggles the same flag it renders.
#[derive(Debug, Deserialize)]
pub struct SetEmailRead {
    /// `true` → mark unread (strip the local `seen` flag); `false` → mark read.
    pub unread: bool,
}

/// Response for `PATCH /emails/{id}` — the email's new read state.
#[derive(Debug, Serialize)]
pub struct EmailReadState {
    pub id: EmailId,
    pub unread: bool,
}

/// `PATCH /emails/{id}` — mark an email read/unread (SOUL §28). **Local only**:
/// this flips catalerum's stored `seen` flag, never the provider's (§14 — the
/// remote mailbox is untouched, and a provider re-sync that carries fresh flags
/// may overwrite it). Gated `email:write`; `404` for a foreign/unknown id.
async fn set_email_read(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
    Json(body): Json<SetEmailRead>,
) -> ApiResult<Json<EmailReadState>> {
    let p = auth.principal();
    auth.require(Action::Write, "email")?;
    let email_id: EmailId = id
        .parse()
        .map_err(|_| ApiError::bad_request("invalid email id"))?;
    let updated = state
        .store()
        .emails()
        .set_seen(p.workspace_id, email_id, !body.unread)
        .await
        .map_err(map_email_err)?;
    Ok(Json(EmailReadState {
        id: updated.id,
        unread: is_unread(&updated.flags),
    }))
}

// ---------------------------------------------------------------------------
// Email sources (connections)
// ---------------------------------------------------------------------------

/// The provider sub-kind of an email connection. The core
/// [`ConnectionKind`](catalerum_core::model::ConnectionKind) stays abstract
/// (`Email`); the concrete provider rides in the connection `config` blob
/// (SOUL §3.2/§28). This is the wire token the client sends. All four backends
/// are implemented (`catalerum-email`): a local Maildir plus the network
/// providers IMAP, JMAP, and Gmail.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmailProviderKind {
    /// A local Maildir directory (`new/`/`cur/`/`tmp/`). Read-only.
    #[default]
    Maildir,
    /// RFC 3501 IMAP over TLS.
    Imap,
    /// RFC 8621 JMAP over HTTP.
    Jmap,
    /// The Gmail API (OAuth2 refresh-token grant).
    Gmail,
}

impl EmailProviderKind {
    /// The stable token persisted in `connections.config.provider`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EmailProviderKind::Maildir => "maildir",
            EmailProviderKind::Imap => "imap",
            EmailProviderKind::Jmap => "jmap",
            EmailProviderKind::Gmail => "gmail",
        }
    }

    /// Parse a stored `connections.config.provider` token back to its kind —
    /// the inverse of [`Self::as_str`]. `None` for an unknown/absent token.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "maildir" => Some(EmailProviderKind::Maildir),
            "imap" => Some(EmailProviderKind::Imap),
            "jmap" => Some(EmailProviderKind::Jmap),
            "gmail" => Some(EmailProviderKind::Gmail),
            _ => None,
        }
    }

    /// The `config` keys that hold **secrets** for this provider — the fields
    /// the detail view must never serve and the update route back-fills from
    /// the stored blob when the client omits them (an "unchanged" edit).
    #[must_use]
    pub fn secret_keys(self) -> &'static [&'static str] {
        match self {
            EmailProviderKind::Maildir => &[],
            EmailProviderKind::Imap => &["password"],
            EmailProviderKind::Jmap => &["token"],
            EmailProviderKind::Gmail => &["client_secret", "refresh_token"],
        }
    }
}

/// Body for `POST /email/connections` — register a read-only email source
/// (SOUL §28). The abstract core kind is always `Email`; the provider sub-kind +
/// settings persist in the connection `config`. Read-only ingest (catalerum reads
/// mail, never sends/replies, §14). Only the fields relevant to the chosen
/// `provider` are read; the rest are ignored.
#[derive(Debug, Default, Deserialize)]
pub struct CreateEmailConnection {
    /// Provider sub-kind: `maildir` | `imap` | `jmap` | `gmail`.
    pub provider: EmailProviderKind,
    /// Human-readable name for the source.
    pub name: String,
    /// **Maildir**: the directory that contains `new/`/`cur/`/`tmp/`.
    #[serde(default)]
    pub root: String,
    /// **Maildir/IMAP**: mailbox/folder name (defaults to `"INBOX"`).
    #[serde(default)]
    pub mailbox: Option<String>,
    /// **IMAP**: server hostname.
    #[serde(default)]
    pub host: Option<String>,
    /// **IMAP**: server port (defaults to 993, implicit TLS).
    #[serde(default)]
    pub port: Option<u16>,
    /// **IMAP**: login username.
    #[serde(default)]
    pub username: Option<String>,
    /// **IMAP**: login password.
    #[serde(default)]
    pub password: Option<String>,
    /// **JMAP**: session resource URL.
    #[serde(default)]
    pub session_url: Option<String>,
    /// **JMAP**: bearer token.
    #[serde(default)]
    pub token: Option<String>,
    /// **JMAP**: optional account-id override.
    #[serde(default)]
    pub account_id: Option<String>,
    /// **Gmail**: OAuth2 client id.
    #[serde(default)]
    pub client_id: Option<String>,
    /// **Gmail**: OAuth2 client secret.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// **Gmail**: long-lived OAuth2 refresh token.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// **Gmail**: label id to ingest (defaults to `"INBOX"`).
    #[serde(default)]
    pub label: Option<String>,
}

/// A required string field: trimmed, non-empty, else `Err(message)`.
fn req(value: &Option<String>, provider: &str, field: &str) -> Result<String, String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("{provider} email connection requires a non-empty `{field}`"))
}

/// An optional string field: trimmed, dropped when blank/absent.
fn opt(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Build the stored `config` blob for an email connection from its create body —
/// the exact shape `catalerum_email::provider_from_connection` reads for the
/// chosen provider. Returns `Err(message)` when a required field is missing.
/// Secrets ride in the config blob in plaintext (the M-stage stub the CalDAV
/// provider also uses; the encrypted vault behind `credential_ref` lands later,
/// SOUL §13). `pub(crate)` so the `create_email_connection` LLM tool builds the
/// identical blob (SOUL §7/§28) — one config shape, two authoring surfaces.
pub(crate) fn build_email_config(
    body: &CreateEmailConnection,
) -> Result<serde_json::Value, String> {
    use serde_json::json;
    match body.provider {
        EmailProviderKind::Maildir => {
            let root = body.root.trim();
            if root.is_empty() {
                return Err(
                    "maildir email connection requires a non-empty `root` directory".to_string(),
                );
            }
            let mailbox = opt(&body.mailbox).unwrap_or_else(|| "INBOX".to_string());
            Ok(json!({ "provider": body.provider.as_str(), "root": root, "name": mailbox }))
        }
        EmailProviderKind::Imap => {
            let mut cfg = json!({
                "provider": body.provider.as_str(),
                "host": req(&body.host, "imap", "host")?,
                "username": req(&body.username, "imap", "username")?,
                "password": req(&body.password, "imap", "password")?,
                "mailbox": opt(&body.mailbox).unwrap_or_else(|| "INBOX".to_string()),
            });
            if let Some(port) = body.port {
                cfg["port"] = port.into();
            }
            Ok(cfg)
        }
        EmailProviderKind::Jmap => {
            let mut cfg = json!({
                "provider": body.provider.as_str(),
                "session_url": req(&body.session_url, "jmap", "session_url")?,
                "token": req(&body.token, "jmap", "token")?,
            });
            if let Some(account_id) = opt(&body.account_id) {
                cfg["account_id"] = account_id.into();
            }
            Ok(cfg)
        }
        EmailProviderKind::Gmail => Ok(json!({
            "provider": body.provider.as_str(),
            "client_id": req(&body.client_id, "gmail", "client_id")?,
            "client_secret": req(&body.client_secret, "gmail", "client_secret")?,
            "refresh_token": req(&body.refresh_token, "gmail", "refresh_token")?,
            "label": opt(&body.label).unwrap_or_else(|| "INBOX".to_string()),
        })),
    }
}

/// `POST /email/connections` — register an email source. `email:write`-gated
/// (every role but Viewer). Provisions nothing on its own: a `CollectEmail`
/// automation trigger filled with this connection is what pulls it (SOUL §10/§28).
async fn create_email_connection(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreateEmailConnection>,
) -> ApiResult<(StatusCode, Json<Connection>)> {
    auth.require(Action::Write, "email")?;
    let ws = auth.principal().workspace_id;
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("connection name must not be empty"));
    }
    let config = build_email_config(&body).map_err(ApiError::bad_request)?;
    let connection = state
        .store()
        .connections()
        .create(ws, ConnectionKind::Email, name, None, Some(config))
        .await
        .map_err(|e| ApiError::internal(format!("creating email connection: {e}")))?;
    Ok((StatusCode::CREATED, Json(connection)))
}

/// `DELETE /email/connections/{id}` — remove an email source (connection) and,
/// via the `ON DELETE CASCADE` FKs, its synced mailboxes + emails. The remote
/// mailbox is untouched (re-adding re-syncs). Gated `email:write` (symmetric with
/// create); `404` for a foreign/unknown id; `400` if it isn't an email connection.
async fn delete_email_connection(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConnectionId>,
) -> ApiResult<StatusCode> {
    auth.require(Action::Write, "email")?;
    let ws = auth.principal().workspace_id;
    let connection = state
        .store()
        .connections()
        .get(ws, id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    if connection.kind != ConnectionKind::Email {
        return Err(ApiError::bad_request("not an email connection"));
    }
    state.store().connections().delete(ws, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /email/connections` — this workspace's email sources (the abstract
/// `Connection`s of kind `Email`), newest first. `email:read`-gated.
///
/// Each source is annotated with its **collect status** (SOUL §29): `collecting`
/// is `true` only when an enabled automation heads a `CollectEmail` trigger at it.
/// A `collecting: false` source is **dormant** — registered but nothing will ever
/// ingest from it (adding a connection provisions nothing, §28) — which the UI
/// surfaces as an "idle" warning so the "I added my email but see no mail" trap is
/// no longer silent. Presentation-only; no mutation.
async fn list_email_connections(
    State(state): State<AppState>,
    auth: Auth,
) -> ApiResult<Json<Vec<ConnectionView>>> {
    auth.require(Action::Read, "email")?;
    let ws = auth.principal().workspace_id;
    let connections: Vec<Connection> = state
        .store()
        .connections()
        .list_by_workspace(ws)
        .await
        .map_err(|e| ApiError::internal(format!("listing email connections: {e}")))?
        .into_iter()
        .filter(|c| c.kind == ConnectionKind::Email)
        .collect();
    // Scan the workspace's automations once to decide which sources are live
    // (SOUL §29). A read-only projection over automations the caller can already
    // see; only the derived boolean is returned, never automation contents.
    let automations = state
        .store()
        .automations()
        .list_by_workspace(ws)
        .await
        .map_err(|e| ApiError::internal(format!("listing automations: {e}")))?;
    Ok(Json(crate::connection_status::annotate(
        connections,
        &automations,
    )))
}

/// One email source with its **non-secret** settings (SOUL §28) — the edit
/// form's prefill for `GET /email/connections/{id}`. Secrets (passwords /
/// tokens) NEVER cross the wire: `has_secrets` only says whether any are
/// stored, so the form can render "(unchanged)" placeholders.
#[derive(Debug, Serialize)]
pub struct EmailConnectionDetail {
    pub id: ConnectionId,
    pub name: String,
    pub provider: EmailProviderKind,
    /// Every non-secret provider setting from the stored config blob
    /// (`root`/`host`/`port`/`username`/`mailbox`/`session_url`/…). Secret keys
    /// are stripped server-side.
    pub settings: serde_json::Map<String, serde_json::Value>,
    /// Whether any secret (password / token / client secret) is stored — the
    /// values themselves are never served.
    pub has_secrets: bool,
}

/// Load an **email** connection's full row, workspace-scoped: `404` for a
/// foreign/unknown id, `400` when the id names a non-email connection. The
/// shared front half of the detail/update routes.
async fn load_email_connection_row(
    state: &AppState,
    ws: catalerum_core::WorkspaceId,
    id: ConnectionId,
) -> Result<catalerum_store::ConnectionRow, ApiError> {
    let connection = state
        .store()
        .connections()
        .get(ws, id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    if connection.kind != ConnectionKind::Email {
        return Err(ApiError::bad_request("not an email connection"));
    }
    state
        .store()
        .connections()
        .get_row(ws, id)
        .await
        .map_err(|_| ApiError::NotFound)
}

/// `GET /email/connections/{id}` — one email source with its non-secret
/// settings (the edit form's prefill). Gated `email:write` — the settings blob
/// (hosts, usernames) is an **editing** surface, not part of the read view the
/// connection listing serves.
async fn get_email_connection(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConnectionId>,
) -> ApiResult<Json<EmailConnectionDetail>> {
    auth.require(Action::Write, "email")?;
    let ws = auth.principal().workspace_id;
    let row = load_email_connection_row(&state, ws, id).await?;
    let config = row.config();
    let provider = config
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .and_then(EmailProviderKind::from_token)
        .unwrap_or_default();
    let secret_keys = provider.secret_keys();
    let mut settings = serde_json::Map::new();
    let mut has_secrets = false;
    if let Some(obj) = config.as_object() {
        for (k, v) in obj {
            if k == "provider" {
                continue;
            }
            if secret_keys.contains(&k.as_str()) {
                has_secrets = has_secrets || !v.is_null();
            } else {
                settings.insert(k.clone(), v.clone());
            }
        }
    }
    let connection: Connection = row
        .try_into()
        .map_err(|e| ApiError::internal(format!("email connection row: {e}")))?;
    Ok(Json(EmailConnectionDetail {
        id: connection.id,
        name: connection.name,
        provider,
        settings,
        has_secrets,
    }))
}

/// `PUT /email/connections/{id}` — update an email source's name + settings
/// (SOUL §28). Takes the same body as create; a **blank/omitted secret keeps
/// the stored one** (so editing a host never forces re-typing the password) —
/// unless the provider kind changed, in which case the old secrets don't apply
/// and the body must carry the new provider's full credentials. Gated
/// `email:write`; `404` foreign/unknown, `400` non-email.
async fn update_email_connection(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConnectionId>,
    Json(mut body): Json<CreateEmailConnection>,
) -> ApiResult<Json<Connection>> {
    auth.require(Action::Write, "email")?;
    let ws = auth.principal().workspace_id;
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request("connection name must not be empty"));
    }
    let row = load_email_connection_row(&state, ws, id).await?;
    let prev = row.config();
    // Back-fill omitted secrets from the stored blob — only when the provider is
    // unchanged (another provider's stored secret is meaningless to this one).
    let same_provider =
        prev.get("provider").and_then(serde_json::Value::as_str) == Some(body.provider.as_str());
    if same_provider {
        let stored = |key: &str| {
            prev.get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        let keep = |field: &mut Option<String>, key: &str| {
            if opt(field).is_none() {
                *field = stored(key);
            }
        };
        keep(&mut body.password, "password");
        keep(&mut body.token, "token");
        keep(&mut body.client_secret, "client_secret");
        keep(&mut body.refresh_token, "refresh_token");
    }
    let config = build_email_config(&body).map_err(ApiError::bad_request)?;
    let updated = state
        .store()
        .connections()
        .update_named_config(ws, id, &name, config)
        .await
        .map_err(|e| ApiError::internal(format!("updating email connection: {e}")))?;
    Ok(Json(updated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_scripts_and_event_handlers() {
        let dirty = r#"<p onmouseover="steal()">Hi</p><script>alert(1)</script><img src="https://x.com/a.png" onerror="steal()">"#;
        let clean = sanitize_email_html(dirty);
        assert!(!clean.contains("script"), "script tag dropped: {clean}");
        assert!(!clean.contains("onerror"), "event handler dropped: {clean}");
        assert!(
            !clean.contains("onmouseover"),
            "event handler dropped: {clean}"
        );
        assert!(clean.contains("Hi"), "text content survives: {clean}");
        assert!(
            clean.contains(r#"src="https://x.com/a.png""#),
            "https image survives: {clean}"
        );
    }

    #[test]
    fn sanitize_constrains_url_schemes() {
        let clean = sanitize_email_html(
            r#"<a href="javascript:alert(1)">j</a><a href="https://x.com">ok</a><a href="mailto:a@x.com">m</a>"#,
        );
        assert!(
            !clean.contains("javascript:"),
            "javascript: dropped: {clean}"
        );
        assert!(clean.contains(r#"href="https://x.com""#), "{clean}");
        assert!(clean.contains(r#"href="mailto:a@x.com""#), "{clean}");
        // Every link is force-annotated against tab-napping / referer leaks.
        assert!(clean.contains("noopener"), "link rel forced: {clean}");
    }

    #[test]
    fn sanitize_allows_data_urls_only_for_inline_images() {
        // An inline image survives…
        let img = sanitize_email_html(r#"<img src="data:image/png;base64,AAAA">"#);
        assert!(img.contains("data:image/png"), "inline image kept: {img}");
        // …but a data: document link is dropped, even with a smuggled scheme
        // spelling (the URL parser strips tabs/newlines before reading it).
        let link = sanitize_email_html(
            "<a href=\"data:text/html,<script>x</script>\">a</a><a href=\"da\tta:text/html,x\">b</a>",
        );
        assert!(!link.contains("data:"), "data: link dropped: {link}");
        assert!(
            !link.contains("ta:text"),
            "smuggled data: link dropped: {link}"
        );
        // …and a data: *non-image* img src is dropped too.
        let doc_img = sanitize_email_html(r#"<img src="data:text/html,x">"#);
        assert!(
            !doc_img.contains("data:"),
            "non-image data: src dropped: {doc_img}"
        );
    }

    #[test]
    fn sanitize_keeps_email_presentation_markup() {
        let clean = sanitize_email_html(
            r##"<table width="600" bgcolor="#ffffff"><tr><td style="padding:8px" align="center"><font color="#333333">x</font></td></tr></table><center>y</center>"##,
        );
        assert!(clean.contains(r#"width="600""#), "{clean}");
        assert!(clean.contains(r#"style="padding:8px""#), "{clean}");
        assert!(clean.contains("<font"), "{clean}");
        assert!(clean.contains("<center>"), "{clean}");
        // cid: references survive (they simply don't resolve in the workbench).
        let cid = sanitize_email_html(r#"<img src="cid:part1@x">"#);
        assert!(cid.contains("cid:part1@x"), "{cid}");
    }

    #[test]
    fn unread_when_no_seen_flag() {
        assert!(is_unread(&["flagged".into()]));
        assert!(!is_unread(&["Seen".into(), "flagged".into()]));
        assert!(is_unread(&[]));
    }

    #[test]
    fn from_formatting() {
        assert_eq!(
            fmt_from(Some(&EmailAddress {
                name: Some("Ada".into()),
                address: "ada@x.com".into()
            })),
            Some("Ada <ada@x.com>".to_string())
        );
        assert_eq!(
            fmt_from(Some(&EmailAddress::new("bob@x.com"))),
            Some("bob@x.com".to_string())
        );
        assert_eq!(fmt_from(None), None);
    }

    #[test]
    fn cross_folder_context_computes_count_and_others() {
        use std::collections::HashMap;
        let mut folders: HashMap<String, Vec<String>> = HashMap::new();
        folders.insert(
            "<shared@x>".into(),
            vec!["Archive".into(), "INBOX".into(), "Sent".into()],
        );
        folders.insert("<solo@x>".into(), vec!["INBOX".into()]);

        // Cross-filed: count is all folders; also_in drops the row's own mailbox.
        let (count, also) = cross_folder_context(Some("<shared@x>"), "INBOX", &folders);
        assert_eq!(count, 3);
        assert_eq!(also, vec!["Archive".to_string(), "Sent".to_string()]);

        // Single-filed → 1, empty.
        assert_eq!(
            cross_folder_context(Some("<solo@x>"), "INBOX", &folders),
            (1, Vec::new())
        );
        // No Message-ID, or an id the group query didn't return → single-filed.
        assert_eq!(
            cross_folder_context(None, "INBOX", &folders),
            (1, Vec::new())
        );
        assert_eq!(
            cross_folder_context(Some("<unknown@x>"), "INBOX", &folders),
            (1, Vec::new())
        );
    }

    #[test]
    fn cross_folder_context_caps_also_in() {
        use std::collections::HashMap;
        // Seven folders (six others once own is removed) → also_in capped at ALSO_IN_CAP,
        // but folder_count stays exact.
        let names: Vec<String> = (0..7).map(|i| format!("F{i}")).collect();
        let mut folders: HashMap<String, Vec<String>> = HashMap::new();
        folders.insert("<m@x>".into(), names);
        let (count, also) = cross_folder_context(Some("<m@x>"), "F0", &folders);
        assert_eq!(count, 7);
        assert_eq!(also.len(), ALSO_IN_CAP);
    }

    #[test]
    fn email_view_serde_omits_single_filed_defaults() {
        // A single-filed row omits both cross-folder fields on the wire.
        let solo = EmailView {
            id: EmailId::new(),
            mailbox: "INBOX".into(),
            from: None,
            subject: "Hi".into(),
            received_at: None,
            unread: false,
            has_attachments: false,
            folder_count: 1,
            also_in: Vec::new(),
        };
        let v = serde_json::to_value(&solo).unwrap();
        assert!(v.get("folder_count").is_none(), "default count omitted");
        assert!(v.get("also_in").is_none(), "empty also_in omitted");

        // A cross-filed row carries both.
        let dup = EmailView {
            folder_count: 3,
            also_in: vec!["Archive".into(), "Sent".into()],
            ..solo
        };
        let v = serde_json::to_value(&dup).unwrap();
        assert_eq!(v["folder_count"], 3);
        assert_eq!(v["also_in"], serde_json::json!(["Archive", "Sent"]));
    }

    #[test]
    fn email_query_defaults() {
        let q: EmailQuery = serde_json::from_str("{}").unwrap();
        assert!(
            q.mailbox.is_none() && q.sender.is_none() && q.unread.is_none() && q.limit.is_none()
        );
    }

    fn maildir_body(root: &str, mailbox: Option<&str>) -> CreateEmailConnection {
        CreateEmailConnection {
            provider: EmailProviderKind::Maildir,
            name: "Inbox".to_string(),
            root: root.to_string(),
            mailbox: mailbox.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn email_provider_kind_tokens_round_trip() {
        assert_eq!(EmailProviderKind::Maildir.as_str(), "maildir");
        assert_eq!(EmailProviderKind::Imap.as_str(), "imap");
        assert_eq!(EmailProviderKind::Gmail.as_str(), "gmail");
        let k: EmailProviderKind = serde_json::from_value(serde_json::json!("jmap")).unwrap();
        assert_eq!(k, EmailProviderKind::Jmap);
        assert!(serde_json::from_value::<EmailProviderKind>(serde_json::json!("smtp")).is_err());
        // from_token inverts as_str for every kind; unknown tokens parse to None.
        for k in [
            EmailProviderKind::Maildir,
            EmailProviderKind::Imap,
            EmailProviderKind::Jmap,
            EmailProviderKind::Gmail,
        ] {
            assert_eq!(EmailProviderKind::from_token(k.as_str()), Some(k));
        }
        assert_eq!(EmailProviderKind::from_token("smtp"), None);
    }

    #[test]
    fn secret_keys_cover_every_credential_build_writes() {
        // Every secret `build_email_config` persists MUST be in its provider's
        // `secret_keys`, or the detail route would serve it to the client.
        let imap = build_email_config(&CreateEmailConnection {
            provider: EmailProviderKind::Imap,
            name: "w".into(),
            host: Some("h".into()),
            username: Some("u".into()),
            password: Some("pw".into()),
            ..Default::default()
        })
        .unwrap();
        let jmap = build_email_config(&CreateEmailConnection {
            provider: EmailProviderKind::Jmap,
            name: "f".into(),
            session_url: Some("https://x/jmap".into()),
            token: Some("tok".into()),
            ..Default::default()
        })
        .unwrap();
        let gmail = build_email_config(&CreateEmailConnection {
            provider: EmailProviderKind::Gmail,
            name: "g".into(),
            client_id: Some("cid".into()),
            client_secret: Some("csec".into()),
            refresh_token: Some("rtok".into()),
            ..Default::default()
        })
        .unwrap();
        for (kind, cfg, secrets) in [
            (EmailProviderKind::Imap, &imap, vec!["pw"]),
            (EmailProviderKind::Jmap, &jmap, vec!["tok"]),
            (EmailProviderKind::Gmail, &gmail, vec!["csec", "rtok"]),
        ] {
            let keys = kind.secret_keys();
            for (k, v) in cfg.as_object().unwrap() {
                let is_secret_value = secrets.contains(&v.as_str().unwrap_or_default());
                assert_eq!(
                    keys.contains(&k.as_str()),
                    is_secret_value,
                    "{kind:?} key {k} misclassified"
                );
            }
        }
        assert!(EmailProviderKind::Maildir.secret_keys().is_empty());
    }

    #[test]
    fn set_email_read_body_parses() {
        let b: SetEmailRead = serde_json::from_value(serde_json::json!({"unread": false})).unwrap();
        assert!(!b.unread);
        assert!(serde_json::from_value::<SetEmailRead>(serde_json::json!({})).is_err());
    }

    #[test]
    fn build_email_config_maildir_trims_and_defaults_mailbox() {
        // Maildir with an explicit mailbox: trimmed root + name.
        let cfg = build_email_config(&maildir_body(" /var/mail/me ", Some(" Archive "))).unwrap();
        assert_eq!(cfg["provider"], "maildir");
        assert_eq!(cfg["root"], "/var/mail/me");
        assert_eq!(cfg["name"], "Archive");
        // No mailbox (or blank) → defaults to INBOX.
        assert_eq!(
            build_email_config(&maildir_body("/m", None)).unwrap()["name"],
            "INBOX"
        );
        assert_eq!(
            build_email_config(&maildir_body("/m", Some("  "))).unwrap()["name"],
            "INBOX"
        );
    }

    #[test]
    fn build_email_config_rejects_blank_root() {
        assert!(build_email_config(&maildir_body("   ", None)).is_err());
    }

    #[test]
    fn build_email_config_imap_requires_host_user_pass_and_defaults() {
        let ok = CreateEmailConnection {
            provider: EmailProviderKind::Imap,
            name: "Work".into(),
            host: Some("imap.example.com".into()),
            username: Some("me".into()),
            password: Some("pw".into()),
            port: Some(993),
            ..Default::default()
        };
        let cfg = build_email_config(&ok).unwrap();
        assert_eq!(cfg["provider"], "imap");
        assert_eq!(cfg["host"], "imap.example.com");
        assert_eq!(cfg["mailbox"], "INBOX");
        assert_eq!(cfg["port"], 993);
        // Missing password → error.
        let bad = CreateEmailConnection {
            password: None,
            ..ok
        };
        assert!(build_email_config(&bad).is_err());
    }

    #[test]
    fn build_email_config_jmap_requires_url_and_token() {
        let ok = CreateEmailConnection {
            provider: EmailProviderKind::Jmap,
            name: "Fastmail".into(),
            session_url: Some("https://api.fastmail.com/jmap/session".into()),
            token: Some("secret".into()),
            account_id: Some("acc1".into()),
            ..Default::default()
        };
        let cfg = build_email_config(&ok).unwrap();
        assert_eq!(cfg["provider"], "jmap");
        assert_eq!(cfg["account_id"], "acc1");
        let bad = CreateEmailConnection { token: None, ..ok };
        assert!(build_email_config(&bad).is_err());
    }

    #[test]
    fn build_email_config_gmail_requires_oauth_triplet_and_defaults_label() {
        let ok = CreateEmailConnection {
            provider: EmailProviderKind::Gmail,
            name: "Personal".into(),
            client_id: Some("cid".into()),
            client_secret: Some("csec".into()),
            refresh_token: Some("rtok".into()),
            ..Default::default()
        };
        let cfg = build_email_config(&ok).unwrap();
        assert_eq!(cfg["provider"], "gmail");
        assert_eq!(cfg["label"], "INBOX");
        let bad = CreateEmailConnection {
            refresh_token: None,
            ..ok
        };
        assert!(build_email_config(&bad).is_err());
    }
}
