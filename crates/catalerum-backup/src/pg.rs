//! The Postgres half of a backup: a pure-Rust logical dump + restore over the
//! sqlx `COPY` protocol (SOUL §30).
//!
//! Dump = `COPY <table> (<cols>) TO STDOUT (FORMAT text)` per table, gzipped.
//! Restore = load each table back with `COPY … FROM STDIN`. Two load strategies,
//! chosen by the restoring role's privilege (see [`restore_postgres`]):
//!   * **superuser** — disable FK/user triggers for the load
//!     (`session_replication_role = replica`, a superuser-only GUC) so the load is
//!     order-independent; the historical fast path.
//!   * **non-superuser** — load tables **parents-before-children** in foreign-key
//!     dependency order so the still-enforced FKs are satisfied by order, not by
//!     disabled triggers. This makes disaster recovery work under a hardened,
//!     least-privilege DB role (SOUL §14), which cannot set that GUC.
//!
//! Both need only that the schema already exist.
//!
//! No external `pg_dump`/`pg_restore` — everything runs through the same pool the
//! repositories use, so a backup needs no extra binary on the host and is
//! testable against the repo's ephemeral Postgres. The SQL itself lives in
//! [`catalerum_store::backup`]; this module owns the artifact layout,
//! compression, checksums and load order.

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};

use catalerum_core::error::{Error, Result};
use catalerum_core::provider::StorageBackend;
use catalerum_store::backup::{self as store_backup, RestoreSession};
use catalerum_store::StoreError;

use crate::{get_all, put_bytes, Manifest, PgPool, TableManifest};

/// A table to dump: its name and columns in ordinal order.
pub(crate) use catalerum_store::backup::TableSpec;

/// Map a store error into the crate error vocabulary.
fn pg_err(e: StoreError) -> Error {
    Error::provider(format!("postgres: {e}"))
}

/// Enumerate the user tables to dump: every base table in the `public` schema
/// except `_sqlx_migrations` (see [`store_backup::list_tables`]).
pub(crate) async fn list_tables(pool: &PgPool) -> Result<Vec<TableSpec>> {
    store_backup::list_tables(pool).await.map_err(pg_err)
}

/// The Postgres schema version: the max applied `_sqlx_migrations.version`
/// (`None` if no migrations are recorded). Restore compares it against the live
/// DB so data never loads into a schema it was not dumped from.
pub(crate) async fn schema_version(pool: &PgPool) -> Result<Option<i64>> {
    store_backup::schema_version(pool).await.map_err(pg_err)
}

/// Dump one table to `<prefix>/<id>/postgres/<table>.copy.gz` and return its
/// manifest entry (row count, compressed size, sha-256). The dump is the
/// Postgres text `COPY` format — every row is exactly one `\n`-terminated line
/// (data newlines are escaped), so the row count is the newline count.
pub(crate) async fn dump_table(
    pool: &PgPool,
    dest: &dyn StorageBackend,
    prefix: &str,
    id: &str,
    spec: &TableSpec,
) -> Result<TableManifest> {
    let plain = store_backup::copy_out(pool, spec).await.map_err(pg_err)?;
    let rows = plain.iter().filter(|&&b| b == b'\n').count() as u64;

    let compressed = gzip(&plain)?;
    let sha256 = sha256_hex(&compressed);
    let bytes = compressed.len() as u64;

    let key = format!("{prefix}/{id}/postgres/{}.copy.gz", spec.name);
    put_bytes(dest, &key, compressed, "application/gzip").await?;

    Ok(TableManifest {
        name: spec.name.clone(),
        columns: spec.columns.clone(),
        rows,
        bytes,
        sha256,
    })
}

/// Restore every table in `manifest` into `pool`, replacing current contents.
/// Returns the total rows loaded. All work happens inside one transaction that
/// rolls back on any error (the connection is dropped), so the live DB is never
/// left half-loaded.
///
/// Picks a strategy from the restoring role's privilege (one cheap
/// `current_setting('is_superuser')` up front):
///   * **superuser** — the fast path: `SET session_replication_role = replica`
///     disables FK/user triggers, so all tables truncate + `COPY … FROM STDIN`
///     load order-independently. That GUC is superuser-only (SUSET); a hardened
///     role errors on it with SQLSTATE `42501`.
///   * **non-superuser** — a dependency-ordered load: the FK graph is read from
///     the catalog, tables are topologically sorted, and each is `COPY`-ed
///     **parents-before-children** so the FKs that are *still enforced* are
///     satisfied by order. No superuser GUC is touched.
///
/// `force_ordered` forces the dependency-ordered path even on a superuser role
/// (so CI can exercise it; see [`BackupEngine::with_force_ordered_restore`]).
///
/// **Minimal privilege for the non-superuser path.** The restoring role must be
/// able to `TRUNCATE` and `INSERT` (via `COPY`) every dumped table — i.e. be the
/// **owner** of those tables (or hold explicit `TRUNCATE` + `INSERT` grants). The
/// tables are truncated with a single `TRUNCATE … CASCADE` (Postgres refuses to
/// truncate a FK-referenced table on its own even when the referencing table is
/// empty, so per-table reverse-order truncation is not possible; the combined
/// statement handles all inter-table references atomically and needs no
/// superuser). One thing it genuinely **cannot** do without superuser: resolve a
/// true FK **cycle** — a non-superuser can neither set `session_replication_role`
/// nor `DISABLE TRIGGER ALL` (the system FK triggers reject a non-superuser). If
/// the graph has a cycle, this path errors clearly naming the cycle (there is no
/// FK cycle in today's schema). [`BackupEngine::with_force_ordered_restore`].
pub(crate) async fn restore_postgres(
    pool: &PgPool,
    dest: &dyn StorageBackend,
    prefix: &str,
    id: &str,
    manifest: &Manifest,
    force_ordered: bool,
) -> Result<u64> {
    if manifest.postgres.tables.is_empty() {
        return Ok(0);
    }

    let mut session = RestoreSession::open(pool).await.map_err(pg_err)?;
    let superuser = session.is_superuser().await.map_err(pg_err)?;

    if superuser && !force_ordered {
        return restore_with_disabled_triggers(session, dest, prefix, id, manifest).await;
    }

    // Non-superuser (or forced): load parents-before-children so the FKs that
    // remain enforced are satisfied by order, not by disabled triggers.
    let names: Vec<String> = manifest
        .postgres
        .tables
        .iter()
        .map(|t| t.name.clone())
        .collect();
    let edges = session.fk_edges().await.map_err(pg_err)?;
    match topo_order(&names, &edges) {
        Ok(order) => restore_in_dependency_order(session, dest, prefix, id, manifest, &order).await,
        Err(cycle) => {
            if superuser {
                // Reachable only via `force_ordered` on a superuser role: a real
                // FK cycle can't be dependency-ordered, so fall back to the
                // trigger-off load (a genuine non-superuser can't reach here).
                restore_with_disabled_triggers(session, dest, prefix, id, manifest).await
            } else {
                Err(Error::invalid(format!(
                    "cannot restore without superuser: tables [{}] form a foreign-key cycle that \
                     dependency ordering can't resolve, and a non-superuser role can neither set \
                     session_replication_role nor DISABLE TRIGGER ALL on the system FK triggers; \
                     restore under a superuser role instead",
                    cycle.join(", ")
                )))
            }
        }
    }
}

/// The superuser fast path: disable FK/user triggers for the load so table order
/// is irrelevant. Truncate all listed tables together (mutual FKs don't block a
/// combined truncate), then `COPY … FROM STDIN` each. The session role is
/// connection-scoped (not transactional), so the commit resets it before the
/// connection returns to the pool.
async fn restore_with_disabled_triggers(
    mut session: RestoreSession,
    dest: &dyn StorageBackend,
    prefix: &str,
    id: &str,
    manifest: &Manifest,
) -> Result<u64> {
    session.begin(true).await.map_err(pg_err)?;
    truncate_all(&mut session, manifest).await?;

    let mut total = 0u64;
    for table in &manifest.postgres.tables {
        total += copy_table_in(&mut session, dest, prefix, id, table).await?;
    }

    session.commit().await.map_err(pg_err)?;
    Ok(total)
}

/// The non-superuser path: FK triggers stay on, so `COPY` each table in `order`
/// (parents first). `order` is a topological sort covering every manifest table
/// exactly once. The truncate is still a single combined `TRUNCATE … CASCADE`:
/// Postgres rejects truncating a FK-referenced table on its own even when the
/// referencing table is empty, so a reverse-order per-table truncate is
/// impossible — the combined statement resolves all references atomically and
/// needs no superuser (only `TRUNCATE` privilege / table ownership).
async fn restore_in_dependency_order(
    mut session: RestoreSession,
    dest: &dyn StorageBackend,
    prefix: &str,
    id: &str,
    manifest: &Manifest,
    order: &[String],
) -> Result<u64> {
    let by_name: HashMap<&str, &TableManifest> = manifest
        .postgres
        .tables
        .iter()
        .map(|t| (t.name.as_str(), t))
        .collect();

    session.begin(false).await.map_err(pg_err)?;
    truncate_all(&mut session, manifest).await?;

    let mut total = 0u64;
    for name in order {
        // `order` is built from the manifest's own table names, so this is total.
        let table = by_name
            .get(name.as_str())
            .expect("dependency order is derived from the manifest table names");
        total += copy_table_in(&mut session, dest, prefix, id, table).await?;
    }

    session.commit().await.map_err(pg_err)?;
    Ok(total)
}

/// `TRUNCATE` every dumped table in one statement (see
/// [`RestoreSession::truncate`]).
async fn truncate_all(session: &mut RestoreSession, manifest: &Manifest) -> Result<()> {
    let names: Vec<&str> = manifest
        .postgres
        .tables
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    session.truncate(&names).await.map_err(pg_err)
}

/// Verify + decompress one table's dump and `COPY … FROM STDIN` it into the
/// session. Returns the table's row count. Shared by both restore strategies.
async fn copy_table_in(
    session: &mut RestoreSession,
    dest: &dyn StorageBackend,
    prefix: &str,
    id: &str,
    table: &TableManifest,
) -> Result<u64> {
    let key = format!("{prefix}/{id}/postgres/{}.copy.gz", table.name);
    let compressed = get_all(dest, &key).await?;

    // Integrity: the stored dump must match the manifest's hash.
    let actual = sha256_hex(&compressed);
    if actual != table.sha256 {
        return Err(Error::invalid(format!(
            "checksum mismatch for table `{}` in backup `{id}` (corrupt or tampered dump)",
            table.name
        )));
    }
    let plain = gunzip(&compressed)?;

    let spec = TableSpec {
        name: table.name.clone(),
        columns: table.columns.clone(),
    };
    session.copy_in(&spec, &plain).await.map_err(pg_err)?;
    Ok(table.rows)
}

/// Topologically sort `names` so every table sorts **after** the tables it
/// references (parents before children), given FK `edges` = `(child, parent)`.
///
/// Only edges with both endpoints in `names` count. **Self-references**
/// (`child == parent`) impose no ordering and are ignored (so they can never
/// create a false cycle). **Composite** and duplicate FKs to the same parent
/// collapse to one edge. Ordering is deterministic: ties break by the input
/// order of `names`. Returns `Err(cyclic_tables)` (sorted) if a real cycle
/// prevents a total order — the caller decides whether it can be broken.
fn topo_order(
    names: &[String],
    edges: &[(String, String)],
) -> std::result::Result<Vec<String>, Vec<String>> {
    let present: HashSet<&str> = names.iter().map(String::as_str).collect();
    let mut indeg: HashMap<&str, usize> = names.iter().map(|s| (s.as_str(), 0usize)).collect();
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut seen: HashSet<(&str, &str)> = HashSet::new();

    for (child, parent) in edges {
        let (c, p) = (child.as_str(), parent.as_str());
        if c == p || !present.contains(c) || !present.contains(p) {
            continue; // self-ref, or an edge touching a table we're not loading
        }
        if !seen.insert((p, c)) {
            continue; // composite / duplicate FK to the same parent → one edge
        }
        children.entry(p).or_default().push(c);
        *indeg.get_mut(c).expect("child is in `names`") += 1;
    }

    // Kahn's algorithm, scanning `names` in input order for a stable result.
    let mut done: HashSet<&str> = HashSet::with_capacity(names.len());
    let mut order: Vec<String> = Vec::with_capacity(names.len());
    loop {
        let mut progressed = false;
        for name in names {
            let n = name.as_str();
            if done.contains(n) || indeg[n] != 0 {
                continue;
            }
            done.insert(n);
            order.push(name.clone());
            progressed = true;
            if let Some(cs) = children.get(n) {
                for &child in cs {
                    *indeg.get_mut(child).expect("child is in `names`") -= 1;
                }
            }
        }
        if !progressed {
            break;
        }
    }

    if order.len() == names.len() {
        Ok(order)
    } else {
        let mut cycle: Vec<String> = names
            .iter()
            .filter(|n| !done.contains(n.as_str()))
            .cloned()
            .collect();
        cycle.sort();
        Err(cycle)
    }
}

/// Gzip a buffer.
fn gzip(plain: &[u8]) -> Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(plain)?;
    Ok(enc.finish()?)
}

/// Gunzip a buffer.
fn gunzip(compressed: &[u8]) -> Result<Vec<u8>> {
    let mut dec = GzDecoder::new(compressed);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)?;
    Ok(out)
}

/// Hex-encoded SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gzip_round_trips() {
        let data = b"id\tworkspace\tcatalerum\n1\tworkspace-1\thello\n".to_vec();
        let comp = gzip(&data).unwrap();
        assert!(comp != data, "compression should change the bytes");
        assert_eq!(gunzip(&comp).unwrap(), data);
    }

    #[test]
    fn sha256_is_stable_hex() {
        // Known SHA-256 of the empty input.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
    fn edges(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(c, p)| (c.to_string(), p.to_string()))
            .collect()
    }
    /// Assert every child sorts strictly after each parent it references.
    fn assert_parents_first(order: &[String], es: &[(String, String)]) {
        let pos = |t: &str| order.iter().position(|x| x == t).expect("in order");
        for (c, p) in es {
            if c != p {
                assert!(pos(p) < pos(c), "parent `{p}` must load before child `{c}`");
            }
        }
    }

    #[test]
    fn topo_orders_parents_before_children() {
        // grant → session (child, parent); workspace → grant; org → workspace.
        let ns = names(&["sessions", "grants", "workspaces", "organisations"]);
        let es = edges(&[
            ("sessions", "grants"),
            ("sessions", "workspaces"),
            ("grants", "workspaces"),
            ("workspaces", "organisations"),
        ]);
        let order = topo_order(&ns, &es).expect("acyclic");
        assert_eq!(order.len(), ns.len(), "every table placed exactly once");
        assert_parents_first(&order, &es);
    }

    #[test]
    fn topo_ignores_self_reference() {
        // A table that references itself imposes no ordering and never loops.
        let ns = names(&["tree", "leaf"]);
        let es = edges(&[("tree", "tree"), ("leaf", "tree")]);
        let order = topo_order(&ns, &es).expect("self-ref is not a cycle");
        assert_eq!(order, names(&["tree", "leaf"]));
    }

    #[test]
    fn topo_dedupes_composite_and_duplicate_edges() {
        // A composite FK (sessions → grants on two columns) and a redundant second
        // FK to the same parent collapse to one edge — no double-counted in-degree.
        let ns = names(&["sessions", "grants"]);
        let es = edges(&[
            ("sessions", "grants"),
            ("sessions", "grants"),
            ("sessions", "grants"),
        ]);
        let order = topo_order(&ns, &es).expect("dedup keeps it acyclic");
        assert_eq!(order, names(&["grants", "sessions"]));
    }

    #[test]
    fn topo_is_deterministic_for_independent_tables() {
        // No edges: input order is preserved verbatim (stable tie-breaking).
        let ns = names(&["c", "a", "b"]);
        assert_eq!(topo_order(&ns, &[]).unwrap(), ns);
    }

    #[test]
    fn topo_detects_cycles_and_names_them() {
        // A ⇄ B mutual FK cannot be ordered; C is acyclic but depends on A, so it
        // also can't be placed. The error lists exactly the unplaced tables, sorted.
        let ns = names(&["a", "b", "c", "d"]);
        let es = edges(&[("a", "b"), ("b", "a"), ("c", "a")]);
        let err = topo_order(&ns, &es).expect_err("mutual FK is a cycle");
        assert_eq!(err, names(&["a", "b", "c"]));
        // The acyclic remainder ("d") is simply absent from the cycle report.
        assert!(!err.contains(&"d".to_string()));
    }
}
