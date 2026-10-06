//! The migration ledger's own bookkeeping: its table names, generation stamp,
//! bootstrap SQL and the fail-closed decisions over an observed database state.

use crate::{Dialect, MigrationError, sql_identifier};

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

/// The two bookkeeping tables owned by one migration namespace.
///
/// This is the single source of truth for ledger names, deterministic bootstrap
/// SQL, and the fail-closed decision over an observed database state. Backend
/// shells only provide locking, probing, and statement execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerSchema {
    ledger_table: String,
    meta_table: String,
}

/// The only valid outcomes of probing a ledger namespace under its bootstrap
/// lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerBootstrapAction {
    /// Neither bookkeeping table exists; create the stamped generation.
    Create,
    /// Both tables exist; validate the stamped generation without DDL.
    Validate,
}

impl LedgerSchema {
    /// Build the authoritative bookkeeping schema for a validated SQL prefix.
    pub fn with_prefix(prefix: &str) -> Result<Self, MigrationError> {
        let prefix = sql_identifier(prefix)?;
        Ok(Self {
            ledger_table: format!("{prefix}_schema_migrations"),
            meta_table: format!("{prefix}_schema_migrations_meta"),
        })
    }

    #[must_use]
    pub fn ledger_table(&self) -> &str {
        &self.ledger_table
    }

    #[must_use]
    pub fn meta_table(&self) -> &str {
        &self.meta_table
    }

    /// Decide bootstrap from an atomic presence probe taken under the backend's
    /// namespace-level bootstrap lock.
    pub fn bootstrap_action(
        &self,
        ledger_exists: bool,
        meta_exists: bool,
    ) -> Result<LedgerBootstrapAction, MigrationError> {
        match (ledger_exists, meta_exists) {
            (false, false) => Ok(LedgerBootstrapAction::Create),
            (true, true) => Ok(LedgerBootstrapAction::Validate),
            _ => Err(MigrationError::IncompleteLedger {
                ledger_table: self.ledger_table.clone(),
                meta_table: self.meta_table.clone(),
                ledger_exists,
                meta_exists,
            }),
        }
    }

    /// Require the complete bookkeeping generation for a read-only verifier.
    ///
    /// Unlike bootstrap, verification must never turn an absent namespace into
    /// a create decision: applications report the missing ledger and leave DDL
    /// exclusively to the migration command.
    pub fn verify_presence(
        &self,
        ledger_exists: bool,
        meta_exists: bool,
    ) -> Result<(), MigrationError> {
        match (ledger_exists, meta_exists) {
            (true, true) => Ok(()),
            (false, false) => Err(MigrationError::MissingLedger {
                ledger_table: self.ledger_table.clone(),
                meta_table: self.meta_table.clone(),
            }),
            _ => Err(MigrationError::IncompleteLedger {
                ledger_table: self.ledger_table.clone(),
                meta_table: self.meta_table.clone(),
                ledger_exists,
                meta_exists,
            }),
        }
    }

    /// Unconditional, generation-stamped bootstrap statements for a fresh
    /// namespace. Callers execute these only after [`Self::bootstrap_action`]
    /// returns [`LedgerBootstrapAction::Create`] while holding the bootstrap
    /// lock.
    #[must_use]
    pub fn create_statements(&self, dialect: Dialect) -> [String; 3] {
        let (version_type, applied_at_type, applied_at_default) = match dialect {
            Dialect::Postgres => ("BIGINT", "TIMESTAMPTZ", "now()"),
            Dialect::Sqlite => ("INTEGER", "TEXT", "(datetime('now'))"),
        };
        [
            format!(
                "CREATE TABLE {} (\
                 bundle_id TEXT NOT NULL, \
                 version {version_type} NOT NULL, \
                 checksum TEXT NOT NULL, \
                 description TEXT NOT NULL, \
                 applied_at {applied_at_type} NOT NULL DEFAULT {applied_at_default}, \
                 applied_by TEXT NOT NULL, \
                 PRIMARY KEY (bundle_id, version))",
                self.ledger_table
            ),
            format!(
                "CREATE TABLE {} (ledger_version {version_type} NOT NULL)",
                self.meta_table
            ),
            format!(
                "INSERT INTO {} (ledger_version) VALUES ({LEDGER_VERSION})",
                self.meta_table
            ),
        ]
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_bootstrap_decision_is_total_and_fail_closed() {
        // Cause/effect decision table over (ledger table, meta table):
        // R1 (absent, absent) -> Create; R2 (present, present) -> Validate;
        // R3/R4 exactly one present -> IncompleteLedger. No observed database
        // state can silently fall through to conditional DDL.
        let schema = LedgerSchema::with_prefix("runtime").unwrap();
        assert_eq!(
            schema.bootstrap_action(false, false).unwrap(),
            LedgerBootstrapAction::Create
        );
        assert_eq!(
            schema.bootstrap_action(true, true).unwrap(),
            LedgerBootstrapAction::Validate
        );
        for (ledger_exists, meta_exists) in [(true, false), (false, true)] {
            assert!(matches!(
                schema
                    .bootstrap_action(ledger_exists, meta_exists)
                    .unwrap_err(),
                MigrationError::IncompleteLedger {
                    ledger_exists: found_ledger,
                    meta_exists: found_meta,
                    ..
                } if found_ledger == ledger_exists && found_meta == meta_exists
            ));
        }
    }

    #[test]
    fn ledger_verification_never_bootstraps_an_absent_namespace() {
        // The application-side decision table is intentionally distinct from
        // bootstrap: absent/absent is a typed operational prerequisite, both
        // present is readable, and either split generation fails closed.
        let schema = LedgerSchema::with_prefix("runtime").unwrap();
        assert!(matches!(
            schema.verify_presence(false, false),
            Err(MigrationError::MissingLedger { .. })
        ));
        assert!(schema.verify_presence(true, true).is_ok());
        for (ledger_exists, meta_exists) in [(true, false), (false, true)] {
            assert!(matches!(
                schema.verify_presence(ledger_exists, meta_exists),
                Err(MigrationError::IncompleteLedger {
                    ledger_exists: found_ledger,
                    meta_exists: found_meta,
                    ..
                }) if found_ledger == ledger_exists && found_meta == meta_exists
            ));
        }
    }

    #[test]
    fn ledger_bootstrap_sql_is_unconditional_and_generation_stamped() {
        // Causes: fresh namespace plus either supported dialect. Effects: three
        // fixed commands (ledger, meta, v1 stamp), with no state-dependent SQL.
        // This covers the bootstrap half of the R1 decision-table rule above.
        let schema = LedgerSchema::with_prefix("runtime").unwrap();
        for dialect in [Dialect::Postgres, Dialect::Sqlite] {
            let statements = schema.create_statements(dialect);
            assert_eq!(statements.len(), 3);
            let sql = statements.join(";");
            assert!(!sql.to_ascii_uppercase().contains("IF EXISTS"));
            assert!(!sql.to_ascii_uppercase().contains("IF NOT EXISTS"));
            assert!(!sql.to_ascii_uppercase().contains("OR REPLACE"));
            assert!(sql.contains("ledger_version) VALUES (1)"));
        }
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
}
