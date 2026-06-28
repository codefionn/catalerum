-- catalerum-store — terminal sessions, active + history (SOUL §20).
--
-- One interactive terminal an agent stood up. `workdir_id` references the
-- persistent workdir it runs in, or is NULL for an ephemeral session (a temp dir
-- synced to object storage on demand). `kind` is persistent|ephemeral; `backend`
-- the Executor runtime; `status` the lifecycle (active|closed|failed). `host_dir`
-- records where the session's files live on disk (for the ephemeral flush);
-- `sync_prefix` the last object-storage key prefix it was persisted under. Per
-- workspace (the §18 tenancy boundary). PTY / process state is node-local and is
-- NOT stored here — only the durable record of the session.

CREATE TABLE terminal_sessions (
    id            UUID        PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- The persistent workdir this session runs in; NULL ⇒ ephemeral session.
    workdir_id    UUID        REFERENCES terminal_workdirs (id) ON DELETE CASCADE,
    -- 'persistent' | 'ephemeral'.
    kind          TEXT        NOT NULL DEFAULT 'persistent',
    -- Executor runtime: 'local' | 'sandbox' | 'container' | 'kubernetes'.
    backend       TEXT        NOT NULL,
    -- Lifecycle: 'active' | 'closed' | 'failed'.
    status        TEXT        NOT NULL DEFAULT 'active',
    -- Where the session's files live on disk (for the ephemeral flush).
    host_dir      TEXT,
    -- Last object-storage key prefix this session was persisted under.
    sync_prefix   TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at     TIMESTAMPTZ
);

CREATE INDEX terminal_sessions_workdir_idx ON terminal_sessions (workdir_id, created_at DESC);
CREATE INDEX terminal_sessions_workspace_idx ON terminal_sessions (workspace_id);
