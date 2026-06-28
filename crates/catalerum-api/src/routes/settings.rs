//! Per-user LLM settings REST (SOUL §7/§13) — choose the chat model and the
//! speech (TTS) model + voice from the workbench, layered over the immutable
//! `[llm]` config base (principle 10).
//!
//! The `[llm]` TOML block sets the boot-time defaults; this surface lets a user
//! override them at runtime (stored in `llm_settings`, keyed `(workspace, user)`).
//! An unset field falls back to the config default, so a blank choice is sent /
//! stored as `None`. The selection is kept apart from the [`Profile`] so it never
//! leaks into the chat system prompt.
//!
//! Two companion **read-only catalog** routes feed the UI's autocomplete: the
//! gateway's full model list and a speech model's voice list. Like
//! `GET /status`, they're authenticated but carry no secret and gate on no
//! capability — listing the gateway's offerings is the same trust level as
//! showing its config.
//!
//! Routes:
//! - `GET /llm-settings`              — the caller's selections (an empty record if unset)
//! - `PUT /llm-settings`              — replace the caller's selections (blank field clears it)
//! - `GET /llm-models?search=`        — the gateway model catalog (autocomplete source)
//! - `GET /llm-voices?model=`         — a speech model's voices (autocomplete source)
//! - `GET /search-settings`           — the caller's default web-search provider (empty if unset)
//! - `PUT /search-settings`           — set/clear the caller's default web-search provider
//! - `GET /search-providers`          — the web-search provider catalog (SOUL §27)
//!
//! Settings reads require `profile:read` (every role); writes require
//! `profile:write` (a Viewer is `403`) — the selection is part of the caller's
//! personalization record, same trust domain as the profile (SOUL §19/§22). The
//! `[search]` provider API keys are **never** part of this surface — they are
//! billed server-side secrets that live only in config/env (SOUL §13); the search
//! catalog exposes only provider names, enabled-ness, and the caller's default.

use axum::extract::{Query, State};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use catalerum_core::capability::Action;
use catalerum_core::error::Error;
use catalerum_core::model::{
    default_voice_input_speed, LlmSettings, SearchSettings, StorageSettings,
};
use catalerum_llm::catalog::{ModelInfo, ModelKind, VoiceInfo};

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::model_validation::{validate_model, validate_voice};
use crate::state::AppState;

/// Mount the LLM-settings + catalog routes, plus the web-search settings/catalog.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/llm-settings", get(get_settings).put(set_settings))
        .route(
            "/llm-settings/image-models",
            put(set_image_input_models_route),
        )
        .route("/llm-models", get(list_models))
        .route("/llm-voices", get(list_voices))
        .route(
            "/search-settings",
            get(get_search_settings).put(set_search_settings),
        )
        .route("/search-providers", get(list_search_providers))
        .route(
            "/storage-settings",
            get(get_storage_settings).put(set_storage_settings),
        )
}

/// Body for `PUT /llm-settings` — the overridable `[llm]` selections plus browser
/// microphone compression. Each model/voice is optional; an absent (or blank)
/// field clears that selection (→ config default). A full replacement: a model
/// field not sent is treated as cleared. Older clients omitting the speed get
/// the 1.5× default.
#[derive(Debug, Default, Deserialize)]
pub struct UpdateLlmSettings {
    #[serde(default)]
    pub chat_model: Option<String>,
    #[serde(default)]
    pub speech_model: Option<String>,
    #[serde(default)]
    pub speech_voice: Option<String>,
    #[serde(default)]
    pub transcription_model: Option<String>,
    /// Time-compress browser microphone recordings before STT (1.0–2.0).
    #[serde(default = "default_voice_input_speed")]
    pub voice_input_speed: f32,
    #[serde(default)]
    pub ocr_model: Option<String>,
}

/// Query for `GET /llm-models` — an optional substring `search` plus an optional
/// model-type `kind` (`llm` / `tts` / `stt` / `embedding` / `all`). Each picker
/// requests its own kind so the speech field lists pure TTS models and the
/// transcription field pure STT models, rather than one mixed catalog; an absent
/// or unknown `kind` lists the full catalog.
#[derive(Debug, Default, Deserialize)]
pub struct ModelsQuery {
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

/// Map a `kind` query value to a [`ModelKind`]; absent or unrecognized → the full
/// catalog ([`ModelKind::All`]).
fn model_kind(kind: &Option<String>) -> ModelKind {
    match kind.as_deref().map(str::trim) {
        Some("llm" | "chat") => ModelKind::Chat,
        Some("tts") => ModelKind::Tts,
        Some("stt") => ModelKind::Stt,
        Some("embedding") => ModelKind::Embedding,
        _ => ModelKind::All,
    }
}

/// Query for `GET /llm-voices` — the speech model whose voices to list; absent
/// falls back to the `[llm].speech_model` config default.
#[derive(Debug, Default, Deserialize)]
pub struct VoicesQuery {
    #[serde(default)]
    pub model: Option<String>,
}

/// Trim an optional string and collapse blank/whitespace-only to `None`, so the
/// store never holds an empty-string selection (which would shadow the config
/// default with `""`).
fn normalize(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

async fn get_settings(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<LlmSettings>> {
    let p = auth.principal();
    auth.require(Action::Read, "profile")?;
    let settings = state
        .store()
        .llm_settings()
        .get(p.workspace_id, p.user_id)
        .await?;
    Ok(Json(settings))
}

async fn set_settings(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<UpdateLlmSettings>,
) -> ApiResult<Json<LlmSettings>> {
    let p = auth.principal();
    auth.require(Action::Write, "profile")?;
    if !body.voice_input_speed.is_finite() || !(1.0..=2.0).contains(&body.voice_input_speed) {
        return Err(ApiError::bad_request(
            "voice_input_speed must be between 1.0 and 2.0",
        ));
    }
    // Validate each chosen model id against the gateway catalog (like
    // `set_search_settings` validates the provider) so a typo can't silently
    // persist and fail at chat/speech time. Best-effort: a gateway outage doesn't
    // block the save.
    if let Some(m) = normalize(&body.chat_model) {
        validate_model(&state, "chat", m, ModelKind::Chat).await?;
    }
    if let Some(m) = normalize(&body.speech_model) {
        validate_model(&state, "speech", m, ModelKind::Tts).await?;
    }
    if let Some(m) = normalize(&body.transcription_model) {
        validate_model(&state, "transcription", m, ModelKind::Stt).await?;
    }
    // Vision models are chat models whose catalog entry advertises `image` input
    // — there is no separate OCR kind, so validate against the chat catalog.
    if let Some(m) = normalize(&body.ocr_model) {
        validate_model(&state, "ocr", m, ModelKind::Chat).await?;
    }
    // The voice is per speech model, so validate it against the model being set
    // (else the `[llm].speech_model` default it will fall back to). Skipped when no
    // speech model resolves (nothing concrete to query voices for).
    if let Some(voice) = normalize(&body.speech_voice) {
        let model = normalize(&body.speech_model)
            .map(str::to_string)
            .unwrap_or_else(|| state.config().llm.speech_model.clone());
        if !model.trim().is_empty() {
            validate_voice(&state, &model, voice).await?;
        }
    }
    let settings = state
        .store()
        .llm_settings()
        .set(
            p.workspace_id,
            p.user_id,
            normalize(&body.chat_model),
            normalize(&body.speech_model),
            normalize(&body.speech_voice),
            normalize(&body.transcription_model),
            body.voice_input_speed,
            normalize(&body.ocr_model),
        )
        .await?;
    Ok(Json(settings))
}

/// Body for `PUT /llm-settings/image-models` (SOUL §7/§9) — the per-user list of
/// model ids to force-treat as accepting image input, a full replacement.
#[derive(Debug, Default, Deserialize)]
pub struct UpdateImageInputModels {
    #[serde(default)]
    pub models: Vec<String>,
}

/// `PUT /llm-settings/image-models` — replace the user's force-image-input list.
/// A dedicated route (not part of `set_settings`) so the chat sidebar toggle and
/// the settings panel can edit just this list without a full `LlmSettings`
/// round-trip, and so the two writers never clobber each other's columns.
async fn set_image_input_models_route(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<UpdateImageInputModels>,
) -> ApiResult<Json<LlmSettings>> {
    let p = auth.principal();
    auth.require(Action::Write, "profile")?;
    // Trim, drop blanks, and dedupe so the stored list is clean. No catalog
    // validation on purpose: the whole point is to force a model the catalog
    // under-reports, so an id the catalog doesn't enumerate is legitimate here.
    let mut models: Vec<String> = body
        .models
        .into_iter()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect();
    models.sort();
    models.dedup();
    let settings = state
        .store()
        .llm_settings()
        .set_image_input_models(p.workspace_id, p.user_id, &models)
        .await?;
    Ok(Json(settings))
}

async fn list_models(
    State(state): State<AppState>,
    _auth: Auth,
    Query(q): Query<ModelsQuery>,
) -> ApiResult<Json<Vec<ModelInfo>>> {
    let search = normalize(&q.search);
    // Filter by the requested kind so each picker gets the right class of model —
    // pure TTS for the speech field, pure STT for transcription. STT/TTS-only
    // models (e.g. `voxtral-mini-tts`) live only under their `type`, so the kind
    // filter is the only way to surface them.
    let models = state.llm().list_models(model_kind(&q.kind), search).await?;
    Ok(Json(models))
}

async fn list_voices(
    State(state): State<AppState>,
    _auth: Auth,
    Query(q): Query<VoicesQuery>,
) -> ApiResult<Json<Vec<VoiceInfo>>> {
    // A specific speech model, else the configured default — voices are
    // per-model, so we always query against a concrete model id.
    let model = normalize(&q.model)
        .map(str::to_string)
        .unwrap_or_else(|| state.config().llm.speech_model.clone());
    // Voices are a best-effort autocomplete aid: a model with no voice list (the
    // gateway answers 404/502 for non-TTS or voiceless models) yields an empty
    // list, never a 500 — the picker just falls back to free text. The failure is
    // still logged: an empty picker for a model that HAS voices is otherwise
    // indistinguishable from a healthy voiceless one.
    let voices = state.llm().voices(&model).await.unwrap_or_else(|e| {
        tracing::warn!(model = %model, error = %e, "llm-voices lookup failed; returning empty list");
        Vec::new()
    });
    Ok(Json(voices))
}

// ---------------------------------------------------------------------------
// Web search settings + catalog (SOUL §27)
// ---------------------------------------------------------------------------

/// One row of the search-providers catalog the UI renders. Carries no secret —
/// just which engines exist, which are configured (`enabled`), and which is *this
/// caller's* effective default (their per-user override, else `[search].backend`).
/// Provider API keys live only in server config (SOUL §13) and are never exposed.
#[derive(Debug, Serialize)]
pub struct SearchProviderInfo {
    /// Provider id (`brave`, `tavily`, …).
    pub name: String,
    /// Whether the provider is configured server-side (its credential is set).
    pub enabled: bool,
    /// Whether this provider is the caller's effective default.
    pub is_default: bool,
}

/// Body for `PUT /search-settings` — the per-user default-provider override. An
/// absent/blank field clears the override (→ the `[search].backend` config
/// default). A blank choice from the UI is sent as `null`.
#[derive(Debug, Default, Deserialize)]
pub struct UpdateSearchSettings {
    #[serde(default)]
    pub default_provider: Option<String>,
}

async fn get_search_settings(
    State(state): State<AppState>,
    auth: Auth,
) -> ApiResult<Json<SearchSettings>> {
    let p = auth.principal();
    auth.require(Action::Read, "profile")?;
    let settings = state
        .store()
        .search_settings()
        .get(p.workspace_id, p.user_id)
        .await?;
    Ok(Json(settings))
}

async fn set_search_settings(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<UpdateSearchSettings>,
) -> ApiResult<Json<SearchSettings>> {
    let p = auth.principal();
    auth.require(Action::Write, "profile")?;
    let provider = normalize(&body.default_provider);
    // Only allow pinning a provider that is actually configured — a default the
    // router can't serve would silently fail every bare search. Clearing (None)
    // is always allowed (falls back to `[search].backend`).
    if let Some(name) = provider {
        let enabled = state
            .config()
            .search
            .provider_status()
            .into_iter()
            .any(|(n, on)| on && n == name);
        if !enabled {
            return Err(Error::invalid(format!(
                "search provider `{name}` is not configured; set its API key in [search] first"
            ))
            .into());
        }
    }
    let settings = state
        .store()
        .search_settings()
        .set(p.workspace_id, p.user_id, provider)
        .await?;
    Ok(Json(settings))
}

async fn list_search_providers(
    State(state): State<AppState>,
    auth: Auth,
) -> ApiResult<Json<Vec<SearchProviderInfo>>> {
    let p = auth.principal();
    auth.require(Action::Read, "profile")?;
    let cfg = &state.config().search;
    // Effective default = the caller's override (if set) else `[search].backend`.
    let user_default = state
        .store()
        .search_settings()
        .get(p.workspace_id, p.user_id)
        .await?
        .default_provider;
    let effective_default = user_default.unwrap_or_else(|| cfg.backend.clone());
    let providers = cfg
        .provider_status()
        .into_iter()
        .map(|(name, enabled)| SearchProviderInfo {
            name: name.to_string(),
            enabled,
            is_default: name == effective_default,
        })
        .collect();
    Ok(Json(providers))
}

// ---------------------------------------------------------------------------
// Storage settings (default files store, SOUL §9)
// ---------------------------------------------------------------------------

/// Body for `PUT /storage-settings` — the per-user default-store override. An
/// absent/blank field clears the override (→ the `[storage]` config default). A
/// blank choice from the UI is sent as `null`.
#[derive(Debug, Default, Deserialize)]
pub struct UpdateStorageSettings {
    #[serde(default)]
    pub default_store: Option<String>,
}

async fn get_storage_settings(
    State(state): State<AppState>,
    auth: Auth,
) -> ApiResult<Json<StorageSettings>> {
    let p = auth.principal();
    auth.require(Action::Read, "profile")?;
    let settings = state
        .store()
        .storage_settings()
        .get(p.workspace_id, p.user_id)
        .await?;
    Ok(Json(settings))
}

async fn set_storage_settings(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<UpdateStorageSettings>,
) -> ApiResult<Json<StorageSettings>> {
    let p = auth.principal();
    auth.require(Action::Write, "profile")?;
    let store = normalize(&body.default_store);
    // Only allow pinning a store that actually resolves (config or runtime) — a
    // default the resolver can't serve would silently break every bare op.
    // Clearing (None) is always allowed (falls back to the `[storage]` default).
    if let Some(name) = store {
        crate::routes::storage::resolve(&state, p.workspace_id, None, Some(name))
            .await
            .map_err(|_| {
                ApiError::bad_request(format!(
                    "storage store `{name}` is not configured; add it in [storage] or as a storage connection"
                ))
            })?;
    }
    let settings = state
        .store()
        .storage_settings()
        .set(p.workspace_id, p.user_id, store)
        .await?;
    Ok(Json(settings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_body_decodes_partial() {
        // A partial body leaves the omitted fields `None` (full-replace → cleared).
        let b: UpdateLlmSettings =
            serde_json::from_str(r#"{"chat_model":"gpt-4o","speech_voice":"nova"}"#).unwrap();
        assert_eq!(b.chat_model.as_deref(), Some("gpt-4o"));
        assert_eq!(b.speech_voice.as_deref(), Some("nova"));
        assert!(b.speech_model.is_none());
        assert!(b.transcription_model.is_none());
        assert!(b.ocr_model.is_none());
        assert_eq!(b.voice_input_speed, 1.5);
    }

    #[test]
    fn update_body_decodes_voice_input_speed() {
        let b: UpdateLlmSettings = serde_json::from_str(r#"{"voice_input_speed":1.25}"#).unwrap();
        assert_eq!(b.voice_input_speed, 1.25);
    }

    #[test]
    fn normalize_collapses_blank_to_none() {
        assert_eq!(normalize(&Some("  gpt-4o ".to_string())), Some("gpt-4o"));
        assert_eq!(normalize(&Some("   ".to_string())), None);
        assert_eq!(normalize(&Some(String::new())), None);
        assert_eq!(normalize(&None), None);
    }

    #[test]
    fn models_query_decodes_search_and_kind() {
        // An empty query carries neither field and maps to the full catalog.
        let q: ModelsQuery = serde_json::from_str("{}").unwrap();
        assert!(q.search.is_none());
        assert_eq!(model_kind(&q.kind), ModelKind::All);
        // Both fields decode; `kind` maps to the matching `ModelKind`.
        let q: ModelsQuery = serde_json::from_str(r#"{"search":"gpt","kind":"tts"}"#).unwrap();
        assert_eq!(q.search.as_deref(), Some("gpt"));
        assert_eq!(model_kind(&q.kind), ModelKind::Tts);
        // `llm`/`chat` both mean chat; an unknown kind falls back to the full list.
        assert_eq!(model_kind(&Some("stt".into())), ModelKind::Stt);
        assert_eq!(model_kind(&Some("llm".into())), ModelKind::Chat);
        assert_eq!(model_kind(&Some("bogus".into())), ModelKind::All);
    }

    #[test]
    fn update_search_settings_decodes_and_clears() {
        let set: UpdateSearchSettings =
            serde_json::from_str(r#"{"default_provider":"tavily"}"#).unwrap();
        assert_eq!(set.default_provider.as_deref(), Some("tavily"));
        // An empty body (or blank field) means "clear the override".
        let empty: UpdateSearchSettings = serde_json::from_str("{}").unwrap();
        assert!(empty.default_provider.is_none());
        assert_eq!(normalize(&Some("  brave ".to_string())), Some("brave"));
        assert_eq!(normalize(&Some("   ".to_string())), None);
    }
}
