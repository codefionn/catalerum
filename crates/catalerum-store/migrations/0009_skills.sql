-- catalerum-store — M5 skills (SOUL §5, §23).
--
-- A skill is a reusable, named capability bundle: a markdown `instructions`
-- runbook, an optional restricted tool set, and optional `code` (run via the
-- Executor §20). Skills are authored by users or agents (markdown-first, like a
-- runbook), invoked by the LLM via `use_skill`, and capability-gated
-- (`skill:use@<name>`). Every row carries `workspace_id` (the tenancy boundary,
-- §18); all repository queries are workspace-filtered. `(workspace_id, name)` is
-- UNIQUE — skills are invoked by name.

CREATE TABLE skills (
    id               UUID PRIMARY KEY,
    workspace_id     UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name             TEXT        NOT NULL,
    description      TEXT        NOT NULL DEFAULT '',
    instructions_md  TEXT        NOT NULL DEFAULT '',
    -- JSONB array of tool names this skill is allowed to use (a subset of the
    -- registry); empty = no restriction expressed here.
    tools            JSONB       NOT NULL DEFAULT '[]'::jsonb,
    -- Optional executable code: { language, source, entrypoint? }. NULL for a
    -- pure-instructions (runbook) skill.
    code             JSONB,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, name)
);
