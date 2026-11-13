use catalerum_core::id::{CalendarId, ConnectionId, WorkspaceId};
use catalerum_core::model::{Calendar, Connection, ConnectionKind, Cursor};
use sqlx::types::Json;
use crate::error::{Result, StoreError};
use crate::rows::{CalendarRow, connection_kind_to_text, ConnectionRow};
use super::{map, PgPool};

// ===========================================================================
// Connections
// ===========================================================================

/// The full set of `connections` columns, in row order (for `SELECT`/`RETURNING`).
const CONNECTION_COLS: &str =
    "id, workspace_id, kind, name, credential_ref, config, sync_token, created_at, updated_at";

/// CRUD for the `connections` table — provider connections (calendar / storage /
/// channel, SOUL §6.1/§8). Per-provider settings (local dir path, CalDAV base
/// URL, …) ride in the `config` JSON blob; the incremental-sync position rides
/// in `sync_token` (the core `Connection::cursor`).
#[derive(Clone, Debug)]
pub struct ConnectionRepo {
    pool: PgPool,
}

impl ConnectionRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a connection in a workspace. `config` is the per-provider settings
    /// blob (defaults to `{}` if `None`); `credential_ref` points into the
    /// secret store (never plaintext, SOUL §13).
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        kind: ConnectionKind,
        name: &str,
        credential_ref: Option<&str>,
        config: Option<serde_json::Value>,
    ) -> Result<Connection> {
        let id = ConnectionId::new().into_uuid();
        let kind_text = connection_kind_to_text(kind)?;
        let config = config.unwrap_or_else(|| serde_json::json!({}));
        let row: ConnectionRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO connections (id, workspace_id, kind, name, credential_ref, config)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING {CONNECTION_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(kind_text)
        .bind(name)
        .bind(credential_ref)
        .bind(Json(config))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Get-or-create a connection by `(workspace_id, kind, name)`. Idempotent and
    /// **race-free** at the DB level (the `connections_workspace_kind_name_uq`
    /// constraint): concurrent callers converge on one row instead of the
    /// application-level find-or-create's TOCTOU window. On conflict the existing
    /// row's id is preserved and `credential_ref` is set only if the row had none
    /// (`COALESCE`), so a re-ensure never clobbers a stored secret. `config` is
    /// **refreshed when provided** (`Some`) and **preserved when omitted** (`None`,
    /// also `COALESCE`): a caller that re-ensures with new settings (e.g. the email
    /// driver moving a Maildir's `root`) sees them take effect, while a caller that
    /// only ensures existence (e.g. the storage catalogue passing `None`) never
    /// wipes an existing blob. This is the safe get-or-create the storage catalogue
    /// uses before cataloguing an upload (SOUL §9/§3.4).
    pub async fn ensure(
        &self,
        workspace_id: WorkspaceId,
        kind: ConnectionKind,
        name: &str,
        credential_ref: Option<&str>,
        config: Option<serde_json::Value>,
    ) -> Result<Connection> {
        let id = ConnectionId::new().into_uuid();
        let kind_text = connection_kind_to_text(kind)?;
        // `$6` is NULL when no config is supplied: the INSERT defaults it to `{}`,
        // and the conflict UPDATE keeps the existing config (COALESCE). When a
        // config IS supplied it both inserts and refreshes the stored config.
        let config = config.map(Json);
        #[cfg(not(feature = "sqlite"))]
        let statement = format!(
            "INSERT INTO connections (id, workspace_id, kind, name, credential_ref, config)
             VALUES ($1, $2, $3, $4, $5, COALESCE($6::jsonb, '{{}}'::jsonb))
             ON CONFLICT (workspace_id, kind, name) DO UPDATE SET
                 credential_ref = COALESCE(EXCLUDED.credential_ref, connections.credential_ref),
                 config = COALESCE($6::jsonb, connections.config),
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {CONNECTION_COLS}"
        );
        #[cfg(feature = "sqlite")]
        let statement = format!(
            "INSERT INTO connections (id, workspace_id, kind, name, credential_ref, config)
             VALUES ($1, $2, $3, $4, $5, COALESCE($6, '{{}}'))
             ON CONFLICT (workspace_id, kind, name) DO UPDATE SET
                 credential_ref = COALESCE(EXCLUDED.credential_ref, connections.credential_ref),
                 config = COALESCE($6, connections.config),
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {CONNECTION_COLS}"
        );
        let row: ConnectionRow = sqlx::query_as(sqlx::AssertSqlSafe(statement))
            .bind(id)
            .bind(workspace_id.into_uuid())
            .bind(kind_text)
            .bind(name)
            .bind(credential_ref)
            .bind(config)
            .fetch_one(&self.pool)
            .await
            .map_err(map)?;
        row.try_into()
    }

    /// Reconcile a config-defined connection by `(workspace_id, kind, name)`.
    /// Unlike [`Self::ensure`], the credential is replaced **exactly**, including
    /// clearing it when `credential_ref` is `None`. Immutable deployment config
    /// is authoritative for a same-named connection, while the row identity is
    /// preserved so automation triggers and migration ledgers keep their foreign
    /// keys across restarts.
    pub async fn reconcile_configured(
        &self,
        workspace_id: WorkspaceId,
        kind: ConnectionKind,
        name: &str,
        credential_ref: Option<&str>,
        config: serde_json::Value,
    ) -> Result<Connection> {
        let id = ConnectionId::new().into_uuid();
        let kind_text = connection_kind_to_text(kind)?;
        #[cfg(not(feature = "sqlite"))]
        let statement = format!(
            "INSERT INTO connections (id, workspace_id, kind, name, credential_ref, config)
             VALUES ($1, $2, $3, $4, $5, $6::jsonb)
             ON CONFLICT (workspace_id, kind, name) DO UPDATE SET
                 credential_ref = EXCLUDED.credential_ref,
                 config = EXCLUDED.config,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {CONNECTION_COLS}"
        );
        #[cfg(feature = "sqlite")]
        let statement = format!(
            "INSERT INTO connections (id, workspace_id, kind, name, credential_ref, config)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (workspace_id, kind, name) DO UPDATE SET
                 credential_ref = EXCLUDED.credential_ref,
                 config = EXCLUDED.config,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {CONNECTION_COLS}"
        );
        let row: ConnectionRow = sqlx::query_as(sqlx::AssertSqlSafe(statement))
            .bind(id)
            .bind(workspace_id.into_uuid())
            .bind(kind_text)
            .bind(name)
            .bind(credential_ref)
            .bind(Json(config))
            .fetch_one(&self.pool)
            .await
            .map_err(map)?;
        row.try_into()
    }

    /// Fetch a connection, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: ConnectionId) -> Result<Connection> {
        let row: ConnectionRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {CONNECTION_COLS} FROM connections
             WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch the full row (including store-only `config`/timestamps), scoped to
    /// its workspace. Use this when the `config` blob or timestamps are needed.
    pub async fn get_row(
        &self,
        workspace_id: WorkspaceId,
        id: ConnectionId,
    ) -> Result<ConnectionRow> {
        sqlx::query_as(assert_sql_safe!(
            "SELECT {CONNECTION_COLS} FROM connections
             WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)
    }

    /// List a workspace's connections, newest first.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Connection>> {
        let rows: Vec<ConnectionRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {CONNECTION_COLS} FROM connections
             WHERE workspace_id = $1
             ORDER BY created_at DESC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Connection::try_from).collect()
    }

    /// Replace a connection's `name` + per-provider `config` blob, workspace-scoped
    /// (SOUL §28 — editing an existing source from the workbench). Leaves
    /// `credential_ref` and `sync_token` untouched; a caller that lets users omit
    /// unchanged secrets must merge them from the existing blob **before** calling
    /// (this is a full config replace). Returns the updated connection.
    pub async fn update_named_config(
        &self,
        workspace_id: WorkspaceId,
        id: ConnectionId,
        name: &str,
        config: serde_json::Value,
    ) -> Result<Connection> {
        let row: ConnectionRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE connections SET name = $3, config = $4, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {CONNECTION_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(name)
        .bind(Json(config))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Update the incremental-sync cursor (`sync_token`), workspace-scoped. Pass
    /// `None` to clear it (e.g. to force a full re-sync). Returns the updated
    /// connection.
    pub async fn update_cursor(
        &self,
        workspace_id: WorkspaceId,
        id: ConnectionId,
        cursor: Option<&Cursor>,
    ) -> Result<Connection> {
        let token = cursor.map(|c| c.0.as_str());
        let row: ConnectionRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE connections SET sync_token = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {CONNECTION_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(token)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Set (or clear) a connection's Google push **watch state** under the
    /// `"watch"` key of its `config` JSONB, workspace-scoped (SOUL §8/§16 M7 push
    /// half). This rides the existing `config` blob rather than a new column: the
    /// value is provider-managed runtime state (the live `events.watch` channel id /
    /// resource id / expiry) that the calendar factory ignores, kept beside the
    /// user-set provider keys. `Some(v)` writes/replaces `config.watch = v`;
    /// `None` removes the key (a stopped/absent watch). Atomic per row (a single
    /// `jsonb_set` / `-` update), so it never races the OAuth callback's
    /// `credential_ref` seal (which touches only the secret store). Returns the
    /// updated connection.
    #[cfg(not(feature = "sqlite"))]
    pub async fn set_watch_state(
        &self,
        workspace_id: WorkspaceId,
        id: ConnectionId,
        watch: Option<serde_json::Value>,
    ) -> Result<Connection> {
        // `jsonb_set(config, '{watch}', $3, true)` inserts-or-replaces the key;
        // `config - 'watch'` removes it. Branch on presence so a clear truly drops
        // the key (rather than storing `null`, which the scan would misread as live).
        let row: ConnectionRow = match watch {
            Some(v) => sqlx::query_as(assert_sql_safe!(
                "UPDATE connections
                    SET config = jsonb_set(COALESCE(config, '{{}}'::jsonb), '{{watch}}', $3::jsonb, true),
                        updated_at = CURRENT_TIMESTAMP
                  WHERE id = $1 AND workspace_id = $2
                  RETURNING {CONNECTION_COLS}"
            ))
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .bind(Json(v))
            .fetch_one(&self.pool)
            .await
            .map_err(map)?,
            None => sqlx::query_as(assert_sql_safe!(
                "UPDATE connections
                    SET config = COALESCE(config, '{{}}'::jsonb) - 'watch',
                        updated_at = CURRENT_TIMESTAMP
                  WHERE id = $1 AND workspace_id = $2
                  RETURNING {CONNECTION_COLS}"
            ))
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .fetch_one(&self.pool)
            .await
            .map_err(map)?,
        };
        row.try_into()
    }

    /// SQLite JSON1 implementation of [`Self::set_watch_state`]. Keeping this as
    /// a compile-time specialization avoids runtime dialect branches.
    #[cfg(feature = "sqlite")]
    pub async fn set_watch_state(
        &self,
        workspace_id: WorkspaceId,
        id: ConnectionId,
        watch: Option<serde_json::Value>,
    ) -> Result<Connection> {
        let row: ConnectionRow = match watch {
            Some(value) => sqlx::query_as(assert_sql_safe!(
                "UPDATE connections
                    SET config = json_set(COALESCE(config, '{{}}'), '$.watch', json($3)),
                        updated_at = CURRENT_TIMESTAMP
                  WHERE id = $1 AND workspace_id = $2
                  RETURNING {CONNECTION_COLS}"
            ))
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .bind(Json(value))
            .fetch_one(&self.pool)
            .await
            .map_err(map)?,
            None => sqlx::query_as(assert_sql_safe!(
                "UPDATE connections
                    SET config = json_remove(COALESCE(config, '{{}}'), '$.watch'),
                        updated_at = CURRENT_TIMESTAMP
                  WHERE id = $1 AND workspace_id = $2
                  RETURNING {CONNECTION_COLS}"
            ))
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .fetch_one(&self.pool)
            .await
            .map_err(map)?,
        };
        row.try_into()
    }

    /// Point a connection at a sealed credential (its `credential_ref`), or clear it
    /// (`None`), workspace-scoped. This is the minimal set-credential mutation the
    /// Google flows need: the OAuth callback uses it to **re-authorize a
    /// `credential_ref`-less connection in place** (sealing a fresh secret and
    /// pointing the existing row at it, rather than creating a duplicate), and
    /// opportunistic Gmail resealing uses it to swap a legacy plaintext connection
    /// onto the encrypted store (SOUL §13/§28). Touches **only** the `credential_ref`
    /// column, never `config`, so it never races the `config`-only writers
    /// (`set_watch_state` / `scrub_config_keys`) — mirroring `set_watch_state`'s
    /// column-scoped discipline. Returns the updated connection.
    pub async fn set_credential_ref(
        &self,
        workspace_id: WorkspaceId,
        id: ConnectionId,
        credential_ref: Option<&str>,
    ) -> Result<Connection> {
        let row: ConnectionRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE connections SET credential_ref = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {CONNECTION_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(credential_ref)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Remove the named top-level keys from a connection's `config` JSONB,
    /// workspace-scoped — the scoped, additive mutation opportunistic Gmail resealing
    /// uses to strip the now-redundant plaintext OAuth credentials
    /// (`client_id`/`client_secret`/`refresh_token`) once they've been sealed
    /// (SOUL §13/§28). Deliberately **targeted** (a single `config - '{keys}'::text[]`
    /// delete) rather than a whole-blob overwrite: it removes only the named keys and
    /// leaves every other key intact, so it can't clobber a concurrent additive
    /// `config` writer (the `set_watch_state` `jsonb_set` path) the way an
    /// `update_config(id, whole_json)` would — it mirrors that additive discipline.
    /// A key that isn't present is a no-op; passing no keys leaves `config`
    /// unchanged. Returns the updated connection.
    #[cfg(not(feature = "sqlite"))]
    pub async fn scrub_config_keys(
        &self,
        workspace_id: WorkspaceId,
        id: ConnectionId,
        keys: &[&str],
    ) -> Result<Connection> {
        // `jsonb - text[]` deletes every matching top-level key in one shot (leaving
        // the rest of the blob untouched), so this is a single atomic row update.
        let keys: Vec<String> = keys.iter().map(|k| (*k).to_string()).collect();
        let row: ConnectionRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE connections
                SET config = COALESCE(config, '{{}}'::jsonb) - $3::text[],
                    updated_at = CURRENT_TIMESTAMP
              WHERE id = $1 AND workspace_id = $2
              RETURNING {CONNECTION_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&keys)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    #[cfg(feature = "sqlite")]
    pub async fn scrub_config_keys(
        &self,
        workspace_id: WorkspaceId,
        id: ConnectionId,
        keys: &[&str],
    ) -> Result<Connection> {
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "UPDATE connections SET config = json_remove(COALESCE(config, '{}')",
        );
        for key in keys {
            // Provider config keys are identifiers, but quoting the JSON path is
            // still required for punctuation and makes this safe for arbitrary
            // future keys. JSON encoding supplies the correct escaping.
            let quoted = serde_json::to_string(key).map_err(StoreError::invalid)?;
            query.push(", ").push_bind(format!("$.{quoted}"));
        }
        query
            .push("), updated_at = CURRENT_TIMESTAMP WHERE id = ")
            .push_bind(id.into_uuid())
            .push(" AND workspace_id = ")
            .push_bind(workspace_id.into_uuid())
            .push(format!(" RETURNING {CONNECTION_COLS}"));
        let row: ConnectionRow = query
            .build_query_as()
            .fetch_one(&self.pool)
            .await
            .map_err(map)?;
        row.try_into()
    }

    /// Delete a connection (cascades to its calendars and events),
    /// workspace-scoped.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: ConnectionId) -> Result<()> {
        let res = sqlx::query("DELETE FROM connections WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }
}

// ===========================================================================
// Calendars
// ===========================================================================

/// The full set of `calendars` columns, in row order.
const CALENDAR_COLS: &str =
    "id, workspace_id, connection_id, external_id, name, read_only, created_at";

/// CRUD for the `calendars` table (SOUL §6.1/§8). Calendars belong to a
/// connection; `upsert` is keyed on `(connection_id, external_id)` so
/// re-listing a provider's calendars is idempotent.
#[derive(Clone, Debug)]
pub struct CalendarRepo {
    pool: PgPool,
}

impl CalendarRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Upsert a calendar by `(connection_id, external_id)`. On conflict the
    /// `name` and `read_only` flag are refreshed; the id is preserved. Idempotent
    /// (SOUL §3.4): re-listing the provider's calendars never duplicates.
    pub async fn upsert(
        &self,
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        external_id: &str,
        name: &str,
        read_only: bool,
    ) -> Result<Calendar> {
        let id = CalendarId::new().into_uuid();
        let row: CalendarRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO calendars (id, workspace_id, connection_id, external_id, name, read_only)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (connection_id, external_id)
             DO UPDATE SET name = EXCLUDED.name, read_only = EXCLUDED.read_only
             RETURNING {CALENDAR_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(connection_id.into_uuid())
        .bind(external_id)
        .bind(name)
        .bind(read_only)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Get-or-create a **local** (database-native) calendar — one with no
    /// provider connection (`connection_id IS NULL`) that is read-write and
    /// never synced (SOUL §8/§11). Idempotent per `(workspace_id, external_id)`
    /// via the local partial unique index (migration `0018`): a repeated call
    /// with the same `external_id` returns the same calendar with its `name`
    /// refreshed, so a default calendar (`external_id = "default"`) is safe to
    /// ensure on every write. Always `read_only = false`.
    pub async fn upsert_local(
        &self,
        workspace_id: WorkspaceId,
        external_id: &str,
        name: &str,
    ) -> Result<Calendar> {
        let id = CalendarId::new().into_uuid();
        let row: CalendarRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO calendars (id, workspace_id, connection_id, external_id, name, read_only)
             VALUES ($1, $2, NULL, $3, $4, FALSE)
             ON CONFLICT (workspace_id, external_id) WHERE connection_id IS NULL
             DO UPDATE SET name = EXCLUDED.name
             RETURNING {CALENDAR_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(external_id)
        .bind(name)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a calendar, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: CalendarId) -> Result<Calendar> {
        let row: CalendarRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {CALENDAR_COLS} FROM calendars
             WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// List the calendars belonging to a connection, workspace-scoped.
    pub async fn list_by_connection(
        &self,
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
    ) -> Result<Vec<Calendar>> {
        let rows: Vec<CalendarRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {CALENDAR_COLS} FROM calendars
             WHERE workspace_id = $1 AND connection_id = $2
             ORDER BY name ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(connection_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Calendar::from).collect())
    }

    /// List all calendars in a workspace.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Calendar>> {
        let rows: Vec<CalendarRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {CALENDAR_COLS} FROM calendars
             WHERE workspace_id = $1
             ORDER BY name ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Calendar::from).collect())
    }

    /// Delete a calendar (cascades to its events), workspace-scoped.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: CalendarId) -> Result<()> {
        let res = sqlx::query("DELETE FROM calendars WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Record that a provider calendar `(connection_id, external_id)` was removed
    /// by the user, so the sync paths won't re-`upsert` it (migration `0057`).
    /// Idempotent (a repeated exclude is a no-op). The row is cleared by the FK
    /// cascade when its connection is deleted, so removing + re-adding a source
    /// resurfaces the calendar. See [`Self::excluded_external_ids`].
    pub async fn exclude(
        &self,
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        external_id: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO calendar_exclusions (workspace_id, connection_id, external_id)
             VALUES ($1, $2, $3)
             ON CONFLICT (connection_id, external_id) DO NOTHING",
        )
        .bind(workspace_id.into_uuid())
        .bind(connection_id.into_uuid())
        .bind(external_id)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(())
    }

    /// The `external_id`s a connection's sync must skip re-creating — the
    /// user-deleted provider calendars recorded by [`Self::exclude`]. Empty for a
    /// connection with no exclusions; both `catalerum-ingest` sync paths consult
    /// this before upserting a listed provider calendar.
    pub async fn excluded_external_ids(
        &self,
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
    ) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT external_id FROM calendar_exclusions
             WHERE workspace_id = $1 AND connection_id = $2",
        )
        .bind(workspace_id.into_uuid())
        .bind(connection_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(|(e,)| e).collect())
    }
}
