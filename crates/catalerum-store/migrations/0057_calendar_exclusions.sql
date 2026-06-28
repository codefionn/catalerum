-- catalerum-store — persistent per-calendar exclusions (SOUL §8/§10/§11).
--
-- Deleting a *synced* (provider-backed) calendar can't just drop the `calendars`
-- row: the next connection sync upserts it straight back by
-- `(connection_id, external_id)`. This table records "the user removed this
-- provider calendar" so both sync paths (`catalerum-ingest` sync.rs + collect.rs)
-- skip re-creating it. Keyed on `(connection_id, external_id)` to mirror the
-- calendars upsert key; the FK cascades on connection delete, so removing (and
-- re-adding) a source clears its exclusions and the calendars re-sync. The
-- external provider is never touched — this is a local "don't mirror this" flag.
CREATE TABLE calendar_exclusions (
    workspace_id  UUID        NOT NULL REFERENCES workspaces  (id) ON DELETE CASCADE,
    connection_id UUID        NOT NULL REFERENCES connections (id) ON DELETE CASCADE,
    external_id   TEXT        NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (connection_id, external_id)
);

CREATE INDEX calendar_exclusions_workspace_idx ON calendar_exclusions (workspace_id);
