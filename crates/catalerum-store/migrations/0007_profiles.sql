-- catalerum-store — M4 user profile (personalization, SOUL §5, §22).
--
-- A profile is a structured, per-user record (timezone, working hours,
-- preferences, relationships, defaults) that is **injected into the chat system
-- prompt on every turn** so the assistant personalizes its answers. One row per
-- (workspace, user); the free-form `fields` JSONB object is set/merged via the
-- `update_profile` tool and the API. Every field is inspectable/editable — never
-- hidden state (principle 16). Workspace-scoped (§18).

-- ---------------------------------------------------------------------------
-- profiles — per-user structured fields. The composite primary key
-- (`workspace_id`, `user_id`) makes a member's profile unique within a
-- workspace; `fields` is a flat JSON object of arbitrary key→value pairs,
-- merged on update (top-level keys, right wins).
-- ---------------------------------------------------------------------------
CREATE TABLE profiles (
    workspace_id  UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    user_id       UUID        NOT NULL,
    fields        JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, user_id)
);
