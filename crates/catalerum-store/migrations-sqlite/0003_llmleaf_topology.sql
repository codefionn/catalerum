CREATE TABLE llmleaf_topology (
    kind       TEXT NOT NULL CHECK (kind IN ('provider', 'route')),
    name       TEXT NOT NULL,
    spec       TEXT NOT NULL,
    enabled    INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (kind, name)
);
