//! Synchronous SQLite backend shell over the pure [`awaken_scoped_migration`]
//! core.
//!
//! It owns only what is SQLite-specific: the synchronous `rusqlite` driver, the
//! ledger DDL, and a per-bundle transaction. SQLite is single-writer, so its
//! single-applier guard (P6) is the run transaction itself, opened with
//! `BEGIN IMMEDIATE` to take the write lock before the ledger is read — the
//! backend-neutral counterpart of the Postgres advisory lock. Migration bodies
//! run through `execute_batch`, the simple-query path, so a body may contain
//! multiple statements — mirroring the Postgres shell's `raw_sql`. The apply
//! decision is delegated to [`awaken_scoped_migration::plan`].
//!
//! This backend lives in its own crate, separate from the sqlx-driven backends
//! in `awaken-scoped-migration`, because `rusqlite` and `sqlx` each pull the
//! native `links = "sqlite3"` crate `libsqlite3-sys` at versions that cannot
//! coexist in one dependency graph. Keeping this driver in a sibling crate lets
//! a consumer select exactly one native SQLite backend. See ADR-0005.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, Transaction, TransactionBehavior};

use awaken_scoped_migration::{
    AppliedMigration, Dialect, LedgerBootstrapAction, LedgerSchema, MigrationBundle,
    MigrationError, check_ledger_version, plan, render, sql_identifier,
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
    ledger: LedgerSchema,
    applied_by: String,
}

impl SqliteMigrationRunner {
    pub fn with_prefix(prefix: impl AsRef<str>) -> Result<Self, MigrationError> {
        let prefix = sql_identifier(prefix.as_ref())?;
        let ledger = LedgerSchema::with_prefix(&prefix)?;
        Ok(Self {
            applied_by: "awaken-scoped-migration".to_string(),
            prefix,
            ledger,
        })
    }

    #[must_use]
    pub fn with_applied_by(mut self, applied_by: impl Into<String>) -> Self {
        self.applied_by = applied_by.into();
        self
    }

    #[must_use]
    pub fn ledger_table(&self) -> &str {
        self.ledger.ledger_table()
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
        self.ensure_ledger(&tx)?;

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
                self.ledger.ledger_table()
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
        let exists = |table: &str| {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                [table],
                |row| row.get::<_, bool>(0),
            )
            .map_err(sqlite_error("sqlite_migration_bootstrap_probe"))
        };
        let action = self.ledger.bootstrap_action(
            exists(self.ledger.ledger_table())?,
            exists(self.ledger.meta_table())?,
        )?;
        if action == LedgerBootstrapAction::Create {
            for statement in self.ledger.create_statements(DIALECT) {
                conn.execute_batch(&statement)
                    .map_err(sqlite_error("sqlite_migration_bootstrap_create"))?;
            }
        }
        self.assert_ledger_version(conn)
    }

    /// Read the stamped ledger version and fail closed unless it matches the
    /// version this runner expects.
    fn assert_ledger_version(&self, conn: &Connection) -> Result<(), MigrationError> {
        let sql = format!("SELECT ledger_version FROM {}", self.ledger.meta_table());
        let mut statement = conn
            .prepare(&sql)
            .map_err(sqlite_error("sqlite_migration_meta_read"))?;
        let rows = statement
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(sqlite_error("sqlite_migration_meta_read"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sqlite_error("sqlite_migration_meta_decode"))?;
        if rows.len() != 1 {
            return Err(MigrationError::LedgerMetadataRowCount {
                meta_table: self.ledger.meta_table().to_string(),
                found: rows.len(),
            });
        }
        check_ledger_version(self.ledger.ledger_table(), rows[0])
    }

    fn applied_versions(
        &self,
        conn: &Connection,
        bundle_id: &str,
    ) -> Result<BTreeMap<i64, String>, MigrationError> {
        let sql = format!(
            "SELECT version, checksum FROM {} WHERE bundle_id = ?1 ORDER BY version",
            self.ledger.ledger_table()
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
    use awaken_scoped_migration::{LEDGER_VERSION, Migration};

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
            &format!("SELECT count(*) FROM {}", runner.ledger.meta_table()),
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn stamps_ledger_version_on_fresh_ledger() {
        // Cause/effect rule R1: both ledger tables absent -> acquire the
        // IMMEDIATE lock, execute the three unconditional bootstrap commands,
        // stamp v1 exactly once, then apply the bundle. Re-run observes R2
        // (both present) and validates without another stamp.
        let conn = Connection::open_in_memory().unwrap();
        let runner = SqliteMigrationRunner::with_prefix("awaken").unwrap();
        runner.run_bundle(&conn, &bundle()).unwrap();

        let version: i64 = conn
            .query_row(
                &format!("SELECT ledger_version FROM {}", runner.ledger.meta_table()),
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
        // Cause/effect rule R2b: both tables present but the generation stamp
        // differs -> no migration body runs and LedgerVersionMismatch is final.
        let conn = Connection::open_in_memory().unwrap();
        let runner = SqliteMigrationRunner::with_prefix("awaken").unwrap();
        runner.run_bundle(&conn, &bundle()).unwrap();

        // Simulate a ledger written by a different migrator generation.
        conn.execute(
            &format!(
                "UPDATE {} SET ledger_version = ?1",
                runner.ledger.meta_table()
            ),
            rusqlite::params![LEDGER_VERSION + 1],
        )
        .unwrap();

        assert!(matches!(
            runner.run_bundle(&conn, &bundle()).unwrap_err(),
            MigrationError::LedgerVersionMismatch { found, .. } if found == LEDGER_VERSION + 1
        ));
    }

    #[test]
    fn fails_closed_on_partial_ledger_without_repairing_it() {
        // Cause/effect rules R3/R4: exactly one bookkeeping table present ->
        // IncompleteLedger and zero repair DDL. Both asymmetric states are
        // exercised so the presence decision table is covered at the backend.
        for create in [
            "CREATE TABLE awaken_schema_migrations (\
             bundle_id TEXT NOT NULL, version INTEGER NOT NULL, checksum TEXT NOT NULL, \
             description TEXT NOT NULL, applied_at TEXT NOT NULL, applied_by TEXT NOT NULL, \
             PRIMARY KEY (bundle_id, version))",
            "CREATE TABLE awaken_schema_migrations_meta (ledger_version INTEGER NOT NULL)",
        ] {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(create).unwrap();
            let runner = SqliteMigrationRunner::with_prefix("awaken").unwrap();
            assert!(matches!(
                runner.run_bundle(&conn, &bundle()).unwrap_err(),
                MigrationError::IncompleteLedger { .. }
            ));
            let table_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
                     AND name IN ('awaken_schema_migrations', \
                                  'awaken_schema_migrations_meta')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(table_count, 1, "runner must not repair partial state");
        }
    }

    #[test]
    fn fails_closed_when_metadata_has_not_exactly_one_stamp() {
        // Cause/effect rule R2c: both tables present but metadata cardinality is
        // zero or greater than one -> LedgerMetadataRowCount; applying a bundle
        // would otherwise bless an ambiguous migrator generation.
        for extra_rows in [0, 2] {
            let conn = Connection::open_in_memory().unwrap();
            let schema = LedgerSchema::with_prefix("awaken").unwrap();
            for statement in schema.create_statements(DIALECT).into_iter().take(2) {
                conn.execute_batch(&statement).unwrap();
            }
            for _ in 0..extra_rows {
                conn.execute(
                    "INSERT INTO awaken_schema_migrations_meta (ledger_version) VALUES (?1)",
                    [LEDGER_VERSION],
                )
                .unwrap();
            }
            let runner = SqliteMigrationRunner::with_prefix("awaken").unwrap();
            assert!(matches!(
                runner.run_bundle(&conn, &bundle()).unwrap_err(),
                MigrationError::LedgerMetadataRowCount { found, .. } if found == extra_rows
            ));
        }
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
