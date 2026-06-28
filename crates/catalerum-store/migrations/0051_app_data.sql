-- catalerum-store — per-App durable key/value store (SOUL §12/§29).
--
-- An emerged App (an AI-authored declarative UI, SOUL §5/§12) is only fully
-- featured if it can persist a data model its handlers grow and present over
-- time ("automation collects → data model grows → App presents"). The first-class
-- object stores (notes, tasks, events, files, external Postgres) are the right
-- home for *shared, first-class* facts; but a small App that just wants to keep
-- one JSON document (a habit tracker's per-user state, a dashboard's saved
-- layout) has no fit there — a note is markdown-shaped and pollutes the Notes
-- panel + semantic index, a file has no write tool and reads back as async
-- extracted text, and an external Postgres connection is an admin-provisioned
-- server. This table is that missing lightweight primitive: a workspace-scoped
-- `(app, key) → JSONB` map, reached through the same capability-gated tools
-- everything else uses (`app_data_get`/`set`/`list`/`delete`, gated on
-- `ui:read`/`ui:write`, SOUL §19).
--
-- SCOPING (the isolation boundary, SOUL §12/§29): `app` is the namespace an
-- App's data lives under. When a store tool is called from an App event handler
-- the runtime *forces* `app` to the firing UI's id (from `ToolContext::ui_id`),
-- so one App can never read or write another App's keys — the namespace is not a
-- caller-supplied argument on that path. A full-authority caller (chat / an
-- automation) may name any `app` explicitly, which is how the "automation
-- collects → App presents" loop writes into the App's namespace. Every row
-- carries `workspace_id` (the tenancy boundary, SOUL §18) and all repository
-- queries are workspace-filtered.
--
-- Value/row caps mirror the `initial_state` cap philosophy (MAX_INITIAL_STATE_
-- ELEMENTS) and are enforced in the repository, not the schema: a single value
-- is byte-bounded and the number of keys per (workspace, app) is bounded, so an
-- App cannot use this as unbounded blob storage.

CREATE TABLE app_data (
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- The App namespace: a `ui_definitions.id` (as text) on the handler path, or
    -- any caller-named namespace on the trusted chat/automation path. Free TEXT so
    -- both forms coexist without a join.
    app           TEXT        NOT NULL,
    key           TEXT        NOT NULL,
    value         JSONB       NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- One value per (workspace, app, key); `set` upserts. The PK doubles as the
    -- index for both point reads and the (workspace, app) list/count prefix scans.
    PRIMARY KEY (workspace_id, app, key)
);
