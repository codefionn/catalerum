//! catalerum-email — concrete [`EmailProvider`] backends (SOUL §28).
//!
//! The trait itself lives in `catalerum-core`
//! ([`catalerum_core::EmailProvider`]); this crate provides the implementations.
//! Email is a **read-only** ingest source, the same shape as calendars (§8) and
//! storage (§9): pull messages on a cursor, normalize to the canonical
//! [`Email`], and (later, in the ingest layer) land them in Postgres + chunk /
//! embed / project. catalerum reads mail; it is **not** a mail client — no
//! composing, sending, or replying (§14).
//!
//! - [`MaildirProvider`] — a local **Maildir** directory (`new/` + `cur/`),
//!   parsed with `mailparse`. Read-only; content-hash cursor; idempotent
//!   re-sync. The local-first dev fixture (mirrors local `.ics`, principle 8).
//! - [`imap::ImapProvider`] — RFC 3501 IMAP over TLS (`async-imap`). Incremental
//!   by `UIDVALIDITY` + a per-uid flag signature; emits explicit deletions.
//! - [`jmap::JmapProvider`] — RFC 8621 JMAP over HTTP (`reqwest`). Incremental by
//!   the account's `Email/changes` state.
//! - [`gmail::GmailProvider`] — the Gmail API over HTTP (`reqwest`), authorized by
//!   an OAuth2 refresh-token grant. Incremental by `history.list`.
//!
//! The three network providers are **incremental deltas**
//! ([`EmailProvider::is_incremental`] is `true`): `sync` returns only the
//! messages that changed since the cursor and names every removal in
//! `deletions`, so the ingest worker treats them as authoritative and never
//! diff-reconciles (which would mistake a small delta of new mail for a
//! wholesale deletion).
//!
//! # Maildir layout & identity
//! A Maildir is a directory with `tmp/`, `new/`, and `cur/` subdirectories. A
//! delivered message is a single file; its name is globally unique. On first read
//! it lives in `new/`; once seen it is moved to `cur/` with a `:2,<flags>` suffix
//! encoding IMAP-style flags (`S`een, `R`eplied, `F`lagged, …). The **base name**
//! (before `:2,`) is stable across that move, so it is the message's `uid` — the
//! idempotency key the store upserts by `(mailbox_id, uid)` (§3.4), surviving the
//! `new → cur` transition without re-ingesting.

#![forbid(unsafe_code)]

pub mod gmail;
pub mod imap;
pub mod jmap;

pub use gmail::{
    reseal_gmail_plaintext, GmailProvider, GmailResealer, GmailTokenStore, GmailTokens,
    GMAIL_PLAINTEXT_KEYS, GMAIL_READONLY_SCOPE,
};

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use mailparse::{addrparse, DispositionType, MailAddr, MailHeaderMap, ParsedMail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use catalerum_core::error::{Error, Result};
use catalerum_core::model::{Connection, ConnectionKind};
use catalerum_core::provider::{EmailProvider, SyncBatch};
use catalerum_core::{
    ConnectionId, Cursor, Email, EmailAddress, EmailId, ExtractedAttachment, Mailbox, MailboxId,
    WorkspaceId,
};

/// The config key holding the [`EmailSubKind`] discriminator.
pub const PROVIDER_KEY: &str = "provider";

/// Config keys that point at a local Maildir root (canonical `root`; aliases for
/// ergonomics). Their presence also *infers* the Maildir backend when no explicit
/// `"provider"` is set.
const MAILDIR_ROOT_KEYS: &[&str] = &["root", "path", "dir", "maildir"];

/// The concrete email backend behind an email-kind [`Connection`] (SOUL §28).
/// All four are implemented: a local Maildir plus the network providers IMAP,
/// JMAP, and Gmail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmailSubKind {
    /// A local Maildir directory.
    Maildir,
    /// RFC 3501 IMAP over TLS.
    Imap,
    /// RFC 8621 JMAP over HTTP.
    Jmap,
    /// The Gmail API (OAuth2 refresh-token grant).
    Gmail,
}

impl EmailSubKind {
    /// Read the sub-kind from a connection's `config`, falling back to inference
    /// from the present keys when `"provider"` is absent.
    pub fn from_config(config: &Value) -> Result<Self> {
        if let Some(token) = config.get(PROVIDER_KEY).and_then(Value::as_str) {
            return serde_json::from_value(Value::String(token.to_string()))
                .map_err(|_| Error::invalid(format!("unknown email provider `{token}`")));
        }
        if MAILDIR_ROOT_KEYS.iter().any(|k| config.get(*k).is_some()) {
            return Ok(Self::Maildir);
        }
        Err(Error::invalid(
            "email connection config has no `provider` and no recognisable keys",
        ))
    }
}

/// True when `config` selects no email backend at all — neither an explicit
/// `"provider"` nor any key that *infers* one (the Maildir root aliases). Such a
/// connection is an unconfigured placeholder (e.g. created but never filled in)
/// with nothing to sync, so the poller skips it as a no-op rather than failing
/// [`EmailSubKind::from_config`] and logging the same error every tick. A config
/// that *does* name a backend but is broken (unknown provider, missing host, an
/// absent Maildir) is "configured but failing" and still surfaces as an error.
#[must_use]
pub fn is_unconfigured(config: &Value) -> bool {
    config.get(PROVIDER_KEY).is_none()
        && !MAILDIR_ROOT_KEYS.iter().any(|k| config.get(*k).is_some())
}

/// Build a live [`EmailProvider`] from a [`Connection`] and its `config` JSON (the
/// same JSON `catalerum-store` persists in `connections.config`). The connection
/// must be of kind [`ConnectionKind::Email`]. The backend is chosen by
/// [`EmailSubKind::from_config`]; construction only parses config (the network
/// providers connect lazily in `sync`), so an unreachable server surfaces at sync
/// time, not here. Returned boxed behind [`Arc`] for the ingest scheduler.
///
/// A **Gmail** connection with sealed OAuth credentials (a `credential_ref`) needs
/// the token seam — use [`provider_from_connection_with`]; this entry (no seam)
/// errors for such a connection. A legacy plaintext Gmail connection (no
/// `credential_ref`) still builds here.
pub fn provider_from_connection(
    connection: &Connection,
    config: &Value,
) -> Result<Arc<dyn EmailProvider>> {
    provider_from_connection_with(connection, config, None)
}

/// Like [`provider_from_connection`], but threads the [`GmailTokenStore`] seam a
/// sealed Gmail connection needs (the ingest layer builds one backed by the AES-GCM
/// secret store, keyed by the connection's `credential_ref` — the same encrypted
/// entry the Google Calendar flow seals). `gmail_tokens` is ignored for every
/// non-Gmail backend.
///
/// Gmail auth is decided by the connection's `credential_ref` (SOUL §13):
/// - **present** ⇒ sealed path. The seam is required; `None` (no secret store /
///   `[secrets].master_key` unset) is a clear error rather than a silent fall-back
///   to plaintext (the sealed blob is unreadable without the key).
/// - **absent** ⇒ legacy plaintext path (`client_id`/`client_secret`/
///   `refresh_token` from `config`), with a one-per-sync `warn` pointing at
///   `/auth/google/connect?kind=email` to re-seal. This keeps running deployments
///   syncing while nudging them onto the encrypted store.
pub fn provider_from_connection_with(
    connection: &Connection,
    config: &Value,
    gmail_tokens: Option<Arc<dyn GmailTokenStore>>,
) -> Result<Arc<dyn EmailProvider>> {
    if connection.kind != ConnectionKind::Email {
        return Err(Error::invalid(format!(
            "connection {} is not an email connection (kind = {:?})",
            connection.id, connection.kind
        )));
    }
    let ws = connection.workspace_id;
    let id = connection.id;
    match EmailSubKind::from_config(config)? {
        EmailSubKind::Maildir => Ok(Arc::new(MaildirProvider::from_config(ws, id, config)?)),
        EmailSubKind::Imap => Ok(Arc::new(imap::ImapProvider::from_config(ws, id, config)?)),
        EmailSubKind::Jmap => Ok(Arc::new(jmap::JmapProvider::from_config(ws, id, config)?)),
        EmailSubKind::Gmail => {
            if connection.credential_ref.is_some() {
                let tokens = gmail_tokens.ok_or_else(|| {
                    Error::invalid(
                        "Gmail connection has sealed OAuth credentials but no secret store is \
                         available — set [secrets].master_key to decrypt them",
                    )
                })?;
                Ok(Arc::new(gmail::GmailProvider::from_sealed(
                    ws, id, config, tokens,
                )?))
            } else {
                tracing::warn!(
                    workspace = %ws,
                    connection = %id,
                    "Gmail connection `{}` uses plaintext OAuth credentials in its config; \
                     re-connect via /auth/google/connect?kind=email to seal them (encrypted at rest)",
                    connection.name,
                );
                Ok(Arc::new(gmail::GmailProvider::from_config(ws, id, config)?))
            }
        }
    }
}

/// A local **Maildir** [`EmailProvider`] (SOUL §28). Reads one Maildir directory
/// (its `new/` + `cur/`) as a single mailbox. Holds the owning workspace +
/// connection so it can mint the canonical [`Mailbox`]/[`Email`] (mirroring
/// `catalerum-calendar`'s local provider).
#[derive(Clone, Debug)]
pub struct MaildirProvider {
    workspace_id: WorkspaceId,
    connection_id: ConnectionId,
    /// The Maildir root (contains `new/`/`cur/`/`tmp/`).
    root: PathBuf,
    /// The provider-native mailbox identifier (the Maildir path, stable).
    external_id: String,
    /// Human-readable mailbox name (defaults to `"INBOX"`).
    name: String,
}

impl MaildirProvider {
    /// Build a Maildir provider for `root` owned by `workspace_id`/`connection_id`.
    /// The `external_id` (provider-native mailbox id) defaults to the root path's
    /// string; the mailbox `name` defaults to `"INBOX"`.
    #[must_use]
    pub fn new(
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        root: impl Into<PathBuf>,
    ) -> Self {
        let root = root.into();
        let external_id = root.to_string_lossy().to_string();
        Self {
            workspace_id,
            connection_id,
            root,
            external_id,
            name: "INBOX".to_string(),
        }
    }

    /// Build a Maildir provider from a connection's `config` JSON: the Maildir
    /// root from a `root` (or `path`/`dir`/`maildir`) key, and the optional
    /// mailbox `name`. Errors if no root key is present.
    pub fn from_config(
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        config: &Value,
    ) -> Result<Self> {
        let root = MAILDIR_ROOT_KEYS
            .iter()
            .find_map(|k| config.get(*k).and_then(Value::as_str))
            .ok_or_else(|| {
                Error::invalid("maildir email config requires a `root` (or `path`/`dir`) key")
            })?;
        let mut provider = Self::new(workspace_id, connection_id, root);
        if let Some(name) = config.get("name").and_then(Value::as_str) {
            provider = provider.with_name(name);
        }
        Ok(provider)
    }

    /// Override the mailbox display name (default `"INBOX"`).
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// The single [`Mailbox`] this provider exposes (the Maildir root).
    #[must_use]
    pub fn mailbox(&self) -> Mailbox {
        Mailbox {
            id: stable_mailbox_id(self.connection_id, &self.external_id),
            workspace_id: self.workspace_id,
            connection_id: self.connection_id,
            external_id: self.external_id.clone(),
            name: self.name.clone(),
            read_only: true,
        }
    }

    /// Read + parse every message in `new/` + `cur/` into canonical [`Email`]s,
    /// plus a content cursor over the message set. Unparseable messages are
    /// logged and skipped (one bad file never fails the whole sync).
    async fn read_all(&self, mailbox: &Mailbox) -> Result<(Vec<Email>, Cursor)> {
        let mut emails = Vec::new();
        for sub in ["new", "cur"] {
            let dir = self.root.join(sub);
            let mut entries = match tokio::fs::read_dir(&dir).await {
                Ok(rd) => rd,
                // A missing new/ or cur/ is fine (an empty/partial Maildir).
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(Error::Io(e)),
            };
            let mut names: Vec<String> = Vec::new();
            while let Some(entry) = entries.next_entry().await.map_err(Error::Io)? {
                if entry.file_type().await.map_err(Error::Io)?.is_file() {
                    let n = entry.file_name().to_string_lossy().to_string();
                    // Skip Maildir dotfiles (e.g. `.` control files).
                    if !n.starts_with('.') {
                        names.push(n);
                    }
                }
            }
            names.sort(); // deterministic order
            for name in names {
                let (uid, flags) = split_maildir_name(&name);
                let path = dir.join(&name);
                let raw = match tokio::fs::read(&path).await {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "skipping unreadable maildir message");
                        continue;
                    }
                };
                match parse_email(uid, flags, &raw, mailbox.workspace_id, mailbox.id) {
                    Ok(email) => emails.push(email),
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "skipping unparseable maildir message");
                    }
                }
            }
        }
        let cursor = content_cursor(&emails);
        Ok((emails, cursor))
    }
}

#[async_trait]
impl EmailProvider for MaildirProvider {
    async fn list_mailboxes(&self) -> Result<Vec<Mailbox>> {
        Ok(vec![self.mailbox()])
    }

    async fn sync(&self, mailbox: &Mailbox, cursor: Option<Cursor>) -> Result<SyncBatch<Email>> {
        let (emails, next_cursor) = self.read_all(mailbox).await?;
        // The content cursor is a hash of the current (uid, flags) set, so an
        // unchanged Maildir yields the same cursor — the caller may skip the
        // write (idempotent, §3.4). Like the local calendar provider, we cannot
        // observe deletions across calls, so the ingest worker reconciles
        // removals by diffing the returned uid set against stored uids.
        let unchanged = cursor.as_ref() == Some(&next_cursor);
        Ok(SyncBatch {
            upserts: if unchanged { Vec::new() } else { emails },
            deletions: Vec::new(),
            next_cursor,
            has_more: false,
        })
    }
}

/// Parse a raw RFC 5322 message into a canonical [`Email`] under
/// `workspace_id`/`mailbox_id`. A fresh random [`EmailId`] is assigned; identity
/// is `(mailbox_id, uid)`, so the id is irrelevant to idempotency.
pub fn parse_email(
    uid: String,
    flags: Vec<String>,
    raw: &[u8],
    workspace_id: WorkspaceId,
    mailbox_id: MailboxId,
) -> Result<Email> {
    // Refuse a multipart bomb before parsing. `mailparse::parse_mail` parses MIME
    // multipart *recursively with no depth limit*, so an adversarially deep email
    // (anyone can send you one) overflows the stack → process abort → a poison
    // message that crash-loops the sync worker on every retry. The count of
    // `multipart/` markers is an upper bound on nesting depth (each nested
    // container declares exactly one), so this caps recursion depth before it can
    // run — a depth the recursive `collect_parts` walk below also respects.
    if count_multipart_markers(raw) > MAX_MULTIPART_PARTS {
        return Err(Error::provider(format!(
            "email rejected: multipart nesting exceeds the safe limit ({MAX_MULTIPART_PARTS})"
        )));
    }
    let mail =
        mailparse::parse_mail(raw).map_err(|e| Error::provider(format!("parse email: {e}")))?;

    let subject = mail.headers.get_first_value("Subject").unwrap_or_default();
    let message_id = mail
        .headers
        .get_first_value("Message-ID")
        .or_else(|| mail.headers.get_first_value("Message-Id"))
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    let received_at = mail
        .headers
        .get_first_value("Date")
        .and_then(|d| mailparse::dateparse(&d).ok())
        .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0));
    let from = mail
        .headers
        .get_first_value("From")
        .map(|v| parse_addr_list(&v))
        .unwrap_or_default()
        .into_iter()
        .next();
    let to = mail
        .headers
        .get_first_value("To")
        .map(|v| parse_addr_list(&v))
        .unwrap_or_default();
    let cc = mail
        .headers
        .get_first_value("Cc")
        .map(|v| parse_addr_list(&v))
        .unwrap_or_default();

    let mut body_text = None;
    let mut body_html = None;
    let mut has_attachments = false;
    collect_parts(&mail, &mut body_text, &mut body_html, &mut has_attachments);

    Ok(Email {
        id: EmailId::new(),
        workspace_id,
        mailbox_id,
        uid,
        message_id,
        from,
        to,
        cc,
        subject,
        received_at,
        body_text,
        body_html,
        has_attachments,
        flags,
        // Labels are a catalerum-side classifier verdict (a `LabelEmail` action),
        // never carried by the provider — a freshly parsed message has none.
        labels: Vec::new(),
        raw_ref: None,
        // Attachment **references** are filled by the archival seam once the parts
        // are written to object storage (SOUL §9/§28/§29) — see [`extract_attachments`];
        // a freshly parsed message carries none.
        attachments: Vec::new(),
        // Carry the raw bytes to the ingest worker so it can archive the message
        // (body + attachments) to object storage (SOUL §9/§28).
        raw: Some(raw.to_vec()),
    })
}

/// Extract the attachment MIME parts of a raw RFC 5322 message (SOUL §9/§28/§29)
/// as decoded bytes, for archival to object storage. Mirrors [`collect_parts`]'s
/// precedence exactly — the first `text/plain` and first `text/html` leaf are the
/// **body** (never returned here, even when they carry a `filename`), and every
/// other leaf that declares `Content-Disposition: attachment` *or* a
/// `filename`/`name` param is an attachment — so what this returns is precisely the
/// set of parts that make [`parse_email`] report `has_attachments = true`.
///
/// Bounded by the same multipart-bomb guard as [`parse_email`]; an unparseable
/// message or an undecodable part yields no (or fewer) attachments rather than an
/// error, since archival is best-effort and never blocks the write path.
#[must_use]
pub fn extract_attachments(raw: &[u8]) -> Vec<ExtractedAttachment> {
    if count_multipart_markers(raw) > MAX_MULTIPART_PARTS {
        return Vec::new();
    }
    let Ok(mail) = mailparse::parse_mail(raw) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut text_seen = false;
    let mut html_seen = false;
    collect_attachments(&mail, &mut text_seen, &mut html_seen, &mut out);
    out
}

/// Recursive companion to [`collect_parts`] that collects attachment parts (see
/// [`extract_attachments`]). The `text_seen`/`html_seen` flags track the body slots
/// so the readable body is never mistaken for an attachment, matching
/// [`collect_parts`] branch-for-branch.
fn collect_attachments(
    part: &ParsedMail,
    text_seen: &mut bool,
    html_seen: &mut bool,
    out: &mut Vec<ExtractedAttachment>,
) {
    if part.subparts.is_empty() {
        let disposition = part.get_content_disposition();
        let explicit_attachment = matches!(disposition.disposition, DispositionType::Attachment);
        let filename = disposition
            .params
            .get("filename")
            .or_else(|| part.ctype.params.get("name"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let named = filename.is_some();
        let mimetype = part.ctype.mimetype.as_str();
        if !explicit_attachment && mimetype == "text/plain" && !*text_seen {
            *text_seen = true;
        } else if !explicit_attachment && mimetype == "text/html" && !*html_seen {
            *html_seen = true;
        } else if explicit_attachment || named {
            if let Ok(data) = part.get_body_raw() {
                if !data.is_empty() {
                    out.push(ExtractedAttachment {
                        filename,
                        content_type: Some(part.ctype.mimetype.clone()),
                        data,
                    });
                }
            }
        }
    } else {
        for sub in &part.subparts {
            collect_attachments(sub, text_seen, html_seen, out);
        }
    }
}

/// Parse an address-list header value (`From`/`To`/`Cc`) into [`EmailAddress`]es,
/// flattening RFC 5322 groups. A malformed value yields an empty list rather than
/// failing the whole message.
fn parse_addr_list(value: &str) -> Vec<EmailAddress> {
    match addrparse(value) {
        Ok(list) => list.iter().flat_map(flatten_addr).collect(),
        Err(_) => Vec::new(),
    }
}

fn flatten_addr(addr: &MailAddr) -> Vec<EmailAddress> {
    match addr {
        MailAddr::Single(info) => vec![EmailAddress {
            name: info.display_name.clone(),
            address: info.addr.clone(),
        }],
        MailAddr::Group(group) => group
            .addrs
            .iter()
            .map(|info| EmailAddress {
                name: info.display_name.clone(),
                address: info.addr.clone(),
            })
            .collect(),
    }
}

/// Safe cap on MIME multipart nesting (see [`count_multipart_markers`]). A
/// recursion this deep parses comfortably within the worker stack, while real
/// mail is nowhere near it (a few levels, plus a handful per forwarded message);
/// past it we refuse rather than risk a stack-overflow abort.
const MAX_MULTIPART_PARTS: usize = 256;

/// Count `multipart/` content-type markers in the raw message (ASCII
/// case-insensitive, non-overlapping). Each nested multipart container declares
/// exactly one, so this is an **upper bound** on the MIME nesting depth — used to
/// refuse a multipart bomb before `mailparse::parse_mail` (which recurses per
/// level with no limit) can overflow the stack. Over-counts harmlessly when the
/// literal string appears in a body; it never under-counts a real container.
fn count_multipart_markers(raw: &[u8]) -> usize {
    const NEEDLE: &[u8] = b"multipart/";
    let mut count = 0;
    let mut i = 0;
    while i + NEEDLE.len() <= raw.len() {
        if raw[i..i + NEEDLE.len()].eq_ignore_ascii_case(NEEDLE) {
            count += 1;
            i += NEEDLE.len();
        } else {
            i += 1;
        }
    }
    count
}

/// Walk the MIME tree collecting the first `text/plain` body, the first
/// `text/html` body, and whether any part is an attachment. A single-part text
/// message has no subparts; multipart messages recurse.
fn collect_parts(
    part: &ParsedMail,
    text: &mut Option<String>,
    html: &mut Option<String>,
    has_attachments: &mut bool,
) {
    if part.subparts.is_empty() {
        let disposition = part.get_content_disposition();
        // Only an *explicit* `Content-Disposition: attachment` makes a text part
        // not-the-body. A `filename`/`name` param on an INLINE part is RFC 2183-
        // legal for the readable body (forwards, gateway-rewritten mail), so it
        // must not suppress body capture — the name heuristic flags attachments
        // only for non-body mimetypes (e.g. an inline image in multipart/related).
        let explicit_attachment = matches!(disposition.disposition, DispositionType::Attachment);
        let named =
            disposition.params.contains_key("filename") || part.ctype.params.contains_key("name");
        let mimetype = part.ctype.mimetype.as_str();
        if !explicit_attachment && mimetype == "text/plain" && text.is_none() {
            *text = part.get_body().ok().filter(|b| !b.is_empty());
        } else if !explicit_attachment && mimetype == "text/html" && html.is_none() {
            *html = part.get_body().ok().filter(|b| !b.is_empty());
        } else if explicit_attachment || named {
            *has_attachments = true;
        }
    } else {
        for sub in &part.subparts {
            collect_parts(sub, text, html, has_attachments);
        }
    }
}

/// Split a Maildir filename into its stable base `uid` and decoded flags. A
/// `new/` name has no `:2,` suffix (no flags yet); a `cur/` name is
/// `<base>:2,<flags>`.
fn split_maildir_name(name: &str) -> (String, Vec<String>) {
    match name.split_once(":2,") {
        Some((base, flag_chars)) => {
            let flags = flag_chars.chars().filter_map(map_flag).collect();
            (base.to_string(), flags)
        }
        None => (name.to_string(), Vec::new()),
    }
}

/// Map a Maildir flag char to a provider-native flag token.
fn map_flag(c: char) -> Option<String> {
    Some(
        match c {
            'S' => "seen",
            'R' => "answered",
            'F' => "flagged",
            'D' => "draft",
            'T' => "trashed",
            'P' => "passed",
            _ => return None,
        }
        .to_string(),
    )
}

/// A content cursor over the message set: a hash of the sorted `uid:flags` keys,
/// so an unchanged Maildir yields the same cursor (idempotent skip), and an
/// add/remove/flag-change moves it.
fn content_cursor(emails: &[Email]) -> Cursor {
    let mut keys: Vec<String> = emails
        .iter()
        .map(|e| format!("{}:{}", e.uid, e.flags.join("")))
        .collect();
    keys.sort();
    let mut hasher = Sha256::new();
    for k in &keys {
        hasher.update(k.as_bytes());
        hasher.update(b"\n");
    }
    Cursor::new(format!("{:x}", hasher.finalize()))
}

/// Cap on bytes read from a provider's HTTP response body. `reqwest::Response::json()`
/// (and `.text()`) buffer the **entire** body before decoding with no size limit, so
/// a compromised/buggy upstream (a self-hosted/3rd-party JMAP server, the Gmail API)
/// could OOM the sync worker. Generous for a batch of email bodies; exceeding it
/// errors (a truncated JSON wouldn't decode anyway).
pub(crate) const MAX_HTTP_RESPONSE_BYTES: usize = 64 * 1024 * 1024; // 64 MiB

/// Read and JSON-decode a provider HTTP response, capped at [`MAX_HTTP_RESPONSE_BYTES`]
/// — the bounded replacement for `resp.json()`, shared by the JMAP and Gmail backends
/// (streams via `reqwest::Response::chunk()`, so no `futures` dependency is needed).
pub(crate) async fn read_json_capped(mut resp: reqwest::Response, what: &str) -> Result<Value> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| Error::provider(format!("read {what}: {e}")))?
    {
        if buf.len() + chunk.len() > MAX_HTTP_RESPONSE_BYTES {
            return Err(Error::provider(format!(
                "{what} exceeds the {MAX_HTTP_RESPONSE_BYTES}-byte cap; refusing to buffer it"
            )));
        }
        buf.extend_from_slice(chunk.as_ref());
    }
    serde_json::from_slice(&buf).map_err(|e| Error::provider(format!("{what} decode: {e}")))
}

/// A stable [`MailboxId`] over `(connection_id, external_id)` (UUID v5), so the
/// provider's `Mailbox` matches what the store would upsert by
/// `(connection_id, external_id)` across runs — no DB round-trip needed.
pub(crate) fn stable_mailbox_id(connection_id: ConnectionId, external_id: &str) -> MailboxId {
    let seed = format!("{connection_id}/{external_id}");
    MailboxId::from_uuid(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        seed.as_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write(dir: &Path, sub: &str, name: &str, content: &str) {
        let d = dir.join(sub);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(name), content).unwrap();
    }

    const SIMPLE: &str = "From: Ada Lovelace <ada@example.com>\r\n\
To: Charles Babbage <charles@example.com>, friend@example.org\r\n\
Cc: cc@example.com\r\n\
Subject: Analytical Engine\r\n\
Date: Mon, 15 Jun 2026 09:30:00 +0000\r\n\
Message-ID: <abc123@example.com>\r\n\
\r\n\
The engine weaves algebraic patterns.\r\n";

    const MULTIPART: &str = "From: bob@example.com\r\n\
To: alice@example.com\r\n\
Subject: With attachment\r\n\
Date: Tue, 16 Jun 2026 10:00:00 +0000\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"BB\"\r\n\
\r\n\
--BB\r\n\
Content-Type: text/plain\r\n\
\r\n\
See attached.\r\n\
--BB\r\n\
Content-Type: application/octet-stream; name=\"data.bin\"\r\n\
Content-Disposition: attachment; filename=\"data.bin\"\r\n\
\r\n\
AQIDBA==\r\n\
--BB--\r\n";

    /// Build a `depth`-level nested `multipart/mixed` message with a `text/plain`
    /// core, in O(depth) (distinct boundaries per level so parsing is unambiguous).
    fn nested_multipart(depth: usize) -> Vec<u8> {
        let mut s = String::new();
        for k in 0..depth {
            s.push_str(&format!(
                "Content-Type: multipart/mixed; boundary=\"b{k}\"\r\n\r\n--b{k}\r\n"
            ));
        }
        s.push_str("Content-Type: text/plain\r\n\r\ndeep body\r\n");
        for k in (0..depth).rev() {
            s.push_str(&format!("--b{k}--\r\n"));
        }
        s.into_bytes()
    }

    #[test]
    fn rejects_deeply_nested_multipart_bomb() {
        // Past the cap the message is refused *before* mailparse recurses — without
        // this, parsing recurses per level and overflows the stack (process abort,
        // a poison message that crash-loops sync). Returning an Err here (not
        // aborting) is the assertion.
        let raw = nested_multipart(MAX_MULTIPART_PARTS + 1000);
        let err = parse_email(
            "u".into(),
            vec![],
            &raw,
            WorkspaceId::new(),
            MailboxId::new(),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("multipart nesting"), "got {err}");
        // A shallow, normal multipart is unaffected — it parses and finds the body.
        let ok = parse_email(
            "u".into(),
            vec![],
            &nested_multipart(3),
            WorkspaceId::new(),
            MailboxId::new(),
        )
        .unwrap();
        assert_eq!(ok.body_text.as_deref(), Some("deep body"));
    }

    #[test]
    fn is_unconfigured_flags_only_empty_configs() {
        // Empty / backend-less configs are unconfigured placeholders the poller skips.
        assert!(is_unconfigured(&serde_json::json!({})));
        assert!(is_unconfigured(&serde_json::json!({ "label": "INBOX" })));
        // Anything that names or infers a backend is "configured" (even if broken):
        // an explicit provider, or a Maildir-root alias whose dir may not exist.
        assert!(!is_unconfigured(&serde_json::json!({ "provider": "imap" })));
        assert!(!is_unconfigured(
            &serde_json::json!({ "provider": "nonsense" })
        ));
        assert!(!is_unconfigured(&serde_json::json!({ "root": "/mail/b" })));
        // The predicate is exactly "from_config would fail for lack of any selector".
        assert!(EmailSubKind::from_config(&serde_json::json!({})).is_err());
        assert!(EmailSubKind::from_config(&serde_json::json!({ "root": "/mail/b" })).is_ok());
    }

    #[test]
    fn count_multipart_markers_bounds_nesting() {
        assert_eq!(
            count_multipart_markers(b"multipart/mixed then MULTIPART/alternative"),
            2
        );
        assert_eq!(count_multipart_markers(b"text/plain only"), 0);
        assert_eq!(count_multipart_markers(b"Multipart/"), 1);
        // Each level of a nested message contributes exactly one marker.
        assert_eq!(count_multipart_markers(&nested_multipart(5)), 5);
    }

    #[test]
    fn extract_attachments_returns_parts_not_the_body() {
        // The multipart fixture has one text/plain body + one attachment (data.bin).
        // Extraction returns exactly the attachment — never the readable body — so it
        // agrees with `has_attachments` (SOUL §9/§28/§29).
        let atts = extract_attachments(MULTIPART.as_bytes());
        assert_eq!(atts.len(), 1, "one attachment, the body is not returned");
        assert_eq!(atts[0].filename.as_deref(), Some("data.bin"));
        assert_eq!(
            atts[0].content_type.as_deref(),
            Some("application/octet-stream")
        );
        assert!(!atts[0].data.is_empty());

        // A plain single-part message has no attachments.
        assert!(extract_attachments(SIMPLE.as_bytes()).is_empty());

        // A multipart bomb is refused (empty) before mailparse can recurse, and
        // garbage never panics.
        assert!(extract_attachments(&nested_multipart(MAX_MULTIPART_PARTS + 1000)).is_empty());
        let _ = extract_attachments(b"not a mime message at all");
    }

    #[test]
    fn parses_a_simple_message() {
        let ws = WorkspaceId::new();
        let mb = MailboxId::new();
        let email = parse_email(
            "uid-1".into(),
            vec!["seen".into()],
            SIMPLE.as_bytes(),
            ws,
            mb,
        )
        .unwrap();
        assert_eq!(email.uid, "uid-1");
        assert_eq!(email.subject, "Analytical Engine");
        assert_eq!(email.message_id.as_deref(), Some("<abc123@example.com>"));
        assert_eq!(email.from.as_ref().unwrap().address, "ada@example.com");
        assert_eq!(
            email.from.as_ref().unwrap().name.as_deref(),
            Some("Ada Lovelace")
        );
        // To has two addresses (one named, one bare).
        assert_eq!(email.to.len(), 2);
        assert_eq!(email.to[0].address, "charles@example.com");
        assert_eq!(email.to[1].address, "friend@example.org");
        assert_eq!(email.cc.len(), 1);
        assert_eq!(email.cc[0].address, "cc@example.com");
        assert!(email
            .body_text
            .as_deref()
            .unwrap()
            .contains("algebraic patterns"));
        assert!(email.body_html.is_none());
        assert!(!email.has_attachments);
        assert_eq!(email.flags, vec!["seen".to_string()]);
        assert_eq!(
            email.received_at.unwrap().to_rfc3339(),
            "2026-06-15T09:30:00+00:00"
        );
        assert_eq!(email.workspace_id, ws);
        assert_eq!(email.mailbox_id, mb);
    }

    #[test]
    fn detects_attachments_in_multipart() {
        let email = parse_email(
            "uid-2".into(),
            vec![],
            MULTIPART.as_bytes(),
            WorkspaceId::new(),
            MailboxId::new(),
        )
        .unwrap();
        assert_eq!(email.subject, "With attachment");
        assert!(
            email.has_attachments,
            "the application/octet-stream part is an attachment"
        );
        assert!(email.body_text.as_deref().unwrap().contains("See attached"));
    }

    // Message-ID is the cross-folder dedup key (SOUL §29): the same RFC 5322 message
    // seen in two folders shares this header but lands as two `(mailbox_id, uid)`
    // rows. The parse must be total over the awkward cases — absent and blank both
    // normalize to `None` so the dedup index never keys on an empty string — while a
    // present id is carried verbatim (only trimmed; an opaque handle otherwise).
    #[test]
    fn message_id_absent_or_blank_parses_as_none() {
        // No Message-ID header at all ⇒ None.
        const NO_MSGID: &str = "From: a@example.com\r\n\
To: b@example.com\r\n\
Subject: No id\r\n\
\r\n\
body\r\n";
        let e = parse_email(
            "u".into(),
            vec![],
            NO_MSGID.as_bytes(),
            WorkspaceId::new(),
            MailboxId::new(),
        )
        .unwrap();
        assert!(
            e.message_id.is_none(),
            "an absent Message-ID parses as None"
        );

        // A blank / whitespace-only value ⇒ None (never `Some("")`), so a row with a
        // degenerate header is not grouped with other blank-id rows.
        const BLANK_MSGID: &str = "From: a@example.com\r\n\
Subject: Blank id\r\n\
Message-ID:    \r\n\
\r\n\
body\r\n";
        let e = parse_email(
            "u".into(),
            vec![],
            BLANK_MSGID.as_bytes(),
            WorkspaceId::new(),
            MailboxId::new(),
        )
        .unwrap();
        assert!(
            e.message_id.is_none(),
            "a blank Message-ID normalizes to None"
        );

        // A present id is trimmed of surrounding whitespace but otherwise verbatim.
        const PADDED_MSGID: &str = "From: a@example.com\r\n\
Subject: Padded id\r\n\
Message-ID:   <keep@example.com>  \r\n\
\r\n\
body\r\n";
        let e = parse_email(
            "u".into(),
            vec![],
            PADDED_MSGID.as_bytes(),
            WorkspaceId::new(),
            MailboxId::new(),
        )
        .unwrap();
        assert_eq!(e.message_id.as_deref(), Some("<keep@example.com>"));
    }

    // A readable body on an INLINE part that carries a filename/name param (RFC
    // 2183-legal; emitted by some forwards / gateways). It must be captured as the
    // body, NOT misclassified as an attachment.
    const INLINE_NAMED_BODY: &str = "From: bob@example.com\r\n\
To: alice@example.com\r\n\
Subject: Inline named body\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/plain; name=\"message.txt\"\r\n\
Content-Disposition: inline; filename=\"message.txt\"\r\n\
\r\n\
This is the actual body text.\r\n";

    #[test]
    fn inline_named_text_part_is_body_not_attachment() {
        let email = parse_email(
            "uid-inline".into(),
            vec![],
            INLINE_NAMED_BODY.as_bytes(),
            WorkspaceId::new(),
            MailboxId::new(),
        )
        .unwrap();
        assert!(
            email
                .body_text
                .as_deref()
                .unwrap_or_default()
                .contains("This is the actual body text"),
            "an inline text part with a filename must still be captured as the body"
        );
        assert!(
            !email.has_attachments,
            "an inline-named readable body is not an attachment"
        );
    }

    #[test]
    fn email_sub_kind_from_config_explicit_and_inferred() {
        use serde_json::json;
        assert_eq!(
            EmailSubKind::from_config(&json!({"provider": "maildir"})).unwrap(),
            EmailSubKind::Maildir
        );
        // Inference from a root key.
        assert_eq!(
            EmailSubKind::from_config(&json!({"root": "/var/mail"})).unwrap(),
            EmailSubKind::Maildir
        );
        assert_eq!(
            EmailSubKind::from_config(&json!({"provider": "imap"})).unwrap(),
            EmailSubKind::Imap
        );
        assert!(EmailSubKind::from_config(&json!({"provider": "bogus"})).is_err());
        assert!(EmailSubKind::from_config(&json!({"unknown": 1})).is_err());
    }

    fn email_conn(credential_ref: Option<&str>) -> Connection {
        Connection {
            id: ConnectionId::new(),
            workspace_id: WorkspaceId::new(),
            kind: ConnectionKind::Email,
            name: "Gmail".into(),
            credential_ref: credential_ref.map(str::to_string),
            cursor: None,
        }
    }

    struct FakeGmailStore;
    #[async_trait]
    impl GmailTokenStore for FakeGmailStore {
        async fn load(&self) -> Result<GmailTokens> {
            Ok(GmailTokens {
                client_id: "cid".into(),
                client_secret: "sec".into(),
                refresh_token: "rt".into(),
                ..GmailTokens::default()
            })
        }
        async fn store(&self, _tokens: &GmailTokens) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn gmail_factory_sealed_when_credential_ref_present() {
        use serde_json::json;
        // credential_ref present + seam present ⇒ sealed provider builds, and it
        // does NOT need plaintext credentials in config.
        let c = email_conn(Some("cred-1"));
        let seam: Arc<dyn GmailTokenStore> = Arc::new(FakeGmailStore);
        let p = provider_from_connection_with(
            &c,
            &json!({ "provider": "gmail", "label": "INBOX" }),
            Some(seam),
        );
        assert!(p.is_ok(), "sealed gmail builds with the token seam present");

        // credential_ref present but NO seam ⇒ a clear error (no plaintext fallback).
        let err = provider_from_connection_with(
            &c,
            &json!({ "provider": "gmail", "label": "INBOX" }),
            None,
        );
        assert!(matches!(err, Err(Error::Invalid(_))));
    }

    #[test]
    fn gmail_factory_legacy_plaintext_when_no_credential_ref() {
        use serde_json::json;
        // No credential_ref ⇒ legacy plaintext path (config triplet), no seam needed.
        let c = email_conn(None);
        let p = provider_from_connection(
            &c,
            &json!({ "provider": "gmail", "client_id": "x", "client_secret": "y", "refresh_token": "z" }),
        );
        assert!(
            p.is_ok(),
            "legacy plaintext gmail still builds (back-compat)"
        );

        // Plaintext path missing the triplet still errors as before.
        let err = provider_from_connection(&c, &json!({ "provider": "gmail" }));
        assert!(matches!(err, Err(Error::Invalid(_))));
    }

    #[test]
    fn maildir_from_config_reads_root_and_name() {
        use serde_json::json;
        let p = MaildirProvider::from_config(
            WorkspaceId::new(),
            ConnectionId::new(),
            &json!({"provider": "maildir", "root": "/var/mail/me", "name": "Archive"}),
        )
        .unwrap();
        assert_eq!(p.name, "Archive");
        assert_eq!(p.external_id, "/var/mail/me");
        // Missing root → error.
        assert!(MaildirProvider::from_config(
            WorkspaceId::new(),
            ConnectionId::new(),
            &json!({"provider": "maildir"})
        )
        .is_err());
    }

    #[test]
    fn splits_maildir_names_and_flags() {
        assert_eq!(
            split_maildir_name("1700000000.abcd:2,FS"),
            (
                "1700000000.abcd".to_string(),
                vec!["flagged".to_string(), "seen".to_string()]
            )
        );
        // A `new/` name (no flags) keeps the whole name as the uid.
        assert_eq!(
            split_maildir_name("1700000000.efgh"),
            ("1700000000.efgh".to_string(), Vec::new())
        );
    }

    #[tokio::test]
    async fn maildir_sync_reads_new_and_cur_and_is_cursor_idempotent() {
        let ws = WorkspaceId::new();
        let conn = ConnectionId::new();
        let dir = tempfile::tempdir().unwrap();
        // A message in new/ (unseen) and one in cur/ (seen, with flags).
        write(dir.path(), "new", "msg-new", SIMPLE);
        write(dir.path(), "cur", "msg-cur:2,S", MULTIPART);

        let provider = MaildirProvider::new(ws, conn, dir.path()).with_name("INBOX");
        let mailboxes = provider.list_mailboxes().await.unwrap();
        assert_eq!(mailboxes.len(), 1);
        let mailbox = &mailboxes[0];
        assert_eq!(mailbox.name, "INBOX");
        assert!(mailbox.read_only);

        let batch = provider.sync(mailbox, None).await.unwrap();
        assert_eq!(batch.upserts.len(), 2, "both new/ and cur/ messages");
        // The cur/ message's uid is the base name (flags stripped); it is seen.
        let cur = batch.upserts.iter().find(|e| e.uid == "msg-cur").unwrap();
        assert_eq!(cur.flags, vec!["seen".to_string()]);
        assert!(cur.has_attachments);
        let new = batch.upserts.iter().find(|e| e.uid == "msg-new").unwrap();
        assert!(new.flags.is_empty());

        // Re-syncing from the returned cursor yields no upserts (unchanged set).
        let again = provider
            .sync(mailbox, Some(batch.next_cursor.clone()))
            .await
            .unwrap();
        assert!(
            again.upserts.is_empty(),
            "unchanged Maildir → idempotent skip"
        );
        assert_eq!(again.next_cursor, batch.next_cursor);

        // Adding a message moves the cursor and re-surfaces the set.
        write(dir.path(), "new", "msg-3", SIMPLE);
        let grown = provider
            .sync(mailbox, Some(batch.next_cursor.clone()))
            .await
            .unwrap();
        assert_ne!(grown.next_cursor, batch.next_cursor);
        assert_eq!(grown.upserts.len(), 3);
    }
}
