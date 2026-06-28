-- catalerum-store — external PostgreSQL connections (SOUL §11/§13/§19):
-- an encrypted secret store for connection credentials, plus the managed-schema
-- migration ledger (manual, hand-written SQL migrations). The `connections`
-- table (0003) is reused with `kind = 'postgres'`; per-connection settings
-- (host/port/database/username/options) ride in its `config` JSONB, and the
-- password is stored **only** encrypted in `secret_store`, referenced by the
-- connection's `credential_ref`.

-- ---------------------------------------------------------------------------
-- secret_store — workspace-scoped secrets encrypted at rest with AES-256-GCM.
-- `nonce` is the per-row 96-bit GCM nonce; `ciphertext` is the sealed value
-- (with the appended GCM tag). The master key lives only in config/env
-- (`[secrets] master_key`), never in the database — losing it orphans every
-- row here. `ref` is the opaque token stored as a connection's `credential_ref`.
-- ---------------------------------------------------------------------------
CREATE TABLE secret_store (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    ref           TEXT        NOT NULL,
    nonce         BYTEA       NOT NULL,
    ciphertext    BYTEA       NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- `ref` is unique within its workspace: a get/delete is scoped by both.
    CONSTRAINT secret_store_ref_uq UNIQUE (workspace_id, ref)
);

CREATE INDEX secret_store_workspace_idx ON secret_store (workspace_id);

-- ---------------------------------------------------------------------------
-- external_db_migration_scripts — ordered, hand-written SQL migrations authored
-- for a specific external Postgres connection. `version` is unique per
-- connection and applied in ascending order; `checksum` is the SHA-256 of
-- `up_sql`, so a script edited after it was applied is detected (drift guard).
-- ---------------------------------------------------------------------------
CREATE TABLE external_db_migration_scripts (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id)  ON DELETE CASCADE,
    connection_id UUID        NOT NULL REFERENCES connections (id) ON DELETE CASCADE,
    version       BIGINT      NOT NULL,
    name          TEXT        NOT NULL,
    up_sql        TEXT        NOT NULL,
    checksum      TEXT        NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT external_db_migration_scripts_version_uq UNIQUE (connection_id, version)
);

CREATE INDEX external_db_migration_scripts_conn_idx
    ON external_db_migration_scripts (connection_id, version);

-- ---------------------------------------------------------------------------
-- external_db_migrations — the applied-migration ledger, tracked in catalerum's
-- OWN database (not the external one) and keyed by connection. A row is written
-- after a manual script applies successfully against the external DB; the
-- UNIQUE (connection_id, version) makes re-running the migrator a no-op.
-- Declarative auto-migration is idempotent by construction (it diffs the live
-- schema each run) and does not record here.
-- ---------------------------------------------------------------------------
CREATE TABLE external_db_migrations (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id)  ON DELETE CASCADE,
    connection_id UUID        NOT NULL REFERENCES connections (id) ON DELETE CASCADE,
    version       BIGINT      NOT NULL,
    name          TEXT        NOT NULL,
    checksum      TEXT        NOT NULL,
    applied_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT external_db_migrations_version_uq UNIQUE (connection_id, version)
);

CREATE INDEX external_db_migrations_conn_idx
    ON external_db_migrations (connection_id, version);
