//! Dynamic SQL execution against an **external** Postgres pool (SOUL §11).
//!
//! These are the low-level executors behind the `sql_query` tool + the `SqlQuery`
//! automation action. They keep `sqlx` encapsulated in the store crate and take
//! only a borrowed [`PgPool`] (built by the API's external-DB registry) plus a
//! parameter list — the API layer owns connection resolution and capability
//! gating.
//!
//! Two safety properties matter here:
//! - **Single statement only.** Any interior `;` is rejected, so a query can't
//!   smuggle a second statement past the capability check that classified it.
//! - **Read mode can't write.** [`run_read`] wraps the caller's statement in a
//!   subquery (`SELECT to_jsonb(_sub) FROM ( <sql> ) _sub`). Postgres forbids
//!   data-modifying statements in a subquery, so a `SELECT`-gated call is
//!   incapable of mutating data even if the classifier were fooled — and
//!   `to_jsonb` renders every column type faithfully into JSON.

use sqlx::{types::Json, PgPool};

use crate::error::{Result, StoreError};

/// Reject a multi-statement string (defense against `;`-smuggling). Returns the
/// statement with any single trailing `;` and surrounding whitespace stripped.
fn single_statement(sql: &str) -> Result<&str> {
    let trimmed = sql.trim().trim_end_matches(';').trim_end();
    if trimmed.contains(';') {
        return Err(StoreError::invalid(
            "only a single SQL statement is allowed",
        ));
    }
    if trimmed.is_empty() {
        return Err(StoreError::invalid("empty SQL statement"));
    }
    Ok(trimmed)
}

/// A parameter after decoding the JSON-facing `sql_query` representation.
///
/// Plain arrays and objects stay JSONB for backwards compatibility. A native
/// PostgreSQL text array is deliberately explicit because the same JSON array
/// cannot unambiguously mean both `jsonb` and `text[]`:
/// `{"$pg_type":"text[]","value":["one","two"]}`.
#[derive(Debug)]
enum SqlParam {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Json(Json<serde_json::Value>),
    TextArray(Option<Vec<String>>),
}

fn decode_param(value: &serde_json::Value) -> Result<SqlParam> {
    if let serde_json::Value::Object(object) = value {
        let is_typed =
            object.len() == 2 && object.contains_key("$pg_type") && object.contains_key("value");
        if is_typed {
            let pg_type = object
                .get("$pg_type")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    StoreError::invalid("typed SQL parameter `$pg_type` must be a string")
                })?;
            return match (pg_type, object.get("value")) {
                ("text[]", Some(serde_json::Value::Null)) => Ok(SqlParam::TextArray(None)),
                ("text[]", Some(serde_json::Value::Array(values))) => {
                    let values = values
                        .iter()
                        .map(|value| {
                            value.as_str().map(str::to_owned).ok_or_else(|| {
                                StoreError::invalid(
                                    "typed SQL parameter `text[]` requires an array of strings or null",
                                )
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    Ok(SqlParam::TextArray(Some(values)))
                }
                ("text[]", _) => Err(StoreError::invalid(
                    "typed SQL parameter `text[]` requires an array of strings or null",
                )),
                (other, _) => Err(StoreError::invalid(format!(
                    "unsupported typed SQL parameter `{other}`; supported types: text[]"
                ))),
            };
        }
    }

    Ok(match value {
        serde_json::Value::Null => SqlParam::Null,
        serde_json::Value::Bool(value) => SqlParam::Bool(*value),
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                SqlParam::Int(value)
            } else if let Some(value) = number.as_u64() {
                let value = i64::try_from(value).map_err(|_| {
                    StoreError::invalid("integer SQL parameter exceeds PostgreSQL bigint range")
                })?;
                SqlParam::Int(value)
            } else {
                SqlParam::Float(number.as_f64().unwrap_or_default())
            }
        }
        serde_json::Value::String(value) => SqlParam::String(value.clone()),
        other => SqlParam::Json(Json(other.clone())),
    })
}

fn decode_params(params: &[serde_json::Value]) -> Result<Vec<SqlParam>> {
    params.iter().map(decode_param).collect()
}

/// Bind each decoded parameter positionally (`$1`, `$2`, …). Shared by
/// [`run_read`] and [`run_write`] via a macro because sqlx's query and
/// query-scalar builders don't share a bind trait.
macro_rules! bind_all {
    ($query:expr, $params:expr) => {{
        let mut q = $query;
        for p in $params {
            q = match p {
                SqlParam::Null => q.bind(Option::<String>::None),
                SqlParam::Bool(value) => q.bind(*value),
                SqlParam::Int(value) => q.bind(*value),
                SqlParam::Float(value) => q.bind(*value),
                SqlParam::String(value) => q.bind(value.clone()),
                SqlParam::Json(value) => q.bind(value.clone()),
                SqlParam::TextArray(value) => q.bind(value.clone()),
            };
        }
        q
    }};
}

/// Run a read-only query, returning up to `max_rows` rows, each rendered as a
/// JSON object via Postgres `to_jsonb`. See the module docs for why this can
/// never modify data.
///
/// # Errors
/// Multiple statements, an empty statement, or any database error.
pub async fn run_read(
    pool: &PgPool,
    sql: &str,
    params: &[serde_json::Value],
    max_rows: u64,
) -> Result<Vec<serde_json::Value>> {
    let stmt = single_statement(sql)?;
    let params = decode_params(params)?;
    // `max_rows` is a `u64` we format in — not user text — so it cannot inject.
    let wrapped = format!("SELECT to_jsonb(_sub) FROM ( {stmt} ) AS _sub LIMIT {max_rows}");
    let query = bind_all!(
        sqlx::query_scalar::<_, serde_json::Value>(&wrapped),
        &params
    );
    query.fetch_all(pool).await.map_err(StoreError::from_sqlx)
}

/// Run a data-modifying statement (`INSERT`/`UPDATE`/`DELETE`), returning the
/// number of affected rows. `RETURNING` output is not surfaced (use [`run_read`]
/// with a `SELECT` for row output).
///
/// # Errors
/// Multiple statements, an empty statement, or any database error.
pub async fn run_write(pool: &PgPool, sql: &str, params: &[serde_json::Value]) -> Result<u64> {
    let stmt = single_statement(sql)?;
    let params = decode_params(params)?;
    let query = bind_all!(sqlx::query(stmt), &params);
    let res = query.execute(pool).await.map_err(StoreError::from_sqlx)?;
    Ok(res.rows_affected())
}

/// Apply a batch of DDL/DML `statements` atomically in one transaction against an
/// external pool (SOUL §11) — used by the declarative auto-migrator to apply its
/// additive plan all-or-nothing. Each statement is a single statement (they are
/// generated, not user text).
///
/// # Errors
/// Any database error; the transaction is rolled back and nothing is applied.
pub async fn run_ddl_batch(pool: &PgPool, statements: &[String]) -> Result<()> {
    let mut tx = pool.begin().await.map_err(StoreError::from_sqlx)?;
    for stmt in statements {
        sqlx::query(stmt)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::from_sqlx)?;
    }
    tx.commit().await.map_err(StoreError::from_sqlx)?;
    Ok(())
}

/// Run a raw SQL `script` (possibly multiple statements) atomically in one
/// transaction — the executor behind a hand-written manual migration. Uses the
/// simple-query protocol, so it accepts a multi-statement script verbatim.
///
/// # Errors
/// Any database error; the whole script is rolled back on failure.
pub async fn run_sql_script(pool: &PgPool, script: &str) -> Result<()> {
    use sqlx::Executor as _;
    let mut tx = pool.begin().await.map_err(StoreError::from_sqlx)?;
    // Execute the raw `&str` (simple query protocol → multiple statements) on the
    // transaction's connection. This is the `query`-style path (a `Send` future),
    // unlike `sqlx::raw_sql(..).execute(..)` which yields a non-`Send` future.
    (&mut *tx)
        .execute(script)
        .await
        .map_err(StoreError::from_sqlx)?;
    tx.commit().await.map_err(StoreError::from_sqlx)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn single_statement_strips_trailing_semicolon() {
        assert_eq!(single_statement("SELECT 1;").unwrap(), "SELECT 1");
        assert_eq!(single_statement("  SELECT 1 ; ").unwrap(), "SELECT 1");
    }

    #[test]
    fn single_statement_rejects_multiple() {
        assert!(single_statement("SELECT 1; DROP TABLE t").is_err());
        assert!(single_statement("").is_err());
        assert!(single_statement("   ;  ").is_err());
    }

    #[test]
    fn typed_text_array_is_distinct_from_plain_json_array() {
        assert!(matches!(
            decode_param(&json!({"$pg_type": "text[]", "value": ["a", "b"]})).unwrap(),
            SqlParam::TextArray(Some(values)) if values == ["a", "b"]
        ));
        assert!(matches!(
            decode_param(&json!(["a", "b"])).unwrap(),
            SqlParam::Json(_)
        ));
        assert!(decode_param(&json!({"$pg_type": "text[]", "value": ["a", 2]})).is_err());
    }

    #[tokio::test]
    async fn typed_text_array_and_plain_json_array_bind_to_their_postgres_types() {
        let Some(url) = std::env::var("CATALERUM_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()
        else {
            eprintln!(
                "skipping typed_text_array_and_plain_json_array_bind_to_their_postgres_types: \
                 set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("connect test database");
        let rows = run_read(
            &pool,
            "SELECT $1::text[] AS tags, $2::jsonb AS payload",
            &[
                json!({"$pg_type": "text[]", "value": ["Dairy-free", "Vegetarian"]}),
                json!(["still", "jsonb"]),
            ],
            1,
        )
        .await
        .expect("bind typed text array and jsonb array");
        assert_eq!(
            rows,
            vec![json!({
                "tags": ["Dairy-free", "Vegetarian"],
                "payload": ["still", "jsonb"]
            })]
        );
    }
}
