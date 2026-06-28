-- Per-workspace sandboxes (SOUL §20): exactly one long-lived, secure container/
-- Pod per workspace that all terminal sessions and run_command exec into. The
-- live container/Pod handles are node-local (tracked by the API's sandbox
-- manager / the in-cluster operator); this row is only the persisted desired +
-- observed state. PRIMARY KEY (workspace_id) enforces "exactly one per workspace".
CREATE TABLE workspace_sandboxes (
    workspace_id  UUID        PRIMARY KEY REFERENCES workspaces (id) ON DELETE CASCADE,
    -- ExecutorKind token: 'container' (podman/docker) | 'kubernetes'.
    backend       TEXT        NOT NULL,
    image         TEXT        NOT NULL,
    -- pending | ready | failed | stopped
    status        TEXT        NOT NULL DEFAULT 'pending',
    -- Backend reference (container name / Pod name) once provisioned.
    container_ref TEXT,
    -- Persistent /work volume / PVC name.
    volume_ref    TEXT,
    last_activity TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX workspace_sandboxes_status_idx ON workspace_sandboxes (status);
