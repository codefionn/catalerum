-- catalerum-store — bind a conversation to an agent profile (SOUL §19/§12).
--
-- The chat "run as a profile" picker: a conversation may be bound to an
-- `agent_profile`, so the §7 chat loop runs as that profile — its model, system
-- prompt, and tool/skill set — with capabilities **intersected with the user's
-- own role** (never an escalation; see the ws handler). NULL = the default chat
-- (the user's role, the workspace default model). Same-workspace composite FK
-- (§18 defense-in-depth): a conversation can only reference a profile in its own
-- workspace; `ON DELETE SET NULL (agent_profile_id)` detaches (nulls only that
-- column, never the NOT-NULL `workspace_id`) when the profile is deleted.

ALTER TABLE conversations
    ADD COLUMN agent_profile_id UUID;

ALTER TABLE conversations
    ADD CONSTRAINT conversations_agent_profile_id_fk
    FOREIGN KEY (workspace_id, agent_profile_id) REFERENCES agent_profiles (workspace_id, id)
    ON DELETE SET NULL (agent_profile_id);
