CREATE TABLE llmleaf_topology (
    kind       TEXT NOT NULL CHECK (kind IN ('provider', 'route')),
    name       TEXT NOT NULL,
    spec       JSONB NOT NULL,
    enabled    BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (kind, name)
);
