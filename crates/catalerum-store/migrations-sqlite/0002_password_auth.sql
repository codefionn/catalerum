CREATE TABLE password_credentials (
    user_id       BLOB PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    password_hash TEXT NOT NULL,
    created_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE instance_bootstrap (
    singleton      INTEGER PRIMARY KEY CHECK (singleton = 1),
    initialized_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    initialized_by BLOB NOT NULL REFERENCES users(id)
);
