# ADR-0001: A neutral, open foundation tier

- **Status:** Accepted
- **Implementation:** in-progress
- **Date:** 2026-06-23

## Context

Repositories are split by **product** (`awaken-next` = agent platform,
`oversight` = task management) with `awaken-iam` as the shared *common* component.
But "common" has two levels: identity-common (IAM) and infrastructure-common
(generic primitives like SQL migration and configuration). The infrastructure
primitives sit **below** IAM — IAM depends on them — and have consumers that do
not use IAM at all (a pure storage crate needs migration but not identity).

Placing such a primitive inside any one component's repo forces the wrong
dependency direction: `awaken-sql-migration` currently lives in `awaken-next`, so
anything reusing it would depend on `awaken-next`; putting it in `awaken-iam`
would make a storage crate depend on identity. Both invert the layering.

## Decision

We will keep a **single, neutral, open foundation repo** below every product and
the IAM common layer. A crate belongs here only when **all** hold:

1. **Proven-shared (rule of three).** Depended on by ≥2 components across product
   lines today — not speculatively. Premature sharing is worse than duplication.
2. **Below everything.** It depends on no product or component above it; the
   dependency direction is strictly one-way (enforced by `xtask guardrail-lints`).
3. **Generic, no domain, no secrets.** Mechanism only — never a product's domain
   model and never credential/secret handling.
4. **Permissive.** MIT OR Apache-2.0, with `cargo deny` keeping the graph
   copyleft-free so every consumer can link it.

The first members are `awaken-sql-migration` (promoted out of `awaken-next`, where
it was mis-placed) and `awaken-config`.

## Consequences

- Easier: products and IAM share one base without cross-product coupling; the
  one-way rule is structural (a foundation crate cannot reach a product).
- Harder: a second repo and its release/version coordination (mitigated by
  published versions and consumer pinning); and the discipline to *not* add a
  crate until it has earned its place.
- New guardrail: `xtask guardrail-lints` fails any `awaken-*` / `oversight-*`
  dependency that is not itself a foundation crate.

## Alternatives considered

- **Foundation inside `awaken-iam`:** rejected — IAM is a consumer of the
  foundation, not its home; a storage crate would be forced to depend on identity,
  and IAM would lose focus.
- **Foundation inside a product repo (`awaken-next`):** rejected — the current
  mis-placement of `awaken-sql-migration`; makes reuse a cross-product dependency.
- **One monorepo for everything:** viable (the `awaken-1.0.0-dev` pattern) but
  rejected here because repositories are already split by product; the consistent
  choice in a product-split world is a neutral foundation repo, not folding the
  base into one component.
