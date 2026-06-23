//! Backend-agnostic SQL schema migration ledger.
//!
//! The crate splits a migration runner into two layers:
//!
//! - a **pure core** (this module): the [`Migration`] / [`MigrationBundle`]
//!   value types, validation, checksums, the error taxonomy, and [`plan`] — the
//!   one function that decides what still needs to run. None of it touches a
//!   database, so it is fully unit-testable without a connection.
//! - a thin **backend shell** per driver (e.g. [`postgres`]) that only fetches
//!   the already-applied versions, calls [`plan`], and applies the returned
//!   migrations using its own driver, dialect, and transaction strategy.
//!
//! Migration SQL is dialect-bound and therefore lives with each backend's
//! bundles, never here; only the *mechanism* is shared.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "sqlite")]
pub mod sqlite;

/// One ordered SQL migration inside a named bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    version: i64,
    description: String,
    sql: String,
}

impl Migration {
    pub fn new(
        version: i64,
        description: impl Into<String>,
        sql: impl Into<String>,
    ) -> Result<Self, MigrationError> {
        let migration = Self {
            version,
            description: description.into(),
            sql: sql.into(),
        };
        migration.validate()?;
        Ok(migration)
    }

    #[must_use]
    pub const fn version(&self) -> i64 {
        self.version
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// Human-readable, zero-padded version label (`V0001`), derived from the
    /// integer version. Used in diagnostics and as the leading field of the
    /// checksum so reordering a recorded version fails closed.
    #[must_use]
    pub fn label(&self) -> String {
        format!("V{:04}", self.version)
    }

    /// Dialect-independent identity of the migration:
    /// `SHA-256(label_bytes || 0x00 || template_bytes)`.
    ///
    /// The hash is taken over the neutral template (the stored token SQL),
    /// never the per-dialect rendered SQL, so the same migration verifies to
    /// the same checksum on Postgres and SQLite even though the SQL each
    /// backend applies legitimately differs. The label is included so a
    /// reordered version (same template, different position) is detected as
    /// drift. The `0x00` separator keeps the label and template fields
    /// unambiguous.
    #[must_use]
    pub fn checksum(&self) -> String {
        let label = self.label();
        let mut input = Vec::with_capacity(label.len() + 1 + self.sql.len());
        input.extend_from_slice(label.as_bytes());
        input.push(0x00);
        input.extend_from_slice(self.sql.as_bytes());
        sha256_hex(&input)
    }

    /// Dialect-neutral validation. Statement-count and other driver-specific
    /// limits are enforced by the backend shell, not here.
    fn validate(&self) -> Result<(), MigrationError> {
        if self.version <= 0 {
            return Err(MigrationError::InvalidMigration {
                version: self.version,
                reason: "version must be positive",
            });
        }
        if self.description.trim().is_empty() {
            return Err(MigrationError::InvalidMigration {
                version: self.version,
                reason: "description must not be blank",
            });
        }
        if self.sql.trim().is_empty() {
            return Err(MigrationError::InvalidMigration {
                version: self.version,
                reason: "sql must not be blank",
            });
        }
        Ok(())
    }
}

/// A service-owned, independently versioned migration stream.
///
/// Bundles carry no cross-bundle dependencies: services are deployed split or
/// aggregated, so no bundle may hard-couple to another (a cross-service FK would
/// defeat splitting). When several bundles share one database, execution order
/// is the registration order passed to the backend runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationBundle {
    bundle_id: String,
    migrations: Vec<Migration>,
}

impl MigrationBundle {
    pub fn new(
        bundle_id: impl Into<String>,
        migrations: Vec<Migration>,
    ) -> Result<Self, MigrationError> {
        let bundle = Self {
            bundle_id: bundle_id.into(),
            migrations,
        };
        bundle.validate()?;
        Ok(bundle)
    }

    #[must_use]
    pub fn bundle_id(&self) -> &str {
        &self.bundle_id
    }

    #[must_use]
    pub fn migrations(&self) -> &[Migration] {
        &self.migrations
    }

    fn validate(&self) -> Result<(), MigrationError> {
        validate_bundle_id(&self.bundle_id)?;
        let mut seen = BTreeSet::new();
        let mut previous = 0;
        for migration in &self.migrations {
            migration.validate()?;
            if !seen.insert(migration.version) {
                return Err(MigrationError::DuplicateMigrationVersion {
                    bundle_id: self.bundle_id.clone(),
                    version: migration.version,
                });
            }
            if migration.version <= previous {
                return Err(MigrationError::InvalidMigrationOrder {
                    bundle_id: self.bundle_id.clone(),
                    previous,
                    current: migration.version,
                });
            }
            previous = migration.version;
        }
        Ok(())
    }
}

/// One migration that a runner applied (or confirmed) during a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedMigration {
    pub bundle_id: String,
    pub version: i64,
    pub checksum: String,
    pub description: String,
}

/// Decide which migrations in `bundle` still need to run, given the versions
/// already recorded in the ledger (`applied`: version → checksum).
///
/// Pure and backend-agnostic: it performs every interesting decision — unknown
/// applied version, checksum drift, and already-applied skip — and returns the
/// migrations to apply in order. A backend shell only fetches `applied`, calls
/// this, and applies the result with its own driver.
pub fn plan<'a>(
    bundle: &'a MigrationBundle,
    applied: &BTreeMap<i64, String>,
) -> Result<Vec<&'a Migration>, MigrationError> {
    validate_applied_versions(bundle, applied)?;
    let mut pending = Vec::new();
    for migration in bundle.migrations() {
        match applied.get(&migration.version()) {
            Some(existing) => {
                let recomputed = migration.checksum();
                if existing != &recomputed {
                    return Err(MigrationError::ChecksumMismatch {
                        bundle_id: bundle.bundle_id().to_string(),
                        version: migration.version(),
                        // The ledger value is the recorded source of truth; the
                        // recomputed value is what current code produces.
                        expected: existing.clone(),
                        actual: recomputed,
                    });
                }
            }
            None => pending.push(migration),
        }
    }
    Ok(pending)
}

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("invalid SQL identifier prefix '{0}'")]
    InvalidSqlIdentifier(String),
    #[error("invalid migration bundle id '{0}'")]
    InvalidBundleId(String),
    #[error("invalid migration {version}: {reason}")]
    InvalidMigration { version: i64, reason: &'static str },
    #[error("bundle '{bundle_id}' contains duplicate migration version {version}")]
    DuplicateMigrationVersion { bundle_id: String, version: i64 },
    #[error(
        "bundle '{bundle_id}' migrations are not strictly increasing: {previous} then {current}"
    )]
    InvalidMigrationOrder {
        bundle_id: String,
        previous: i64,
        current: i64,
    },
    #[error("duplicate migration bundle '{0}'")]
    DuplicateBundle(String),
    #[error("bundle '{bundle_id}' has unknown applied version {version}")]
    UnknownAppliedVersion { bundle_id: String, version: i64 },
    #[error(
        "bundle '{bundle_id}' migration {version} checksum mismatch: expected {expected}, actual {actual}"
    )]
    ChecksumMismatch {
        bundle_id: String,
        version: i64,
        expected: String,
        actual: String,
    },
    /// A backend driver operation failed. The core never constructs this; each
    /// backend shell maps its driver error here so the error type stays
    /// dialect-agnostic.
    #[error("migration backend operation '{operation}' failed: {message}")]
    Backend {
        operation: &'static str,
        message: String,
    },
}

/// Validate a SQL identifier used as a table-name prefix (ASCII, leading
/// letter, `[A-Za-z0-9_]`).
pub fn sql_identifier(value: &str) -> Result<String, MigrationError> {
    let valid = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic());
    if valid {
        return Ok(value.to_string());
    }
    Err(MigrationError::InvalidSqlIdentifier(value.to_string()))
}

fn validate_applied_versions(
    bundle: &MigrationBundle,
    applied_versions: &BTreeMap<i64, String>,
) -> Result<(), MigrationError> {
    let declared = bundle
        .migrations()
        .iter()
        .map(Migration::version)
        .collect::<BTreeSet<_>>();
    for version in applied_versions.keys() {
        if !declared.contains(version) {
            return Err(MigrationError::UnknownAppliedVersion {
                bundle_id: bundle.bundle_id().to_string(),
                version: *version,
            });
        }
    }
    Ok(())
}

fn validate_bundle_id(bundle_id: &str) -> Result<(), MigrationError> {
    let valid = !bundle_id.is_empty()
        && bundle_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        && bundle_id
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric());
    if valid {
        return Ok(());
    }
    Err(MigrationError::InvalidBundleId(bundle_id.to_string()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            use std::fmt::Write;
            let _ = write!(&mut out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
mod checksum_tests {
    use super::*;

    #[test]
    fn hashes_label_nul_then_template() {
        let migration = Migration::new(1, "first", "CREATE TABLE a (id TEXT)").unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(b"V0001");
        expected.push(0x00);
        expected.extend_from_slice(b"CREATE TABLE a (id TEXT)");
        assert_eq!(migration.checksum(), sha256_hex(&expected));
    }

    #[test]
    fn label_is_zero_padded() {
        assert_eq!(Migration::new(1, "x", "SELECT 1").unwrap().label(), "V0001");
        assert_eq!(
            Migration::new(4242, "x", "SELECT 1").unwrap().label(),
            "V4242"
        );
    }

    #[test]
    fn checksum_is_dialect_independent_for_one_template() {
        // The checksum is taken over the neutral template, never the rendered
        // SQL, so the same migration verifies identically on every backend.
        let template = "CREATE TABLE {prefix}_event (id {pk_autoinc}, payload {json})";
        let a = Migration::new(7, "events", template).unwrap();
        let b = Migration::new(7, "events", template).unwrap();
        assert_eq!(a.checksum(), b.checksum());
    }

    #[test]
    fn reordering_a_version_changes_the_checksum() {
        // Same template, different version label ⇒ different identity, so a
        // reordered recorded version fails closed in `plan()`.
        let template = "CREATE TABLE a (id TEXT)";
        let at_one = Migration::new(1, "x", template).unwrap();
        let at_two = Migration::new(2, "x", template).unwrap();
        assert_ne!(at_one.checksum(), at_two.checksum());
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    fn bundle() -> MigrationBundle {
        MigrationBundle::new(
            "runtime.core",
            vec![
                Migration::new(1, "first", "CREATE TABLE a (id TEXT)").unwrap(),
                Migration::new(2, "second", "CREATE TABLE b (id TEXT)").unwrap(),
            ],
        )
        .unwrap()
    }

    #[test]
    fn plans_all_when_ledger_empty() {
        let b = bundle();
        let pending = plan(&b, &BTreeMap::new()).unwrap();
        assert_eq!(
            pending.iter().map(|m| m.version()).collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn skips_already_applied_with_matching_checksum() {
        let b = bundle();
        let mut applied = BTreeMap::new();
        applied.insert(1, b.migrations()[0].checksum());
        let pending = plan(&b, &applied).unwrap();
        assert_eq!(pending.iter().map(|m| m.version()).collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn rejects_checksum_drift() {
        let b = bundle();
        let mut applied = BTreeMap::new();
        applied.insert(1, "deadbeef".to_string());
        let err = plan(&b, &applied).unwrap_err();
        assert!(matches!(
            err,
            MigrationError::ChecksumMismatch { version: 1, expected, .. } if expected == "deadbeef"
        ));
    }

    #[test]
    fn rejects_unknown_applied_version() {
        let b = bundle();
        let mut applied = BTreeMap::new();
        applied.insert(99, "x".to_string());
        assert!(matches!(
            plan(&b, &applied).unwrap_err(),
            MigrationError::UnknownAppliedVersion { version: 99, .. }
        ));
    }

    #[test]
    fn bundle_rejects_non_increasing_versions() {
        let err = MigrationBundle::new(
            "runtime.core",
            vec![
                Migration::new(2, "a", "CREATE TABLE a (id TEXT)").unwrap(),
                Migration::new(2, "b", "CREATE TABLE b (id TEXT)").unwrap(),
            ],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            MigrationError::DuplicateMigrationVersion { version: 2, .. }
        ));
    }
}
