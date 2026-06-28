//! Cross-pod terminal-session forwarding (multi-pod HA, SOUL §16 M7 / §20).
//!
//! A terminal session's PTY is pod-local: only the pod that opened it holds the
//! live handle. Under the N-replica Deployment a request can land on any pod, so
//! instead of failing with "owned by another pod (enable session affinity)" the
//! non-owning pod now **routes the operation to the owner**:
//!
//! 1. **Discovery over Valkey** — every pod announces `cat:pod:{pod_id}` →
//!    `{"addr":"<ip>:<port>"}` on its heartbeat clock (main.rs) with a TTL, via
//!    the bus [`Registry`](catalerum_bus::Registry) role. A dead pod's entry
//!    lapses on its own; a lookup miss degrades to the precise "owner
//!    unreachable" error (never data loss — the PTY died with its pod anyway).
//! 2. **Encrypted transport** — the forwarded call is one `POST /internal/pod`
//!    carrying an AES-256-GCM-sealed envelope keyed by a subkey derived from
//!    `[secrets].master_key` (which every pod already shares, §13). The HTTP
//!    layer itself is plain in-cluster traffic: confidentiality, integrity *and*
//!    authentication all come from the AEAD — only a peer holding the master key
//!    can mint or read an envelope, so the route needs no session auth. A
//!    timestamp inside the sealed request bounds replay; a response is sealed
//!    against the request's nonce so it can't be swapped between calls.
//!
//! Unary ops answer with one sealed JSON payload; the `Output` op streams the
//! owner's live PTY output as length-prefixed sealed frames (a per-frame counter
//! in the AAD prevents reorder/replay within the stream).
//!
//! Forwarding is enabled only when a master key is configured (multi-pod
//! deployments require one anyway for shared credentials); without it the route
//! 404s and the manager keeps today's precise errors.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use bytes::{Buf, BytesMut};
use catalerum_bus::{pod_key, Bus};
use catalerum_core::computer::ComputerOp;
use catalerum_core::error::{Error, Result};
use catalerum_core::provider::ByteStream;
use catalerum_core::{ComputerAgentId, TerminalSessionId, UserId, WorkspaceId};
use futures::StreamExt;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};
use sha2::{Digest, Sha256};

/// Wire-format magic + version for both request and unary-response bodies.
const MAGIC: &[u8; 4] = b"CPF1";
/// AAD context for a sealed request.
const AAD_REQ: &[u8] = b"cat:pod-fwd:req:v1";
/// AAD context prefix for a sealed unary response (followed by the request nonce).
const AAD_RES: &[u8] = b"cat:pod-fwd:res:v1";
/// AAD context prefix for a sealed stream frame (request nonce + frame counter).
const AAD_STREAM: &[u8] = b"cat:pod-fwd:stm:v1";
/// How far a request's embedded timestamp may drift from the receiver's clock.
const REPLAY_WINDOW_MS: i64 = 30_000;
/// Registry announcement TTL — must comfortably outlive the 30s heartbeat clock.
pub const ANNOUNCE_TTL: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------
// Discovery (Valkey registry announcements)
// ---------------------------------------------------------------------------

/// A pod's registry announcement payload: where peers can reach it.
#[derive(Debug, Serialize, Deserialize)]
struct PodAnnouncement {
    /// `<ip>:<port>` of the pod's API listener, reachable on the pod network.
    addr: String,
}

/// Announce this pod's reachable address under `cat:pod:{pod_id}` (re-announced
/// on the heartbeat clock; the TTL retires a crashed pod's entry on its own).
pub async fn announce_self(bus: &Bus, pod_id: &str, addr: &str) {
    let payload = match serde_json::to_vec(&PodAnnouncement {
        addr: addr.to_string(),
    }) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "failed to encode pod announcement");
            return;
        }
    };
    if let Err(e) = bus
        .registry()
        .announce(&pod_key(pod_id), payload, ANNOUNCE_TTL)
        .await
    {
        tracing::warn!(error = %e, "failed to announce pod address on the bus registry");
    }
}

/// Look up a peer pod's announced address. `None` when the pod is gone (TTL
/// lapsed) or never announced.
pub(crate) async fn lookup_pod_addr(bus: &Bus, pod_id: &str) -> Result<Option<String>> {
    let raw = bus
        .registry()
        .lookup(&pod_key(pod_id))
        .await
        .map_err(|e| Error::provider(format!("pod registry lookup failed: {e}")))?;
    Ok(raw.and_then(|bytes| {
        serde_json::from_slice::<PodAnnouncement>(&bytes)
            .ok()
            .map(|a| a.addr)
    }))
}

/// The address this pod advertises to peers: the configured `[server].pod_ip`
/// (k8s downward-API `status.podIP`), else the auto-detected primary local IP,
/// combined with `listen`'s port. `None` when neither resolves — forwarding
/// then degrades to the precise "route to that pod" errors.
pub fn advertised_addr(listen: &str, pod_ip: &str) -> Option<String> {
    let port = listen.rsplit_once(':').map(|(_, p)| p)?.trim();
    if port.is_empty() || port.parse::<u16>().is_err() {
        return None;
    }
    let ip = match pod_ip.trim() {
        "" => detect_local_ip()?,
        configured => configured.to_string(),
    };
    Some(format!("{ip}:{port}"))
}

/// Best-effort primary-interface IP: the local address of a UDP socket
/// "connected" to a routable target (no packet is sent). Loopback-only hosts
/// yield their loopback address, which still works for same-host multi-process
/// dev setups.
fn detect_local_ip() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:9").ok()?; // TEST-NET-1: routable-looking, never sent to
    Some(sock.local_addr().ok()?.ip().to_string())
}

// ---------------------------------------------------------------------------
// The sealed envelope
// ---------------------------------------------------------------------------

/// AES-256-GCM cipher for pod-forward envelopes, keyed by a subkey derived from
/// the shared `[secrets].master_key` (domain-separated so the raw master key is
/// never used as a traffic key). Mirrors catalerum-store's credential `Cipher`,
/// plus AAD binding.
pub struct PodCipher {
    key: LessSafeKey,
    rng: SystemRandom,
}

impl PodCipher {
    /// Derive the forwarding subkey from the 32-byte master key.
    #[must_use]
    pub fn from_master_key(master_key: &[u8; 32]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"catalerum:pod-forward:v1");
        hasher.update(master_key);
        let subkey: [u8; 32] = hasher.finalize().into();
        let unbound =
            UnboundKey::new(&AES_256_GCM, &subkey).expect("SHA-256 output is a valid AES-256 key");
        Self {
            key: LessSafeKey::new(unbound),
            rng: SystemRandom::new(),
        }
    }

    /// Seal `plaintext` bound to `aad`; returns `(nonce, ciphertext_with_tag)`.
    fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<([u8; NONCE_LEN], Vec<u8>)> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        self.rng
            .fill(&mut nonce_bytes)
            .map_err(|_| Error::other("secure RNG failure"))?;
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let mut in_out = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(nonce, Aad::from(aad), &mut in_out)
            .map_err(|_| Error::other("pod-forward encryption failed"))?;
        Ok((nonce_bytes, in_out))
    }

    /// Open a sealed payload bound to `aad`. Fails closed on a wrong key, a
    /// tampered ciphertext, or a mismatched AAD context.
    fn open(&self, aad: &[u8], nonce_bytes: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
        let nonce = Nonce::try_assume_unique_for_key(nonce_bytes)
            .map_err(|_| Error::invalid("malformed pod-forward nonce"))?;
        let mut in_out = ciphertext.to_vec();
        let plain = self
            .key
            .open_in_place(nonce, Aad::from(aad), &mut in_out)
            .map_err(|_| Error::invalid("pod-forward envelope failed to authenticate"))?;
        Ok(plain.to_vec())
    }
}

/// The AAD a unary response is sealed under: the response context plus the
/// request's nonce, so a sealed response can't be replayed against another call.
fn response_aad(req_nonce: &[u8]) -> Vec<u8> {
    let mut aad = AAD_RES.to_vec();
    aad.extend_from_slice(req_nonce);
    aad
}

/// The AAD one output-stream frame is sealed under: stream context + request
/// nonce + the frame's position, so frames can't be dropped-and-replayed or
/// reordered undetected.
fn stream_aad(req_nonce: &[u8], seq: u64) -> Vec<u8> {
    let mut aad = AAD_STREAM.to_vec();
    aad.extend_from_slice(req_nonce);
    aad.extend_from_slice(&seq.to_be_bytes());
    aad
}

/// `MAGIC || nonce || ciphertext` — the framing shared by requests and unary
/// responses.
fn encode_body(nonce: &[u8; NONCE_LEN], ciphertext: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(MAGIC.len() + NONCE_LEN + ciphertext.len());
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(nonce);
    body.extend_from_slice(ciphertext);
    body
}

/// Split a `MAGIC || nonce || ciphertext` body.
fn decode_body(body: &[u8]) -> Result<(&[u8], &[u8])> {
    if body.len() < MAGIC.len() + NONCE_LEN || &body[..MAGIC.len()] != MAGIC {
        return Err(Error::invalid("malformed pod-forward envelope"));
    }
    let rest = &body[MAGIC.len()..];
    Ok(rest.split_at(NONCE_LEN))
}

// ---------------------------------------------------------------------------
// Ops + request/response payloads
// ---------------------------------------------------------------------------

/// One forwarded terminal-session operation, executed on the owning pod exactly
/// as if the request had landed there. `data`/`output` ride as base64 so raw PTY
/// bytes survive JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PodOp {
    /// [`TerminalManager::write`](crate::terminal::TerminalManager::write).
    Write { data_b64: String },
    /// [`read_wait`](crate::terminal::TerminalManager::read_wait) (`wait_secs`
    /// 0 = plain read). The owner runs the whole drain-until-quiet loop so a
    /// waited read costs one round trip, not one per poll.
    Read { max_bytes: u64, wait_secs: u64 },
    /// [`resize`](crate::terminal::TerminalManager::resize).
    Resize { cols: u16, rows: u16 },
    /// [`close`](crate::terminal::TerminalManager::close) — the owner kills the
    /// PTY and marks the durable row closed.
    Close,
    /// [`output`](crate::terminal::TerminalManager::output) — answered with a
    /// sealed frame **stream**, not a unary payload.
    Output,
    /// [`persist`](crate::terminal::TerminalManager::persist) — the files live
    /// on the owner; object storage is shared, so the owner uploads directly.
    Persist {
        prefix: String,
        source_subdir: Option<String>,
    },
    /// [`read_file`](crate::terminal::TerminalManager::read_file).
    ReadFile {
        path: String,
        offset: Option<u64>,
        limit: Option<u64>,
    },
    /// Read a complete binary image file for native model input.
    ReadMediaFile { path: String },
    /// [`write_file`](crate::terminal::TerminalManager::write_file).
    WriteFile { path: String, content: String },
    /// [`edit_file`](crate::terminal::TerminalManager::edit_file).
    EditFile {
        path: String,
        old: String,
        new: String,
        replace_all: bool,
    },
    /// The `stage_object` tool body (store resolution happens on the owner;
    /// `user_id` carries the caller's per-user default-store preference across).
    StageObject {
        store: Option<String>,
        key: String,
        dest_path: Option<String>,
        user_id: Option<UserId>,
    },
    /// The `store_object` tool body (see [`PodOp::StageObject`]).
    StoreObject {
        store: Option<String>,
        key: Option<String>,
        path: String,
        user_id: Option<UserId>,
    },
    /// A **computer-agent** op (SOUL §19/§20) forwarded to the pod that holds the
    /// agent's live WebSocket: the owner runs `computer_op` against its local
    /// [`ComputerRegistry`](crate::computer_registry) connection and answers with
    /// the agent's `OpResponse` as JSON. Handled in `routes::internal::respond`
    /// (via [`execute_computer_op`]) **before** the terminal ownership check — a
    /// computer op carries no terminal session. The inner op is named
    /// `computer_op` to avoid colliding with the outer `op` tag.
    ComputerRequest {
        agent_id: ComputerAgentId,
        computer_op: ComputerOp,
        timeout_ms: u64,
    },
    /// Ask the owner pod to drop an agent's live connection (on revoke), so a
    /// revoke on any pod tears the socket down at once rather than waiting for the
    /// owner's next heartbeat revoked-check.
    ComputerDisconnect { agent_id: ComputerAgentId },
}

/// The sealed request payload.
#[derive(Debug, Serialize, Deserialize)]
pub struct PodRequest {
    /// Sender clock (ms since epoch) — bounds replay to [`REPLAY_WINDOW_MS`].
    pub ts_ms: i64,
    /// The requesting pod (diagnostics only; authenticity comes from the AEAD).
    pub from: String,
    pub workspace_id: WorkspaceId,
    pub session_id: TerminalSessionId,
    #[serde(flatten)]
    pub op: PodOp,
}

/// The sealed unary response payload: the op's JSON result, or a typed error to
/// rebuild on the requesting side.
#[derive(Debug, Serialize, Deserialize)]
struct PodOutcome {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Json>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Rebuild a [`PodOutcome`] error as the same kind of [`Error`] the owner saw,
/// so a forwarded call surfaces to the agent/tool exactly like a local one.
fn outcome_to_result(outcome: PodOutcome) -> Result<Json> {
    if outcome.ok {
        return Ok(outcome.result.unwrap_or(Json::Null));
    }
    let msg = outcome.error.unwrap_or_else(|| "unknown error".to_string());
    Err(match outcome.error_kind.as_deref() {
        Some("invalid") => Error::invalid(msg),
        Some("provider") => Error::provider(msg),
        _ => Error::other(msg),
    })
}

/// Classify an [`Error`] for the wire (mirrored by [`outcome_to_result`]).
fn error_kind(e: &Error) -> &'static str {
    // The core error's Display is stable; kind classification keeps the precise
    // "invalid" (agent-visible, actionable) vs "provider/other" split across the
    // hop without exhaustively matching every variant.
    match e {
        Error::Invalid(_) => "invalid",
        Error::Provider(_) => "provider",
        _ => "other",
    }
}

// ---------------------------------------------------------------------------
// Shared comms handle
// ---------------------------------------------------------------------------

/// The shared pod-to-pod comms state: the envelope cipher plus this pod's
/// identity/address. Built once in `AppState` when a master key is configured.
pub struct PodComms {
    cipher: PodCipher,
    /// This pod's stable id (matches `terminal_sessions.pod_id` stamps).
    pub pod_id: String,
    /// The address announced to peers, when one could be determined.
    pub advertised_addr: Option<String>,
}

impl PodComms {
    /// Build from the decoded 32-byte master key.
    #[must_use]
    pub fn new(master_key: &[u8; 32], pod_id: String, advertised_addr: Option<String>) -> Self {
        Self {
            cipher: PodCipher::from_master_key(master_key),
            pod_id,
            advertised_addr,
        }
    }

    /// Seal an outgoing request; returns `(body, request_nonce)` — keep the
    /// nonce to authenticate the response.
    fn seal_request(&self, req: &PodRequest) -> Result<(Vec<u8>, [u8; NONCE_LEN])> {
        let plain = serde_json::to_vec(req)
            .map_err(|e| Error::other(format!("encoding pod-forward request: {e}")))?;
        let (nonce, ct) = self.cipher.seal(AAD_REQ, &plain)?;
        Ok((encode_body(&nonce, &ct), nonce))
    }

    /// Open an incoming request body; returns the request plus its nonce (to
    /// seal the response against). Rejects stale timestamps (replay window).
    pub fn open_request(&self, body: &[u8]) -> Result<(PodRequest, [u8; NONCE_LEN])> {
        let (nonce, ct) = decode_body(body)?;
        let plain = self.cipher.open(AAD_REQ, nonce, ct)?;
        let req: PodRequest = serde_json::from_slice(&plain)
            .map_err(|e| Error::invalid(format!("malformed pod-forward request: {e}")))?;
        let now = chrono::Utc::now().timestamp_millis();
        if (now - req.ts_ms).abs() > REPLAY_WINDOW_MS {
            return Err(Error::invalid("stale pod-forward request (replay window)"));
        }
        let mut nonce_arr = [0u8; NONCE_LEN];
        nonce_arr.copy_from_slice(nonce);
        Ok((req, nonce_arr))
    }

    /// Seal a unary op outcome against the request's nonce.
    pub fn seal_response(&self, req_nonce: &[u8; NONCE_LEN], result: &Result<Json>) -> Vec<u8> {
        let outcome = match result {
            Ok(v) => PodOutcome {
                ok: true,
                result: Some(v.clone()),
                error_kind: None,
                error: None,
            },
            Err(e) => PodOutcome {
                ok: false,
                result: None,
                error_kind: Some(error_kind(e).to_string()),
                error: Some(e.to_string()),
            },
        };
        // Serialization of this struct can't fail; sealing only on RNG failure —
        // degrade to an empty body, which the peer rejects as malformed.
        let plain = serde_json::to_vec(&outcome).unwrap_or_default();
        match self.cipher.seal(&response_aad(req_nonce), &plain) {
            Ok((nonce, ct)) => encode_body(&nonce, &ct),
            Err(_) => Vec::new(),
        }
    }

    /// Open a unary response, verifying it answers *this* request.
    fn open_response(&self, req_nonce: &[u8; NONCE_LEN], body: &[u8]) -> Result<Json> {
        let (nonce, ct) = decode_body(body)?;
        let plain = self.cipher.open(&response_aad(req_nonce), nonce, ct)?;
        let outcome: PodOutcome = serde_json::from_slice(&plain)
            .map_err(|e| Error::provider(format!("malformed pod-forward response: {e}")))?;
        outcome_to_result(outcome)
    }

    /// Seal one output-stream frame (`[u32 len][nonce][ciphertext]`).
    pub fn seal_frame(
        &self,
        req_nonce: &[u8; NONCE_LEN],
        seq: u64,
        chunk: &[u8],
    ) -> Result<Vec<u8>> {
        let (nonce, ct) = self.cipher.seal(&stream_aad(req_nonce, seq), chunk)?;
        let len = u32::try_from(NONCE_LEN + ct.len())
            .map_err(|_| Error::other("pod-forward frame too large"))?;
        let mut frame = Vec::with_capacity(4 + NONCE_LEN + ct.len());
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(&nonce);
        frame.extend_from_slice(&ct);
        Ok(frame)
    }
}

// ---------------------------------------------------------------------------
// The forwarding client
// ---------------------------------------------------------------------------

/// How the [`TerminalManager`](crate::terminal::TerminalManager) reaches a
/// session's owning pod. Object-safe so the manager doesn't depend on the HTTP
/// client (tests substitute an in-process impl).
#[async_trait]
pub trait PodForwarder: Send + Sync {
    /// Execute a unary `op` for `(workspace, session)` on pod `pod`.
    async fn call(
        &self,
        pod: &str,
        workspace_id: WorkspaceId,
        session_id: TerminalSessionId,
        op: PodOp,
    ) -> Result<Json>;

    /// Subscribe to the session's live output on pod `pod`.
    async fn output(
        &self,
        pod: &str,
        workspace_id: WorkspaceId,
        session_id: TerminalSessionId,
    ) -> Result<ByteStream>;
}

/// The real forwarder: Valkey discovery + sealed HTTP to the owner's
/// `/internal/pod`.
pub struct HttpPodForwarder {
    comms: Arc<PodComms>,
    bus: Bus,
    http: reqwest::Client,
}

impl HttpPodForwarder {
    /// Build over the shared comms handle and bus.
    #[must_use]
    pub fn new(comms: Arc<PodComms>, bus: Bus) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client build cannot fail with static options");
        Self { comms, bus, http }
    }

    /// Resolve `pod`'s announced address or fail with the precise routing error.
    async fn addr_of(&self, pod: &str) -> Result<String> {
        lookup_pod_addr(&self.bus, pod).await?.ok_or_else(|| {
            Error::invalid(format!(
                "terminal session's owning pod (`{pod}`) is no longer reachable \
                 (not announced on the bus registry); the session died with it"
            ))
        })
    }

    /// Build and seal the request for `op`; returns `(url, body, req_nonce)`.
    async fn prepare(
        &self,
        pod: &str,
        workspace_id: WorkspaceId,
        session_id: TerminalSessionId,
        op: PodOp,
    ) -> Result<(String, Vec<u8>, [u8; NONCE_LEN])> {
        let addr = self.addr_of(pod).await?;
        let req = PodRequest {
            ts_ms: chrono::Utc::now().timestamp_millis(),
            from: self.comms.pod_id.clone(),
            workspace_id,
            session_id,
            op,
        };
        let (body, nonce) = self.comms.seal_request(&req)?;
        Ok((format!("http://{addr}/internal/pod"), body, nonce))
    }

    /// Per-op total-request timeout: reads may legitimately block for their
    /// whole `wait_secs`; storage syncs can move real bytes; everything else is
    /// interactive-fast.
    fn op_timeout(op: &PodOp) -> Duration {
        match op {
            PodOp::Read { wait_secs, .. } => Duration::from_secs(wait_secs + 30),
            PodOp::Persist { .. } | PodOp::StageObject { .. } | PodOp::StoreObject { .. } => {
                Duration::from_secs(300)
            }
            _ => Duration::from_secs(60),
        }
    }
}

#[async_trait]
impl PodForwarder for HttpPodForwarder {
    async fn call(
        &self,
        pod: &str,
        workspace_id: WorkspaceId,
        session_id: TerminalSessionId,
        op: PodOp,
    ) -> Result<Json> {
        let timeout = Self::op_timeout(&op);
        let (url, body, nonce) = self.prepare(pod, workspace_id, session_id, op).await?;
        let resp = self
            .http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .timeout(timeout)
            .body(body)
            .send()
            .await
            .map_err(|e| Error::provider(format!("forwarding to pod `{pod}` failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::provider(format!(
                "pod `{pod}` rejected the forwarded request: HTTP {}",
                resp.status()
            )));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Error::provider(format!("reading pod `{pod}` response: {e}")))?;
        self.comms.open_response(&nonce, &bytes)
    }

    async fn output(
        &self,
        pod: &str,
        workspace_id: WorkspaceId,
        session_id: TerminalSessionId,
    ) -> Result<ByteStream> {
        let (url, body, nonce) = self
            .prepare(pod, workspace_id, session_id, PodOp::Output)
            .await?;
        let resp = self
            .http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            // No total timeout: a live output subscription is open-ended (the
            // connect timeout still bounds a dead peer).
            .body(body)
            .send()
            .await
            .map_err(|e| Error::provider(format!("forwarding to pod `{pod}` failed: {e}")))?;
        if !resp.status().is_success() {
            // A subscription error rides back as a sealed unary outcome (409) so
            // the owner's precise error survives the hop; other statuses degrade
            // to a generic message.
            let status = resp.status();
            if let Ok(bytes) = resp.bytes().await {
                self.comms.open_response(&nonce, &bytes)?;
            }
            return Err(Error::provider(format!(
                "pod `{pod}` rejected the forwarded output request: HTTP {status}"
            )));
        }
        Ok(decode_frame_stream(
            self.comms.clone(),
            nonce,
            resp.bytes_stream(),
        ))
    }
}

/// Decode + decrypt a `[u32 len][nonce][ct]` frame stream into plain output
/// bytes, enforcing the per-frame sequence counter. Ends when the peer closes
/// the connection; any malformed/tampered frame ends it with an error.
fn decode_frame_stream(
    comms: Arc<PodComms>,
    req_nonce: [u8; NONCE_LEN],
    upstream: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
) -> ByteStream {
    struct DecodeState<S> {
        upstream: S,
        buf: BytesMut,
        seq: u64,
        done: bool,
    }
    let state = DecodeState {
        upstream: Box::pin(upstream),
        buf: BytesMut::new(),
        seq: 0,
        done: false,
    };
    futures::stream::unfold(
        (comms, req_nonce, state),
        |(comms, nonce, mut st)| async move {
            loop {
                if st.done {
                    return None;
                }
                // A complete frame in the buffer?
                if st.buf.len() >= 4 {
                    let len =
                        u32::from_be_bytes([st.buf[0], st.buf[1], st.buf[2], st.buf[3]]) as usize;
                    if st.buf.len() >= 4 + len {
                        st.buf.advance(4);
                        let frame = st.buf.split_to(len);
                        if frame.len() < NONCE_LEN {
                            st.done = true;
                            return Some((
                                Err(Error::provider("malformed pod-forward output frame")),
                                (comms, nonce, st),
                            ));
                        }
                        let (fr_nonce, ct) = frame.split_at(NONCE_LEN);
                        let aad = stream_aad(&nonce, st.seq);
                        match comms.cipher.open(&aad, fr_nonce, ct) {
                            Ok(plain) => {
                                st.seq += 1;
                                return Some((Ok(plain), (comms, nonce, st)));
                            }
                            Err(e) => {
                                st.done = true;
                                return Some((Err(e), (comms, nonce, st)));
                            }
                        }
                    }
                }
                // Need more bytes.
                match st.upstream.next().await {
                    Some(Ok(chunk)) => st.buf.extend_from_slice(&chunk),
                    Some(Err(e)) => {
                        st.done = true;
                        return Some((
                            Err(Error::provider(format!("pod-forward output stream: {e}"))),
                            (comms, nonce, st),
                        ));
                    }
                    None => {
                        st.done = true;
                        if !st.buf.is_empty() {
                            return Some((
                                Err(Error::provider("truncated pod-forward output stream")),
                                (comms, nonce, st),
                            ));
                        }
                        return None;
                    }
                }
            }
        },
    )
    .boxed()
}

// ---------------------------------------------------------------------------
// Owner-side execution
// ---------------------------------------------------------------------------

/// Execute one forwarded unary op against this pod's local managers — the owner
/// side of the hop (`POST /internal/pod`). Each arm is exactly what the
/// corresponding tool/route does locally, so a forwarded call is
/// indistinguishable from one that landed on the right pod. `Output` is handled
/// by the streaming route handler, never here. `storage` carries the
/// store-resolution deps for the `stage_object`/`store_object` bodies; `None`
/// answers those with "not configured".
pub(crate) async fn execute_op(
    manager: &std::sync::Arc<crate::terminal::TerminalManager>,
    storage: Option<(&crate::state::StorageRegistry, &catalerum_store::Store)>,
    req: &PodRequest,
) -> Result<Json> {
    let ws = req.workspace_id;
    let id = req.session_id;
    match &req.op {
        PodOp::Write { data_b64 } => {
            manager.write(ws, id, from_b64(data_b64)?).await?;
            Ok(json!({ "ok": true }))
        }
        PodOp::Read {
            max_bytes,
            wait_secs,
        } => {
            let max = usize::try_from(*max_bytes).unwrap_or(usize::MAX);
            let bytes = manager.read_wait(ws, id, max, *wait_secs).await?;
            Ok(json!({ "output_b64": b64(&bytes) }))
        }
        PodOp::Resize { cols, rows } => {
            manager.resize(ws, id, *cols, *rows).await?;
            Ok(json!({ "ok": true }))
        }
        PodOp::Close => {
            manager.close(ws, id).await?;
            Ok(json!({ "ok": true }))
        }
        PodOp::Output => Err(Error::invalid("output is a streaming op")),
        PodOp::Persist {
            prefix,
            source_subdir,
        } => {
            let keys = manager
                .persist(ws, id, prefix, source_subdir.as_deref())
                .await?;
            Ok(json!({ "keys": keys }))
        }
        PodOp::ReadFile {
            path,
            offset,
            limit,
        } => {
            let rf = manager
                .read_file(
                    ws,
                    id,
                    path,
                    offset.map(|v| usize::try_from(v).unwrap_or(usize::MAX)),
                    limit.map(|v| usize::try_from(v).unwrap_or(usize::MAX)),
                )
                .await?;
            Ok(json!({
                "content": rf.content,
                "truncated": rf.truncated,
                "total_lines": rf.total_lines,
                "size": rf.size,
            }))
        }
        PodOp::ReadMediaFile { path } => {
            let media = manager.read_media_file(ws, id, path).await?;
            Ok(json!({
                "content_b64": b64(&media.bytes),
                "size": media.size,
            }))
        }
        PodOp::WriteFile { path, content } => {
            let (bytes, overwrote) = manager.write_file(ws, id, path, content).await?;
            Ok(json!({ "bytes": bytes, "overwrote": overwrote }))
        }
        PodOp::EditFile {
            path,
            old,
            new,
            replace_all,
        } => {
            let replacements = manager
                .edit_file(ws, id, path, old, new, *replace_all)
                .await?;
            Ok(json!({ "replacements": replacements }))
        }
        PodOp::StageObject {
            store,
            key,
            dest_path,
            user_id,
        } => {
            let (registry, store_db) = storage
                .ok_or_else(|| Error::invalid("object storage is not configured on this pod"))?;
            crate::terminal::stage_object_via(
                manager,
                registry,
                store_db,
                ws,
                id,
                *user_id,
                store.as_deref(),
                key,
                dest_path.as_deref(),
            )
            .await
        }
        PodOp::StoreObject {
            store,
            key,
            path,
            user_id,
        } => {
            let (registry, store_db) = storage
                .ok_or_else(|| Error::invalid("object storage is not configured on this pod"))?;
            crate::terminal::store_object_via(
                manager,
                registry,
                store_db,
                ws,
                id,
                *user_id,
                store.as_deref(),
                key.as_deref(),
                path,
            )
            .await
        }
        // Computer-agent ops are dispatched in `routes::internal::respond` before
        // this terminal-only executor is reached (they carry no terminal session).
        PodOp::ComputerRequest { .. } | PodOp::ComputerDisconnect { .. } => Err(Error::invalid(
            "computer-agent ops are dispatched before the terminal executor",
        )),
    }
}

// ---------------------------------------------------------------------------
// Computer-agent forwarding (owner side + requester side)
// ---------------------------------------------------------------------------

/// Owner side of a forwarded computer-agent op: run it against this pod's live
/// [`ComputerRegistry`](crate::computer_registry) connection and answer with the
/// agent's [`OpResponse`](catalerum_core::computer::OpResponse) as JSON. A
/// dispatch failure (the agent isn't actually held here — a stale ownership key)
/// is folded into an `ok:false` `OpResponse` so the requester surfaces it as an
/// ordinary "not online" rather than a transport error. Called from
/// `routes::internal::respond`.
pub(crate) async fn execute_computer_op(
    registry: &Arc<crate::computer_registry::ComputerRegistry>,
    req: &PodRequest,
) -> Result<Json> {
    match &req.op {
        PodOp::ComputerRequest {
            agent_id,
            computer_op,
            timeout_ms,
        } => {
            let timeout = Duration::from_millis(*timeout_ms);
            let resp = registry
                .request_local(*agent_id, computer_op.clone(), timeout)
                .await
                .unwrap_or_else(|e| catalerum_core::computer::OpResponse::err("", e.to_string()));
            Ok(serde_json::to_value(resp).unwrap_or(Json::Null))
        }
        PodOp::ComputerDisconnect { agent_id } => {
            registry.disconnect(*agent_id).await;
            Ok(json!({ "disconnected": true }))
        }
        _ => Err(Error::invalid("not a computer-agent op")),
    }
}

/// Requester side: seal a computer-agent `op` and POST it to the owning `pod`'s
/// `/internal/pod`, returning the sealed JSON the owner produced. `workspace_id`
/// rides for logging/defense; the owner routes purely by the agent id carried in
/// `op` (the terminal `session_id` is unused for computer ops — a nil placeholder).
pub(crate) async fn forward_computer_op(
    comms: &PodComms,
    bus: &Bus,
    http: &reqwest::Client,
    pod: &str,
    workspace_id: WorkspaceId,
    op: PodOp,
    timeout: Duration,
) -> Result<Json> {
    let addr = lookup_pod_addr(bus, pod).await?.ok_or_else(|| {
        Error::invalid(format!(
            "the computer agent's owning pod (`{pod}`) is no longer reachable \
             (not announced on the bus registry)"
        ))
    })?;
    let req = PodRequest {
        ts_ms: chrono::Utc::now().timestamp_millis(),
        from: comms.pod_id.clone(),
        workspace_id,
        // Computer ops carry no terminal session; the owner ignores this field.
        session_id: TerminalSessionId::from_uuid(uuid::Uuid::nil()),
        op,
    };
    let (body, nonce) = comms.seal_request(&req)?;
    let url = format!("http://{addr}/internal/pod");
    let resp = http
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .timeout(timeout)
        .body(body)
        .send()
        .await
        .map_err(|e| Error::provider(format!("forwarding to pod `{pod}` failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::provider(format!(
            "pod `{pod}` rejected the forwarded computer request: HTTP {}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| Error::provider(format!("reading pod `{pod}` response: {e}")))?;
    comms.open_response(&nonce, &bytes)
}

// ---------------------------------------------------------------------------
// Base64 helpers shared by the op payloads
// ---------------------------------------------------------------------------

/// Standard base64 for raw PTY bytes riding a JSON op/result.
pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Decode [`b64`]-encoded bytes.
pub fn from_b64(s: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| Error::provider(format!("malformed base64 in pod-forward payload: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comms(pod: &str) -> PodComms {
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = i as u8;
        }
        PodComms::new(&key, pod.to_string(), None)
    }

    fn request(op: PodOp) -> PodRequest {
        PodRequest {
            ts_ms: chrono::Utc::now().timestamp_millis(),
            from: "pod-a".into(),
            workspace_id: WorkspaceId::new(),
            session_id: TerminalSessionId::new(),
            op,
        }
    }

    #[test]
    fn request_roundtrips_between_two_pods_sharing_the_key() {
        let a = comms("pod-a");
        let b = comms("pod-b");
        let req = request(PodOp::Write {
            data_b64: b64(b"echo hi\n"),
        });
        let (body, _) = a.seal_request(&req).unwrap();
        let (opened, _) = b.open_request(&body).unwrap();
        assert_eq!(opened.from, "pod-a");
        assert!(matches!(opened.op, PodOp::Write { .. }));
    }

    #[test]
    fn tampered_request_fails_closed() {
        let a = comms("pod-a");
        let (mut body, _) = a.seal_request(&request(PodOp::Close)).unwrap();
        let last = body.len() - 1;
        body[last] ^= 0xff;
        assert!(comms("pod-b").open_request(&body).is_err());
    }

    #[test]
    fn wrong_key_fails_closed() {
        let a = comms("pod-a");
        let (body, _) = a.seal_request(&request(PodOp::Close)).unwrap();
        let mut other_key = [0u8; 32];
        other_key[0] = 0xff;
        let stranger = PodComms::new(&other_key, "pod-x".into(), None);
        assert!(stranger.open_request(&body).is_err());
    }

    #[test]
    fn stale_request_is_rejected() {
        let a = comms("pod-a");
        let mut req = request(PodOp::Close);
        req.ts_ms -= REPLAY_WINDOW_MS + 1_000;
        let (body, _) = a.seal_request(&req).unwrap();
        let err = comms("pod-b").open_request(&body).unwrap_err();
        assert!(err.to_string().contains("stale"), "{err}");
    }

    #[test]
    fn response_binds_to_its_request_nonce() {
        let a = comms("pod-a");
        let b = comms("pod-b");
        let (_, nonce1) = a.seal_request(&request(PodOp::Close)).unwrap();
        let (_, nonce2) = a.seal_request(&request(PodOp::Close)).unwrap();
        let resp = b.seal_response(&nonce1, &Ok(json!({"ok": true})));
        // Correct pairing opens; a response replayed against another request fails.
        assert!(a.open_response(&nonce1, &resp).is_ok());
        assert!(a.open_response(&nonce2, &resp).is_err());
    }

    #[test]
    fn error_outcomes_rebuild_their_kind() {
        let a = comms("pod-a");
        let b = comms("pod-b");
        let (_, nonce) = a.seal_request(&request(PodOp::Close)).unwrap();
        let resp = b.seal_response(&nonce, &Err(Error::invalid("no such session")));
        let err = a.open_response(&nonce, &resp).unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{err}");
        assert!(err.to_string().contains("no such session"));
    }

    #[test]
    fn stream_frames_decrypt_in_order_and_reject_reorder() {
        let a = comms("pod-a");
        let (_, nonce) = a.seal_request(&request(PodOp::Output)).unwrap();
        let f0 = a.seal_frame(&nonce, 0, b"hello ").unwrap();
        let f1 = a.seal_frame(&nonce, 1, b"world").unwrap();
        // In-order decrypt succeeds…
        let (n0, c0) = f0[4..].split_at(NONCE_LEN);
        assert_eq!(
            a.cipher.open(&stream_aad(&nonce, 0), n0, c0).unwrap(),
            b"hello "
        );
        let (n1, c1) = f1[4..].split_at(NONCE_LEN);
        assert_eq!(
            a.cipher.open(&stream_aad(&nonce, 1), n1, c1).unwrap(),
            b"world"
        );
        // …but frame 1 presented at position 0 fails (reorder detected).
        assert!(a.cipher.open(&stream_aad(&nonce, 0), n1, c1).is_err());
    }

    #[test]
    fn frame_stream_reassembles_across_chunk_boundaries() {
        // The client-side decoder must reassemble frames split arbitrarily by the
        // transport. Seal two frames, concatenate, and feed them back in 3-byte
        // chunks.
        let a = comms("pod-a");
        let comms_arc = std::sync::Arc::new(comms("pod-a"));
        let (_, nonce) = a.seal_request(&request(PodOp::Output)).unwrap();
        let mut wire = comms_arc.seal_frame(&nonce, 0, b"first ").unwrap();
        wire.extend(comms_arc.seal_frame(&nonce, 1, b"second").unwrap());
        let chunks: Vec<reqwest::Result<bytes::Bytes>> = wire
            .chunks(3)
            .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
            .collect();
        let upstream = futures::stream::iter(chunks);
        let decoded = decode_frame_stream(comms_arc, nonce, upstream);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let out: Vec<Vec<u8>> = rt.block_on(async {
            decoded
                .map(|r| r.expect("frame decodes"))
                .collect::<Vec<_>>()
                .await
        });
        assert_eq!(out, vec![b"first ".to_vec(), b"second".to_vec()]);
    }

    #[test]
    fn advertised_addr_prefers_configured_ip() {
        assert_eq!(
            advertised_addr("0.0.0.0:8787", "10.42.0.7"),
            Some("10.42.0.7:8787".to_string())
        );
        assert_eq!(advertised_addr("bad-listen", "10.42.0.7"), None);
        // Auto-detect path returns *some* local ip with the listen port.
        if let Some(addr) = advertised_addr("0.0.0.0:9999", "") {
            assert!(addr.ends_with(":9999"), "{addr}");
        }
    }
}

/// Two-"pod" end-to-end forwarding (SOUL §16 M7): pod A owns a real local-shell
/// PTY; pod B (no backends) drives it through the sealed `/internal/pod` channel
/// — discovery over a shared in-process bus registry (standing in for Valkey),
/// AES-256-GCM envelopes under the shared master key. Gated on live Postgres.
#[cfg(test)]
mod two_pod_tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::routing::post;
    use axum::Router;
    use catalerum_bus::Bus;
    use catalerum_core::model::{ExecutorKind, TerminalSessionStatus};
    use catalerum_core::provider::Executor;
    use futures::StreamExt;

    use super::*;
    use crate::config::ExecConfig;
    use crate::routes::internal::{respond, ForwardDeps};
    use crate::terminal::TerminalManager;

    fn db_url() -> Option<String> {
        std::env::var("CATALERUM_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()
    }

    fn master_key() -> [u8; 32] {
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = 0x40 ^ (i as u8);
        }
        k
    }

    fn manager(
        store: &catalerum_store::Store,
        pod: &str,
        with_backend: bool,
    ) -> Arc<TerminalManager> {
        let cfg = ExecConfig {
            enabled: true,
            backend: "local".to_string(),
            shell: "/bin/sh".to_string(),
            ..Default::default()
        };
        let mut backends: HashMap<ExecutorKind, Arc<dyn Executor>> = HashMap::new();
        if with_backend {
            backends.insert(
                ExecutorKind::Local,
                Arc::new(catalerum_exec::LocalExecutor::new()),
            );
        }
        Arc::new(TerminalManager::new(
            backends,
            store.clone(),
            None,
            &cfg,
            None,
            pod.to_string(),
        ))
    }

    #[tokio::test]
    async fn pod_b_drives_a_session_owned_by_pod_a() {
        let Some(url) = db_url() else {
            eprintln!(
                "skipping pod_b_drives_a_session_owned_by_pod_a: set CATALERUM_TEST_DATABASE_URL"
            );
            return;
        };
        let store = catalerum_store::Store::connect(&url)
            .await
            .expect("connect+migrate");
        let ws = store
            .workspaces()
            .create("pod-fwd", &format!("pod-fwd-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");

        // One shared bus = the shared Valkey both pods would talk to.
        let bus = Bus::in_process();
        let key = master_key();

        // --- Pod A: owns the PTY, serves /internal/pod --------------------------
        let manager_a = manager(&store, "fwd-pod-a", true);
        let comms_a = Arc::new(PodComms::new(&key, "fwd-pod-a".into(), None));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        {
            let (manager_a, comms_a, store) = (manager_a.clone(), comms_a.clone(), store.clone());
            let app = Router::new().route(
                "/internal/pod",
                post(move |body: axum::body::Bytes| {
                    let deps = ForwardDeps {
                        comms: comms_a.clone(),
                        pod_id: "fwd-pod-a".into(),
                        store: store.clone(),
                        manager: Some(manager_a.clone()),
                        storage: None,
                        computer_registry: Arc::new(
                            crate::computer_registry::ComputerRegistry::new(
                                "fwd-pod-a".into(),
                                None,
                                None,
                            ),
                        ),
                    };
                    async move { respond(deps, &body).await }
                }),
            );
            tokio::spawn(async move {
                axum::serve(listener, app).await.expect("serve pod A");
            });
        }
        announce_self(&bus, "fwd-pod-a", &addr.to_string()).await;

        // --- Pod B: no backends, only the forwarder -----------------------------
        let manager_b = manager(&store, "fwd-pod-b", false);
        let comms_b = Arc::new(PodComms::new(&key, "fwd-pod-b".into(), None));
        manager_b.set_forwarder(Arc::new(HttpPodForwarder::new(comms_b, bus.clone())));

        // Open on A (stamps the row with fwd-pod-a)…
        let session = manager_a.open(ws.id, None).await.expect("open on A");
        let id = session.id;

        // …subscribe to the live output *through B* (sealed frame stream)…
        let mut output_via_b = manager_b.output(ws.id, id).await.expect("output via B");

        // …drive it through B: write + waited read are forwarded to A.
        manager_b
            .write(ws.id, id, b"echo forwarded-$((20+22))\n".to_vec())
            .await
            .expect("write via B");
        let out = manager_b
            .read_wait(ws.id, id, 0, 10)
            .await
            .expect("read via B");
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("forwarded-42"), "PTY output: {text}");

        // The forwarded output subscription saw the same bytes.
        let mut streamed = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while !String::from_utf8_lossy(&streamed).contains("forwarded-42") {
            let chunk = tokio::time::timeout_at(deadline, output_via_b.next())
                .await
                .expect("output stream stalled")
                .expect("output stream ended early")
                .expect("output frame decodes");
            streamed.extend_from_slice(&chunk);
        }

        // Workdir file ops forward too: create on B, read back on B (the file
        // physically lives on A's filesystem).
        let (bytes, overwrote) = manager_b
            .write_file(ws.id, id, "notes/hello.txt", "written across pods")
            .await
            .expect("write_file via B");
        assert_eq!((bytes, overwrote), (19, false));
        let rf = manager_b
            .read_file(ws.id, id, "notes/hello.txt", None, None)
            .await
            .expect("read_file via B");
        assert_eq!(rf.content, "written across pods");
        let replaced = manager_b
            .edit_file(ws.id, id, "notes/hello.txt", "across", "between", false)
            .await
            .expect("edit_file via B");
        assert_eq!(replaced, 1);

        // Close through B: A tears the PTY down and marks the row closed.
        manager_b.close(ws.id, id).await.expect("close via B");
        let row = store
            .terminal_sessions()
            .get(ws.id, id)
            .await
            .expect("row")
            .expect("row exists");
        assert_eq!(row.status, TerminalSessionStatus::Closed);
        assert!(manager_a
            .list(ws.id)
            .await
            .expect("list")
            .iter()
            .all(|s| s.id != id));
    }

    /// Without a valid envelope the endpoint reveals nothing: garbage → 403; a
    /// stranger keyed differently → 403.
    #[tokio::test]
    async fn endpoint_rejects_unauthenticated_bodies() {
        let Some(url) = db_url() else {
            eprintln!(
                "skipping endpoint_rejects_unauthenticated_bodies: set CATALERUM_TEST_DATABASE_URL"
            );
            return;
        };
        let store = catalerum_store::Store::connect(&url)
            .await
            .expect("connect+migrate");
        let comms = Arc::new(PodComms::new(&master_key(), "fwd-pod-a".into(), None));

        // Garbage body.
        let deps = ForwardDeps {
            comms: comms.clone(),
            pod_id: "fwd-pod-a".into(),
            store: store.clone(),
            manager: None,
            storage: None,
            computer_registry: Arc::new(crate::computer_registry::ComputerRegistry::new(
                "fwd-pod-a".into(),
                None,
                None,
            )),
        };
        let resp = respond(deps, b"not an envelope").await;
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);

        // A syntactically-valid envelope sealed under a DIFFERENT key.
        let mut other = master_key();
        other[0] ^= 0xff;
        let stranger = PodComms::new(&other, "intruder".into(), None);
        let req = PodRequest {
            ts_ms: chrono::Utc::now().timestamp_millis(),
            from: "intruder".into(),
            workspace_id: WorkspaceId::new(),
            session_id: TerminalSessionId::new(),
            op: PodOp::Close,
        };
        let (body, _) = stranger.seal_request(&req).unwrap();
        let deps = ForwardDeps {
            comms,
            pod_id: "fwd-pod-a".into(),
            store,
            manager: None,
            storage: None,
            computer_registry: Arc::new(crate::computer_registry::ComputerRegistry::new(
                "fwd-pod-a".into(),
                None,
                None,
            )),
        };
        let resp = respond(deps, &body).await;
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);
    }

    /// Cross-pod **computer-agent** forwarding (SOUL §19/§20): an agent's live
    /// socket is held on pod A; a `computer_*` op issued on pod B is routed to A
    /// (via the ownership key + the sealed `POST /internal/pod`), served by the
    /// agent there, and the response flows back — and a revoke on B tears the
    /// socket down on A. No DB needed: the fake agent is a local registration and
    /// ownership rides the in-process bus.
    #[tokio::test]
    async fn pod_b_drives_a_computer_agent_owned_by_pod_a() {
        use crate::computer_registry::{ComputerRegistry, DispatchError};
        use catalerum_core::computer::{ComputerCapabilities, OpResponse, ServerToAgent};

        // Shared in-process bus = the shared Valkey both pods use for the agent→pod
        // ownership key and the pod→addr announcements.
        let bus = Bus::in_process();
        let key = master_key();
        let ws = WorkspaceId::new();
        let agent_id = ComputerAgentId::new();

        // A lazy store the computer branch never queries (it returns before the
        // terminal ownership check).
        let lazy = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/catalerum_test")
            .expect("lazy pool");
        let store = catalerum_store::Store::new(lazy);

        // --- Pod A: holds the live agent socket + serves /internal/pod ----------
        let reg_a = Arc::new(ComputerRegistry::new(
            "cfwd-pod-a".into(),
            Some(bus.clone()),
            None,
        ));
        let comms_a = Arc::new(PodComms::new(&key, "cfwd-pod-a".into(), None));
        reg_a.set_pod_comms(comms_a.clone());

        // A fake agent: a live registration whose responder echoes each request.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let close = reg_a
            .connect(
                agent_id,
                ws,
                "boxA".into(),
                ComputerCapabilities::default(),
                tx,
            )
            .await;
        {
            let reg_a = reg_a.clone();
            tokio::spawn(async move {
                while let Some(frame) = rx.recv().await {
                    if let ServerToAgent::Request { id, op } = frame {
                        reg_a
                            .resolve_response(
                                agent_id,
                                OpResponse::ok(
                                    id,
                                    json!({ "served_by": "pod-a", "verb": op.verb() }),
                                ),
                            )
                            .await;
                    }
                }
            });
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        {
            let (reg_a, comms_a, store) = (reg_a.clone(), comms_a.clone(), store.clone());
            let app = Router::new().route(
                "/internal/pod",
                post(move |body: axum::body::Bytes| {
                    let deps = ForwardDeps {
                        comms: comms_a.clone(),
                        pod_id: "cfwd-pod-a".into(),
                        store: store.clone(),
                        manager: None,
                        storage: None,
                        computer_registry: reg_a.clone(),
                    };
                    async move { respond(deps, &body).await }
                }),
            );
            tokio::spawn(async move {
                axum::serve(listener, app).await.expect("serve pod A");
            });
        }
        announce_self(&bus, "cfwd-pod-a", &addr.to_string()).await;

        // --- Pod B: no local conn — must forward to pod A -----------------------
        let reg_b = Arc::new(ComputerRegistry::new(
            "cfwd-pod-b".into(),
            Some(bus.clone()),
            None,
        ));
        reg_b.set_pod_comms(Arc::new(PodComms::new(&key, "cfwd-pod-b".into(), None)));

        // The op is issued on pod B, forwarded to pod A, and served by the agent.
        let resp = reg_b
            .request(
                agent_id,
                ComputerOp::Stat {
                    cwd: None,
                    path: "/x".into(),
                },
                Duration::from_secs(5),
            )
            .await
            .expect("forwarded op response");
        assert!(resp.ok);
        assert_eq!(resp.data["served_by"], "pod-a");
        assert_eq!(resp.data["verb"], "stat");

        // A *local-only* request on B (the owner-side path) is Offline there.
        assert_eq!(
            reg_b
                .request_local(
                    agent_id,
                    ComputerOp::Stat {
                        cwd: None,
                        path: "/x".into(),
                    },
                    Duration::from_millis(50),
                )
                .await
                .unwrap_err(),
            DispatchError::Offline
        );

        // A revoke on pod B tears the socket down on pod A (cross-pod disconnect).
        reg_b.disconnect_everywhere(agent_id).await;
        assert!(
            !reg_a.is_online(agent_id).await,
            "pod A dropped the agent on a cross-pod disconnect"
        );
        tokio::time::timeout(Duration::from_secs(1), close.notified())
            .await
            .expect("pod A's close handle fired on cross-pod disconnect");
    }
}
