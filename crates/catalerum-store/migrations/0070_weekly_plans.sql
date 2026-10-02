-- catalerum-store — weekly plans (reusable week templates, SOUL §8).
--
-- A *weekly plan* is a named template of recurring slots (weekday + wall-clock
-- time in the plan's IANA timezone). Several plans coexist per workspace and are
-- edited independently; *applying* one to a concrete week materialises a real
-- calendar event per entry (through the normal local / provider write path).
--
--   weekly_plans         — the template header (+ default target calendar, tz)
--   weekly_plan_entries  — its slots; weekday 0 = Monday … 6 = Sunday, times are
--                          minutes after local midnight (end may be 1440 = 24:00)
--   weekly_plan_events   — the LINK between a plan application and the events it
--                          created: one row per materialised event, keyed by the
--                          event. Re-applying a week updates the linked events in
--                          place (idempotent, SOUL §3.4); un-applying deletes
--                          exactly them.
--
-- Link lifecycle via FKs: an event deleted elsewhere (UI, sync reconcile) drops
-- its link row (CASCADE); a deleted entry leaves its link with entry_id NULL so
-- the next re-apply of that week removes the orphaned event; a deleted plan
-- drops its links but leaves the events (they are ordinary calendar events).
-- Calendars referenced by a plan/entry fall back to the default on delete.
-- Every row carries workspace_id (tenancy boundary, SOUL §18).

CREATE TABLE weekly_plans (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name          TEXT        NOT NULL,
    description   TEXT,
    calendar_id   UUID        REFERENCES calendars (id) ON DELETE SET NULL,
    timezone      TEXT        NOT NULL DEFAULT 'UTC',
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX weekly_plans_workspace_idx ON weekly_plans (workspace_id, name);

CREATE TABLE weekly_plan_entries (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    plan_id       UUID        NOT NULL REFERENCES weekly_plans (id) ON DELETE CASCADE,
    weekday       INTEGER     NOT NULL CHECK (weekday BETWEEN 0 AND 6),
    start_minute  INTEGER     NOT NULL CHECK (start_minute BETWEEN 0 AND 1439),
    end_minute    INTEGER     NOT NULL CHECK (end_minute BETWEEN 1 AND 1440),
    all_day       BOOLEAN     NOT NULL DEFAULT FALSE,
    summary       TEXT        NOT NULL,
    location      TEXT,
    body          TEXT,
    labels        JSONB       NOT NULL DEFAULT '[]'::jsonb,
    calendar_id   UUID        REFERENCES calendars (id) ON DELETE SET NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX weekly_plan_entries_plan_idx ON weekly_plan_entries (plan_id, weekday, start_minute);

CREATE TABLE weekly_plan_events (
    event_id      UUID PRIMARY KEY REFERENCES events (id) ON DELETE CASCADE,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    plan_id       UUID        NOT NULL REFERENCES weekly_plans (id) ON DELETE CASCADE,
    entry_id      UUID        REFERENCES weekly_plan_entries (id) ON DELETE SET NULL,
    week_start    DATE        NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One materialised event per (entry, week): re-apply finds it here.
CREATE UNIQUE INDEX weekly_plan_events_entry_week_uq
    ON weekly_plan_events (entry_id, week_start)
    WHERE entry_id IS NOT NULL;
CREATE INDEX weekly_plan_events_plan_week_idx ON weekly_plan_events (plan_id, week_start);
CREATE INDEX weekly_plan_events_workspace_idx ON weekly_plan_events (workspace_id);
