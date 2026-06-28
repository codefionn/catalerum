//! Emerged-UI REST (SOUL §12 — the "emerged UI" feature).
//!
//! Read surface over the workspace's AI-authored declarative UIs
//! ([`UiDefinition`]). The web app fetches a definition here to mount the
//! interpreter (inline-in-chat replay and, later, the standalone Apps panel).
//! All routes are workspace-scoped to the authenticated principal (the client
//! never names a workspace, SOUL §18) and capability-gated ([`Auth::require`],
//! SOUL §19): reads need `ui:read` (every role).
//!
//! Authoring (create / patch) happens through the LLM tools
//! (`present_ui` / `create_ui_components` / `edit_ui_components` / `edit_ui`,
//! see [`crate::tools`]) so it is
//! transcript-native and carries no raw markup; a full REST write surface for
//! the Apps panel lands with that phase. Deletion exists on both surfaces:
//! the `delete_ui` tool for the transcript and `DELETE /uis/{id}` for the
//! Apps panel's per-row delete — same `ui:write` gate either way.
//!
//! Routes:
//! - `GET    /uis`          list this workspace's UIs (most-recently-edited first)
//! - `GET    /uis/{id}`     fetch one UI's full definition
//! - `DELETE /uis/{id}`     remove one UI (`ui:write`)
//! - `POST   /uis/{id}/event`  fire a node's handler (tool / Boa script) and return
//!   the [`UiAction`](catalerum_core::model_ui::UiAction)s to apply (SOUL §12)

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use catalerum_core::capability::Action;
use catalerum_core::model::UiDefinition;
use catalerum_core::model_ui::{EventName, NodeKind};
use catalerum_core::tool::ToolContext;
use catalerum_core::UiDefinitionId;

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::ui_runtime;

/// Mount the emerged-UI routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/uis", get(list))
        .route("/uis/{id}", get(get_one).delete(delete_one))
        .route("/uis/by-name/{name}", get(get_by_name))
        .route("/uis/{id}/event", post(event))
        .route("/uis/{id}/validate", post(validate))
        .route("/uis/{id}/compute", post(compute))
        .route("/uis/{id}/image/{node_id}", get(db_image))
}

async fn list(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<UiDefinition>>> {
    auth.require(Action::Read, "ui")?;
    let ws = auth.principal().workspace_id;
    let uis = state.store().ui_definitions().list_by_workspace(ws).await?;
    Ok(Json(uis))
}

async fn get_one(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<UiDefinitionId>,
) -> ApiResult<Json<UiDefinition>> {
    auth.require(Action::Read, "ui")?;
    let ws = auth.principal().workspace_id;
    let def = state.store().ui_definitions().get(ws, id).await?;
    Ok(Json(def))
}

/// `DELETE /uis/{id}` — remove one emerged UI, workspace-scoped. The same
/// `ui:write` gate as the `delete_ui` tool (deletion is a write, like
/// `delete_note`); 404 when the id is absent from this workspace.
async fn delete_one(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<UiDefinitionId>,
) -> ApiResult<StatusCode> {
    auth.require(Action::Write, "ui")?;
    let ws = auth.principal().workspace_id;
    state.store().ui_definitions().delete(ws, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /uis/by-name/{name}` — one UI's full definition by its `present_ui`
/// name slug (workspace-unique). The mount path for an `app_ref` whose target
/// is a name rather than a ui id.
async fn get_by_name(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
) -> ApiResult<Json<UiDefinition>> {
    auth.require(Action::Read, "ui")?;
    let ws = auth.principal().workspace_id;
    let def = state
        .store()
        .ui_definitions()
        .get_by_name(ws, name.trim())
        .await?;
    Ok(Json(def))
}

/// Body for `POST /uis/{id}/event`: the fired node, the event, and the client's
/// full transient-state snapshot (so a handler sees in-progress fields, not a
/// stale row), plus the firing node's `for_each` bindings (`{}` when none).
#[derive(Debug, Deserialize)]
pub struct UiEvent {
    /// The id of the node whose handler fired.
    pub node_id: String,
    /// The event that fired (`click`, `submit`, …).
    pub event: EventName,
    /// The client's transient state at fire time.
    #[serde(default)]
    pub state: Value,
    /// `for_each` item/index bindings in scope at the fired node.
    #[serde(default)]
    pub scope: Value,
}

/// Fire a node handler (SOUL §12). Gated on `ui:read` (every role may *run* a UI);
/// the handler's own side effects are then gated on the firing user's capability
/// set + the `[ui].handler_tools` allow-list inside [`ui_runtime::run_handler`].
/// Returns the [`UiAction`](catalerum_core::model_ui::UiAction)s to apply.
async fn event(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<UiDefinitionId>,
    Json(body): Json<UiEvent>,
) -> ApiResult<Json<Vec<Value>>> {
    auth.require(Action::Read, "ui")?;
    let principal = auth.principal();
    let def = state
        .store()
        .ui_definitions()
        .get(principal.workspace_id, id)
        .await?;

    let disp = dispatcher(&state, &auth, id);
    let (mut actions, final_state) = ui_runtime::run_handler(
        &disp,
        &def.definition,
        &body.node_id,
        body.event,
        body.state,
        body.scope,
    )
    .await?;

    // Piggyback refreshed `computed.*` values onto the response (SOUL §12): one
    // `set computed` action the client applies like any other.
    if !def.definition.computed.is_empty() {
        let computed = ui_runtime::run_computed(&disp, &def.definition, final_state).await?;
        actions.push(serde_json::json!({ "op": "set", "path": "computed", "value": computed }));
    }
    Ok(Json(actions))
}

/// Body for `POST /uis/{id}/validate`: the named [`ValidationKind::Script`] rule,
/// the field value to check, and the transient-state snapshot.
///
/// [`ValidationKind::Script`]: catalerum_core::model_ui::ValidationKind::Script
#[derive(Debug, Deserialize)]
pub struct UiValidate {
    /// A key into the spec's `scripts` (the validation handler).
    pub handler: String,
    /// The field's current value.
    #[serde(default)]
    pub value: Value,
    /// The client's transient state at validation time.
    #[serde(default)]
    pub state: Value,
}

/// Run a script validation rule (SOUL §12), returning `{ ok, message? }`. Same
/// gate as firing a handler (`ui:read` + the firing user's grant).
async fn validate(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<UiDefinitionId>,
    Json(body): Json<UiValidate>,
) -> ApiResult<Json<Value>> {
    auth.require(Action::Read, "ui")?;
    let principal = auth.principal();
    let def = state
        .store()
        .ui_definitions()
        .get(principal.workspace_id, id)
        .await?;
    let result = ui_runtime::run_validation(
        &dispatcher(&state, &auth, id),
        &def.definition,
        &body.handler,
        body.value,
        body.state,
    )
    .await?;
    Ok(Json(result))
}

/// Body for `POST /uis/{id}/compute`: the transient-state snapshot to derive the
/// `computed.*` values from (on mount, the spec's `initial_state`).
#[derive(Debug, Deserialize)]
pub struct UiCompute {
    /// The client's transient state.
    #[serde(default)]
    pub state: Value,
}

/// Evaluate the UI's `computed.*` derived values against `state` (SOUL §12),
/// returning the `{ name: value }` object. The client calls this on mount; later
/// refreshes piggyback on `/event` responses.
async fn compute(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<UiDefinitionId>,
    Json(body): Json<UiCompute>,
) -> ApiResult<Json<Value>> {
    auth.require(Action::Read, "ui")?;
    let principal = auth.principal();
    let def = state
        .store()
        .ui_definitions()
        .get(principal.workspace_id, id)
        .await?;
    let computed =
        ui_runtime::run_computed(&dispatcher(&state, &auth, id), &def.definition, body.state)
            .await?;
    Ok(Json(computed))
}

/// Query for `GET /uis/{id}/image/{node_id}`: `p` is a JSON array of the
/// client-resolved bind-parameter values for the node's authored SQL (the
/// `{{path}}` templates in `props.db.params`, resolved against live state).
#[derive(Debug, Default, Deserialize)]
pub struct DbImageQuery {
    #[serde(default)]
    p: Option<String>,
}

/// Most bind parameters an image query may carry (the authored `db.params` list
/// is tiny — a row id, maybe a size variant).
const MAX_IMAGE_PARAMS: usize = 16;
/// Largest image payload served (10 MiB) — bounds the decode + response body.
const MAX_IMAGE_BYTES: usize = 10 << 20;

/// `GET /uis/{id}/image/{node_id}?p=[…]` — serve an image stored in an external
/// database (SOUL §11/§12): the `image` node's **spec-held** `props.db`
/// (`{connection, sql, params?, column?}`) names the query; only its bind
/// values come from the URL, so a client can never run SQL the author didn't
/// write. The query is dispatched through the regular `sql_query` tool under
/// the same gates as any UI handler (the `[ui].handler_tools` allow-list + the
/// caller's capped capabilities, so `db:read@<conn>` is enforced), forced to
/// `mode=read`. The one returned cell is decoded (Postgres `\x…` bytea hex,
/// base64, or a `data:` URL) and served **only** when it sniffs as a raster
/// image (png/jpeg/gif/webp) — never as HTML/SVG — with `nosniff`.
async fn db_image(
    State(state): State<AppState>,
    auth: Auth,
    Path((id, node_id)): Path<(UiDefinitionId, String)>,
    Query(q): Query<DbImageQuery>,
) -> ApiResult<Response> {
    auth.require(Action::Read, "ui")?;
    let principal = auth.principal();
    let def = state
        .store()
        .ui_definitions()
        .get(principal.workspace_id, id)
        .await?;

    let node = ui_runtime::find_node_in_spec(&def.definition, &node_id)
        .ok_or_else(|| ApiError::bad_request(format!("no node `{node_id}` in this UI")))?;
    if node.kind != NodeKind::Image {
        return Err(ApiError::bad_request(format!(
            "node `{node_id}` is not an image"
        )));
    }
    let db = node
        .props
        .get("db")
        .and_then(Value::as_object)
        .ok_or_else(|| ApiError::bad_request(format!("image `{node_id}` has no `db` source")))?;
    let connection = db.get("connection").and_then(Value::as_str).unwrap_or("");
    let sql = db.get("sql").and_then(Value::as_str).unwrap_or("");
    let column = db.get("column").and_then(Value::as_str);

    // Bind values from the URL — values only, capped; the SQL is spec-held.
    let params: Vec<Value> = match q.p.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Vec::new(),
        Some(raw) => serde_json::from_str::<Vec<Value>>(raw)
            .map_err(|e| ApiError::bad_request(format!("`p` must be a JSON array: {e}")))?,
    };
    if params.len() > MAX_IMAGE_PARAMS {
        return Err(ApiError::bad_request(format!(
            "too many image parameters (max {MAX_IMAGE_PARAMS})"
        )));
    }

    // Same trust boundary as firing a handler: the UI allow-list + the caller's
    // capped context (which `sql_query` itself narrows to `db:read@<conn>`).
    let disp = dispatcher(&state, &auth, id);
    ui_runtime::gate_dispatchable(&disp.registry, "sql_query", disp.allow.as_ref(), false)?;
    let result = disp
        .registry
        .dispatch(
            "sql_query",
            serde_json::json!({
                "connection": connection,
                "sql": sql,
                "params": params,
                "mode": "read",
                "max_rows": 1,
            }),
            &disp.ctx,
        )
        .await?;

    let row = result
        .get("rows")
        .and_then(Value::as_array)
        .and_then(|r| r.first())
        .ok_or(ApiError::NotFound)?;
    let cell = image_cell(row, column).ok_or_else(|| {
        ApiError::bad_request(
            "the image query must return one column (or name it via `db.column`)".to_string(),
        )
    })?;
    let (bytes, declared) = decode_image_cell(cell).ok_or_else(|| {
        ApiError::bad_request("the image cell is not decodable image data".to_string())
    })?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(ApiError::bad_request(format!(
            "image too large ({} bytes > {MAX_IMAGE_BYTES})",
            bytes.len()
        )));
    }
    // Raster-only: refuse anything that does not sniff as an image, whatever the
    // data-URL claimed — this endpoint must never serve markup (SVG/HTML → XSS).
    let content_type = sniff_raster(&bytes).ok_or_else(|| {
        ApiError::bad_request(format!(
            "unsupported image format{}",
            declared
                .map(|d| format!(" (declared `{d}`)"))
                .unwrap_or_default()
        ))
    })?;
    Ok((
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::CONTENT_DISPOSITION, "inline".to_string()),
            (header::CACHE_CONTROL, "private, max-age=60".to_string()),
        ],
        bytes,
    )
        .into_response())
}

/// Pick the image cell out of the one result row: the named `column` when the
/// spec sets one, else the row's single column (`None` when ambiguous/missing).
fn image_cell<'a>(row: &'a Value, column: Option<&str>) -> Option<&'a Value> {
    let obj = row.as_object()?;
    match column {
        Some(c) => obj.get(c),
        None if obj.len() == 1 => obj.values().next(),
        None => None,
    }
}

/// Decode an image cell into raw bytes plus any declared media type: Postgres
/// `\x…` bytea hex (what `to_jsonb` emits for `bytea`), a `data:…;base64,` URL,
/// or plain base64 text.
fn decode_image_cell(cell: &Value) -> Option<(Vec<u8>, Option<String>)> {
    use base64::Engine as _;
    let s = cell.as_str()?.trim();
    if let Some(hex) = s.strip_prefix("\\x") {
        return decode_hex(hex).map(|b| (b, None));
    }
    if let Some(rest) = s.strip_prefix("data:") {
        let (mime, payload) = rest.split_once(";base64,")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(payload.trim())
            .ok()?;
        return Some((bytes, Some(mime.to_string())));
    }
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .ok()
        .map(|b| (b, None))
}

/// Decode a lowercase/uppercase hex string into bytes (`None` on any non-hex
/// character or odd length).
fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

/// The media type of a raster image by magic bytes — the only formats this
/// endpoint will serve (no SVG/HTML, which could carry script).
fn sniff_raster(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Build the per-request [`Dispatcher`](ui_runtime::Dispatcher): the registry, the
/// `[ui].handler_tools` allow-list, and a [`ToolContext`] capped to the firing
/// token's **effective authority** — the grant's capabilities when the bearer is
/// grant-scoped (SOUL §19/§26), else the role's base set. A UI is never more
/// powerful than the same principal in chat, and a scoped token's UI handlers are
/// bounded by its grant just like its tool calls (SOUL §13/§19).
///
/// `ui_id` is the firing App: it rides in the [`ToolContext`] so the per-App
/// key/value tools (`app_data_*`, SOUL §12/§29) scope to *this* App's namespace
/// and cannot reach another App's keys (the namespace is not a caller argument on
/// the handler path).
fn dispatcher(state: &AppState, auth: &Auth, ui_id: UiDefinitionId) -> ui_runtime::Dispatcher {
    let principal = auth.principal();
    ui_runtime::Dispatcher {
        registry: state.registry().clone(),
        allow: Arc::new(state.config().ui.handler_tools.iter().cloned().collect()),
        ctx: ToolContext {
            workspace_id: Some(principal.workspace_id),
            user_id: Some(principal.user_id),
            agent_id: None,
            grant_id: None,
            capabilities: Some(auth.capabilities()),
            dry_run: false,
            gate: None,
            conversation_id: None,
            ui_id: Some(ui_id),
            registry: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn image_cell_by_column_or_single() {
        let row = json!({ "photo": "abc" });
        assert_eq!(image_cell(&row, None), Some(&json!("abc")));
        assert_eq!(image_cell(&row, Some("photo")), Some(&json!("abc")));
        assert_eq!(image_cell(&row, Some("missing")), None);
        // Two columns without a `column` selector is ambiguous.
        let two = json!({ "a": 1, "b": 2 });
        assert_eq!(image_cell(&two, None), None);
        assert_eq!(image_cell(&two, Some("b")), Some(&json!(2)));
    }

    #[test]
    fn decode_cell_hex_base64_and_data_url() {
        use base64::Engine as _;
        let png = [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2];
        // Postgres `to_jsonb(bytea)` form: \x-prefixed hex.
        let hex = format!(
            "\\x{}",
            png.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        assert_eq!(decode_image_cell(&json!(hex)).unwrap().0, png.to_vec());
        // Plain base64.
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        assert_eq!(decode_image_cell(&json!(b64)).unwrap().0, png.to_vec());
        // A data URL keeps its declared mime.
        let (bytes, mime) =
            decode_image_cell(&json!(format!("data:image/png;base64,{b64}"))).unwrap();
        assert_eq!(bytes, png.to_vec());
        assert_eq!(mime.as_deref(), Some("image/png"));
        // Garbage decodes to None (odd hex, non-base64, non-strings).
        assert!(decode_image_cell(&json!("\\xabc")).is_none());
        assert!(decode_image_cell(&json!("not base64 !!")).is_none());
        assert!(decode_image_cell(&json!(42)).is_none());
    }

    #[test]
    fn sniff_only_rasters() {
        assert_eq!(
            sniff_raster(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0]),
            Some("image/png")
        );
        assert_eq!(sniff_raster(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_raster(b"GIF89a...."), Some("image/gif"));
        let mut webp = b"RIFF\x00\x00\x00\x00WEBPVP8 ".to_vec();
        assert_eq!(sniff_raster(&webp), Some("image/webp"));
        webp[8] = b'X';
        assert_eq!(sniff_raster(&webp), None);
        // SVG/HTML must never be served from here.
        assert_eq!(sniff_raster(b"<svg xmlns=..."), None);
        assert_eq!(sniff_raster(b"<!doctype html>"), None);
    }
}
