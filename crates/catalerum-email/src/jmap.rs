//! RFC 8621 JMAP email provider over HTTP (SOUL §28).
//!
//! A read-only ingest backend ([`EmailProvider`]) for a JMAP account (Fastmail,
//! Stalwart, …). catalerum **reads** mail; it never sends or mutates server
//! state (§14), so sync only ever issues `Mailbox/get`, `Email/query`,
//! `Email/get`, and a raw-blob `GET` against the session `downloadUrl` — all
//! reads.
//!
//! ## Raw-message archival (SOUL §28/§29)
//! `Email/get` returns structured fields, not the RFC 5322 bytes, so to feed the
//! archival seam (which MIME-extracts attachments and offloads the `.eml`) each
//! emitted message's `blobId` is downloaded via the session `downloadUrl` URI
//! template (RFC 8620 §6.2) and stashed in [`Email::raw`]. The download is
//! **bounded** (the shared [`crate::MAX_HTTP_RESPONSE_BYTES`] cap) and
//! **fail-soft**: any download failure logs a warning and leaves `raw: None`, so
//! the message still lands (archival simply no-ops for it) and the sync succeeds.
//! Only messages actually being (re-)emitted are downloaded — never re-listed
//! known ids — so the cost mirrors IMAP's per-changed-message full-body fetch.
//!
//! ## Incrementality (SOUL §3.4)
//! JMAP `Email/changes` is account-wide, but catalerum syncs per **mailbox**, so
//! — exactly like the IMAP provider — each sync takes a cheap snapshot of the
//! mailbox (`Email/query` for the ids, `Email/get` for just `id`+`keywords`),
//! encodes `{emailId → keyword-signature}` in the [`Cursor`], and emits a delta:
//! a new or keyword-changed message is (re-)fetched in full and **upserted**; an
//! id present in the cursor but gone now is named in `deletions`. JMAP email ids
//! are account-stable (no `UIDVALIDITY` equivalent), so the cursor needs no
//! generation marker.
//!
//! Being a true delta ([`is_incremental`](EmailProvider::is_incremental) is
//! `true`), the ingest worker treats this provider as authoritative for deletions
//! and never diff-reconciles.
//!
//! Auth is a bearer `token` from the connection `config` (plaintext M-stage stub,
//! as CalDAV; the encrypted vault behind `credential_ref` lands later, SOUL §13).

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use catalerum_core::error::{Error, Result};
use catalerum_core::model::{Cursor, Email, EmailAddress, Mailbox};
use catalerum_core::provider::{EmailProvider, SyncBatch};
use catalerum_core::{ConnectionId, EmailId, WorkspaceId};

use crate::stable_mailbox_id;

/// JMAP capability URNs every request advertises (core + mail).
const USING: [&str; 2] = ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"];

/// How many email ids to request per `Email/get` batch (servers cap large gets).
const BATCH: usize = 200;

/// Safety cap on a single mailbox snapshot, so one enormous folder can't fetch
/// unbounded ids in a tick (the rest is picked up on later runs).
const SNAPSHOT_CAP: usize = 10_000;

/// Properties requested by the full `Email/get` (the fetch that becomes an
/// upsert). `blobId` is the handle used to download the raw RFC 5322 bytes for
/// archival (RFC 8620); the rest map to the canonical [`Email`] fields.
const EMAIL_PROPERTIES: &[&str] = &[
    "id",
    "blobId",
    "messageId",
    "from",
    "to",
    "cc",
    "subject",
    "receivedAt",
    "keywords",
    "hasAttachment",
    "textBody",
    "htmlBody",
    "bodyValues",
];

/// A read-only JMAP [`EmailProvider`] (SOUL §28).
#[derive(Clone, Debug)]
pub struct JmapProvider {
    workspace_id: WorkspaceId,
    connection_id: ConnectionId,
    /// The JMAP session resource URL (e.g. `https://api.fastmail.com/jmap/session`).
    session_url: String,
    /// Bearer token.
    token: String,
    /// Optional account-id override (else the primary mail account is used).
    account_id: Option<String>,
    http: reqwest::Client,
}

impl JmapProvider {
    /// Build from a connection's `config` JSON. Required: `session_url` (or
    /// `base_url`), `token`. Optional: `account_id`.
    pub fn from_config(
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        config: &Value,
    ) -> Result<Self> {
        let session_url = opt_str(config, "session_url")
            .or_else(|| opt_str(config, "base_url"))
            .or_else(|| opt_str(config, "url"))
            .ok_or_else(|| Error::invalid("jmap email config requires a `session_url`"))?;
        let token = opt_str(config, "token")
            .ok_or_else(|| Error::invalid("jmap email config requires a bearer `token`"))?;
        let account_id = opt_str(config, "account_id");
        let http = reqwest::Client::builder()
            .user_agent("catalerum-email")
            // Fail fast on an unreachable JMAP server instead of hanging the sync
            // worker. No overall timeout: an Email/get blob can be large and
            // shouldn't be aborted mid-transfer.
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| Error::provider(format!("build http client: {e}")))?;
        Ok(Self {
            workspace_id,
            connection_id,
            session_url,
            token,
            account_id,
            http,
        })
    }

    /// Fetch the session resource → the [`Session`] handles sync needs.
    async fn session(&self) -> Result<Session> {
        let resp = self
            .http
            .get(&self.session_url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| Error::provider(format!("JMAP session GET: {e}")))?;
        let resp = ensure_success(resp, "session")?;
        let v: Value = crate::read_json_capped(resp, "JMAP session").await?;
        let api_url = v
            .get("apiUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::provider("JMAP session has no apiUrl"))?
            .to_string();
        let account_id = self
            .account_id
            .clone()
            .or_else(|| resolve_account_id(&v))
            .ok_or_else(|| Error::provider("JMAP session has no mail account"))?;
        // RFC 8620 §6.2: the downloadUrl URI template. REQUIRED by the spec, but
        // treated as optional here — a server that omits it just means raw-blob
        // archival is unavailable (messages land with `raw: None`), never a sync
        // failure.
        let download_url = v
            .get("downloadUrl")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(Session {
            api_url,
            account_id,
            download_url,
        })
    }

    /// POST a batch of JMAP method calls to `api_url`, returning the parsed body.
    async fn request(&self, api_url: &str, method_calls: Value) -> Result<Value> {
        let body = json!({ "using": USING, "methodCalls": method_calls });
        let resp = self
            .http
            .post(api_url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::provider(format!("JMAP request: {e}")))?;
        let resp = ensure_success(resp, "request")?;
        crate::read_json_capped(resp, "JMAP request").await
    }

    /// All email ids in `mailbox_id`, newest first, paged to [`SNAPSHOT_CAP`].
    /// The bool is **truncated**: `true` when the cap was hit, i.e. the mailbox may
    /// hold more than the snapshot saw — which makes prior-but-absent ids ambiguous
    /// (deleted vs. fell-past-the-cap), so the caller must not treat them as
    /// deletions (see [`snapshot_deletions`]).
    async fn query_ids(
        &self,
        api_url: &str,
        account_id: &str,
        mailbox_id: &str,
    ) -> Result<(Vec<String>, bool)> {
        let mut ids = Vec::new();
        let mut truncated = false;
        let mut position = 0i64;
        loop {
            let resp = self
                .request(
                    api_url,
                    json!([[
                        "Email/query",
                        {
                            "accountId": account_id,
                            "filter": { "inMailbox": mailbox_id },
                            "sort": [{ "property": "receivedAt", "isAscending": false }],
                            "position": position,
                            "limit": BATCH,
                            "calculateTotal": false
                        },
                        "0"
                    ]]),
                )
                .await?;
            let args = method_result(&resp, "Email/query")?;
            let page: Vec<String> = args
                .get("ids")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let got = page.len();
            ids.extend(page);
            if got < BATCH || ids.len() >= SNAPSHOT_CAP {
                if ids.len() >= SNAPSHOT_CAP {
                    truncated = true;
                    tracing::warn!(
                        cap = SNAPSHOT_CAP,
                        mailbox = mailbox_id,
                        "JMAP snapshot hit cap"
                    );
                }
                break;
            }
            position += BATCH as i64;
        }
        Ok((ids, truncated))
    }

    /// `id → keyword-signature` for `ids` (the cheap `Email/get` of just keywords).
    async fn keyword_sigs(
        &self,
        api_url: &str,
        account_id: &str,
        ids: &[String],
    ) -> Result<BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        for chunk in ids.chunks(BATCH) {
            let resp = self
                .request(
                    api_url,
                    json!([[
                        "Email/get",
                        { "accountId": account_id, "ids": chunk, "properties": ["id", "keywords"] },
                        "0"
                    ]]),
                )
                .await?;
            let args = method_result(&resp, "Email/get")?;
            if let Some(list) = args.get("list").and_then(Value::as_array) {
                for e in list {
                    if let Some(id) = e.get("id").and_then(Value::as_str) {
                        out.insert(id.to_string(), flag_sig(&keyword_tokens(e)));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Full `Email/get` for `ids`, mapped to canonical [`Email`]s. When the
    /// session advertised a `download_url`, each message's raw RFC 5322 bytes are
    /// also downloaded (via its `blobId`) and stashed in [`Email::raw`] for the
    /// archival seam — bounded and fail-soft (see [`Self::download_raw`]).
    async fn fetch_emails(
        &self,
        api_url: &str,
        account_id: &str,
        download_url: Option<&str>,
        ids: &[String],
        mailbox: &Mailbox,
    ) -> Result<Vec<Email>> {
        let mut out = Vec::new();
        for chunk in ids.chunks(BATCH) {
            let resp = self
                .request(
                    api_url,
                    json!([[
                        "Email/get",
                        {
                            "accountId": account_id,
                            "ids": chunk,
                            "properties": EMAIL_PROPERTIES,
                            "fetchTextBodyValues": true,
                            "fetchHTMLBodyValues": true
                        },
                        "0"
                    ]]),
                )
                .await?;
            let args = method_result(&resp, "Email/get")?;
            if let Some(list) = args.get("list").and_then(Value::as_array) {
                for e in list {
                    let mut email = jmap_to_email(e, mailbox);
                    if let (Some(dl), Some(blob)) = (download_url, blob_id(e)) {
                        email.raw = self.download_raw(dl, account_id, blob, &email.uid).await;
                    }
                    out.push(email);
                }
            }
        }
        Ok(out)
    }

    /// Download a message's raw RFC 5322 bytes by expanding the session
    /// `download_url` template (RFC 8620 §6.2) for `blob_id` and `GET`ting it with
    /// the same bearer auth as the API. **Bounded** to
    /// [`crate::MAX_HTTP_RESPONSE_BYTES`] and **fail-soft**: on any failure
    /// (network, non-2xx incl. `404`, or the size cap) it logs a warning and
    /// returns `None`, so the message still upserts with `raw: None` (archival
    /// no-ops) and the sync as a whole succeeds.
    async fn download_raw(
        &self,
        download_url: &str,
        account_id: &str,
        blob_id: &str,
        uid: &str,
    ) -> Option<Vec<u8>> {
        let url = expand_download_url(download_url, account_id, blob_id);
        let resp = match self.http.get(&url).bearer_auth(&self.token).send().await {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!(uid, blob = blob_id, error = %e, "JMAP blob download failed; archiving without raw");
                return None;
            }
        };
        if !resp.status().is_success() {
            tracing::warn!(
                uid,
                blob = blob_id,
                status = %resp.status(),
                "JMAP blob download non-success; archiving without raw"
            );
            return None;
        }
        match read_bytes_capped(resp).await {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                tracing::warn!(uid, blob = blob_id, error = %e, "JMAP blob read failed; archiving without raw");
                None
            }
        }
    }
}

#[async_trait]
impl EmailProvider for JmapProvider {
    async fn list_mailboxes(&self) -> Result<Vec<Mailbox>> {
        let Session {
            api_url,
            account_id,
            ..
        } = self.session().await?;
        let resp = self
            .request(
                &api_url,
                json!([[
                    "Mailbox/get",
                    { "accountId": account_id, "ids": null, "properties": ["id", "name", "role"] },
                    "0"
                ]]),
            )
            .await?;
        let args = method_result(&resp, "Mailbox/get")?;
        let list = args
            .get("list")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mailboxes = list
            .iter()
            .filter_map(|m| {
                let external_id = m.get("id").and_then(Value::as_str)?.to_string();
                let name = m
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&external_id)
                    .to_string();
                Some(Mailbox {
                    id: stable_mailbox_id(self.connection_id, &external_id),
                    workspace_id: self.workspace_id,
                    connection_id: self.connection_id,
                    external_id,
                    name,
                    read_only: true,
                })
            })
            .collect();
        Ok(mailboxes)
    }

    async fn sync(&self, mailbox: &Mailbox, cursor: Option<Cursor>) -> Result<SyncBatch<Email>> {
        let Session {
            api_url,
            account_id,
            download_url,
        } = self.session().await?;
        let (ids, truncated) = self
            .query_ids(&api_url, &account_id, &mailbox.external_id)
            .await?;
        let sigs = self.keyword_sigs(&api_url, &account_id, &ids).await?;

        let prior = JmapCursor::decode(cursor.as_ref()).unwrap_or_default();
        let to_fetch: Vec<String> = sigs
            .iter()
            .filter(|(id, sig)| prior.k.get(*id).map(|s| s != *sig).unwrap_or(true))
            .map(|(id, _)| id.clone())
            .collect();
        let deletions = snapshot_deletions(&prior, &sigs, truncated);

        let upserts = self
            .fetch_emails(
                &api_url,
                &account_id,
                download_url.as_deref(),
                &to_fetch,
                mailbox,
            )
            .await?;

        Ok(SyncBatch {
            upserts,
            deletions,
            next_cursor: JmapCursor { k: sigs }.encode(),
            has_more: false,
        })
    }

    fn is_incremental(&self) -> bool {
        true
    }
}

/// Ids in `prior` absent from the current snapshot `sigs` are deletions — but
/// **only when the snapshot is complete**. A truncated snapshot (a mailbox larger
/// than [`SNAPSHOT_CAP`]) can't tell a genuinely-deleted email from one that simply
/// fell past the newest-`SNAPSHOT_CAP` window as new mail arrived, so it returns no
/// deletions rather than false-deleting a still-existing email from the catalogue
/// (the ingest worker treats this provider as authoritative for deletions). A
/// genuine deletion in a >cap mailbox is then left stale-but-present — the safe
/// direction (no data loss).
fn snapshot_deletions(
    prior: &JmapCursor,
    sigs: &BTreeMap<String, String>,
    truncated: bool,
) -> Vec<String> {
    if truncated {
        return Vec::new();
    }
    prior
        .k
        .keys()
        .filter(|id| !sigs.contains_key(*id))
        .cloned()
        .collect()
}

/// The bits of a JMAP session resource sync needs: where to POST method calls,
/// which account to scope them to, and the `downloadUrl` template for raw blobs.
struct Session {
    api_url: String,
    account_id: String,
    download_url: Option<String>,
}

/// The per-mailbox cursor: `emailId → keyword signature`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct JmapCursor {
    #[serde(default)]
    k: BTreeMap<String, String>,
}

impl JmapCursor {
    fn decode(cursor: Option<&Cursor>) -> Option<Self> {
        cursor.and_then(|c| serde_json::from_str(&c.0).ok())
    }

    fn encode(&self) -> Cursor {
        Cursor::new(serde_json::to_string(self).unwrap_or_default())
    }
}

/// The mail account id from a session resource: the primary mail account, else
/// any account that advertises the mail capability, else the first account.
fn resolve_account_id(session: &Value) -> Option<String> {
    if let Some(id) = session
        .pointer("/primaryAccounts/urn:ietf:params:jmap:mail")
        .and_then(Value::as_str)
    {
        return Some(id.to_string());
    }
    let accounts = session.get("accounts").and_then(Value::as_object)?;
    accounts.keys().next().cloned()
}

/// Validate `methodResponses[0]` is the expected method (not a JMAP `error`) and
/// return its argument object.
fn method_result<'a>(resp: &'a Value, expected: &str) -> Result<&'a Value> {
    let entry = resp
        .pointer("/methodResponses/0")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::provider("JMAP response has no methodResponses[0]"))?;
    let name = entry.first().and_then(Value::as_str).unwrap_or("");
    let args = entry
        .get(1)
        .ok_or_else(|| Error::provider("JMAP method response has no args"))?;
    if name == "error" {
        let kind = args
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(Error::provider(format!("JMAP {expected} error: {kind}")));
    }
    Ok(args)
}

/// Map a JMAP `Email` object to the canonical [`Email`].
fn jmap_to_email(e: &Value, mailbox: &Mailbox) -> Email {
    let uid = e
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let message_id = e
        .get("messageId")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(str::to_string);
    let from = addr_list(e.get("from")).into_iter().next();
    let to = addr_list(e.get("to"));
    let cc = addr_list(e.get("cc"));
    let subject = e
        .get("subject")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let received_at = e
        .get("receivedAt")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    Email {
        id: EmailId::new(),
        workspace_id: mailbox.workspace_id,
        mailbox_id: mailbox.id,
        uid,
        message_id,
        from,
        to,
        cc,
        subject,
        received_at,
        body_text: body_part_value(e, "textBody"),
        body_html: body_part_value(e, "htmlBody"),
        has_attachments: e
            .get("hasAttachment")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        flags: keyword_tokens(e),
        labels: Vec::new(),
        // The raw RFC 5322 bytes aren't part of the structured `Email/get`; they're
        // downloaded separately from the message's `blobId` via the session
        // `downloadUrl` and filled in by `fetch_emails` (bounded + fail-soft). Left
        // `None` here so a mapping used without a download (or when the blob GET
        // fails) still yields a valid message — archival just no-ops for it.
        raw_ref: None,
        attachments: Vec::new(),
        raw: None,
    }
}

/// Resolve the first non-empty body value for a body-part list (`textBody` /
/// `htmlBody`) via the email's `bodyValues` map.
fn body_part_value(email: &Value, list_key: &str) -> Option<String> {
    let parts = email.get(list_key)?.as_array()?;
    let body_values = email.get("bodyValues")?.as_object()?;
    for part in parts {
        if let Some(pid) = part.get("partId").and_then(Value::as_str) {
            if let Some(val) = body_values
                .get(pid)
                .and_then(|v| v.get("value"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// Map a JMAP address-list property (`[{name,email}, …]`) to [`EmailAddress`]es.
fn addr_list(v: Option<&Value>) -> Vec<EmailAddress> {
    v.and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let address = a.get("email").and_then(Value::as_str)?.to_string();
                    let name = a
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    Some(EmailAddress { name, address })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// JMAP `keywords` (`{"$seen": true, …}`) → sorted provider flag tokens.
fn keyword_tokens(email: &Value) -> Vec<String> {
    let mut tokens: Vec<String> = email
        .get("keywords")
        .and_then(Value::as_object)
        .map(|kw| {
            kw.iter()
                .filter(|(_, v)| v.as_bool() == Some(true))
                .filter_map(|(k, _)| keyword_token(k).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    tokens.sort();
    tokens.dedup();
    tokens
}

fn keyword_token(keyword: &str) -> Option<&'static str> {
    match keyword {
        "$seen" => Some("seen"),
        "$flagged" => Some("flagged"),
        "$answered" => Some("answered"),
        "$draft" => Some("draft"),
        _ => None,
    }
}

/// Order-independent signature of a flag-token set (sorted uppercase initials).
fn flag_sig(tokens: &[String]) -> String {
    let mut chars: Vec<char> = tokens
        .iter()
        .filter_map(|t| t.chars().next())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    chars.sort_unstable();
    chars.dedup();
    chars.into_iter().collect()
}

/// The `blobId` of a JMAP `Email` (RFC 8620): the handle passed to the session
/// `downloadUrl` to fetch the message's raw RFC 5322 bytes.
fn blob_id(email: &Value) -> Option<&str> {
    email
        .get("blobId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Expand a JMAP `downloadUrl` URI template (RFC 8620 §6.2) for a raw-message
/// blob. `type`/`name` are only hints for the `Content-Type`/filename the server
/// puts on the response — they don't change which bytes come back — so we ask for
/// a generic octet-stream named after the blob.
fn expand_download_url(template: &str, account_id: &str, blob_id: &str) -> String {
    expand_uri_template(
        template,
        &[
            ("accountId", account_id),
            ("blobId", blob_id),
            ("type", "application/octet-stream"),
            ("name", blob_id),
        ],
    )
}

/// Minimal `{var}` URI-template substitution: replace each `{name}` whose `name`
/// is in `vars` with its percent-encoded value; leave an unknown `{…}` span (or a
/// `{` with no closing `}`) verbatim. JMAP's `downloadUrl` only uses simple
/// string expansion — no RFC 6570 operators (`{+var}`, `{?…}`) — so this is all
/// that's needed, and it pulls in no dependency.
fn expand_uri_template(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let name = &after[..close];
                match vars.iter().find(|(k, _)| *k == name) {
                    Some((_, val)) => out.push_str(&pct_encode(val)),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Percent-encode a URI-template variable value: keep the RFC 3986 unreserved set
/// (`A-Za-z0-9-._~`) verbatim and `%XX`-encode every other byte. Conservative
/// enough to be safe in both the path and query segments of a `downloadUrl`.
fn pct_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

/// Stream a response body into memory, bounded to [`crate::MAX_HTTP_RESPONSE_BYTES`]
/// — the raw-bytes twin of [`crate::read_json_capped`], for the raw-blob download
/// (a blob is bytes, not JSON). Exceeding the cap errors rather than buffering an
/// unbounded body from a buggy/compromised server.
async fn read_bytes_capped(mut resp: reqwest::Response) -> Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| Error::provider(format!("read JMAP blob: {e}")))?
    {
        if buf.len() + chunk.len() > crate::MAX_HTTP_RESPONSE_BYTES {
            return Err(Error::provider(format!(
                "JMAP blob exceeds the {}-byte cap; refusing to buffer it",
                crate::MAX_HTTP_RESPONSE_BYTES
            )));
        }
        buf.extend_from_slice(chunk.as_ref());
    }
    Ok(buf)
}

fn ensure_success(resp: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(Error::unauthorized(format!(
            "JMAP {what} returned {status}"
        )));
    }
    if !status.is_success() {
        return Err(Error::provider(format!("JMAP {what} returned {status}")));
    }
    Ok(resp)
}

fn opt_str(config: &Value, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mailbox() -> Mailbox {
        Mailbox {
            id: catalerum_core::MailboxId::new(),
            workspace_id: WorkspaceId::new(),
            connection_id: ConnectionId::new(),
            external_id: "mb1".into(),
            name: "Inbox".into(),
            read_only: true,
        }
    }

    #[test]
    fn from_config_requires_url_and_token() {
        let (ws, c) = (WorkspaceId::new(), ConnectionId::new());
        assert!(JmapProvider::from_config(ws, c, &json!({"token": "t"})).is_err());
        assert!(JmapProvider::from_config(ws, c, &json!({"session_url": "https://x/s"})).is_err());
        let ok = JmapProvider::from_config(
            ws,
            c,
            &json!({"session_url": "https://x/s", "token": "t", "account_id": "a"}),
        )
        .unwrap();
        assert_eq!(ok.account_id.as_deref(), Some("a"));
    }

    #[test]
    fn resolves_primary_then_first_account() {
        let primary = json!({
            "apiUrl": "https://x/api",
            "primaryAccounts": { "urn:ietf:params:jmap:mail": "acc-9" },
            "accounts": { "acc-1": {}, "acc-9": {} }
        });
        assert_eq!(resolve_account_id(&primary).as_deref(), Some("acc-9"));
        let no_primary = json!({ "accounts": { "only": {} } });
        assert_eq!(resolve_account_id(&no_primary).as_deref(), Some("only"));
    }

    #[test]
    fn method_result_detects_errors() {
        let ok = json!({ "methodResponses": [["Email/get", { "list": [] }, "0"]] });
        assert!(method_result(&ok, "Email/get").is_ok());
        let err = json!({ "methodResponses": [["error", { "type": "unknownMethod" }, "0"]] });
        let e = method_result(&err, "Email/get").unwrap_err();
        assert!(format!("{e}").contains("unknownMethod"));
    }

    #[test]
    fn maps_a_full_jmap_email() {
        let e = json!({
            "id": "M1",
            "messageId": ["<abc@x.com>"],
            "from": [{ "name": "Ada", "email": "ada@x.com" }],
            "to": [{ "email": "bob@x.com" }],
            "cc": [],
            "subject": "Hello",
            "receivedAt": "2026-06-18T09:30:00Z",
            "keywords": { "$seen": true, "$flagged": true },
            "hasAttachment": true,
            "textBody": [{ "partId": "p1", "type": "text/plain" }],
            "htmlBody": [{ "partId": "p2", "type": "text/html" }],
            "bodyValues": {
                "p1": { "value": "plain body" },
                "p2": { "value": "<p>html body</p>" }
            }
        });
        let mb = mailbox();
        let email = jmap_to_email(&e, &mb);
        assert_eq!(email.uid, "M1");
        assert_eq!(email.message_id.as_deref(), Some("<abc@x.com>"));
        assert_eq!(email.from.as_ref().unwrap().address, "ada@x.com");
        assert_eq!(email.from.as_ref().unwrap().name.as_deref(), Some("Ada"));
        assert_eq!(email.to.len(), 1);
        assert_eq!(email.subject, "Hello");
        assert!(email.has_attachments);
        assert_eq!(email.body_text.as_deref(), Some("plain body"));
        assert_eq!(email.body_html.as_deref(), Some("<p>html body</p>"));
        assert_eq!(email.flags, vec!["flagged".to_string(), "seen".to_string()]);
        assert_eq!(
            email.received_at.unwrap().to_rfc3339(),
            "2026-06-18T09:30:00+00:00"
        );
        assert_eq!(email.workspace_id, mb.workspace_id);
        assert_eq!(email.mailbox_id, mb.id);
    }

    #[test]
    fn keyword_signature_is_order_independent() {
        let a = json!({ "keywords": { "$seen": true, "$flagged": true } });
        let b = json!({ "keywords": { "$flagged": true, "$seen": true } });
        assert_eq!(flag_sig(&keyword_tokens(&a)), flag_sig(&keyword_tokens(&b)));
        // A false keyword is not set.
        let c = json!({ "keywords": { "$seen": false } });
        assert_eq!(keyword_tokens(&c), Vec::<String>::new());
    }

    #[test]
    fn cursor_round_trips() {
        let mut k = BTreeMap::new();
        k.insert("M1".to_string(), "S".to_string());
        let c = JmapCursor { k }.encode();
        assert_eq!(
            JmapCursor::decode(Some(&c)).unwrap().k.get("M1").unwrap(),
            "S"
        );
        assert!(JmapCursor::decode(Some(&Cursor::new("opaque"))).is_none());
    }

    #[test]
    fn snapshot_deletions_only_on_a_complete_snapshot() {
        // Prior knew M1, M2, M3; the current snapshot has M1 + a new M4 (M2/M3 gone).
        let prior = JmapCursor {
            k: ["M1", "M2", "M3"]
                .into_iter()
                .map(|id| (id.to_string(), "s".to_string()))
                .collect(),
        };
        let mut sigs = BTreeMap::new();
        sigs.insert("M1".to_string(), "s".to_string());
        sigs.insert("M4".to_string(), "s".to_string());

        // Complete snapshot → M2/M3 are genuine deletions.
        let mut del = snapshot_deletions(&prior, &sigs, false);
        del.sort();
        assert_eq!(del, vec!["M2".to_string(), "M3".to_string()]);

        // Truncated snapshot (mailbox > SNAPSHOT_CAP) → emit NO deletions: M2/M3 may
        // merely have fallen past the newest-N window, so deleting them would lose
        // still-existing mail.
        assert!(snapshot_deletions(&prior, &sigs, true).is_empty());
    }

    #[test]
    fn full_email_get_requests_blob_id_and_reads_it() {
        // The full fetch must ask for `blobId` — that's the handle we download.
        assert!(EMAIL_PROPERTIES.contains(&"blobId"));
        assert!(EMAIL_PROPERTIES.contains(&"id"));
        // Round-trip: a blobId in an `Email/get` fixture is read back out.
        assert_eq!(blob_id(&json!({ "id": "M1", "blobId": "B7" })), Some("B7"));
        assert_eq!(blob_id(&json!({ "id": "M1" })), None);
        assert_eq!(blob_id(&json!({ "id": "M1", "blobId": "" })), None);
    }

    #[test]
    fn expands_download_url_percent_encoding_values() {
        let t = "https://h/dl/{accountId}/{blobId}/{type}/{name}";
        // account has a space, blob has a slash → both percent-encoded; the
        // octet-stream `type` hint encodes its `/` too, and `name` reuses the blob.
        let url = expand_download_url(t, "ac me", "b/2");
        assert_eq!(
            url,
            "https://h/dl/ac%20me/b%2F2/application%2Foctet-stream/b%2F2"
        );
    }

    #[test]
    fn uri_template_passthrough_and_unknown_vars() {
        // Unreserved chars pass through untouched; an unknown `{var}` and an
        // unterminated `{` are both left verbatim (only known vars expand).
        let out = expand_uri_template("x/{blobId}/{unknown}/{trailing", &[("blobId", "a~b.c_d-e")]);
        assert_eq!(out, "x/a~b.c_d-e/{unknown}/{trailing");
        // Every reserved/non-ASCII byte is `%XX` (upper-hex) encoded.
        assert_eq!(pct_encode("a b/c?=&é"), "a%20b%2Fc%3F%3D%26%C3%A9");
    }

    // --- Fake-transport end-to-end sync (raw-blob download) --------------------

    const RAW_MSG: &[u8] = b"From: a@x.com\r\nSubject: Hi\r\n\r\nHello raw body\r\n";

    /// Read one HTTP request off `stream` → `(method, path, body)`. Minimal, just
    /// enough for the JMAP client: request line + `Content-Length`-delimited body.
    async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<(String, String, Vec<u8>)> {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 2048];
        let header_end = loop {
            let n = stream.read(&mut tmp).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let header = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let request_line = header.lines().next()?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next()?.to_string();
        let path = parts.next()?.to_string();
        let content_length = header
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
            })
            .unwrap_or(0);
        let mut body = buf[header_end..].to_vec();
        while body.len() < content_length {
            let n = stream.read(&mut tmp).await.ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        Some((method, path, body))
    }

    /// Spawn a throwaway JMAP server (session + `Email/query`/`Email/get` + a raw
    /// blob download) on an ephemeral port; returns its base URL. The blob GET
    /// returns `200`+[`RAW_MSG`] when `blob_ok`, else `404`.
    async fn spawn_jmap_server(blob_ok: bool) -> String {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let session_json = json!({
            "apiUrl": format!("{base}/api"),
            "primaryAccounts": { "urn:ietf:params:jmap:mail": "acc" },
            "accounts": { "acc": {} },
            "downloadUrl": format!("{base}/download/{{accountId}}/{{blobId}}/{{type}}/{{name}}"),
        })
        .to_string();

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let Some((method, path, body)) = read_request(&mut stream).await else {
                    continue;
                };
                let (status, ctype, payload): (&str, &str, Vec<u8>) = if method == "GET"
                    && path.starts_with("/download/")
                {
                    if blob_ok {
                        ("200 OK", "application/octet-stream", RAW_MSG.to_vec())
                    } else {
                        ("404 Not Found", "text/plain", b"gone".to_vec())
                    }
                } else if method == "GET" {
                    (
                        "200 OK",
                        "application/json",
                        session_json.clone().into_bytes(),
                    )
                } else {
                    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let m = v
                        .pointer("/methodCalls/0/0")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let wants_blob = v
                        .pointer("/methodCalls/0/1/properties")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().any(|p| p.as_str() == Some("blobId")))
                        .unwrap_or(false);
                    let out = if m == "Email/query" {
                        json!({ "methodResponses": [["Email/query", { "ids": ["M1"] }, "0"]] })
                    } else if m == "Email/get" && wants_blob {
                        json!({ "methodResponses": [["Email/get", { "list": [{
                                "id": "M1", "blobId": "B1", "subject": "Hi",
                                "receivedAt": "2026-06-18T09:30:00Z",
                                "from": [{ "email": "a@x.com" }],
                                "messageId": ["<m@x>"],
                                "keywords": { "$seen": true },
                                "textBody": [{ "partId": "p1" }],
                                "bodyValues": { "p1": { "value": "body" } }
                            }] }, "0"]] })
                    } else if m == "Email/get" {
                        json!({ "methodResponses": [["Email/get", { "list": [{
                                "id": "M1", "keywords": { "$seen": true }
                            }] }, "0"]] })
                    } else {
                        json!({ "methodResponses": [["error", { "type": "unknownMethod" }, "0"]] })
                    };
                    (
                        "200 OK",
                        "application/json",
                        serde_json::to_vec(&out).unwrap(),
                    )
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&payload).await;
                let _ = stream.shutdown().await;
            }
        });
        base
    }

    fn provider_for(base: &str) -> JmapProvider {
        JmapProvider::from_config(
            WorkspaceId::new(),
            ConnectionId::new(),
            &json!({ "session_url": format!("{base}/session"), "token": "t" }),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn sync_downloads_raw_blob_when_available() {
        let base = spawn_jmap_server(true).await;
        let batch = provider_for(&base).sync(&mailbox(), None).await.unwrap();
        assert_eq!(batch.upserts.len(), 1);
        let email = &batch.upserts[0];
        assert_eq!(email.uid, "M1");
        // The raw RFC 5322 bytes were downloaded from the blob and stashed for archival.
        assert_eq!(email.raw.as_deref(), Some(RAW_MSG));
    }

    #[tokio::test]
    async fn sync_succeeds_with_no_raw_when_blob_download_404s() {
        let base = spawn_jmap_server(false).await;
        let batch = provider_for(&base).sync(&mailbox(), None).await.unwrap();
        // Fail-soft: the message still lands (structured fields intact), just with
        // `raw: None` — archival no-ops for it and the sync as a whole succeeds.
        assert_eq!(batch.upserts.len(), 1);
        assert!(batch.upserts[0].raw.is_none());
        assert_eq!(batch.upserts[0].subject, "Hi");
    }
}
