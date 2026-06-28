-- catalerum-store — one-time login tokens (SOUL §18).
--
-- Dev magic-link login (SOUL §17/§18) and any future one-time-credential flow
-- mint a high-entropy opaque token, hand the raw token to the caller, and store
-- only its hash here. The row is consumed exactly once: redemption flips
-- `consumed_at` atomically (UPDATE ... WHERE consumed_at IS NULL), making
-- single-use enforceable and auditable. FKs cascade with the owning workspace
-- and user, matching the `sessions` table style.

CREATE TABLE login_tokens (
    token_hash    TEXT PRIMARY KEY,
    user_id       UUID        NOT NULL REFERENCES users (id)      ON DELETE CASCADE,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at    TIMESTAMPTZ NOT NULL,
    consumed_at   TIMESTAMPTZ
);

CREATE INDEX login_tokens_user_idx       ON login_tokens (user_id);
CREATE INDEX login_tokens_workspace_idx  ON login_tokens (workspace_id);
CREATE INDEX login_tokens_expires_idx    ON login_tokens (expires_at);
