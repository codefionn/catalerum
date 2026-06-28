-- catalerum-store — M4 memories (personalization, SOUL §5, §22).
--
-- A memory is a durable, free-text fact the assistant curates about you ("prefers
-- morning meetings", "ships on Fridays"). Scoped `user` (private to one member)
-- or `workspace` (shared). Stored in Postgres as the source of truth; later
-- embedded into Qdrant for semantic recall and recalled into context when
-- relevant (that derivation is rebuildable and lands in a later slice). Every
-- memory is an inspectable, editable row — never hidden state (principle 16).
-- Every row carries `workspace_id` (the tenancy boundary, §18); all repository
-- queries are workspace-filtered.

-- ---------------------------------------------------------------------------
-- memories — a curated free-text fact. `scope` is 'user' | 'workspace'; for a
-- 'user' memory `user_id` names the member it is private to (NULL for
-- 'workspace'). `source_kind`/`source_id` optionally record the core `SourceRef`
-- the memory was derived from (e.g. a conversation), split like `documents`.
-- `point_id` is the Qdrant handle once embedded (NULL until then).
-- ---------------------------------------------------------------------------
CREATE TABLE memories (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    scope         TEXT        NOT NULL,
    user_id       UUID,
    text          TEXT        NOT NULL,
    source_kind   TEXT,
    source_id     TEXT,
    point_id      UUID,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Recall hot path: a workspace's memories, most-recent first, with the
-- user-visibility filter (workspace-scoped + the acting user's private ones).
CREATE INDEX memories_workspace_created_idx ON memories (workspace_id, created_at DESC);
CREATE INDEX memories_workspace_user_idx ON memories (workspace_id, user_id);
