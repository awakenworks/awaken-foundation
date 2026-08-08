use super::*;

fn published(version: i64, description: &str, sql: &str) -> Migration {
    let label = version_label(version);
    let mut input = Vec::with_capacity(label.len() + 1 + sql.len());
    input.extend_from_slice(label.as_bytes());
    input.push(0);
    input.extend_from_slice(sql.as_bytes());
    Migration::published_legacy(version, description, sql, sha256_hex(&input)).unwrap()
}

#[test]
fn migration_and_bundle_validation_covers_each_input_class() {
    // Cause/effect decision table for constructor validation:
    // R1 version <= 0 -> InvalidMigration(version); R2 blank description ->
    // InvalidMigration(description); R3 blank body -> InvalidMigration(sql);
    // R4 all fields valid -> accessors preserve the accepted values. The three
    // invalid causes are mutually exclusive here so each diagnostic is pinned.
    for version in [0, -1] {
        assert!(matches!(
            Migration::new(version, "x", "CREATE TABLE a (id TEXT)").unwrap_err(),
            MigrationError::InvalidMigration {
                reason: "version must be positive",
                ..
            }
        ));
    }
    assert!(matches!(
        Migration::new(1, "   ", "CREATE TABLE a (id TEXT)").unwrap_err(),
        MigrationError::InvalidMigration {
            version: 1,
            reason: "description must not be blank"
        }
    ));
    assert!(matches!(
        Migration::new(1, "x", "   ").unwrap_err(),
        MigrationError::InvalidMigration {
            version: 1,
            reason: "sql must not be blank"
        }
    ));

    let migration = Migration::new(1, "create store", "CREATE TABLE a (id TEXT)").unwrap();
    assert_eq!(migration.description(), "create store");
    let bundle = MigrationBundle::new("runtime.core", vec![migration]).unwrap();
    assert_eq!(bundle.bundle_id(), "runtime.core");
    assert_eq!(bundle.migrations()[0].version(), 1);
}

#[test]
fn identifier_validation_covers_each_grammar_class() {
    // Cause/effect decision table over identifier grammar:
    // R1 allowed leading character + allowed tail -> return the identifier;
    // R2 empty, R3 forbidden leading character, or R4 forbidden tail byte ->
    // the corresponding typed error retaining the rejected input. Bundle ids
    // additionally allow `_-.` after their first alphanumeric character.
    assert_eq!(sql_identifier("awaken_v2").unwrap(), "awaken_v2");
    for bad in ["", "1leading", "has space", "dash-no"] {
        assert!(matches!(
            sql_identifier(bad).unwrap_err(),
            MigrationError::InvalidSqlIdentifier(value) if value == bad
        ));
    }

    MigrationBundle::new(
        "runtime.core-v2",
        vec![Migration::new(1, "x", "CREATE TABLE a (id TEXT)").unwrap()],
    )
    .unwrap();
    for bad in ["", "-leading", "has space", "tab\tname"] {
        assert!(matches!(
            MigrationBundle::new(
                bad,
                vec![Migration::new(1, "x", "CREATE TABLE a (id TEXT)").unwrap()],
            )
            .unwrap_err(),
            MigrationError::InvalidBundleId(value) if value == bad
        ));
    }
}

#[test]
fn independence_lint_covers_tokenizer_exclusions_and_legacy_keywords() {
    // migration-allow-conditional: the conditional strings below are pinned
    // published-legacy test fixtures, never newly authored migration bodies.
    // Cause/effect decision table for tokenized ownership references:
    // R1 table words in comments/string literals -> ignored; R2 a quoted
    // identifier -> normalized to its created bare identifier; R3 legacy
    // IF [NOT] EXISTS noise -> skipped while resolving CREATE/ALTER ownership.
    // Every rule yields one self-contained bundle and no cross-bundle error.
    let non_executable = MigrationBundle::new(
        "runtime.comments",
        vec![
            Migration::new(
                1,
                "seed",
                "CREATE TABLE {prefix}_a (id TEXT, note TEXT); \
             -- FROM {prefix}_ghost_line\n\
             /* FROM {prefix}_ghost_block */ \
             INSERT INTO {prefix}_a VALUES ('1', 'FROM {prefix}_ghost_string')",
            )
            .unwrap(),
        ],
    )
    .unwrap();
    assert!(lint(&[non_executable]).is_ok());

    let quoted = MigrationBundle::new(
        "runtime.quoted",
        vec![
            Migration::new(
                1,
                "quoted self-reference",
                "CREATE TABLE awaken_users (id TEXT); \
             INSERT INTO awaken_users SELECT id FROM \"AWAKEN_USERS\"",
            )
            .unwrap(),
        ],
    )
    .unwrap();
    assert!(lint(&[quoted]).is_ok());

    let conditional = MigrationBundle::new(
        "runtime.legacy-keywords",
        vec![
            published(
                1,
                "legacy create",
                "CREATE TABLE IF NOT EXISTS {prefix}_jobs (id TEXT)",
            ),
            published(
                2,
                "legacy alter",
                "ALTER TABLE IF EXISTS {prefix}_jobs ADD COLUMN note TEXT",
            ),
        ],
    )
    .unwrap();
    assert!(lint(&[conditional]).is_ok());
}

#[test]
fn every_error_variant_formats_its_distinguishing_context() {
    // Cause/effect coverage rationale: each MigrationError variant is a
    // separate terminal outcome. Constructing all variants and checking their
    // distinguishing fields keeps the public Display taxonomy exhaustive; a
    // new variant makes this table intentionally non-exhaustive at review time.
    let cases: Vec<(MigrationError, &[&str])> = vec![
        (
            MigrationError::InvalidSqlIdentifier("1bad".into()),
            &["prefix", "1bad"],
        ),
        (
            MigrationError::InvalidBundleId("-bad".into()),
            &["bundle id", "-bad"],
        ),
        (
            MigrationError::InvalidMigration {
                version: 2,
                reason: "version must be positive",
            },
            &["invalid migration 2", "version must be positive"],
        ),
        (
            MigrationError::ConditionalSql {
                version: 3,
                clause: "IF EXISTS",
            },
            &["0003", "IF EXISTS"],
        ),
        (
            MigrationError::PublishedLegacyChecksumMismatch {
                version: 4,
                body_kind: "sqlite",
                expected: "aaaa".into(),
                actual: "bbbb".into(),
            },
            &["0004", "sqlite", "aaaa", "bbbb"],
        ),
        (
            MigrationError::DuplicateMigrationVersion {
                bundle_id: "runtime.core".into(),
                version: 5,
            },
            &["runtime.core", "0005"],
        ),
        (
            MigrationError::InvalidMigrationOrder {
                bundle_id: "runtime.core".into(),
                previous: 5,
                current: 1,
            },
            &["0005", "0001"],
        ),
        (
            MigrationError::DuplicateBundle("runtime.core".into()),
            &["duplicate", "runtime.core"],
        ),
        (
            MigrationError::CrossBundleReference {
                bundle_id: "iam.authz".into(),
                version: 1,
                table: "users".into(),
            },
            &["iam.authz", "0001", "users"],
        ),
        (
            MigrationError::UnknownAppliedVersion {
                bundle_id: "runtime.core".into(),
                version: 9,
            },
            &["runtime.core", "0009"],
        ),
        (
            MigrationError::ChecksumMismatch {
                bundle_id: "runtime.core".into(),
                version: 1,
                expected: "aaaa".into(),
                actual: "bbbb".into(),
            },
            &["checksum mismatch", "aaaa", "bbbb"],
        ),
        (
            MigrationError::PendingMigration {
                bundle_id: "runtime.core".into(),
                version: 2,
            },
            &["runtime.core", "0002", "pending"],
        ),
        (
            MigrationError::LedgerVersionMismatch {
                ledger_table: "awaken_schema_migrations".into(),
                expected: 1,
                found: 2,
            },
            &["awaken_schema_migrations", "1", "2"],
        ),
        (
            MigrationError::LedgerMetadataRowCount {
                meta_table: "awaken_schema_migrations_meta".into(),
                found: 3,
            },
            &["awaken_schema_migrations_meta", "3"],
        ),
        (
            MigrationError::IncompleteLedger {
                ledger_table: "ledger".into(),
                meta_table: "meta".into(),
                ledger_exists: true,
                meta_exists: false,
            },
            &["ledger", "meta", "true", "false"],
        ),
        (
            MigrationError::Backend {
                operation: "sqlite_migration_apply",
                message: "syntax error".into(),
            },
            &["sqlite_migration_apply", "syntax error"],
        ),
    ];

    for (error, fragments) in cases {
        let rendered = error.to_string();
        for fragment in fragments {
            assert!(
                rendered.contains(fragment),
                "`{rendered}` should contain `{fragment}`"
            );
        }
    }
}
