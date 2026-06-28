//! Pod-to-pod forwarding endpoint (multi-pod HA, SOUL §16 M7 / §20).
//!
//! `POST /internal/pod` executes one forwarded terminal-session op on the pod
//! that owns the session's PTY. There is **no session `Auth`** here: the body is
//! an AES-256-GCM-sealed envelope under a subkey of the shared
//! `[secrets].master_key` (see `crate::pod_forward`), so only a peer pod holding
//! that key can produce a request that authenticates — the envelope *is* the
//! authorization, exactly like the signed-token public routes. Without a master
//! key the route `404`s (single-pod dev).
//!
//! Unary ops answer `200` with a sealed outcome (op errors ride *inside* the
//! envelope, rebuilt as their original kind on the requesting pod). The `Output`
//! op streams sealed, sequence-numbered frames of the live PTY output instead.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use futures::StreamExt;

use catalerum_core::error::Error;
use catalerum_store::Store;

use crate::pod_forward::{PodComms, PodOp};
use crate::state::{AppState, StorageRegistry};
use crate::terminal::TerminalManager;

/// Forwarded `edit_file`/`create_file` payloads can carry file-sized content
/// (the file tools' hard read ceiling is 8 MiB); bound the sealed body well
/// above that but far below anything abusable.
const FORWARD_BODY_LIMIT: usize = 32 * 1024 * 1024;

/// Mount the internal pod-forwarding route.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/internal/pod", post(forward))
        .layer(DefaultBodyLimit::max(FORWARD_BODY_LIMIT))
}

/// Everything the owner side needs to answer one forwarded request — extracted
/// from [`AppState`] by the route handler, and constructible piecewise by the
/// two-pod integration test (which has no full `AppState`).
pub(crate) struct ForwardDeps {
    pub comms: Arc<PodComms>,
    /// This pod's stable identity — a request for a session this pod does *not*
    /// own is answered with a precise error, never forwarded onward.
    pub pod_id: String,
    pub store: Store,
    pub manager: Option<Arc<TerminalManager>>,
    /// Store-resolution deps for the forwarded `stage_object`/`store_object`
    /// bodies; `None` answers those ops with "storage is not configured".
    pub storage: Option<(StorageRegistry, Store)>,
    /// The pod-local computer-agent registry — the owner side of a forwarded
    /// `ComputerRequest`/`ComputerDisconnect` (SOUL §19/§20).
    pub computer_registry: Arc<crate::computer_registry::ComputerRegistry>,
}

/// `POST /internal/pod` — decrypt, execute locally, answer sealed.
async fn forward(State(state): State<AppState>, body: Bytes) -> Response {
    // No master key → forwarding doesn't exist on this deployment.
    let Some(comms) = state.pod_comms().cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let deps = ForwardDeps {
        comms,
        pod_id: state.pod_id().to_string(),
        store: state.store().clone(),
        manager: state.terminal_manager().cloned(),
        storage: Some((state.storage().clone(), state.store().clone())),
        computer_registry: state.computer_registry(),
    };
    respond(deps, &body).await
}

/// Answer one sealed forwarded request. Split from the route handler so the
/// two-pod test can serve it without an `AppState`.
pub(crate) async fn respond(deps: ForwardDeps, body: &[u8]) -> Response {
    // A body that fails to authenticate gets a bare 403 — nothing about the
    // deployment is revealed to a caller without the key.
    let (req, req_nonce) = match deps.comms.open_request(body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "rejected pod-forward request");
            return StatusCode::FORBIDDEN.into_response();
        }
    };

    // Computer-agent ops (SOUL §19/§20) carry no terminal session: dispatch them
    // to the local computer registry *before* the terminal ownership check below.
    // A stale ownership key (the agent moved pods) surfaces as an `ok:false`
    // OpResponse from `execute_computer_op`, not a routing error.
    if matches!(
        req.op,
        PodOp::ComputerRequest { .. } | PodOp::ComputerDisconnect { .. }
    ) {
        let result = crate::pod_forward::execute_computer_op(&deps.computer_registry, &req).await;
        return sealed_unary(&deps.comms, &req_nonce, result);
    }

    // Only execute ops for sessions this pod actually owns: forwarding is a
    // single hop by construction (the requester read the owning pod off the
    // durable row), so a mismatch means a stale row or a misdirected request —
    // answer precisely rather than forwarding onward.
    let owned = deps
        .store
        .terminal_sessions()
        .get(req.workspace_id, req.session_id)
        .await
        .ok()
        .flatten()
        .is_some_and(|row| row.pod_id.as_deref() == Some(deps.pod_id.as_str()));
    if !owned {
        let err: catalerum_core::error::Result<serde_json::Value> = Err(Error::invalid(
            "terminal session is not owned by this pod (stale routing)",
        ));
        return sealed_unary(&deps.comms, &req_nonce, err);
    }

    let Some(manager) = deps.manager.clone() else {
        let err = Err(Error::invalid("terminals are not enabled on this pod"));
        return sealed_unary(&deps.comms, &req_nonce, err);
    };
    if matches!(req.op, PodOp::Output) {
        return output_stream(&manager, &deps.comms, &req, req_nonce).await;
    }
    let storage = deps.storage.as_ref().map(|(reg, db)| (reg, db));
    let result = crate::pod_forward::execute_op(&manager, storage, &req).await;
    sealed_unary(&deps.comms, &req_nonce, result)
}

/// Seal a unary outcome (result *or* error) into a `200` response.
fn sealed_unary(
    comms: &PodComms,
    req_nonce: &[u8; 12],
    result: catalerum_core::error::Result<serde_json::Value>,
) -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        comms.seal_response(req_nonce, &result),
    )
        .into_response()
}

/// Answer an `Output` op with a stream of sealed, sequence-numbered frames of
/// the session's live PTY output. Subscription errors (session gone, not live
/// here) come back as a sealed unary error on a `409` so the requester can
/// rebuild the precise message.
async fn output_stream(
    manager: &Arc<TerminalManager>,
    comms: &Arc<PodComms>,
    req: &crate::pod_forward::PodRequest,
    req_nonce: [u8; 12],
) -> Response {
    let output = match manager.output(req.workspace_id, req.session_id).await {
        Ok(o) => o,
        Err(e) => {
            let sealed = sealed_unary(comms, &req_nonce, Err(e));
            return with_status(sealed, StatusCode::CONFLICT);
        }
    };
    // Seal each output chunk as one counted frame; a sealing failure (RNG) ends
    // the stream — the peer surfaces the truncation as a stream error.
    let comms = comms.clone();
    let frames = output
        .enumerate()
        .map(move |(seq, chunk)| -> Result<Vec<u8>, std::io::Error> {
            let bytes = chunk.map_err(std::io::Error::other)?;
            comms
                .seal_frame(&req_nonce, seq as u64, &bytes)
                .map_err(std::io::Error::other)
        });
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        Body::from_stream(frames),
    )
        .into_response()
}

/// Re-status a sealed unary response (streaming errors ride a `409` so the
/// client knows not to expect frames).
fn with_status(mut resp: Response, status: StatusCode) -> Response {
    *resp.status_mut() = status;
    resp
}
