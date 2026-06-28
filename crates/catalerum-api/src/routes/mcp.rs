//! MCP over **authenticated HTTP** (SOUL §26/§29) — the full streamable-HTTP
//! transport: request/response plus the server→client **SSE-streaming** half.
//!
//! The same external MCP server that `catalerum mcp` serves over stdio, exposed
//! inside the main API so a remote agent (Claude Code / Codex / opencode) can
//! connect over HTTP instead of spawning a local process.
//!
//! - **`POST /mcp`** — one JSON-RPC request, **content-negotiated** back. A plain
//!   JSON client gets `application/json`. A streamable-HTTP client (which sends
//!   `Accept: text/event-stream` alongside `application/json`) gets the response as
//!   an SSE stream: a single event for a fast request, or — when the request
//!   carries a `progressToken` in `_meta` — a stream of `notifications/progress`
//!   updates (a coarse `started` → `completed` bracket around the call) ending with
//!   the final response. Full backward compatibility: no `Accept` header, no
//!   streaming.
//! - **`GET /mcp`** (`Accept: text/event-stream`) — opens a standalone server→client
//!   SSE stream for **unsolicited** server messages, scoped to the authenticated
//!   bearer's **workspace** (the HTTP surface mints no `Mcp-Session-Id`, so the
//!   push channel is keyed by workspace — see [`catalerum_mcp::sse`]). Idle streams
//!   are held open with periodic keep-alive comments; a producer pushes through the
//!   process-global hub seam ([`crate::mcp_push_bridge`]), which fans out both
//!   locally and — once the bus bridge is installed — across pods (SOUL §16 M7).
//!
//! **Same enforcement as every other surface, no backdoor (principle 15).** Every
//! request — GET and POST — is authenticated (a session or workspace-bound service
//! token, §18) and the resulting [`ToolContext`] is scoped to that principal's
//! workspace plus the role's base capabilities (§19), identical to the chat agent
//! loop (`routes::ws`). Each `tools/call` is then deny-by-default at registry
//! dispatch, so a Viewer can list and read but not write, exactly as over stdio.
//! `POST /mcp` therefore needs no capability of its own; it grants nothing the
//! principal does not already hold. `GET /mcp` is different: its pushes are
//! workspace-wide and carry no per-message capability tag, so the stream itself
//! is gated on workspace-wide read authority ([`require_workspace_read`]) — a
//! grant-scoped token narrower than that is refused rather than handed pushes
//! its grant does not cover (§19).

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures::StreamExt;
use serde::Serialize;
use serde_json::Value;

use catalerum_core::capability::{Action, Capability};
use catalerum_core::model::McpEndpoint;
use catalerum_core::tool::ToolContext;
use catalerum_core::WorkspaceId;
use catalerum_mcp::{sse_frame, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, McpServer};

use crate::auth::Auth;
use crate::error::ApiResult;
use crate::mcp_endpoint::{build_endpoint_server, default_endpoint_caps};
use crate::mcp_providers::{SkillPromptProvider, WorkspaceResourceProvider};
use crate::state::AppState;

/// How often an idle `GET /mcp` stream emits a keep-alive comment so proxies and
/// load balancers don't drop the (otherwise silent) connection.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Mount the MCP-over-HTTP routes: `POST /mcp` (request/response, optionally
/// streaming) and `GET /mcp` (the server→client SSE stream).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/mcp", post(handle).get(handle_get))
        // Custom Boa-scripted endpoints (SOUL §26): by name under a workspace token,
        // or by a signed, shareable scoped token.
        .route("/mcp/e/{name}", post(handle_named_endpoint))
        .route("/mcp/s/{token}", post(handle_scoped_endpoint))
}

/// `POST /mcp/e/{name}` — serve a custom endpoint by name, authenticated with a
/// **workspace** token (session or service token). The caller's workspace scopes
/// which endpoint is reachable; the endpoint's own script + pinned scope do the
/// rest. A missing/disabled endpoint 404s.
///
/// **Caller authority bounds the endpoint (SOUL §19):** the endpoint's own
/// capability set (its pinned grant, else the read-only default) is
/// **intersected with the caller's effective authority**, so invoking an
/// endpoint can never exercise authority the caller does not themselves hold —
/// a Viewer cannot ride an endpoint's write-capable pinned grant. A caller
/// whose authority covers none of the endpoint's capabilities is `403`.
async fn handle_named_endpoint(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> Response {
    let ws = auth.principal().workspace_id;
    serve_endpoint(
        &state,
        ws,
        &name,
        req,
        accepts_event_stream(&headers),
        Some(auth.capabilities()),
    )
    .await
}

/// `POST /mcp/s/{token}` — serve a custom endpoint from a **signed scoped token**,
/// no login. The token carries `{workspace, endpoint, expiry}`; a bad/expired
/// token 404s (indistinguishable from an unknown endpoint, no probing signal).
///
/// The signature proves the token was minted here; the server-side record
/// (hash-only, SOUL §26) makes it **revocable** — a token whose row was revoked
/// (or whose endpoint was deleted, cascading the row away) 404s exactly like a
/// forged one, even though its signature still verifies.
async fn handle_scoped_endpoint(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> Response {
    let now = chrono::Utc::now().timestamp();
    let Ok(claims) = state.endpoint_signer().verify(&token, now) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Revocation / liveness check: the token must still be recorded live.
    if state
        .store()
        .mcp_endpoint_tokens()
        .get_live_by_token_hash(&catalerum_iam::token::hash_token(&token))
        .await
        .is_err()
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    serve_endpoint(
        &state,
        claims.workspace_id,
        &claims.endpoint,
        req,
        accepts_event_stream(&headers),
        None,
    )
    .await
}

/// Keep the endpoint capabilities a caller's authority **covers** (SOUL §19):
/// an endpoint cap survives only if some caller capability covers it. This is
/// the anti-escalation bound for `POST /mcp/e/{name}` — the endpoint acts with
/// at most the intersection of its own authority and the caller's.
fn intersect_caps(endpoint: &[Capability], caller: &[Capability]) -> Vec<Capability> {
    endpoint
        .iter()
        .filter(|ec| caller.iter().any(|cc| cc.covers(ec)))
        .cloned()
        .collect()
}

/// Shared serve path for both endpoint routes: load the endpoint, resolve its
/// authority, build its isolated [`McpServer`], and dispatch one JSON-RPC request.
///
/// `caller_caps` is the authenticated caller's effective authority for
/// `/mcp/e/{name}` (intersected with the endpoint's own caps, fail-closed on an
/// empty intersection); `None` for the unauthenticated `/mcp/s/{token}` path,
/// where the endpoint's own caps stand alone (the share token *is* the
/// authorization).
async fn serve_endpoint(
    state: &AppState,
    workspace_id: WorkspaceId,
    name: &str,
    req: JsonRpcRequest,
    wants_sse: bool,
    caller_caps: Option<Vec<Capability>>,
) -> Response {
    let endpoint = match state
        .store()
        .mcp_endpoints()
        .get_by_name(workspace_id, name)
        .await
    {
        Ok(e) if e.enabled => e,
        // Unknown or disabled — both 404 (no existence signal for a disabled one).
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let caps = endpoint_caps(state, workspace_id, &endpoint).await;
    let caps = match caller_caps {
        Some(caller) => {
            let bounded = intersect_caps(&caps, &caller);
            if bounded.is_empty() {
                // Fail closed: the caller's authority covers none of this
                // endpoint's capabilities, so serving would either be a no-op or
                // an escalation. Refuse up front (SOUL §19, deny-by-default).
                return (
                    StatusCode::FORBIDDEN,
                    "your authority covers none of this endpoint's capabilities",
                )
                    .into_response();
            }
            bounded
        }
        None => caps,
    };
    let server = match build_endpoint_server(
        &endpoint,
        state.registry().clone(),
        workspace_id,
        caps,
    )
    .await
    {
        Ok(server) => server,
        Err(e) => {
            tracing::warn!(endpoint = %name, error = %e, "failed to build MCP endpoint server");
            return (
                StatusCode::BAD_GATEWAY,
                format!("endpoint `{name}` script error: {e}"),
            )
                .into_response();
        }
    };
    into_response(server.handle(req).await, wants_sse)
}

/// Resolve the capabilities an endpoint's script runs under: its explicit grant's
/// capabilities when set (and resolvable), else the minimal read-only default
/// ([`default_endpoint_caps`]). The subdir scope is pinned separately by the host.
async fn endpoint_caps(
    state: &AppState,
    workspace_id: WorkspaceId,
    endpoint: &McpEndpoint,
) -> Vec<Capability> {
    match endpoint.grant_id {
        Some(gid) => match state.store().grants().get(workspace_id, gid).await {
            Ok(grant) => grant.capabilities,
            Err(_) => default_endpoint_caps(),
        },
        None => default_endpoint_caps(),
    }
}

/// `POST /mcp` — handle one JSON-RPC request against the workspace-scoped tool
/// registry. `initialize` / `tools/list` / `tools/call` / `ping` (+ prompts /
/// resources when providers are attached) are all served by the shared
/// [`McpServer`]. A notification (no `id`) returns `202` with no body.
async fn handle(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> Response {
    let p = auth.principal();
    // Scope the server to this bearer's **effective authority** (§19), enforced
    // per-call at registry dispatch: the role's base set for a normal token, or —
    // when the bearer is grant-scoped (SOUL §19/§26) — the grant's capabilities, so
    // an MCP client (Claude Code / Codex / opencode) is bounded by the grant rather
    // than the minting user's full role. `Auth` already resolved + fail-closed the
    // grant at extraction time.
    let ctx = ToolContext {
        workspace_id: Some(p.workspace_id),
        user_id: Some(p.user_id),
        capabilities: Some(auth.capabilities()),
        ..Default::default()
    };
    // Attach the same store-backed prompts (skills §23) + resources (notes/tasks
    // read views) the stdio server serves, scoped to this request's workspace —
    // so HTTP MCP is at full parity with `catalerum mcp` (tools + prompts +
    // resources), not tools-only.
    let prompts = Arc::new(SkillPromptProvider::new(
        state.store().clone(),
        p.workspace_id,
    ));
    let resources = Arc::new(WorkspaceResourceProvider::new(
        state.store().clone(),
        p.workspace_id,
    ));
    let server = McpServer::new(state.registry().clone(), ctx)
        .with_server_info("catalerum", env!("CARGO_PKG_VERSION"))
        .with_prompts(prompts)
        .with_resources(resources);
    let wants_sse = accepts_event_stream(&headers);
    // A request that opts into SSE *and* asks for progress (`_meta.progressToken`)
    // gets the streaming answer: progress notifications bracketing the final
    // response. Every other request keeps the plain request/response shape (a
    // single JSON body, or a single-event SSE stream when SSE was accepted).
    if is_progress_stream_request(&req, wants_sse) {
        let token = req.progress_token().cloned().unwrap_or(Value::Null);
        return event_stream_response(progress_stream(server, req, token));
    }
    into_response(server.handle(req).await, wants_sse)
}

/// `GET /mcp` — the server→client SSE stream for **unsolicited** messages (MCP
/// streamable-HTTP). Authenticated exactly like `POST /mcp` and additionally
/// gated on workspace-wide read authority ([`require_workspace_read`]); the
/// stream is scoped to the principal's workspace (the surface mints no session
/// id). A client that does not accept `text/event-stream` gets `406` — this
/// endpoint only speaks SSE.
async fn handle_get(auth: Auth, headers: HeaderMap) -> Response {
    if !accepts_event_stream(&headers) {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "GET /mcp requires `Accept: text/event-stream`",
        )
            .into_response();
    }
    if let Err(e) = require_workspace_read(&auth) {
        return e.into_response();
    }
    let workspace = auth.principal().workspace_id;
    // Server-pushed messages for this workspace, interleaved with keep-alive
    // comments so an idle stream survives intermediaries. The stream lives until
    // the client disconnects (axum drops the body). The hub is fed both locally
    // (this pod's producers) and, once the cross-pod bridge is installed, by pushes
    // relayed from peer pods over the bus (`crate::mcp_push_bridge`, SOUL §16 M7).
    let pushes = crate::mcp_push_bridge::hub()
        .subscribe_stream(workspace)
        .map(|json| sse_frame(&json));
    let keepalive = futures::stream::unfold((), |()| async {
        tokio::time::sleep(KEEPALIVE_INTERVAL).await;
        Some((": keep-alive\n\n".to_string(), ()))
    });
    event_stream_response(futures::stream::select(pushes, keepalive))
}

/// The `GET /mcp` capability gate (§19): the push hub fans out **workspace-wide**
/// and its messages carry no per-message capability tag, so holding the stream is
/// equivalent to reading any standard domain. Require `read` over every standard
/// domain: every role-derived token passes (a Viewer's base set is exactly these
/// reads), while a grant-scoped token narrower than workspace-wide read is
/// refused instead of receiving pushes its grant does not cover — the recorded
/// grant-bypass class, closed here before the hub gains its first producer.
fn require_workspace_read(auth: &Auth) -> ApiResult<()> {
    for domain in catalerum_iam::STANDARD_DOMAINS {
        auth.require(Action::Read, domain)?;
    }
    Ok(())
}

/// Whether a request should be answered as a **progress-streaming** SSE response:
/// the client accepted SSE, it is a real request (not a notification), and it
/// supplied a `progressToken` in `_meta` asking for progress updates.
fn is_progress_stream_request(req: &JsonRpcRequest, wants_sse: bool) -> bool {
    wants_sse && !req.is_notification() && req.progress_token().is_some()
}

/// Whether the client's `Accept` header opts into SSE. The standard MCP
/// "streamable HTTP" client sends `Accept: application/json, text/event-stream`
/// and parses the POST response as an SSE stream; a plain JSON client omits it.
/// Matches the `text/event-stream` media type only (ignoring any `;q=`/whitespace
/// params); a bare `*/*` does *not* opt in (JSON stays the safe default).
fn accepts_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| {
            accept
                .split(',')
                .filter_map(|m| m.split(';').next())
                .any(|m| m.trim().eq_ignore_ascii_case("text/event-stream"))
        })
}

/// Map the server's optional response into HTTP. A notification (`None` — no
/// `id`, so there is nothing to return) is always `202 Accepted` with an empty
/// body. A real response is either a single-event **SSE stream** (when the client
/// accepted `text/event-stream`, per the streamable-HTTP transport) or a plain
/// `200 application/json` body.
fn into_response(resp: Option<JsonRpcResponse>, wants_sse: bool) -> Response {
    match resp {
        None => StatusCode::ACCEPTED.into_response(),
        Some(r) if wants_sse => sse_response(r),
        Some(r) => Json(r).into_response(),
    }
}

/// Frame a single JSON-RPC response as an SSE stream carrying one event, then
/// close — the fast-path streamable-HTTP response for a request that accepted SSE
/// but asked for no progress.
fn sse_response(resp: JsonRpcResponse) -> Response {
    // Serializing a JSON-RPC response can't realistically fail; on the impossible
    // error, fall back to a plain JSON body rather than an empty stream.
    let Ok(payload) = serde_json::to_string(&resp) else {
        return Json(resp).into_response();
    };
    event_stream_response(futures::stream::once(async move { sse_frame(&payload) }))
}

/// The progress-streaming body for a `tools/call` (or any request) that supplied a
/// `progressToken`: a coarse `started` progress notification (emitted immediately,
/// while the call runs), then — once the call completes — a `completed` progress
/// notification and the final JSON-RPC response, after which the stream ends.
///
/// Tools do not report fine-grained progress yet (there is no progress channel on
/// [`ToolContext`]), so this is a genuine but coarse `started`/`completed` bracket
/// rather than invented per-step progress. When per-tool progress lands, additional
/// `notifications/progress` events slot into this same stream.
fn progress_stream(
    server: McpServer,
    req: JsonRpcRequest,
    token: Value,
) -> impl futures::Stream<Item = String> + Send + 'static {
    let started_token = token.clone();
    let started = futures::stream::once(async move {
        frame(&JsonRpcNotification::progress(
            started_token,
            0.0,
            Some(1.0),
            Some("started".to_string()),
        ))
    });
    // The tail runs the call, then emits `completed` + the response as one chunk
    // (SSE events are just concatenated `data:` blocks). The `started` event above
    // has already flushed to the client by the time this future is polled, so the
    // client sees progress while the call is still running.
    let tail = futures::stream::once(async move {
        let resp = server.handle(req).await;
        let completed =
            JsonRpcNotification::progress(token, 1.0, Some(1.0), Some("completed".to_string()));
        let mut out = frame(&completed);
        if let Some(resp) = resp {
            out.push_str(&frame(&resp));
        }
        out
    });
    started.chain(tail)
}

/// Wrap a stream of SSE frame strings as a `200 text/event-stream` response. The
/// frames are pre-serialized by [`sse_frame`]; this only attaches the transport
/// headers and body.
fn event_stream_response<S>(frames: S) -> Response
where
    S: futures::Stream<Item = String> + Send + 'static,
{
    let body = Body::from_stream(frames.map(Ok::<String, Infallible>));
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .header(CACHE_CONTROL, "no-cache")
        .body(body)
        .expect("static SSE response headers are always valid")
}

/// Serialize one JSON-RPC message and frame it as a single SSE event.
fn frame<T: Serialize>(msg: &T) -> String {
    sse_frame(&serde_json::to_string(msg).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn accept(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(axum::http::header::ACCEPT, value.parse().unwrap());
        h
    }

    #[test]
    fn notification_is_accepted_with_no_body() {
        // `None` (a JSON-RPC notification) → 202, never a JSON/SSE envelope,
        // regardless of the negotiated content type.
        assert_eq!(into_response(None, false).status(), StatusCode::ACCEPTED);
        assert_eq!(into_response(None, true).status(), StatusCode::ACCEPTED);
    }

    #[test]
    fn response_is_200_json() {
        let r = JsonRpcResponse::ok(json!(1), json!({ "ok": true }));
        let resp = into_response(Some(r), false);
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok()),
            Some("application/json")
        );
    }

    #[test]
    fn response_is_sse_when_accepted() {
        // A streamable-HTTP client (accepts text/event-stream) gets the response
        // framed as an SSE stream, not a JSON body.
        let r = JsonRpcResponse::ok(json!(1), json!({ "ok": true }));
        let resp = into_response(Some(r), true);
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok()),
            Some("text/event-stream")
        );
    }

    #[test]
    fn accept_header_opts_into_sse() {
        // The standard streamable-HTTP client accepts both; either token opts in.
        assert!(accepts_event_stream(&accept(
            "application/json, text/event-stream"
        )));
        assert!(accepts_event_stream(&accept("text/event-stream")));
        // Params (`;q=`) and casing are tolerated.
        assert!(accepts_event_stream(&accept("text/event-stream; q=0.9")));
        assert!(accepts_event_stream(&accept("TEXT/Event-Stream")));
        // JSON-only, wildcard, and a missing header all stay on the JSON path.
        assert!(!accepts_event_stream(&accept("application/json")));
        assert!(!accepts_event_stream(&accept("*/*")));
        assert!(!accepts_event_stream(&HeaderMap::new()));
    }

    #[test]
    fn parses_a_jsonrpc_request_body() {
        // The wire shape MCP clients POST deserializes into a request the server
        // can handle (method + id round-trip).
        let req: JsonRpcRequest =
            serde_json::from_value(json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/list" }))
                .unwrap();
        assert_eq!(req.method, "tools/list");
        assert_eq!(req.id, Some(Value::from(7)));
        assert!(!req.is_notification());
    }

    fn request(value: Value) -> JsonRpcRequest {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn progress_stream_selected_only_with_sse_and_a_token() {
        let with_token = request(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "slow", "_meta": { "progressToken": "t" } },
        }));
        // SSE + a progress token → stream progress.
        assert!(is_progress_stream_request(&with_token, true));
        // No SSE accepted → plain JSON, even with a token.
        assert!(!is_progress_stream_request(&with_token, false));
        // SSE but no token → the single-event fast path, not a progress stream.
        let no_token = request(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "slow" },
        }));
        assert!(!is_progress_stream_request(&no_token, true));
        // A notification never gets a progress stream (it gets 202).
        let notif = request(json!({
            "jsonrpc": "2.0", "method": "tools/call",
            "params": { "name": "slow", "_meta": { "progressToken": "t" } },
        }));
        assert!(!is_progress_stream_request(&notif, true));
    }

    #[tokio::test]
    async fn progress_stream_brackets_the_response_with_started_and_completed() {
        use catalerum_core::tool::ToolRegistry;

        // An empty registry is enough: `ping` needs no tool, and the wrapping is
        // method-agnostic.
        let server = McpServer::new(ToolRegistry::new(), ToolContext::default());
        let req = request(json!({
            "jsonrpc": "2.0", "id": 1, "method": "ping",
            "params": { "_meta": { "progressToken": "tok" } },
        }));
        let token = req.progress_token().cloned().unwrap();
        let frames: Vec<String> = progress_stream(server, req, token).collect().await;
        let joined = frames.concat();

        // Every emitted event is SSE-framed.
        assert!(joined.starts_with("data: "), "{joined}");
        // Ordering: started notification → completed notification → final response.
        let started = joined
            .find(r#""message":"started""#)
            .expect("started event");
        let completed = joined
            .find(r#""message":"completed""#)
            .expect("completed event");
        let response = joined.find(r#""result""#).expect("final response");
        assert!(started < completed && completed < response, "{joined}");
        // The client's progress token round-trips into the progress notifications.
        assert!(joined.contains(r#""progressToken":"tok""#), "{joined}");
        assert!(joined.contains("notifications/progress"), "{joined}");
    }

    #[test]
    fn event_stream_response_is_200_text_event_stream() {
        let resp = event_stream_response(futures::stream::once(async { sse_frame("{}") }));
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok()),
            Some("text/event-stream")
        );
    }

    #[test]
    fn get_stream_gate_passes_every_role_derived_token() {
        use catalerum_core::model::Role;
        use catalerum_core::{UserId, WorkspaceId};
        // Every role's base set holds read on all standard domains (a Viewer's
        // base set is exactly these reads), so role-derived tokens keep the
        // stream — the gate cuts only narrower-than-workspace grants.
        for role in [Role::Owner, Role::Admin, Role::Member, Role::Viewer] {
            let auth = Auth::from_principal(catalerum_iam::Principal::new(
                UserId::new(),
                WorkspaceId::new(),
                role,
            ));
            assert!(
                require_workspace_read(&auth).is_ok(),
                "{role:?} holds workspace-wide read"
            );
        }
    }

    #[test]
    fn get_stream_gate_refuses_a_narrow_grant_scoped_token() {
        use catalerum_core::capability::Resource;
        use catalerum_core::model::{Grant, Role};
        use catalerum_core::{GrantId, UserId, WorkspaceId};
        // A token minted `{notes:read}` must not receive workspace-wide pushes,
        // whatever the minting user's role — the §19 grant-bypass class.
        let ws = WorkspaceId::new();
        let grant = Grant {
            id: GrantId::nil(),
            workspace_id: ws,
            name: "notes-only".to_string(),
            capabilities: vec![Capability::new(Action::Read, Resource::domain("notes"))],
            constraints: Default::default(),
        };
        let auth = Auth::with_grant(
            catalerum_iam::Principal::new(UserId::new(), ws, Role::Owner),
            grant,
        );
        assert!(matches!(
            require_workspace_read(&auth),
            Err(crate::error::ApiError::Forbidden(_))
        ));
    }

    #[test]
    fn get_stream_gate_passes_a_workspace_wide_read_grant() {
        use catalerum_core::model::Role;
        use catalerum_core::{UserId, WorkspaceId};
        // A viewer-shaped grant (read on every standard domain) is exactly the
        // authority the stream requires, so it may hold it.
        let ws = WorkspaceId::new();
        let auth = Auth::with_grant(
            catalerum_iam::Principal::new(UserId::new(), ws, Role::Member),
            catalerum_iam::role_grant(ws, Role::Viewer),
        );
        assert!(require_workspace_read(&auth).is_ok());
    }

    #[test]
    fn intersect_caps_keeps_only_what_the_caller_covers() {
        use catalerum_core::capability::Resource;
        use catalerum_core::model::Role;

        let read = Capability::new(Action::Read, Resource::domain("storage"));
        let write = Capability::new(Action::Write, Resource::domain("storage"));
        let endpoint_caps = vec![read.clone(), write.clone()];

        // An Owner (`*`) covers everything — the endpoint keeps its full set.
        let owner = catalerum_iam::base_capabilities(Role::Owner);
        assert_eq!(intersect_caps(&endpoint_caps, &owner), endpoint_caps);

        // A Viewer holds `storage:read` but not write: the endpoint pinned to a
        // write grant is clipped to read-only when a Viewer invokes it — the
        // anti-escalation bound of `POST /mcp/e/{name}` (SOUL §19).
        let viewer = catalerum_iam::base_capabilities(Role::Viewer);
        assert_eq!(intersect_caps(&endpoint_caps, &viewer), vec![read]);

        // A grant-scoped caller with authority over a *different* domain covers
        // nothing here → empty intersection (the route refuses with 403).
        let notes_only = vec![Capability::new(Action::Read, Resource::domain("notes"))];
        assert!(intersect_caps(&endpoint_caps, &notes_only).is_empty());

        // An empty caller authority clips everything.
        assert!(intersect_caps(&endpoint_caps, &[]).is_empty());
        // An empty endpoint set stays empty regardless of caller.
        assert!(intersect_caps(&[], &owner).is_empty());
    }
}
