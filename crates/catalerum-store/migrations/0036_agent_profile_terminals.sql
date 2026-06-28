-- catalerum-store — §19/§20 agent-profile terminal allow-list.
--
-- Adds the `terminals` set to `agent_profiles`: a JSONB array of terminal-workdir
-- **names** (matching `terminal_workdirs.name`, the per-workspace key) that this
-- profile may open a terminal in — the same name-keyed, "empty = all" shape as the
-- existing `tools`/`skills`/`subagents`/`channels` sets. A non-empty list bounds the
-- profile's `open_terminal` to those persistent workdirs (and forbids the ephemeral,
-- workdir-less kind), the terminal analogue of how `tools` bounds the registry.
--
-- Backfills NULL-free: existing profiles get '[]' (= every workdir allowed), so the
-- behaviour is unchanged until an admin narrows a profile's list.

ALTER TABLE agent_profiles
    ADD COLUMN terminals JSONB NOT NULL DEFAULT '[]'::jsonb;
