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

/// Schema version of the ledger itself (its own bookkeeping tables), distinct
/// from the per-bundle migration versions it records.
///
/// The ledger has no migration path of its own (it cannot ledger its own
/// creation), so instead of evolving it in place the runner stamps a fresh
/// ledger with this value and refuses to operate on a ledger stamped with any
/// other — a different generation of the migrator wrote it, and silently reusing
/// it could corrupt bookkeeping. The full meta-migration mechanism is deferred;
/// until it exists the ledger schema is frozen at this version.
pub const LEDGER_VERSION: i64 = 1;

/// Fail closed unless the ledger's stamped version equals [`LEDGER_VERSION`].
///
/// Pure and backend-agnostic so both backend shells share one decision: each
/// reads the stamped value with its own driver, then defers the verdict here.
pub fn check_ledger_version(ledger_table: &str, found: i64) -> Result<(), MigrationError> {
    if found == LEDGER_VERSION {
        return Ok(());
    }
    Err(MigrationError::LedgerVersionMismatch {
        ledger_table: ledger_table.to_string(),
        expected: LEDGER_VERSION,
        found,
    })
}

/// The SQL dialect a backend runner targets.
///
/// Each backend shell knows its own dialect (`PostgresMigrationRunner` ⇒
/// [`Postgres`](Dialect::Postgres), `SqliteMigrationRunner` ⇒
/// [`Sqlite`](Dialect::Sqlite)) and threads it into [`plan`] and
/// [`Migration::checksum_for`]. A portable migration resolves identically for
/// every dialect; a [`Migration::per_dialect`] escape-hatch migration resolves
/// to the selected dialect's body and checksum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    Sqlite,
}

impl Dialect {
    /// `{json}` column type: structured JSON storage.
    const fn json(self) -> &'static str {
        match self {
            Self::Postgres => "JSONB",
            Self::Sqlite => "TEXT",
        }
    }

    /// `{timestamptz}` column type: an instant with time-zone semantics.
    const fn timestamptz(self) -> &'static str {
        match self {
            Self::Postgres => "TIMESTAMPTZ",
            Self::Sqlite => "TEXT",
        }
    }

    /// `{now}` value expression: the current instant at statement time.
    const fn now(self) -> &'static str {
        match self {
            Self::Postgres => "now()",
            Self::Sqlite => "CURRENT_TIMESTAMP",
        }
    }

    /// `{blob}` column type: opaque binary bytes.
    const fn blob(self) -> &'static str {
        match self {
            Self::Postgres => "BYTEA",
            Self::Sqlite => "BLOB",
        }
    }

    /// `{pk_autoinc}` column clause: an auto-incrementing integer primary key.
    const fn pk_autoinc(self) -> &'static str {
        match self {
            Self::Postgres => "BIGSERIAL PRIMARY KEY",
            Self::Sqlite => "INTEGER PRIMARY KEY AUTOINCREMENT",
        }
    }
}

/// Render a portable token template to backend SQL for `dialect`.
///
/// A migration body is a dialect-neutral template carrying the closed token
/// vocabulary — `{prefix}` for the per-database table prefix and a small type
/// vocabulary (`{json}`, `{timestamptz}`, `{now}`, `{blob}`, `{pk_autoinc}`).
/// Each token resolves to its backend form per the definitive table in
/// `docs/design/scoped-migration.md`, so one template serves both backends.
///
/// Rendering happens at **apply time** in the runner, which knows its own
/// `dialect` and `prefix`; the stored template and its
/// [`Migration::checksum_for`] stay over the template, so a portable migration's
/// recorded identity is dialect-independent while the rendered SQL legitimately
/// differs. Any other `{...}` sequence is not a token and passes through
/// untouched — it is raw dialect SQL the author wrote deliberately.
#[must_use]
pub fn render(template: &str, dialect: Dialect, prefix: &str) -> String {
    template
        .replace("{prefix}", prefix)
        .replace("{json}", dialect.json())
        .replace("{timestamptz}", dialect.timestamptz())
        .replace("{now}", dialect.now())
        .replace("{blob}", dialect.blob())
        .replace("{pk_autoinc}", dialect.pk_autoinc())
}

/// A migration's SQL body.
///
/// The default is [`Portable`](MigrationBody::Portable): one dialect-neutral
/// token template with the same checksum on every backend. The
/// [`PerDialect`](MigrationBody::PerDialect) escape hatch carries one body per
/// dialect and is checksummed per the *selected* dialect, opting out of
/// dialect-independence explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MigrationBody {
    Portable(String),
    PerDialect { postgres: String, sqlite: String },
}

/// One ordered SQL migration inside a named bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    version: i64,
    description: String,
    body: MigrationBody,
}

impl Migration {
    /// A portable migration: one dialect-neutral token template, identical
    /// checksum on every backend. This is the default and the norm.
    pub fn new(
        version: i64,
        description: impl Into<String>,
        sql: impl Into<String>,
    ) -> Result<Self, MigrationError> {
        let migration = Self {
            version,
            description: description.into(),
            body: MigrationBody::Portable(sql.into()),
        };
        migration.validate()?;
        Ok(migration)
    }

    /// Escape hatch for SQL that genuinely cannot be expressed in the portable
    /// token vocabulary: one body per dialect.
    ///
    /// Such a migration **opts out of dialect-independence explicitly** — it is
    /// checksummed per the *selected* dialect's body (so its recorded identity
    /// legitimately differs across backends) and is otherwise treated like any
    /// other migration by [`plan`]. Portable [`Migration::new`] migrations
    /// remain the default; reach for this only when the tokens cannot express
    /// the statement.
    pub fn per_dialect(
        version: i64,
        description: impl Into<String>,
        postgres: impl Into<String>,
        sqlite: impl Into<String>,
    ) -> Result<Self, MigrationError> {
        let migration = Self {
            version,
            description: description.into(),
            body: MigrationBody::PerDialect {
                postgres: postgres.into(),
                sqlite: sqlite.into(),
            },
        };
        migration.validate()?;
        Ok(migration)
    }

    #[must_use]
    pub const fn version(&self) -> i64 {
        self.version
    }

    /// Human-readable, zero-padded version label (`V0001`).
    ///
    /// The stored [`version`](Self::version) stays an `i64` for ordering and the
    /// ledger primary key; this is purely the display form — Flyway-style, sorts
    /// the way it reads, and far more legible in a ledger row or a diagnostic
    /// than a bare `1`. It is recorded in the ledger's `description` (see
    /// [`ledger_description`](Self::ledger_description)), surfaced in error
    /// messages, and used as the leading field of the
    /// [`checksum_for`](Self::checksum_for) so reordering a recorded version
    /// fails closed.
    #[must_use]
    pub fn label(&self) -> String {
        version_label(self.version)
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The description as recorded in the ledger row: the readable
    /// [`label`](Self::label) followed by the human description (`"V0001 create
    /// the event store"`), so a ledger scan reads the version label without
    /// decoding the integer column.
    #[must_use]
    pub fn ledger_description(&self) -> String {
        format!("{} {}", self.label(), self.description)
    }

    /// The SQL body this migration applies on `dialect`. A portable migration
    /// returns the same template for every dialect; a [`Migration::per_dialect`]
    /// one returns the dialect-specific body.
    #[must_use]
    pub fn sql_for(&self, dialect: Dialect) -> &str {
        match &self.body {
            MigrationBody::Portable(sql) => sql,
            MigrationBody::PerDialect { postgres, sqlite } => match dialect {
                Dialect::Postgres => postgres,
                Dialect::Sqlite => sqlite,
            },
        }
    }

    /// Every distinct SQL body this migration carries: one for a portable
    /// migration, both dialect bodies for a [`Migration::per_dialect`] one. Used
    /// by the dialect-agnostic build-time independence lint.
    fn bodies(&self) -> Vec<&str> {
        match &self.body {
            MigrationBody::Portable(sql) => vec![sql.as_str()],
            MigrationBody::PerDialect { postgres, sqlite } => {
                vec![postgres.as_str(), sqlite.as_str()]
            }
        }
    }

    /// Recorded identity of the migration on `dialect`:
    /// `SHA-256(label_bytes || 0x00 || body_bytes)`.
    ///
    /// For a portable migration the body is the neutral template, so the hash is
    /// **dialect-independent**: the same migration verifies to the same checksum
    /// on Postgres and SQLite even though the SQL each backend ultimately applies
    /// may differ. For a [`Migration::per_dialect`] escape-hatch migration the
    /// body is the selected dialect's SQL, so its recorded identity legitimately
    /// differs across backends — that is the explicit cost of opting out of
    /// dialect-independence. The label is included so a reordered version (same
    /// body, different position) is detected as drift; the `0x00` separator keeps
    /// the label and body fields unambiguous.
    #[must_use]
    pub fn checksum_for(&self, dialect: Dialect) -> String {
        let label = self.label();
        let body = self.sql_for(dialect);
        let mut input = Vec::with_capacity(label.len() + 1 + body.len());
        input.extend_from_slice(label.as_bytes());
        input.push(0x00);
        input.extend_from_slice(body.as_bytes());
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
        // Every body a migration carries must be non-blank: a portable migration
        // has one, a per-dialect escape hatch has one per dialect (a blank
        // postgres or sqlite body is rejected).
        if self.bodies().iter().any(|body| body.trim().is_empty()) {
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
/// this with its own `dialect`, and applies the result with its own driver.
///
/// `dialect` selects which checksum a recorded migration is compared against: a
/// portable migration's checksum is identical for every dialect, so this only
/// matters for a [`Migration::per_dialect`] escape-hatch migration, whose
/// recorded identity is per dialect. Otherwise a per-dialect migration is
/// planned exactly like any other.
pub fn plan<'a>(
    bundle: &'a MigrationBundle,
    applied: &BTreeMap<i64, String>,
    dialect: Dialect,
) -> Result<Vec<&'a Migration>, MigrationError> {
    validate_applied_versions(bundle, applied)?;
    let mut pending = Vec::new();
    for migration in bundle.migrations() {
        match applied.get(&migration.version()) {
            Some(existing) => {
                let recomputed = migration.checksum_for(dialect);
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

/// Build-time validation across a whole set of bundles, intended to run in a
/// consumer's `#[test]` (the pre-push layer of the migration hook table).
///
/// It enforces the invariants that need the full set, not a single bundle:
///
/// - **unique, strictly-increasing versions within each bundle** — re-checks the
///   per-bundle invariant [`MigrationBundle::new`] already guarantees, so `lint`
///   is a single, sufficient entry point;
/// - **distinct bundle ids** across the set;
/// - **bundle independence** — a bundle's DDL may reference only tables it itself
///   creates, so no bundle silently couples to another (a cross-bundle reference
///   defeats the separable dimension the crate exists to provide).
///
/// The independence rule is reference-based: the lint collects the table names
/// each bundle `CREATE`s, then rejects any `FROM`/`JOIN`/`REFERENCES`/`ALTER`
/// naming a table outside that set.
pub fn lint(bundles: &[MigrationBundle]) -> Result<(), MigrationError> {
    let mut seen_ids = BTreeSet::new();
    for bundle in bundles {
        // Re-assert the per-bundle version invariants so `lint` is the one
        // function a consumer needs to call.
        bundle.validate()?;
        if !seen_ids.insert(bundle.bundle_id()) {
            return Err(MigrationError::DuplicateBundle(
                bundle.bundle_id().to_string(),
            ));
        }
        check_bundle_independence(bundle)?;
    }
    Ok(())
}

/// Reject a bundle that references any table it does not itself create. Tables
/// share the per-database `{prefix}`, so ownership is read from what the bundle
/// creates, not from the prefix: a bundle may reference only the tables created
/// by its own migrations.
fn check_bundle_independence(bundle: &MigrationBundle) -> Result<(), MigrationError> {
    let scans = bundle
        .migrations()
        .iter()
        .map(|migration| {
            // Scan every body a migration carries so a per-dialect escape hatch
            // is held to the same independence rule on both dialects; a portable
            // migration has a single body.
            let mut scan = TableScan::default();
            for body in migration.bodies() {
                let body_scan = scan_tables(body);
                scan.created.extend(body_scan.created);
                scan.referenced.extend(body_scan.referenced);
            }
            (migration.version(), scan)
        })
        .collect::<Vec<_>>();
    let created = scans
        .iter()
        .flat_map(|(_, scan)| &scan.created)
        .collect::<BTreeSet<_>>();
    for (version, scan) in &scans {
        for table in &scan.referenced {
            if !created.contains(table) {
                return Err(MigrationError::CrossBundleReference {
                    bundle_id: bundle.bundle_id().to_string(),
                    version: *version,
                    table: table.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Tables a single migration body creates and references, harvested by a
/// dependency-free SQL word scan. This is a build-time lint, never the apply
/// hot path, so a lightweight tokenizer is deliberate.
#[derive(Default)]
struct TableScan {
    created: Vec<String>,
    referenced: Vec<String>,
}

/// Scan one SQL body for the tables it `CREATE`s and the tables it references via
/// `FROM` / `JOIN` / `REFERENCES` / `ALTER TABLE`. Comments and string literals
/// are skipped; identifiers are normalized so the same table compares equal
/// regardless of case or quoting.
fn scan_tables(sql: &str) -> TableScan {
    let words = tokenize_words(sql);
    let mut scan = TableScan::default();
    let mut index = 0;
    while index < words.len() {
        match words[index].to_ascii_uppercase().as_str() {
            "CREATE" if next_is(&words, index + 1, "TABLE") => {
                let name = index + 2 + leading_count(&words, index + 2, &["IF", "NOT", "EXISTS"]);
                if let Some(table) = words.get(name) {
                    scan.created.push(normalize_table(table));
                }
                index = name + 1;
            }
            "ALTER" if next_is(&words, index + 1, "TABLE") => {
                let name = index + 2 + leading_count(&words, index + 2, &["IF", "EXISTS", "ONLY"]);
                if let Some(table) = words.get(name) {
                    scan.referenced.push(normalize_table(table));
                }
                index = name + 1;
            }
            "FROM" | "JOIN" | "REFERENCES" => {
                if let Some(table) = words.get(index + 1) {
                    scan.referenced.push(normalize_table(table));
                }
                index += 2;
            }
            _ => index += 1,
        }
    }
    scan
}

fn next_is(words: &[String], index: usize, keyword: &str) -> bool {
    words
        .get(index)
        .is_some_and(|word| word.eq_ignore_ascii_case(keyword))
}

/// Count leading words at `index` that match one of `keywords` (case-insensitive),
/// so noise like `IF NOT EXISTS` can be stepped over to reach the table name.
fn leading_count(words: &[String], index: usize, keywords: &[&str]) -> usize {
    let mut count = 0;
    while let Some(word) = words.get(index + count) {
        if keywords
            .iter()
            .any(|keyword| word.eq_ignore_ascii_case(keyword))
        {
            count += 1;
        } else {
            break;
        }
    }
    count
}

/// Normalize a table token for comparison: drop any identifier quoting and
/// lower-case it (SQL identifiers are case-insensitive). `{prefix}`-bearing
/// names compare verbatim, which is what keeps templated bundles comparable.
fn normalize_table(raw: &str) -> String {
    raw.trim_matches('"').to_ascii_lowercase()
}

/// Split a SQL body into bare word runs, skipping `--`/`/* */` comments and
/// `'...'` string literals and unwrapping `"..."` quoted identifiers. A word run
/// is the identifier alphabet plus `{` `}` `.` so a templated name like
/// `{prefix}_sessions` or a qualified `schema.table` stays a single token.
fn tokenize_words(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut words = Vec::new();
    let mut index = 0;
    while index < len {
        let byte = bytes[index];
        if byte == b'-' && bytes.get(index + 1) == Some(&b'-') {
            index += 2;
            while index < len && bytes[index] != b'\n' {
                index += 1;
            }
        } else if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            while index + 1 < len && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            index = (index + 2).min(len);
        } else if byte == b'\'' {
            index += 1;
            while index < len {
                if bytes[index] == b'\'' {
                    if bytes.get(index + 1) == Some(&b'\'') {
                        index += 2;
                        continue;
                    }
                    index += 1;
                    break;
                }
                index += 1;
            }
        } else if byte == b'"' {
            index += 1;
            let start = index;
            while index < len && bytes[index] != b'"' {
                index += 1;
            }
            if start < index {
                words.push(sql[start..index].to_string());
            }
            index += 1;
        } else if is_word_byte(byte) {
            let start = index;
            while index < len && is_word_byte(bytes[index]) {
                index += 1;
            }
            words.push(sql[start..index].to_string());
        } else {
            index += 1;
        }
    }
    words
}

const fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'{' | b'}' | b'.')
}

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("invalid SQL identifier prefix '{0}'")]
    InvalidSqlIdentifier(String),
    #[error("invalid migration bundle id '{0}'")]
    InvalidBundleId(String),
    #[error("invalid migration {version}: {reason}")]
    InvalidMigration { version: i64, reason: &'static str },
    #[error("bundle '{bundle_id}' contains duplicate migration version V{version:04}")]
    DuplicateMigrationVersion { bundle_id: String, version: i64 },
    #[error(
        "bundle '{bundle_id}' migrations are not strictly increasing: V{previous:04} then V{current:04}"
    )]
    InvalidMigrationOrder {
        bundle_id: String,
        previous: i64,
        current: i64,
    },
    #[error("duplicate migration bundle '{0}'")]
    DuplicateBundle(String),
    #[error(
        "bundle '{bundle_id}' migration V{version:04} references table '{table}' that no migration in the bundle creates"
    )]
    CrossBundleReference {
        bundle_id: String,
        version: i64,
        table: String,
    },
    #[error("bundle '{bundle_id}' has unknown applied version V{version:04}")]
    UnknownAppliedVersion { bundle_id: String, version: i64 },
    #[error(
        "bundle '{bundle_id}' migration V{version:04} checksum mismatch: expected {expected}, actual {actual}"
    )]
    ChecksumMismatch {
        bundle_id: String,
        version: i64,
        expected: String,
        actual: String,
    },
    #[error(
        "ledger '{ledger_table}' is stamped version {found}, but this runner expects {expected}"
    )]
    LedgerVersionMismatch {
        ledger_table: String,
        expected: i64,
        found: i64,
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

/// Render a migration version as its zero-padded display label (`V0001`).
///
/// The integer remains the source of truth for ordering and the ledger primary
/// key; this is only the readable form used in the ledger `description` and in
/// diagnostics. A free function so callers holding only an `i64` (the ledger,
/// error formatting) get the same label as [`Migration::label`].
#[must_use]
pub fn version_label(version: i64) -> String {
    format!("V{version:04}")
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
        assert_eq!(
            migration.checksum_for(Dialect::Postgres),
            sha256_hex(&expected)
        );
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
        // The checksum of a portable migration is taken over the neutral
        // template, so it verifies identically on every backend.
        let template = "CREATE TABLE {prefix}_event (id {pk_autoinc}, payload {json})";
        let migration = Migration::new(7, "events", template).unwrap();
        assert_eq!(
            migration.checksum_for(Dialect::Postgres),
            migration.checksum_for(Dialect::Sqlite)
        );
    }

    #[test]
    fn per_dialect_checksum_differs_across_dialects() {
        // The escape hatch opts out of dialect-independence: each dialect's body
        // is hashed, so the recorded identity legitimately differs per backend.
        let migration = Migration::per_dialect(
            3,
            "json column",
            "ALTER TABLE t ADD COLUMN payload JSONB",
            "ALTER TABLE t ADD COLUMN payload TEXT",
        )
        .unwrap();
        let pg = migration.checksum_for(Dialect::Postgres);
        let sqlite = migration.checksum_for(Dialect::Sqlite);
        assert_ne!(pg, sqlite);

        // Each side is exactly `SHA-256(label || 0x00 || that dialect's body)`.
        let mut expected_pg = Vec::new();
        expected_pg.extend_from_slice(b"V0003");
        expected_pg.push(0x00);
        expected_pg.extend_from_slice(b"ALTER TABLE t ADD COLUMN payload JSONB");
        assert_eq!(pg, sha256_hex(&expected_pg));
    }

    #[test]
    fn per_dialect_sql_selects_the_dialect_body() {
        let migration = Migration::per_dialect(
            1,
            "now default",
            "INSERT INTO t (at) VALUES (now())",
            "INSERT INTO t (at) VALUES (CURRENT_TIMESTAMP)",
        )
        .unwrap();
        assert_eq!(
            migration.sql_for(Dialect::Postgres),
            "INSERT INTO t (at) VALUES (now())"
        );
        assert_eq!(
            migration.sql_for(Dialect::Sqlite),
            "INSERT INTO t (at) VALUES (CURRENT_TIMESTAMP)"
        );
    }

    #[test]
    fn per_dialect_rejects_a_blank_body() {
        // A per-dialect escape hatch must carry a real body for *each* dialect;
        // a blank sqlite side fails closed at construction.
        let err =
            Migration::per_dialect(1, "broken", "CREATE TABLE a (id TEXT)", "   ").unwrap_err();
        assert!(matches!(
            err,
            MigrationError::InvalidMigration {
                version: 1,
                reason: "sql must not be blank"
            }
        ));
    }

    #[test]
    fn reordering_a_version_changes_the_checksum() {
        // Same template, different version label ⇒ different identity, so a
        // reordered recorded version fails closed in `plan()`.
        let template = "CREATE TABLE a (id TEXT)";
        let at_one = Migration::new(1, "x", template).unwrap();
        let at_two = Migration::new(2, "x", template).unwrap();
        assert_ne!(
            at_one.checksum_for(Dialect::Postgres),
            at_two.checksum_for(Dialect::Postgres)
        );
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
        let pending = plan(&b, &BTreeMap::new(), Dialect::Postgres).unwrap();
        assert_eq!(
            pending.iter().map(|m| m.version()).collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn skips_already_applied_with_matching_checksum() {
        let b = bundle();
        let mut applied = BTreeMap::new();
        applied.insert(1, b.migrations()[0].checksum_for(Dialect::Postgres));
        let pending = plan(&b, &applied, Dialect::Postgres).unwrap();
        assert_eq!(pending.iter().map(|m| m.version()).collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn rejects_checksum_drift() {
        let b = bundle();
        let mut applied = BTreeMap::new();
        applied.insert(1, "deadbeef".to_string());
        let err = plan(&b, &applied, Dialect::Postgres).unwrap_err();
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
            plan(&b, &applied, Dialect::Postgres).unwrap_err(),
            MigrationError::UnknownAppliedVersion { version: 99, .. }
        ));
    }

    #[test]
    fn per_dialect_is_planned_then_skipped_like_any_migration() {
        // Acceptance criterion: a per_dialect migration round-trips and is
        // skipped on re-run, recorded under the running dialect's checksum.
        let b = MigrationBundle::new(
            "runtime.core",
            vec![
                Migration::new(1, "first", "CREATE TABLE a (id TEXT)").unwrap(),
                Migration::per_dialect(
                    2,
                    "json column",
                    "ALTER TABLE a ADD COLUMN payload JSONB",
                    "ALTER TABLE a ADD COLUMN payload TEXT",
                )
                .unwrap(),
            ],
        )
        .unwrap();

        // Empty ledger ⇒ both pending, the escape hatch among them.
        let pending = plan(&b, &BTreeMap::new(), Dialect::Sqlite).unwrap();
        assert_eq!(
            pending.iter().map(|m| m.version()).collect::<Vec<_>>(),
            [1, 2]
        );

        // Record both under the SQLite-dialect checksums; the re-run skips them.
        let mut applied = BTreeMap::new();
        for migration in b.migrations() {
            applied.insert(migration.version(), migration.checksum_for(Dialect::Sqlite));
        }
        assert!(plan(&b, &applied, Dialect::Sqlite).unwrap().is_empty());

        // The same ledger fails closed under Postgres: the escape hatch's
        // recorded identity is per dialect, so the SQLite checksum is drift there.
        let err = plan(&b, &applied, Dialect::Postgres).unwrap_err();
        assert!(matches!(
            err,
            MigrationError::ChecksumMismatch { version: 2, .. }
        ));
    }

    #[test]
    fn lint_accepts_independent_bundles() {
        let runtime = MigrationBundle::new(
            "runtime.core",
            vec![
                Migration::new(1, "users", "CREATE TABLE {prefix}_users (id {pk_autoinc})")
                    .unwrap(),
                Migration::new(
                    2,
                    "sessions",
                    "CREATE TABLE {prefix}_sessions (id {pk_autoinc}, \
                     user_id BIGINT REFERENCES {prefix}_users (id))",
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let gateway = MigrationBundle::new(
            "gateway.audit",
            vec![
                Migration::new(
                    1,
                    "log",
                    "CREATE TABLE {prefix}_audit_log (id {pk_autoinc})",
                )
                .unwrap(),
            ],
        )
        .unwrap();
        assert!(lint(&[runtime, gateway]).is_ok());
    }

    #[test]
    fn lint_allows_intra_bundle_alter() {
        let bundle = MigrationBundle::new(
            "runtime.core",
            vec![
                Migration::new(1, "create", "CREATE TABLE {prefix}_jobs (id {pk_autoinc})")
                    .unwrap(),
                Migration::new(
                    2,
                    "extend",
                    "ALTER TABLE {prefix}_jobs ADD COLUMN note TEXT",
                )
                .unwrap(),
            ],
        )
        .unwrap();
        assert!(lint(&[bundle]).is_ok());
    }

    #[test]
    fn lint_rejects_duplicate_bundle_id() {
        let one = MigrationBundle::new(
            "runtime.core",
            vec![Migration::new(1, "a", "CREATE TABLE {prefix}_a (id TEXT)").unwrap()],
        )
        .unwrap();
        let two = MigrationBundle::new(
            "runtime.core",
            vec![Migration::new(1, "b", "CREATE TABLE {prefix}_b (id TEXT)").unwrap()],
        )
        .unwrap();
        assert!(matches!(
            lint(&[one, two]).unwrap_err(),
            MigrationError::DuplicateBundle(id) if id == "runtime.core"
        ));
    }

    #[test]
    fn lint_rejects_cross_bundle_foreign_key() {
        let owner = MigrationBundle::new(
            "iam.identity",
            vec![
                Migration::new(1, "users", "CREATE TABLE {prefix}_users (id {pk_autoinc})")
                    .unwrap(),
            ],
        )
        .unwrap();
        // authz reaches into identity's table via a foreign key — exactly the
        // silent coupling the reference rule forbids.
        let coupled = MigrationBundle::new(
            "iam.authz",
            vec![
                Migration::new(
                    1,
                    "grants",
                    "CREATE TABLE {prefix}_grants (id {pk_autoinc}, \
                 user_id BIGINT REFERENCES {prefix}_users (id))",
                )
                .unwrap(),
            ],
        )
        .unwrap();
        assert!(matches!(
            lint(&[owner, coupled]).unwrap_err(),
            MigrationError::CrossBundleReference { bundle_id, version: 1, table }
                if bundle_id == "iam.authz" && table == "{prefix}_users"
        ));
    }

    #[test]
    fn lint_rejects_cross_bundle_select() {
        let bundle = MigrationBundle::new(
            "runtime.core",
            vec![
                Migration::new(
                    1,
                    "seed",
                    "CREATE TABLE {prefix}_a (id TEXT); \
                 INSERT INTO {prefix}_a SELECT id FROM {prefix}_elsewhere",
                )
                .unwrap(),
            ],
        )
        .unwrap();
        assert!(matches!(
            lint(&[bundle]).unwrap_err(),
            MigrationError::CrossBundleReference { table, .. } if table == "{prefix}_elsewhere"
        ));
    }

    #[test]
    fn version_renders_as_zero_padded_label() {
        let m = Migration::new(1, "first", "CREATE TABLE a (id TEXT)").unwrap();
        assert_eq!(m.label(), "V0001");
        assert_eq!(version_label(42), "V0042");
        // Wider versions keep all their digits rather than truncating.
        assert_eq!(version_label(12_345), "V12345");
    }

    #[test]
    fn ledger_description_prefixes_the_label() {
        let m = Migration::new(7, "create the event store", "CREATE TABLE a (id TEXT)").unwrap();
        assert_eq!(m.ledger_description(), "V0007 create the event store");
    }

    #[test]
    fn diagnostics_use_the_readable_label() {
        let err = MigrationBundle::new(
            "runtime.core",
            vec![
                Migration::new(3, "a", "CREATE TABLE a (id TEXT)").unwrap(),
                Migration::new(3, "b", "CREATE TABLE b (id TEXT)").unwrap(),
            ],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("V0003"),
            "diagnostic should label the version: {err}"
        );
    }

    #[test]
    fn ledger_version_check_accepts_expected() {
        assert!(check_ledger_version("awaken_schema_migrations", LEDGER_VERSION).is_ok());
    }

    #[test]
    fn ledger_version_check_fails_closed_on_mismatch() {
        let err = check_ledger_version("awaken_schema_migrations", LEDGER_VERSION + 1).unwrap_err();
        assert!(matches!(
            err,
            MigrationError::LedgerVersionMismatch {
                expected,
                found,
                ledger_table,
            } if expected == LEDGER_VERSION
                && found == LEDGER_VERSION + 1
                && ledger_table == "awaken_schema_migrations"
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

    #[test]
    fn rejects_a_backwards_version_step() {
        // Convergence acceptance, distinct from the duplicate-version case: a
        // bundle whose versions step backwards (3 then 1) is not strictly
        // increasing and fails closed, the ordering invariant the bundle
        // re-asserts for the whole set.
        let err = MigrationBundle::new(
            "runtime.core",
            vec![
                Migration::new(3, "a", "CREATE TABLE a (id TEXT)").unwrap(),
                Migration::new(1, "b", "CREATE TABLE b (id TEXT)").unwrap(),
            ],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            MigrationError::InvalidMigrationOrder {
                previous: 3,
                current: 1,
                ..
            }
        ));
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    const TEMPLATE: &str = "CREATE TABLE {prefix}_event (\n    \
        id {pk_autoinc},\n    \
        payload {json} NOT NULL,\n    \
        body {blob},\n    \
        created_at {timestamptz} NOT NULL DEFAULT {now}\n)";

    #[test]
    fn renders_every_token_for_postgres() {
        let sql = render(TEMPLATE, Dialect::Postgres, "awaken");
        assert!(sql.contains("CREATE TABLE awaken_event ("));
        assert!(sql.contains("id BIGSERIAL PRIMARY KEY,"));
        assert!(sql.contains("payload JSONB NOT NULL,"));
        assert!(sql.contains("body BYTEA,"));
        assert!(sql.contains("created_at TIMESTAMPTZ NOT NULL DEFAULT now()"));
        // No token survives rendering.
        assert!(!sql.contains('{'));
    }

    #[test]
    fn renders_every_token_for_sqlite() {
        let sql = render(TEMPLATE, Dialect::Sqlite, "awaken");
        assert!(sql.contains("CREATE TABLE awaken_event ("));
        assert!(sql.contains("id INTEGER PRIMARY KEY AUTOINCREMENT,"));
        assert!(sql.contains("payload TEXT NOT NULL,"));
        assert!(sql.contains("body BLOB,"));
        assert!(sql.contains("created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP"));
        assert!(!sql.contains('{'));
    }

    #[test]
    fn prefix_token_uses_runner_prefix() {
        assert!(
            render("SELECT * FROM {prefix}_t", Dialect::Sqlite, "gateway").contains("gateway_t")
        );
    }

    #[test]
    fn unknown_token_passes_through_untouched() {
        // Anything outside the closed vocabulary is raw SQL the author wrote
        // deliberately, not a token, so rendering leaves it exactly as-is.
        let out = render("CREATE TABLE t (x {unknown})", Dialect::Postgres, "awaken");
        assert_eq!(out, "CREATE TABLE t (x {unknown})");
    }

    #[test]
    fn checksum_is_dialect_independent_while_rendered_sql_differs() {
        let migration = Migration::new(1, "create event", TEMPLATE).unwrap();
        // The recorded identity is the template's checksum — identical regardless
        // of which backend later renders it.
        assert_eq!(
            migration.checksum_for(Dialect::Postgres),
            migration.checksum_for(Dialect::Sqlite),
        );
        // Yet the SQL each backend ultimately applies differs by design.
        let pg = render(
            migration.sql_for(Dialect::Postgres),
            Dialect::Postgres,
            "awaken",
        );
        let lite = render(
            migration.sql_for(Dialect::Sqlite),
            Dialect::Sqlite,
            "awaken",
        );
        assert_ne!(pg, lite, "rendered SQL must differ per backend");
    }
}
