-- catalerum-store — pending `ask_user` question forms (SOUL §7/§12).
--
-- When the chat LLM calls the `ask_user` tool, the questions are persisted here,
-- tied to a conversation, so the interactive form survives a page reload / socket
-- reconnect (the turn is NOT held open). It is resolved when the user answers —
-- their answer arrives as an ordinary follow-up turn. At most one row per
-- conversation is unresolved at a time. Every row carries `workspace_id` (the
-- tenancy boundary, SOUL §18) and all queries are workspace-filtered.

CREATE TABLE pending_questions (
    id              UUID PRIMARY KEY,
    workspace_id    UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    conversation_id UUID        NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    questions       JSONB       NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at     TIMESTAMPTZ
);

-- The hot path: the most-recent unresolved question for a conversation.
CREATE INDEX pending_questions_unresolved_idx
    ON pending_questions (workspace_id, conversation_id, created_at DESC)
    WHERE resolved_at IS NULL;
