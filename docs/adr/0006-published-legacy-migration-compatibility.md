# ADR-0006: Preserve published legacy migrations through one checksummed constructor

- **Status:** Proposed
- **Implementation:** done
- **Date:** 2026-08-02

## Context

The scoped migration ledger correctly treats the recorded checksum as durable
truth and refuses to start when current code changes a published migration.
Deterministic-SQL admission was added after some consumers had already recorded
conditional migrations such as `DROP TABLE IF EXISTS`. Rewriting those files to
pass the new admission rule changes their checksums and makes existing databases
unstartable. Keeping the old bytes with `Migration::new` is also impossible
because current admission correctly rejects conditional DDL.

The ledger runner, checksum algorithm, ordering, and backend shells already have
one authoritative implementation. A consumer-side migration bypass or direct
ledger rewrite would create a second path and destroy the drift guarantee.

## Decision

`Migration` will expose compatibility constructors for portable and per-dialect
historical bodies: `published_legacy` and `published_legacy_per_dialect`. They
preserve migrations that predate deterministic admission while requiring the
caller to pin every checksum already recorded by supported backends.

If two released numbering tracks assigned different immutable bodies to the same
version but their completed bundles converge to the same schema,
`published_legacy_with_aliases` keeps one canonical body for absent receipts and
accepts a closed list of alternate historical receipt checksums. An alias never
selects or executes alternate SQL. The consumer must test each supported released
ledger prefix and its forward convergence; arbitrary interrupted prefixes are not
implied by the alias.

Construction and every later bundle validation recompute
`SHA-256(label || 0x00 || sql)` and reject any mismatch. Only this exact variant
skips conditional-SQL admission. It continues through the same `MigrationBundle`,
`plan`, runner, transaction, and ledger paths as every other migration. New
migrations must use `Migration::new` or `Migration::per_dialect`.

## Consequences

- Existing databases and fresh databases converge through the same historical
  migration bytes without changing ledger truth.
- A historical file cannot be silently cleaned up, reordered, or repurposed; its
  pinned checksum makes drift fail during application construction, before I/O.
- A known parallel numbering track can be retired without editing its ledger or
  keeping a second runner; exact aliases verify through the same `plan` path.
- Alias use is safe only when consumer tests prove the named complete release
  prefixes converge through the canonical pending suffix.
- Deterministic SQL remains mandatory for all new migration constructors.
- The compatibility marker is explicit at each exceptional call site and does
  not add a runner, repository, ledger table, or backend-specific path.

## Alternatives considered

- **Rewrite the published SQL and update ledger checksums.** Rejected because it
  falsifies durable history and cannot be made safe across all deployed stores.
- **Grandfather every migration or weaken conditional-SQL validation.** Rejected
  because it would turn compatibility into a general authoring bypass.
- **Handle the old version in each consumer before running the bundle.** Rejected
  because that duplicates migration planning and transaction semantics above the
  authoritative foundation owner.
- **Pin consumers forever to the older migration crate.** Rejected because it
  forks the mechanism and blocks security and correctness updates.
