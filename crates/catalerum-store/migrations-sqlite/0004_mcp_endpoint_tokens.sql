-- SQLite mirror of Postgres migration 0068 (revocable MCP endpoint share
-- tokens, SOUL §26). See migrations/0068_mcp_endpoint_tokens.sql for the full
-- rationale: the HMAC signature makes a scoped token unforgeable; this table
-- makes it revocable — the serve path requires a live row.

CREATE TABLE mcp_endpoint_tokens (
    id            BLOB PRIMARY KEY,
    workspace_id  BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    endpoint_id   BLOB NOT NULL REFERENCES mcp_endpoints(id) ON DELETE CASCADE,
    token_hash    TEXT NOT NULL UNIQUE,
    created_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at    TEXT NOT NULL,
    revoked_at    TEXT
);

CREATE INDEX mcp_endpoint_tokens_hash_idx
    ON mcp_endpoint_tokens (token_hash)
    WHERE revoked_at IS NULL;

CREATE INDEX mcp_endpoint_tokens_endpoint_idx
    ON mcp_endpoint_tokens (endpoint_id, created_at DESC);
