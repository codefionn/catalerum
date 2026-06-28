-- catalerum-store — external MCP servers, runtime-managed (SOUL §26).
--
-- The durable, DB-backed form of a `[[mcp.servers]]` config entry: an external
-- MCP server catalerum connects to **as a client** (stdio or HTTP/SSE), folding
-- its tools into the §7 registry as `{name}_{tool}` (each gated on
-- `mcp:use@{name}`, §19). Created / edited / deleted at runtime by the
-- `*_mcp_server` tools and hot-(dis)connected without a restart.
--
-- Per workspace (the §18 tenancy boundary); `(workspace_id, name)` is UNIQUE — a
-- server is referenced by name. `args` / `env` / `auth` / `tools` are JSONB the
-- `catalerum-mcp` client owns the typed meaning of. `auth` carries credentials
-- verbatim for now (a follow-up moves secrets behind the §13 secret store), so
-- the row is sensitive — never log it.

CREATE TABLE mcp_servers (
    id            UUID        PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- Human-readable name, unique within the workspace; prefixes the tools.
    name          TEXT        NOT NULL,
    -- "stdio" (spawn `command`) or "http" (connect to `url`).
    transport     TEXT        NOT NULL DEFAULT 'stdio',
    -- Program to spawn (stdio transport).
    command       TEXT        NOT NULL DEFAULT '',
    -- JSONB array of arguments to `command`.
    args          JSONB       NOT NULL DEFAULT '[]'::jsonb,
    -- JSONB object of extra environment variables for the child.
    env           JSONB       NOT NULL DEFAULT '{}'::jsonb,
    -- Endpoint URL (http transport).
    url           TEXT        NOT NULL DEFAULT '',
    -- JSONB auth spec (kind + that mode's fields; credentials verbatim).
    auth          JSONB       NOT NULL DEFAULT '{}'::jsonb,
    -- Whether to connect this server.
    enabled       BOOLEAN     NOT NULL DEFAULT TRUE,
    -- JSONB array of remote tool names to import; empty = import all.
    tools         JSONB       NOT NULL DEFAULT '[]'::jsonb,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, name)
);

CREATE INDEX mcp_servers_workspace_idx ON mcp_servers (workspace_id);
