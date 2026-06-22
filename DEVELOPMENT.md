# Development

## Toolchain

- Rust (pinned by [`rust-toolchain.toml`](rust-toolchain.toml), 1.96.0).
- Node ≥ 20 and pnpm ≥ 10 for the hook tooling.

## Setup

```sh
pnpm install
pnpm hooks:install
```

## Common tasks

```sh
cargo test --workspace
cargo run -p xtask -- guardrail-lints     # no-upward-dependency guardrail
pnpm check                                # guardrails + fmt + clippy + tests + cargo deny
```
