-- catalerum-store — small instance-wide operational settings.
--
-- Most catalerum state is workspace-scoped tenant data. A few deployment-level
-- facts apply to the running installation as a whole. Keep those bounded JSON
-- documents here rather than smuggling them into a workspace or user profile.

CREATE TABLE instance_settings (
    key         TEXT        PRIMARY KEY,
    value       JSONB       NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
