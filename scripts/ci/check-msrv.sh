#!/usr/bin/env bash
# Verify each public crate group at its declared compiler floor, independently
# of the developer toolchain. SQLx 0.9 requires Rust 1.94; the other base crates
# retain their existing Rust 1.88 contract.
set -euo pipefail

cargo +1.88.0 check --locked --workspace \
  --exclude awaken-scoped-migration \
  --exclude awaken-scoped-migration-sqlite \
  --all-targets --all-features

cargo +1.94.0 check --locked \
  --package awaken-scoped-migration \
  --package awaken-scoped-migration-sqlite \
  --all-targets --all-features
