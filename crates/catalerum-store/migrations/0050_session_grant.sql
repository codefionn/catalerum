-- catalerum-store — scope a bearer token to a named §19 grant (SOUL §19/§26).
--
-- A minted service/API-key token (a `sessions` row) may be **scoped** to a named
-- capability grant, so an external MCP client (Claude Code / Codex / opencode)
-- holds the grant's attenuated authority instead of the minting user's full role.
-- The token-issuance surface enforces the attenuation invariant at mint time (the
-- grant must be ⊆ the caller's authority); this column records which grant a
-- session is bound to. `NULL` = a role-derived session (today's default).
--
-- Same-workspace + fail-closed by construction:
--   * the FK is composite on `(workspace_id, grant_id)` → `grants (workspace_id,
--     id)`, so a session can only reference a grant in its **own** workspace (§18
--     defense-in-depth); a NULL `grant_id` skips the check (MATCH SIMPLE), leaving
--     a role-derived session unconstrained.
--   * `ON DELETE CASCADE` (NOT `SET NULL`, unlike `automations.grant_id`): a token
--     with **no** grant means the minter's FULL role authority, so nulling a
--     deleted grant would silently *escalate* the token. Instead, deleting a grant
--     cascade-revokes every token scoped to it — it simply stops verifying. The
--     API additionally re-resolves the grant on every request and fails closed if
--     it is gone, so the invariant holds even on the in-memory store (no FK there).
ALTER TABLE sessions ADD COLUMN grant_id UUID;

ALTER TABLE sessions
    ADD CONSTRAINT sessions_grant_id_fk
    FOREIGN KEY (workspace_id, grant_id) REFERENCES grants (workspace_id, id)
    ON DELETE CASCADE;

CREATE INDEX sessions_grant_id_idx ON sessions (grant_id);
