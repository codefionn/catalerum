use catalerum_core::id::{UserId, WorkspaceId};
use catalerum_core::model::{LlmSettings, Map, Profile, SearchSettings, StorageSettings};
use sqlx::types::Json;
use crate::error::Result;
use crate::rows::{LlmSettingsRow, ProfileRow, SearchSettingsRow, StorageSettingsRow};
use super::{map, PgPool};

// ===========================================================================
// Profiles (personalization, SOUL §22)
// ===========================================================================

/// The full set of `profiles` columns, in row order (for `SELECT`/`RETURNING`).
const PROFILE_COLS: &str = "workspace_id, user_id, fields, created_at, updated_at";

/// CRUD for the `profiles` table — the per-user structured record injected into
/// the chat system prompt every turn (SOUL §22). Keyed on `(workspace_id,
/// user_id)`.
#[derive(Clone, Debug)]
pub struct ProfileRepo {
    pool: PgPool,
}

impl ProfileRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The profile for `user_id` in `workspace_id`, or an **empty** profile (no
    /// fields) when none has been created yet — so callers always get a usable
    /// `Profile` without a `NotFound` branch.
    pub async fn get(&self, workspace_id: WorkspaceId, user_id: UserId) -> Result<Profile> {
        let row: Option<ProfileRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {PROFILE_COLS} FROM profiles WHERE workspace_id = $1 AND user_id = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(match row {
            Some(r) => r.into(),
            None => Profile {
                workspace_id,
                user_id,
                fields: Map::new(),
            },
        })
    }

    /// Merge `fields` into the user's profile (top-level keys, the incoming value
    /// wins), creating the row on first write, and return the merged profile.
    /// Uses Postgres JSONB `||` so existing keys not in `fields` are preserved.
    /// An empty `fields` is a no-op upsert that just returns the current profile.
    pub async fn merge(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        fields: &Map,
    ) -> Result<Profile> {
        let row: ProfileRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO profiles (workspace_id, user_id, fields)
             VALUES ($1, $2, $3)
             ON CONFLICT (workspace_id, user_id)
             DO UPDATE SET fields = profiles.fields || EXCLUDED.fields,
                           updated_at = CURRENT_TIMESTAMP
             RETURNING {PROFILE_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .bind(Json(fields))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }
}

/// The full set of `llm_settings` columns, in row order (for `SELECT`/`RETURNING`).
const LLM_SETTINGS_COLS: &str = "workspace_id, user_id, chat_model, decision_model, speech_model, \
     speech_voice, transcription_model, voice_input_speed, ocr_model, image_input_models, \
     created_at, updated_at";

/// CRUD for the `llm_settings` table — a per-user override of the `[llm]` config
/// model/voice defaults (SOUL §7/§13). Keyed on `(workspace_id, user_id)`; each
/// unset field falls back to the boot-time config default at use.
#[derive(Clone, Debug)]
pub struct LlmSettingsRepo {
    pool: PgPool,
}

impl LlmSettingsRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The settings for `user_id` in `workspace_id`, or a default record (model
    /// fields unset, microphone speed 1.5×) when none exists — so callers always
    /// get a usable value without a `NotFound` branch.
    pub async fn get(&self, workspace_id: WorkspaceId, user_id: UserId) -> Result<LlmSettings> {
        let row: Option<LlmSettingsRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {LLM_SETTINGS_COLS} FROM llm_settings WHERE workspace_id = $1 AND user_id = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(match row {
            Some(r) => r.into(),
            None => LlmSettings {
                workspace_id,
                user_id,
                chat_model: None,
                decision_model: None,
                speech_model: None,
                speech_voice: None,
                transcription_model: None,
                voice_input_speed: catalerum_core::model::default_voice_input_speed(),
                ocr_model: None,
                image_input_models: Vec::new(),
            },
        })
    }

    /// Replace the user's model/voice selections (a full upsert), creating the row
    /// on first write, and return the stored record. A `None` field **clears** that
    /// selection — the effective value then falls back to the `[llm]` config
    /// default — so a blank choice from the UI is sent as `None`.
    // One arg per selection column; bundling them into a struct would only move
    // the list one call up (the settings route is the sole real caller).
    #[allow(clippy::too_many_arguments)]
    pub async fn set(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        chat_model: Option<&str>,
        speech_model: Option<&str>,
        speech_voice: Option<&str>,
        transcription_model: Option<&str>,
        voice_input_speed: f32,
        ocr_model: Option<&str>,
        decision_model: Option<&str>,
    ) -> Result<LlmSettings> {
        let row: LlmSettingsRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO llm_settings
                 (workspace_id, user_id, chat_model, speech_model, speech_voice, transcription_model,
                  voice_input_speed, ocr_model, decision_model)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (workspace_id, user_id)
             DO UPDATE SET chat_model = EXCLUDED.chat_model,
                           speech_model = EXCLUDED.speech_model,
                           speech_voice = EXCLUDED.speech_voice,
                           transcription_model = EXCLUDED.transcription_model,
                           voice_input_speed = EXCLUDED.voice_input_speed,
                           ocr_model = EXCLUDED.ocr_model,
                           decision_model = EXCLUDED.decision_model,
                           updated_at = CURRENT_TIMESTAMP
             RETURNING {LLM_SETTINGS_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .bind(chat_model)
        .bind(speech_model)
        .bind(speech_voice)
        .bind(transcription_model)
        .bind(voice_input_speed)
        .bind(ocr_model)
        .bind(decision_model)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Replace the user's **force-image-input** model list (SOUL §7/§9) — a
    /// column-scoped upsert that never disturbs the model/voice selections (and
    /// [`set`](Self::set) never disturbs this list), so the two writers coexist.
    /// Creates the row on first write; returns the stored record.
    pub async fn set_image_input_models(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        models: &[String],
    ) -> Result<LlmSettings> {
        let row: LlmSettingsRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO llm_settings (workspace_id, user_id, image_input_models)
             VALUES ($1, $2, $3)
             ON CONFLICT (workspace_id, user_id)
             DO UPDATE SET image_input_models = EXCLUDED.image_input_models,
                           updated_at = CURRENT_TIMESTAMP
             RETURNING {LLM_SETTINGS_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .bind(sqlx::types::Json(models))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }
}

/// The full set of `search_settings` columns, in row order (for `SELECT`/`RETURNING`).
const SEARCH_SETTINGS_COLS: &str =
    "workspace_id, user_id, default_provider, created_at, updated_at";

/// CRUD for the `search_settings` table — a per-user override of the `[search]`
/// default provider (SOUL §7/§13). Keyed on `(workspace_id, user_id)`; an unset
/// `default_provider` falls back to the boot-time `[search].backend` at use.
/// Holds nothing secret — provider API keys live only in config (SOUL §13).
#[derive(Clone, Debug)]
pub struct SearchSettingsRepo {
    pool: PgPool,
}

impl SearchSettingsRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The settings for `user_id` in `workspace_id`, or an **empty** record
    /// (provider unset) when none exists — so callers always get a usable value
    /// without a `NotFound` branch (an unset choice then falls back to config).
    pub async fn get(&self, workspace_id: WorkspaceId, user_id: UserId) -> Result<SearchSettings> {
        let row: Option<SearchSettingsRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {SEARCH_SETTINGS_COLS} FROM search_settings WHERE workspace_id = $1 AND user_id = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(match row {
            Some(r) => r.into(),
            None => SearchSettings {
                workspace_id,
                user_id,
                default_provider: None,
            },
        })
    }

    /// Replace the user's default-provider selection (a full upsert), creating the
    /// row on first write, and return the stored record. A `None` value **clears**
    /// the selection — the effective default then falls back to `[search].backend`
    /// — so a blank choice from the UI is sent as `None`.
    pub async fn set(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        default_provider: Option<&str>,
    ) -> Result<SearchSettings> {
        let row: SearchSettingsRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO search_settings (workspace_id, user_id, default_provider)
             VALUES ($1, $2, $3)
             ON CONFLICT (workspace_id, user_id)
             DO UPDATE SET default_provider = EXCLUDED.default_provider,
                           updated_at = CURRENT_TIMESTAMP
             RETURNING {SEARCH_SETTINGS_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .bind(default_provider)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }
}

/// The full set of `storage_settings` columns, in row order (for `SELECT`/`RETURNING`).
const STORAGE_SETTINGS_COLS: &str = "workspace_id, user_id, default_store, created_at, updated_at";

/// CRUD for the `storage_settings` table — a per-user override of the default
/// files store (SOUL §7/§9/§13). Keyed on `(workspace_id, user_id)`; an unset
/// `default_store` falls back to the boot-time `[storage]` config default at use.
/// Holds nothing secret — backend credentials live only in config / a
/// `Connection`'s credential ref (SOUL §13).
#[derive(Clone, Debug)]
pub struct StorageSettingsRepo {
    pool: PgPool,
}

impl StorageSettingsRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The settings for `user_id` in `workspace_id`, or an **empty** record
    /// (store unset) when none exists — so callers always get a usable value
    /// without a `NotFound` branch (an unset choice then falls back to config).
    pub async fn get(&self, workspace_id: WorkspaceId, user_id: UserId) -> Result<StorageSettings> {
        let row: Option<StorageSettingsRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {STORAGE_SETTINGS_COLS} FROM storage_settings WHERE workspace_id = $1 AND user_id = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(match row {
            Some(r) => r.into(),
            None => StorageSettings {
                workspace_id,
                user_id,
                default_store: None,
            },
        })
    }

    /// Replace the user's default-store selection (a full upsert), creating the
    /// row on first write, and return the stored record. A `None` value **clears**
    /// the selection — the effective default then falls back to the `[storage]`
    /// config default — so a blank choice from the UI is sent as `None`.
    pub async fn set(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        default_store: Option<&str>,
    ) -> Result<StorageSettings> {
        let row: StorageSettingsRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO storage_settings (workspace_id, user_id, default_store)
             VALUES ($1, $2, $3)
             ON CONFLICT (workspace_id, user_id)
             DO UPDATE SET default_store = EXCLUDED.default_store,
                           updated_at = CURRENT_TIMESTAMP
             RETURNING {STORAGE_SETTINGS_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .bind(default_store)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }
}
