# SQLite driver and migration compatibility tests

The driver graph must use one `libsqlite3-sys` with patched bundled SQLite for
[RUSTSEC-2026-0459](https://rustsec.org/advisories/RUSTSEC-2026-0459.html).
The SQLx dependency is an immutable upstream Git revision, not a registry release
claim. The synchronous runner retains a compatible rusqlite range; the standalone
lockfile must select a patched driver. A downstream lockfile is a separate claim.

## Compiler and source graph

[`check-msrv.sh`](../../scripts/ci/check-msrv.sh) checks the other base crates at
Rust 1.88 and both migration crates at their declared Rust 1.94 floor, with all
targets and features. A newer developer compiler does not substitute for either
check. Both backend shells must resolve SQLx from the same Git URL and full
revision; the lockfile must contain one SQLite native library.

`AssertSqlSafe` is limited to audited SQL sources: explicit migration code
rendered with the validated prefix, or statements generated from `LedgerSchema`.
Record values and bundle identifiers remain bind parameters. Test SQL uses
literals or the test's own validated namespace. Static SQL needs no assertion.

## Required behavior

| Boundary | Positive case | Refusal or regression case |
| --- | --- | --- |
| SQLite cross-driver ledger | SQLx and rusqlite accept the same tables, version stamp and checksums | Drift, unknown versions, incomplete metadata and a changed applied body refuse |
| Multi-statement bodies | Every statement in one explicit migration is applied | A later SQL failure or a refused receipt rolls back the body and its receipt |
| Single applier | Independent SQLite handles and PostgreSQL pools produce one applied bundle | Concurrent startup cannot record a second application or strand a lock |
| Read-only verification | An already-applied bundle verifies | Missing or pending schema never causes DDL through verification |
| Data binding | Descriptions, bundle ids and application values remain parameters | Dynamic identifiers must originate in the validated ledger schema |
| Compiler floors | Both declared groups compile at their own minimum version | An accidental new dependency floor fails the corresponding check |
| Repository privacy | Only portable source, contracts and relative references are tracked | Bytecode, local paths, private environment values and execution output stay outside Git |

SQLite tests are in [`sqlite.rs`](../../crates/awaken-scoped-migration/src/sqlite.rs)
and the [synchronous runner](../../crates/awaken-scoped-migration-sqlite/src/lib.rs).
PostgreSQL tests cover [read-only verification](../../crates/awaken-scoped-migration/src/postgres.rs)
and [transaction behavior](../../crates/awaken-scoped-migration/src/postgres/transaction_tests.rs).
PostgreSQL qualification requires an explicitly configured, isolated
`AWAKEN_TEST_POSTGRES_URL`; an unconfigured test's early return is not a native
PostgreSQL result. Runtime environment values and raw results are not committed.

Run the normal `pnpm check`, the explicit compiler-floor checks and the configured
PostgreSQL tests before publishing. Preserve the original security refusal and
controlled rollback or locking regressions separately from restored positives.
This dependency update does not change the ledger generation or migrate installed
application data.

## Shared runtime and broker test dependencies

The shared Tokio requirement starts at a patched version for
[RUSTSEC-2026-0386](https://rustsec.org/advisories/RUSTSEC-2026-0386.html).
The former broker fixture library pulled an archive crate for which
[RUSTSEC-2026-0440](https://rustsec.org/advisories/RUSTSEC-2026-0440.html)
has no maintained release. The [NATS fixture](../../crates/awaken-connection-transports/src/nats/test_fixture.rs)
uses the Docker CLI to create its own broker on a dynamic loopback port, waits
for a real NATS connection, and removes only its exact container ID. The existing
round-trip, typed-address and trace-propagation cases still use a real broker.
A cleanup case verifies that dropping one fixture preserves another and that
cleanup also runs during unwinding. Neither advisory is waived.
