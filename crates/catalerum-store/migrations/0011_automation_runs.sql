-- catalerum-store — M5 automation run/step state (SOUL §5, §11).
--
-- The durable audit trail behind §11's "durable run/step state + audit in
-- Postgres". An `automation_run` is one execution of an automation (created when
-- the engine fires a matched trigger); each `automation_step` is one action
-- within that run, ordered by `ordinal`. Status is stored as lowercase TEXT
-- (matching the core `RunStatus`/`StepStatus` snake_case serde forms). Every row
-- carries `workspace_id` (the tenancy boundary, §18); all repository queries are
-- workspace-filtered. Runs cascade-delete with their automation; steps with
-- their run.

CREATE TABLE automation_runs (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    automation_id UUID        NOT NULL REFERENCES automations (id) ON DELETE CASCADE,
    -- running | succeeded | failed | cancelled
    status        TEXT        NOT NULL,
    -- What fired the run (matched trigger + event payload). NULL if not recorded.
    trigger       JSONB,
    -- Failure detail when status = 'failed'.
    error         TEXT,
    started_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Set when the run reaches a terminal state; NULL while running.
    finished_at   TIMESTAMPTZ
);

-- Hot path: list an automation's runs, most-recent first.
CREATE INDEX automation_runs_automation_idx
    ON automation_runs (automation_id, started_at DESC);

CREATE TABLE automation_steps (
    id            UUID        PRIMARY KEY,
    run_id        UUID        NOT NULL REFERENCES automation_runs (id) ON DELETE CASCADE,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- 0-based position within the run (ascending execution order).
    ordinal       INT         NOT NULL,
    -- The executed action spec (a §11 typed action, as JSON).
    action        JSONB       NOT NULL,
    -- running | succeeded | failed | skipped
    status        TEXT        NOT NULL,
    -- The action's result, when it produced one.
    output        JSONB,
    -- Failure detail when status = 'failed'.
    error         TEXT,
    started_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at   TIMESTAMPTZ,
    -- One step per ordinal within a run. This UNIQUE btree on (run_id, ordinal)
    -- also serves the only hot read (`list_steps`: run_id equality + ordinal
    -- order), so no separate index is needed.
    UNIQUE (run_id, ordinal)
);
