//! Async SQLite backend shell over the pure migration core, driven by `sqlx`.
//!
//! This is the SQLite counterpart of [`postgres`](crate::postgres): both share
//! the one async `sqlx` idiom (a [`sqlx::Pool`] and [`sqlx::Transaction`]), so a
//! service already on `sqlx` reaches for the same runner shape on either backend.
//! It owns only what is SQLite-specific: the `sqlx` SQLite driver, the SQLite
//! ledger DDL, and the single-applier guard.
//!
//! The crate also ships [`sqlite`](crate::sqlite), an *independent* synchronous
//! `rusqlite` shell over the same core for callers that hold a borrowed
//! `&Connection` inside a `spawn_blocking` closure. Both checksum and plan under
//! [`Dialect::Sqlite`], so a bundle migrates identically through either; pick the
//! one that matches the driver the service already runs on.
//!
//! SQLite is single-writer, so the single-applier guard (P6) is the run
//! transaction itself, opened with `BEGIN IMMEDIATE` to take the write lock
//! before the ledger is read — the backend-neutral counterpart of the Postgres
//! advisory lock. Migration bodies run through the multi-statement simple-query
//! path (`raw_sql`), mirroring the Postgres shell and `rusqlite`'s
//! `execute_batch`. The apply decision is delegated to [`crate::plan`].

use std::collections::{BTreeMap, BTreeSet};

use sqlx::{Row, SqlitePool};

use crate::{
    AppliedMigration, Dialect, LEDGER_VERSION, MigrationBundle, MigrationError,
    check_ledger_version, plan, render, sql_identifier,
};

/// The dialect this backend shell applies and checksums migrations under.
const DIALECT: Dialect = Dialect::Sqlite;

/// `sqlx`-driven, async SQLite migration runner with a per-prefix ledger table.
#[derive(Debug, Clone)]
pub struct SqlxSqliteMigrationRunner {
    pool: SqlitePool,
    prefix: String,
    ledger_table: String,
    meta_table: String,
    applied_by: String,
}

impl SqlxSqliteMigrationRunner {
    pub fn with_prefix(pool: SqlitePool, prefix: impl AsRef<str>) -> Result<Self, MigrationError> {
        let prefix = sql_identifier(prefix.as_ref())?;
        Ok(Self {
            pool,
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

    pub async fn run_bundle(
        &self,
        bundle: &MigrationBundle,
    ) -> Result<Vec<AppliedMigration>, MigrationError> {
        self.ensure_ledger().await?;
        self.assert_ledger_version().await?;

        // Open the run's transaction with `BEGIN IMMEDIATE` so the write lock is
        // taken *before* the ledger is read. This is the SQLite single-applier
        // guard (P6): two connections starting against the same database can no
        // longer both observe an empty ledger and both apply (a check-then-apply
        // TOCTOU); the loser blocks on the write lock (subject to the pool's
        // `busy_timeout`), then verifies. A plain `begin()` would issue a
        // DEFERRED `BEGIN` that only upgrades on the first DDL, leaving that race
        // open — so `begin_with` threads the explicit statement instead. The
        // returned transaction holds the guard across read+apply; `commit`
        // releases it on success and `Drop` rolls back — releasing it — on drift
        // or any error, so a failed run never strands the lock. It is the
        // backend-neutral counterpart of the Postgres `pg_advisory_xact_lock`;
        // see `docs/design/scoped-migration.md`.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(sqlite_error("sqlite_sqlx_migration_begin"))?;

        let applied_versions = self.applied_versions(&mut tx, bundle.bundle_id()).await?;
        let pending = plan(bundle, &applied_versions, DIALECT)?;

        let mut applied = Vec::new();
        for migration in pending {
            // Render the portable token template to SQLite SQL at apply time,
            // then run it on the simple-query path so a migration body may
            // contain multiple statements, mirroring the Postgres shell's
            // `raw_sql` and `rusqlite`'s `execute_batch`. The template (not the
            // rendered SQL) is what `plan` checksums, so the recorded identity
            // stays dialect-independent.
            let sql = render(migration.sql_for(DIALECT), DIALECT, &self.prefix);
            sqlx::raw_sql(&sql)
                .execute(&mut *tx)
                .await
                .map_err(sqlite_error("sqlite_sqlx_migration_apply"))?;

            let insert_sql = format!(
                "INSERT INTO {} (bundle_id, version, checksum, description, applied_by)
                 VALUES (?, ?, ?, ?, ?)",
                self.ledger_table
            );
            let checksum = migration.checksum_for(DIALECT);
            // The ledger records the readable `V0001`-labelled description so a
            // ledger scan reads the version label without decoding the integer.
            let description = migration.ledger_description();
            sqlx::query(&insert_sql)
                .bind(bundle.bundle_id())
                .bind(migration.version())
                .bind(&checksum)
                .bind(&description)
                .bind(&self.applied_by)
                .execute(&mut *tx)
                .await
                .map_err(sqlite_error("sqlite_sqlx_migration_record"))?;

            applied.push(AppliedMigration {
                bundle_id: bundle.bundle_id().to_string(),
                version: migration.version(),
                checksum,
                description,
            });
        }

        tx.commit()
            .await
            .map_err(sqlite_error("sqlite_sqlx_migration_commit"))?;
        Ok(applied)
    }

    /// Run bundles in registration order, rejecting duplicate bundle ids.
    pub async fn run_bundles(
        &self,
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
            applied.extend(self.run_bundle(bundle).await?);
        }
        Ok(applied)
    }

    async fn ensure_ledger(&self) -> Result<(), MigrationError> {
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
        sqlx::query(&sql)
            .execute(&self.pool)
            .await
            .map_err(sqlite_error("sqlite_sqlx_migration_ledger_schema"))?;

        // Companion marker table that stamps the ledger's own schema version.
        // The ledger has no migration path of its own, so the version is carried
        // here and asserted on every run rather than evolved in place.
        let meta_sql = format!(
            "CREATE TABLE IF NOT EXISTS {} (ledger_version INTEGER NOT NULL)",
            self.meta_table
        );
        sqlx::query(&meta_sql)
            .execute(&self.pool)
            .await
            .map_err(sqlite_error("sqlite_sqlx_migration_meta_schema"))?;

        // Seed exactly once: a freshly created (empty) marker is stamped with the
        // current version; an already-stamped ledger is left untouched.
        let seed_sql = format!(
            "INSERT INTO {meta} (ledger_version)
             SELECT ? WHERE NOT EXISTS (SELECT 1 FROM {meta})",
            meta = self.meta_table
        );
        sqlx::query(&seed_sql)
            .bind(LEDGER_VERSION)
            .execute(&self.pool)
            .await
            .map_err(sqlite_error("sqlite_sqlx_migration_meta_seed"))?;
        Ok(())
    }

    /// Read the stamped ledger version and fail closed unless it matches the
    /// version this runner expects.
    async fn assert_ledger_version(&self) -> Result<(), MigrationError> {
        let sql = format!("SELECT ledger_version FROM {} LIMIT 1", self.meta_table);
        let row = sqlx::query(&sql)
            .fetch_one(&self.pool)
            .await
            .map_err(sqlite_error("sqlite_sqlx_migration_meta_read"))?;
        let found: i64 = row
            .try_get("ledger_version")
            .map_err(sqlite_error("sqlite_sqlx_migration_meta_decode"))?;
        check_ledger_version(&self.ledger_table, found)
    }

    async fn applied_versions(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        bundle_id: &str,
    ) -> Result<BTreeMap<i64, String>, MigrationError> {
        let sql = format!(
            "SELECT version, checksum FROM {} WHERE bundle_id = ? ORDER BY version",
            self.ledger_table
        );
        let rows = sqlx::query(&sql)
            .bind(bundle_id)
            .fetch_all(&mut **tx)
            .await
            .map_err(sqlite_error("sqlite_sqlx_migration_read_ledger"))?;
        rows.into_iter()
            .map(|row| {
                let version: i64 = row
                    .try_get("version")
                    .map_err(sqlite_error("sqlite_sqlx_migration_decode_ledger"))?;
                let checksum: String = row
                    .try_get("checksum")
                    .map_err(sqlite_error("sqlite_sqlx_migration_decode_ledger"))?;
                Ok((version, checksum))
            })
            .collect()
    }
}

fn sqlite_error(operation: &'static str) -> impl Fn(sqlx::Error) -> MigrationError {
    move |error| MigrationError::Backend {
        operation,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Migration;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    /// A pool over a fresh temp-file database. SQLite's `:memory:` is
    /// per-connection, so a pooled in-memory database would not survive the
    /// runner's distinct `ensure_ledger`/`begin` connections; a file is the
    /// honest substrate and lets the concurrency test open a second pool over
    /// the same database.
    fn pool_at(path: &std::path::Path) -> SqlitePool {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            // Block on a held write lock instead of failing fast, so a loser in
            // the single-applier race waits for the guard rather than erroring.
            .busy_timeout(std::time::Duration::from_secs(10));
        SqlitePoolOptions::new()
            .max_connections(4)
            .connect_lazy_with(options)
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "awaken-scoped-migration-sqlx-{tag}-{}.sqlite",
            std::process::id()
        ))
    }

    fn cleanup(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    fn bundle() -> MigrationBundle {
        MigrationBundle::new(
            "runtime.core",
            vec![
                Migration::new(1, "create a", "CREATE TABLE a (id TEXT PRIMARY KEY)").unwrap(),
                // A multi-statement migration: legal via the simple-query path.
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
        sqlx::query("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?")
            .bind(name)
            .fetch_optional(pool)
            .await
            .unwrap()
            .is_some()
    }

    #[tokio::test]
    async fn applies_bundle_once_and_is_idempotent() {
        let path = temp_path("idempotent");
        cleanup(&path);
        let pool = pool_at(&path);
        let runner = SqlxSqliteMigrationRunner::with_prefix(pool.clone(), "awaken").unwrap();

        let first = runner.run_bundle(&bundle()).await.unwrap();
        assert_eq!(first.len(), 2);
        assert!(table_exists(&pool, "a").await);
        assert!(table_exists(&pool, "b").await);
        // The ledger records the readable label alongside the description.
        assert_eq!(first[0].description, "V0001 create a");
        let recorded: String =
            sqlx::query("SELECT description FROM awaken_schema_migrations WHERE version = 1")
                .fetch_one(&pool)
                .await
                .unwrap()
                .get("description");
        assert_eq!(recorded, "V0001 create a");

        let second = runner.run_bundle(&bundle()).await.unwrap();
        assert!(second.is_empty());

        pool.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn renders_portable_tokens_at_apply_time() {
        let path = temp_path("tokens");
        cleanup(&path);
        let pool = pool_at(&path);
        let runner = SqlxSqliteMigrationRunner::with_prefix(pool.clone(), "gateway").unwrap();
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

        let applied = runner.run_bundle(&bundle).await.unwrap();
        assert_eq!(applied.len(), 1);
        // The `{prefix}` token rendered to the runner's prefix, creating the
        // prefixed table; no `{...}` token leaked into the applied DDL.
        assert!(table_exists(&pool, "gateway_event").await);

        // `{pk_autoinc}` rendered to an auto-incrementing integer key: inserting
        // a row without an id assigns one, which only holds for INTEGER PRIMARY
        // KEY AUTOINCREMENT.
        sqlx::query("INSERT INTO gateway_event (payload) VALUES ('{}')")
            .execute(&pool)
            .await
            .unwrap();
        let id: i64 = sqlx::query("SELECT id FROM gateway_event")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("id");
        assert_eq!(id, 1);

        // Re-running is idempotent: the recorded checksum is over the template,
        // so the same template verifies and nothing re-applies.
        assert!(runner.run_bundle(&bundle).await.unwrap().is_empty());

        pool.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn fails_closed_on_checksum_drift() {
        let path = temp_path("drift");
        cleanup(&path);
        let pool = pool_at(&path);
        let runner = SqlxSqliteMigrationRunner::with_prefix(pool.clone(), "awaken").unwrap();
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

        pool.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn stamps_ledger_version_and_fails_closed_on_mismatch() {
        let path = temp_path("ledger-version");
        cleanup(&path);
        let pool = pool_at(&path);
        let runner = SqlxSqliteMigrationRunner::with_prefix(pool.clone(), "awaken").unwrap();
        runner.run_bundle(&bundle()).await.unwrap();

        let version: i64 = sqlx::query("SELECT ledger_version FROM awaken_schema_migrations_meta")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("ledger_version");
        assert_eq!(version, LEDGER_VERSION);
        // Seeded exactly once, and re-running does not duplicate the stamp.
        let count: i64 = sqlx::query("SELECT count(*) AS n FROM awaken_schema_migrations_meta")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("n");
        assert_eq!(count, 1);

        // Simulate a ledger written by a different migrator generation.
        sqlx::query("UPDATE awaken_schema_migrations_meta SET ledger_version = ?")
            .bind(LEDGER_VERSION + 1)
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            runner.run_bundle(&bundle()).await.unwrap_err(),
            MigrationError::LedgerVersionMismatch { found, .. } if found == LEDGER_VERSION + 1
        ));

        pool.close().await;
        cleanup(&path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_runs_apply_each_migration_once() {
        // Two independent pools race the same on-disk database. The
        // `BEGIN IMMEDIATE` single-applier guard (P6) must serialise them so each
        // migration is applied exactly once: one pool applies the whole bundle,
        // the loser blocks on the write lock (via `busy_timeout`), then finds the
        // ledger already populated and applies nothing.
        let path = temp_path("guard");
        cleanup(&path);

        let run = |pool: SqlitePool| async move {
            let runner = SqlxSqliteMigrationRunner::with_prefix(pool, "awaken").unwrap();
            runner
                .run_bundle(&bundle())
                .await
                .map(|applied| applied.len())
        };
        let (left, right) = tokio::join!(run(pool_at(&path)), run(pool_at(&path)));

        // Exactly one run applied both migrations; the other applied none. A
        // sorted exact match pins the outcome to {0, 2} so a regression that let
        // both runs apply (e.g. {2, 2}) or split the work (e.g. {1, 1}) fails.
        let mut applied = vec![left.unwrap(), right.unwrap()];
        applied.sort_unstable();
        assert_eq!(applied, vec![0, 2]);

        let pool = pool_at(&path);
        assert!(table_exists(&pool, "a").await);
        assert!(table_exists(&pool, "b").await);
        let ledger_rows: i64 = sqlx::query("SELECT COUNT(*) AS n FROM awaken_schema_migrations")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("n");
        assert_eq!(ledger_rows, 2);

        pool.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn rejects_duplicate_bundle_id() {
        let path = temp_path("dup-bundle");
        cleanup(&path);
        let pool = pool_at(&path);
        let runner = SqlxSqliteMigrationRunner::with_prefix(pool.clone(), "awaken").unwrap();
        let err = runner.run_bundles(&[bundle(), bundle()]).await.unwrap_err();
        assert!(matches!(err, MigrationError::DuplicateBundle(id) if id == "runtime.core"));

        pool.close().await;
        cleanup(&path);
    }
}
