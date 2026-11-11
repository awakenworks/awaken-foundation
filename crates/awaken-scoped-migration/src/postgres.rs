//! PostgreSQL backend shell over the pure migration core.
//!
//! It owns only what is Postgres-specific: the `sqlx` driver, the advisory-lock
//! and ledger DDL. Migration bodies run through the multi-statement simple-query
//! path (`raw_sql`), mirroring SQLite's `execute_batch`, so a body is just SQL.
//! The apply decision is delegated to [`crate::plan`].

use std::collections::BTreeMap;

use sqlx::{AssertSqlSafe, PgPool, Row};

use crate::{
    AppliedMigration, Dialect, LedgerBootstrapAction, LedgerSchema, MigrationBundle,
    MigrationError, check_ledger_version, plan, render, sql_identifier,
};

/// The dialect this backend shell applies and checksums migrations under.
const DIALECT: Dialect = Dialect::Postgres;

// The aggregate consumes the catalog snapshot before either name is resolved.
// A relation discovered by a later syscache invalidation cannot count unless
// its OID was already present in that snapshot. Keep to_regclass for PostgreSQL's
// own search_path, privilege, shadowing and temporary-namespace semantics.
// Validated names are ASCII identifiers: C-locale lower plus name preserves
// unquoted folding and identifier truncation without a locale dependency.
const LEDGER_PRESENCE_SQL: &str = "
    SELECT COALESCE(pg_catalog.to_regclass($1)::pg_catalog.oid OPERATOR(pg_catalog.=)
                       ANY(catalog_snapshot.candidate_oids), false) AS ledger_exists,
           COALESCE(pg_catalog.to_regclass($2)::pg_catalog.oid OPERATOR(pg_catalog.=)
                       ANY(catalog_snapshot.candidate_oids), false) AS meta_exists
    FROM (
        SELECT pg_catalog.array_agg(relations.oid) AS candidate_oids
        FROM pg_catalog.pg_class AS relations
        WHERE relations.relname OPERATOR(pg_catalog.=) ANY(ARRAY[
            pg_catalog.lower($1 COLLATE pg_catalog.\"C\")::pg_catalog.name,
            pg_catalog.lower($2 COLLATE pg_catalog.\"C\")::pg_catalog.name
        ])
    ) AS catalog_snapshot";

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
            // Migration is the trusted SQL-authoring boundary; the prefix was
            // validated by sql_identifier, and receipt values remain bound.
            let sql = render(migration.sql_for(DIALECT), DIALECT, &self.prefix);
            sqlx::raw_sql(AssertSqlSafe(sql))
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
            sqlx::query(AssertSqlSafe(insert_sql))
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
        self.assert_ledger_presence().await?;
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

    async fn assert_ledger_presence(&self) -> Result<(), MigrationError> {
        let presence = sqlx::query(LEDGER_PRESENCE_SQL)
            .bind(self.ledger.ledger_table())
            .bind(self.ledger.meta_table())
            .fetch_one(&self.pool)
            .await
            .map_err(pg_error("postgres_migration_verify_probe"))?;
        self.ledger.verify_presence(
            presence
                .try_get("ledger_exists")
                .map_err(pg_error("postgres_migration_verify_decode"))?,
            presence
                .try_get("meta_exists")
                .map_err(pg_error("postgres_migration_verify_decode"))?,
        )
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

        let presence = sqlx::query(LEDGER_PRESENCE_SQL)
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
            // These statements come only from the validated LedgerSchema.
            for statement in self.ledger.create_statements(DIALECT) {
                sqlx::raw_sql(AssertSqlSafe(statement))
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
        let rows = sqlx::query(AssertSqlSafe(sql))
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
        let rows = sqlx::query(AssertSqlSafe(sql))
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

    struct OwnedSchema {
        admin: PgPool,
        url: String,
        name: String,
        nonce: String,
    }

    impl OwnedSchema {
        async fn create() -> Option<Self> {
            let url = std::env::var("AWAKEN_TEST_POSTGRES_URL").ok()?;
            let admin = sqlx::postgres::PgPoolOptions::new()
                .max_connections(3)
                .connect(&url)
                .await
                .unwrap();
            let identities = sqlx::query(
                "SELECT md5(random()::text || clock_timestamp()::text || pg_backend_pid()::text) \
                        AS identity, md5(random()::text || clock_timestamp()::text) AS nonce",
            )
            .fetch_one(&admin)
            .await
            .unwrap();
            let identity: String = identities.get("identity");
            assert_eq!(identity.len(), 32);
            assert!(identity.bytes().all(|byte| byte.is_ascii_hexdigit()));
            let name = format!("foundation_ledger_{identity}");
            let nonce: String = identities.get("nonce");
            assert_eq!(nonce.len(), 32);
            assert!(nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
            sqlx::raw_sql(AssertSqlSafe(format!(
                "CREATE SCHEMA {name}; COMMENT ON SCHEMA {name} IS '{nonce}'"
            )))
            .execute(&admin)
            .await
            .unwrap();
            println!("foundation ledger fixture: schema={name} nonce={nonce}");
            let fixture = Self {
                admin,
                url,
                name,
                nonce,
            };
            fixture.assert_owner().await;
            Some(fixture)
        }

        async fn assert_owner(&self) {
            let owned: Option<bool> = sqlx::query_scalar(
                "SELECT pg_get_userbyid(nspowner) = current_user \
                    AND obj_description(oid, 'pg_namespace') = $2 \
                 FROM pg_namespace WHERE nspname = $1",
            )
            .bind(&self.name)
            .bind(&self.nonce)
            .fetch_one(&self.admin)
            .await
            .unwrap();
            assert_eq!(owned, Some(true), "refuse a foreign fixture namespace");
        }

        async fn pool(&self, application: &str) -> PgPool {
            let pool = self.pool_on_path(application, &self.name).await;
            let actual: String = sqlx::query_scalar("SELECT current_schema()")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(actual, self.name, "never initialize another schema");
            pool
        }

        async fn pool_on_path(&self, application: &str, path: &str) -> PgPool {
            let options: sqlx::postgres::PgConnectOptions = self.url.parse().unwrap();
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect_with(
                    options
                        .application_name(application)
                        .options([("search_path", path)]),
                )
                .await
                .unwrap()
        }

        async fn cleanup(self) {
            self.assert_owner().await;
            sqlx::raw_sql(AssertSqlSafe(format!("DROP SCHEMA {} CASCADE", self.name)))
                .execute(&self.admin)
                .await
                .unwrap();
            self.admin.close().await;
        }
    }

    async fn assert_presence(pool: &PgPool, ledger: &str, meta: &str, expected: (bool, bool)) {
        let actual: (bool, bool) = sqlx::query_as(LEDGER_PRESENCE_SQL)
            .bind(ledger)
            .bind(meta)
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(actual, expected);
        let original_resolution: (bool, bool) = sqlx::query_as(
            "SELECT pg_catalog.to_regclass($1) IS NOT NULL, \
                    pg_catalog.to_regclass($2) IS NOT NULL",
        )
        .bind(ledger)
        .bind(meta)
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(actual, original_resolution);
    }

    async fn wait_for_bootstrap(pool: &PgPool, application: &str, ledger: &str) -> i32 {
        loop {
            let waiting: Vec<(i32,)> = sqlx::query_as(
                "SELECT activity.pid FROM pg_stat_activity AS activity \
                 JOIN pg_locks AS waiting ON waiting.pid = activity.pid \
                 WHERE activity.datname = current_database() \
                   AND activity.application_name = $1 \
                   AND waiting.locktype = 'advisory' AND NOT waiting.granted \
                   AND waiting.mode = 'ExclusiveLock' AND waiting.objsubid = 2 \
                   AND waiting.classid::bigint = (hashtext($2)::bigint & 4294967295) \
                   AND waiting.objid::bigint = \
                       (hashtext('ledger-bootstrap-v1')::bigint & 4294967295)",
            )
            .bind(application)
            .bind(ledger)
            .fetch_all(pool)
            .await
            .unwrap();
            match waiting.as_slice() {
                [(pid,)] => return *pid,
                [] => tokio::task::yield_now().await,
                _ => panic!("one-slot initializer has multiple bootstrap waiters"),
            }
        }
    }

    async fn cold_initialize(
        pool: PgPool,
        reuse_presence: bool,
    ) -> Result<Vec<AppliedMigration>, MigrationError> {
        let runner = PostgresMigrationRunner::with_prefix(pool.clone(), "runtime").unwrap();
        if reuse_presence {
            // Reuse the same persistent SQLx statement cache after the other
            // initializer commits, rather than relying on a first Parse.
            let absent: (bool, bool) = sqlx::query_as(LEDGER_PRESENCE_SQL)
                .bind(runner.ledger.ledger_table())
                .bind(runner.ledger.meta_table())
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(absent, (false, false));
        }
        // Match a consumer's prepared checksum preflight: warm only the ledger
        // name, then enter the actual runner without probing the meta name.
        let absent: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
            .bind(runner.ledger_table())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(absent.is_none());
        runner.run_bundle(&bundle(false)).await
    }

    #[tokio::test]
    async fn postgres_cold_initializers_observe_one_complete_ledger_generation() {
        assert_cold_initializers(false).await;
    }

    #[tokio::test]
    async fn postgres_cached_presence_observes_one_complete_ledger_generation() {
        assert_cold_initializers(true).await;
    }

    async fn assert_cold_initializers(reuse_presence: bool) {
        let Some(fixture) = OwnedSchema::create().await else {
            return;
        };
        let app_b = format!("{}-b", fixture.name);
        let app_a = format!("{}-a", fixture.name);
        let pool_b = fixture.pool(&app_b).await;
        let pool_a = fixture.pool(&app_a).await;
        let pid_b: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&pool_b)
            .await
            .unwrap();
        let pid_a: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&pool_a)
            .await
            .unwrap();
        assert_ne!(pid_b, pid_a);
        let mut controller = fixture.admin.begin().await.unwrap();
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtext('runtime_schema_migrations'), \
                                        hashtext('ledger-bootstrap-v1'))",
        )
        .execute(&mut *controller)
        .await
        .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut init_b = tokio::spawn(cold_initialize(pool_b.clone(), reuse_presence));
        let waiting_b = tokio::time::timeout_at(
            deadline,
            wait_for_bootstrap(&fixture.admin, &app_b, "runtime_schema_migrations"),
        )
        .await;
        if !waiting_b.as_ref().is_ok_and(|pid| *pid == pid_b) {
            init_b.abort();
            controller.rollback().await.unwrap();
            panic!("B did not wait on its original backend: {waiting_b:?}");
        }
        let mut init_a = tokio::spawn(cold_initialize(pool_a.clone(), reuse_presence));
        let waiting_a = tokio::time::timeout_at(
            deadline,
            wait_for_bootstrap(&fixture.admin, &app_a, "runtime_schema_migrations"),
        )
        .await;
        if !waiting_a.as_ref().is_ok_and(|pid| *pid == pid_a) {
            init_a.abort();
            init_b.abort();
            controller.rollback().await.unwrap();
            panic!("A did not wait on its original backend: {waiting_a:?}");
        }
        controller.commit().await.unwrap();
        let initialized =
            tokio::time::timeout_at(deadline, async { tokio::join!(&mut init_b, &mut init_a) })
                .await;
        let (b, a) = match initialized {
            Ok(results) => results,
            Err(error) => {
                init_b.abort();
                init_a.abort();
                panic!("cold initialization deadline exceeded: {error}");
            }
        };
        let b = b.unwrap().unwrap();
        let a = a.unwrap().unwrap();
        assert_eq!(b.len() + a.len(), 1, "one migration receipt is published");
        let runner = PostgresMigrationRunner::with_prefix(pool_a.clone(), "runtime").unwrap();
        runner.verify_bundle(&bundle(false)).await.unwrap();
        pool_a.close().await;
        pool_b.close().await;
        fixture.cleanup().await;
    }

    #[tokio::test]
    async fn postgres_actual_partial_ledgers_are_never_bootstrapped_or_repaired() {
        let Some(fixture) = OwnedSchema::create().await else {
            return;
        };
        let pool = fixture.pool("foundation-ledger-partial").await;
        for (prefix, ledger_exists) in [("ledger_only", true), ("meta_only", false)] {
            let schema = LedgerSchema::with_prefix(prefix).unwrap();
            let statements = schema.create_statements(DIALECT);
            let selected = if ledger_exists {
                &statements[..1]
            } else {
                &statements[1..]
            };
            for statement in selected {
                sqlx::raw_sql(AssertSqlSafe(statement.as_str()))
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            let runner = PostgresMigrationRunner::with_prefix(pool.clone(), prefix).unwrap();
            for error in [
                runner.run_bundle(&bundle(false)).await.unwrap_err(),
                runner.verify_bundle(&bundle(false)).await.unwrap_err(),
            ] {
                assert!(matches!(error, MigrationError::IncompleteLedger {
                    ledger_exists: actual_ledger, meta_exists: actual_meta, ..
                } if actual_ledger == ledger_exists && actual_meta != ledger_exists));
            }
            let actual: Vec<(String,)> = sqlx::query_as(
                "SELECT relations.relname::text FROM pg_catalog.pg_class AS relations \
                 JOIN pg_catalog.pg_namespace AS namespaces ON namespaces.oid = relations.relnamespace \
                 WHERE namespaces.nspname = $1 AND relations.relname::text IN ($2, $3)",
            ).bind(&fixture.name).bind(schema.ledger_table()).bind(schema.meta_table())
                .fetch_all(&fixture.admin).await.unwrap();
            assert_eq!(
                actual.len(),
                1,
                "a genuine partial generation stays unchanged"
            );
            assert_eq!(
                actual[0].0,
                if ledger_exists {
                    schema.ledger_table()
                } else {
                    schema.meta_table()
                }
            );
        }
        pool.close().await;
        fixture.cleanup().await;
    }

    #[tokio::test]
    async fn postgres_presence_preserves_unquoted_folding_and_identifier_truncation() {
        let Some(fixture) = OwnedSchema::create().await else {
            return;
        };
        let pool = fixture.pool("foundation-ledger-identifiers").await;
        // The second meta name is 64 ASCII bytes. PostgreSQL truncates it to
        // 63, while the ledger name remains distinct and fits in full.
        for prefix in ["MixedCase".to_string(), format!("Long{}", "x".repeat(37))] {
            let runner = PostgresMigrationRunner::with_prefix(pool.clone(), &prefix).unwrap();
            assert_eq!(runner.run_bundle(&bundle(false)).await.unwrap().len(), 1);
            assert!(runner.run_bundle(&bundle(false)).await.unwrap().is_empty());
            runner.verify_bundle(&bundle(false)).await.unwrap();
            assert_presence(
                &pool,
                runner.ledger.ledger_table(),
                runner.ledger.meta_table(),
                (true, true),
            )
            .await;
        }
        pool.close().await;
        fixture.cleanup().await;
    }

    #[tokio::test]
    async fn postgres_presence_preserves_search_path_shadowing_and_relation_kinds() {
        let Some(back) = OwnedSchema::create().await else {
            return;
        };
        let front = OwnedSchema::create().await.unwrap();
        let back_pool = back.pool("foundation-ledger-path-back").await;
        let runner = PostgresMigrationRunner::with_prefix(back_pool.clone(), "path").unwrap();
        runner.run_bundle(&bundle(false)).await.unwrap();
        let front_pool = front.pool("foundation-ledger-path-front").await;
        // Same-name relations outside the search path must not leak through
        // the catalog candidate array; implicit pg_catalog remains visible.
        assert_presence(
            &front_pool,
            "path_schema_migrations",
            "path_schema_migrations_meta",
            (false, false),
        )
        .await;
        assert_presence(&front_pool, "pg_class", "pg_namespace", (true, true)).await;

        sqlx::raw_sql(AssertSqlSafe(format!(
            "CREATE VIEW {}.path_schema_migrations_meta AS \
             SELECT 999::bigint AS ledger_version",
            front.name
        )))
        .execute(&front.admin)
        .await
        .unwrap();
        let shadow_path = format!("{},{}", front.name, back.name);
        let shadow_pool = front
            .pool_on_path("foundation-ledger-shadow", &shadow_path)
            .await;
        assert_presence(
            &shadow_pool,
            "path_schema_migrations",
            "path_schema_migrations_meta",
            (true, true),
        )
        .await;
        let shadow = PostgresMigrationRunner::with_prefix(shadow_pool.clone(), "path").unwrap();
        assert!(matches!(
            shadow.verify_bundle(&bundle(false)).await,
            Err(MigrationError::LedgerVersionMismatch { found: 999, .. })
        ));
        // Each name resolves independently: the ledger came from the later
        // schema above. Sequences and views also count as relations, matching
        // the original to_regclass probe rather than filtering to tables.
        sqlx::raw_sql(AssertSqlSafe(format!(
            "CREATE SEQUENCE {}.path_schema_migrations",
            front.name
        )))
        .execute(&front.admin)
        .await
        .unwrap();
        assert_presence(
            &front_pool,
            "path_schema_migrations",
            "path_schema_migrations_meta",
            (true, true),
        )
        .await;
        let valid_path = format!("{},{}", back.name, front.name);
        let valid_pool = back
            .pool_on_path("foundation-ledger-valid-path", &valid_path)
            .await;
        PostgresMigrationRunner::with_prefix(valid_pool.clone(), "path")
            .unwrap()
            .verify_bundle(&bundle(false))
            .await
            .unwrap();
        valid_pool.close().await;
        shadow_pool.close().await;
        front_pool.close().await;
        back_pool.close().await;
        front.cleanup().await;
        back.cleanup().await;
    }

    #[tokio::test]
    async fn postgres_presence_uses_catalog_operators_with_catalog_last_on_the_path() {
        let Some(fixture) = OwnedSchema::create().await else {
            return;
        };
        let pool = fixture.pool("foundation-ledger-before-operators").await;
        let runner = PostgresMigrationRunner::with_prefix(pool.clone(), "operator_probe").unwrap();
        runner.run_bundle(&bundle(false)).await.unwrap();
        sqlx::raw_sql(AssertSqlSafe(format!(
            "CREATE FUNCTION {schema}.name_equal(pg_catalog.name, pg_catalog.name) \
                 RETURNS boolean LANGUAGE SQL IMMUTABLE AS 'SELECT false'; \
             CREATE OPERATOR {schema}.= \
                 (LEFTARG = pg_catalog.name, RIGHTARG = pg_catalog.name, \
                  PROCEDURE = {schema}.name_equal); \
             CREATE FUNCTION {schema}.oid_equal(pg_catalog.oid, pg_catalog.oid) \
                 RETURNS boolean LANGUAGE SQL IMMUTABLE AS 'SELECT false'; \
             CREATE OPERATOR {schema}.= \
                 (LEFTARG = pg_catalog.oid, RIGHTARG = pg_catalog.oid, \
                  PROCEDURE = {schema}.oid_equal); \
             CREATE COLLATION {schema}.\"C\" FROM pg_catalog.\"POSIX\"",
            schema = fixture.name
        )))
        .execute(&fixture.admin)
        .await
        .unwrap();
        let path = format!("{},pg_catalog", fixture.name);
        let shadow = fixture
            .pool_on_path("foundation-ledger-operators", &path)
            .await;
        // Prove that ordinary unqualified comparisons resolve to the hostile
        // operators, while the presence probe still uses catalog equality.
        let ordinary: (bool, bool) = sqlx::query_as(
            "SELECT 'same'::pg_catalog.name = 'same'::pg_catalog.name, \
                    1::pg_catalog.oid = 1::pg_catalog.oid",
        )
        .fetch_one(&shadow)
        .await
        .unwrap();
        assert_eq!(ordinary, (false, false));
        assert_presence(
            &shadow,
            runner.ledger.ledger_table(),
            runner.ledger.meta_table(),
            (true, true),
        )
        .await;
        shadow.close().await;
        pool.close().await;
        fixture.cleanup().await;
    }

    #[tokio::test]
    async fn postgres_presence_keeps_an_uninitialized_temp_path_read_only() {
        let Some(fixture) = OwnedSchema::create().await else {
            return;
        };
        let persistent = fixture.pool("foundation-ledger-persistent").await;
        PostgresMigrationRunner::with_prefix(persistent.clone(), "temp_path")
            .unwrap()
            .run_bundle(&bundle(false))
            .await
            .unwrap();
        let path = format!("pg_temp,{}", fixture.name);
        let pool = fixture.pool_on_path("foundation-ledger-temp", &path).await;
        let initial_temp: i64 = sqlx::query_scalar("SELECT pg_my_temp_schema()::bigint")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(initial_temp, 0);
        let runner = PostgresMigrationRunner::with_prefix(pool.clone(), "temp_path").unwrap();
        runner.verify_bundle(&bundle(false)).await.unwrap();
        assert_presence(
            &pool,
            "temp_path_schema_migrations",
            "temp_path_schema_migrations_meta",
            (true, true),
        )
        .await;
        assert_presence(&pool, "absent_ledger", "absent_meta", (false, false)).await;
        let after_verify: i64 = sqlx::query_scalar("SELECT pg_my_temp_schema()::bigint")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            after_verify, 0,
            "presence checks must not initialize pg_temp"
        );

        sqlx::raw_sql(AssertSqlSafe(
            "CREATE TEMP SEQUENCE temp_path_schema_migrations; \
             CREATE TEMP VIEW temp_path_schema_migrations_meta AS \
             SELECT 888::bigint AS ledger_version",
        ))
        .execute(&pool)
        .await
        .unwrap();
        assert_presence(
            &pool,
            "temp_path_schema_migrations",
            "temp_path_schema_migrations_meta",
            (true, true),
        )
        .await;
        assert!(matches!(
            runner.verify_bundle(&bundle(false)).await,
            Err(MigrationError::LedgerVersionMismatch { found: 888, .. })
        ));
        pool.close().await;
        persistent.close().await;
        fixture.cleanup().await;
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

        assert!(matches!(
            runner.verify_bundle(&bundle(false)).await,
            Err(MigrationError::MissingLedger { .. })
        ));
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

        sqlx::raw_sql(AssertSqlSafe(format!(
            "DROP TABLE {prefix}_item; DROP TABLE {prefix}_schema_migrations; \
             DROP TABLE {prefix}_schema_migrations_meta"
        )))
        .execute(&pool)
        .await
        .unwrap();
    }
}
