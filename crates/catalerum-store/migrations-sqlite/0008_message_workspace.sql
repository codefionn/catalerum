-- SQLite mirror of Postgres migration 0072 (`messages.workspace_id`, SOUL §18).
-- See migrations/0072_message_workspace.sql for the rationale.
--
-- SQLite cannot add a NOT NULL column without a default, nor tighten one later
-- without rebuilding the table (and dropping `messages` would fire the
-- `conversations.summary_upto` ON DELETE SET NULL). So the column stays
-- nullable here. The repo INSERT always derives it from the conversation, and
-- every repo read filters on it, so a NULL row is unreachable rather than
-- cross-tenant.

ALTER TABLE messages
    ADD COLUMN workspace_id BLOB REFERENCES workspaces(id) ON DELETE CASCADE;

UPDATE messages
    SET workspace_id = (
        SELECT c.workspace_id FROM conversations c WHERE c.id = messages.conversation_id
    )
    WHERE workspace_id IS NULL;

CREATE INDEX messages_workspace_idx
    ON messages (workspace_id, created_at DESC);
