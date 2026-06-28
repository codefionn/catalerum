-- catalerum-store — deferred tool-call approvals (SOUL §7/§12/§19).
--
-- When a profile's tool guard (§19) classifies a tool call as
-- "require-user-feedback", the call is recorded here (held, never run) tied to a
-- conversation, so the Approve/Reject prompt survives a page reload / socket
-- reconnect / server restart — the turn is NOT held open. It is resolved when the
-- user decides: on approve the agent re-runs the call (the guard now allows it);
-- on reject the guard denies it. `decision` records the ruling; a superseded row
-- resolves with a NULL decision. At most one row per conversation is unresolved at
-- a time. Every row carries `workspace_id` (the tenancy boundary, SOUL §18) and
-- all queries are workspace-filtered.

CREATE TABLE pending_approvals (
    id              UUID PRIMARY KEY,
    workspace_id    UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    conversation_id UUID        NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    tool            TEXT        NOT NULL,
    arguments       JSONB       NOT NULL,
    reason          TEXT        NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at     TIMESTAMPTZ,
    decision        TEXT        -- 'approved' | 'rejected', NULL while pending / superseded
);

-- The hot path: the most-recent unresolved approval for a conversation.
CREATE INDEX pending_approvals_unresolved_idx
    ON pending_approvals (workspace_id, conversation_id, created_at DESC)
    WHERE resolved_at IS NULL;

-- The resume path: a resolved decision for a conversation (the guard consults this
-- when the agent re-attempts the approved/rejected call).
CREATE INDEX pending_approvals_resolved_idx
    ON pending_approvals (workspace_id, conversation_id, resolved_at DESC)
    WHERE resolved_at IS NOT NULL;
