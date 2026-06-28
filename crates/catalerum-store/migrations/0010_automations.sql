-- catalerum-store — M5 automations (SOUL §5, §11).
--
-- A durable trigger→condition→action automation: a set of `triggers`, an
-- optional `condition` predicate, and ordered typed `actions` — all kept as
-- JSONB here (the `catalerum-automation` engine owns their typed interpretation
-- and execution; Postgres is the definition source of truth, §6.1). `spec` is
-- the full original authoring document. Every row carries `workspace_id` (the
-- tenancy boundary, §18); all repository queries are workspace-filtered.
-- `(workspace_id, name)` is UNIQUE — automations are named per workspace.
-- `grant_id` is the §19 grant the automation runs under (a soft reference; the
-- `grants` table lands with the policy engine, so no FK yet).

CREATE TABLE automations (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name          TEXT        NOT NULL,
    enabled       BOOLEAN     NOT NULL DEFAULT TRUE,
    -- JSONB array of trigger specs (CalendarEvent / Schedule / Webhook / …).
    triggers      JSONB       NOT NULL DEFAULT '[]'::jsonb,
    -- Optional predicate over store/graph/vectors. NULL = fire unconditionally.
    condition     JSONB,
    -- Ordered JSONB array of typed action specs (also the LLM's tools, §11).
    actions       JSONB       NOT NULL DEFAULT '[]'::jsonb,
    -- The full original authoring spec (source of truth for round-tripping).
    spec          JSONB,
    -- The §19 grant the automation runs under (soft ref; grants table is future).
    grant_id      UUID,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, name)
);
