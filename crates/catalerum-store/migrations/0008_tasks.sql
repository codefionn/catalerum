-- catalerum-store — M5 tasks & Kanban board (SOUL §5, §24).
--
-- A board has ordered columns (e.g. Backlog → To-do → Doing → Done) holding
-- tasks (markdown body, optional assignee = user or agent). Tasks are created by
-- the user or the LLM/automations and worked one-by-one: an agent pulls the next
-- task from a column, does the work within its grant, and completes it. Every row
-- carries `workspace_id` (the tenancy boundary, §18); all repository queries are
-- workspace-filtered.

-- ---------------------------------------------------------------------------
-- boards — a named Kanban board.
-- ---------------------------------------------------------------------------
CREATE TABLE boards (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name          TEXT        NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- columns — a board's ordered stages. `ordinal` is the sort position; deleting a
-- board drops its columns (and their tasks, via the tasks FK below).
-- ---------------------------------------------------------------------------
CREATE TABLE board_columns (
    id            UUID PRIMARY KEY,
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    board_id      UUID        NOT NULL REFERENCES boards (id) ON DELETE CASCADE,
    name          TEXT        NOT NULL,
    ordinal       INTEGER     NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX board_columns_board_idx ON board_columns (board_id, ordinal);

-- ---------------------------------------------------------------------------
-- tasks — a card in a column. `assignee_kind`/`assignee_id` model the optional
-- core `Author` (a user or an agent, §5); `ordinal` is the sort position within
-- the column; `status` is the lifecycle (`open`|`in_progress`|`blocked`|`done`),
-- parallel to the column stage. Deleting a column or board cascades.
-- ---------------------------------------------------------------------------
CREATE TABLE tasks (
    id             UUID PRIMARY KEY,
    workspace_id   UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    board_id       UUID        NOT NULL REFERENCES boards (id) ON DELETE CASCADE,
    column_id      UUID        NOT NULL REFERENCES board_columns (id) ON DELETE CASCADE,
    title          TEXT        NOT NULL,
    body_md        TEXT        NOT NULL DEFAULT '',
    assignee_kind  TEXT,
    assignee_id    UUID,
    ordinal        INTEGER     NOT NULL,
    status         TEXT        NOT NULL DEFAULT 'open',
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- "Next task in a column" + board views, in order.
CREATE INDEX tasks_column_ordinal_idx ON tasks (column_id, ordinal);
CREATE INDEX tasks_board_idx ON tasks (board_id);
