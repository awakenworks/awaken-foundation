//! SQLite backend shell over the pure migration core.
//!
//! It owns only what is SQLite-specific: the synchronous `rusqlite` driver, the
//! ledger DDL, and a per-bundle transaction. SQLite is single-writer, so its
//! single-applier guard (P6) is the run transaction itself, opened with
//! `BEGIN IMMEDIATE` to take the write lock before the ledger is read — the
//! backend-neutral counterpart of the Postgres advisory lock. Unlike the
//! Postgres shell there is no single-statement limit — `execute_batch` runs
//! multi-statement migrations. The apply decision is delegated to [`crate::plan`].

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::{AppliedMigration, MigrationBundle, MigrationError, plan, sql_identifier};

/// SQLite-backed migration runner with a per-prefix ledger table.
///
/// Synchronous by design: call it from inside the store's existing
/// `spawn_blocking` closure with a borrowed connection.
#[derive(Debug, Clone)]
pub struct SqliteMigrationRunner {
    ledger_table: String,
    applied_by: String,
}

impl SqliteMigrationRunner {
    pub fn with_prefix(prefix: impl AsRef<str>) -> Result<Self, MigrationError> {
        let prefix = sql_identifier(prefix.as_ref())?;
        Ok(Self {
            ledger_table: format!("{prefix}_schema_migrations"),
            applied_by: "awaken-scoped-migration".to_string(),
        })
    }

    #[must_use]
    pub fn with_applied_by(mut self, applied_by: impl Into<String>) -> Self {
        self.applied_by = applied_by.into();
        self
    }

    #[must_use]
    pub fn ledger_table(&self) -> &str {
        &self.ledger_table
    }

    /// Acquire the single-applier guard for a run (P6).
    ///
    /// On SQLite the guard is the run transaction itself: `run_bundle` opens it
    /// with `BEGIN IMMEDIATE`, which takes the write lock before the ledger is
    /// read, and the transaction releases it on commit or rollback. There is
    /// therefore nothing further to acquire here — this is the backend-neutral
    /// guard's default no-op, kept so the SQLite shell mirrors the Postgres
    /// shell's `pg_advisory_xact_lock`. See `docs/design/scoped-migration.md`.
    fn acquire_applier_guard(&self) -> Result<(), MigrationError> {
        Ok(())
    }

    pub fn run_bundle(
        &self,
        conn: &Connection,
        bundle: &MigrationBundle,
    ) -> Result<Vec<AppliedMigration>, MigrationError> {
        self.ensure_ledger(conn)?;
        // Open the run's transaction with `BEGIN IMMEDIATE` so the write lock is
        // taken *before* the ledger is read. This is the SQLite single-applier
        // guard (P6): two processes starting against the same database can no
        // longer both observe an empty ledger and both apply (a check-then-apply
        // TOCTOU); the loser blocks on the write lock, then verifies. A deferred
        // transaction would only upgrade on the first DDL, leaving that race
        // open. `new_unchecked` keeps the `&Connection` signature, so the runner
        // still slots into the stores' existing `create_tables(&Connection)`
        // call sites without threading a `&mut` through their lock guards. The
        // transaction holds the guard across read+apply; `commit` releases it on
        // success and `Drop` rolls back — releasing it — on drift or any error,
        // so a failed run never strands the lock.
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
            .map_err(sqlite_error("sqlite_migration_begin"))?;
        self.acquire_applier_guard()?;

        let applied_versions = self.applied_versions(&tx, bundle.bundle_id())?;
        let pending = plan(bundle, &applied_versions)?;

        let mut applied = Vec::new();
        for migration in pending {
            // `execute_batch` runs the simple-query path, so a migration body
            // may contain multiple statements.
            tx.execute_batch(migration.sql())
                .map_err(sqlite_error("sqlite_migration_apply"))?;

            let checksum = migration.checksum();
            let insert_sql = format!(
                "INSERT INTO {} (bundle_id, version, checksum, description, applied_by)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                self.ledger_table
            );
            tx.execute(
                &insert_sql,
                rusqlite::params![
                    bundle.bundle_id(),
                    migration.version(),
                    checksum,
                    migration.description(),
                    self.applied_by,
                ],
            )
            .map_err(sqlite_error("sqlite_migration_record"))?;

            applied.push(AppliedMigration {
                bundle_id: bundle.bundle_id().to_string(),
                version: migration.version(),
                checksum,
                description: migration.description().to_string(),
            });
        }

        tx.commit()
            .map_err(sqlite_error("sqlite_migration_commit"))?;
        Ok(applied)
    }

    /// Run bundles in registration order, rejecting duplicate bundle ids.
    pub fn run_bundles(
        &self,
        conn: &Connection,
        bundles: &[MigrationBundle],
    ) -> Result<Vec<AppliedMigration>, MigrationError> {
        let mut seen = BTreeSet::new();
        for bundle in bundles {
            if !seen.insert(bundle.bundle_id()) {
                return Err(MigrationError::DuplicateBundle(
                    bundle.bundle_id().to_string(),
                ));
            }
        }
        let mut applied = Vec::new();
        for bundle in bundles {
            applied.extend(self.run_bundle(conn, bundle)?);
        }
        Ok(applied)
    }

    fn ensure_ledger(&self, conn: &Connection) -> Result<(), MigrationError> {
        let sql = format!(
            "CREATE TABLE IF NOT EXISTS {} (
                bundle_id TEXT NOT NULL,
                version INTEGER NOT NULL,
                checksum TEXT NOT NULL,
                description TEXT NOT NULL,
                applied_at TEXT NOT NULL DEFAULT (datetime('now')),
                applied_by TEXT NOT NULL,
                PRIMARY KEY (bundle_id, version)
            )",
            self.ledger_table
        );
        conn.execute_batch(&sql)
            .map_err(sqlite_error("sqlite_migration_ledger_schema"))
    }

    fn applied_versions(
        &self,
        conn: &Connection,
        bundle_id: &str,
    ) -> Result<BTreeMap<i64, String>, MigrationError> {
        let sql = format!(
            "SELECT version, checksum FROM {} WHERE bundle_id = ?1 ORDER BY version",
            self.ledger_table
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(sqlite_error("sqlite_migration_read_ledger"))?;
        let rows = stmt
            .query_map([bundle_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sqlite_error("sqlite_migration_read_ledger"))?;
        let mut applied = BTreeMap::new();
        for row in rows {
            let (version, checksum) =
                row.map_err(sqlite_error("sqlite_migration_decode_ledger"))?;
            applied.insert(version, checksum);
        }
        Ok(applied)
    }
}

fn sqlite_error(operation: &'static str) -> impl Fn(rusqlite::Error) -> MigrationError {
    move |error| MigrationError::Backend {
        operation,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Migration;

    fn bundle() -> MigrationBundle {
        MigrationBundle::new(
            "runtime.core",
            vec![
                Migration::new(1, "create a", "CREATE TABLE a (id TEXT PRIMARY KEY)").unwrap(),
                // A multi-statement migration: legal for SQLite via execute_batch.
                Migration::new(
                    2,
                    "create b and index",
                    "CREATE TABLE b (id TEXT PRIMARY KEY); CREATE INDEX idx_b ON b (id);",
                )
                .unwrap(),
            ],
        )
        .unwrap()
    }

    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |_| Ok(()),
        )
        .is_ok()
    }

    #[test]
    fn applies_bundle_once_and_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        let runner = SqliteMigrationRunner::with_prefix("awaken").unwrap();

        let first = runner.run_bundle(&conn, &bundle()).unwrap();
        assert_eq!(first.len(), 2);
        assert!(table_exists(&conn, "a"));
        assert!(table_exists(&conn, "b"));

        let second = runner.run_bundle(&conn, &bundle()).unwrap();
        assert!(second.is_empty());
    }

    #[test]
    fn fails_closed_on_checksum_drift() {
        let conn = Connection::open_in_memory().unwrap();
        let runner = SqliteMigrationRunner::with_prefix("awaken").unwrap();
        runner.run_bundle(&conn, &bundle()).unwrap();

        let changed = MigrationBundle::new(
            "runtime.core",
            vec![
                Migration::new(1, "create a", "CREATE TABLE a (id INTEGER PRIMARY KEY)").unwrap(),
                Migration::new(
                    2,
                    "create b and index",
                    "CREATE TABLE b (id TEXT PRIMARY KEY); CREATE INDEX idx_b ON b (id);",
                )
                .unwrap(),
            ],
        )
        .unwrap();
        assert!(matches!(
            runner.run_bundle(&conn, &changed).unwrap_err(),
            MigrationError::ChecksumMismatch { version: 1, .. }
        ));
    }

    #[test]
    fn concurrent_runs_apply_each_migration_once() {
        // Two connections race the same on-disk database from separate threads.
        // The `BEGIN IMMEDIATE` single-applier guard (P6) must serialise them so
        // each migration is applied exactly once: one connection applies the
        // whole bundle, the loser blocks on the write lock and then finds the
        // ledger already populated and applies nothing.
        let path = std::env::temp_dir().join(format!(
            "awaken-scoped-migration-guard-{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let path_ref = &path;

        let applied = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(move || {
                        let conn = Connection::open(path_ref).unwrap();
                        // Wait for the guard instead of failing fast on a busy
                        // write lock, so the loser blocks rather than erroring.
                        conn.busy_timeout(std::time::Duration::from_secs(10))
                            .unwrap();
                        let runner = SqliteMigrationRunner::with_prefix("awaken").unwrap();
                        runner.run_bundle(&conn, &bundle()).map(|a| a.len())
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap().unwrap())
                .collect::<Vec<_>>()
        });

        // Exactly one run applied both migrations; the other applied none. A
        // sorted exact match pins the outcome to {0, 2} so a regression that let
        // both runs apply (e.g. {2, 2}) or split the work (e.g. {1, 1}) fails.
        let mut applied_sorted = applied.clone();
        applied_sorted.sort_unstable();
        assert_eq!(applied_sorted, vec![0, 2]);

        let conn = Connection::open(path_ref).unwrap();
        assert!(table_exists(&conn, "a"));
        assert!(table_exists(&conn, "b"));
        let ledger_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM awaken_schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(ledger_rows, 2);

        drop(conn);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn rejects_duplicate_bundle_id() {
        let conn = Connection::open_in_memory().unwrap();
        let runner = SqliteMigrationRunner::with_prefix("awaken").unwrap();
        let err = runner
            .run_bundles(&conn, &[bundle(), bundle()])
            .unwrap_err();
        assert!(matches!(err, MigrationError::DuplicateBundle(id) if id == "runtime.core"));
    }
}
