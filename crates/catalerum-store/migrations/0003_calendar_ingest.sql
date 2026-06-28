-- catalerum-store — M2 calendar ingest schema (SOUL §6.1, §6.2, §8, §10).
--
-- Lands the provider-connection / calendar / event tables plus the durable
-- `job_queue` that the ingest + automation workers drain. Every tenant row
-- carries `workspace_id` (the `job_queue` allows a NULL workspace for global
-- maintenance jobs) and all repository queries are workspace-filtered. UUID
-- primary keys, timestamptz timestamps. Ingestion is idempotent + incremental:
-- events upsert by (calendar_id, uid) and connections carry an opaque sync
-- cursor, so re-running a sync never duplicates (SOUL §3.4).

-- ---------------------------------------------------------------------------
-- connections — a configured link to an external provider (calendar / storage /
-- channel). `kind` holds the abstract `ConnectionKind` token (calendar /
-- storage / channel); per-provider details (local dir path, CalDAV base URL,
-- Google account, …) ride in `config` JSONB. `credential_ref` is an opaque
-- pointer into the secret store (never plaintext, SOUL §13). `sync_token` is the
-- opaque incremental-sync cursor (sync-token / ETag / sequence, SOUL §8/§15).
-- ---------------------------------------------------------------------------
CREATE TABLE connections (
    id              UUID PRIMARY KEY,
    workspace_id    UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    kind            TEXT        NOT NULL,
    name            TEXT        NOT NULL,
    credential_ref  TEXT,
    config          JSONB       NOT NULL DEFAULT '{}'::jsonb,
    sync_token      TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX connections_workspace_idx ON connections (workspace_id, created_at DESC);
CREATE INDEX connections_kind_idx      ON connections (workspace_id, kind);

-- ---------------------------------------------------------------------------
-- calendars — a calendar exposed by a connection. `external_id` is the
-- provider-native identifier; unique per connection so re-listing upserts.
-- ---------------------------------------------------------------------------
CREATE TABLE calendars (
    id             UUID PRIMARY KEY,
    workspace_id   UUID        NOT NULL REFERENCES workspaces (id)  ON DELETE CASCADE,
    connection_id  UUID        NOT NULL REFERENCES connections (id) ON DELETE CASCADE,
    external_id    TEXT        NOT NULL,
    name           TEXT        NOT NULL,
    read_only      BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- A provider calendar id is unique within its connection: re-listing upserts.
    CONSTRAINT calendars_connection_external_uq UNIQUE (connection_id, external_id)
);

CREATE INDEX calendars_workspace_idx  ON calendars (workspace_id);
CREATE INDEX calendars_connection_idx ON calendars (connection_id);

-- ---------------------------------------------------------------------------
-- events — normalized calendar events. Fields mirror iCalendar / provider
-- semantics faithfully (SOUL §8/§15): `uid`, `rrule`, `etag`, `sequence`.
-- `attendees` is a JSONB array of resolved `EntityRef`s. The UNIQUE
-- (calendar_id, uid) makes incremental sync idempotent: events upsert by uid
-- (INSERT … ON CONFLICT DO UPDATE), so re-running never duplicates (SOUL §3.4).
-- ---------------------------------------------------------------------------
CREATE TABLE events (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    calendar_id   UUID        NOT NULL REFERENCES calendars (id)  ON DELETE CASCADE,
    uid           TEXT        NOT NULL,
    starts_at     TIMESTAMPTZ NOT NULL,
    ends_at       TIMESTAMPTZ NOT NULL,
    all_day       BOOLEAN     NOT NULL DEFAULT FALSE,
    rrule         TEXT,
    summary       TEXT        NOT NULL,
    location      TEXT,
    body          TEXT,
    attendees     JSONB       NOT NULL DEFAULT '[]'::jsonb,
    etag          TEXT,
    sequence      INTEGER     NOT NULL DEFAULT 0,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT events_calendar_uid_uq UNIQUE (calendar_id, uid)
);

CREATE INDEX events_workspace_starts_idx ON events (workspace_id, starts_at);
CREATE INDEX events_calendar_idx         ON events (calendar_id);

-- ---------------------------------------------------------------------------
-- job_queue — the durable work queue (SOUL §6.2). All async work (sync,
-- projection, embedding, automation runs) is enqueued here first; workers drain
-- it via SELECT … FOR UPDATE SKIP LOCKED. `workspace_id` is nullable for global
-- maintenance jobs. `status` ∈ {pending, running, done, failed}; `run_after`
-- gates scheduling + retry backoff; `locked_at`/`locked_by` lease a row to one
-- worker; `attempts`/`last_error` track retries.
-- ---------------------------------------------------------------------------
CREATE TABLE job_queue (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        REFERENCES workspaces (id) ON DELETE CASCADE,
    kind          TEXT        NOT NULL,
    payload       JSONB       NOT NULL DEFAULT '{}'::jsonb,
    status        TEXT        NOT NULL DEFAULT 'pending',
    attempts      INTEGER     NOT NULL DEFAULT 0,
    run_after     TIMESTAMPTZ NOT NULL DEFAULT now(),
    locked_at     TIMESTAMPTZ,
    locked_by     TEXT,
    last_error    TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Dequeue hot path: claim the oldest runnable pending job
-- (WHERE status = 'pending' AND run_after <= now() ORDER BY run_after).
CREATE INDEX job_queue_dequeue_idx ON job_queue (status, run_after);
CREATE INDEX job_queue_workspace_idx ON job_queue (workspace_id);
