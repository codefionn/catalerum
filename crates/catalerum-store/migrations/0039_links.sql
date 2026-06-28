-- catalerum-store — relationships between objects (SOUL §5, §6.3).
--
-- A `link` is a user- or agent-authored, directed relationship (`from → to`)
-- between any two first-class objects: a note↔event, a file↔email, and so on.
-- Both endpoints are the core `SourceRef` sum type, stored split across
-- (`from_kind`, `from_id`) / (`to_kind`, `to_id`) — the same encoding the
-- `documents` table uses (a discriminator + a uuid string, or a uri for
-- `external`). Postgres is the source of truth; the derived Neo4j graph projects
-- each row as a `RELATES_TO` edge (rebuildable, SOUL §6.3). Every row carries
-- `workspace_id` (the tenancy boundary, SOUL §18) and all repository queries are
-- workspace-filtered.

-- ---------------------------------------------------------------------------
-- links — a directed relationship between two objects. `label` is an optional
-- free-text relation kind ("attachment", "follow-up"); `note` an optional
-- annotation. `author_kind`/`author_id` model the core `Author` sum type (a
-- human `User` or an `Agent`, SOUL §5/§21) so an automation can draft a link and
-- the user can inspect it. Endpoints are polymorphic `SourceRef`s, so there is no
-- foreign key on them: a link may outlive the object it points at until
-- reconciled.
-- ---------------------------------------------------------------------------
CREATE TABLE links (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    from_kind     TEXT        NOT NULL,
    from_id       TEXT        NOT NULL,
    to_kind       TEXT        NOT NULL,
    to_id         TEXT        NOT NULL,
    label         TEXT,
    note          TEXT,
    -- The core `Author` sum type, split across two columns: a discriminator
    -- ('user' | 'agent') and the referenced principal id. Kept as a plain UUID
    -- (no FK) because it points at one of two tables depending on `author_kind`.
    author_kind   TEXT        NOT NULL,
    author_id     UUID        NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Dedup identical directed+labeled links within a workspace. `COALESCE(label,'')`
-- folds a NULL label to '' so two *unlabeled* links between the same ordered pair
-- also collapse (a plain UNIQUE would treat NULLs as distinct and allow dups).
CREATE UNIQUE INDEX links_uniq_idx
    ON links (workspace_id, from_kind, from_id, to_kind, to_id, COALESCE(label, ''));

-- "What is linked *from* X" and "what is linked *to* X" — the two directions of
-- the `list_for` lookup (a UNION over both).
CREATE INDEX links_from_idx ON links (workspace_id, from_kind, from_id);
CREATE INDEX links_to_idx   ON links (workspace_id, to_kind, to_id);
