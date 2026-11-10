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

The optional NATS transport tests use the installed Docker CLI and
`nats:2.10-alpine`: `cargo test -p awaken-connection-transports --features nats`.
Their one private fixture publishes a random IPv4 loopback port, proves readiness
with the NATS protocol, and joins cleanup of its exact name/label/container ID.
It does not depend on the unmaintained tar stack in `testcontainers`.
