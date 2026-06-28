-- Per-user default files store preference (SOUL §7/§9/§13).
--
-- The `[storage]` TOML block is the immutable boot-time base (the default backend
-- a no-`?store=` op resolves to, plus each backend's billed credentials); this
-- table is the runtime layer that lets a user pick their preferred default store
-- from the workbench Settings (principle 10 — "config is the base; runtime state
-- layers on via the API"). Kept a separate record from `profiles`/`llm_settings`
-- so a preference never leaks into the chat system prompt.
--
-- It holds NOTHING secret: a store NAME, not credentials. Backend secrets live
-- only in config/env or a storage `Connection`'s credential ref (they are shared
-- workspace infrastructure credentials, not personalization). NULL
-- `default_store` means "unset — fall back to the `[storage]` config default".
-- Keyed on `(workspace_id, user_id)` like `search_settings`.
CREATE TABLE storage_settings (
    workspace_id   UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    user_id        UUID        NOT NULL,
    default_store  TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, user_id)
);
