//! SQLite backend shell over the pure migration core, driven by `sqlx`.
//!
//! It owns only what is SQLite-specific: the `sqlx` SQLite driver, the ledger DDL
//! and a per-bundle transaction. SQLite is single-writer, so its single-applier
//! guard (P6) is the run transaction itself, opened with `BEGIN IMMEDIATE` to take
//! the write lock before the ledger is read, the backend-neutral counterpart of the
//! Postgres advisory lock. Migration bodies run as unprepared-and-uncached queries;
//! SQLite executes every statement of a multi-statement body. (`raw_sql` on a
//! transaction would make the runner's futures `!Send`.) The apply
//! decision is delegated to [`crate::plan`].
//!
//! The ledger tables, their DDL and the recorded checksums are exactly those of the
//! synchronous `awaken-scoped-migration-sqlite` runner, so a database migrated by
//! either runner is accepted by the other.

use std::collections::{BTreeMap, BTreeSet};

use sqlx::{Row, SqlitePool};

use crate::{
    AppliedMigration, Dialect, LedgerBootstrapAction, LedgerSchema, MigrationBundle,
    MigrationError, check_ledger_version, plan, render, sql_identifier,
};

/// The dialect this backend shell applies and checksums migrations under.
const DIALECT: Dialect = Dialect::Sqlite;

/// SQLite-backed migration runner with a per-prefix ledger table.
#[derive(Debug, Clone)]
pub struct SqliteMigrationRunner {
    pool: SqlitePool,
    prefix: String,
    ledger: LedgerSchema,
    applied_by: String,
}

impl SqliteMigrationRunner {
    pub fn with_prefix(pool: SqlitePool, prefix: impl AsRef<str>) -> Result<Self, MigrationError> {
        let prefix = sql_identifier(prefix.as_ref())?;
        let ledger = LedgerSchema::with_prefix(&prefix)?;
        Ok(Self {
            pool,
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

    pub async fn run_bundle(
        &self,
        bundle: &MigrationBundle,
    ) -> Result<Vec<AppliedMigration>, MigrationError> {
        // `BEGIN IMMEDIATE` takes the write lock before the ledger is read: two
        // processes starting against the same database cannot both observe an
        // empty ledger and both apply. The loser waits on the lock (the pool's
        // busy timeout), then finds the ledger populated. The transaction holds
        // the guard across read and apply; commit releases it on success and
        // dropping the transaction rolls back, releasing it, on any error.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(sqlite_error("sqlite_migration_begin"))?;
        self.ensure_ledger(&mut tx).await?;

        let applied_versions =
            applied_versions(&mut tx, self.ledger.ledger_table(), bundle.bundle_id()).await?;
        let pending = plan(bundle, &applied_versions, DIALECT)?;

        let mut applied = Vec::new();
        for migration in pending {
            // Render the portable token template to SQLite SQL at apply time; the
            // template (not the rendered SQL) is what `plan` checksums.
            let sql = render(migration.sql_for(DIALECT), DIALECT, &self.prefix);
            sqlx::query(&sql)
                .persistent(false)
                .execute(&mut *tx)
                .await
                .map_err(sqlite_error("sqlite_migration_apply"))?;

            let checksum = migration.checksum_for(DIALECT);
            let description = migration.ledger_description();
            let insert_sql = format!(
                "INSERT INTO {} (bundle_id, version, checksum, description, applied_by)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                self.ledger.ledger_table()
            );
            sqlx::query(&insert_sql)
                .bind(bundle.bundle_id())
                .bind(migration.version())
                .bind(&checksum)
                .bind(&description)
                .bind(&self.applied_by)
                .execute(&mut *tx)
                .await
                .map_err(sqlite_error("sqlite_migration_record"))?;

            applied.push(AppliedMigration {
                bundle_id: bundle.bundle_id().to_string(),
                version: migration.version(),
                checksum,
                description,
            });
        }

        tx.commit()
            .await
            .map_err(sqlite_error("sqlite_migration_commit"))?;
        Ok(applied)
    }

    /// Run bundles in registration order, rejecting duplicate bundle ids.
    pub async fn run_bundles(
        &self,
        bundles: &[MigrationBundle],
    ) -> Result<Vec<AppliedMigration>, MigrationError> {
        reject_duplicates(bundles)?;
        let mut applied = Vec::new();
        for bundle in bundles {
            applied.extend(self.run_bundle(bundle).await?);
        }
        Ok(applied)
    }

    /// Verify that every migration in `bundle` is already applied.
    ///
    /// Read-only: it neither creates the ledger nor records or executes a
    /// migration. Missing schema, drift, unknown versions and a pending migration
    /// all fail closed.
    pub async fn verify_bundle(&self, bundle: &MigrationBundle) -> Result<(), MigrationError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(sqlite_error("sqlite_migration_verify_begin"))?;
        let (ledger_exists, meta_exists) = self.ledger_presence(&mut tx).await?;
        self.ledger.verify_presence(ledger_exists, meta_exists)?;
        assert_ledger_version(
            &mut tx,
            self.ledger.ledger_table(),
            self.ledger.meta_table(),
        )
        .await?;
        let applied_versions =
            applied_versions(&mut tx, self.ledger.ledger_table(), bundle.bundle_id()).await?;
        let pending = plan(bundle, &applied_versions, DIALECT)?;
        if let Some(migration) = pending.first() {
            return Err(MigrationError::PendingMigration {
                bundle_id: bundle.bundle_id().to_string(),
                version: migration.version(),
            });
        }
        tx.rollback()
            .await
            .map_err(sqlite_error("sqlite_migration_verify_rollback"))
    }

    /// Verify several independent bundles without applying any DDL.
    pub async fn verify_bundles(&self, bundles: &[MigrationBundle]) -> Result<(), MigrationError> {
        reject_duplicates(bundles)?;
        for bundle in bundles {
            self.verify_bundle(bundle).await?;
        }
        Ok(())
    }

    async fn ledger_presence(
        &self,
        connection: &mut sqlx::SqliteConnection,
    ) -> Result<(bool, bool), MigrationError> {
        let mut exists = Vec::with_capacity(2);
        for table in [self.ledger.ledger_table(), self.ledger.meta_table()] {
            let found: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            )
            .bind(table)
            .fetch_one(&mut *connection)
            .await
            .map_err(sqlite_error("sqlite_migration_bootstrap_probe"))?;
            exists.push(found);
        }
        Ok((exists[0], exists[1]))
    }

    async fn ensure_ledger(
        &self,
        connection: &mut sqlx::SqliteConnection,
    ) -> Result<(), MigrationError> {
        let (ledger_exists, meta_exists) = self.ledger_presence(connection).await?;
        let action = self.ledger.bootstrap_action(ledger_exists, meta_exists)?;
        if action == LedgerBootstrapAction::Create {
            for statement in self.ledger.create_statements(DIALECT) {
                sqlx::query(&statement)
                    .persistent(false)
                    .execute(&mut *connection)
                    .await
                    .map_err(sqlite_error("sqlite_migration_bootstrap_create"))?;
            }
        }
        assert_ledger_version(
            connection,
            self.ledger.ledger_table(),
            self.ledger.meta_table(),
        )
        .await
    }
}

/// Read the stamped ledger version and fail closed unless it matches the
/// version this runner expects.
///
/// Free functions over a borrowed connection (rather than `&self` methods) keep
/// every runner future provably `Send`, so callers may spawn migrations.
async fn assert_ledger_version(
    connection: &mut sqlx::SqliteConnection,
    ledger_table: &str,
    meta_table: &str,
) -> Result<(), MigrationError> {
    let sql = format!("SELECT ledger_version FROM {meta_table}");
    let rows = sqlx::query(&sql)
        .fetch_all(&mut *connection)
        .await
        .map_err(sqlite_error("sqlite_migration_meta_read"))?;
    if rows.len() != 1 {
        return Err(MigrationError::LedgerMetadataRowCount {
            meta_table: meta_table.to_string(),
            found: rows.len(),
        });
    }
    let found: i64 = rows[0]
        .try_get(0)
        .map_err(sqlite_error("sqlite_migration_meta_decode"))?;
    check_ledger_version(ledger_table, found)
}

async fn applied_versions(
    connection: &mut sqlx::SqliteConnection,
    ledger_table: &str,
    bundle_id: &str,
) -> Result<BTreeMap<i64, String>, MigrationError> {
    let sql = format!(
        "SELECT version, checksum FROM {ledger_table} WHERE bundle_id = ?1 ORDER BY version"
    );
    let rows = sqlx::query(&sql)
        .bind(bundle_id)
        .fetch_all(&mut *connection)
        .await
        .map_err(sqlite_error("sqlite_migration_read_ledger"))?;
    rows.into_iter()
        .map(|row| {
            let version: i64 = row
                .try_get(0)
                .map_err(sqlite_error("sqlite_migration_decode_ledger"))?;
            let checksum: String = row
                .try_get(1)
                .map_err(sqlite_error("sqlite_migration_decode_ledger"))?;
            Ok((version, checksum))
        })
        .collect()
}

fn reject_duplicates(bundles: &[MigrationBundle]) -> Result<(), MigrationError> {
    let mut seen = BTreeSet::new();
    for bundle in bundles {
        if !seen.insert(bundle.bundle_id()) {
            return Err(MigrationError::DuplicateBundle(
                bundle.bundle_id().to_string(),
            ));
        }
    }
    Ok(())
}

fn sqlite_error(operation: &'static str) -> impl Fn(sqlx::Error) -> MigrationError {
    move |error| MigrationError::Backend {
        operation,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::time::Duration;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    use super::*;
    use crate::{LEDGER_VERSION, Migration};

    async fn memory() -> SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap()
    }

    fn bundle() -> MigrationBundle {
        MigrationBundle::new(
            "runtime.core",
            vec![
                Migration::new(1, "create a", "CREATE TABLE a (id TEXT PRIMARY KEY)").unwrap(),
                // A multi-statement migration: legal through `raw_sql`.
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

    async fn table_exists(pool: &SqlitePool, name: &str) -> bool {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        )
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn scalar_i64(pool: &SqlitePool, sql: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn applies_bundle_once_and_is_idempotent() {
        let pool = memory().await;
        let runner = SqliteMigrationRunner::with_prefix(pool.clone(), "awaken").unwrap();

        let first = runner.run_bundle(&bundle()).await.unwrap();
        assert_eq!(first.len(), 2);
        assert!(table_exists(&pool, "a").await);
        assert!(table_exists(&pool, "b").await);
        let index: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_b')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(index, "every statement of a multi-statement migration runs");
        assert_eq!(first[0].description, "V0001 create a");
        let recorded: String = sqlx::query_scalar(
            "SELECT description FROM awaken_schema_migrations WHERE version = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(recorded, "V0001 create a");

        assert!(runner.run_bundle(&bundle()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn renders_portable_tokens_at_apply_time() {
        let pool = memory().await;
        let runner = SqliteMigrationRunner::with_prefix(pool.clone(), "gateway").unwrap();
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

        assert_eq!(runner.run_bundle(&bundle).await.unwrap().len(), 1);
        assert!(table_exists(&pool, "gateway_event").await);
        sqlx::query("INSERT INTO gateway_event (payload) VALUES ('{}')")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(scalar_i64(&pool, "SELECT id FROM gateway_event").await, 1);
        assert!(runner.run_bundle(&bundle).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fails_closed_on_checksum_drift() {
        let pool = memory().await;
        let runner = SqliteMigrationRunner::with_prefix(pool, "awaken").unwrap();
        runner.run_bundle(&bundle()).await.unwrap();

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
            runner.run_bundle(&changed).await.unwrap_err(),
            MigrationError::ChecksumMismatch { version: 1, .. }
        ));
    }

    #[tokio::test]
    async fn stamps_ledger_version_on_fresh_ledger() {
        // R1: both ledger tables absent -> bootstrap and stamp once; a re-run
        // observes both tables and validates without another stamp.
        let pool = memory().await;
        let runner = SqliteMigrationRunner::with_prefix(pool.clone(), "awaken").unwrap();
        runner.run_bundle(&bundle()).await.unwrap();
        let meta = runner.ledger.meta_table().to_string();
        assert_eq!(
            scalar_i64(&pool, &format!("SELECT ledger_version FROM {meta}")).await,
            LEDGER_VERSION
        );
        assert_eq!(
            scalar_i64(&pool, &format!("SELECT count(*) FROM {meta}")).await,
            1
        );
        runner.run_bundle(&bundle()).await.unwrap();
        assert_eq!(
            scalar_i64(&pool, &format!("SELECT count(*) FROM {meta}")).await,
            1
        );
    }

    #[tokio::test]
    async fn fails_closed_on_ledger_version_mismatch() {
        let pool = memory().await;
        let runner = SqliteMigrationRunner::with_prefix(pool.clone(), "awaken").unwrap();
        runner.run_bundle(&bundle()).await.unwrap();
        sqlx::query(&format!(
            "UPDATE {} SET ledger_version = ?1",
            runner.ledger.meta_table()
        ))
        .bind(LEDGER_VERSION + 1)
        .execute(&pool)
        .await
        .unwrap();
        assert!(matches!(
            runner.run_bundle(&bundle()).await.unwrap_err(),
            MigrationError::LedgerVersionMismatch { found, .. } if found == LEDGER_VERSION + 1
        ));
    }

    #[tokio::test]
    async fn fails_closed_on_partial_ledger_without_repairing_it() {
        // R3/R4: exactly one bookkeeping table present -> IncompleteLedger and no
        // repair DDL, for both asymmetric states.
        for create in [
            "CREATE TABLE awaken_schema_migrations (\
             bundle_id TEXT NOT NULL, version INTEGER NOT NULL, checksum TEXT NOT NULL, \
             description TEXT NOT NULL, applied_at TEXT NOT NULL, applied_by TEXT NOT NULL, \
             PRIMARY KEY (bundle_id, version))",
            "CREATE TABLE awaken_schema_migrations_meta (ledger_version INTEGER NOT NULL)",
        ] {
            let pool = memory().await;
            sqlx::raw_sql(create).execute(&pool).await.unwrap();
            let runner = SqliteMigrationRunner::with_prefix(pool.clone(), "awaken").unwrap();
            assert!(matches!(
                runner.run_bundle(&bundle()).await.unwrap_err(),
                MigrationError::IncompleteLedger { .. }
            ));
            assert_eq!(
                scalar_i64(
                    &pool,
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
                     AND name IN ('awaken_schema_migrations', 'awaken_schema_migrations_meta')"
                )
                .await,
                1,
                "runner must not repair partial state"
            );
        }
    }

    #[tokio::test]
    async fn fails_closed_when_metadata_has_not_exactly_one_stamp() {
        for extra_rows in [0_usize, 2] {
            let pool = memory().await;
            let schema = LedgerSchema::with_prefix("awaken").unwrap();
            for statement in schema.create_statements(DIALECT).into_iter().take(2) {
                sqlx::raw_sql(&statement).execute(&pool).await.unwrap();
            }
            for _ in 0..extra_rows {
                sqlx::query(
                    "INSERT INTO awaken_schema_migrations_meta (ledger_version) VALUES (?1)",
                )
                .bind(LEDGER_VERSION)
                .execute(&pool)
                .await
                .unwrap();
            }
            let runner = SqliteMigrationRunner::with_prefix(pool, "awaken").unwrap();
            assert!(matches!(
                runner.run_bundle(&bundle()).await.unwrap_err(),
                MigrationError::LedgerMetadataRowCount { found, .. } if found == extra_rows
            ));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_runs_apply_each_migration_once() {
        // Two pools race the same on-disk database. The `BEGIN IMMEDIATE`
        // single-applier guard (P6) serialises them: one applies the whole bundle,
        // the loser waits on the write lock and then applies nothing.
        let path = std::env::temp_dir().join(format!(
            "awaken-scoped-migration-sqlx-guard-{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap()
            .create_if_missing(true)
            .busy_timeout(Duration::from_secs(10));
        let open = || async {
            SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(options.clone())
                .await
                .unwrap()
        };
        let (first, second) = (open().await, open().await);
        let (runner_a, runner_b) = (
            SqliteMigrationRunner::with_prefix(first.clone(), "awaken").unwrap(),
            SqliteMigrationRunner::with_prefix(second.clone(), "awaken").unwrap(),
        );
        let (bundle_a, bundle_b) = (bundle(), bundle());
        // Both futures are polled concurrently; each pool's connection runs on its
        // own sqlx worker thread, so the loser genuinely waits on the write lock.
        let (a, b) = tokio::join!(
            runner_a.run_bundle(&bundle_a),
            runner_b.run_bundle(&bundle_b)
        );
        let mut applied = vec![a.unwrap().len(), b.unwrap().len()];
        applied.sort_unstable();
        assert_eq!(applied, vec![0, 2]);
        assert!(table_exists(&first, "a").await);
        assert!(table_exists(&first, "b").await);
        assert_eq!(
            scalar_i64(&first, "SELECT COUNT(*) FROM awaken_schema_migrations").await,
            2
        );

        first.close().await;
        second.close().await;
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    #[tokio::test]
    async fn rejects_duplicate_bundle_id() {
        let runner = SqliteMigrationRunner::with_prefix(memory().await, "awaken").unwrap();
        let err = runner.run_bundles(&[bundle(), bundle()]).await.unwrap_err();
        assert!(matches!(err, MigrationError::DuplicateBundle(id) if id == "runtime.core"));
    }

    #[tokio::test]
    async fn runs_multiple_distinct_bundles_in_registration_order() {
        let pool = memory().await;
        let runner = SqliteMigrationRunner::with_prefix(pool.clone(), "awaken").unwrap();
        let audit = MigrationBundle::new(
            "gateway.audit",
            vec![
                Migration::new(1, "create log", "CREATE TABLE log (id TEXT PRIMARY KEY)").unwrap(),
            ],
        )
        .unwrap();

        let applied = runner.run_bundles(&[bundle(), audit]).await.unwrap();
        assert_eq!(applied.len(), 3);
        assert_eq!(applied[0].bundle_id, "runtime.core");
        assert_eq!(applied[2].bundle_id, "gateway.audit");
        for table in ["a", "b", "log"] {
            assert!(table_exists(&pool, table).await);
        }
    }

    #[tokio::test]
    async fn records_the_configured_applied_by_actor() {
        for (configured, expected) in [
            (None, "awaken-scoped-migration"),
            (Some("control-plane"), "control-plane"),
        ] {
            let pool = memory().await;
            let mut runner = SqliteMigrationRunner::with_prefix(pool.clone(), "awaken").unwrap();
            if let Some(actor) = configured {
                runner = runner.with_applied_by(actor);
            }
            assert_eq!(runner.ledger_table(), "awaken_schema_migrations");
            runner.run_bundle(&bundle()).await.unwrap();
            let actors: Vec<String> = sqlx::query_scalar(
                "SELECT applied_by FROM awaken_schema_migrations ORDER BY version",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(actors, vec![expected, expected]);
        }
    }

    #[tokio::test]
    async fn releases_the_applier_guard_after_apply_failure() {
        // Valid ledger + invalid migration SQL -> Backend(apply), rollback, no
        // receipt, and the IMMEDIATE guard released: a later valid run succeeds.
        let pool = memory().await;
        let runner = SqliteMigrationRunner::with_prefix(pool.clone(), "awaken").unwrap();
        let broken = MigrationBundle::new(
            "runtime.core",
            vec![Migration::new(1, "broken", "THIS IS NOT VALID SQL").unwrap()],
        )
        .unwrap();
        assert!(matches!(
            runner.run_bundle(&broken).await.unwrap_err(),
            MigrationError::Backend {
                operation: "sqlite_migration_apply",
                ..
            }
        ));
        assert!(
            !table_exists(&pool, "awaken_schema_migrations").await
                || scalar_i64(
                    &pool,
                    "SELECT COUNT(*) FROM awaken_schema_migrations WHERE bundle_id = 'runtime.core'"
                )
                .await
                    == 0
        );

        assert_eq!(runner.run_bundle(&bundle()).await.unwrap().len(), 2);
        assert!(table_exists(&pool, "a").await);
    }

    #[tokio::test]
    async fn verification_is_read_only_and_fail_closed() {
        // R1 missing ledger -> MissingLedger without creating it; R2 fully applied
        // -> Ok; R3 a newly pending version -> PendingMigration without DDL.
        let pool = memory().await;
        let runner = SqliteMigrationRunner::with_prefix(pool.clone(), "awaken").unwrap();
        let one = MigrationBundle::new(
            "runtime.core",
            vec![Migration::new(1, "create a", "CREATE TABLE a (id TEXT PRIMARY KEY)").unwrap()],
        )
        .unwrap();
        assert!(matches!(
            runner.verify_bundle(&one).await,
            Err(MigrationError::MissingLedger { .. })
        ));
        assert!(!table_exists(&pool, "awaken_schema_migrations").await);

        runner.run_bundle(&one).await.unwrap();
        runner.verify_bundle(&one).await.unwrap();
        assert!(matches!(
            runner.verify_bundle(&bundle()).await,
            Err(MigrationError::PendingMigration { version: 2, .. })
        ));
        assert!(
            !table_exists(&pool, "b").await,
            "verification must not apply DDL"
        );
    }
}

#[cfg(test)]
mod send_probe {
    #![allow(dead_code)]
    // Migration futures must be `Send` so a consumer can run them on a spawned
    // task; this fails to compile if a change makes them `!Send`.
    fn run_bundle_future_is_send(
        runner: super::SqliteMigrationRunner,
        bundle: crate::MigrationBundle,
    ) {
        fn assert_send<T: Send>(_: T) {}
        assert_send(async move { runner.run_bundle(&bundle).await });
        let _ = |runner: super::SqliteMigrationRunner, bundle: crate::MigrationBundle| {
            assert_send(async move { runner.verify_bundle(&bundle).await })
        };
    }
}
