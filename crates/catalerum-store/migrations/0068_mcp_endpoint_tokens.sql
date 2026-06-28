-- catalerum-store — revocable share tokens for Boa-scripted MCP endpoints (SOUL §26).
--
-- `POST /mcp-endpoints/{id}/token` mints an HMAC-signed scoped token that serves
-- exactly one endpoint (`POST /mcp/s/{token}`) with no login. The signature makes
-- a token unforgeable, but a *stateless* token is irrevocable until expiry — so
-- every minted token is also recorded here (hash only, mirroring `sessions` /
-- `login_tokens`): the serve path requires a live row (not revoked, not expired),
-- and `DELETE /mcp-endpoints/{id}/tokens/{token_id}` revokes one immediately.
--
-- Every row carries `workspace_id` (the tenancy boundary, SOUL §18) and all
-- repository queries are workspace-filtered. Rows cascade away with their
-- endpoint (deleting the endpoint kills its outstanding share tokens) and with
-- the workspace.

CREATE TABLE mcp_endpoint_tokens (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    endpoint_id   UUID        NOT NULL REFERENCES mcp_endpoints (id) ON DELETE CASCADE,
    token_hash    TEXT        NOT NULL UNIQUE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at    TIMESTAMPTZ NOT NULL,
    revoked_at    TIMESTAMPTZ
);

-- Serve-time hot path: is this token hash live?
CREATE INDEX mcp_endpoint_tokens_hash_idx
    ON mcp_endpoint_tokens (token_hash)
    WHERE revoked_at IS NULL;

-- Management list: an endpoint's tokens.
CREATE INDEX mcp_endpoint_tokens_endpoint_idx
    ON mcp_endpoint_tokens (endpoint_id, created_at DESC);
