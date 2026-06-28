-- catalerum-store — attach a `/<skill>` invocation snapshot to a message (SOUL §12/§23).
--
-- The composer's `/<skill>` command: the invoked skill's runbook is snapshotted
-- onto the user message row — JSONB {name, instructions, tools} — so the agent
-- loop can render it into the turn the model sees (on the live turn and on every
-- replay), while the row's `content` stays the short invocation text the UI
-- shows. A snapshot (not a name to re-resolve) so the transcript stays stable if
-- the skill is later edited or deleted. NULL for every other message, mirroring
-- how `attachments` rides the row as references.

ALTER TABLE messages
    ADD COLUMN skill JSONB;
