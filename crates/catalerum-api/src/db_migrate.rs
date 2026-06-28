//! Managed schema for external Postgres connections (SOUL §11/§19).
//!
//! Two ways to shape a connected database's schema:
//! - **Declarative auto-migration** (additive-safe): describe the desired tables/
//!   columns/indexes; [`diff`] introspects the live schema and emits only *safe
//!   additive* DDL (`CREATE TABLE`, `ADD COLUMN`, `CREATE INDEX`). Destructive or
//!   risky changes (dropping, type changes, adding a `NOT NULL` column without a
//!   default) are **blocked** and reported — they need a hand-written migration.
//! - **Manual migrations**: ordered SQL scripts applied in version order and
//!   tracked in a per-connection ledger (see [`catalerum_store::ExternalDbMigrationRepo`]).
//!
//! The pure [`diff`] is unit-tested; the DB-touching helpers ([`introspect`]) run
//! against a caller-supplied external pool.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use catalerum_core::Error as CoreError;
use catalerum_store::PgPool;

/// A column in a desired table.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DesiredColumn {
    pub name: String,
    /// The SQL type, verbatim (e.g. `uuid`, `text`, `timestamptz`, `date`).
    #[serde(rename = "type")]
    pub sql_type: String,
    /// Whether the column allows NULL (default `true` — additive-safe).
    #[serde(default = "yes")]
    pub nullable: bool,
    /// Whether the column is part of the primary key (only honored on CREATE TABLE).
    #[serde(default)]
    pub primary_key: bool,
    /// Optional column default expression, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

fn yes() -> bool {
    true
}

/// A desired index on a table.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DesiredIndex {
    pub name: String,
    pub columns: Vec<String>,
    #[serde(default)]
    pub unique: bool,
}

/// A desired table.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DesiredTable {
    pub name: String,
    pub columns: Vec<DesiredColumn>,
    #[serde(default)]
    pub indexes: Vec<DesiredIndex>,
}

/// The full desired schema, as authored by the user (the "declarative database").
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DesiredSchema {
    /// Target schema (`search_path`) — defaults to `public`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub tables: Vec<DesiredTable>,
}

impl DesiredSchema {
    fn schema_name(&self) -> &str {
        self.schema.as_deref().unwrap_or("public")
    }
}

/// The introspected live schema: table → its columns, plus the set of existing
/// index names per table.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ActualSchema {
    /// table name → (column name → is_nullable).
    pub tables: HashMap<String, HashMap<String, bool>>,
    /// existing index names (schema-wide).
    pub indexes: HashSet<String>,
}

/// A change the additive-safe migrator refuses to apply automatically.
#[derive(Clone, Debug, Serialize)]
pub struct BlockedChange {
    pub object: String,
    pub reason: String,
}

/// The result of diffing a desired schema against the live one.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MigrationPlan {
    /// The additive DDL statements that will be applied (in order).
    pub apply: Vec<String>,
    /// Changes that were detected but not applied (need a manual migration).
    pub blocked: Vec<BlockedChange>,
}

/// Quote a SQL identifier by wrapping in double quotes and doubling any embedded
/// quote — so a table/column/index name can never break out of its DDL context.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Validate a model-authored managed-schema prefix before it is used as a
/// PostgreSQL schema name. Keeping prefixes to portable, unquoted-identifier
/// characters makes them predictable in later `sql_query` calls while
/// [`quote_ident`] remains the final injection boundary.
pub(crate) fn validate_schema_prefix(prefix: &str) -> Result<(), CoreError> {
    if prefix.is_empty() {
        return Err(CoreError::invalid("schema `prefix` must not be empty"));
    }
    if prefix.len() > 63 {
        return Err(CoreError::invalid(
            "schema `prefix` must be at most 63 bytes (PostgreSQL identifier limit)",
        ));
    }
    let mut chars = prefix.chars();
    let Some(first) = chars.next() else {
        return Err(CoreError::invalid("schema `prefix` must not be empty"));
    };
    if !(first == '_' || first.is_ascii_alphabetic())
        || !chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
    {
        return Err(CoreError::invalid(
            "schema `prefix` must start with a letter or underscore and contain only ASCII letters, digits, and underscores",
        ));
    }
    if prefix.eq_ignore_ascii_case("information_schema")
        || prefix.to_ascii_lowercase().starts_with("pg_")
    {
        return Err(CoreError::invalid(
            "system schema prefixes (`pg_*` and `information_schema`) cannot be managed",
        ));
    }
    Ok(())
}

/// Whether an exact PostgreSQL schema namespace currently exists.
pub(crate) async fn schema_exists(pool: &PgPool, prefix: &str) -> Result<bool, CoreError> {
    let rows = catalerum_store::sql_run_read(
        pool,
        "SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = $1) AS exists",
        &[serde_json::Value::String(prefix.to_string())],
        1,
    )
    .await
    .map_err(CoreError::from)?;
    Ok(rows
        .first()
        .and_then(|row| row.get("exists"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false))
}

/// List user-created PostgreSQL schemas whose names begin with `prefix`.
pub(crate) async fn list_schemas(pool: &PgPool, prefix: &str) -> Result<Vec<String>, CoreError> {
    let rows = catalerum_store::sql_run_read(
        pool,
        "SELECT nspname AS prefix
         FROM pg_namespace
         WHERE nspname <> 'information_schema'
           AND nspname NOT LIKE 'pg\\_%' ESCAPE '\\'
           AND left(nspname, char_length($1)) = $1
         ORDER BY nspname",
        &[serde_json::Value::String(prefix.to_string())],
        1_000,
    )
    .await
    .map_err(CoreError::from)?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            row.get("prefix")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect())
}

/// Rich, read-only schema description used by the agent tool. The existing
/// [`ActualSchema`] stays deliberately lean for diffing; this view includes the
/// SQL types/defaults and complete index definitions an editor needs.
pub(crate) async fn describe_schema(
    pool: &PgPool,
    prefix: &str,
) -> Result<serde_json::Value, CoreError> {
    let param = [serde_json::Value::String(prefix.to_string())];
    let columns = catalerum_store::sql_run_read(
        pool,
        "SELECT table_name, ordinal_position, column_name, data_type, udt_name,
                is_nullable, column_default
         FROM information_schema.columns
         WHERE table_schema = $1
         ORDER BY table_name, ordinal_position",
        &param,
        10_000,
    )
    .await
    .map_err(CoreError::from)?;
    let indexes = catalerum_store::sql_run_read(
        pool,
        "SELECT tablename AS table_name, indexname AS index_name, indexdef AS definition
         FROM pg_indexes
         WHERE schemaname = $1
         ORDER BY tablename, indexname",
        &param,
        10_000,
    )
    .await
    .map_err(CoreError::from)?;
    Ok(serde_json::json!({
        "prefix": prefix,
        "columns": columns,
        "indexes": indexes,
    }))
}

/// The column fragment for a `CREATE TABLE` (`"name" type [DEFAULT ..] [NOT NULL]`).
fn column_fragment(col: &DesiredColumn) -> String {
    let mut s = format!("{} {}", quote_ident(&col.name), col.sql_type);
    if let Some(def) = &col.default {
        s.push_str(&format!(" DEFAULT {def}"));
    }
    if !col.nullable {
        s.push_str(" NOT NULL");
    }
    s
}

/// Compute the additive-safe [`MigrationPlan`] to bring `actual` toward `desired`.
/// Pure — no I/O — so it is unit-tested directly.
#[must_use]
pub fn diff(desired: &DesiredSchema, actual: &ActualSchema) -> MigrationPlan {
    let schema = desired.schema_name();
    let mut plan = MigrationPlan::default();

    for table in &desired.tables {
        let qtable = format!("{}.{}", quote_ident(schema), quote_ident(&table.name));
        match actual.tables.get(&table.name) {
            None => {
                // New table: CREATE TABLE with all columns + PK, then its indexes.
                let mut cols: Vec<String> = table.columns.iter().map(column_fragment).collect();
                let pk: Vec<String> = table
                    .columns
                    .iter()
                    .filter(|c| c.primary_key)
                    .map(|c| quote_ident(&c.name))
                    .collect();
                if !pk.is_empty() {
                    cols.push(format!("PRIMARY KEY ({})", pk.join(", ")));
                }
                plan.apply.push(format!(
                    "CREATE TABLE IF NOT EXISTS {qtable} (\n  {}\n)",
                    cols.join(",\n  ")
                ));
                for idx in &table.indexes {
                    plan.apply.push(create_index_ddl(schema, &table.name, idx));
                }
            }
            Some(existing) => {
                // Existing table: add missing columns (additive-safe only).
                for col in &table.columns {
                    if existing.contains_key(&col.name) {
                        // A present column is left alone — type/nullability changes are
                        // destructive-risky and handed to a manual migration.
                        continue;
                    }
                    if !col.nullable && col.default.is_none() {
                        plan.blocked.push(BlockedChange {
                            object: format!("{}.{}", table.name, col.name),
                            reason: "adding a NOT NULL column without a default to an existing \
                                     table needs a manual migration (backfill, then SET NOT NULL)"
                                .to_string(),
                        });
                        continue;
                    }
                    plan.apply.push(format!(
                        "ALTER TABLE {qtable} ADD COLUMN IF NOT EXISTS {}",
                        column_fragment(col)
                    ));
                }
                for idx in &table.indexes {
                    if !actual.indexes.contains(&idx.name) {
                        plan.apply.push(create_index_ddl(schema, &table.name, idx));
                    }
                }
            }
        }
    }
    plan
}

fn create_index_ddl(schema: &str, table: &str, idx: &DesiredIndex) -> String {
    let cols: Vec<String> = idx.columns.iter().map(|c| quote_ident(c)).collect();
    format!(
        "CREATE {}INDEX IF NOT EXISTS {} ON {}.{} ({})",
        if idx.unique { "UNIQUE " } else { "" },
        quote_ident(&idx.name),
        quote_ident(schema),
        quote_ident(table),
        cols.join(", ")
    )
}

/// Introspect the live schema of `pool` for `schema_name` (SOUL §11): read the
/// columns from `information_schema.columns` and index names from `pg_indexes`.
pub async fn introspect(pool: &PgPool, schema_name: &str) -> Result<ActualSchema, CoreError> {
    let mut actual = ActualSchema::default();

    let cols = catalerum_store::sql_run_read(
        pool,
        "SELECT table_name, column_name, is_nullable
         FROM information_schema.columns
         WHERE table_schema = $1
         ORDER BY table_name, ordinal_position",
        &[serde_json::Value::String(schema_name.to_string())],
        10_000,
    )
    .await
    .map_err(CoreError::from)?;
    for row in cols {
        let table = row.get("table_name").and_then(|v| v.as_str()).unwrap_or("");
        let col = row
            .get("column_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let nullable = row
            .get("is_nullable")
            .and_then(|v| v.as_str())
            .map(|s| s.eq_ignore_ascii_case("YES"))
            .unwrap_or(true);
        if table.is_empty() || col.is_empty() {
            continue;
        }
        actual
            .tables
            .entry(table.to_string())
            .or_default()
            .insert(col.to_string(), nullable);
    }

    let idx = catalerum_store::sql_run_read(
        pool,
        "SELECT indexname FROM pg_indexes WHERE schemaname = $1",
        &[serde_json::Value::String(schema_name.to_string())],
        10_000,
    )
    .await
    .map_err(CoreError::from)?;
    for row in idx {
        if let Some(name) = row.get("indexname").and_then(|v| v.as_str()) {
            actual.indexes.insert(name.to_string());
        }
    }

    Ok(actual)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired(json: serde_json::Value) -> DesiredSchema {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn managed_schema_prefixes_are_portable_and_never_system_names() {
        for valid in ["crm", "billing_v2", "_private", "Public"] {
            assert!(validate_schema_prefix(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "2fast",
            "has-dash",
            "has.dot",
            "information_schema",
            "pg_catalog",
            "PG_temp_1",
        ] {
            assert!(validate_schema_prefix(invalid).is_err(), "{invalid}");
        }
        assert!(validate_schema_prefix(&"a".repeat(64)).is_err());
    }

    #[test]
    fn new_table_emits_create_with_pk_and_indexes() {
        let d = desired(serde_json::json!({
            "tables": [{
                "name": "articles",
                "columns": [
                    { "name": "id", "type": "uuid", "nullable": false, "primary_key": true },
                    { "name": "day", "type": "date", "nullable": false },
                    { "name": "title", "type": "text" }
                ],
                "indexes": [ { "name": "articles_day_idx", "columns": ["day"] } ]
            }]
        }));
        let plan = diff(&d, &ActualSchema::default());
        assert!(plan.blocked.is_empty());
        assert_eq!(plan.apply.len(), 2);
        assert!(plan.apply[0].contains("CREATE TABLE IF NOT EXISTS \"public\".\"articles\""));
        assert!(plan.apply[0].contains("PRIMARY KEY (\"id\")"));
        assert!(plan.apply[0].contains("\"day\" date NOT NULL"));
        assert!(plan.apply[1].contains("CREATE INDEX IF NOT EXISTS \"articles_day_idx\""));
    }

    #[test]
    fn existing_table_adds_missing_nullable_column_only() {
        let mut actual = ActualSchema::default();
        let mut cols = HashMap::new();
        cols.insert("id".to_string(), false);
        cols.insert("day".to_string(), false);
        actual.tables.insert("articles".to_string(), cols);

        let d = desired(serde_json::json!({
            "tables": [{
                "name": "articles",
                "columns": [
                    { "name": "id", "type": "uuid" },
                    { "name": "day", "type": "date" },
                    { "name": "summary", "type": "text" }
                ]
            }]
        }));
        let plan = diff(&d, &actual);
        // Only the new nullable `summary` column is added; id/day are untouched.
        assert_eq!(plan.apply.len(), 1);
        assert!(plan.apply[0].contains("ADD COLUMN IF NOT EXISTS \"summary\" text"));
        assert!(plan.blocked.is_empty());
    }

    #[test]
    fn not_null_column_without_default_is_blocked_on_existing_table() {
        let mut actual = ActualSchema::default();
        actual.tables.insert("articles".to_string(), HashMap::new());
        let d = desired(serde_json::json!({
            "tables": [{
                "name": "articles",
                "columns": [ { "name": "kind", "type": "text", "nullable": false } ]
            }]
        }));
        let plan = diff(&d, &actual);
        assert!(plan.apply.is_empty());
        assert_eq!(plan.blocked.len(), 1);
        assert_eq!(plan.blocked[0].object, "articles.kind");
    }

    #[test]
    fn not_null_column_with_default_is_additive() {
        let mut actual = ActualSchema::default();
        actual.tables.insert("t".to_string(), HashMap::new());
        let d = desired(serde_json::json!({
            "tables": [{
                "name": "t",
                "columns": [ { "name": "n", "type": "int", "nullable": false, "default": "0" } ]
            }]
        }));
        let plan = diff(&d, &actual);
        assert_eq!(plan.apply.len(), 1);
        assert!(plan.apply[0].contains("ADD COLUMN IF NOT EXISTS \"n\" int DEFAULT 0 NOT NULL"));
        assert!(plan.blocked.is_empty());
    }

    #[test]
    fn quote_ident_escapes_embedded_quotes() {
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_ident("day"), "\"day\"");
    }
}
