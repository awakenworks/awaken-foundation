# Scoped migration — design review and target

`awaken-scoped-migration` is the backend-agnostic ledger for **namespace-scoped,
independent** SQL migrations. The name carries the intent: migrations are owned by
a *scope* (a namespaced bundle) and each scope evolves independently — the
property the rest of this document calls "separable and combinable". This document
reviews the **current** design (ported from `awaken-next`, where it was named
`awaken-sql-migration`), names its problems, and specifies the **target** design
the foundation crate converges to.

The review is grounded in two real implementations: the ported crate
(`crates/awaken-scoped-migration`) and `awaken-iam`'s parallel
`store/migration.rs`. They disagree on the most important axis, and the
convergence below resolves it by **taking the better idea from each**.

## Current shape (what we have)

- A **pure core**: `Migration` / `MigrationBundle` value types, validation,
  SHA-256 checksums, the `plan()` decision (unknown applied version, checksum
  drift, already-applied skip), and a `MigrationError` taxonomy.
- A **thin backend shell** per driver (`postgres` over sqlx, `sqlite` over
  rusqlite) that fetches applied versions, calls `plan()`, and applies the result
  with its own ledger DDL and transaction.
- A **per-prefix ledger** table (`{prefix}_schema_migrations`) recording
  `(bundle_id, version, checksum, description, applied_at, applied_by)`.

The two-layer split (pure decision core + dialect shell) is good and is kept. The
problems are below.

## What "separable and combinable" actually means

The crate's reason to exist is **separable-and-combinable migrations**, and
`awaken-next` already uses it that way. The property is **two-dimensional**, not
just "different services":

- **Separable along the bundle dimension.** Inside one database, each store
  module owns its own `MigrationBundle` with a namespaced id (`runtime.event_store`,
  `runtime.outbox`, `gateway.session_store`, …) and its **own independent version
  stream**. Adding a store module adds a bundle; it never touches another's
  versions. A bundle must not hard-couple to another (a cross-bundle FK would
  defeat splitting), so bundles compose in any subset.
- **Combinable along the prefix dimension.** `with_prefix` isolates a whole schema
  behind a table prefix and a per-prefix ledger (`{prefix}_schema_migrations`), so
  several services' schemas coexist in one database — the same discipline IAM
  applies with `with_prefix("iam")`.

So: **bundles separate; prefixes combine.** That is the property the foundation
crate must preserve, and the two problems most tied to it (P3, P5) are sharpened
accordingly below.

## Problems

### P1 — SQL is dialect-bound, so portable schemas are written twice (the big one)

The crate's stated rule is "migration SQL is dialect-bound and lives with each
backend's bundles". A service that supports **both** Postgres and SQLite must
therefore author its DDL **twice** — once per dialect — and keep the two copies in
lock-step forever. That is exactly the duplication a "backend-agnostic" crate
should remove.

`awaken-iam`'s parallel implementation already solves this: DDL is written **once**
with portable tokens — `{prefix}` for the table prefix and a small type vocabulary
(`{json}`, `{timestamptz}`, `{now}`, `{pk_autoinc}`, `{blob}`) — and the active
`Dialect` renders each token to its backend form. One template, both backends.
**The version we are retiring is better on this axis**, and the foundation crate
must adopt it.

### P2 — Checksum is over raw SQL, defeating portability even if you tried it

`Migration::checksum()` hashes the raw SQL string. So even if a service hand-wrote
"equivalent" Postgres and SQLite SQL, the two would checksum differently and the
ledger would report **false drift** when the same logical migration is verified on
the other backend. `awaken-iam` checksums the **neutral template** (id + token SQL,
before rendering), so the recorded identity is dialect-independent: the same
checksum on Postgres and SQLite, while the rendered SQL legitimately differs. The
target adopts template-checksumming.

### P3 — Positional append-only versions collide on concurrent append

Versions are strictly increasing per bundle. In `awaken-next` they are
**positional and append-only** — `bundle_from_statements` assigns `index + 1`, and
the contract is "only append at the end, never reorder/insert" (reordering changes
a recorded version's SQL and fails closed at the checksum). That is sound for one
author, but two branches that each append a statement both take the **same next
version**, so they collide at merge — the same shape the IAM integration hit (a
duplicated `AUTHZ_0002`). The bundle validator catches it at *runtime*; the design
still *invites* it. The target makes the clash a build-time, mechanically
detectable condition (see "Versioning" below).

### P4 — Hand-rolled single-statement SQL parser in the hot path

The Postgres shell carries a ~70-line byte scanner (`is_single_statement`) for
dollar-quotes, comments, and string literals, purely to satisfy sqlx's
prepared-protocol "one statement per query" rule. It is a fragile maintenance
liability (escape strings `E'\''`, nested/edge-case dollar tags, future syntax)
sitting on every migration. SQLite needs none of it (`execute_batch` runs
multi-statement). The target removes the asymmetry — apply via the **simple-query
path on both backends** so a migration body is just SQL, and delete the parser.

### P5 — Bundle independence is convention, not an enforced invariant

Bundle independence is exactly what makes the **separable** dimension work, and
`awaken-next` relies on it today — namespaced bundle ids (`runtime.*`, `gateway.*`)
and a "no cross-bundle FK" rule stated in comments. But nothing *enforces* it: a
bundle's SQL can reference another bundle's table and *appear* to work by
registration-order luck, silently coupling two units that are supposed to split
apart. Since the whole value proposition rests on this, the target promotes it to
a checked invariant (a bundle's DDL may not name another scope's prefix), not a
comment.

### P6 — No single-applier guard for concurrent startup (esp. SQLite)

Postgres takes `pg_advisory_xact_lock`; SQLite "relies on single writer". But N
nodes (or processes) starting against the same database race the
check-then-apply (TOCTOU): two can both read an empty ledger and both try to
apply. `awaken-iam` has a backend-neutral **single-applier guard**
(`acquire_applier_guard`) — advisory lock on Postgres, `BEGIN IMMEDIATE` /
app-lock row on SQLite — always released, including on error. The target adopts
this so concurrent startup is safe on both backends.

### P7 — The ledger table has no migration path of its own

`ensure_ledger` is a fixed `CREATE TABLE IF NOT EXISTS`. If the ledger schema ever
needs a column, there is no path to evolve it — the migrator cannot migrate
itself. The target versions the ledger (a `ledger_version` marker) so its own
schema can evolve.

### P8 — Two parallel implementations (the meta-problem)

`awaken-iam` reimplements this crate (scope-partitioned bundles, token rendering,
single-applier, store-prefix isolation). Maintaining both guarantees drift. The
convergence: **the foundation crate is the single home**; IAM retires its copy and
contributes its better ideas (P1, P2, P6) plus its `BundleScope` partitioning.

## Target design

### Two layers, unchanged

Keep the pure decision core + thin dialect shell. `plan()` stays pure and
backend-agnostic; every interesting decision (drift, unknown version, skip)
remains testable without a database. This part of the current design is correct.

### Write SQL once: portable tokens + dialect rendering (fixes P1, P2)

A migration body is a **neutral template** carrying `{prefix}` and a closed type
vocabulary (`{json}`, `{timestamptz}`, `{now}`, `{pk_autoinc}`, `{blob}`, …). The
`Dialect` renders tokens to backend SQL at apply time. The **checksum is taken
over the template**, so a migration's recorded identity is dialect-independent;
the rendered SQL differs per backend by design. Bundles needing genuinely
backend-specific SQL remain possible as an escape hatch, but the default is
write-once.

### Versioning: readable labels, collision caught at build (fixes P3)

There are two separable concerns here — **readability** and **collision
resistance** — and a zero-padded label like `V0001` addresses only the first.

- **Readability.** Versions are recorded and displayed as zero-padded labels
  (`V0001`, `V0002`, …) rather than bare integers. This is the familiar
  Flyway-style form, sorts lexically the way it reads, and is far more legible in
  a ledger or a diff than `1`, `2`. Adopt it.
- **Collision resistance.** Zero-padding does **not** fix P3: two branches that
  each append still both pick `V0003`, exactly as they both picked `3`. The fix is
  orthogonal to the label format — a **build-time uniqueness lint** that rejects a
  bundle containing a duplicate version. That turns the merge collision into a
  failing check at PR time instead of a surprise at deploy, while keeping the
  human-readable sequential labels.

So the target is **zero-padded sequential labels (`V0001`) plus a build-time
duplicate-version lint** — readable *and* collision-safe — rather than ugly
timestamp versions. The ledger still records the version and ordering stays
strictly increasing within a bundle. (Timestamp versions remain a valid escape
hatch for a bundle with many concurrent contributors, but they are not the
default; readability wins for the common case.)

### Apply via the simple-query path on both backends (fixes P4)

Run migration bodies through the multi-statement simple-query path
(`execute_batch` on SQLite; the unprepared path on Postgres). A migration body is
plain SQL; the hand-rolled single-statement parser is deleted. This also lets one
migration carry several statements on Postgres, matching SQLite.

### Enforce bundle independence (fixes P5)

Make "no cross-bundle reference" a checked rule: bundles are
**scope-partitioned** (adopting IAM's `BundleScope` idea — `iam.identity`,
`iam.authz`, …), each independently versioned, and a build-time check (or a
documented review guardrail) rejects a bundle whose DDL names another scope's
prefix. Independence is what makes services separable-and-combinable; it must be
verified, not assumed.

### Backend-neutral single-applier guard (fixes P6)

Wrap each run in a guard the backend implements: `pg_advisory_xact_lock` on
Postgres, `BEGIN IMMEDIATE` (or an app-lock row) on SQLite. Exactly one node
applies a pending bundle; the others wait, then verify the ledger. The guard is
always released, including on drift/apply error, so a failed run never strands the
lock.

### Self-versioned ledger (fixes P7)

Stamp the ledger table with its own `ledger_version`; evolving the migrator's
schema becomes a normal, ordered step rather than an impossible one.

### One home (fixes P8)

The foundation crate is authoritative. `awaken-iam` deletes `store/migration.rs`,
depends on this crate, and keeps only its bundles (the `iam.*` token SQL). This
finally gives the crate its second cross-product consumer (alongside
`awaken-next`'s server/stores), satisfying the foundation's rule-of-three.

## Non-goals (explicit stances)

- **Forward-only, no down-migrations.** Neither implementation has rollback, and
  the target keeps it that way: rollback SQL is rarely correct under data, and a
  fail-closed forward-only ledger (drift ⇒ refuse to start) is safer than a
  reversible one that can half-undo. Recovery is a new forward migration.
- **No ORM / schema diffing.** Migrations are explicit SQL, not generated from a
  model. The crate is a *ledger and runner*, not a schema framework.

## Best practices (every one is hook-verified)

The rule is deliberate: **a "best practice" here is something a hook can verify.**
A guideline that cannot be mechanically checked (e.g. "one logical change per
migration") is review advice, not a listed practice. Every practice below maps to
a check in the next section.

1. **Migrations are immutable once committed.** Never edit or reorder a shipped
   migration; only append new ones. Editing changes what a version means and
   corrupts the ledger.
2. **Destructive ops are deliberate.** A `DROP` / `TRUNCATE` / unqualified
   `DELETE` must be explicitly acknowledged, never incidental — this is also how
   *expand–contract* (add → backfill → tighten, never drop-and-readd) is enforced.
3. **Deterministic statements — no conditional DDL.** No `IF NOT EXISTS`,
   `IF EXISTS`, or `CREATE OR REPLACE`. The ledger already guarantees each
   migration runs **exactly once**, so conditional DDL is redundant *and* harmful:
   it branches on live DB state and **masks drift** — `CREATE TABLE IF NOT EXISTS`
   silently passes when the ledger says "not applied" but the table already exists,
   recording a lie; a bare `CREATE TABLE` fails loudly and surfaces the
   inconsistency. The one legitimate `IF NOT EXISTS` is the ledger's own bootstrap,
   which is crate-internal (and excluded from the hook's discovery).
4. **Portable SQL by default.** Use the token vocabulary (`{prefix}`, `{json}`,
   `{timestamptz}`, …) so one template serves both backends; raw dialect types only
   via the escape hatch.
5. **Readable, unique versions.** Zero-padded labels (`V0001`) within a bundle, no
   duplicates.
6. **Namespace every bundle; never cross scopes.** A bundle id is `<scope>.<unit>`
   (`iam.authz`, `runtime.event_store`) and its DDL names only its own prefix.
   Cross-scope references defeat the separable dimension (P5).

## Validation hooks (what to add, where)

Every practice is enforced by a hook. Most are a **commit-time** static check in
the portable script below; the two that need the bundle *types* (version ordering
across the ledger, cross-scope reference) are enforced by the crate's
`MigrationBundle::new` validation, surfaced through the consumer's `cargo test` in
a **pre-push** hook — still a hook, just a different layer.

| Practice | Check | Hook | Layer |
|---|---|---|---|
| Immutability (1) | append-only: no edit/delete of a shipped line (diff) | `check_migrations.py --staged` | pre-commit |
| Destructive (2) | added `DROP`/`TRUNCATE`/`DELETE`-no-`WHERE` needs a marker | `check_migrations.py` | pre-commit |
| Deterministic (3) | added `IF NOT EXISTS` / `IF EXISTS` / `CREATE OR REPLACE` | `check_migrations.py` | pre-commit |
| Portable SQL (4) | raw `JSONB`/`TIMESTAMPTZ`/`SERIAL`/`now()` outside the escape hatch | `check_migrations.py` | pre-commit |
| Versions (5) | non-`V0001`-form or duplicate version label | `check_migrations.py` | pre-commit |
| Drift (1) | recorded checksum ≠ recomputed | crate `plan()` fail-closed via `cargo test` | pre-push |
| Ordering / cross-scope (5,6) | non-increasing version; DDL names another scope's prefix | crate `MigrationBundle::new` via `cargo test` | pre-push |

### The portable commit hook

This repo ships [`hooks/check_migrations.py`](../../hooks/check_migrations.py): a
single, dependency-free Python file that **auto-discovers** a repo's migration
files and enforces practices 1–5. It is built to be **dropped into any consuming
repo and wired at commit time**, like the family's `check_adr_immutability.py`.

- **Auto-discovery — no configuration.** A file is a migration file if it
  *constructs* migrations (`MigrationBundle::new` / `bundle_from_statements` /
  `Migration::new(` in a `.rs`, or a `.sql` on a migration path); the crate's own
  implementation (anything *defining* `MigrationBundle` / `plan` / a
  `MigrationRunner`) is excluded. A root `.migration-paths` file (globs) can *add*
  paths but is never required.
- **Modes.** `--audit` discovers and checks every migration file in the tree (CI /
  first adoption); `--staged` / `--base REF` check the migration files in the diff
  (pre-commit / pre-push); `--self-test` validates the checker.
- **Wiring (lefthook):**
  ```yaml
  pre-commit:
    commands:
      migrations:
        run: python3 scripts/ci/check_migrations.py --self-test && python3 scripts/ci/check_migrations.py --staged
  pre-push:
    commands:
      migrations:
        run: python3 scripts/ci/check_migrations.py --audit
  ```

Intentional exceptions are made reviewable by an in-diff marker
(`migration-allow-edit`, `migration-allow-destructive`, `migration-allow-raw-sql`,
`migration-allow-conditional`) rather than by disabling the hook. The two
pre-push crate checks stay with the crate, since they need the bundle types and
cannot be derived from a diff. See [`hooks/README.md`](../../hooks/README.md).

## Migration path

1. Land the token-rendering core + template-checksum in the foundation crate
   (port from IAM, behind the existing pure-core/shell split).
2. Add the single-applier guard to both shells.
3. Switch `awaken-next`'s server/stores to the token form (or keep raw-SQL bundles
   via the escape hatch initially).
4. Retire `awaken-iam`'s `store/migration.rs`; depend on this crate; move the
   `iam.*` bundles over.
5. **Rewrite existing bundles to deterministic DDL.** Today both `awaken-next` and
   `awaken-iam` use `CREATE TABLE IF NOT EXISTS` in their bundles (a habit from
   before the ledger guaranteed exactly-once). Drop the `IF NOT EXISTS`: the ledger
   is the single idempotency mechanism, and a bare `CREATE` surfaces drift instead
   of hiding it. The ledger's own bootstrap DDL keeps `IF NOT EXISTS` — it cannot
   ledger its own creation.
6. Add the build-time lints (duplicate version, cross-scope prefix reference) and
   wire `check_migrations.py` into each consumer's hooks.
