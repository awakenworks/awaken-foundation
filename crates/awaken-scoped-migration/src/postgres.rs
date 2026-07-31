//! PostgreSQL backend shell over the pure migration core.
//!
//! It owns only what is Postgres-specific: the `sqlx` driver, the advisory-lock
//! and ledger DDL. Migration bodies run through the multi-statement simple-query
//! path (`raw_sql`), mirroring SQLite's `execute_batch`, so a body is just SQL.
//! The apply decision is delegated to [`crate::plan`].

use std::collections::BTreeMap;

use sqlx::{PgPool, Row};

use crate::{
    AppliedMigration, Dialect, LedgerBootstrapAction, LedgerSchema, MigrationBundle,
    MigrationError, check_ledger_version, plan, render, sql_identifier,
};

/// The dialect this backend shell applies and checksums migrations under.
const DIALECT: Dialect = Dialect::Postgres;

/// PostgreSQL-backed migration runner with a per-prefix ledger table.
#[derive(Debug, Clone)]
pub struct PostgresMigrationRunner {
    pool: PgPool,
    prefix: String,
    ledger: LedgerSchema,
    applied_by: String,
}

impl PostgresMigrationRunner {
    pub fn with_prefix(pool: PgPool, prefix: impl AsRef<str>) -> Result<Self, MigrationError> {
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
            .bind(self.ledger.ledger_table())
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
        let pending = plan(bundle, &applied_versions, DIALECT)?;

        let mut applied = Vec::new();
        for migration in pending {
            // Render the portable token template to Postgres SQL at apply time,
            // then run it on the simple-query path so a migration body may
            // contain multiple statements, mirroring SQLite's `execute_batch`.
            // The template (not the rendered SQL) is what `plan` checksums, so
            // the recorded identity stays dialect-independent.
            let sql = render(migration.sql_for(DIALECT), DIALECT, &self.prefix);
            sqlx::raw_sql(&sql)
                .execute(&mut *tx)
                .await
                .map_err(pg_error("postgres_migration_apply"))?;

            let insert_sql = format!(
                "INSERT INTO {} (bundle_id, version, checksum, description, applied_by)
                 VALUES ($1, $2, $3, $4, $5)",
                self.ledger.ledger_table()
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

    /// Verify that an operator already applied every migration in `bundle`.
    ///
    /// This path is deliberately read-only: it neither creates the ledger nor
    /// records or executes a migration. Application processes use it after a
    /// deployment migration Job; missing schema, drift, unknown versions and a
    /// pending migration all fail closed.
    pub async fn verify_bundle(&self, bundle: &MigrationBundle) -> Result<(), MigrationError> {
        self.assert_ledger_version().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(pg_error("postgres_migration_verify_begin"))?;
        let applied_versions = self.applied_versions(&mut tx, bundle.bundle_id()).await?;
        let pending = plan(bundle, &applied_versions, DIALECT)?;
        if let Some(migration) = pending.first() {
            return Err(MigrationError::PendingMigration {
                bundle_id: bundle.bundle_id().to_string(),
                version: migration.version(),
            });
        }
        tx.rollback()
            .await
            .map_err(pg_error("postgres_migration_verify_rollback"))?;
        Ok(())
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

    /// Verify several independent bundles without applying any DDL.
    pub async fn verify_bundles(&self, bundles: &[MigrationBundle]) -> Result<(), MigrationError> {
        let mut seen = std::collections::BTreeSet::new();
        for bundle in bundles {
            if !seen.insert(bundle.bundle_id()) {
                return Err(MigrationError::DuplicateBundle(
                    bundle.bundle_id().to_string(),
                ));
            }
            self.verify_bundle(bundle).await?;
        }
        Ok(())
    }

    async fn ensure_ledger(&self) -> Result<(), MigrationError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(pg_error("postgres_migration_bootstrap_begin"))?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
            .bind(self.ledger.ledger_table())
            .bind("ledger-bootstrap-v1")
            .execute(&mut *tx)
            .await
            .map_err(pg_error("postgres_migration_bootstrap_lock"))?;

        let presence = sqlx::query(
            "SELECT to_regclass($1) IS NOT NULL AS ledger_exists, \
                    to_regclass($2) IS NOT NULL AS meta_exists",
        )
        .bind(self.ledger.ledger_table())
        .bind(self.ledger.meta_table())
        .fetch_one(&mut *tx)
        .await
        .map_err(pg_error("postgres_migration_bootstrap_probe"))?;
        let action = self.ledger.bootstrap_action(
            presence
                .try_get("ledger_exists")
                .map_err(pg_error("postgres_migration_bootstrap_decode"))?,
            presence
                .try_get("meta_exists")
                .map_err(pg_error("postgres_migration_bootstrap_decode"))?,
        )?;
        if action == LedgerBootstrapAction::Create {
            for statement in self.ledger.create_statements(DIALECT) {
                sqlx::raw_sql(&statement)
                    .execute(&mut *tx)
                    .await
                    .map_err(pg_error("postgres_migration_bootstrap_create"))?;
            }
        }
        self.assert_ledger_version_in(&mut tx).await?;
        tx.commit()
            .await
            .map_err(pg_error("postgres_migration_bootstrap_commit"))
    }

    /// Read the stamped ledger version and fail closed unless it matches the
    /// version this runner expects.
    async fn assert_ledger_version(&self) -> Result<(), MigrationError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(pg_error("postgres_migration_meta_begin"))?;
        self.assert_ledger_version_in(&mut tx).await?;
        tx.rollback()
            .await
            .map_err(pg_error("postgres_migration_meta_rollback"))
    }

    async fn assert_ledger_version_in(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), MigrationError> {
        let sql = format!("SELECT ledger_version FROM {}", self.ledger.meta_table());
        let rows = sqlx::query(&sql)
            .fetch_all(&mut **tx)
            .await
            .map_err(pg_error("postgres_migration_meta_read"))?;
        if rows.len() != 1 {
            return Err(MigrationError::LedgerMetadataRowCount {
                meta_table: self.ledger.meta_table().to_string(),
                found: rows.len(),
            });
        }
        let found: i64 = rows[0]
            .try_get("ledger_version")
            .map_err(pg_error("postgres_migration_meta_decode"))?;
        check_ledger_version(self.ledger.ledger_table(), found)
    }

    async fn applied_versions(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        bundle_id: &str,
    ) -> Result<BTreeMap<i64, String>, MigrationError> {
        let sql = format!(
            "SELECT version, checksum FROM {} WHERE bundle_id = $1 ORDER BY version",
            self.ledger.ledger_table()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Migration;

    fn bundle(version_two: bool) -> MigrationBundle {
        let mut migrations = vec![
            Migration::new(
                1,
                "initial table",
                "CREATE TABLE {prefix}_item (id TEXT PRIMARY KEY)",
            )
            .unwrap(),
        ];
        if version_two {
            migrations.push(
                Migration::new(
                    2,
                    "item description",
                    "ALTER TABLE {prefix}_item ADD COLUMN description TEXT",
                )
                .unwrap(),
            );
        }
        MigrationBundle::new("foundation.verify_test", migrations).unwrap()
    }

    /// Cause/effect decision table: an application verifier cannot bootstrap a
    /// missing ledger, accepts a fully migrated bundle, and rejects a newly
    /// pending version without changing the database.
    #[tokio::test]
    async fn postgres_application_verification_is_read_only_and_fail_closed() {
        let Some(url) = std::env::var("AWAKEN_TEST_POSTGRES_URL").ok() else {
            return;
        };
        let pool = PgPool::connect(&url).await.unwrap();
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let prefix = format!("verify_{suffix}");
        let runner = PostgresMigrationRunner::with_prefix(pool.clone(), &prefix).unwrap();

        assert!(runner.verify_bundle(&bundle(false)).await.is_err());
        let ledger: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
            .bind(runner.ledger_table())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(ledger.is_none(), "verification must not create its ledger");

        runner.run_bundle(&bundle(false)).await.unwrap();
        runner.verify_bundle(&bundle(false)).await.unwrap();
        assert!(matches!(
            runner.verify_bundle(&bundle(true)).await,
            Err(MigrationError::PendingMigration { version: 2, .. })
        ));
        let description_column: Option<String> = sqlx::query_scalar::<_, String>(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_name = $1 AND column_name = 'description'",
        )
        .bind(format!("{prefix}_item"))
        .fetch_optional(&pool)
        .await
        .unwrap();
        assert!(
            description_column.is_none(),
            "verification must not apply DDL"
        );

        sqlx::raw_sql(&format!(
            "DROP TABLE {prefix}_item; DROP TABLE {prefix}_schema_migrations; \
             DROP TABLE {prefix}_schema_migrations_meta"
        ))
        .execute(&pool)
        .await
        .unwrap();
    }
}
