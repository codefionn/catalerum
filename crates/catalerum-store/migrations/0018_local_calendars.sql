-- catalerum-store — local (database-native) calendars (SOUL §8, §11, §12).
--
-- A *local* calendar lives entirely in Postgres: it has no provider connection
-- and is never synced from anything external. The user (or an automation, via
-- the `create_event` tool / `CreateEvent` action) creates and edits its events
-- directly. This is the writable substrate the §11 automations engine targets,
-- and the §8 `CalendarEvent` trigger already polls *all* a workspace's events
-- (`events.list_by_workspace`), so a local calendar's events drive automations
-- with no extra plumbing.
--
-- Model: a local calendar is exactly `calendars.connection_id IS NULL`. We make
-- the column nullable (it was NOT NULL — every calendar belonged to a
-- connection) and keep `read_only = FALSE` for local calendars so the write
-- path is allowed. Provider-backed calendars are unchanged (connection_id set,
-- the existing `(connection_id, external_id)` uniqueness still applies — NULL
-- connection_ids are distinct under that constraint, so it no longer dedups
-- local calendars).

ALTER TABLE calendars ALTER COLUMN connection_id DROP NOT NULL;

-- Local calendars are deduped per workspace by `external_id` instead (the
-- provider unique key is moot when there is no connection). A partial unique
-- index lets the repo get-or-create a named local calendar idempotently
-- (`INSERT … ON CONFLICT (workspace_id, external_id) WHERE connection_id IS NULL`),
-- e.g. the auto-provisioned default calendar the `create_event` tool writes to.
CREATE UNIQUE INDEX calendars_local_external_uq
    ON calendars (workspace_id, external_id)
    WHERE connection_id IS NULL;
