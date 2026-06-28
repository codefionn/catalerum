-- catalerum-store — user-authored Boa-scripted MCP endpoints (SOUL §26).
--
-- An MCP endpoint is a stored JavaScript program that declares MCP tools and, on
-- a `tools/call`, reaches a narrow host bridge (e.g. prefix-scoped
-- `search_semantic`) whose scope is pinned to the endpoint. It is served over its
-- own `POST /mcp/e/{name}` (workspace token) and `POST /mcp/s/{token}` (a signed,
-- shareable scoped token), isolated from the main tool surface — a connecting
-- agent sees only the tools the script declares.
--
-- `author_kind`/`author_id` model the core `Author` sum type (User or Agent), like
-- notes / ui_definitions. Every row carries `workspace_id` (the tenancy boundary,
-- SOUL §18) and all repository queries are workspace-filtered. `(workspace_id,
-- name)` is UNIQUE — endpoints are addressed by name in the URL. `grant_id` is the
-- §19 authority the script's tool calls run under (NULL → a minimal read-only
-- authority resolved at serve time). `bucket_name`/`key_prefix` pin the endpoint's
-- search scope; the host injects them into every search call so a script can never
-- widen its own scope.

CREATE TABLE mcp_endpoints (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name          TEXT        NOT NULL,
    description   TEXT        NOT NULL DEFAULT '',
    script        TEXT        NOT NULL DEFAULT '',
    bucket_name   TEXT,
    key_prefix    TEXT,
    grant_id      UUID        REFERENCES grants (id) ON DELETE SET NULL,
    enabled       BOOLEAN     NOT NULL DEFAULT TRUE,
    author_kind   TEXT        NOT NULL,
    author_id     UUID        NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, name)
);

-- List hot path: a workspace's endpoints, most-recently-edited first.
CREATE INDEX mcp_endpoints_ws_updated_idx
    ON mcp_endpoints (workspace_id, updated_at DESC);
