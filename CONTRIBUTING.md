# Contributing

`awaken-foundation` holds the shared base crates (MIT OR Apache-2.0).

- Install hooks: `pnpm hooks:install`.
- Before pushing: `pnpm check` (guardrails, fmt, clippy, tests, `cargo deny`).
- Commits: `<emoji> <type>(<scope>): <subject>`.
- A new crate must earn its place (≥2 real cross-product consumers) and must not
  depend on anything above the foundation — see
  [ADR-0001](docs/adr/0001-foundation-tier.md).

The mandatory trust-root guard pins the immutable webpki-roots 0.26.11
compatibility re-export and requires its one real certificate-data dependency
to use registry v1.0.10 or newer. `python3 scripts/ci/check_trust_roots.py`
rejects checksum/source/edge drift, old data, and a stale exception. Remove
the RUSTSEC-2026-0381 exception with the last compatibility consumer.
