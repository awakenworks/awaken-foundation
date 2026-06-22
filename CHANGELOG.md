# Changelog

All notable changes to `awaken-foundation` are recorded here.

## [Unreleased]

### Added
- Initial foundation scaffold: Cargo workspace, governance (lefthook hooks + CI
  checks, `cargo deny`, the no-upward-dependency `xtask` guardrail), and ADRs
  0001 (foundation tier) and 0002 (configuration convention).
- `awaken-sql-migration` — backend-agnostic SQL schema migration ledger, promoted
  from `awaken-next` to the shared foundation.

### Decided
- Configuration is a shared convention over `figment`, not a foundation crate,
  until duplication earns one (ADR-0002).
