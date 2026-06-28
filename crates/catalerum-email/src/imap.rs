//! RFC 3501 IMAP email provider over implicit TLS (SOUL §28).
//!
//! A read-only ingest backend ([`EmailProvider`]) for a single IMAP folder
//! (default `INBOX`). catalerum **reads** mail — it never sends, replies, or
//! mutates server state (§14) — so the mailbox is opened with `EXAMINE`
//! (read-only) and bodies are fetched with `BODY.PEEK[]`, which never sets the
//! `\Seen` flag.
//!
//! ## Incrementality (SOUL §3.4)
//! IMAP identity is the message `UID`, stable within a `UIDVALIDITY` generation.
//! The per-mailbox [`Cursor`] encodes `{uidvalidity, uid → flag-signature}`. Each
//! sync:
//! 1. `EXAMINE` the folder → current `UIDVALIDITY`.
//! 2. `UID FETCH 1:* (FLAGS)` → the current `uid → flags` set (cheap; no bodies).
//! 3. A **delta** against the cursor: a uid whose flag-signature is new or changed
//!    is (re-)fetched in full and **upserted**; a uid present in the cursor but
//!    absent now is named in `deletions`. A `UIDVALIDITY` change invalidates every
//!    prior uid, so the whole folder is re-fetched.
//!
//! Because this is a true delta ([`is_incremental`](EmailProvider::is_incremental)
//! is `true`), the ingest worker never diff-reconciles — this provider is
//! authoritative for its own deletions.
//!
//! ## TLS & scope
//! Implicit TLS only (port 993 by default); STARTTLS upgrade and multi-folder
//! `LIST` discovery are future enhancements (one connection = one folder today,
//! mirroring the CalDAV provider). Credentials (username/password) are read from
//! the connection `config` in plaintext — the M-stage stub the CalDAV provider
//! also uses; the encrypted vault behind `credential_ref` lands later (SOUL §13).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_imap::types::{Fetch, Flag};
use async_trait::async_trait;
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

use catalerum_core::error::{Error, Result};
use catalerum_core::model::{Cursor, Email, Mailbox};
use catalerum_core::provider::{EmailProvider, SyncBatch};
use catalerum_core::{ConnectionId, WorkspaceId};

use crate::{parse_email, stable_mailbox_id};

/// The default IMAPS port (implicit TLS).
const DEFAULT_PORT: u16 = 993;

/// How many message bodies to (re-)fetch in a single sync tick. The cheap
/// `UID FETCH … (FLAGS)` pass covers the whole folder, but pulling every changed
/// body at once would balloon memory on a first sync of a large mailbox. The
/// most-recent `BODY_FETCH_CAP` changed uids are fetched each tick; the older
/// remainder is deferred to later ticks (the poller re-runs `sync` on a cadence),
/// converging without ever advancing the cursor past an un-fetched body.
const BODY_FETCH_CAP: usize = 500;

/// The per-tick delta computed from the cursor's prior flag signatures and the
/// folder's current ones: which bodies to fetch (capped, most-recent first),
/// which uids vanished, and the next cursor map. Pure — unit-tested below.
#[derive(Debug, PartialEq, Eq)]
struct ImapDelta {
    /// uids whose body to (re-)fetch this tick, ascending (a capped tail of the
    /// changed set — the highest/newest uids).
    to_fetch: Vec<u32>,
    /// uids present in the cursor but gone from the folder now.
    deletions: Vec<String>,
    /// The cursor's `uid → flag-signature` map to persist after this tick.
    next_f: BTreeMap<String, String>,
    /// Whether any changed uid was deferred (more work remains for a later tick).
    has_more: bool,
}

/// Compute the sync delta against `prior_flags`, fetching at most `cap` bodies.
///
/// `changed` = uids that are new or whose flag-signature differs from the cursor.
/// The newest `cap` of those are fetched this tick; the older remainder is
/// **deferred**. A deferred uid must never get its *current* signature written to
/// the cursor — that would mark its un-fetched body as synced and it would never
/// be fetched. So a deferred uid that was already tracked keeps its **prior**
/// signature (a later deletion stays detectable, a flag change still re-fetches),
/// and a brand-new deferred uid is **omitted** entirely (re-read as new next
/// tick). The empty signature can be a legitimate value (a message with no mapped
/// flags), so omission — not a sentinel — is what distinguishes "not yet synced".
fn compute_delta(
    prior_flags: &BTreeMap<String, String>,
    current_sig: &BTreeMap<String, String>,
    cap: usize,
) -> ImapDelta {
    // Changed uids, ascending (IMAP uids are monotonic, so ascending = oldest..newest).
    let mut changed: Vec<u32> = current_sig
        .iter()
        .filter(|(uid, sig)| prior_flags.get(*uid).map(|s| s != *sig).unwrap_or(true))
        .filter_map(|(uid, _)| uid.parse::<u32>().ok())
        .collect();
    changed.sort_unstable();

    // Fetch the newest `cap`; defer the older head of the list.
    let split = changed.len().saturating_sub(cap.max(1));
    let (deferred_uids, fetch_uids) = changed.split_at(split);
    let to_fetch = fetch_uids.to_vec();
    let deferred: std::collections::BTreeSet<String> =
        deferred_uids.iter().map(u32::to_string).collect();

    let deletions: Vec<String> = prior_flags
        .keys()
        .filter(|uid| !current_sig.contains_key(*uid))
        .cloned()
        .collect();

    let mut next_f = BTreeMap::new();
    for (uid, sig) in current_sig {
        if deferred.contains(uid) {
            // Keep prior sig if previously tracked; otherwise omit (new + deferred).
            if let Some(prev) = prior_flags.get(uid) {
                next_f.insert(uid.clone(), prev.clone());
            }
        } else {
            next_f.insert(uid.clone(), sig.clone());
        }
    }

    ImapDelta {
        to_fetch,
        deletions,
        next_f,
        has_more: !deferred.is_empty(),
    }
}

/// A read-only IMAP [`EmailProvider`] for one folder (SOUL §28).
#[derive(Clone, Debug)]
pub struct ImapProvider {
    workspace_id: WorkspaceId,
    connection_id: ConnectionId,
    host: String,
    port: u16,
    username: String,
    password: String,
    /// The IMAP folder to ingest (also the mailbox `external_id`/`name`).
    folder: String,
}

impl ImapProvider {
    /// Build from a connection's `config` JSON. Required: `host`, `username`,
    /// `password`. Optional: `port` (default 993), `mailbox`/`folder` (default
    /// `INBOX`).
    pub fn from_config(
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        config: &Value,
    ) -> Result<Self> {
        let host = req_str(config, "host")?;
        let username = req_str(config, "username")?;
        let password = req_str(config, "password")?;
        let port = config
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|p| u16::try_from(p).ok())
            .unwrap_or(DEFAULT_PORT);
        let folder = opt_str(config, "mailbox")
            .or_else(|| opt_str(config, "folder"))
            .unwrap_or_else(|| "INBOX".to_string());
        Ok(Self {
            workspace_id,
            connection_id,
            host,
            port,
            username,
            password,
            folder,
        })
    }

    /// The single [`Mailbox`] this connection exposes (the configured folder).
    fn mailbox(&self) -> Mailbox {
        Mailbox {
            id: stable_mailbox_id(self.connection_id, &self.folder),
            workspace_id: self.workspace_id,
            connection_id: self.connection_id,
            external_id: self.folder.clone(),
            name: self.folder.clone(),
            read_only: true,
        }
    }

    /// Open a TLS connection and authenticate, returning a logged-in session.
    async fn connect(&self) -> Result<async_imap::Session<TlsStream<TcpStream>>> {
        // Bound the whole TCP + TLS + LOGIN handshake: a raw socket has no default
        // connect timeout, so an unreachable or unresponsive IMAP server would
        // otherwise hang the sync worker for minutes (the OS TCP timeout) or
        // forever (a server that accepts then stalls). The subsequent FETCHes are
        // intentionally *not* bounded here — a large mailbox sync shouldn't abort
        // mid-transfer.
        let handshake = async {
            let tcp = TcpStream::connect((self.host.as_str(), self.port))
                .await
                .map_err(|e| {
                    Error::provider(format!("IMAP connect {}:{}: {e}", self.host, self.port))
                })?;
            let connector = tls_connector()?;
            let server_name = ServerName::try_from(self.host.clone())
                .map_err(|e| Error::invalid(format!("invalid IMAP host `{}`: {e}", self.host)))?;
            let tls = connector
                .connect(server_name, tcp)
                .await
                .map_err(|e| Error::provider(format!("IMAP TLS handshake: {e}")))?;
            let client = async_imap::Client::new(tls);
            client
                .login(&self.username, &self.password)
                .await
                .map_err(|(e, _client)| Error::unauthorized(format!("IMAP login failed: {e}")))
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), handshake)
            .await
            .map_err(|_| {
                Error::provider(format!(
                    "IMAP handshake to {}:{} timed out",
                    self.host, self.port
                ))
            })?
    }

    /// Sync the folder against `cursor`, returning a delta batch. Split out so the
    /// caller can always `logout` afterwards, success or failure.
    async fn sync_folder(
        &self,
        session: &mut async_imap::Session<TlsStream<TcpStream>>,
        mailbox: &Mailbox,
        cursor: Option<Cursor>,
    ) -> Result<SyncBatch<Email>> {
        let meta = session
            .examine(&self.folder)
            .await
            .map_err(|e| Error::provider(format!("IMAP EXAMINE {}: {e}", self.folder)))?;
        let uidvalidity = meta.uid_validity.unwrap_or(0);
        let exists = meta.exists;

        // A UIDVALIDITY change reassigns every uid, so prior cursor state is moot.
        let prior = ImapCursor::decode(cursor.as_ref());
        let prior_flags = match prior {
            Some(p) if p.v == uidvalidity => p.f,
            _ => BTreeMap::new(),
        };

        // Cheap pass: current uid → flags for the whole folder (no bodies).
        let mut current_sig: BTreeMap<String, String> = BTreeMap::new();
        let mut current_tokens: HashMap<u32, Vec<String>> = HashMap::new();
        if exists > 0 {
            let stream = session
                .uid_fetch("1:*", "(FLAGS)")
                .await
                .map_err(|e| Error::provider(format!("IMAP UID FETCH FLAGS: {e}")))?;
            let fetches: Vec<Fetch> = stream
                .try_collect()
                .await
                .map_err(|e| Error::provider(format!("IMAP read FLAGS: {e}")))?;
            for f in &fetches {
                if let Some(uid) = f.uid {
                    let tokens = flag_tokens(f.flags());
                    current_sig.insert(uid.to_string(), flag_sig(&tokens));
                    current_tokens.insert(uid, tokens);
                }
            }
        }

        // Delta: new/flag-changed uids (re-)fetch; vanished uids delete. The body
        // fetch is capped per tick (`BODY_FETCH_CAP`), so a first sync of a huge
        // folder can't pull every message body into memory at once — the older
        // remainder is deferred to later poller ticks via the cursor. Deletions
        // are computed over the *whole* folder (the FLAGS pass is uncapped), so
        // the cap never causes a false deletion. See [`compute_delta`].
        let delta = compute_delta(&prior_flags, &current_sig, BODY_FETCH_CAP);

        // Full-body fetch for the (capped) changed set. BODY.PEEK[] never sets \Seen.
        let mut upserts = Vec::new();
        if !delta.to_fetch.is_empty() {
            let set = uid_set(&delta.to_fetch);
            let stream = session
                .uid_fetch(set, "(UID FLAGS BODY.PEEK[])")
                .await
                .map_err(|e| Error::provider(format!("IMAP UID FETCH bodies: {e}")))?;
            let fetches: Vec<Fetch> = stream
                .try_collect()
                .await
                .map_err(|e| Error::provider(format!("IMAP read bodies: {e}")))?;
            for f in &fetches {
                let Some(uid) = f.uid else { continue };
                let Some(body) = f.body() else { continue };
                let tokens = current_tokens
                    .get(&uid)
                    .cloned()
                    .unwrap_or_else(|| flag_tokens(f.flags()));
                match parse_email(
                    uid.to_string(),
                    tokens,
                    body,
                    mailbox.workspace_id,
                    mailbox.id,
                ) {
                    Ok(email) => upserts.push(email),
                    Err(e) => {
                        tracing::warn!(uid, error = %e, "skipping unparseable IMAP message")
                    }
                }
            }
        }

        let next = ImapCursor {
            v: uidvalidity,
            f: delta.next_f,
        };
        Ok(SyncBatch {
            upserts,
            deletions: delta.deletions,
            next_cursor: next.encode(),
            has_more: delta.has_more,
        })
    }
}

#[async_trait]
impl EmailProvider for ImapProvider {
    async fn list_mailboxes(&self) -> Result<Vec<Mailbox>> {
        Ok(vec![self.mailbox()])
    }

    async fn sync(&self, mailbox: &Mailbox, cursor: Option<Cursor>) -> Result<SyncBatch<Email>> {
        let mut session = self.connect().await?;
        let result = self.sync_folder(&mut session, mailbox, cursor).await;
        // Best-effort logout — never let a logout error mask the sync outcome.
        let _ = session.logout().await;
        result
    }

    fn is_incremental(&self) -> bool {
        true
    }
}

/// The per-mailbox cursor: the `UIDVALIDITY` generation plus a `uid → flag
/// signature` map, so a re-sync can tell new/flag-changed/removed uids apart.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ImapCursor {
    /// `UIDVALIDITY` of the folder at the time the cursor was written.
    v: u32,
    /// uid (as string) → sorted single-char flag signature (e.g. `"FS"`).
    #[serde(default)]
    f: BTreeMap<String, String>,
}

impl ImapCursor {
    fn decode(cursor: Option<&Cursor>) -> Option<Self> {
        cursor.and_then(|c| serde_json::from_str(&c.0).ok())
    }

    fn encode(&self) -> Cursor {
        Cursor::new(serde_json::to_string(self).unwrap_or_default())
    }
}

/// Map an IMAP flag to catalerum's provider-native flag token (mirrors the
/// Maildir tokens; `is_unread` keys on `"seen"`). Transient/structural flags
/// (`\Recent`, `\*`, custom keywords) are ignored.
fn flag_token(flag: &Flag) -> Option<&'static str> {
    match flag {
        Flag::Seen => Some("seen"),
        Flag::Answered => Some("answered"),
        Flag::Flagged => Some("flagged"),
        Flag::Draft => Some("draft"),
        Flag::Deleted => Some("trashed"),
        _ => None,
    }
}

/// Collect a fetch's flags into sorted, de-duplicated provider tokens.
fn flag_tokens<'a>(flags: impl Iterator<Item = Flag<'a>>) -> Vec<String> {
    let mut tokens: Vec<String> = flags
        .filter_map(|f| flag_token(&f).map(str::to_string))
        .collect();
    tokens.sort();
    tokens.dedup();
    tokens
}

/// A compact, order-independent signature of a flag-token set: the sorted
/// uppercase first letters joined (e.g. `["flagged","seen"] → "FS"`). The five
/// mapped tokens have distinct initials, so the signature is unambiguous.
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

/// Render a sorted uid list as an IMAP sequence-set (comma-joined).
fn uid_set(uids: &[u32]) -> String {
    uids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Build a rustls TLS connector trusting the Mozilla webpki root set, with the
/// `ring` crypto provider (the workspace's rustls provider).
fn tls_connector() -> Result<TlsConnector> {
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| Error::provider(format!("rustls config: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

fn req_str(config: &Value, key: &str) -> Result<String> {
    opt_str(config, key)
        .ok_or_else(|| Error::invalid(format!("imap email config requires a non-empty `{key}`")))
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
    use serde_json::json;

    fn ids() -> (WorkspaceId, ConnectionId) {
        (WorkspaceId::new(), ConnectionId::new())
    }

    #[test]
    fn from_config_reads_required_and_defaults() {
        let (ws, c) = ids();
        let p = ImapProvider::from_config(
            ws,
            c,
            &json!({"provider":"imap","host":"imap.example.com","username":"me","password":"pw"}),
        )
        .unwrap();
        assert_eq!(p.host, "imap.example.com");
        assert_eq!(p.port, 993);
        assert_eq!(p.folder, "INBOX");
        assert_eq!(p.mailbox().external_id, "INBOX");
        assert!(p.mailbox().read_only);
    }

    #[test]
    fn from_config_honours_port_and_folder() {
        let (ws, c) = ids();
        let p = ImapProvider::from_config(
            ws,
            c,
            &json!({"host":"h","username":"u","password":"p","port":1143,"mailbox":"Archive"}),
        )
        .unwrap();
        assert_eq!(p.port, 1143);
        assert_eq!(p.folder, "Archive");
    }

    #[test]
    fn from_config_rejects_missing_credentials() {
        let (ws, c) = ids();
        assert!(ImapProvider::from_config(ws, c, &json!({"host":"h","username":"u"})).is_err());
        assert!(ImapProvider::from_config(ws, c, &json!({"username":"u","password":"p"})).is_err());
        // Blank values count as missing.
        assert!(ImapProvider::from_config(
            ws,
            c,
            &json!({"host":"  ","username":"u","password":"p"})
        )
        .is_err());
    }

    #[test]
    fn flag_signature_is_order_independent_and_unread_aware() {
        assert_eq!(
            flag_sig(&["seen".into(), "flagged".into()]),
            flag_sig(&["flagged".into(), "seen".into()])
        );
        assert_eq!(flag_sig(&["seen".into(), "flagged".into()]), "FS");
        assert_eq!(flag_sig(&[]), "");
    }

    #[test]
    fn cursor_round_trips_and_rejects_garbage() {
        let mut f = BTreeMap::new();
        f.insert("12".to_string(), "S".to_string());
        f.insert("13".to_string(), "FS".to_string());
        let c = ImapCursor { v: 42, f }.encode();
        let back = ImapCursor::decode(Some(&c)).unwrap();
        assert_eq!(back.v, 42);
        assert_eq!(back.f.get("13").unwrap(), "FS");
        // A legacy/opaque cursor decodes to None → treated as a full first sync.
        assert!(ImapCursor::decode(Some(&Cursor::new("opaque"))).is_none());
        assert!(ImapCursor::decode(None).is_none());
    }

    #[test]
    fn uid_set_is_comma_joined() {
        assert_eq!(uid_set(&[1, 5, 9]), "1,5,9");
        assert_eq!(uid_set(&[]), "");
    }

    fn bt(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn delta_first_sync_fetches_all_within_cap() {
        let current = bt(&[("1", "S"), ("2", ""), ("3", "FS")]);
        let d = compute_delta(&BTreeMap::new(), &current, 500);
        assert_eq!(d.to_fetch, vec![1, 2, 3]);
        assert!(d.deletions.is_empty());
        assert_eq!(d.next_f, current);
        assert!(!d.has_more);
    }

    #[test]
    fn delta_skips_unchanged_but_keeps_them_in_cursor() {
        let prior = bt(&[("1", "S"), ("2", "")]);
        let current = bt(&[("1", "S"), ("2", ""), ("3", "F")]); // only 3 is new
        let d = compute_delta(&prior, &current, 500);
        assert_eq!(d.to_fetch, vec![3]);
        assert_eq!(d.next_f, current); // all three tracked
        assert!(!d.has_more);
    }

    #[test]
    fn delta_flag_change_refetches() {
        let d = compute_delta(&bt(&[("7", "")]), &bt(&[("7", "S")]), 500);
        assert_eq!(d.to_fetch, vec![7]);
        assert_eq!(d.next_f.get("7").unwrap(), "S");
    }

    #[test]
    fn delta_emits_deletions_for_vanished_uids() {
        let prior = bt(&[("1", "S"), ("2", "F")]);
        let current = bt(&[("1", "S")]); // 2 gone
        let d = compute_delta(&prior, &current, 500);
        assert!(d.to_fetch.is_empty());
        assert_eq!(d.deletions, vec!["2".to_string()]);
        assert_eq!(d.next_f, bt(&[("1", "S")]));
    }

    #[test]
    fn delta_cap_defers_older_changed_and_omits_new_deferred() {
        // Five brand-new uids, cap 2 → fetch the newest two (4,5), defer 1,2,3.
        let current = bt(&[("1", ""), ("2", "S"), ("3", "F"), ("4", "S"), ("5", "")]);
        let d = compute_delta(&BTreeMap::new(), &current, 2);
        assert_eq!(d.to_fetch, vec![4, 5]);
        assert!(d.has_more);
        // Fetched uids carry their current sig; deferred-*new* uids are OMITTED so
        // they re-read as new next tick — even uid 1/5 whose real sig is "" (the
        // empty sig is a real value, so omission, not a sentinel, marks "unsynced").
        assert_eq!(d.next_f, bt(&[("4", "S"), ("5", "")]));
        assert!(!d.next_f.contains_key("1"));
        assert!(!d.next_f.contains_key("3"));
    }

    #[test]
    fn delta_deferred_previously_tracked_keeps_prior_sig() {
        // uid 1 was tracked (sig "S"); its flags changed to "FS" but newer changed
        // uids crowd it out of the cap, so it's deferred.
        let prior = bt(&[("1", "S")]);
        let current = bt(&[("1", "FS"), ("8", "S"), ("9", "F")]);
        let d = compute_delta(&prior, &current, 2);
        assert_eq!(d.to_fetch, vec![8, 9]); // newest two
                                            // Deferred-but-tracked → keep PRIOR sig "S", so a later deletion is still
                                            // detectable and the changed flags re-fetch on a subsequent tick.
        assert_eq!(d.next_f.get("1").unwrap(), "S");
        assert!(d.has_more);
    }

    #[test]
    fn delta_converges_over_ticks_with_a_tight_cap() {
        let current = bt(&[("1", ""), ("2", "S"), ("3", "F")]);
        let mut cursor = BTreeMap::new();
        let mut fetched = Vec::new();
        for _ in 0..3 {
            let d = compute_delta(&cursor, &current, 1);
            fetched.extend(d.to_fetch.iter().copied());
            cursor = d.next_f;
        }
        fetched.sort_unstable();
        assert_eq!(fetched, vec![1, 2, 3], "every uid fetched across ticks");
        assert_eq!(
            cursor, current,
            "cursor reflects the full state once drained"
        );
        // A further tick is a stable no-op.
        let stable = compute_delta(&cursor, &current, 1);
        assert!(stable.to_fetch.is_empty() && !stable.has_more);
    }
}
