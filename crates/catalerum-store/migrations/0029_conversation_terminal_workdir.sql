-- catalerum-store — bind a conversation to a persistent terminal workdir (SOUL §20/§12).
--
-- The chat "terminal" picker: a conversation may be bound to a persistent
-- `terminal_workdir`, so the §7 chat loop tells the agent to default
-- `open_terminal`'s `workdir_id` to it. NULL = no bound terminal (the agent
-- picks/asks). `ON DELETE SET NULL` detaches the binding when the workdir is
-- deleted — it never deletes the conversation. Workspace scoping is enforced at
-- the app layer (the set-terminal route validates the workdir is in-workspace,
-- like the agent-profile picker), so a simple single-column FK suffices.

ALTER TABLE conversations
    ADD COLUMN terminal_workdir_id UUID
        REFERENCES terminal_workdirs (id) ON DELETE SET NULL;
