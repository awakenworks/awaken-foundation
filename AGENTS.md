# AGENTS.md

Guidance for automated contributors in `awaken-foundation`.

## What this repo is

The open base tier. Crates here are depended on by everything above and depend on
nothing above. Read [ADR-0001](docs/adr/0001-foundation-tier.md) before adding a
crate.

## Hard rules

- **One-way dependency.** No foundation crate may depend on a product or
  on the shared common-service layer above it. Enforced by
  `cargo run -p xtask -- guardrail-lints`.
- **Earn your place.** Promote a crate here only when ≥2 components across product
  lines actually depend on it (rule of three). Premature sharing is worse than
  duplication.
- **No domain, no secrets.** Foundation carries generic mechanism only.
- **Stay permissive.** MIT OR Apache-2.0; `cargo deny` keeps the graph copyleft-free.
- **ADR discipline.** Architectural decisions get an ADR ([template](docs/adr/0000-template.md)).

## Checks

`pnpm hooks:install` wires the git hooks; `pnpm check` runs guardrails, fmt,
clippy, tests, and `cargo deny`. Commits: `<emoji> <type>(<scope>): <subject>`.
