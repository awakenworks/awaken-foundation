//! PostgreSQL backend shell over the pure migration core.
//!
//! It owns only what is Postgres-specific: the `sqlx` driver, the advisory-lock
//! and ledger DDL. Migration bodies run through the multi-statement simple-query
//! path (`raw_sql`), mirroring SQLite's `execute_batch`, so a body is just SQL.
//! The apply decision is delegated to [`crate::plan`].

use std::collections::BTreeMap;

use sqlx::{PgPool, Row};

use crate::{
    AppliedMigration, LEDGER_VERSION, MigrationBundle, MigrationError, check_ledger_version, plan,
    sql_identifier,
};

/// PostgreSQL-backed migration runner with a per-prefix ledger table.
#[derive(Debug, Clone)]
pub struct PostgresMigrationRunner {
    pool: PgPool,
    ledger_table: String,
    meta_table: String,
    applied_by: String,
}

impl PostgresMigrationRunner {
    pub fn with_prefix(pool: PgPool, prefix: impl AsRef<str>) -> Result<Self, MigrationError> {
        let prefix = sql_identifier(prefix.as_ref())?;
        Ok(Self {
            pool,
            ledger_table: format!("{prefix}_schema_migrations"),
            meta_table: format!("{prefix}_schema_migrations_meta"),
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
    /// Takes a transaction-scoped advisory lock keyed on the ledger table and
    /// bundle id. Held across the ledger read and the apply, it makes exactly
    /// one connection apply a pending bundle while the others wait, then verify
    /// — closing the concurrent-startup TOCTOU. `pg_advisory_xact_lock` is
    /// released automatically when the transaction commits or rolls back, so a
    /// failed run never strands it. The backend-neutral counterpart on SQLite
    /// is the `BEGIN IMMEDIATE` write lock; see `docs/design/scoped-migration.md`.
    async fn acquire_applier_guard(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        bundle_id: &str,
    ) -> Result<(), MigrationError> {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
            .bind(&self.ledger_table)
            .bind(bundle_id)
            .execute(&mut **tx)
            .await
            .map_err(pg_error("postgres_migration_lock"))?;
        Ok(())
    }

    pub async fn run_bundle(
        &self,
        bundle: &MigrationBundle,
    ) -> Result<Vec<AppliedMigration>, MigrationError> {
        self.ensure_ledger().await?;
        self.assert_ledger_version().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(pg_error("postgres_migration_begin"))?;

        // Take the single-applier guard before reading the ledger and hold it
        // across the apply (P6). It is released when this transaction commits or
        // rolls back, on every exit path.
        self.acquire_applier_guard(&mut tx, bundle.bundle_id())
            .await?;

        let applied_versions = self.applied_versions(&mut tx, bundle.bundle_id()).await?;
        let pending = plan(bundle, &applied_versions)?;

        let mut applied = Vec::new();
        for migration in pending {
            // `raw_sql` runs the simple-query path, so a migration body may
            // contain multiple statements, mirroring SQLite's `execute_batch`.
            sqlx::raw_sql(migration.sql())
                .execute(&mut *tx)
                .await
                .map_err(pg_error("postgres_migration_apply"))?;

            let insert_sql = format!(
                "INSERT INTO {} (bundle_id, version, checksum, description, applied_by)
                 VALUES ($1, $2, $3, $4, $5)",
                self.ledger_table
            );
            let checksum = migration.checksum();
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
                .map_err(pg_error("postgres_migration_record"))?;

            applied.push(AppliedMigration {
                bundle_id: bundle.bundle_id().to_string(),
                version: migration.version(),
                checksum,
                description,
            });
        }

        tx.commit()
            .await
            .map_err(pg_error("postgres_migration_commit"))?;
        Ok(applied)
    }

    /// Run bundles in registration order, rejecting duplicate bundle ids.
    pub async fn run_bundles(
        &self,
        bundles: &[MigrationBundle],
    ) -> Result<Vec<AppliedMigration>, MigrationError> {
        let mut seen = std::collections::BTreeSet::new();
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
                version BIGINT NOT NULL,
                checksum TEXT NOT NULL,
                description TEXT NOT NULL,
                applied_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                applied_by TEXT NOT NULL,
                PRIMARY KEY (bundle_id, version)
            )",
            self.ledger_table
        );
        sqlx::query(&sql)
            .execute(&self.pool)
            .await
            .map_err(pg_error("postgres_migration_ledger_schema"))?;

        // Companion marker table stamping the ledger's own schema version. Kept
        // as its own statement so it stays within the prepared protocol's
        // one-statement-per-query limit, like every other runner query.
        let meta_sql = format!(
            "CREATE TABLE IF NOT EXISTS {} (ledger_version BIGINT NOT NULL)",
            self.meta_table
        );
        sqlx::query(&meta_sql)
            .execute(&self.pool)
            .await
            .map_err(pg_error("postgres_migration_meta_schema"))?;

        // Seed exactly once: a freshly created (empty) marker is stamped with the
        // current version; an already-stamped ledger is left untouched.
        let seed_sql = format!(
            "INSERT INTO {meta} (ledger_version)
             SELECT $1 WHERE NOT EXISTS (SELECT 1 FROM {meta})",
            meta = self.meta_table
        );
        sqlx::query(&seed_sql)
            .bind(LEDGER_VERSION)
            .execute(&self.pool)
            .await
            .map_err(pg_error("postgres_migration_meta_seed"))?;
        Ok(())
    }

    /// Read the stamped ledger version and fail closed unless it matches the
    /// version this runner expects.
    async fn assert_ledger_version(&self) -> Result<(), MigrationError> {
        let sql = format!("SELECT ledger_version FROM {} LIMIT 1", self.meta_table);
        let row = sqlx::query(&sql)
            .fetch_one(&self.pool)
            .await
            .map_err(pg_error("postgres_migration_meta_read"))?;
        let found: i64 = row
            .try_get("ledger_version")
            .map_err(pg_error("postgres_migration_meta_decode"))?;
        check_ledger_version(&self.ledger_table, found)
    }

    async fn applied_versions(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        bundle_id: &str,
    ) -> Result<BTreeMap<i64, String>, MigrationError> {
        let sql = format!(
            "SELECT version, checksum FROM {} WHERE bundle_id = $1 ORDER BY version",
            self.ledger_table
        );
        let rows = sqlx::query(&sql)
            .bind(bundle_id)
            .fetch_all(&mut **tx)
            .await
            .map_err(pg_error("postgres_migration_read_ledger"))?;
        rows.into_iter()
            .map(|row| {
                let version: i64 = row
                    .try_get("version")
                    .map_err(pg_error("postgres_migration_decode_ledger"))?;
                let checksum: String = row
                    .try_get("checksum")
                    .map_err(pg_error("postgres_migration_decode_ledger"))?;
                Ok((version, checksum))
            })
            .collect()
    }
}

fn pg_error(operation: &'static str) -> impl Fn(sqlx::Error) -> MigrationError {
    move |error| MigrationError::Backend {
        operation,
        message: error.to_string(),
    }
}
