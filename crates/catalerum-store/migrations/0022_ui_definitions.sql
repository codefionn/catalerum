-- catalerum-store — emerged UIs: AI-authored declarative component trees.
--
-- An "emerged UI" is a typed, closed-vocabulary JSON component tree (`UiSpec`,
-- in catalerum-core::model_ui) that the AI creates and edits through tools and
-- the Leptos web app renders with one generic interpreter (SOUL §5/§12, §15:
-- a new capability is a new tool over existing surface). Postgres is the source
-- of truth; transient UI state (current view, open dialogs, in-progress inputs)
-- is client-side and deliberately NOT stored here in v1.
--
-- Every row carries `workspace_id` (the tenancy boundary, SOUL §18) and all
-- repository queries are workspace-filtered.
--
-- APPEND-ONLY ENUM RULE: the spec's closed enums (NodeKind, EventName, Handler,
-- ClientOp, ValidationKind, UiPatchOp) are stored inside `definition` JSONB and
-- reject unknown variants on load. Never rename/remove a variant without a
-- blob-rewriting migration gated on `spec_version` — old specs would otherwise
-- silently fail to deserialize.

-- ---------------------------------------------------------------------------
-- ui_definitions — one emerged UI. `author_kind`/`author_id` model the core
-- `Author` sum type (a human `User` or an `Agent`), like notes. `name` is an
-- optional slug, UNIQUE-when-set per workspace (the Apps panel handle).
-- `spec_version` is the JSONB format version; `version` is an optimistic
-- edit-concurrency counter bumped on every patch (distinct from any UI state).
-- ---------------------------------------------------------------------------
CREATE TABLE ui_definitions (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    author_kind   TEXT        NOT NULL,
    author_id     UUID        NOT NULL,
    name          TEXT,
    title         TEXT        NOT NULL,
    description   TEXT,
    spec_version  INTEGER     NOT NULL DEFAULT 1,
    version       BIGINT      NOT NULL DEFAULT 1,
    definition    JSONB       NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A named UI is unique within its workspace; anonymous (NULL name) inline UIs
-- are identified solely by their UUID, so the uniqueness is partial.
CREATE UNIQUE INDEX ui_definitions_ws_name_idx
    ON ui_definitions (workspace_id, name) WHERE name IS NOT NULL;

-- List hot path: a workspace's UIs, most-recently-edited first.
CREATE INDEX ui_definitions_ws_updated_idx
    ON ui_definitions (workspace_id, updated_at DESC);
