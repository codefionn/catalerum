-- catalerum-store — labels on stored files & directories (SOUL §9).
--
-- A user (or an automation) can tag any path in a store's tree — a **file** or a
-- **directory** — with a free-text `label`. Unlike an `objects` row, a label is
-- keyed by `(store, path)` rather than a catalogue id, so it can tag a directory
-- (which has no object row — directories are implicit prefixes) or a file whose
-- bytes exist but isn't catalogued yet. `is_dir` records which the path is.
--
-- The `store` is the `?store=` selector the path lives on ('' → the default
-- store): a path is only unambiguous within a store (a `docs/x.pdf` can exist on
-- several backends, SOUL §9). `path` is the user-facing key, never the physical
-- `<workspace_id>/…` namespaced one (§18). `author_kind`/`author_id` model the
-- core `Author` sum type (a human `User` or an `Agent`, SOUL §5/§21), split the
-- same way `notes`/`links` store it. Every row carries `workspace_id` (the
-- tenancy boundary, §18) and all repository queries are workspace-filtered.

CREATE TABLE object_labels (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    store         TEXT        NOT NULL DEFAULT '',
    path          TEXT        NOT NULL,
    is_dir        BOOLEAN     NOT NULL DEFAULT false,
    label         TEXT        NOT NULL,
    -- The core `Author` sum type, split across two columns: a discriminator
    -- ('user' | 'agent') and the referenced principal id. Kept as a plain UUID
    -- (no FK) because it points at one of two tables depending on `author_kind`.
    author_kind   TEXT        NOT NULL,
    author_id     UUID        NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- One label per (store, path): re-applying the same label to the same path is
    -- idempotent (add() upserts), never a duplicate.
    CONSTRAINT object_labels_uniq UNIQUE (workspace_id, store, path, label)
);

-- "What labels are on this path" + "list a store's labelled paths under a prefix"
-- (the Files panel badges its tree from this).
CREATE INDEX object_labels_store_path_idx ON object_labels (workspace_id, store, path);
-- "Which paths carry label X" — the label filter.
CREATE INDEX object_labels_label_idx ON object_labels (workspace_id, label);
