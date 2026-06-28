-- Per-user LLM model + voice selections (SOUL §7/§13).
--
-- The `[llm]` TOML block is the immutable boot-time base (chat/embedding/speech/
-- transcription models + the default TTS voice); this table is the runtime layer
-- that lets a user override the chat model and the speech model/voice from the
-- workbench Settings (principle 10 — "config is the base; runtime state layers on
-- via the API"). Kept a separate record from `profiles` so a model/voice choice
-- never leaks into the chat system prompt (which renders every profile field).
--
-- Each model/voice column is nullable: NULL means "unset — fall back to the
-- `[llm]` config default". Keyed on `(workspace_id, user_id)` like `profiles`.
CREATE TABLE llm_settings (
    workspace_id         UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    user_id              UUID        NOT NULL,
    chat_model           TEXT,
    speech_model         TEXT,
    speech_voice         TEXT,
    transcription_model  TEXT,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, user_id)
);
