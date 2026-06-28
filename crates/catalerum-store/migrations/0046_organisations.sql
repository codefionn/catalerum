-- catalerum-store — organisations above workspaces (SOUL §5/§6.1/§18).
--
-- An **organisation** is the administrative grouping above the tenancy boundary.
-- Every workspace belongs to exactly one organisation. Org membership + roles
-- (Owner / Admin / Member) govern administration only — creating/archiving
-- workspaces in the org, managing org members, org-level policy — and confer
-- **no** data access: the workspace stays the sole data + capability boundary
-- (SOUL §18/§19), and organisations never appear in capability strings.
--
-- This migration is additive + backfill-safe for existing deployments:
--   1. create `organisations` + `org_memberships`;
--   2. seed a well-known **default organisation** (slug `default`, a fixed id so
--      the dev seed + `WorkspaceRepo` can reference it without a lookup);
--   3. attach every pre-existing workspace to the default organisation (the
--      "organisation backfill" open question, SOUL §29 — the obvious safe
--      default: one default org holds all legacy workspaces);
--   4. backfill `org_memberships` from existing workspace memberships so nobody
--      is locked out of administering the shell they already owned — each distinct
--      user gets the *highest* org role their workspace roles imply
--      (owner→Owner, admin→Admin, otherwise Member);
--   5. make `workspaces.organisation_id` NOT NULL + FK once backfilled.
--
-- The default org's `workspace_creation` policy is seeded `members` — the
-- single-user / `just dev` default (SOUL §18). Multi-user operators tighten it to
-- `admins` via the org-policy endpoint; newly-created orgs get the mode-derived
-- default at creation time.

-- ---------------------------------------------------------------------------
-- organisations — the administrative grouping above workspaces.
-- ---------------------------------------------------------------------------
CREATE TABLE organisations (
    id                  UUID        PRIMARY KEY,
    name                TEXT        NOT NULL,
    slug                TEXT        NOT NULL UNIQUE,
    -- Deny-by-default org policy: who may create workspaces in this org
    -- ('disabled' | 'admins' | 'members'). Stored as the core `CreationPolicy`
    -- snake_case token.
    workspace_creation  TEXT        NOT NULL DEFAULT 'members',
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- org_memberships — binds a user to an organisation with an administrative role.
-- Mirrors `memberships` (workspace ⇄ user) but for the org shell; a user may hold
-- an org membership without any workspace membership (they administer the shell,
-- never its contents — SOUL §18).
-- ---------------------------------------------------------------------------
CREATE TABLE org_memberships (
    organisation_id  UUID        NOT NULL REFERENCES organisations (id) ON DELETE CASCADE,
    user_id          UUID        NOT NULL REFERENCES users (id)         ON DELETE CASCADE,
    role             TEXT        NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (organisation_id, user_id)
);

CREATE INDEX org_memberships_user_idx ON org_memberships (user_id);

-- The well-known default organisation. Fixed id (mirrors
-- `catalerum_iam::DEFAULT_ORGANISATION_ID`) so the dev seed and `WorkspaceRepo`
-- can attach the default workspace without a slug lookup. Idempotent.
INSERT INTO organisations (id, name, slug, workspace_creation)
VALUES ('def00000-0000-4000-8000-000000000000', 'Default', 'default', 'members')
ON CONFLICT (id) DO NOTHING;

-- ---------------------------------------------------------------------------
-- workspaces.organisation_id — every workspace belongs to one organisation.
-- Added nullable, backfilled to the default org, then locked NOT NULL + FK.
-- ---------------------------------------------------------------------------
ALTER TABLE workspaces
    ADD COLUMN organisation_id UUID;

-- Backfill: attach every pre-existing workspace to the default organisation.
UPDATE workspaces
    SET organisation_id = 'def00000-0000-4000-8000-000000000000'
    WHERE organisation_id IS NULL;

ALTER TABLE workspaces
    ALTER COLUMN organisation_id SET NOT NULL;

ALTER TABLE workspaces
    ADD CONSTRAINT workspaces_organisation_id_fk
    FOREIGN KEY (organisation_id) REFERENCES organisations (id);

CREATE INDEX workspaces_organisation_idx ON workspaces (organisation_id);

-- Backfill org memberships from existing workspace memberships so existing users
-- retain administrative reach over the default org that now holds their
-- workspaces. Each distinct user gets the highest org role their workspace roles
-- imply. New/fresh installs have no memberships yet — this is a no-op there, and
-- the dev seed adds the admin as the default org's Owner (SOUL §17).
INSERT INTO org_memberships (organisation_id, user_id, role)
SELECT 'def00000-0000-4000-8000-000000000000',
       m.user_id,
       CASE
           WHEN bool_or(m.role = 'owner') THEN 'owner'
           WHEN bool_or(m.role = 'admin') THEN 'admin'
           ELSE 'member'
       END
FROM memberships m
GROUP BY m.user_id
ON CONFLICT (organisation_id, user_id) DO NOTHING;
