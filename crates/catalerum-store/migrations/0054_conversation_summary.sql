-- Persistent chat-thread auto-compaction state (SOUL §7/§12).
--
-- When a conversation's replayed transcript approaches the model's context
-- window, a background pass folds the older part into a rolling `summary`;
-- `summary_upto` is the last message the summary covers. The next turn seeds
-- the agent loop with [summary] + messages *after* summary_upto instead of the
-- whole transcript. Messages themselves are never deleted — the conversation
-- view still shows everything; this only bounds what the model re-reads.
--
-- ON DELETE SET NULL: a regenerate prunes the transcript tail with a plain
-- message DELETE — if that removes the covered anchor row, the pointer nulls
-- itself and the (now stale) summary is ignored (a summary is only used when
-- BOTH columns are set), then rebuilt by the next compaction pass.
ALTER TABLE conversations
    ADD COLUMN summary TEXT,
    ADD COLUMN summary_upto UUID REFERENCES messages(id) ON DELETE SET NULL;
