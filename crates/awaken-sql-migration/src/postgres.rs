//! PostgreSQL backend shell over the pure migration core.
//!
//! It owns only what is Postgres-specific: the `sqlx` driver, the advisory-lock
//! and ledger DDL, and the single-statement constraint of the prepared
//! protocol. The apply decision is delegated to [`crate::plan`].

use std::collections::BTreeMap;

use sqlx::{PgPool, Row};

use crate::{AppliedMigration, MigrationBundle, MigrationError, plan, sql_identifier};

/// PostgreSQL-backed migration runner with a per-prefix ledger table.
#[derive(Debug, Clone)]
pub struct PostgresMigrationRunner {
    pool: PgPool,
    ledger_table: String,
    applied_by: String,
}

impl PostgresMigrationRunner {
    pub fn with_prefix(pool: PgPool, prefix: impl AsRef<str>) -> Result<Self, MigrationError> {
        let prefix = sql_identifier(prefix.as_ref())?;
        Ok(Self {
            pool,
            ledger_table: format!("{prefix}_schema_migrations"),
            applied_by: "awaken-sql-migration".to_string(),
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
        // Fail fast (before touching the database) on the prepared-protocol
        // single-statement limit. This is a Postgres-driver constraint, not a
        // property of migrations in general, so it lives here, not in the core.
        for migration in bundle.migrations() {
            if !is_single_statement(migration.sql()) {
                return Err(MigrationError::InvalidMigration {
                    version: migration.version(),
                    reason: "sql must contain exactly one statement",
                });
            }
        }

        self.ensure_ledger().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(pg_error("postgres_migration_begin"))?;

        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
            .bind(&self.ledger_table)
            .bind(bundle.bundle_id())
            .execute(&mut *tx)
            .await
            .map_err(pg_error("postgres_migration_lock"))?;

        let applied_versions = self.applied_versions(&mut tx, bundle.bundle_id()).await?;
        let pending = plan(bundle, &applied_versions)?;

        let mut applied = Vec::new();
        for migration in pending {
            sqlx::query(migration.sql())
                .execute(&mut *tx)
                .await
                .map_err(pg_error("postgres_migration_apply"))?;

            let insert_sql = format!(
                "INSERT INTO {} (bundle_id, version, checksum, description, applied_by)
                 VALUES ($1, $2, $3, $4, $5)",
                self.ledger_table
            );
            let checksum = migration.checksum();
            sqlx::query(&insert_sql)
                .bind(bundle.bundle_id())
                .bind(migration.version())
                .bind(&checksum)
                .bind(migration.description())
                .bind(&self.applied_by)
                .execute(&mut *tx)
                .await
                .map_err(pg_error("postgres_migration_record"))?;

            applied.push(AppliedMigration {
                bundle_id: bundle.bundle_id().to_string(),
                version: migration.version(),
                checksum,
                description: migration.description().to_string(),
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
        Ok(())
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

/// True if `sql` holds at most one top-level statement.
///
/// The prepared protocol used by the runner rejects multiple `;`-separated
/// statements. Semicolons inside single-quoted strings, dollar-quoted bodies
/// (`$$ … $$` / `$tag$ … $tag$`), and `--` / `/* */` comments are not
/// separators; a single trailing `;` is allowed.
fn is_single_statement(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut terminators = 0;
    let mut content_after_terminator = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    let quote = bytes[i] == b'\'';
                    i += 1;
                    if quote {
                        break;
                    }
                }
                content_after_terminator = terminators > 0;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 2;
            }
            b'$' if dollar_tag_len(&bytes[i..]).is_some() => {
                let tag_len = dollar_tag_len(&bytes[i..]).expect("checked by match guard");
                let tag = &bytes[i..i + tag_len];
                i += tag_len;
                while i < bytes.len() && !bytes[i..].starts_with(tag) {
                    i += 1;
                }
                i = (i + tag_len).min(bytes.len());
                content_after_terminator = terminators > 0;
            }
            b';' => {
                terminators += 1;
                i += 1;
            }
            other => {
                if !other.is_ascii_whitespace() && terminators > 0 {
                    content_after_terminator = true;
                }
                i += 1;
            }
        }
        if content_after_terminator {
            return false;
        }
    }
    true
}

/// Length of a dollar-quote opening tag (`$tag$`) at the start of `bytes`, or
/// `None` if it is not a tag (e.g. a bare `$1` parameter marker).
fn dollar_tag_len(bytes: &[u8]) -> Option<usize> {
    debug_assert_eq!(bytes.first(), Some(&b'$'));
    let mut j = 1;
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
        j += 1;
    }
    if bytes.get(j) == Some(&b'$') {
        Some(j + 1)
    } else {
        None
    }
}

#[cfg(test)]
mod statement_validation_tests {
    use super::*;

    #[test]
    fn accepts_single_statement_without_trailing_semicolon() {
        assert!(is_single_statement(
            "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY)"
        ));
    }

    #[test]
    fn accepts_single_trailing_semicolon() {
        assert!(is_single_statement("CREATE TABLE t (id TEXT);"));
        assert!(is_single_statement("CREATE TABLE t (id TEXT);   \n"));
    }

    #[test]
    fn allows_semicolons_inside_string_literals() {
        assert!(is_single_statement(
            "INSERT INTO t (note) VALUES ('a; b; c')"
        ));
    }

    #[test]
    fn allows_semicolons_inside_dollar_quoted_body() {
        assert!(is_single_statement(
            "CREATE FUNCTION f() RETURNS void AS $$ BEGIN x; y; END $$ LANGUAGE plpgsql"
        ));
        assert!(is_single_statement(
            "CREATE FUNCTION f() RETURNS void AS $body$ BEGIN x; END $body$ LANGUAGE plpgsql"
        ));
    }

    #[test]
    fn allows_semicolons_inside_comments() {
        assert!(is_single_statement(
            "CREATE TABLE t (id TEXT) -- drop; recreate;\n"
        ));
        assert!(is_single_statement("CREATE TABLE t (id TEXT) /* a; b */"));
    }

    #[test]
    fn rejects_two_top_level_statements() {
        assert!(!is_single_statement(
            "CREATE TABLE a (id TEXT); CREATE TABLE b (id TEXT)"
        ));
    }

    #[test]
    fn dollar_parameter_marker_is_not_a_tag() {
        assert_eq!(dollar_tag_len(b"$1, $2"), None);
        assert!(!is_single_statement(
            "UPDATE t SET a = $1; UPDATE t SET b = $2"
        ));
    }
}
