-- Per-user web-search provider preference (SOUL §7/§13).
--
-- The `[search]` TOML block is the immutable boot-time base (the default backend
-- a no-`provider` search resolves to, plus each provider's billed API key); this
-- table is the runtime layer that lets a user pick their preferred default engine
-- from the workbench Settings (principle 10 — "config is the base; runtime state
-- layers on via the API"). Kept a separate record from `profiles`/`llm_settings`
-- so a preference never leaks into the chat system prompt.
--
-- It holds NOTHING secret: provider API keys live only in config/env (they are
-- shared workspace infrastructure credentials, not personalization). NULL
-- `default_provider` means "unset — fall back to `[search].backend`". Keyed on
-- `(workspace_id, user_id)` like `llm_settings`.
CREATE TABLE search_settings (
    workspace_id      UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    user_id           UUID        NOT NULL,
    default_provider  TEXT,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, user_id)
);
