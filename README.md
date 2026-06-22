# awaken-foundation

The **open, neutral base tier** shared across the Awaken/Oversight stack: small,
permissively-licensed crates that products and the IAM common layer depend on,
and that depend on no product themselves.

> Dual-licensed [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE).

## What lives here

A crate belongs in the foundation only when it is **proven-shared** — depended on
by at least two components across product lines — **stable**, **carries no product
domain or secrets**, and **depends on nothing above it**. See
[ADR-0001](docs/adr/0001-foundation-tier.md).

```
crates/
  awaken-sql-migration   backend-agnostic SQL schema migration ledger (postgres/sqlite)
xtask/                   foundation guardrail (no upward dependency)
```

Configuration is a shared **convention over `figment`**, not a crate — see
[ADR-0002](docs/adr/0002-configuration-convention.md). A config crate is promoted
only when duplication across services earns it (rule of three).

## Layering

```
products:        awaken-next   oversight   pack-hub
common service:  awaken-iam
foundation:      awaken-foundation   <- you are here (depends on nothing above)
```

## Develop

See [DEVELOPMENT](DEVELOPMENT.md): `pnpm hooks:install`, then `cargo test` and
`cargo run -p xtask -- guardrail-lints`.
