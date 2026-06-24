#!/usr/bin/env bash
#
# Foundation unit-test coverage gate.
#
# Enforces a line-coverage floor (default 90%) over the foundation crates'
# testable surface, measured with cargo-llvm-cov. The pure migration core and the
# SQLite backend shell run entirely in-process, so they are the surface a unit
# suite can cover; the Postgres shell is a thin driver layer exercised only
# against a live database, so it is built out (its feature is off, so it is not
# compiled into the report) and the `xtask` guardrail binary is excluded — both
# are covered by their own checks, not this floor.
#
# Wired into `pre-push` (lefthook) and `pnpm check`; below the floor the command
# exits non-zero and fails the build.
set -euo pipefail

THRESHOLD="${COVERAGE_MIN_LINES:-90}"

cargo llvm-cov \
  --features sqlite \
  --package awaken-scoped-migration \
  --ignore-filename-regex '(xtask|/postgres\.rs)' \
  --fail-under-lines "$THRESHOLD" \
  --summary-only
