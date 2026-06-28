-- catalerum-store — M3 storage catalogue: buckets & objects (SOUL §6.1, §9, §10).
--
-- The blob backend (local FS / S3 / WebDAV) owns the bytes; these rows are the
-- catalogued, *queryable* handle — the "catalogue of things" (§1). A bucket is a
-- storage `Connection`'s container (mirrors calendars on a calendar connection);
-- an object is one catalogued key within it. Bytes never live in the DB (§14).
-- Every tenant row carries `workspace_id` (§18) and all repo queries are
-- workspace-filtered. Cataloguing is idempotent (§3.4): objects upsert by
-- (bucket_id, key), so a re-upload refreshes metadata and never duplicates.

-- ---------------------------------------------------------------------------
-- buckets — a storage container exposed by a (storage-kind) connection. `name`
-- is unique within its connection so get-or-create upserts, never duplicates.
-- `prefix` optionally scopes the bucket to a key prefix (§5).
-- ---------------------------------------------------------------------------
CREATE TABLE buckets (
    id             UUID PRIMARY KEY,
    workspace_id   UUID        NOT NULL REFERENCES workspaces (id)  ON DELETE CASCADE,
    connection_id  UUID        NOT NULL REFERENCES connections (id) ON DELETE CASCADE,
    name           TEXT        NOT NULL,
    prefix         TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- A bucket name is unique within its connection: ensure() upserts/gets.
    CONSTRAINT buckets_connection_name_uq UNIQUE (connection_id, name)
);

CREATE INDEX buckets_workspace_idx  ON buckets (workspace_id);
CREATE INDEX buckets_connection_idx ON buckets (connection_id);

-- ---------------------------------------------------------------------------
-- objects — catalogued metadata for one object in a bucket. The blob itself
-- stays in the bucket (never the DB, §14); this row is the searchable handle.
-- `size` is BIGINT (the core `u64` maps onto i64 — object sizes never approach
-- the boundary). `etag`/`sha256` drive change-detection + dedup;
-- `extracted_text_id` links to the `documents` row holding extracted text once
-- ingested (§10), nulling on document delete. The UNIQUE (bucket_id, key) makes
-- cataloguing idempotent: an upload upserts by key (INSERT … ON CONFLICT DO
-- UPDATE), so re-running never duplicates (§3.4).
-- ---------------------------------------------------------------------------
CREATE TABLE objects (
    id                 UUID PRIMARY KEY,
    workspace_id       UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    bucket_id          UUID        NOT NULL REFERENCES buckets (id)    ON DELETE CASCADE,
    key                TEXT        NOT NULL,
    size               BIGINT      NOT NULL DEFAULT 0,
    content_type       TEXT,
    etag               TEXT,
    last_modified      TIMESTAMPTZ NOT NULL DEFAULT now(),
    sha256             TEXT,
    extracted_text_id  UUID        REFERENCES documents (id) ON DELETE SET NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT objects_bucket_key_uq UNIQUE (bucket_id, key)
);

-- "Most recent objects in a workspace" (recent_objects) + per-bucket key lookups.
CREATE INDEX objects_workspace_idx ON objects (workspace_id, last_modified DESC);
CREATE INDEX objects_bucket_idx    ON objects (bucket_id, key);
