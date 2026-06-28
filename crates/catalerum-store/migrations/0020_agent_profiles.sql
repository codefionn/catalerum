-- catalerum-store — §19 agent profiles (SOUL §5/§19/§25).
--
-- An `agent_profile` is the **durable, named** form of the §19 scoped agent: a
-- reusable configuration bundling a model choice, a system prompt, an allowed
-- tool/skill set, the **subagents** it may delegate to, the **channels** it
-- listens on, and the §19 `grant` that is its authority — all within one
-- workspace (the tenancy + data boundary, §18). It exists so *separate, securely
-- scoped data access* is a first-class object: each profile can hold a different,
-- attenuated grant, and a parent profile may only delegate to a subagent whose
-- grant is ⊆ its own (enforced at delegation time, not here).
--
-- The tool / skill / subagent / channel sets are JSONB arrays of **names**
-- (matching how skills, automation `LlmAgent` actions, and channel inbound
-- routing are all name-keyed within a workspace). `(workspace_id, name)` is
-- UNIQUE — a profile is referenced by name. Every row carries `workspace_id`;
-- all repository queries are workspace-filtered (§18).

CREATE TABLE agent_profiles (
    id             UUID        PRIMARY KEY,
    workspace_id   UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- Human-readable name, unique within the workspace.
    name           TEXT        NOT NULL,
    -- Model id to run against; NULL uses the workspace default model.
    model          TEXT,
    -- System prompt; NULL uses the default agent system prompt.
    system_prompt  TEXT,
    -- JSONB array of tool names the profile may dispatch (a subset of the
    -- registry); empty = advertise the whole registry (the grant still bounds it).
    tools          JSONB       NOT NULL DEFAULT '[]'::jsonb,
    -- JSONB array of skill names whose runbooks seed the system prompt.
    skills         JSONB       NOT NULL DEFAULT '[]'::jsonb,
    -- JSONB array of agent-profile names this profile may delegate to. A subagent
    -- runs under its own grant, enforced ⊆ this profile's grant (attenuation §19).
    subagents      JSONB       NOT NULL DEFAULT '[]'::jsonb,
    -- JSONB array of channel names this profile listens on: an inbound message on
    -- one routes to this profile's agent loop (SOUL §25).
    channels       JSONB       NOT NULL DEFAULT '[]'::jsonb,
    -- The §19 grant that is this profile's authority. NULL runs under bounded
    -- base-Member capabilities (the interim default).
    grant_id       UUID,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, name),
    -- Parity with `grants`: a unique key on exactly the composite-FK target so a
    -- (future) same-workspace FK can reference a profile.
    UNIQUE (workspace_id, id),
    -- Same-workspace composite FK (§18 defense-in-depth): a profile can only
    -- reference a grant in its **own** workspace. `MATCH SIMPLE` (default) skips
    -- the check on a NULL `grant_id`, so a grantless profile is allowed.
    -- `ON DELETE SET NULL (grant_id)` nulls only `grant_id` on grant delete
    -- (never the NOT-NULL `workspace_id`), detaching rather than dangling.
    CONSTRAINT agent_profiles_grant_id_fk
        FOREIGN KEY (workspace_id, grant_id) REFERENCES grants (workspace_id, id)
        ON DELETE SET NULL (grant_id)
);

CREATE INDEX agent_profiles_workspace_idx ON agent_profiles (workspace_id);
