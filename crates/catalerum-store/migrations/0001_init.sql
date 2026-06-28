-- catalerum-store — M1 initial schema (SOUL §6.1).
--
-- Postgres is the source of truth. Every tenant row carries `workspace_id` and
-- all repository queries are workspace-filtered. UUID primary keys, timestamptz
-- timestamps. This migration lands the M1 tables needed for auth + chat; the
-- remaining §6.1 tables (connections, calendars, events, buckets, objects,
-- entities, documents, chunks, notes, profiles, memories, skills, boards,
-- columns, tasks, channels, agents, grants, automations, triggers,
-- automation_runs, automation_steps, mcp_tokens, job_queue, audit_log) arrive in
-- later migrations as their crates come online.

-- ---------------------------------------------------------------------------
-- workspaces — the tenant root.
-- ---------------------------------------------------------------------------
CREATE TABLE workspaces (
    id          UUID PRIMARY KEY,
    name        TEXT        NOT NULL,
    slug        TEXT        NOT NULL UNIQUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- users — authenticated principals (global, may belong to many workspaces).
-- `sso_issuer` / `sso_subject` are the OIDC/SAML pair; NULL for seeded admins.
-- ---------------------------------------------------------------------------
CREATE TABLE users (
    id            UUID PRIMARY KEY,
    email         TEXT        NOT NULL UNIQUE,
    display_name  TEXT        NOT NULL,
    sso_issuer    TEXT,
    sso_subject   TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- issuer/subject travel together: both set or both NULL.
    CONSTRAINT users_sso_pair_ck
        CHECK ((sso_issuer IS NULL) = (sso_subject IS NULL))
);

-- An SSO subject is globally unique when present.
CREATE UNIQUE INDEX users_sso_idx
    ON users (sso_issuer, sso_subject)
    WHERE sso_issuer IS NOT NULL;

-- ---------------------------------------------------------------------------
-- memberships — binds a user to a workspace with a role.
-- ---------------------------------------------------------------------------
CREATE TABLE memberships (
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    user_id       UUID        NOT NULL REFERENCES users (id)      ON DELETE CASCADE,
    role          TEXT        NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, user_id)
);

CREATE INDEX memberships_user_idx ON memberships (user_id);

-- ---------------------------------------------------------------------------
-- sessions — opaque server-side auth sessions (store-only; no core type).
-- `token_hash` holds a hash of the bearer/cookie token, never the raw token.
-- ---------------------------------------------------------------------------
CREATE TABLE sessions (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    user_id       UUID        NOT NULL REFERENCES users (id)      ON DELETE CASCADE,
    token_hash    TEXT        NOT NULL UNIQUE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at    TIMESTAMPTZ NOT NULL
);

CREATE INDEX sessions_user_idx       ON sessions (user_id);
CREATE INDEX sessions_workspace_idx  ON sessions (workspace_id);
CREATE INDEX sessions_expires_idx    ON sessions (expires_at);

-- ---------------------------------------------------------------------------
-- conversations — chat threads (web / channel / mcp origin).
-- ---------------------------------------------------------------------------
CREATE TABLE conversations (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    title         TEXT,
    origin        TEXT        NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX conversations_workspace_idx
    ON conversations (workspace_id, created_at DESC);

-- ---------------------------------------------------------------------------
-- messages — turns within a conversation (OpenAI/OpenRouter shape).
-- `tool_calls` is a JSONB array of {id,name,arguments}; `tool_call_id` links a
-- tool result back to its assistant call.
-- ---------------------------------------------------------------------------
CREATE TABLE messages (
    id               UUID PRIMARY KEY,
    conversation_id  UUID        NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    role             TEXT        NOT NULL,
    content          TEXT        NOT NULL,
    tool_calls       JSONB       NOT NULL DEFAULT '[]'::jsonb,
    tool_call_id     TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX messages_conversation_idx
    ON messages (conversation_id, created_at ASC);
