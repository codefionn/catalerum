//! External-database **collect** jobs (SOUL §11/§19) — the SQL twin of
//! [`crate::collect`].
//!
//! A `CollectSql` trigger (filled with an external **Postgres** connection) is a
//! poll source over **tables**: the scheduler enqueues a [`JOB_KIND_COLLECT_SQL`]
//! job on the trigger's `every` cadence, and a worker holding an
//! [`AutomationContext`] runs [`run_collect_sql`]. That discovers every table
//! matching the trigger's `tables` wildcard pattern (so tables created *later*
//! join automatically), pulls rows inserted past each table's committed cursor,
//! and **fires one automation run per new row** — the row rides the run's
//! trigger event (`trigger.row.<column>`). This closes the emerged-UI loop: an
//! App `INSERT`s via the `sql_query` tool and an automation reacts per row.
//!
//! ## What "new" means
//! Each table needs a **monotonically increasing cursor column** — an
//! auto-increment integer (sequence/identity, preferring the primary key) or an
//! insertion timestamp (`created_at`-style), auto-detected unless the trigger
//! pins `cursor_column`. A table with neither is skipped with a warning. The
//! **first** poll of a table *anchors* its cursor at the current maximum and
//! fires nothing — "new inserts" means new after wiring, never a history replay.
//! Updates/deletes are invisible by design.
//!
//! ## Ledger / commit / idempotency
//! The same per-source committed-prefix ledger as email/calendar collect
//! ([`CollectLedger`], packed into the connection's `sync_token`), keyed by the
//! qualified table name. Rows are pulled `>=` the cursor and deduped by a
//! `cursor-value ␟ row-uid` committed entry, so equal cursor values (timestamp
//! ties) re-emit at the boundary but never re-run. The cursor advances over a
//! table's batch only once every fired row **committed** (`commit_on` semantics
//! exactly as on `CollectEmail`); the committed set is then pruned to the
//! boundary entries the `>=` re-pull would re-emit. Two automations polling the
//! same connection share this ledger (the same caveat as email collect — the
//! worker's per-connection collect mutex serializes them).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use catalerum_automation::Trigger;
use catalerum_core::capability::{
    allows as capability_allows, Action as CapAction, Capability, Resource,
};
use catalerum_core::id::WorkspaceId;
use catalerum_core::model::Connection;
use catalerum_store::{
    connect_external, sql_run_read, PgPool, PoolConfig, PostgresConnectionConfig, SecretStore,
    Store,
};

use crate::automation::AutomationContext;
use crate::collect::{
    load_enabled, parse_connection, persist_source, run_item, CollectLedger, CollectReport,
};
use crate::error::{IngestError, Result};

/// The `job_queue.kind` token for a SQL-collect job (SOUL §11). Enqueued by the
/// scheduler on a `CollectSql` trigger's cadence; a worker with an
/// [`AutomationContext`] runs [`run_collect_sql`].
pub const JOB_KIND_COLLECT_SQL: &str = "collect_sql";

/// How many matching tables one poll considers (a wildcard like `*` over a huge
/// schema stays bounded; the overflow is logged, never silent).
const TABLE_DISCOVERY_CAP: u64 = 200;
/// How many new rows one poll processes per table (the over-cap tail pins the
/// cursor and drains across later polls).
const ROW_POLL_CAP: usize = 500;
/// Hard per-session `statement_timeout` (ms) for collect polls — the queries
/// here are simple ordered scans; anything slower should fail the job visibly.
const STATEMENT_TIMEOUT_MS: u64 = 15_000;
/// The separator between a committed entry's cursor value and row uid (an ASCII
/// unit separator — never part of a rendered cursor value or a `col=value` uid).
const ENTRY_SEP: char = '\u{1f}';

/// How a cursor column's values compare in the poll's `WHERE` clause. Derived
/// from the column's `information_schema` `data_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorKind {
    /// `smallint`/`integer`/`bigint` — bound as `bigint`.
    Int,
    /// `timestamp with time zone` — bound as text, cast `::timestamptz`.
    TimestampTz,
    /// `timestamp without time zone` — bound as text, cast `::timestamp`.
    Timestamp,
    /// `date` — bound as text, cast `::date`.
    Date,
    /// Anything else — the **column** is cast to text and compared textually
    /// (deterministic, though only insertion-ordered if the author picked a
    /// column whose text order is).
    Text,
}

impl CursorKind {
    fn from_data_type(data_type: &str) -> Self {
        match data_type.trim().to_ascii_lowercase().as_str() {
            "smallint" | "integer" | "bigint" => CursorKind::Int,
            "timestamp with time zone" => CursorKind::TimestampTz,
            "timestamp without time zone" => CursorKind::Timestamp,
            "date" => CursorKind::Date,
            _ => CursorKind::Text,
        }
    }

    /// The `WHERE` fragment comparing the (quoted) cursor column against `$1`
    /// with the given operator (`>=` for the poll, `=` for the anchor boundary).
    fn compare_fragment(self, colq: &str, op: &str) -> String {
        match self {
            CursorKind::Int => format!("{colq} {op} $1::bigint"),
            CursorKind::TimestampTz => format!("{colq} {op} $1::timestamptz"),
            CursorKind::Timestamp => format!("{colq} {op} $1::timestamp"),
            CursorKind::Date => format!("{colq} {op} $1::date"),
            CursorKind::Text => format!("({colq})::text {op} $1"),
        }
    }

    /// The bind parameter for a stored cursor string.
    fn bind_param(self, cursor: &str) -> Value {
        match self {
            CursorKind::Int => cursor
                .parse::<i64>()
                .map(Value::from)
                .unwrap_or_else(|_| Value::String(cursor.to_string())),
            _ => Value::String(cursor.to_string()),
        }
    }
}

/// One column of a matched table, from `information_schema`.
#[derive(Clone, Debug)]
struct ColumnInfo {
    name: String,
    data_type: String,
    is_pk: bool,
    /// Sequence-backed (`nextval(...)` default) or an identity column — the
    /// auto-increment signal.
    is_serial: bool,
}

/// Run a SQL collect job (SOUL §11/§19): open the external Postgres connection,
/// discover the tables matching the trigger's wildcard pattern, and fire one
/// automation run per row inserted past each table's committed cursor.
///
/// # Errors
/// A database/store failure (a retryable job failure — the scheduler re-enqueues
/// next window). A disabled/deleted automation is a no-op (`Ok` empty report); a
/// table without a usable cursor column is skipped with a warning.
pub async fn run_collect_sql(
    store: &Store,
    ctx: &AutomationContext,
    workspace_id: WorkspaceId,
    payload: &crate::collect::CollectPayload,
) -> Result<CollectReport> {
    run_collect_sql_with(store, ctx, workspace_id, payload, None).await
}

/// Like [`run_collect_sql`], but threads the encrypted secret store so a
/// credentialed connection can decrypt its password (SOUL §13). A connection
/// whose `credential_ref` is set **fails closed** without one.
pub async fn run_collect_sql_with(
    store: &Store,
    ctx: &AutomationContext,
    workspace_id: WorkspaceId,
    payload: &crate::collect::CollectPayload,
    secrets: Option<&Arc<SecretStore>>,
) -> Result<CollectReport> {
    let trigger: Trigger = serde_json::from_value(payload.trigger.clone())
        .map_err(|e| IngestError::invalid_job(format!("collect_sql bad trigger: {e}")))?;
    let Trigger::CollectSql {
        connection,
        tables,
        cursor_column,
        commit_on,
        ..
    } = trigger
    else {
        return Err(IngestError::invalid_job(
            "collect_sql job carries a non-CollectSql trigger".to_string(),
        ));
    };
    let connection_id = parse_connection(&connection)?;

    let Some(automation) = load_enabled(store, workspace_id, payload.automation_id).await? else {
        return Ok(CollectReport::default());
    };

    let row = store
        .connections()
        .get_row(workspace_id, connection_id)
        .await?;
    if row.kind != "postgres" {
        return Err(IngestError::invalid_job(format!(
            "collect_sql trigger names connection `{}`, which is a {} connection, not postgres",
            row.name, row.kind
        )));
    }

    // Enforce the collect capability under the automation's recorded grant,
    // before any external I/O (SOUL §11/§19). `db` capability selectors are keyed
    // on the connection **name** (the `sql_query` convention); accept the id
    // selector too so a grant minted either way covers its own connection.
    authorize_collect_db(store, workspace_id, &automation, &row.name, &connection).await?;

    let cfg: PostgresConnectionConfig =
        serde_json::from_value(row.config.0.clone()).map_err(|e| {
            IngestError::invalid_job(format!("invalid postgres connection config: {e}"))
        })?;
    let password = match row.credential_ref.as_deref() {
        Some(reference) => {
            let Some(secrets) = secrets else {
                return Err(IngestError::invalid_job(
                    "collect_sql: connection has an encrypted credential but this worker has \
                     no secret store — set [secrets].master_key"
                        .to_string(),
                ));
            };
            let bytes = secrets.get(workspace_id, reference).await?;
            String::from_utf8(bytes)
                .map_err(|_| IngestError::invalid_job("stored credential is not valid UTF-8"))?
        }
        None => String::new(),
    };
    let spec = cfg.to_spec(password, Some(STATEMENT_TIMEOUT_MS));
    let pool = connect_external(
        &spec,
        &PoolConfig {
            max_connections: 2,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(10),
        },
    )
    .await?;

    let connection_dom: Connection = row.clone().try_into().map_err(IngestError::Store)?;
    let default_schema = cfg.schema.as_deref().unwrap_or("public");
    let matched = discover_tables(&pool, &tables, default_schema).await?;
    if matched.is_empty() {
        tracing::debug!(workspace = %workspace_id, connection = %connection_id,
            pattern = %tables, "collect_sql: no tables match the pattern");
    }

    let mut ledger = CollectLedger::decode(connection_dom.cursor.as_ref());
    let mut report = CollectReport::default();

    for (schema, table) in matched {
        let qualified = format!("{schema}.{table}");
        let columns = table_columns(&pool, &schema, &table).await?;
        let Some((col, kind)) = pick_cursor_column(&columns, cursor_column.as_deref()) else {
            tracing::warn!(workspace = %workspace_id, table = %qualified,
                "collect_sql: no usable cursor column (need a sequence/identity integer \
                 or a created_at-style timestamp, or set `cursor_column`); skipping table");
            continue;
        };
        report.sources += 1;
        let pk_cols: Vec<String> = columns
            .iter()
            .filter(|c| c.is_pk)
            .map(|c| c.name.clone())
            .collect();

        let mut state = ledger.sources.get(&qualified).cloned().unwrap_or_default();

        // First poll: anchor the cursor at the table's current maximum and fire
        // nothing — only rows inserted after wiring are "new". The rows AT the
        // maximum are marked committed: the steady-state pull is `>=` (so a
        // timestamp tie arriving later is never missed), which would otherwise
        // re-emit — and fire — the anchor row itself.
        if !state.initialized {
            state.cursor = max_cursor_value(&pool, &schema, &table, &col, kind).await?;
            if let Some(cursor) = state.cursor.as_deref() {
                for row_json in boundary_rows(&pool, &schema, &table, &col, kind, cursor).await? {
                    state.committed.insert(format!(
                        "{cursor}{ENTRY_SEP}{}",
                        row_uid(&row_json, &pk_cols)
                    ));
                }
            }
            state.initialized = true;
            persist_source(
                store,
                workspace_id,
                connection_id,
                &mut ledger,
                &qualified,
                &state,
            )
            .await?;
            continue;
        }

        let rows = poll_rows(&pool, &schema, &table, &col, kind, state.cursor.as_deref()).await?;
        let drained_fully = rows.len() <= ROW_POLL_CAP;
        let mut all_committed = drained_fully;
        let mut advance_to: Option<String> = None;

        for row_json in rows.into_iter().take(ROW_POLL_CAP) {
            let Some(cursor_value) = row_json.get(&col).and_then(render_cursor) else {
                // A NULL/unrenderable cursor value can never be ordered past —
                // the row is invisible to this trigger (log, don't wedge the cursor).
                tracing::warn!(workspace = %workspace_id, table = %qualified, column = %col,
                    "collect_sql: row with NULL cursor value skipped");
                continue;
            };
            let entry = format!("{cursor_value}{ENTRY_SEP}{}", row_uid(&row_json, &pk_cols));
            if state.committed.contains(&entry) {
                continue;
            }
            let trigger_json = json!({
                "kind": JOB_KIND_COLLECT_SQL,
                "connection": connection,
                "table": qualified,
                "cursor_column": col,
                "row": row_json,
            });
            report.runs_fired += 1;
            let committed = run_item(
                store,
                ctx,
                workspace_id,
                &automation,
                trigger_json,
                commit_on.as_deref(),
            )
            .await?;
            if committed {
                state.committed.insert(entry);
                report.committed += 1;
                advance_to = Some(cursor_value);
            } else {
                all_committed = false;
            }
            // Persist after each row so a crash/redelivery re-runs at most the
            // current row, not the whole table (the §29 at-least-once window).
            persist_source(
                store,
                workspace_id,
                connection_id,
                &mut ledger,
                &qualified,
                &state,
            )
            .await?;
        }

        advance_table(&mut state, all_committed, advance_to);
        persist_source(
            store,
            workspace_id,
            connection_id,
            &mut ledger,
            &qualified,
            &state,
        )
        .await?;
    }

    pool.close().await;
    Ok(report)
}

/// Advance a table's ledger after its batch (the contiguous-prefix rule): the
/// cursor moves to the last committed row's value only when the **whole** drained
/// batch committed; the committed set is then pruned to the entries at the new
/// boundary (the only rows a `>=` re-pull re-emits — everything strictly below
/// can never return, so the set stays bounded).
fn advance_table(
    state: &mut crate::collect::SourceState,
    all_committed: bool,
    advance_to: Option<String>,
) {
    if !all_committed {
        // Leave the cursor before the uncommitted/over-cap tail; keep the
        // committed entries so succeeded rows are skipped on the re-pull.
        return;
    }
    let Some(cursor) = advance_to else {
        return; // no new rows this poll — nothing to move.
    };
    let boundary = format!("{cursor}{ENTRY_SEP}");
    state.committed.retain(|e| e.starts_with(&boundary));
    state.cursor = Some(cursor);
}

/// Enforce `db:read@<connection>` for a SQL collect poll under the automation's
/// recorded grant (SOUL §11/§19) — the same deny shape as
/// `crate::collect::authorize_collect`, keyed on the connection **name** (the
/// `sql_query` capability convention) with the id selector accepted too. No
/// recorded grant → allowed (the run executes under the runner's default bounded
/// authority; Member's base set holds domain-wide `db:read`).
async fn authorize_collect_db(
    store: &Store,
    workspace_id: WorkspaceId,
    automation: &catalerum_core::Automation,
    connection_name: &str,
    connection_ref: &str,
) -> Result<()> {
    let Some(grant_id) = automation.grant_id else {
        return Ok(());
    };
    let grant = match store.grants().get(workspace_id, grant_id).await {
        Ok(g) => g,
        Err(catalerum_store::StoreError::NotFound) => {
            return Err(IngestError::forbidden(format!(
            "collect denied: automation `{}` references grant {grant_id}, which no longer exists",
            automation.name
        )))
        }
        Err(e) => return Err(e.into()),
    };
    let by_name = Capability::new(CapAction::Read, Resource::new("db", connection_name.trim()));
    let by_id = Capability::new(CapAction::Read, Resource::new("db", connection_ref.trim()));
    if capability_allows(&grant, &by_name) || capability_allows(&grant, &by_id) {
        Ok(())
    } else {
        Err(IngestError::forbidden(format!(
            "collect denied: automation `{}`'s grant `{}` does not cover db:read@{} \
             (the connection its collect_sql trigger polls)",
            automation.name,
            grant.name,
            connection_name.trim(),
        )))
    }
}

/// Double-quote a SQL identifier (doubling embedded quotes). The names quoted
/// here come from `information_schema`, so they are real catalog identifiers.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Translate a `*`-wildcard glob into a `LIKE` pattern: `*` → `%`, with LIKE's
/// own metacharacters (`%`, `_`, `\`) escaped so they match literally.
fn like_pattern(glob: &str) -> String {
    let mut out = String::with_capacity(glob.len());
    for ch in glob.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            '*' => out.push('%'),
            c => out.push(c),
        }
    }
    out
}

/// Split a trigger's `tables` pattern into `(schema LIKE, table LIKE)` patterns:
/// a `schema.table` form globs both halves; an unqualified pattern matches in
/// `default_schema` literally.
fn table_like_patterns(pattern: &str, default_schema: &str) -> (String, String) {
    match pattern.trim().split_once('.') {
        Some((schema, table)) => (like_pattern(schema.trim()), like_pattern(table.trim())),
        None => (
            like_pattern(default_schema.trim()),
            like_pattern(pattern.trim()),
        ),
    }
}

/// The base tables matching the trigger's wildcard pattern, as
/// `(schema, table)` pairs in stable order. System schemas never match a
/// wildcard schema glob.
async fn discover_tables(
    pool: &PgPool,
    pattern: &str,
    default_schema: &str,
) -> Result<Vec<(String, String)>> {
    let (schema_like, table_like) = table_like_patterns(pattern, default_schema);
    let rows = sql_run_read(
        pool,
        "SELECT table_schema, table_name FROM information_schema.tables \
         WHERE table_type = 'BASE TABLE' \
           AND table_schema NOT IN ('pg_catalog', 'information_schema') \
           AND table_schema LIKE $1 AND table_name LIKE $2 \
         ORDER BY table_schema, table_name",
        &[Value::String(schema_like), Value::String(table_like)],
        TABLE_DISCOVERY_CAP,
    )
    .await?;
    if rows.len() as u64 == TABLE_DISCOVERY_CAP {
        tracing::warn!(
            pattern,
            cap = TABLE_DISCOVERY_CAP,
            "collect_sql: table discovery hit its cap; tables beyond it are not polled"
        );
    }
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some((
                r.get("table_schema")?.as_str()?.to_string(),
                r.get("table_name")?.as_str()?.to_string(),
            ))
        })
        .collect())
}

/// A table's columns (name, type, PK membership, auto-increment signal) from
/// `information_schema`, in ordinal order.
async fn table_columns(pool: &PgPool, schema: &str, table: &str) -> Result<Vec<ColumnInfo>> {
    let rows = sql_run_read(
        pool,
        "SELECT c.column_name, c.data_type, c.is_identity, c.column_default, \
                (pk.column_name IS NOT NULL) AS is_pk \
         FROM information_schema.columns c \
         LEFT JOIN ( \
             SELECT kcu.column_name \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
               ON kcu.constraint_name = tc.constraint_name \
              AND kcu.constraint_schema = tc.constraint_schema \
             WHERE tc.constraint_type = 'PRIMARY KEY' \
               AND tc.table_schema = $1 AND tc.table_name = $2 \
         ) pk ON pk.column_name = c.column_name \
         WHERE c.table_schema = $1 AND c.table_name = $2 \
         ORDER BY c.ordinal_position",
        &[
            Value::String(schema.to_string()),
            Value::String(table.to_string()),
        ],
        500,
    )
    .await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let name = r.get("column_name")?.as_str()?.to_string();
            let data_type = r.get("data_type")?.as_str()?.to_string();
            let is_identity = r
                .get("is_identity")
                .and_then(Value::as_str)
                .is_some_and(|v| v.eq_ignore_ascii_case("yes"));
            let has_seq_default = r
                .get("column_default")
                .and_then(Value::as_str)
                .is_some_and(|d| d.trim_start().starts_with("nextval("));
            let is_pk = r.get("is_pk").and_then(Value::as_bool).unwrap_or(false);
            Some(ColumnInfo {
                name,
                data_type,
                is_pk,
                is_serial: is_identity || has_seq_default,
            })
        })
        .collect())
}

/// Timestamp column names accepted as an insertion-order cursor when no
/// auto-increment column exists, in preference order.
const TIMESTAMP_CURSOR_NAMES: &[&str] = &["created_at", "inserted_at", "created", "added_at"];

/// Pick the cursor column: the explicit `cursor_column` when set (`None` if the
/// table lacks it — skip with a warning), else auto-detect a sequence/identity
/// integer column (preferring the primary key), else a `created_at`-style
/// timestamp column.
fn pick_cursor_column(
    columns: &[ColumnInfo],
    explicit: Option<&str>,
) -> Option<(String, CursorKind)> {
    if let Some(want) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        let col = columns.iter().find(|c| c.name == want)?;
        return Some((col.name.clone(), CursorKind::from_data_type(&col.data_type)));
    }
    let serial = |c: &&ColumnInfo| {
        c.is_serial && CursorKind::from_data_type(&c.data_type) == CursorKind::Int
    };
    if let Some(col) = columns
        .iter()
        .find(|c| c.is_pk && serial(c))
        .or_else(|| columns.iter().find(serial))
    {
        return Some((col.name.clone(), CursorKind::Int));
    }
    for want in TIMESTAMP_CURSOR_NAMES {
        if let Some(col) = columns.iter().find(|c| {
            c.name == *want
                && matches!(
                    CursorKind::from_data_type(&c.data_type),
                    CursorKind::TimestampTz | CursorKind::Timestamp | CursorKind::Date
                )
        }) {
            return Some((col.name.clone(), CursorKind::from_data_type(&col.data_type)));
        }
    }
    None
}

/// The table's current maximum cursor value rendered as a ledger string, or
/// `None` for an empty table (everything later is then new).
async fn max_cursor_value(
    pool: &PgPool,
    schema: &str,
    table: &str,
    col: &str,
    kind: CursorKind,
) -> Result<Option<String>> {
    let qualified = format!("{}.{}", quote_ident(schema), quote_ident(table));
    let colq = quote_ident(col);
    let select = match kind {
        CursorKind::Text => format!("({colq})::text AS cursor_value"),
        _ => format!("{colq} AS cursor_value"),
    };
    let rows = sql_run_read(
        pool,
        &format!("SELECT {select} FROM {qualified} WHERE {colq} IS NOT NULL ORDER BY {colq} DESC"),
        &[],
        1,
    )
    .await?;
    Ok(rows
        .first()
        .and_then(|r| r.get("cursor_value"))
        .and_then(render_cursor))
}

/// The rows exactly AT a cursor value (the `>=` re-emit boundary) — marked
/// committed when a table is anchored, so the anchor rows never fire.
async fn boundary_rows(
    pool: &PgPool,
    schema: &str,
    table: &str,
    col: &str,
    kind: CursorKind,
    cursor: &str,
) -> Result<Vec<Value>> {
    let qualified = format!("{}.{}", quote_ident(schema), quote_ident(table));
    let colq = quote_ident(col);
    Ok(sql_run_read(
        pool,
        &format!(
            "SELECT * FROM {qualified} WHERE {}",
            kind.compare_fragment(&colq, "=")
        ),
        &[kind.bind_param(cursor)],
        ROW_POLL_CAP as u64,
    )
    .await?)
}

/// Pull the rows at-or-past the committed cursor in ascending cursor order
/// (`ROW_POLL_CAP + 1` so an over-cap tail is detectable). No cursor (anchored
/// on an empty table) pulls everything.
async fn poll_rows(
    pool: &PgPool,
    schema: &str,
    table: &str,
    col: &str,
    kind: CursorKind,
    cursor: Option<&str>,
) -> Result<Vec<Value>> {
    let qualified = format!("{}.{}", quote_ident(schema), quote_ident(table));
    let colq = quote_ident(col);
    let (stmt, params) = match cursor {
        Some(cur) => (
            format!(
                "SELECT * FROM {qualified} WHERE {} ORDER BY {colq} ASC",
                kind.compare_fragment(&colq, ">=")
            ),
            vec![kind.bind_param(cur)],
        ),
        None => (
            format!("SELECT * FROM {qualified} WHERE {colq} IS NOT NULL ORDER BY {colq} ASC"),
            Vec::new(),
        ),
    };
    Ok(sql_run_read(pool, &stmt, &params, (ROW_POLL_CAP as u64) + 1).await?)
}

/// Render a row's cursor value as the ledger string. Numbers render bare (an
/// i64 round-trips exactly), strings verbatim (`to_jsonb` renders timestamps as
/// ISO-8601 text); `NULL` is unrenderable.
fn render_cursor(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// A row's dedup identity: its primary-key values (`col=value`, joined) when the
/// table has a PK, else a v5 uuid over the whole row's JSON.
fn row_uid(row: &Value, pk_cols: &[String]) -> String {
    if pk_cols.is_empty() {
        return uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, row.to_string().as_bytes())
            .to_string();
    }
    pk_cols
        .iter()
        .map(|c| {
            format!(
                "{c}={}",
                row.get(c).map(Value::to_string).unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::SourceState;
    use serde_json::json;

    fn col(name: &str, data_type: &str, is_pk: bool, is_serial: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.to_string(),
            data_type: data_type.to_string(),
            is_pk,
            is_serial,
        }
    }

    #[test]
    fn like_pattern_translates_globs_and_escapes_metacharacters() {
        assert_eq!(like_pattern("orders_*"), "orders\\_%");
        assert_eq!(like_pattern("*"), "%");
        assert_eq!(like_pattern("a%b"), "a\\%b");
        assert_eq!(like_pattern("a\\b"), "a\\\\b");
        assert_eq!(like_pattern("plain"), "plain");
    }

    #[test]
    fn table_patterns_qualify_against_the_default_schema() {
        assert_eq!(
            table_like_patterns("orders_*", "public"),
            ("public".to_string(), "orders\\_%".to_string())
        );
        assert_eq!(
            table_like_patterns("analytics.fact_*", "public"),
            ("analytics".to_string(), "fact\\_%".to_string())
        );
        assert_eq!(
            table_like_patterns("*.events", "app"),
            ("%".to_string(), "events".to_string())
        );
    }

    #[test]
    fn cursor_column_detection_prefers_serial_pk_then_timestamps() {
        // Serial PK wins even when a serial non-PK column comes first.
        let cols = vec![
            col("seq", "bigint", false, true),
            col("id", "bigint", true, true),
            col("created_at", "timestamp with time zone", false, false),
        ];
        assert_eq!(
            pick_cursor_column(&cols, None),
            Some(("id".to_string(), CursorKind::Int))
        );

        // No serial → the created_at-style timestamp.
        let cols = vec![
            col("id", "uuid", true, false),
            col("created_at", "timestamp with time zone", false, false),
        ];
        assert_eq!(
            pick_cursor_column(&cols, None),
            Some(("created_at".to_string(), CursorKind::TimestampTz))
        );

        // Neither → None (the table is skipped with a warning).
        let cols = vec![
            col("id", "uuid", true, false),
            col("name", "text", false, false),
        ];
        assert_eq!(pick_cursor_column(&cols, None), None);

        // Explicit column: honored with its type-derived kind; missing → None.
        let cols = vec![col("at", "timestamp without time zone", false, false)];
        assert_eq!(
            pick_cursor_column(&cols, Some("at")),
            Some(("at".to_string(), CursorKind::Timestamp))
        );
        assert_eq!(pick_cursor_column(&cols, Some("nope")), None);
    }

    #[test]
    fn where_fragments_cast_per_kind() {
        assert_eq!(
            CursorKind::Int.compare_fragment("\"id\"", ">="),
            "\"id\" >= $1::bigint"
        );
        assert_eq!(
            CursorKind::TimestampTz.compare_fragment("\"created_at\"", ">="),
            "\"created_at\" >= $1::timestamptz"
        );
        assert_eq!(
            CursorKind::Text.compare_fragment("\"ref\"", ">="),
            "(\"ref\")::text >= $1"
        );
        assert_eq!(
            CursorKind::Int.compare_fragment("\"id\"", "="),
            "\"id\" = $1::bigint"
        );
        // Int cursors bind as numbers so the bigint cast is exact.
        assert_eq!(CursorKind::Int.bind_param("42"), json!(42));
        assert_eq!(
            CursorKind::TimestampTz.bind_param("2026-07-11T00:00:00+00:00"),
            json!("2026-07-11T00:00:00+00:00")
        );
    }

    #[test]
    fn row_uid_uses_pk_values_or_hashes_the_row() {
        let row = json!({ "id": 7, "tenant": "a", "body": "x" });
        assert_eq!(row_uid(&row, &["id".to_string()]), "id=7");
        assert_eq!(
            row_uid(&row, &["tenant".to_string(), "id".to_string()]),
            "tenant=\"a\",id=7"
        );
        // No PK → a stable v5 uuid over the row JSON (same row, same uid).
        let a = row_uid(&row, &[]);
        let b = row_uid(&row, &[]);
        assert_eq!(a, b);
        assert_ne!(a, row_uid(&json!({ "id": 8 }), &[]));
    }

    #[test]
    fn advance_prunes_committed_to_the_boundary() {
        let mut state = SourceState::default();
        state.committed.insert(format!("41{ENTRY_SEP}id=1"));
        state.committed.insert(format!("42{ENTRY_SEP}id=2"));
        state.committed.insert(format!("42{ENTRY_SEP}id=3"));

        // Whole batch committed → cursor moves, entries strictly below drop,
        // boundary entries (re-emitted by the >= re-pull) stay.
        advance_table(&mut state, true, Some("42".to_string()));
        assert_eq!(state.cursor.as_deref(), Some("42"));
        assert!(!state.committed.contains(&format!("41{ENTRY_SEP}id=1")));
        assert!(state.committed.contains(&format!("42{ENTRY_SEP}id=2")));
        assert!(state.committed.contains(&format!("42{ENTRY_SEP}id=3")));

        // An uncommitted tail pins the cursor and keeps every entry.
        let mut pinned = SourceState {
            cursor: Some("42".to_string()),
            ..Default::default()
        };
        pinned.committed.insert(format!("43{ENTRY_SEP}id=4"));
        advance_table(&mut pinned, false, Some("43".to_string()));
        assert_eq!(pinned.cursor.as_deref(), Some("42"));
        assert!(pinned.committed.contains(&format!("43{ENTRY_SEP}id=4")));

        // No new rows → nothing moves.
        let mut idle = SourceState {
            cursor: Some("42".to_string()),
            initialized: true,
            ..Default::default()
        };
        advance_table(&mut idle, true, None);
        assert_eq!(idle.cursor.as_deref(), Some("42"));
    }

    #[test]
    fn quote_ident_doubles_embedded_quotes() {
        assert_eq!(quote_ident("plain"), "\"plain\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }
}
