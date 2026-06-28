-- catalerum-store — §19 capability grants (SOUL §18/§19).
--
-- A `grant` is a named capability bundle (with global constraints) a workspace
-- Owner/Admin defines; an automation (or, later, an agent) runs *under* a grant,
-- so its authority is an explicit, attenuated set rather than its creator's full
-- role. This migration persists the bundle the `Grant` core model already
-- describes; the runtime enforcement (the action runner resolving an automation's
-- grant into its `ToolContext` capabilities) lands in a follow-up slice. Every row
-- carries `workspace_id` (§18); all repo queries are workspace-filtered.

CREATE TABLE grants (
    id            UUID        PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- Human-readable name, unique within the workspace (so a grant is referenceable
    -- by name and re-defining is an upsert target).
    name          TEXT        NOT NULL,
    -- The capabilities this grant confers (a JSON array of `Capability`).
    capabilities  JSONB       NOT NULL DEFAULT '[]'::jsonb,
    -- Global constraints (env allow-list, rate/cost caps, time window, dry-run,
    -- per-action approval) — a JSON `Constraints` object.
    constraints   JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, name),
    -- The FK target for the **same-workspace** composite FK below. `id` is already
    -- unique (PK), but Postgres requires a unique constraint on exactly the
    -- referenced columns `(workspace_id, id)`.
    UNIQUE (workspace_id, id)
);

CREATE INDEX grants_workspace_idx ON grants (workspace_id);

-- Now that `grants` exists, give the (already-present) `automations.grant_id`
-- column referential integrity — and crucially **enforce same-workspace at the DB
-- layer** (§18 defense-in-depth): the FK is composite on `(workspace_id, grant_id)`
-- so an automation can only reference a grant in its **own** workspace, not merely
-- any grant. With `MATCH SIMPLE` (default), a NULL `grant_id` skips the check, so a
-- grantless automation is unconstrained. `ON DELETE SET NULL (grant_id)` nulls
-- *only* `grant_id` on grant delete (never the NOT-NULL `workspace_id`), detaching
-- the automation rather than dangling.
ALTER TABLE automations
    ADD CONSTRAINT automations_grant_id_fk
    FOREIGN KEY (workspace_id, grant_id) REFERENCES grants (workspace_id, id)
    ON DELETE SET NULL (grant_id);
