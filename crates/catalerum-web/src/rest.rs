//! REST transport for the workbench's non-streaming API calls (SOUL §12).
//!
//! The chat panel talks over the WebSocket ([`crate::ws`]); the Calendar view
//! talks over plain REST. This module wraps `gloo_net`'s fetch client for the
//! calendar surface (`/connections`, `/calendars`, `/events`) — attaching the
//! dev session token as a bearer credential, decoding the JSON contract types
//! from [`crate::api`], and rendering errors as a single [`RestError`].
//!
//! Bodies are (de)serialized with `serde_json` directly rather than via
//! `gloo_net`'s `json` feature, so the wasm bundle keeps its current feature set
//! (`websocket`, `http`).

use futures::FutureExt;
use gloo_net::http::Request;

use crate::api::{
    ActiveTurn, AddColumn, AddOrgMember, AgentProfile, Automation, AutomationRun, BackendObject,
    Board, Calendar, CollectNowResult, ComputerAgentView, Connection, Conversation,
    ConversationQuestion, CreateAgentProfile, CreateAutomation, CreateBoard, CreateCalendar,
    CreateConnection, CreateConversation, CreateEmailConnection, CreateEvent, CreateGrant,
    CreateMemory, CreateNote, CreateOrg, CreateOrgWorkspace, CreateSkill, CreateTask, CreateToken,
    CreatedToken, CreatedWorkspace, EditTask, EmailDetail, EmailView, EnrollComputerAgent,
    EnrolledComputerAgent, Event, ExchangeCode, ExchangeSession, FetchRequest, FetchedPage,
    FileLabel, FireResult, Grant, GraphQueryRequest, GraphQueryResponse, LlmSettings, Mailbox,
    McpEndpoint, McpEndpointBody, McpServerBody, McpServerView, Memory, Message, MessageHit,
    MintEndpointToken, MintedEndpointToken, ModelInfo, MoveTask, MyOrganisation, NewFileLabel,
    NewStorageStore, NodeTypeHit, Note, ObjectHit, ObjectText, OnboardingState, OrgMemberView,
    Organisation, PendingApproval, PendingQuestion, PersonalizeRequest, PersonalizeResponse,
    Profile, RenameBoard, RenameColumn, RenameConversation, RunDetail, ScanReport,
    SearchProviderInfo, SearchSettings, SetConversationModel, SetConversationProfile,
    SetConversationReasoning, SetEnabled, SetOrgPolicy, SetTaskStatus, Skill, StatusInfo,
    StorageObject, StorageSettings, StorageStore, SwitchResponse, SwitchWorkspace, SyncEnqueued,
    Task, TerminalSession, TokenView, ToolInfo, UpdateAgentProfile, UpdateAutomation, UpdateEvent,
    UpdateMemory, UpdateNote, UpdateSkill, UserLookup, VoiceInfo, WorkspaceMembership,
    WorkspaceShell, AGENT_PROFILES_PATH, AUDIO_TRANSCRIBE_PATH, AUTH_EXCHANGE_PATH,
    AUTH_PASSWORD_PATH, AUTH_SETUP_PATH, AUTH_SWITCH_PATH, AUTOMATIONS_PATH,
    AUTOMATION_NODE_TYPES_PATH, BOARDS_PATH, CALENDARS_PATH, COLUMNS_PATH, COMPUTER_AGENTS_PATH,
    CONNECTIONS_PATH, CONVERSATIONS_PATH, DB_CONNECTIONS_PATH, EMAILS_PATH, EMAIL_CONNECTIONS_PATH,
    EVENTS_PATH, FETCH_PATH, GRANTS_PATH, GRAPH_QUERY_PATH, LLMLEAF_PATH, LLM_MODELS_PATH,
    LLM_SETTINGS_PATH, LLM_VOICES_PATH, MAILBOXES_PATH, MCP_ENDPOINTS_PATH, MCP_SERVERS_PATH,
    MEMORIES_PATH, NOTES_PATH, ONBOARDING_COMPLETE_PATH, ONBOARDING_PERSONALIZE_PATH,
    ONBOARDING_STATE_PATH, ORGANISATIONS_PATH, PROFILE_PATH, SEARCH_PROVIDERS_PATH,
    SEARCH_SETTINGS_PATH, SKILLS_PATH, STATUS_PATH, STORAGE_CATALOGUE_PATH, STORAGE_LABELS_PATH,
    STORAGE_OBJECTS_PATH, STORAGE_SETTINGS_PATH, STORAGE_STORES_PATH, TASKS_PATH,
    TERMINAL_SESSIONS_PATH, TOKENS_PATH, TOOLS_PATH, TRIGGERS_PATH, UIS_PATH, USERS_PATH,
    WORKSPACES_PATH,
};
use crate::components::emerged::model::{EventName, UiAction, UiDefinition};

/// An error from a REST call: a transport failure, a non-2xx status (carrying
/// the server's `{"error": …}` message when present), or a JSON decode failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestError {
    /// The fetch could not be performed (network / CORS / offline).
    Transport(String),
    /// The server returned a non-success status; carries status + message.
    Status { status: u16, message: String },
    /// The response body could not be decoded into the expected type.
    Decode(String),
}

impl std::fmt::Display for RestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RestError::Transport(e) => write!(f, "network error: {e}"),
            RestError::Status { status, message } => {
                if message.is_empty() {
                    write!(f, "request failed (HTTP {status})")
                } else {
                    write!(f, "{message} (HTTP {status})")
                }
            }
            RestError::Decode(e) => write!(f, "could not parse server response: {e}"),
        }
    }
}

impl std::error::Error for RestError {}

/// Build an absolute API URL from a root-mounted path (`/events`, …). The origin
/// is resolved at runtime (`api.<current-host>` in the browser, [`API_BASE`] in
/// dev/tests) — see [`crate::api::api_base`].
fn url(path: &str) -> String {
    format!("{}{path}", crate::api::api_base().trim_end_matches('/'))
}

/// Pull the server-rendered error message out of a non-2xx body. The API renders
/// errors as `{"error": "...", "kind": "..."}`; fall back to the raw body.
fn error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().to_string())
}

/// Build the [`RestError::Status`] for a non-2xx response — the one place every
/// verb helper funnels through. A `401` on a request that **carried** a bearer
/// means the session is dead server-side ("unknown session token" / "session
/// expired"): the cached credential is dropped and the page bounces to the login
/// surface ([`crate::auth::redirect_to_login`]) instead of leaving every panel
/// stuck on the same dead error. Token-less calls (the anonymous login probe)
/// never bounce. The error is still returned so the in-flight caller renders
/// something sensible during the reload.
fn status_error(status: u16, body: &str, token: Option<&str>) -> RestError {
    if crate::auth::is_session_expired(status, token) {
        crate::auth::redirect_to_login();
    }
    RestError::Status {
        status,
        message: error_message(body),
    }
}

/// Issue a GET, attaching `token` as a bearer credential, and return the raw
/// response body of a 2xx as text — for callers that hand the server's JSON on
/// verbatim (the chat debug export) instead of decoding it into a contract type.
async fn get_body(full_url: &str, token: Option<&str>) -> Result<String, RestError> {
    let mut req = Request::get(full_url);
    if let Some(tok) = token {
        if !tok.is_empty() {
            req = req.header("Authorization", &format!("Bearer {tok}"));
        }
    }
    let resp = req
        .send()
        .await
        .map_err(|e| RestError::Transport(e.to_string()))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| RestError::Transport(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(status_error(status, &body, token));
    }
    Ok(body)
}

/// Issue a GET, attaching `token` as a bearer credential, and decode the JSON
/// body into `T`.
async fn get_json<T: serde::de::DeserializeOwned>(
    full_url: &str,
    token: Option<&str>,
) -> Result<T, RestError> {
    let body = get_body(full_url, token).await?;
    serde_json::from_str::<T>(&body).map_err(|e| RestError::Decode(e.to_string()))
}

/// Issue a POST with a JSON `body` (or an empty body when `None`), attaching
/// `token` as a bearer credential, and decode the JSON response into `T`.
async fn post_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(
    full_url: &str,
    token: Option<&str>,
    body: Option<&B>,
) -> Result<T, RestError> {
    let mut req = Request::post(full_url).header("Content-Type", "application/json");
    if let Some(tok) = token {
        if !tok.is_empty() {
            req = req.header("Authorization", &format!("Bearer {tok}"));
        }
    }
    // Always send a JSON body so `Content-Type` matches; `null` for no payload.
    let payload = match body {
        Some(b) => serde_json::to_string(b).map_err(|e| RestError::Decode(e.to_string()))?,
        None => "null".to_string(),
    };
    let req = req
        .body(payload)
        .map_err(|e| RestError::Transport(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|e| RestError::Transport(e.to_string()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| RestError::Transport(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(status_error(status, &text, token));
    }
    serde_json::from_str::<T>(&text).map_err(|e| RestError::Decode(e.to_string()))
}

async fn post_json_empty<B: serde::Serialize>(
    full_url: &str,
    token: Option<&str>,
    body: &B,
) -> Result<(), RestError> {
    let mut req = Request::post(full_url).header("Content-Type", "application/json");
    if let Some(tok) = token.filter(|tok| !tok.is_empty()) {
        req = req.header("Authorization", &format!("Bearer {tok}"));
    }
    let payload = serde_json::to_string(body).map_err(|e| RestError::Decode(e.to_string()))?;
    let response = req
        .body(payload)
        .map_err(|e| RestError::Transport(e.to_string()))?
        .send()
        .await
        .map_err(|e| RestError::Transport(e.to_string()))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| RestError::Transport(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(status_error(status, &text, token));
    }
    Ok(())
}

/// Issue a PUT with a JSON `body`, attaching `token` as a bearer credential, and
/// decode the JSON response into `T`.
async fn put_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(
    full_url: &str,
    token: Option<&str>,
    body: &B,
) -> Result<T, RestError> {
    let mut req = Request::put(full_url).header("Content-Type", "application/json");
    if let Some(tok) = token {
        if !tok.is_empty() {
            req = req.header("Authorization", &format!("Bearer {tok}"));
        }
    }
    let payload = serde_json::to_string(body).map_err(|e| RestError::Decode(e.to_string()))?;
    let req = req
        .body(payload)
        .map_err(|e| RestError::Transport(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|e| RestError::Transport(e.to_string()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| RestError::Transport(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(status_error(status, &text, token));
    }
    serde_json::from_str::<T>(&text).map_err(|e| RestError::Decode(e.to_string()))
}

/// Issue a PATCH with a JSON `body`, attaching `token` as a bearer credential,
/// and decode the JSON response into `T`.
async fn patch_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(
    full_url: &str,
    token: Option<&str>,
    body: &B,
) -> Result<T, RestError> {
    let mut req = Request::patch(full_url).header("Content-Type", "application/json");
    if let Some(tok) = token {
        if !tok.is_empty() {
            req = req.header("Authorization", &format!("Bearer {tok}"));
        }
    }
    let payload = serde_json::to_string(body).map_err(|e| RestError::Decode(e.to_string()))?;
    let req = req
        .body(payload)
        .map_err(|e| RestError::Transport(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|e| RestError::Transport(e.to_string()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| RestError::Transport(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(status_error(status, &text, token));
    }
    serde_json::from_str::<T>(&text).map_err(|e| RestError::Decode(e.to_string()))
}

/// Issue a PUT with a raw **binary** body (a `Uint8Array`), attaching `token` as
/// a bearer credential. Used for object uploads, where the body is file bytes,
/// not JSON. Treats any 2xx as success (the API returns `201` with an
/// `UploadResult` JSON the caller doesn't need); surfaces the server's
/// `{"error": …}` on a non-2xx.
async fn put_bytes(
    full_url: &str,
    token: Option<&str>,
    content_type: Option<&str>,
    bytes: &[u8],
) -> Result<(), RestError> {
    // Copy the bytes into a JS typed array — gloo-net's `body` takes any
    // `Into<JsValue>`, and a `Uint8Array` is a valid `fetch` BodyInit.
    let array = js_sys::Uint8Array::new_with_length(bytes.len() as u32);
    array.copy_from(bytes);

    let mut req = Request::put(full_url);
    if let Some(ct) = content_type {
        if !ct.is_empty() {
            req = req.header("Content-Type", ct);
        }
    }
    if let Some(tok) = token {
        if !tok.is_empty() {
            req = req.header("Authorization", &format!("Bearer {tok}"));
        }
    }
    let req = req
        .body(array)
        .map_err(|e| RestError::Transport(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|e| RestError::Transport(e.to_string()))?;
    let status = resp.status();
    if (200..300).contains(&status) {
        return Ok(());
    }
    let body = resp
        .text()
        .await
        .map_err(|e| RestError::Transport(e.to_string()))?;
    Err(status_error(status, &body, token))
}

/// Issue a POST with a raw **binary** body (a `Uint8Array`) and decode the JSON
/// response into `T`. The binary sibling of [`post_json`] — used where the request
/// body is media bytes, not JSON (the mic recorder's audio blob → a transcript).
async fn post_bytes_json<T: serde::de::DeserializeOwned>(
    full_url: &str,
    token: Option<&str>,
    content_type: Option<&str>,
    request_id: Option<&str>,
    bytes: &[u8],
) -> Result<T, RestError> {
    let array = js_sys::Uint8Array::new_with_length(bytes.len() as u32);
    array.copy_from(bytes);

    let mut req = Request::post(full_url);
    if let Some(ct) = content_type {
        if !ct.is_empty() {
            req = req.header("Content-Type", ct);
        }
    }
    if let Some(tok) = token {
        if !tok.is_empty() {
            req = req.header("Authorization", &format!("Bearer {tok}"));
        }
    }
    if let Some(id) = request_id {
        req = req.header("X-Catalerum-Transcription-Id", id);
    }
    let req = req
        .body(array)
        .map_err(|e| RestError::Transport(e.to_string()))?;
    let send = req.send();
    futures::pin_mut!(send);
    let mut deadline = Box::pin(rest_sleep_ms(20_000).fuse());
    let resp = futures::select! {
        response = send.fuse() => response.map_err(|e| RestError::Transport(e.to_string()))?,
        () = deadline => return Err(RestError::Transport("request timed out".to_string())),
    };
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| RestError::Transport(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(status_error(status, &text, token));
    }
    serde_json::from_str::<T>(&text).map_err(|e| RestError::Decode(e.to_string()))
}

async fn rest_sleep_ms(ms: u32) {
    let (tx, rx) = futures::channel::oneshot::channel::<()>();
    gloo_timers::callback::Timeout::new(ms, move || {
        let _ = tx.send(());
    })
    .forget();
    let _ = rx.await;
}

/// Issue a DELETE, attaching `token` as a bearer credential. Treats any 2xx
/// (including the `204 No Content` the API returns) as success; on a non-2xx it
/// surfaces the server's `{"error": …}` message.
async fn delete_resource(full_url: &str, token: Option<&str>) -> Result<(), RestError> {
    let mut req = Request::delete(full_url);
    if let Some(tok) = token {
        if !tok.is_empty() {
            req = req.header("Authorization", &format!("Bearer {tok}"));
        }
    }
    let resp = req
        .send()
        .await
        .map_err(|e| RestError::Transport(e.to_string()))?;
    let status = resp.status();
    if (200..300).contains(&status) {
        return Ok(());
    }
    let body = resp
        .text()
        .await
        .map_err(|e| RestError::Transport(e.to_string()))?;
    Err(status_error(status, &body, token))
}

/// `GET /events` — all of this workspace's events (newest start ordering is the
/// API's; the view groups them by day). Optional bounds are added by the caller
/// via [`list_events_filtered`] when needed.
pub async fn list_events(token: Option<&str>) -> Result<Vec<Event>, RestError> {
    get_json(&url(EVENTS_PATH), token).await
}

/// `GET /events?from=…&to=…` — events whose `start` falls in the window, with
/// either bound optional (`from` inclusive, `to` exclusive; RFC 3339 strings).
/// Both `None` is equivalent to [`list_events`].
pub async fn list_events_filtered(
    token: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<Vec<Event>, RestError> {
    let mut params: Vec<String> = Vec::new();
    if let Some(f) = from {
        params.push(format!("from={}", encode_query_component(f)));
    }
    if let Some(t) = to {
        params.push(format!("to={}", encode_query_component(t)));
    }
    let full = if params.is_empty() {
        url(EVENTS_PATH)
    } else {
        format!("{}?{}", url(EVENTS_PATH), params.join("&"))
    };
    get_json(&full, token).await
}

/// `GET /calendars` — this workspace's calendars (for name/colour lookup).
pub async fn list_calendars(token: Option<&str>) -> Result<Vec<Calendar>, RestError> {
    get_json(&url(CALENDARS_PATH), token).await
}

/// `POST /calendars` — create a local (database-native) calendar. Returns the
/// created [`Calendar`].
pub async fn create_calendar(
    token: Option<&str>,
    body: &CreateCalendar,
) -> Result<Calendar, RestError> {
    post_json(&url(CALENDARS_PATH), token, Some(body)).await
}

/// `DELETE /calendars/{id}` — delete a local calendar (and its events).
pub async fn delete_calendar(token: Option<&str>, id: &str) -> Result<(), RestError> {
    let path = format!("{CALENDARS_PATH}/{}", encode_path_segment(id));
    delete_resource(&url(&path), token).await
}

/// `POST /events` — create an event on a local calendar. Returns the stored
/// [`Event`].
pub async fn create_event(token: Option<&str>, body: &CreateEvent) -> Result<Event, RestError> {
    post_json(&url(EVENTS_PATH), token, Some(body)).await
}

/// `PUT /events/{id}` — replace an event's editable fields. Returns the updated
/// [`Event`].
pub async fn update_event(
    token: Option<&str>,
    id: &str,
    body: &UpdateEvent,
) -> Result<Event, RestError> {
    let path = format!("{EVENTS_PATH}/{}", encode_path_segment(id));
    put_json(&url(&path), token, body).await
}

/// `DELETE /events/{id}` — delete an event from a local calendar.
pub async fn delete_event(token: Option<&str>, id: &str) -> Result<(), RestError> {
    let path = format!("{EVENTS_PATH}/{}", encode_path_segment(id));
    delete_resource(&url(&path), token).await
}

/// `GET /connections` — this workspace's connections (newest first).
pub async fn list_connections(token: Option<&str>) -> Result<Vec<Connection>, RestError> {
    get_json(&url(CONNECTIONS_PATH), token).await
}

/// `POST /connections` — create a calendar connection. Returns the created
/// [`Connection`].
pub async fn create_connection(
    token: Option<&str>,
    body: &CreateConnection,
) -> Result<Connection, RestError> {
    post_json(&url(CONNECTIONS_PATH), token, Some(body)).await
}

/// `POST /connections/{id}/sync` — enqueue an incremental sync for a connection.
pub async fn sync_connection(token: Option<&str>, id: &str) -> Result<SyncEnqueued, RestError> {
    let path = format!("{CONNECTIONS_PATH}/{}/sync", encode_path_segment(id));
    post_json::<(), _>(&url(&path), token, None).await
}

/// `DELETE /connections/{id}` — remove a calendar connection (+ its synced data).
pub async fn delete_connection(token: Option<&str>, id: &str) -> Result<(), RestError> {
    let path = format!("{CONNECTIONS_PATH}/{}", encode_path_segment(id));
    delete_resource(&url(&path), token).await
}

// --- Emerged UIs (AI-authored declarative UIs) -----------------------------

/// `GET /uis/{id}` — one emerged UI's full definition (the inline renderer's
/// mount fetch).
pub async fn get_ui(token: Option<&str>, id: &str) -> Result<UiDefinition, RestError> {
    let path = format!("{UIS_PATH}/{}", encode_path_segment(id));
    get_json(&url(&path), token).await
}

/// `GET /uis` — this workspace's emerged UIs (for the future Apps panel).
pub async fn list_uis(token: Option<&str>) -> Result<Vec<UiDefinition>, RestError> {
    get_json(&url(UIS_PATH), token).await
}

/// `DELETE /uis/{id}` — remove an emerged UI (the Apps panel's row delete).
pub async fn delete_ui(token: Option<&str>, id: &str) -> Result<(), RestError> {
    let path = format!("{UIS_PATH}/{}", encode_path_segment(id));
    delete_resource(&url(&path), token).await
}

/// `GET /uis/by-name/{name}` — one emerged UI by its `present_ui` name slug
/// (the mount path for an `app_ref` targeting a name rather than a ui id).
pub async fn get_ui_by_name(token: Option<&str>, name: &str) -> Result<UiDefinition, RestError> {
    let path = format!("{UIS_PATH}/by-name/{}", encode_path_segment(name));
    get_json(&url(&path), token).await
}

/// The snake_case wire token for an [`EventName`] (the server's closed set; the
/// model's catch-all `Unknown` never fires a server handler).
fn event_token(event: EventName) -> &'static str {
    match event {
        EventName::Click => "click",
        EventName::Submit => "submit",
        EventName::Change => "change",
        EventName::Input => "input",
        EventName::Select => "select",
        EventName::Open => "open",
        EventName::Close => "close",
        EventName::Load => "load",
        EventName::Complete => "complete",
        EventName::Unknown => "unknown",
    }
}

#[derive(serde::Serialize)]
struct UiEventBody<'a> {
    node_id: &'a str,
    event: &'a str,
    state: &'a serde_json::Value,
    scope: &'a serde_json::Value,
}

#[derive(serde::Serialize)]
struct UiStateBody<'a> {
    state: &'a serde_json::Value,
}

/// `POST /uis/{id}/compute` — derive the UI's `computed.*` values from `state`
/// (called on mount; later refreshes piggyback on `/event` responses).
pub async fn post_ui_compute(
    token: Option<&str>,
    ui_id: &str,
    state: &serde_json::Value,
) -> Result<serde_json::Value, RestError> {
    let path = format!("{UIS_PATH}/{}/compute", encode_path_segment(ui_id));
    post_json(&url(&path), token, Some(&UiStateBody { state })).await
}

#[derive(serde::Serialize)]
struct UiValidateBody<'a> {
    handler: &'a str,
    value: &'a serde_json::Value,
    state: &'a serde_json::Value,
}

/// `POST /uis/{id}/validate` — run a `ValidationKind::Script` rule, returning
/// `{ ok, message? }`.
pub async fn post_ui_validate(
    token: Option<&str>,
    ui_id: &str,
    handler: &str,
    value: &serde_json::Value,
    state: &serde_json::Value,
) -> Result<serde_json::Value, RestError> {
    let path = format!("{UIS_PATH}/{}/validate", encode_path_segment(ui_id));
    let body = UiValidateBody {
        handler,
        value,
        state,
    };
    post_json(&url(&path), token, Some(&body)).await
}

/// The authed `<img src>` for an emerged UI's external-DB image node
/// (`GET /uis/{id}/image/{node_id}?p=…&token=…`). `params` are the node's
/// client-resolved bind values (the SQL itself lives in the server-held spec);
/// the token rides as a query parameter because a browser cannot attach an
/// `Authorization` header to an `<img>` fetch (same pattern as
/// [`download_url`]).
#[must_use]
pub fn ui_image_url(
    token: Option<&str>,
    ui_id: &str,
    node_id: &str,
    params: &[serde_json::Value],
) -> String {
    let path = format!(
        "{UIS_PATH}/{}/image/{}",
        encode_path_segment(ui_id),
        encode_path_segment(node_id)
    );
    let mut out = url(&path);
    if !params.is_empty() {
        let p = serde_json::to_string(params).unwrap_or_else(|_| "[]".to_string());
        out = append_query(&out, "p", &p);
    }
    if let Some(t) = token {
        if !t.is_empty() {
            out = append_query(&out, "token", t);
        }
    }
    out
}

/// `POST /uis/{id}/event` — fire a node's tool/script handler and return the
/// [`UiAction`]s to apply. Carries the firing node + event + the full client
/// state snapshot + the resolved `for_each` scope.
pub async fn post_ui_event(
    token: Option<&str>,
    ui_id: &str,
    node_id: &str,
    event: EventName,
    state: &serde_json::Value,
    scope: &serde_json::Value,
) -> Result<Vec<UiAction>, RestError> {
    let path = format!("{UIS_PATH}/{}/event", encode_path_segment(ui_id));
    let body = UiEventBody {
        node_id,
        event: event_token(event),
        state,
        scope,
    };
    post_json(&url(&path), token, Some(&body)).await
}

// --- Notes (SOUL §21, M3) --------------------------------------------------

/// `GET /notes` — this workspace's notes, most-recently-edited first.
pub async fn list_notes(token: Option<&str>) -> Result<Vec<Note>, RestError> {
    get_json(&url(NOTES_PATH), token).await
}

/// `POST /notes` — create a note. Returns the created [`Note`].
pub async fn create_note(token: Option<&str>, body: &CreateNote) -> Result<Note, RestError> {
    post_json(&url(NOTES_PATH), token, Some(body)).await
}

/// `PUT /notes/{id}` — update a note's title / markdown / tags. Returns the
/// updated [`Note`].
pub async fn update_note(
    token: Option<&str>,
    id: &str,
    body: &UpdateNote,
) -> Result<Note, RestError> {
    let path = format!("{NOTES_PATH}/{}", encode_path_segment(id));
    put_json(&url(&path), token, body).await
}

/// `DELETE /notes/{id}` — delete a note.
pub async fn delete_note(token: Option<&str>, id: &str) -> Result<(), RestError> {
    let path = format!("{NOTES_PATH}/{}", encode_path_segment(id));
    delete_resource(&url(&path), token).await
}

// --- Terminal sessions (SOUL §20) -------------------------------------------

/// `GET /terminals/sessions` — list the workspace's active terminal sessions.
pub async fn list_terminal_sessions(
    token: Option<&str>,
) -> Result<Vec<TerminalSession>, RestError> {
    get_json(&url(TERMINAL_SESSIONS_PATH), token).await
}

// --- Workspaces (SOUL §18 — switcher) ---------------------------------------

/// `GET /workspaces` — the workspaces the authenticated user is a member of.
pub async fn list_workspaces(token: Option<&str>) -> Result<Vec<WorkspaceMembership>, RestError> {
    get_json(&url(WORKSPACES_PATH), token).await
}

/// `POST /auth/switch` — mint a new session bound to `workspace_id` (must be one
/// the caller belongs to). Returns the new bearer the client adopts.
pub async fn switch_workspace(
    token: Option<&str>,
    workspace_id: &str,
) -> Result<SwitchResponse, RestError> {
    post_json(
        &url(AUTH_SWITCH_PATH),
        token,
        Some(&SwitchWorkspace {
            workspace_id: workspace_id.to_string(),
        }),
    )
    .await
}

/// `POST /auth/exchange` — redeem a one-time handoff code (the `?code=` the
/// magic-link / SSO browser login redirected here with) for the real session
/// bearer. Carries no bearer of its own — the code *is* the credential.
pub async fn exchange_handoff_code(code: &str) -> Result<ExchangeSession, RestError> {
    post_json(
        &url(AUTH_EXCHANGE_PATH),
        None,
        Some(&ExchangeCode {
            code: code.to_string(),
        }),
    )
    .await
}

// --- Organisations (SOUL §18 — the grouping above workspaces) ---------------

/// `GET /organisations` — the caller's organisations, each with the workspaces in
/// it they can see. The switcher groups by these; callers treat a `403`/error as
/// "no org info" and fall back to a flat, ungrouped listing.
pub async fn list_organisations(token: Option<&str>) -> Result<Vec<MyOrganisation>, RestError> {
    get_json(&url(ORGANISATIONS_PATH), token).await
}

/// `POST /organisations` — create an organisation (instance policy gated; the
/// creator becomes its Owner). A `403` surfaces as the server's not-permitted
/// message — the UI never predicts the policy.
pub async fn create_organisation(
    token: Option<&str>,
    body: &CreateOrg,
) -> Result<Organisation, RestError> {
    post_json(&url(ORGANISATIONS_PATH), token, Some(body)).await
}

/// `DELETE /organisations/{id}` — delete an organisation (org **Owner** only;
/// empty, non-default orgs only). A `409` (`RestError::Status` with status 409)
/// carries the server's verbatim reason — the default org, or an org that still
/// holds a workspace (archived ones count too). The caller surfaces that message.
pub async fn delete_organisation(token: Option<&str>, org_id: &str) -> Result<(), RestError> {
    let path = format!("{ORGANISATIONS_PATH}/{}", encode_path_segment(org_id));
    delete_resource(&url(&path), token).await
}

/// `POST /organisations/{id}/workspaces` — create a workspace in an org (org
/// policy gated; the creator becomes its workspace Owner). Returns the new
/// workspace so the caller can switch into it.
pub async fn create_org_workspace(
    token: Option<&str>,
    org_id: &str,
    body: &CreateOrgWorkspace,
) -> Result<CreatedWorkspace, RestError> {
    let path = format!(
        "{ORGANISATIONS_PATH}/{}/workspaces",
        encode_path_segment(org_id)
    );
    post_json(&url(&path), token, Some(body)).await
}

/// `GET /organisations/{id}/members` — list an org's members (org admin/owner
/// only). A `403` is the fail-closed signal the caller is not an org admin.
pub async fn list_org_members(
    token: Option<&str>,
    org_id: &str,
) -> Result<Vec<OrgMemberView>, RestError> {
    let path = format!(
        "{ORGANISATIONS_PATH}/{}/members",
        encode_path_segment(org_id)
    );
    get_json(&url(&path), token).await
}

/// `POST /organisations/{id}/members` — add / re-role an org member by user id
/// (org admin/owner; only an Owner may touch Owner).
pub async fn add_org_member(
    token: Option<&str>,
    org_id: &str,
    body: &AddOrgMember,
) -> Result<OrgMemberView, RestError> {
    let path = format!(
        "{ORGANISATIONS_PATH}/{}/members",
        encode_path_segment(org_id)
    );
    post_json(&url(&path), token, Some(body)).await
}

/// `DELETE /organisations/{id}/members/{user_id}` — remove an org member (org
/// admin/owner; the last Owner cannot be removed server-side).
pub async fn remove_org_member(
    token: Option<&str>,
    org_id: &str,
    user_id: &str,
) -> Result<(), RestError> {
    let path = format!(
        "{ORGANISATIONS_PATH}/{}/members/{}",
        encode_path_segment(org_id),
        encode_path_segment(user_id)
    );
    delete_resource(&url(&path), token).await
}

/// `PUT /organisations/{id}/policy` — set the org's `workspace_creation` policy
/// (org admin/owner). Returns the updated organisation.
pub async fn set_org_policy(
    token: Option<&str>,
    org_id: &str,
    body: &SetOrgPolicy,
) -> Result<Organisation, RestError> {
    let path = format!(
        "{ORGANISATIONS_PATH}/{}/policy",
        encode_path_segment(org_id)
    );
    put_json(&url(&path), token, body).await
}

/// `GET /organisations/{id}/user-lookup?email=…` — resolve an exact email to a
/// user for the add-member flow (org admin/owner). A `404` (`RestError::Status`
/// with status 404) means "no user with that email"; the caller surfaces it as a
/// friendly line rather than a raw error.
pub async fn user_lookup(
    token: Option<&str>,
    org_id: &str,
    email: &str,
) -> Result<UserLookup, RestError> {
    let path = format!(
        "{ORGANISATIONS_PATH}/{}/user-lookup?email={}",
        encode_path_segment(org_id),
        encode_query_component(email)
    );
    get_json(&url(&path), token).await
}

/// `GET /organisations/{id}/workspaces` — every workspace **shell** in the org
/// (org admin/owner), including archived shells flagged by `archived_at`. A `403`
/// is the fail-closed signal the caller is not an org admin.
pub async fn list_org_workspaces(
    token: Option<&str>,
    org_id: &str,
) -> Result<Vec<WorkspaceShell>, RestError> {
    let path = format!(
        "{ORGANISATIONS_PATH}/{}/workspaces",
        encode_path_segment(org_id)
    );
    get_json(&url(&path), token).await
}

/// `DELETE /organisations/{id}/workspaces/{ws_id}` — **soft-archive** a workspace
/// shell (org admin/owner; reversible). The workspace vanishes from the switcher
/// but its data is retained and it can be restored.
pub async fn archive_org_workspace(
    token: Option<&str>,
    org_id: &str,
    ws_id: &str,
) -> Result<(), RestError> {
    let path = format!(
        "{ORGANISATIONS_PATH}/{}/workspaces/{}",
        encode_path_segment(org_id),
        encode_path_segment(ws_id)
    );
    delete_resource(&url(&path), token).await
}

/// `POST /organisations/{id}/workspaces/{ws_id}/restore` — **restore** an archived
/// workspace shell (org admin/owner; clears `archived_at`). Returns the restored
/// shell.
pub async fn restore_org_workspace(
    token: Option<&str>,
    org_id: &str,
    ws_id: &str,
) -> Result<WorkspaceShell, RestError> {
    let path = format!(
        "{ORGANISATIONS_PATH}/{}/workspaces/{}/restore",
        encode_path_segment(org_id),
        encode_path_segment(ws_id)
    );
    post_json::<(), _>(&url(&path), token, None).await
}

// --- Memories + profile (SOUL §22) ------------------------------------------

/// `GET /memories` — the memories visible to the caller, newest first.
pub async fn list_memories(token: Option<&str>) -> Result<Vec<Memory>, RestError> {
    get_json(&url(MEMORIES_PATH), token).await
}

/// `POST /memories` — create a memory (`user` scope is private to the caller).
pub async fn create_memory(token: Option<&str>, body: &CreateMemory) -> Result<Memory, RestError> {
    post_json(&url(MEMORIES_PATH), token, Some(body)).await
}

/// `PUT /memories/{id}` — replace a memory's text.
pub async fn update_memory(
    token: Option<&str>,
    id: &str,
    body: &UpdateMemory,
) -> Result<Memory, RestError> {
    let path = format!("{MEMORIES_PATH}/{}", encode_path_segment(id));
    put_json(&url(&path), token, body).await
}

/// `DELETE /memories/{id}` — delete a memory.
pub async fn delete_memory(token: Option<&str>, id: &str) -> Result<(), RestError> {
    let path = format!("{MEMORIES_PATH}/{}", encode_path_segment(id));
    delete_resource(&url(&path), token).await
}

/// `GET /profile` — the caller's profile (an empty one if unset).
pub async fn get_profile(token: Option<&str>) -> Result<Profile, RestError> {
    get_json(&url(PROFILE_PATH), token).await
}

/// `PUT /profile` — merge `fields` (a JSON object) into the caller's profile.
pub async fn update_profile(
    token: Option<&str>,
    fields: &serde_json::Value,
) -> Result<Profile, RestError> {
    put_json(&url(PROFILE_PATH), token, fields).await
}

// --- Onboarding / quick-start (SOUL §12/§22/§23) ----------------------------

/// `GET /onboarding/state` — the caller's quick-start state (drives first-run).
pub async fn get_onboarding_state(token: Option<&str>) -> Result<OnboardingState, RestError> {
    get_json(&url(ONBOARDING_STATE_PATH), token).await
}

/// `POST /onboarding/personalize` — one turn of the assistant-led personalization
/// chat: the next message plus proposed memories + skill drafts.
pub async fn personalize(
    token: Option<&str>,
    body: &PersonalizeRequest,
) -> Result<PersonalizeResponse, RestError> {
    post_json(&url(ONBOARDING_PERSONALIZE_PATH), token, Some(body)).await
}

/// `POST /onboarding/complete` — stamp the completion sentinel (no body).
pub async fn complete_onboarding(token: Option<&str>) -> Result<OnboardingState, RestError> {
    post_json(&url(ONBOARDING_COMPLETE_PATH), token, None::<&()>).await
}

// --- Kanban boards + tasks (SOUL §24) ---------------------------------------

/// `GET /boards` — this workspace's Kanban boards (with columns).
pub async fn list_boards(token: Option<&str>) -> Result<Vec<Board>, RestError> {
    get_json(&url(BOARDS_PATH), token).await
}

/// `POST /boards` — create a board (empty `columns` → the server's defaults).
pub async fn create_board(token: Option<&str>, body: &CreateBoard) -> Result<Board, RestError> {
    post_json(&url(BOARDS_PATH), token, Some(body)).await
}

/// `PUT /boards/{id}` — rename a board.
pub async fn rename_board(
    token: Option<&str>,
    id: &str,
    body: &RenameBoard,
) -> Result<Board, RestError> {
    let path = format!("{BOARDS_PATH}/{}", encode_path_segment(id));
    put_json(&url(&path), token, body).await
}

/// `DELETE /boards/{id}` — delete a board (+ its columns & tasks).
pub async fn delete_board(token: Option<&str>, id: &str) -> Result<(), RestError> {
    let path = format!("{BOARDS_PATH}/{}", encode_path_segment(id));
    delete_resource(&url(&path), token).await
}

/// `GET /boards/{id}/tasks` — the board's tasks (column order).
pub async fn list_board_tasks(token: Option<&str>, board_id: &str) -> Result<Vec<Task>, RestError> {
    let path = format!("{BOARDS_PATH}/{}/tasks", encode_path_segment(board_id));
    get_json(&url(&path), token).await
}

/// `POST /boards/{id}/tasks` — create a task in a column.
pub async fn create_task(
    token: Option<&str>,
    board_id: &str,
    body: &CreateTask,
) -> Result<Task, RestError> {
    let path = format!("{BOARDS_PATH}/{}/tasks", encode_path_segment(board_id));
    post_json(&url(&path), token, Some(body)).await
}

/// `POST /boards/{id}/columns` — append a column to a board.
pub async fn add_column(
    token: Option<&str>,
    board_id: &str,
    body: &AddColumn,
) -> Result<Board, RestError> {
    let path = format!("{BOARDS_PATH}/{}/columns", encode_path_segment(board_id));
    post_json(&url(&path), token, Some(body)).await
}

/// `PUT /columns/{id}` — rename a column; returns the updated board.
pub async fn rename_column(
    token: Option<&str>,
    column_id: &str,
    body: &RenameColumn,
) -> Result<Board, RestError> {
    let path = format!("{COLUMNS_PATH}/{}", encode_path_segment(column_id));
    put_json(&url(&path), token, body).await
}

/// `DELETE /columns/{id}` — delete an **empty** column (the server refuses with
/// a 400 while tasks remain in it, or when it is the board's only column).
pub async fn delete_column(token: Option<&str>, column_id: &str) -> Result<(), RestError> {
    let path = format!("{COLUMNS_PATH}/{}", encode_path_segment(column_id));
    delete_resource(&url(&path), token).await
}

/// `POST /tasks/{id}/move` — move a task to another column (same board).
pub async fn move_task(
    token: Option<&str>,
    task_id: &str,
    body: &MoveTask,
) -> Result<Task, RestError> {
    let path = format!("{TASKS_PATH}/{}/move", encode_path_segment(task_id));
    post_json(&url(&path), token, Some(body)).await
}

/// `POST /tasks/{id}/status` — set a task's status.
pub async fn set_task_status(
    token: Option<&str>,
    task_id: &str,
    body: &SetTaskStatus,
) -> Result<Task, RestError> {
    let path = format!("{TASKS_PATH}/{}/status", encode_path_segment(task_id));
    post_json(&url(&path), token, Some(body)).await
}

/// `PUT /tasks/{id}` — edit a task's title + markdown body.
pub async fn update_task(
    token: Option<&str>,
    task_id: &str,
    body: &EditTask,
) -> Result<Task, RestError> {
    let path = format!("{TASKS_PATH}/{}", encode_path_segment(task_id));
    put_json(&url(&path), token, body).await
}

/// `DELETE /tasks/{id}` — remove a task (card) from its board.
pub async fn delete_task(token: Option<&str>, task_id: &str) -> Result<(), RestError> {
    let path = format!("{TASKS_PATH}/{}", encode_path_segment(task_id));
    delete_resource(&url(&path), token).await
}

// --- Graph query (SOUL §6.3) ------------------------------------------------

/// `POST /graph/query` — run a safe Datalog program over the derived graph.
/// Returns the column names + rows (a `404` if no graph backend is configured; a
/// `400` if the program is invalid/unsafe — both surfaced as a [`RestError`]).
pub async fn graph_query(
    token: Option<&str>,
    body: &GraphQueryRequest,
) -> Result<GraphQueryResponse, RestError> {
    post_json(&url(GRAPH_QUERY_PATH), token, Some(body)).await
}

// --- Web fetch (SOUL §27) ---------------------------------------------------

/// `POST /fetch` — retrieve a web page as Markdown/HTML/text via the configured
/// backend. Returns the [`FetchedPage`] (a `500` if no fetch backend is
/// configured; a `400` for an empty URL — both surfaced as a [`RestError`]).
pub async fn fetch_url(token: Option<&str>, body: &FetchRequest) -> Result<FetchedPage, RestError> {
    post_json(&url(FETCH_PATH), token, Some(body)).await
}

// --- Email (SOUL §28, read-only) --------------------------------------------

/// `GET /mailboxes` — this workspace's mailboxes.
pub async fn list_mailboxes(token: Option<&str>) -> Result<Vec<Mailbox>, RestError> {
    get_json(&url(MAILBOXES_PATH), token).await
}

/// `GET /emails?…` — compact email rows, with optional `mailbox_id` / `sender` /
/// `unread` filters (recent first). The query is built only from the filters that
/// are set. The sidebar filters by mailbox **id** (names collide across accounts
/// — every account has an `INBOX`).
pub async fn list_emails(
    token: Option<&str>,
    mailbox_id: Option<&str>,
    sender: Option<&str>,
    content: Option<&str>,
    unread: Option<bool>,
) -> Result<Vec<EmailView>, RestError> {
    let mut params: Vec<String> = Vec::new();
    if let Some(m) = mailbox_id.filter(|s| !s.is_empty()) {
        params.push(format!("mailbox_id={}", encode_query_component(m)));
    }
    if let Some(s) = sender.filter(|s| !s.is_empty()) {
        params.push(format!("sender={}", encode_query_component(s)));
    }
    if let Some(c) = content.filter(|s| !s.is_empty()) {
        params.push(format!("q={}", encode_query_component(c)));
    }
    if let Some(u) = unread {
        params.push(format!("unread={u}"));
    }
    let full = if params.is_empty() {
        url(EMAILS_PATH)
    } else {
        format!("{}?{}", url(EMAILS_PATH), params.join("&"))
    };
    get_json(&full, token).await
}

/// `GET /emails/{id}` — one email with its body + recipients (the detail view).
pub async fn get_email(token: Option<&str>, id: &str) -> Result<EmailDetail, RestError> {
    let path = format!("{EMAILS_PATH}/{}", encode_path_segment(id));
    get_json(&url(&path), token).await
}

/// `PATCH /emails/{id}` — mark an email read (`unread = false`) or unread.
/// Flips catalerum's **local** `seen` flag only — the provider's mailbox is
/// never written (SOUL §14). Returns the email's new read state.
pub async fn set_email_read(
    token: Option<&str>,
    id: &str,
    unread: bool,
) -> Result<crate::api::EmailReadState, RestError> {
    let path = format!("{EMAILS_PATH}/{}", encode_path_segment(id));
    patch_json(&url(&path), token, &serde_json::json!({ "unread": unread })).await
}

/// `GET /email/connections` — this workspace's configured email sources.
pub async fn list_email_connections(token: Option<&str>) -> Result<Vec<Connection>, RestError> {
    get_json(&url(EMAIL_CONNECTIONS_PATH), token).await
}

/// `GET /db/connections` — this workspace's external Postgres connections (the
/// sources a `collect_sql` trigger polls; registered by an admin over REST).
pub async fn list_db_connections(token: Option<&str>) -> Result<Vec<Connection>, RestError> {
    get_json(&url(DB_CONNECTIONS_PATH), token).await
}

/// `POST /email/connections` — register a read-only email source. Returns the
/// created [`Connection`]; the background poller syncs it shortly after.
pub async fn create_email_connection(
    token: Option<&str>,
    body: &CreateEmailConnection,
) -> Result<Connection, RestError> {
    post_json(&url(EMAIL_CONNECTIONS_PATH), token, Some(body)).await
}

/// `GET /email/connections/{id}` — one email source with its **non-secret**
/// settings (the edit form's prefill; secrets never cross the wire).
pub async fn get_email_connection(
    token: Option<&str>,
    id: &str,
) -> Result<crate::api::EmailConnectionDetail, RestError> {
    let path = format!("{EMAIL_CONNECTIONS_PATH}/{}", encode_path_segment(id));
    get_json(&url(&path), token).await
}

/// `PUT /email/connections/{id}` — update an email source's name/settings. A
/// blank/omitted secret keeps the stored one (so editing a host never forces
/// re-entering the password). Returns the updated [`Connection`].
pub async fn update_email_connection(
    token: Option<&str>,
    id: &str,
    body: &CreateEmailConnection,
) -> Result<Connection, RestError> {
    let path = format!("{EMAIL_CONNECTIONS_PATH}/{}", encode_path_segment(id));
    put_json(&url(&path), token, body).await
}

/// `DELETE /email/connections/{id}` — remove an email source (+ its synced data).
pub async fn delete_email_connection(token: Option<&str>, id: &str) -> Result<(), RestError> {
    let path = format!("{EMAIL_CONNECTIONS_PATH}/{}", encode_path_segment(id));
    delete_resource(&url(&path), token).await
}

// --- Settings: status + API keys (SOUL §12/§18) -----------------------------

/// `GET /status/login` — the anonymous presentation flags (`sso`, `mode`) the
/// login view reads before any session exists. Deliberately token-less.
pub async fn get_login_status() -> Result<crate::api::LoginStatusInfo, RestError> {
    get_json(&url(crate::api::LOGIN_STATUS_PATH), None).await
}

pub async fn get_setup_status() -> Result<crate::api::SetupStatusInfo, RestError> {
    get_json(&url(AUTH_SETUP_PATH), None).await
}

pub async fn setup_account(
    body: &crate::api::SetupAccount,
) -> Result<crate::api::LoginSession, RestError> {
    post_json(&url(AUTH_SETUP_PATH), None, Some(body)).await
}

pub async fn password_login(
    body: &crate::api::PasswordLogin,
) -> Result<crate::api::LoginSession, RestError> {
    post_json(&url(AUTH_PASSWORD_PATH), None, Some(body)).await
}

pub async fn list_llmleaf_topology(
    token: Option<&str>,
    kind: &str,
) -> Result<Vec<crate::api::LlmleafTopologyEntry>, RestError> {
    get_json(&url(&format!("{LLMLEAF_PATH}/{kind}")), token).await
}

pub async fn put_llmleaf_topology(
    token: Option<&str>,
    kind: &str,
    name: &str,
    body: &crate::api::PutLlmleafTopology,
) -> Result<crate::api::LlmleafTopologyEntry, RestError> {
    put_json(
        &url(&format!("{LLMLEAF_PATH}/{kind}/{}", encode_path_tail(name))),
        token,
        body,
    )
    .await
}

pub async fn delete_llmleaf_topology(
    token: Option<&str>,
    kind: &str,
    name: &str,
) -> Result<(), RestError> {
    delete_resource(
        &url(&format!("{LLMLEAF_PATH}/{kind}/{}", encode_path_tail(name))),
        token,
    )
    .await
}

pub async fn list_managed_users(
    token: Option<&str>,
) -> Result<Vec<crate::api::ManagedUser>, RestError> {
    get_json(&url(USERS_PATH), token).await
}

pub async fn create_managed_user(
    token: Option<&str>,
    body: &crate::api::CreateManagedUser,
) -> Result<crate::api::ManagedUser, RestError> {
    post_json(&url(USERS_PATH), token, Some(body)).await
}

pub async fn reset_managed_password(
    token: Option<&str>,
    user_id: &str,
    password: String,
) -> Result<(), RestError> {
    let body = crate::api::ResetManagedPassword { password };
    post_json_empty(
        &url(&format!(
            "{USERS_PATH}/{}/password",
            encode_path_segment(user_id)
        )),
        token,
        &body,
    )
    .await
}

/// `GET /status` — server version, LLM gateway config, and service health.
pub async fn get_status(token: Option<&str>) -> Result<StatusInfo, RestError> {
    get_json(&url(STATUS_PATH), token).await
}

/// `GET /llm-settings` — the caller's per-user model/voice selections (SOUL §7/§13).
pub async fn get_llm_settings(token: Option<&str>) -> Result<LlmSettings, RestError> {
    get_json(&url(LLM_SETTINGS_PATH), token).await
}

/// `PUT /llm-settings` — replace the caller's selections (a blank field clears it).
pub async fn set_llm_settings(
    token: Option<&str>,
    body: &LlmSettings,
) -> Result<LlmSettings, RestError> {
    put_json(&url(LLM_SETTINGS_PATH), token, body).await
}

/// `PUT /llm-settings/image-models` — replace the caller's force-image-input model
/// list (SOUL §7/§9); returns the updated settings. Separate from
/// [`set_llm_settings`] so the chat sidebar toggle and the settings panel can edit
/// just this list without disturbing the model/voice selections.
pub async fn set_image_input_models(
    token: Option<&str>,
    models: &[String],
) -> Result<LlmSettings, RestError> {
    #[derive(serde::Serialize)]
    struct Body<'a> {
        models: &'a [String],
    }
    put_json(
        &url(&format!("{LLM_SETTINGS_PATH}/image-models")),
        token,
        &Body { models },
    )
    .await
}

/// `GET /llm-models?kind=…` — the gateway's model catalog for autocomplete,
/// filtered to one model class. `kind` is `llm` / `tts` / `stt` (an empty `kind`
/// lists the full catalog) so the speech picker offers pure TTS models and the
/// transcription picker pure STT models — including type-only ids the full catalog
/// omits.
pub async fn list_llm_models(token: Option<&str>, kind: &str) -> Result<Vec<ModelInfo>, RestError> {
    let path = if kind.is_empty() {
        LLM_MODELS_PATH.to_string()
    } else {
        format!("{LLM_MODELS_PATH}?kind={}", encode_query_component(kind))
    };
    get_json(&url(&path), token).await
}

/// The transcript returned by `POST /audio/transcriptions` — the mic recorder's
/// audio, recognized to text (plus the provider's metadata when reported).
#[derive(Clone, Debug, serde::Deserialize)]
pub struct Transcription {
    /// The recognized text.
    pub text: String,
    /// Detected/declared language, when the provider reports it.
    #[serde(default)]
    pub language: Option<String>,
    /// Audio duration in seconds, when reported.
    #[serde(default)]
    pub duration: Option<f32>,
    /// The STT model the transcription ran through.
    #[serde(default)]
    pub model: Option<String>,
}

/// `POST /audio/transcriptions` — transcribe recorded audio bytes to text. The
/// body is the raw audio (a browser `MediaRecorder` blob); `content_type` carries
/// the container hint the server maps to a filename extension. Returns the
/// transcript, which the composer drops into the draft.
pub async fn transcribe_audio(
    token: Option<&str>,
    request_id: &str,
    bytes: &[u8],
    content_type: Option<&str>,
) -> Result<Transcription, RestError> {
    post_bytes_json(
        &url(AUDIO_TRANSCRIBE_PATH),
        token,
        content_type,
        Some(request_id),
        bytes,
    )
    .await
}

/// `GET /llm-voices?model=…` — a speech model's voices for autocomplete; an empty
/// `model` lets the server fall back to the configured default speech model.
pub async fn list_llm_voices(
    token: Option<&str>,
    model: &str,
) -> Result<Vec<VoiceInfo>, RestError> {
    let path = if model.is_empty() {
        LLM_VOICES_PATH.to_string()
    } else {
        format!("{LLM_VOICES_PATH}?model={}", encode_query_component(model))
    };
    get_json(&url(&path), token).await
}

/// `GET /search-settings` — the caller's per-user default web-search provider (SOUL §27/§13).
pub async fn get_search_settings(token: Option<&str>) -> Result<SearchSettings, RestError> {
    get_json(&url(SEARCH_SETTINGS_PATH), token).await
}

/// `PUT /search-settings` — set/clear the caller's default web-search provider (a
/// `None` provider clears the override → the `[search].backend` config default).
pub async fn set_search_settings(
    token: Option<&str>,
    body: &SearchSettings,
) -> Result<SearchSettings, RestError> {
    put_json(&url(SEARCH_SETTINGS_PATH), token, body).await
}

/// `GET /search-providers` — the web-search provider catalog: which engines exist,
/// which are configured server-side, and which is the caller's default (SOUL §27).
pub async fn list_search_providers(
    token: Option<&str>,
) -> Result<Vec<SearchProviderInfo>, RestError> {
    get_json(&url(SEARCH_PROVIDERS_PATH), token).await
}

/// `GET /storage-settings` — the caller's per-user default files store (SOUL §9/§13).
pub async fn get_storage_settings(token: Option<&str>) -> Result<StorageSettings, RestError> {
    get_json(&url(STORAGE_SETTINGS_PATH), token).await
}

/// `PUT /storage-settings` — set/clear the caller's default files store (a `None`
/// store clears the override → the `[storage]` config default).
pub async fn set_storage_settings(
    token: Option<&str>,
    body: &StorageSettings,
) -> Result<StorageSettings, RestError> {
    put_json(&url(STORAGE_SETTINGS_PATH), token, body).await
}

/// `GET /tokens` — the caller's active API-key tokens in the current workspace.
pub async fn list_tokens(token: Option<&str>) -> Result<Vec<TokenView>, RestError> {
    get_json(&url(TOKENS_PATH), token).await
}

/// `POST /tokens` — issue a new bearer token (the raw secret is returned once).
pub async fn create_token(
    token: Option<&str>,
    body: &CreateToken,
) -> Result<CreatedToken, RestError> {
    post_json(&url(TOKENS_PATH), token, Some(body)).await
}

/// `DELETE /tokens/{id}` — revoke one of the caller's tokens by id.
pub async fn revoke_token(token: Option<&str>, id: &str) -> Result<(), RestError> {
    let path = format!("{TOKENS_PATH}/{}", encode_path_segment(id));
    delete_resource(&url(&path), token).await
}

// --- Computer agents (SOUL §19/§20) -------------------------------------------

/// `GET /computer-agents` — the workspace's enrolled computer agents (+ online).
pub async fn list_computer_agents(
    token: Option<&str>,
) -> Result<Vec<ComputerAgentView>, RestError> {
    get_json(&url(COMPUTER_AGENTS_PATH), token).await
}

/// `POST /computer-agents` — enroll one; the raw token is returned once.
pub async fn enroll_computer_agent(
    token: Option<&str>,
    body: &EnrollComputerAgent,
) -> Result<EnrolledComputerAgent, RestError> {
    post_json(&url(COMPUTER_AGENTS_PATH), token, Some(body)).await
}

/// `DELETE /computer-agents/{id}` — revoke one by id.
pub async fn revoke_computer_agent(token: Option<&str>, id: &str) -> Result<(), RestError> {
    let path = format!("{COMPUTER_AGENTS_PATH}/{}", encode_path_segment(id));
    delete_resource(&url(&path), token).await
}

// --- MCP endpoints (SOUL §30) -------------------------------------------------

/// `GET /mcp-endpoints` — the workspace's scripted MCP endpoints (recency order),
/// each in full (script + scope pins + grant), so the manager needs no per-select
/// fetch.
pub async fn list_mcp_endpoints(token: Option<&str>) -> Result<Vec<McpEndpoint>, RestError> {
    get_json(&url(MCP_ENDPOINTS_PATH), token).await
}

/// `POST /mcp-endpoints` — create a scripted endpoint. Returns the created
/// [`McpEndpoint`] (`409` if the name already exists in the workspace).
pub async fn create_mcp_endpoint(
    token: Option<&str>,
    body: &McpEndpointBody,
) -> Result<McpEndpoint, RestError> {
    post_json(&url(MCP_ENDPOINTS_PATH), token, Some(body)).await
}

/// `PUT /mcp-endpoints/{id}` — update an endpoint (name included, so a rename is
/// allowed; `409` if the new name collides). Returns the stored [`McpEndpoint`].
pub async fn update_mcp_endpoint(
    token: Option<&str>,
    id: &str,
    body: &McpEndpointBody,
) -> Result<McpEndpoint, RestError> {
    let path = format!("{MCP_ENDPOINTS_PATH}/{}", encode_path_segment(id));
    put_json(&url(&path), token, body).await
}

/// `DELETE /mcp-endpoints/{id}` — delete an endpoint.
pub async fn delete_mcp_endpoint(token: Option<&str>, id: &str) -> Result<(), RestError> {
    let path = format!("{MCP_ENDPOINTS_PATH}/{}", encode_path_segment(id));
    delete_resource(&url(&path), token).await
}

/// `POST /mcp-endpoints/{id}/token` — mint a shareable scoped URL for one
/// endpoint (a signed token in the path; no bearer header needed to serve).
pub async fn mint_mcp_endpoint_token(
    token: Option<&str>,
    id: &str,
    body: &MintEndpointToken,
) -> Result<MintedEndpointToken, RestError> {
    let path = format!("{MCP_ENDPOINTS_PATH}/{}/token", encode_path_segment(id));
    post_json(&url(&path), token, Some(body)).await
}

// --- External MCP servers (SOUL §26) ----------------------------------------

/// `GET /mcp-servers` — the workspace's external MCP servers (catalerum as an MCP
/// *client*), secrets redacted, with live connection status.
pub async fn list_mcp_servers(token: Option<&str>) -> Result<Vec<McpServerView>, RestError> {
    get_json(&url(MCP_SERVERS_PATH), token).await
}

/// `POST /mcp-servers` — register a server and connect it live. Returns the
/// created (redacted) view (`409` if the name already exists).
pub async fn create_mcp_server(
    token: Option<&str>,
    body: &McpServerBody,
) -> Result<McpServerView, RestError> {
    post_json(&url(MCP_SERVERS_PATH), token, Some(body)).await
}

/// `PUT /mcp-servers/{name}` — replace a server by name and reconnect it. A blank
/// secret keeps the stored one; `name` is the identity (no rename).
pub async fn update_mcp_server(
    token: Option<&str>,
    name: &str,
    body: &McpServerBody,
) -> Result<McpServerView, RestError> {
    let path = format!("{MCP_SERVERS_PATH}/{}", encode_path_segment(name));
    put_json(&url(&path), token, body).await
}

/// `DELETE /mcp-servers/{name}` — disconnect and remove a server by name.
pub async fn delete_mcp_server(token: Option<&str>, name: &str) -> Result<(), RestError> {
    let path = format!("{MCP_SERVERS_PATH}/{}", encode_path_segment(name));
    delete_resource(&url(&path), token).await
}

// --- Conversations (SOUL §12) -----------------------------------------------

/// `GET /conversations` — this workspace's conversations (newest first).
pub async fn list_conversations(token: Option<&str>) -> Result<Vec<Conversation>, RestError> {
    get_json(&url(CONVERSATIONS_PATH), token).await
}

/// `POST /conversations` — start a new chat thread (origin `web`). Returns the
/// created [`Conversation`], whose server-minted id the Chat panel then drives
/// over the `/ws/chat` socket (the WS handler rejects an unknown id, so the
/// thread must be created first).
pub async fn create_conversation(
    token: Option<&str>,
    body: &CreateConversation,
) -> Result<Conversation, RestError> {
    post_json(&url(CONVERSATIONS_PATH), token, Some(body)).await
}

/// `PUT /conversations/{id}` — rename a conversation.
pub async fn get_conversation(
    token: Option<&str>,
    id: &str,
) -> Result<crate::api::Conversation, RestError> {
    let path = format!("{CONVERSATIONS_PATH}/{}", encode_path_segment(id));
    get_json(&url(&path), token).await
}

pub async fn rename_conversation(
    token: Option<&str>,
    id: &str,
    body: &RenameConversation,
) -> Result<Conversation, RestError> {
    let path = format!("{CONVERSATIONS_PATH}/{}", encode_path_segment(id));
    put_json(&url(&path), token, body).await
}

/// `POST /conversations/{id}/profile` — bind (or, with `None`, unbind) the agent
/// profile this thread runs *as* (the chat picker, SOUL §19). Returns the updated
/// [`Conversation`].
pub async fn set_conversation_profile(
    token: Option<&str>,
    id: &str,
    agent_profile_id: Option<&str>,
) -> Result<Conversation, RestError> {
    let path = format!("{CONVERSATIONS_PATH}/{}/profile", encode_path_segment(id));
    let body = SetConversationProfile {
        agent_profile_id: agent_profile_id.map(str::to_string),
    };
    post_json(&url(&path), token, Some(&body)).await
}

/// `POST /conversations/{id}/model` — pin (or clear, with `None`) the model this
/// thread's chat loop thinks with (the chat "model" picker, SOUL §7).
pub async fn set_conversation_model(
    token: Option<&str>,
    id: &str,
    model: Option<&str>,
) -> Result<Conversation, RestError> {
    let path = format!("{CONVERSATIONS_PATH}/{}/model", encode_path_segment(id));
    let body = SetConversationModel {
        model: model.map(str::to_string),
    };
    post_json(&url(&path), token, Some(&body)).await
}

/// `POST /conversations/{id}/reasoning` — set (or clear, with `None`) the reasoning
/// ("thinking") effort this thread's chat loop requests (the chat "thinking" picker,
/// SOUL §7).
pub async fn set_conversation_reasoning(
    token: Option<&str>,
    id: &str,
    reasoning_effort: Option<&str>,
) -> Result<Conversation, RestError> {
    let path = format!("{CONVERSATIONS_PATH}/{}/reasoning", encode_path_segment(id));
    let body = SetConversationReasoning {
        reasoning_effort: reasoning_effort.map(str::to_string),
    };
    post_json(&url(&path), token, Some(&body)).await
}

/// `DELETE /conversations/{id}` — delete a conversation and its messages.
pub async fn delete_conversation(token: Option<&str>, id: &str) -> Result<(), RestError> {
    let path = format!("{CONVERSATIONS_PATH}/{}", encode_path_segment(id));
    delete_resource(&url(&path), token).await
}

/// `GET /conversations/{id}/messages` — a conversation's transcript (oldest
/// first, replay order).
pub async fn list_messages(token: Option<&str>, id: &str) -> Result<Vec<Message>, RestError> {
    let path = format!("{CONVERSATIONS_PATH}/{}/messages", encode_path_segment(id));
    get_json(&url(&path), token).await
}

/// The transcript of `GET /conversations/{id}/messages` as **raw JSON text**,
/// pretty-printed — the chat sidebar's debug export (SOUL §12). Deliberately not
/// decoded into [`Message`]: the point is a verbatim dump (every persisted field,
/// including ones this client doesn't model) that can be pasted into a bug
/// report or handed to an LLM to diagnose a broken thread.
pub async fn conversation_debug_json(token: Option<&str>, id: &str) -> Result<String, RestError> {
    let path = format!("{CONVERSATIONS_PATH}/{}/messages", encode_path_segment(id));
    let body = get_body(&url(&path), token).await?;
    // Re-indent for humans/models; on the (impossible) non-JSON body, pass it through.
    Ok(serde_json::from_str::<serde_json::Value>(&body)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or(body))
}

/// `GET /conversations/{id}/pending_question` — the thread's unresolved `ask_user`
/// form, if any (SOUL §7/§12). Fetched when opening a conversation so a question
/// that was asked before a reload/reconnect re-renders. `None` when nothing pends.
pub async fn get_pending_question(
    token: Option<&str>,
    id: &str,
) -> Result<Option<PendingQuestion>, RestError> {
    let path = format!(
        "{CONVERSATIONS_PATH}/{}/pending_question",
        encode_path_segment(id)
    );
    get_json(&url(&path), token).await
}

/// `GET /conversations/{id}/questions` — every `ask_user` form the thread asked,
/// oldest first, each with the structured answers the user gave (SOUL §7/§12).
/// Fetched when replaying a transcript so an answered form re-renders with the
/// user's actual picks.
pub async fn list_questions(
    token: Option<&str>,
    id: &str,
) -> Result<Vec<ConversationQuestion>, RestError> {
    let path = format!("{CONVERSATIONS_PATH}/{}/questions", encode_path_segment(id));
    get_json(&url(&path), token).await
}

/// `GET /conversations/{id}/pending_approval` — the thread's guard-deferred tool
/// call awaiting Approve/Reject, if any (SOUL §19). Fetched when opening a
/// conversation so an approval prompt that outlived a reload / reconnect / restart
/// re-renders. `None` when nothing pends.
pub async fn get_pending_approval(
    token: Option<&str>,
    id: &str,
) -> Result<Option<PendingApproval>, RestError> {
    let path = format!(
        "{CONVERSATIONS_PATH}/{}/pending_approval",
        encode_path_segment(id)
    );
    get_json(&url(&path), token).await
}

/// `GET /conversations/{id}/active_turn` — the conversation's currently streaming
/// turn, if any (SOUL §7/§12). Fetched when opening a conversation so a client can
/// (re)attach to the live stream instead of only seeing persisted history. `None`
/// when nothing is streaming right now.
pub async fn get_active_turn(
    token: Option<&str>,
    id: &str,
) -> Result<Option<ActiveTurn>, RestError> {
    let path = format!(
        "{CONVERSATIONS_PATH}/{}/active_turn",
        encode_path_segment(id)
    );
    get_json(&url(&path), token).await
}

/// `GET /conversations/search?q=` — search message content across the workspace
/// (newest match first), each hit carrying its conversation title.
pub async fn search_messages(
    token: Option<&str>,
    query: &str,
) -> Result<Vec<MessageHit>, RestError> {
    let path = format!(
        "{CONVERSATIONS_PATH}/search?q={}",
        encode_query_component(query)
    );
    get_json(&url(&path), token).await
}

// --- Grants (SOUL §19, admin-only) ------------------------------------------

/// `GET /grants` — this workspace's capability grants (admin-only; a non-admin
/// principal gets `403`, surfaced as a [`RestError::Status`]).
pub async fn list_grants(token: Option<&str>) -> Result<Vec<Grant>, RestError> {
    get_json(&url(GRANTS_PATH), token).await
}

/// `POST /grants` — create-or-replace a grant by name (idempotent, keeps the id).
/// Returns the stored [`Grant`]. `403` if a capability exceeds the caller's own
/// authority (§19 attenuation), surfaced as the form error.
pub async fn create_grant(token: Option<&str>, body: &CreateGrant) -> Result<Grant, RestError> {
    post_json(&url(GRANTS_PATH), token, Some(body)).await
}

/// `DELETE /grants/{id}` — remove a grant (an automation referencing it is
/// detached). Keyed by id (unlike create, which is by name).
pub async fn delete_grant(token: Option<&str>, id: &str) -> Result<(), RestError> {
    let path = format!("{GRANTS_PATH}/{}", encode_path_segment(id));
    delete_resource(&url(&path), token).await
}

// --- Automations (SOUL §11) -------------------------------------------------

/// `GET /automations` — this workspace's automations (by name).
pub async fn list_automations(token: Option<&str>) -> Result<Vec<Automation>, RestError> {
    get_json(&url(AUTOMATIONS_PATH), token).await
}

/// `GET /automations/node-types` — the full node-type catalog (docs for authoring
/// an automation graph). Static, so the editor can fetch it once.
pub async fn list_automation_node_types(
    token: Option<&str>,
) -> Result<Vec<NodeTypeHit>, RestError> {
    get_json(&url(AUTOMATION_NODE_TYPES_PATH), token).await
}

/// `GET /automations/node-types/search?q=…&limit=N` — semantically rank the
/// node-type catalog against `query`. Returns the matching node types, best first.
pub async fn search_automation_node_types(
    token: Option<&str>,
    query: &str,
    limit: usize,
) -> Result<Vec<NodeTypeHit>, RestError> {
    let path = format!(
        "{AUTOMATION_NODE_TYPES_PATH}/search?q={}&limit={limit}",
        encode_query_component(query),
    );
    get_json(&url(&path), token).await
}

/// `POST /automations` — create an automation. Returns the created
/// [`Automation`] (`409` if the name exists; `400` if the typed spec is invalid).
pub async fn create_automation(
    token: Option<&str>,
    body: &CreateAutomation,
) -> Result<Automation, RestError> {
    post_json(&url(AUTOMATIONS_PATH), token, Some(body)).await
}

/// `PUT /automations/{name}` — create-or-replace the named automation.
pub async fn update_automation(
    token: Option<&str>,
    name: &str,
    body: &UpdateAutomation,
) -> Result<Automation, RestError> {
    let path = format!("{AUTOMATIONS_PATH}/{}", encode_path_segment(name));
    put_json(&url(&path), token, body).await
}

/// `DELETE /automations/{name}` — delete an automation.
pub async fn delete_automation(token: Option<&str>, name: &str) -> Result<(), RestError> {
    let path = format!("{AUTOMATIONS_PATH}/{}", encode_path_segment(name));
    delete_resource(&url(&path), token).await
}

/// `POST /automations/{name}/enabled` — pause / resume without re-validating the
/// spec. Returns the updated [`Automation`].
pub async fn set_automation_enabled(
    token: Option<&str>,
    name: &str,
    enabled: bool,
) -> Result<Automation, RestError> {
    let path = format!("{AUTOMATIONS_PATH}/{}/enabled", encode_path_segment(name));
    post_json(&url(&path), token, Some(&SetEnabled { enabled })).await
}

/// `GET /automations/{name}/runs` — recent runs, newest first.
pub async fn list_automation_runs(
    token: Option<&str>,
    name: &str,
) -> Result<Vec<AutomationRun>, RestError> {
    let path = format!("{AUTOMATIONS_PATH}/{}/runs", encode_path_segment(name));
    get_json(&url(&path), token).await
}

/// `GET /automations/{name}/runs/{run_id}` — one run plus its ordered steps (the
/// audit trail of a single execution).
pub async fn get_automation_run(
    token: Option<&str>,
    name: &str,
    run_id: &str,
) -> Result<RunDetail, RestError> {
    let path = format!(
        "{AUTOMATIONS_PATH}/{}/runs/{}",
        encode_path_segment(name),
        encode_path_segment(run_id)
    );
    get_json(&url(&path), token).await
}

/// `POST /automations/{name}/collect` — "collect now" (SOUL §29): enqueue one
/// immediate poll of a Collect-headed automation, bypassing the trigger's `every`
/// cadence. Returns the enqueued job id (`202`); a `404` (automation absent) or `400`
/// (not a collect automation) surfaces as a [`RestError::Status`]. Gated
/// `automation:write` server-side.
pub async fn collect_now(token: Option<&str>, name: &str) -> Result<CollectNowResult, RestError> {
    let path = format!("{AUTOMATIONS_PATH}/{}/collect", encode_path_segment(name));
    post_json::<(), _>(&url(&path), token, None).await
}

/// `POST /triggers/{name}` — fire a named-signal `trigger` automation on demand
/// (SOUL §11). An optional JSON `payload` rides along as the run's trigger payload
/// (`None` → an empty body, treated as no payload). Returns how many automations
/// matched + their enqueued jobs (`202`); gated `automation:write` server-side.
pub async fn fire_trigger(
    token: Option<&str>,
    name: &str,
    payload: Option<&serde_json::Value>,
) -> Result<FireResult, RestError> {
    let path = format!("{TRIGGERS_PATH}/{}", encode_path_segment(name));
    post_json(&url(&path), token, payload).await
}

// --- Skills (SOUL §23) ------------------------------------------------------

/// `GET /skills` — this workspace's skills (by name).
pub async fn list_skills(token: Option<&str>) -> Result<Vec<Skill>, RestError> {
    get_json(&url(SKILLS_PATH), token).await
}

/// `POST /skills` — create a skill. Returns the created [`Skill`] (`409` if the
/// name already exists).
pub async fn create_skill(token: Option<&str>, body: &CreateSkill) -> Result<Skill, RestError> {
    post_json(&url(SKILLS_PATH), token, Some(body)).await
}

/// `PUT /skills/{name}` — create-or-replace the named skill. Returns the stored
/// [`Skill`].
pub async fn update_skill(
    token: Option<&str>,
    name: &str,
    body: &UpdateSkill,
) -> Result<Skill, RestError> {
    let path = format!("{SKILLS_PATH}/{}", encode_path_segment(name));
    put_json(&url(&path), token, body).await
}

/// `DELETE /skills/{name}` — delete a skill.
pub async fn delete_skill(token: Option<&str>, name: &str) -> Result<(), RestError> {
    let path = format!("{SKILLS_PATH}/{}", encode_path_segment(name));
    delete_resource(&url(&path), token).await
}

// --- Agent profiles (SOUL §19/§25) ------------------------------------------

/// `GET /tools` — the agent tool catalog (name + one-line description), for the
/// Profiles tools checklist. Global/static, so the result is workspace-independent.
pub async fn list_tools(token: Option<&str>) -> Result<Vec<ToolInfo>, RestError> {
    get_json(&url(TOOLS_PATH), token).await
}

/// `GET /agent-profiles` — this workspace's agent profiles (by name).
pub async fn list_agent_profiles(token: Option<&str>) -> Result<Vec<AgentProfile>, RestError> {
    get_json(&url(AGENT_PROFILES_PATH), token).await
}

/// `POST /agent-profiles` — create a profile. Returns the created
/// [`AgentProfile`] (`409` if the name already exists).
pub async fn create_agent_profile(
    token: Option<&str>,
    body: &CreateAgentProfile,
) -> Result<AgentProfile, RestError> {
    post_json(&url(AGENT_PROFILES_PATH), token, Some(body)).await
}

/// `PUT /agent-profiles/{name}` — create-or-replace the named profile.
pub async fn update_agent_profile(
    token: Option<&str>,
    name: &str,
    body: &UpdateAgentProfile,
) -> Result<AgentProfile, RestError> {
    let path = format!("{AGENT_PROFILES_PATH}/{}", encode_path_segment(name));
    put_json(&url(&path), token, body).await
}

/// `DELETE /agent-profiles/{name}` — delete a profile.
pub async fn delete_agent_profile(token: Option<&str>, name: &str) -> Result<(), RestError> {
    let path = format!("{AGENT_PROFILES_PATH}/{}", encode_path_segment(name));
    delete_resource(&url(&path), token).await
}

// --- Storage / Files (SOUL §9, M3) -----------------------------------------

/// `GET /storage/catalogue?prefix=…` — this workspace's catalogued objects
/// (Postgres truth), newest-modified first, each with its bucket name + §10
/// extracted-text link. Works regardless of whether a blob backend is currently
/// configured.
pub async fn list_catalogue(
    token: Option<&str>,
    prefix: &str,
) -> Result<Vec<StorageObject>, RestError> {
    let full = if prefix.is_empty() {
        url(STORAGE_CATALOGUE_PATH)
    } else {
        format!(
            "{}?prefix={}",
            url(STORAGE_CATALOGUE_PATH),
            encode_query_component(prefix)
        )
    };
    get_json(&full, token).await
}

/// `GET /storage/objects?store=&prefix=` — the raw objects on a store's **backend**
/// filesystem (not the catalogue), key-sorted, prefix-filtered (SOUL §9). This is
/// the source the Files panel builds its directory tree from, so a *browse* store's
/// pre-existing on-disk files are listed. Omitting `store` targets the default
/// store; bounded server-side at `DEFAULT_OBJECT_LIMIT` (1000) objects.
pub async fn list_objects(
    token: Option<&str>,
    store: &str,
    prefix: &str,
) -> Result<Vec<BackendObject>, RestError> {
    let mut full = append_store(
        &url(STORAGE_OBJECTS_PATH),
        (!store.is_empty()).then_some(store),
    );
    if !prefix.is_empty() {
        full = append_query(&full, "prefix", prefix);
    }
    get_json(&full, token).await
}

/// `GET /storage/catalogue/{id}/text` — the §10 extracted text for a catalogued
/// object (the read side of the Files panel's "Indexed ✓" badge). `has_text` is
/// false when the object isn't ingested yet; `text` is bounded server-side.
pub async fn object_text(token: Option<&str>, id: &str) -> Result<ObjectText, RestError> {
    let path = format!("{STORAGE_CATALOGUE_PATH}/{}/text", encode_path_segment(id));
    get_json(&url(&path), token).await
}

/// `GET /storage/catalogue/search?q=` — search objects by their §10 extracted-text
/// content; each hit carries a match-windowed excerpt. Only ingested objects match.
pub async fn search_objects(token: Option<&str>, query: &str) -> Result<Vec<ObjectHit>, RestError> {
    let path = format!(
        "{STORAGE_CATALOGUE_PATH}/search?q={}",
        encode_query_component(query)
    );
    get_json(&url(&path), token).await
}

/// `PUT /storage/objects/{key}` — upload an object's bytes (`storage:write`).
/// The server catalogues the blob, enqueues its §10 ingest, and fires
/// `StorageObject` automations; this client ignores the `UploadResult` body and
/// just reports success/failure (the panel reloads the listing afterward).
pub async fn upload_object(
    token: Option<&str>,
    key: &str,
    store: Option<&str>,
    bytes: Vec<u8>,
    content_type: Option<&str>,
) -> Result<(), RestError> {
    let path = format!("{STORAGE_OBJECTS_PATH}/{}", encode_key_path(key));
    let full = append_store(&url(&path), store);
    put_bytes(&full, token, content_type, &bytes).await
}

/// `DELETE /storage/objects/{key}?store=…` — remove an object from `store` (drops
/// the blob and its catalogue row). Idempotent server-side (204 whether or not it
/// existed). `store` selects the backend the object lives on; omitting it targets
/// the default store.
pub async fn delete_object(
    token: Option<&str>,
    key: &str,
    store: Option<&str>,
) -> Result<(), RestError> {
    let path = format!("{STORAGE_OBJECTS_PATH}/{}", encode_key_path(key));
    let full = append_store(&url(&path), store);
    delete_resource(&full, token).await
}

/// Build an authenticated absolute download URL for an object:
/// `GET /storage/objects/{key}?token=…`. A browser cannot attach an
/// `Authorization` header to a plain anchor navigation, so the dev token rides
/// as a query parameter — which the API's `Auth` extractor also accepts
/// (`token` / `access_token`). Returns the bare URL when no token is available.
#[must_use]
pub fn download_url(token: Option<&str>, key: &str, store: Option<&str>) -> String {
    let mut out = format!("{}/{}", url(STORAGE_OBJECTS_PATH), encode_key_path(key));
    if let Some(t) = token {
        if !t.is_empty() {
            out = append_query(&out, "token", t);
        }
    }
    if let Some(s) = store {
        if !s.is_empty() {
            out = append_query(&out, "store", s);
        }
    }
    out
}

/// Fetch a stored object's raw bytes over an authed GET (`GET
/// /storage/objects/{key}`) — the **same server surface** [`download_url`]
/// targets, but fetched so a non-2xx (e.g. the blob was pruned → `404`) is
/// observable and can be surfaced in-panel rather than navigating the browser to
/// a bare error page. `store` selects a non-default backend (`None` = default).
/// The token rides as a bearer header, like the other REST calls.
pub async fn fetch_object_bytes(
    token: Option<&str>,
    key: &str,
    store: Option<&str>,
) -> Result<Vec<u8>, RestError> {
    let path = format!("{STORAGE_OBJECTS_PATH}/{}", encode_key_path(key));
    let full = append_store(&url(&path), store);
    let mut req = Request::get(&full);
    if let Some(tok) = token {
        if !tok.is_empty() {
            req = req.header("Authorization", &format!("Bearer {tok}"));
        }
    }
    let resp = req
        .send()
        .await
        .map_err(|e| RestError::Transport(e.to_string()))?;
    let status = resp.status();
    if !(200..300).contains(&status) {
        let body = resp.text().await.unwrap_or_default();
        return Err(status_error(status, &body, token));
    }
    resp.binary()
        .await
        .map_err(|e| RestError::Transport(e.to_string()))
}

/// Append a `?store=` (or `&store=`) query parameter when `store` is a non-empty
/// name; otherwise return `base` unchanged. The destination-backend selector for
/// the per-object blob routes (SOUL §9).
fn append_store(base: &str, store: Option<&str>) -> String {
    match store {
        Some(s) if !s.is_empty() => append_query(base, "store", s),
        _ => base.to_string(),
    }
}

/// Append a `key=value` query parameter to `url`, percent-encoding the value and
/// choosing `?` or `&` based on whether the URL already has a query.
fn append_query(url: &str, key: &str, value: &str) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}{key}={}", encode_query_component(value))
}

// --- Storage backends ("stores", SOUL §9) ----------------------------------

/// `GET /storage/stores` — the workspace's storage backends (config-defined +
/// runtime), for the Files destination picker + the storage manager.
pub async fn list_stores(token: Option<&str>) -> Result<Vec<StorageStore>, RestError> {
    get_json(&url(STORAGE_STORES_PATH), token).await
}

/// `POST /storage/stores` — add a runtime storage backend (`storage:write`).
pub async fn create_store(
    token: Option<&str>,
    body: &NewStorageStore,
) -> Result<StorageStore, RestError> {
    post_json(&url(STORAGE_STORES_PATH), token, Some(body)).await
}

/// `DELETE /storage/stores/{name}` — remove a runtime storage backend
/// (`storage:write`). The blobs on the backend are left intact.
pub async fn delete_store(token: Option<&str>, name: &str) -> Result<(), RestError> {
    let path = format!("{STORAGE_STORES_PATH}/{}", encode_path_segment(name));
    delete_resource(&url(&path), token).await
}

/// `POST /storage/stores/{name}/scan` — reconcile a store's catalogue with its
/// backend (`storage:write`): index new/changed files, purge vanished ones. Returns
/// a [`ScanReport`]; indexing then runs asynchronously server-side.
pub async fn scan_store(token: Option<&str>, name: &str) -> Result<ScanReport, RestError> {
    let path = format!("{STORAGE_STORES_PATH}/{}/scan", encode_path_segment(name));
    post_json::<(), ScanReport>(&url(&path), token, None).await
}

// --- Labels on files & directories (SOUL §9) -------------------------------

/// `GET /storage/labels?store=&prefix=` — a store's labels (`storage:read`), for
/// the Files panel's tree badges. `store` selects the backend (empty → the
/// default store); `prefix` restricts to paths under it (empty → the whole store).
pub async fn list_labels(
    token: Option<&str>,
    store: &str,
    prefix: &str,
) -> Result<Vec<FileLabel>, RestError> {
    let mut full = url(STORAGE_LABELS_PATH);
    if !store.is_empty() {
        full = append_query(&full, "store", store);
    }
    if !prefix.is_empty() {
        full = append_query(&full, "prefix", prefix);
    }
    get_json(&full, token).await
}

/// `POST /storage/labels` — apply a label to a file or directory path
/// (`storage:write`). Idempotent server-side; returns the (possibly pre-existing)
/// [`FileLabel`].
pub async fn add_label(token: Option<&str>, body: &NewFileLabel) -> Result<FileLabel, RestError> {
    post_json(&url(STORAGE_LABELS_PATH), token, Some(body)).await
}

/// `DELETE /storage/labels/{id}` — remove a label by id (`storage:write`).
pub async fn delete_label(token: Option<&str>, id: &str) -> Result<(), RestError> {
    let path = format!("{STORAGE_LABELS_PATH}/{}", encode_path_segment(id));
    delete_resource(&url(&path), token).await
}

/// Percent-encode a storage key as a URL **path**, preserving `/` as segment
/// separators (the blob route is `/storage/objects/{*key}`, a wildcard capture).
/// Each segment is escaped like a query component so a stray character can never
/// break out of the path; leading/trailing slashes are trimmed (the API trims
/// them too).
fn encode_key_path(key: &str) -> String {
    key.trim_matches('/')
        .split('/')
        .filter(|seg| !seg.is_empty())
        .map(encode_query_component)
        .collect::<Vec<_>>()
        .join("/")
}

/// Percent-encode a value for safe inclusion in a query string (same policy as
/// [`crate::ws`]'s encoder: everything outside the unreserved set is escaped).
fn encode_query_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(hex_nibble(b >> 4));
                out.push(hex_nibble(b & 0x0f));
            }
        }
    }
    out
}

/// Percent-encode a single path segment (a connection id). Ids are UUID strings,
/// but encode defensively so a stray character can never break the path.
fn encode_path_segment(s: &str) -> String {
    encode_query_component(s)
}

/// Percent-encode a slash-namespaced resource name while retaining its path
/// separators. OpenRouter model ids use the canonical `author/model` shape and
/// the llmleaf topology endpoint captures the complete trailing path.
fn encode_path_tail(s: &str) -> String {
    s.split('/')
        .map(encode_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'A' + (n - 10)) as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // url() resolves the origin via api_base(), which falls back to API_BASE off-wasm.
    use crate::api::API_BASE;

    #[test]
    fn url_joins_root_path() {
        assert_eq!(url("/events"), format!("{API_BASE}/events"));
    }

    #[test]
    fn extracts_api_error_message() {
        assert_eq!(
            error_message(r#"{"error":"not found","kind":"not_found"}"#),
            "not found"
        );
        // Falls back to the raw body when not the contract shape.
        assert_eq!(error_message("boom"), "boom");
    }

    #[test]
    fn status_error_display() {
        let e = RestError::Status {
            status: 404,
            message: "not found".to_string(),
        };
        assert_eq!(e.to_string(), "not found (HTTP 404)");
    }

    #[test]
    fn encodes_query_component() {
        assert_eq!(
            encode_query_component("2026-06-13T09:00:00Z"),
            "2026-06-13T09%3A00%3A00Z"
        );
    }

    #[test]
    fn encode_key_path_preserves_slashes_but_escapes_segments() {
        // Slashes stay as separators; other reserved chars are escaped per segment.
        assert_eq!(encode_key_path("docs/report.pdf"), "docs/report.pdf");
        assert_eq!(encode_key_path("a b/c?d"), "a%20b/c%3Fd");
        // Leading/trailing/duplicate slashes are normalized away.
        assert_eq!(encode_key_path("/docs//x/"), "docs/x");
    }

    #[test]
    fn encode_path_tail_preserves_openrouter_namespace() {
        assert_eq!(
            encode_path_tail("deepseek/deepseek-v4-pro"),
            "deepseek/deepseek-v4-pro"
        );
        assert_eq!(encode_path_tail("vendor/a model"), "vendor/a%20model");
    }

    #[test]
    fn download_url_attaches_token_and_encodes_key() {
        assert_eq!(
            download_url(Some("tok 1"), "docs/a.txt", None),
            format!("{API_BASE}/storage/objects/docs/a.txt?token=tok%201")
        );
        // No token → a bare URL (still a valid link; the API 401s without auth).
        assert_eq!(
            download_url(None, "a.txt", None),
            format!("{API_BASE}/storage/objects/a.txt")
        );
        assert_eq!(
            download_url(Some(""), "a.txt", None),
            format!("{API_BASE}/storage/objects/a.txt")
        );
    }

    #[test]
    fn download_url_appends_store_after_token() {
        // Store rides as a second query parameter (`&` after the token's `?`).
        assert_eq!(
            download_url(Some("t"), "a.txt", Some("minio")),
            format!("{API_BASE}/storage/objects/a.txt?token=t&store=minio")
        );
        // Store alone uses `?`; an empty store is omitted.
        assert_eq!(
            download_url(None, "a.txt", Some("s3 prod")),
            format!("{API_BASE}/storage/objects/a.txt?store=s3%20prod")
        );
        assert_eq!(
            download_url(None, "a.txt", Some("")),
            format!("{API_BASE}/storage/objects/a.txt")
        );
    }
}
