# ADR-0001: A neutral, open foundation tier

- **Status:** Accepted
- **Implementation:** in-progress
- **Date:** 2026-06-23

## Context

Repositories are split by **product**, with a separate shared *common* component for identity.
But "common" has two levels: identity-common (the common service) and infrastructure-common
(generic primitives like SQL migration and configuration). The infrastructure
primitives sit **below** the common service — the common service depends on them — and have consumers that do
not use the common service at all (a pure storage crate needs migration but not identity).

Placing such a primitive inside any one component's repo forces the wrong
dependency direction: `awaken-sql-migration` currently lives in the originating product, so
anything reusing it would depend on the originating product; putting it in the common service
would make a storage crate depend on identity. Both invert the dependency direction.

## Decision

We will keep a **single, neutral, open foundation repo** below every product and
the common layer. A crate belongs here only when **all** hold:

1. **Proven-shared (rule of three).** Depended on by ≥2 components across product
   lines today — not speculatively. Premature sharing is worse than duplication.
2. **Below everything.** It depends on no product or component above it; the
   dependency direction is strictly one-way (enforced by `xtask guardrail-lints`).
3. **Generic, no domain, no secrets.** Mechanism only — never a product's domain
   model and never credential/secret handling.
4. **Permissive.** MIT OR Apache-2.0, with `cargo deny` keeping the graph
   copyleft-free so every consumer can link it.

### Amendment: Opaque handshake-material carriers

The "no secrets" rule forbids foundation crates from owning credential policy or
raw secret lifecycles, but it does not forbid a narrow reusable mechanism for
already-materialized handshake material. A foundation crate may carry an opaque,
ephemeral, caller-owned value used by a transport handshake only when all of the
following hold:

- it does not acquire, select, authorize, refresh, rotate, persist, serialize, or
  domain-classify credentials;
- it does not perform vault, OAuth, environment, account, tenant, or connector
  lookup;
- its debug, display, error, tracing, and serialization surfaces cannot expose
  secret values by default;
- it validates representation-level injection hazards it introduces, such as
  HTTP header names/values, without interpreting the credential's authority; and
- consumers remain responsible for credential ownership, permission checks,
  expiry, audit logging, and revocation.

This keeps the foundation out of identity and secret management while allowing
cross-product transport adapters to share a small, audited container for
short-lived handshake material instead of reimplementing redaction and injection
guards at every egress boundary.

The first member is `awaken-scoped-migration` (promoted out of the originating product, where
it was mis-placed and named `awaken-sql-migration`).

## Consequences

- Easier: products and the common service share one base without cross-product coupling; the
  one-way rule is structural (a foundation crate cannot reach a product).
- Harder: a second repo and its release/version coordination (mitigated by
  published versions and consumer pinning); and the discipline to *not* add a
  crate until it has earned its place.
- New guardrail: `xtask guardrail-lints` fails any dependency on a product or common-layer crate that is not itself a foundation crate.

## Alternatives considered

- **Foundation inside the common service:** rejected — the common service is a consumer of the
  foundation, not its home; a storage crate would be forced to depend on identity,
  and the common service would lose focus.
- **Foundation inside a product repo:** rejected — the current
  mis-placement of `awaken-sql-migration`; makes reuse a cross-product dependency.
- **One monorepo for everything:** viable (the single-monorepo pattern) but
  rejected here because repositories are already split by product; the consistent
  choice in a product-split world is a neutral foundation repo, not folding the
  base into one component.
