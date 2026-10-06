# ADR-0007: Reinstate an sqlx SQLite migration runner beside the rusqlite one

- **Status:** Proposed
- **Implementation:** done
- **Date:** 2026-10-06

## Context

[ADR-0005](0005-sqlite-backend-crate-split.md) moved the synchronous rusqlite
SQLite runner into its own crate and removed the sqlx-driven SQLite shell
(`sqlite-sqlx`), because no repository consumed it and keeping it would have been a
second implementation of the same ledger semantics.

That premise has changed. awaken (its ADR-0087) moves all of its embedded SQLite
stores, about 25 crates, from rusqlite to sqlx so that each store's SQLite and
PostgreSQL adapters share one async shape. Those stores also run their schema
migrations. With only the rusqlite runner available, they must open a second
connection through a second driver to the same database file just to migrate it.
That keeps a synchronous driver and the rusqlite version coupling in a graph that
otherwise no longer uses rusqlite.

The dependency facts that bounded ADR-0005 are unchanged:
- `awaken-scoped-migration` already depends on sqlx as an optional dependency for
  its PostgreSQL runner.
- Cargo's `links` check applies at resolution time, including optional
  dependencies. Adding a `sqlite` feature therefore puts no new `libsqlite3-sys`
  version into any consumer's graph.
- This workspace already resolves rusqlite 0.32.1 and `libsqlite3-sys` 0.30.1.

## Decision

We will add a `sqlite` feature to `awaken-scoped-migration` that provides
`awaken_scoped_migration::sqlite::SqliteMigrationRunner`, an async runner driven by
sqlx, beside the existing PostgreSQL runner.

It is a second backend shell, not a second set of semantics:
- It delegates the apply decision to the shared core: `plan`, `render`,
  `check_ledger_version` and `LedgerSchema`.
- It writes the same ledger tables and the same checksums.
- It uses the same `BEGIN IMMEDIATE` single-applier guard.
- It adds the same read-only `verify_bundle` as the PostgreSQL runner.

A database migrated by either SQLite runner is accepted by the other. That
compatibility is a test in `awaken-scoped-migration-sqlite`.

`awaken-scoped-migration-sqlite` stays. It remains the runner for consumers that use
rusqlite, including those floating to rusqlite 0.40 as ADR-0005 allows, and it does
not enable the core crate's sqlx.

## Consequences

- An sqlx-based consumer migrates through its own pool. There is no second driver
  and no second connection.
- Two SQLite shells exist again. Divergence is guarded by sharing the core apply
  logic and by the cross-driver compatibility test. The test suite of the new shell
  mirrors the rusqlite shell's suite case for case.
- A consumer that enables `sqlite` resolves `libsqlite3-sys` 0.30 and so, if it also
  uses rusqlite, rusqlite at most 0.32. This is the same constraint as enabling
  `postgres` today.

## Alternatives considered

- **Consumers keep migrating through the rusqlite runner.** This leaves a second
  driver and a second connection to the same file in every sqlx-based consumer, and
  in-memory databases need a shared-cache workaround so both connections see one
  database.
- **Consumers implement their own sqlx SQLite runner.** This is the duplication
  ADR-0005 removed, outside foundation and without its tests.
