-- catalerum-store — M3 markdown notes schema (SOUL §5, §21).
--
-- A note is a user- or LLM-authored markdown document (shopping lists, goals,
-- journals, meeting notes). Stored in Postgres as the source of truth; later
-- ingested like any document (chunk → embed → project, SOUL §10/§21) — that
-- derivation is rebuildable and lands in a later milestone. Every row carries
-- `workspace_id` (the tenancy boundary, SOUL §18) and all repository queries are
-- workspace-filtered.

-- ---------------------------------------------------------------------------
-- notes — a markdown note. `author_kind`/`author_id` model the core `Author`
-- sum type (a human `User` or an `Agent`, SOUL §5/§21) so an automation can
-- draft a note and the user can edit it. `tags` is a JSONB array of free-text
-- labels. `markdown` is the note body (may be empty). `updated_at` is bumped on
-- every edit and is the natural list ordering (most-recently-touched first).
-- ---------------------------------------------------------------------------
CREATE TABLE notes (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- The core `Author` sum type, split across two columns: a discriminator
    -- ('user' | 'agent') and the referenced principal id. Kept as a plain UUID
    -- (no FK) because it points at one of two tables depending on `author_kind`.
    author_kind   TEXT        NOT NULL,
    author_id     UUID        NOT NULL,
    title         TEXT        NOT NULL,
    markdown      TEXT        NOT NULL DEFAULT '',
    tags          JSONB       NOT NULL DEFAULT '[]'::jsonb,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- List hot path: a workspace's notes, most-recently-edited first.
CREATE INDEX notes_workspace_updated_idx ON notes (workspace_id, updated_at DESC);
