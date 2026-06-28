-- catalerum-store — connections uniqueness (SOUL §6.1/§3.4/§18).
--
-- A connection is identified within a workspace by its (kind, name): two
-- connections with the same kind + name in one workspace are duplicates. Enforce
-- that at the DB so get-or-create is **race-free** (the prior application-level
-- find-or-create in the storage catalogue had a TOCTOU window: two concurrent
-- first-uploads could both miss the connection and create two, splitting the
-- bucket/object catalogue). With this constraint, `ConnectionRepo::ensure`'s
-- `INSERT … ON CONFLICT (workspace_id, kind, name) DO UPDATE` converges
-- concurrent callers onto one row, and a duplicate `create` surfaces as a
-- `Conflict` (HTTP 409), consistent with calendars/buckets/events.
ALTER TABLE connections
    ADD CONSTRAINT connections_workspace_kind_name_uq UNIQUE (workspace_id, kind, name);
