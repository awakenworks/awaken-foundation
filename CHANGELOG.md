# Changelog

All notable changes to `awaken-foundation` are recorded here.

## [Unreleased]

### Added
- Initial foundation scaffold: Cargo workspace, governance (lefthook hooks + CI
  checks, `cargo deny`, the no-upward-dependency `xtask` guardrail), and
  ADR-0001 (foundation tier).
- `awaken-sql-migration` — backend-agnostic SQL schema migration ledger, promoted
  from `awaken-next` to the shared foundation.
- `awaken-config` — namespaced, layered configuration (initial skeleton).
