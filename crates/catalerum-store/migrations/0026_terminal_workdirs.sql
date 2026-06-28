-- catalerum-store — persistent terminal working directories (SOUL §20).
--
-- A durable, named working directory an agent terminal runs in. Several per
-- workspace; the chat dropdown picks one and the set is managed in Settings.
-- `backend` selects the Executor runtime (local|sandbox|container|kubernetes);
-- `path` is the host path for local/sandbox (NULL for container/k8s, which use a
-- named volume / PVC); `config` is JSONB the executor owns (image, mounts,
-- namespace, cpu/mem, sync target). Per workspace (the §18 tenancy boundary);
-- `(workspace_id, name)` is UNIQUE — a workdir is referenced by name.

CREATE TABLE terminal_workdirs (
    id            UUID        PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- Human-readable name, unique within the workspace; shown in the picker.
    name          TEXT        NOT NULL,
    -- Executor runtime: 'local' | 'sandbox' | 'container' | 'kubernetes'.
    backend       TEXT        NOT NULL DEFAULT 'local',
    -- Host path for local/sandbox backends; NULL for container/k8s (volume/PVC).
    path          TEXT,
    -- JSONB the executor owns: image, mounts, namespace, cpu/mem, sync target.
    config        JSONB       NOT NULL DEFAULT '{}'::jsonb,
    -- Whether this workdir is offered in the picker / usable.
    enabled       BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, name)
);

CREATE INDEX terminal_workdirs_workspace_idx ON terminal_workdirs (workspace_id);
