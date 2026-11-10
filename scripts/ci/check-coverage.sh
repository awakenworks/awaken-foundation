#!/usr/bin/env bash
# Migration-mechanism line coverage gate.
#
# The core planner and the sibling rusqlite shell are the in-process migration
# surface. The optional PostgreSQL driver remains excluded because it requires a
# live service and is validated by its integration suite. Keep the repository's
# database-enabled Rust 1.96 floor; llvm-tools-preview supplies its coverage runtime.
set -euo pipefail

coverage_min_lines="${COVERAGE_MIN_LINES:-90}"

cargo llvm-cov \
  --package awaken-scoped-migration \
  --package awaken-scoped-migration-sqlite \
  --ignore-filename-regex '/postgres\.rs' \
  --fail-under-lines "$coverage_min_lines" \
  --summary-only
