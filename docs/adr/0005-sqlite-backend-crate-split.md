# ADR-0005: Split the rusqlite SQLite backend into its own crate

- **Status:** Proposed
- **Implementation:** done
- **Date:** 2026-07-05

## Context

`awaken-scoped-migration` shipped three backend shells behind features:
`postgres` and `sqlite-sqlx` (both `sqlx`-driven) and `sqlite` (the synchronous
`rusqlite` driver). All three lived in one crate.

`rusqlite` and `sqlx` both link the native, `links = "sqlite3"` crate
`libsqlite3-sys`, of which a dependency graph may contain **exactly one**
version (Cargo's `links` uniqueness rule, enforced at resolution time — it holds
even for optional dependencies that are present but not activated). The versions
do not line up and cannot be made to:

- `rusqlite` `^0.32` → `libsqlite3-sys` `0.30`;
- `rusqlite` `0.40` → `libsqlite3-sys` `0.38`;
- `sqlx` `0.8` **and** `0.9` → `libsqlite3-sys` `0.30` (sqlx has not moved to
  0.38, so no sqlx release pairs with rusqlite 0.40).

While both backends resolved to `libsqlite3-sys` 0.30 the single crate was fine.
It stopped being fine the moment a *consumer* needed a newer `rusqlite`: a
product pinning `rusqlite` 0.40 (for a driver that needs it) and this crate's
hard `rusqlite = "0.32"` put two `libsqlite3-sys` majors in one graph — an
unsolvable `links` conflict. The consumer's only escape was to drop the whole
crate and hand-maintain a bespoke store, which is what triggered this ADR.

Two further facts bound the fix, both established empirically with the resolver:

- `rusqlite` 0.40 / `libsqlite3-sys` 0.38 use the `cfg_select!` macro and need a
  rustc newer than 1.88 (fails on 1.92, builds on 1.96), so floating up to it
  raises the effective MSRV for that build — see [ADR tie-in with the 1.88
  MSRV](../../Cargo.toml).
- `sqlx` 0.9 requires Rust 1.94 and still pins `libsqlite3-sys` 0.30, so
  upgrading sqlx neither fixes the conflict nor fits the 1.88 floor.

## Decision

We will house the synchronous `rusqlite` SQLite backend in its own crate,
`awaken-scoped-migration-sqlite`, depending on the pure core crate
`awaken-scoped-migration` (whose public backend-authoring surface — `plan`,
`render`, `check_ledger_version`, the value types, and `MigrationError` — is
already exported). The `sqlx`-driven backends (`postgres`, `sqlite-sqlx`) stay
in `awaken-scoped-migration`.

The new crate requires `rusqlite` with a **range, not a hard `^0.x` pin**:
`rusqlite = ">=0.32, <0.41"`. Pinning a single 0.x of a `links` native library
is precisely what deadlocks a graph that needs a different driver version; the
range lets the resolver unify this backend onto whatever `rusqlite` the rest of
the graph already selected. Its `rust-version` states the floor for the range's
lower end (0.32, which builds on 1.88); a consumer that floats it to a newer
`rusqlite` accepts that driver's higher rustc requirement.

## Consequences

- A consumer that needs `rusqlite` 0.40 depends on `awaken-scoped-migration-sqlite`
  and its own `rusqlite`; the resolver unifies both onto `libsqlite3-sys` 0.38.
  Because it does not enable the core crate's optional `sqlx`, no second
  `libsqlite3-sys` enters the graph. Verified: a downstream crate forcing
  `rusqlite` 0.40 builds against both crates on 1.96 with no conflict.
- Foundation's own workspace keeps `rusqlite` at 0.32 (the range's lower end,
  unified with sqlx's `libsqlite3-sys` 0.30), so the `+1.88.0` MSRV gate stays
  green. A `cargo update` that floats `rusqlite` to 0.40 would turn that gate red
  — the intended signal that the effective MSRV rose.
- The `rusqlite` and `sqlx-sqlite` backends can no longer be built in one graph.
  That combination was never useful (two SQLite drivers in one binary), so the
  loss is nominal; a consumer picks exactly one native SQLite backend.
- This remains blocked only for a graph that genuinely needs *both* sqlx-sqlite
  and rusqlite 0.40 at once — unsolvable by anyone until sqlx moves to
  `libsqlite3-sys` 0.38. When it does, the range absorbs it with no code change.

## Alternatives considered

- **Keep one crate, bump `rusqlite` to 0.40.** Rejected: the crate's optional
  `sqlx` dep keeps `libsqlite3-sys` 0.30 in the resolution graph, so the crate
  fails to resolve even with no features — the `links` check is resolution-time.
- **Bump `sqlx` to 0.9 to unify.** Rejected: sqlx 0.9 still pins
  `libsqlite3-sys` 0.30 (no unification) and requires Rust 1.94 (> our 1.88
  floor).
- **A version *range* on `rusqlite` inside the single crate.** Rejected for the
  same reason as the bump: with sqlx present the resolver holds `rusqlite` at
  0.32 to satisfy `links`, so the range can never float to 0.40 there. The range
  only works once `rusqlite` is alone in its crate — hence the split.
- **Leave it to the consumer** (bespoke store, no shared ledger). Rejected: it
  duplicates the fail-closed ledger mechanism this tier exists to share.
