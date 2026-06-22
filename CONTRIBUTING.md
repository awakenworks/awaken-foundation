# Contributing

`awaken-foundation` holds the shared base crates (MIT OR Apache-2.0).

- Install hooks: `pnpm hooks:install`.
- Before pushing: `pnpm check` (guardrails, fmt, clippy, tests, `cargo deny`).
- Commits: `<emoji> <type>(<scope>): <subject>`.
- A new crate must earn its place (≥2 real cross-product consumers) and must not
  depend on anything above the foundation — see
  [ADR-0001](docs/adr/0001-foundation-tier.md).
