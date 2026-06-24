//! SQLite backend shell over the pure migration core.
//!
//! It owns only what is SQLite-specific: the synchronous `rusqlite` driver, the
//! ledger DDL, and a per-bundle transaction. SQLite is single-writer, so its
//! single-applier guard (P6) is the run transaction itself, opened with
//! `BEGIN IMMEDIATE` to take the write lock before the ledger is read — the
//! backend-neutral counterpart of the Postgres advisory lock. Migration bodies
//! run through `execute_batch`, the simple-query path, so a body may contain
//! multiple statements — mirroring the Postgres shell's `raw_sql`. The apply
//! decision is delegated to [`crate::plan`].

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::{
    AppliedMigration, Dialect, LEDGER_VERSION, MigrationBundle, MigrationError,
    check_ledger_version, plan, render, sql_identifier,
};

/// The dialect this backend shell applies and checksums migrations under.
const DIALECT: Dialect = Dialect::Sqlite;

/// SQLite-backed migration runner with a per-prefix ledger table.
///
/// Synchronous by design: call it from inside the store's existing
/// `spawn_blocking` closure with a borrowed connection.
#[derive(Debug, Clone)]
pub struct SqliteMigrationRunner {
    prefix: String,
    ledger_table: String,
    meta_table: String,
    applied_by: String,
}

impl SqliteMigrationRunner {
    pub fn with_prefix(prefix: impl AsRef<str>) -> Result<Self, MigrationError> {
        let prefix = sql_identifier(prefix.as_ref())?;
        Ok(Self {
            ledger_table: format!("{prefix}_schema_migrations"),
            meta_table: format!("{prefix}_schema_migrations_meta"),
            applied_by: "awaken-scoped-migration".to_string(),
            prefix,
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
        self.assert_ledger_version(conn)?;
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
        let pending = plan(bundle, &applied_versions, DIALECT)?;

        let mut applied = Vec::new();
        for migration in pending {
            // Render the portable token template to SQLite SQL at apply time,
            // then run it on the simple-query path so a migration body may
            // contain multiple statements. The template (not the rendered SQL)
            // is what `plan` checksums, keeping the recorded identity portable.
            let sql = render(migration.sql_for(DIALECT), DIALECT, &self.prefix);
            tx.execute_batch(&sql)
                .map_err(sqlite_error("sqlite_migration_apply"))?;

            let checksum = migration.checksum_for(DIALECT);
            // The ledger records the readable `V0001`-labelled description so a
            // ledger scan reads the version label without decoding the integer.
            let description = migration.ledger_description();
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
                    description,
                    self.applied_by,
                ],
            )
            .map_err(sqlite_error("sqlite_migration_record"))?;

            applied.push(AppliedMigration {
                bundle_id: bundle.bundle_id().to_string(),
                version: migration.version(),
                checksum,
                description,
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
            .map_err(sqlite_error("sqlite_migration_ledger_schema"))?;

        // Companion marker table that stamps the ledger's own schema version.
        // The ledger has no migration path of its own, so the version is carried
        // here and asserted on every run rather than evolved in place.
        let meta_sql = format!(
            "CREATE TABLE IF NOT EXISTS {} (ledger_version INTEGER NOT NULL)",
            self.meta_table
        );
        conn.execute_batch(&meta_sql)
            .map_err(sqlite_error("sqlite_migration_meta_schema"))?;

        // Seed exactly once: stamp a freshly created (empty) marker table with
        // the current version. An already-stamped ledger is left untouched.
        let seed_sql = format!(
            "INSERT INTO {meta} (ledger_version)
             SELECT ?1 WHERE NOT EXISTS (SELECT 1 FROM {meta})",
            meta = self.meta_table
        );
        conn.execute(&seed_sql, rusqlite::params![LEDGER_VERSION])
            .map_err(sqlite_error("sqlite_migration_meta_seed"))?;
        Ok(())
    }

    /// Read the stamped ledger version and fail closed unless it matches the
    /// version this runner expects.
    fn assert_ledger_version(&self, conn: &Connection) -> Result<(), MigrationError> {
        let sql = format!("SELECT ledger_version FROM {} LIMIT 1", self.meta_table);
        let found: i64 = conn
            .query_row(&sql, [], |row| row.get(0))
            .map_err(sqlite_error("sqlite_migration_meta_read"))?;
        check_ledger_version(&self.ledger_table, found)
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
        // The ledger records the readable label alongside the description.
        assert_eq!(first[0].description, "V0001 create a");
        let recorded: String = conn
            .query_row(
                "SELECT description FROM awaken_schema_migrations WHERE version = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(recorded, "V0001 create a");

        let second = runner.run_bundle(&conn, &bundle()).unwrap();
        assert!(second.is_empty());
    }

    #[test]
    fn renders_portable_tokens_at_apply_time() {
        // A portable token template is rendered to SQLite SQL when applied: the
        // runner threads its own dialect and prefix into `render`, so the bundle
        // author never writes `INTEGER PRIMARY KEY AUTOINCREMENT` or the table
        // prefix by hand.
        let conn = Connection::open_in_memory().unwrap();
        let runner = SqliteMigrationRunner::with_prefix("gateway").unwrap();
        let bundle = MigrationBundle::new(
            "runtime.tokens",
            vec![
                Migration::new(
                    1,
                    "create event",
                    "CREATE TABLE {prefix}_event (\
                        id {pk_autoinc}, \
                        payload {json} NOT NULL, \
                        body {blob}, \
                        created_at {timestamptz} NOT NULL DEFAULT {now})",
                )
                .unwrap(),
            ],
        )
        .unwrap();

        let applied = runner.run_bundle(&conn, &bundle).unwrap();
        assert_eq!(applied.len(), 1);
        // The `{prefix}` token rendered to the runner's prefix, creating the
        // prefixed table; no `{...}` token leaked into the applied DDL.
        assert!(table_exists(&conn, "gateway_event"));

        // `{pk_autoinc}` rendered to an auto-incrementing integer key: inserting
        // a row without an id assigns one, which only holds for INTEGER PRIMARY
        // KEY AUTOINCREMENT.
        conn.execute("INSERT INTO gateway_event (payload) VALUES ('{}')", [])
            .unwrap();
        let id: i64 = conn
            .query_row("SELECT id FROM gateway_event", [], |row| row.get(0))
            .unwrap();
        assert_eq!(id, 1);

        // Re-running is idempotent: the recorded checksum is over the template,
        // so the same template verifies and nothing re-applies.
        assert!(runner.run_bundle(&conn, &bundle).unwrap().is_empty());
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

    fn meta_row_count(conn: &Connection, runner: &SqliteMigrationRunner) -> i64 {
        conn.query_row(
            &format!("SELECT count(*) FROM {}", runner.meta_table),
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn stamps_ledger_version_on_fresh_ledger() {
        let conn = Connection::open_in_memory().unwrap();
        let runner = SqliteMigrationRunner::with_prefix("awaken").unwrap();
        runner.run_bundle(&conn, &bundle()).unwrap();

        let version: i64 = conn
            .query_row(
                &format!("SELECT ledger_version FROM {}", runner.meta_table),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, LEDGER_VERSION);
        // Seeded exactly once, and re-running does not duplicate the stamp.
        assert_eq!(meta_row_count(&conn, &runner), 1);
        runner.run_bundle(&conn, &bundle()).unwrap();
        assert_eq!(meta_row_count(&conn, &runner), 1);
    }

    #[test]
    fn fails_closed_on_ledger_version_mismatch() {
        let conn = Connection::open_in_memory().unwrap();
        let runner = SqliteMigrationRunner::with_prefix("awaken").unwrap();
        runner.run_bundle(&conn, &bundle()).unwrap();

        // Simulate a ledger written by a different migrator generation.
        conn.execute(
            &format!("UPDATE {} SET ledger_version = ?1", runner.meta_table),
            rusqlite::params![LEDGER_VERSION + 1],
        )
        .unwrap();

        assert!(matches!(
            runner.run_bundle(&conn, &bundle()).unwrap_err(),
            MigrationError::LedgerVersionMismatch { found, .. } if found == LEDGER_VERSION + 1
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

    #[test]
    fn runs_multiple_distinct_bundles_in_order() {
        // The happy path of `run_bundles`: two independently-versioned bundles
        // share one database and both apply, in registration order.
        let conn = Connection::open_in_memory().unwrap();
        let runner = SqliteMigrationRunner::with_prefix("awaken").unwrap();
        let core = bundle();
        let audit = MigrationBundle::new(
            "gateway.audit",
            vec![
                Migration::new(1, "create log", "CREATE TABLE log (id TEXT PRIMARY KEY)").unwrap(),
            ],
        )
        .unwrap();

        let applied = runner.run_bundles(&conn, &[core, audit]).unwrap();
        // Two from runtime.core plus one from gateway.audit, recorded under each
        // bundle id.
        assert_eq!(applied.len(), 3);
        assert_eq!(applied[2].bundle_id, "gateway.audit");
        assert!(table_exists(&conn, "a"));
        assert!(table_exists(&conn, "b"));
        assert!(table_exists(&conn, "log"));
    }

    #[test]
    fn records_the_configured_applied_by() {
        // `with_applied_by` overrides the actor stamped into every ledger row, and
        // `ledger_table` reports the per-prefix table the rows land in.
        let conn = Connection::open_in_memory().unwrap();
        let runner = SqliteMigrationRunner::with_prefix("awaken")
            .unwrap()
            .with_applied_by("control-plane");
        assert_eq!(runner.ledger_table(), "awaken_schema_migrations");
        runner.run_bundle(&conn, &bundle()).unwrap();

        let applied_by: String = conn
            .query_row(
                &format!(
                    "SELECT applied_by FROM {} WHERE version = 1",
                    runner.ledger_table()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(applied_by, "control-plane");
    }

    #[test]
    fn releases_the_applier_guard_on_the_error_path() {
        // A migration whose body is invalid SQL passes construction (non-blank)
        // but fails at apply, surfacing a Backend error. The run transaction —
        // which is the SQLite single-applier guard (P6) — must roll back and
        // release the write lock on that error path, leaving no ledger rows and
        // no partial table, so a subsequent valid run on the same connection is
        // not blocked by a stranded lock.
        let conn = Connection::open_in_memory().unwrap();
        let runner = SqliteMigrationRunner::with_prefix("awaken").unwrap();

        let broken = MigrationBundle::new(
            "runtime.core",
            vec![Migration::new(1, "broken", "THIS IS NOT VALID SQL").unwrap()],
        )
        .unwrap();
        let err = runner.run_bundle(&conn, &broken).unwrap_err();
        assert!(matches!(
            err,
            MigrationError::Backend {
                operation: "sqlite_migration_apply",
                ..
            }
        ));

        // The guard was released, not stranded: the ledger is empty and a fresh,
        // valid bundle applies cleanly on the very same connection.
        let ledger_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM awaken_schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(ledger_rows, 0);

        let applied = runner.run_bundle(&conn, &bundle()).unwrap();
        assert_eq!(applied.len(), 2);
        assert!(table_exists(&conn, "a"));
    }
}
