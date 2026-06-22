//! SQLite backend shell over the pure migration core.
//!
//! It owns only what is SQLite-specific: the synchronous `rusqlite` driver, the
//! ledger DDL, and a per-bundle transaction (SQLite is single-writer, so there
//! is no advisory lock). Unlike the Postgres shell there is no single-statement
//! limit — `execute_batch` runs multi-statement migrations. The apply decision
//! is delegated to [`crate::plan`].

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

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

    pub fn run_bundle(
        &self,
        conn: &Connection,
        bundle: &MigrationBundle,
    ) -> Result<Vec<AppliedMigration>, MigrationError> {
        self.ensure_ledger(conn)?;
        // `unchecked_transaction` takes `&Connection`, so the runner slots into
        // the stores' existing `create_tables(&Connection)` call sites without
        // threading a `&mut` through their lock guards. SQLite is single-writer,
        // so the implicit `DEFERRED` upgrade to a write lock on first DDL is
        // sufficient; there is no advisory-lock equivalent to acquire.
        let tx = conn
            .unchecked_transaction()
            .map_err(sqlite_error("sqlite_migration_begin"))?;

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
    fn rejects_duplicate_bundle_id() {
        let conn = Connection::open_in_memory().unwrap();
        let runner = SqliteMigrationRunner::with_prefix("awaken").unwrap();
        let err = runner
            .run_bundles(&conn, &[bundle(), bundle()])
            .unwrap_err();
        assert!(matches!(err, MigrationError::DuplicateBundle(id) if id == "runtime.core"));
    }
}
