-- catalerum-store — drop persistent terminal workdirs (SOUL §20).
--
-- Terminals are now always ephemeral: they launch in a throwaway temp dir (synced
-- to object storage on demand), never in a durable named working directory. This
-- removes the persistent-workdir concept entirely — the `terminal_workdirs` table,
-- the columns that referenced it (`terminal_sessions.workdir_id`, the now-single
-- `terminal_sessions.kind`, `conversations.terminal_workdir_id`), and the
-- `agent_profiles.terminals` allow-list that scoped which workdirs a profile could
-- open. Sessions keep only their ephemeral lifecycle (backend/status/host_dir/
-- sync_prefix).

-- Detach the conversation-level "bound terminal" picker.
ALTER TABLE conversations DROP COLUMN IF EXISTS terminal_workdir_id;

-- The profile-level workdir allow-list.
ALTER TABLE agent_profiles DROP COLUMN IF EXISTS terminals;

-- The session's workdir link (drops terminal_sessions_workdir_idx with it) and the
-- persistent/ephemeral discriminant — every session is ephemeral now.
DROP INDEX IF EXISTS terminal_sessions_workdir_idx;
ALTER TABLE terminal_sessions DROP COLUMN IF EXISTS workdir_id;
ALTER TABLE terminal_sessions DROP COLUMN IF EXISTS kind;

-- Finally the table itself.
DROP TABLE IF EXISTS terminal_workdirs;
