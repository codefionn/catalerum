-- catalerum-store — `messages.workspace_id` (SOUL §3.14/§6.1/§18).
--
-- Every tenant row carries `workspace_id` and every query is workspace-filtered.
-- `messages` only reached its workspace through `conversations`, so a lookup by
-- message id alone could cross the tenancy boundary. This adds the column, then
-- the repo filters every read, update and delete by it.
--
-- Additive + backfill-safe for existing deployments:
--   1. add `workspace_id` nullable;
--   2. backfill it from the owning conversation;
--   3. lock it NOT NULL + FK (cascading with the workspace, like `conversations`).
--
-- The repo's INSERT derives the value from `conversations` filtered by the
-- caller's workspace, so a message can never disagree with its conversation.

ALTER TABLE messages
    ADD COLUMN workspace_id UUID;

UPDATE messages m
    SET workspace_id = c.workspace_id
    FROM conversations c
    WHERE c.id = m.conversation_id
      AND m.workspace_id IS NULL;

ALTER TABLE messages
    ALTER COLUMN workspace_id SET NOT NULL;

ALTER TABLE messages
    ADD CONSTRAINT messages_workspace_id_fk
    FOREIGN KEY (workspace_id) REFERENCES workspaces (id) ON DELETE CASCADE;

-- Workspace-wide scans (content search, newest first) and the workspace-delete
-- cascade. Per-conversation reads keep using `messages_conversation_idx`.
CREATE INDEX messages_workspace_idx
    ON messages (workspace_id, created_at DESC);
