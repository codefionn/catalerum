-- catalerum-store — enrolled **computer agents** (SOUL §19/§20).
--
-- A computer agent is a daemon a user installs on a server/desktop that dials out
-- to the API over an authenticated WebSocket and serves scoped file/search/exec/
-- desktop operations the LLM drives through the `computer_*` tools. Enrolling one
-- mints a long-lived bearer token; only its SHA-256 hash is stored here (the same
-- opaque-token scheme as `sessions`/`login_tokens` — the DB never sees the
-- plaintext). The row is workspace-scoped (the tenancy boundary, SOUL §18) and
-- records the owning user, mirroring the token tables.
--
-- `capabilities` is the machine's announced capability snapshot (platform, served
-- directories, exec policy, desktop, sandbox) — NULL until the agent first
-- connects, then refreshed on every reconnect so the enrolled-agent list can show
-- an offline machine's last-known shape. `platform` is denormalised out of it for
-- cheap listing. `last_seen_at` is bumped while a connection is live; liveness
-- itself is an in-memory per-pod fact (SOUL §11) and is not persisted.
--
-- Revocation is a nullable timestamp (`revoked_at`), matching the `consumed_at`
-- idiom of `login_tokens`: a revoked agent's token no longer authenticates and its
-- live connection is dropped, but the row is retained for audit until deleted.

CREATE TABLE computer_agents (
    id            UUID        PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    user_id       UUID        NOT NULL REFERENCES users      (id) ON DELETE CASCADE,
    name          TEXT        NOT NULL,
    token_hash    TEXT        NOT NULL UNIQUE,
    platform      TEXT,
    capabilities  JSONB,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at  TIMESTAMPTZ,
    revoked_at    TIMESTAMPTZ,
    UNIQUE (workspace_id, name)
);

-- List hot path: a workspace's agents, most-recently-enrolled first.
CREATE INDEX computer_agents_workspace_idx ON computer_agents (workspace_id, created_at DESC);
