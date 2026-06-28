-- catalerum-store — soft-archive for workspaces (SOUL §18).
--
-- An org admin/owner "archives" a workspace to retire it from the org. This was a
-- **hard delete** (cascade) until now; it becomes a reversible **soft archive** so
-- an admin can restore a workspace (and its data) instead of losing it.
--
-- Additive + backfill-safe: a new nullable `archived_at` column. Existing rows are
-- active (NULL). Every default workspace listing filters `archived_at IS NULL`;
-- identity lookups (`get`/`get_by_slug`/`get_many`) still return archived rows so
-- restore + org-admin views can resolve them.

ALTER TABLE workspaces
    ADD COLUMN archived_at TIMESTAMPTZ;

-- Partial index over the active workspaces only — matches the default listings'
-- `WHERE archived_at IS NULL` predicate (list / list_by_organisation), and stays
-- small since archived rows are excluded from the index entirely.
CREATE INDEX workspaces_active_organisation_idx
    ON workspaces (organisation_id)
    WHERE archived_at IS NULL;
