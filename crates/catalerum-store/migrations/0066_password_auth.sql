-- Optional local password authentication. Disabled by default in distributed
-- deployments; the all-in-one profile enables it and requires the one-time,
-- race-safe bootstrap below.
CREATE TABLE password_credentials (
    user_id       UUID PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    password_hash TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE instance_bootstrap (
    singleton      SMALLINT PRIMARY KEY CHECK (singleton = 1),
    initialized_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    initialized_by UUID NOT NULL REFERENCES users(id)
);
