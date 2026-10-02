-- SQLite mirror of Postgres migration 0070 (weekly plans, SOUL §8). See
-- migrations/0070_weekly_plans.sql for the full rationale.

CREATE TABLE weekly_plans (
    id            BLOB PRIMARY KEY,
    workspace_id  BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name          TEXT NOT NULL,
    description   TEXT,
    calendar_id   BLOB REFERENCES calendars(id) ON DELETE SET NULL,
    timezone      TEXT NOT NULL DEFAULT 'UTC',
    created_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX weekly_plans_workspace_idx ON weekly_plans (workspace_id, name);

CREATE TABLE weekly_plan_entries (
    id            BLOB PRIMARY KEY,
    workspace_id  BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    plan_id       BLOB NOT NULL REFERENCES weekly_plans(id) ON DELETE CASCADE,
    weekday       INTEGER NOT NULL CHECK (weekday BETWEEN 0 AND 6),
    start_minute  INTEGER NOT NULL CHECK (start_minute BETWEEN 0 AND 1439),
    end_minute    INTEGER NOT NULL CHECK (end_minute BETWEEN 1 AND 1440),
    all_day       INTEGER NOT NULL DEFAULT 0,
    summary       TEXT NOT NULL,
    location      TEXT,
    body          TEXT,
    labels        TEXT NOT NULL DEFAULT '[]',
    calendar_id   BLOB REFERENCES calendars(id) ON DELETE SET NULL,
    created_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX weekly_plan_entries_plan_idx ON weekly_plan_entries (plan_id, weekday, start_minute);

CREATE TABLE weekly_plan_events (
    event_id      BLOB PRIMARY KEY REFERENCES events(id) ON DELETE CASCADE,
    workspace_id  BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    plan_id       BLOB NOT NULL REFERENCES weekly_plans(id) ON DELETE CASCADE,
    entry_id      BLOB REFERENCES weekly_plan_entries(id) ON DELETE SET NULL,
    week_start    TEXT NOT NULL,
    created_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE UNIQUE INDEX weekly_plan_events_entry_week_uq
    ON weekly_plan_events (entry_id, week_start)
    WHERE entry_id IS NOT NULL;
CREATE INDEX weekly_plan_events_plan_week_idx ON weekly_plan_events (plan_id, week_start);
CREATE INDEX weekly_plan_events_workspace_idx ON weekly_plan_events (workspace_id);
