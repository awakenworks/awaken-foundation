# Changelog

All notable changes to `awaken-foundation` are recorded here.

## [Unreleased]

### Added
- Initial foundation scaffold: Cargo workspace, governance (lefthook hooks + CI
  checks, `cargo deny`, the no-upward-dependency `xtask` guardrail), and ADRs
  0001 (foundation tier) and 0002 (configuration convention).
- `awaken-scoped-migration` — backend-agnostic ledger for namespace-scoped,
  independent SQL migrations, promoted from the originating product (where it was
  `awaken-sql-migration`) to the shared foundation. Design review and target are
  recorded in [docs/design/scoped-migration.md](docs/design/scoped-migration.md).
- Auto-discovering migration commit hook (`hooks/check_migrations.py`, wired into
  lefthook `pre-commit`/`pre-push`) that finds the repo's migration files with no
  configuration and enforces every mechanically-checkable best practice —
  append-only immutability, a destructive-op guard, deterministic statements,
  portable SQL, and unique zero-padded version labels — with reviewable in-diff
  `migration-allow-*` override markers.
- `awaken-query` — dialect-aware compiler from `awaken-api-contract` list
  queries to parameterized SQL fragments (filter `WHERE`, `ORDER BY`, opaque
  keyset cursor conditions, and grouped-pagination windows) for PostgreSQL and
  SQLite. Re-homes the proven speak2app query DSL onto foundation types: every
  value bound, every field mapped or rejected, with an optional string-DSL
  front-end (`dsl` feature) that routes through the same allowlist. Design
  review and target are in [docs/design/query.md](docs/design/query.md).
- `GroupedPage`/`GroupBucket` envelopes and `allow_group_field` on `QuerySchema`
  in `awaken-api-contract`, for grouped list responses with per-group totals and
  intra-group cursors.

### Decided
- Configuration is a shared convention over `figment`, not a foundation crate,
  until duplication earns one (ADR-0002).
- Shared query engine: `awaken-query` carries the product-free execution half of
  list queries (validated query → parameterized SQL fragment), distinct from
  `awaken-api-contract`'s contract half (ADR-0004).
