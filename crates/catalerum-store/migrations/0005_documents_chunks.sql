-- catalerum-store — M4 ingest derivation: documents + chunks (SOUL §5, §6.4, §10).
--
-- A `document` is the extracted text of a source artifact (a note, file, or
-- message) — the unit that gets chunked and embedded. A `chunk` is a contiguous
-- slice of a document, embedded into Qdrant; `point_id` is the derived index
-- handle. Both are **derived from Postgres truth** and fully rebuildable: drop
-- them and re-run ingest from the source row (a note's markdown, a file's text)
-- and the Qdrant index reprojects with no data loss (principle 1, SOUL §3.1).
-- Every row carries `workspace_id` (the tenancy boundary, §18); all repository
-- queries filter on it.

-- ---------------------------------------------------------------------------
-- documents — extracted text for one source artifact. The source is modelled as
-- the core `SourceRef` sum type, stored split across (`source_kind`,
-- `source_id`): a discriminator ('note' | 'object' | 'message' | 'event' |
-- 'document' | 'external') and the referenced id (a uuid string for first-class
-- rows, or a uri for `external`). `(workspace_id, source_kind, source_id)` is
-- UNIQUE so re-ingesting the same source upserts one stable document row rather
-- than duplicating (idempotent ingest, §3.4/§10).
-- ---------------------------------------------------------------------------
CREATE TABLE documents (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    source_kind   TEXT        NOT NULL,
    source_id     TEXT        NOT NULL,
    text          TEXT        NOT NULL DEFAULT '',
    summary       TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, source_kind, source_id)
);

-- ---------------------------------------------------------------------------
-- chunks — a contiguous slice of a document, in `ordinal` order, embedded into
-- Qdrant. `point_id` is the Qdrant point this chunk was upserted as (NULL until
-- embedded). `(document_id, ordinal)` is UNIQUE: a document's chunks are a dense
-- 0-based sequence, replaced wholesale on re-chunk. ON DELETE CASCADE from
-- `documents` so dropping a document drops its chunks.
-- ---------------------------------------------------------------------------
CREATE TABLE chunks (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    document_id   UUID        NOT NULL REFERENCES documents (id) ON DELETE CASCADE,
    ordinal       INTEGER     NOT NULL,
    text          TEXT        NOT NULL,
    point_id      UUID,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (document_id, ordinal)
);

-- Re-chunk hot path: a document's chunks in order.
CREATE INDEX chunks_document_ordinal_idx ON chunks (document_id, ordinal);
-- Workspace-scoped sweeps (rebuild / count).
CREATE INDEX chunks_workspace_idx ON chunks (workspace_id);
