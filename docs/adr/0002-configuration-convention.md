# ADR-0002: Configuration is a convention over figment, not a crate

- **Status:** Accepted
- **Implementation:** planned
- **Date:** 2026-06-23

## Context

Services must be **separable and combinable**: each runs standalone or composed
into one deployment, and combining them must never produce a configuration
conflict. The mechanics this needs — layered sources (defaults, file, env, flags)
with deterministic precedence, namespacing so two services never share a key, and
extraction into a strongly-typed config struct — are exactly what mature crates
already provide. [`figment`](https://docs.rs/figment) does all of it: layered
providers, profiles for namespacing, and `extract::<T>()` into a `serde` type.

An early `awaken-config` crate wrapped a hand-written string map. It was weaker
than figment (no real source parsing, no typed extraction, stringly-typed `get`),
and — decisively — it had **no second consumer**: only one service used it, so by
this repo's own rule of three ([ADR-0001](0001-foundation-tier.md)) it had not
earned a place in the foundation. Wrapping figment in a thin crate now would only
freeze an unproven shape.

## Decision

We will treat configuration as a **shared convention over `figment`**, not as a
foundation crate, until duplication proves a crate is warranted:

1. **Use figment directly.** Each service depends on `figment` and extracts its
   own strongly-typed config struct (`#[derive(Deserialize)]`). Config shape is
   checked at startup, not at point-of-use.
2. **Namespace by service.** Every service owns a top-level namespace (`identity`,
   `region`, `billing`, …) — a figment profile / nested table — and reads only
   its own. Two services never share a top-level key, so a combined deployment is
   just several namespaced sections side by side. This generalises the
   `with_prefix` table-prefix discipline the common service already uses for the database.
3. **Deterministic precedence.** Layer order is fixed: defaults < file < env <
   flags; a later layer overrides an earlier one for the same key.
4. **Fail closed.** A required key that is absent is a startup error, never a
   silent default.
5. **Compose once, inject.** The composition root loads the merged configuration
   once and hands each service its typed config; there is no global mutable
   config singleton the services contend over.

If, later, ≥2 services duplicate the same figment boilerplate (profile naming,
env-prefix mapping, the validation skeleton), the **already-grown** common part is
promoted to a thin `awaken-config` crate over figment — shape decided by real
usage, per rule of three.

## Consequences

- Easier: strong typing and battle-tested loading for free; no premature
  abstraction; the no-conflict property holds by namespacing convention.
- Harder: the convention lives in this ADR rather than in a compiler-enforced
  crate, so it depends on review discipline until a crate is justified.
- No new guardrail; the convention is documentation until duplication earns code.

## Alternatives considered

- **A thin `awaken-config` crate over figment now:** rejected — no second
  consumer yet (violates rule of three) and it would lock an unproven API.
- **A hand-written config engine:** rejected — strictly worse than figment and
  pure reinvention.
- **The `config` crate instead of figment:** a viable equivalent; figment is
  chosen for first-class profiles (namespacing) and ergonomic typed extraction.
  Either satisfies this convention.
