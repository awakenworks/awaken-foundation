# Changelog

All notable changes to `awaken-foundation` are recorded here.

## [Unreleased]

### Added
- Initial foundation scaffold: Cargo workspace, governance (lefthook hooks + CI
  checks, `cargo deny`, the no-upward-dependency `xtask` guardrail), and ADRs
  0001 (foundation tier) and 0002 (configuration convention).
- `awaken-scoped-migration` — backend-agnostic ledger for namespace-scoped,
  independent SQL migrations, promoted from `awaken-next` (where it was
  `awaken-sql-migration`) to the shared foundation. Design review and target are
  recorded in [docs/design/scoped-migration.md](docs/design/scoped-migration.md).
- Auto-discovering migration commit hook (`hooks/check_migrations.py`, wired into
  lefthook `pre-commit`/`pre-push`) that finds the repo's migration files with no
  configuration and enforces every mechanically-checkable best practice —
  append-only immutability, a destructive-op guard, deterministic statements,
  portable SQL, and unique zero-padded version labels — with reviewable in-diff
  `migration-allow-*` override markers.

### Decided
- Configuration is a shared convention over `figment`, not a foundation crate,
  until duplication earns one (ADR-0002).
