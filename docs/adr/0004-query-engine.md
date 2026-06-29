# ADR-0004: Shared query engine

- **Status:** Proposed
- **Implementation:** planned
- **Date:** 2026-06-29

## Context

[ADR-0003](0003-api-contract-primitives.md) put the **contract half** of list
queries in `awaken-api-contract`: the `FilterNode` AST, the per-resource
`QuerySchema` allowlist, `ListQuery`, `SortClause`, `PaginationRequest`, and
`CursorPage<T>`. It deliberately kept a query *executor* out of scope, because
database paths, RLS, and projection semantics are product-specific.

The **execution half** — turning a validated query into SQL — is now being
reimplemented per product:

- speak2app ships `s2a-query-dsl` (a mature, ~6k-line crate): a filter parser, a
  parameterized `WHERE`/`ORDER BY` builder, and compound row-value keyset cursor
  conditions. Its security posture is sound (every value parameterized, every
  field allowlisted, array/string bounds enforced).
- oversight-next needs the same filter/sort/cursor machinery, plus **grouped
  pagination** (group by a field, per-group total, per-group cursor) that
  `s2a-query-dsl` does not have.

`s2a-query-dsl` cannot be lifted into foundation as-is. Four constraints bind it
(detailed in [docs/design/query.md](../design/query.md)):

1. it emits PostgreSQL-only SQL (`$N` placeholders, `= ANY`/`!= ALL`), but
   oversight-next is sqlite **and** postgres;
2. it depends on product crates (`s2a-errors`, `s2a-types`), which the one-way
   foundation rule forbids;
3. it is a string-grammar DSL, whereas the contract crate's canonical syntax is
   the structured `FilterNode` tree;
4. it has no grouping, which is the core of the oversight-next need.

A part of execution is nonetheless **generic mechanism, not product domain**:
compiling an already-validated `FilterNode` + field→column map + dialect into a
parameterized SQL fragment. That part is reusable across product lines and
depends on nothing above the base tier.

## Decision

We will add `awaken-query` as a foundation crate for the product-free execution
half of list queries. It will:

1. compile an **already-validated** `ListQuery` (the contract crate's allowlist
   stays the single validation path) plus an author-controlled field→column
   `FieldMap` into a `SqlFragment` — parameterized `WHERE`, `ORDER BY`, limit,
   and ordered bound parameters;
2. render dialect differences (placeholders, set membership, case-insensitive
   match, nulls ordering) through a `Dialect` abstraction covering Postgres and
   SQLite, so the builder is written once — the same token+dialect idiom
   [ADR — scoped migration target](../design/scoped-migration.md) uses;
3. provide opaque keyset cursor encode/decode and a compound row-value keyset
   `WHERE` condition, reusing the contract crate's opaque-cursor convention;
4. provide **grouped** compilation (`GROUP BY` / partitioned per-group count /
   partitioned keyset), with the `GroupedPage<G, T>` response envelope added to
   `awaken-api-contract`;
5. offer the speak2app string grammar only as an optional, feature-gated
   front-end parser that produces the same `FilterNode` and runs the same
   `QuerySchema::validate` — never a second path to the builder.

`awaken-query` will reuse `awaken_api_contract::FilterOperator` and the other
contract types, and will depend only on `awaken-api-contract`, `serde`,
`serde_json`, `thiserror`, and `base64` (plus optional `schemars`). It will not
depend on a storage driver, product crate, HTTP framework, RLS layer, or any
s2a-* crate. It returns fragments and parameters; it never opens a connection,
executes, or supplies `FROM`/projection/tenancy predicates.

This crate is `Proposed` until the rule of three is met. speak2app already runs
the source implementation; oversight-next is the intended second consumer. The
crate is promoted (and this ADR moves to `Accepted`) once oversight-next consumes
it and speak2app migrates off `s2a-query-dsl` onto it.

## Consequences

Two product lines converge on one filter→SQL engine instead of maintaining
parallel builders, and a dual-store consumer writes its query path once for both
backends. The contract crate keeps the single allowlist; this crate keeps the
single SQL emitter and the single cursor mechanism, so a field can never reach
SQL except as a mapped column or a bound parameter.

The security invariants become this crate's guardrails: every value is a bound
parameter, every field is mapped or rejected (identifiers are author-controlled,
never request-derived), array/string bounds are checked before building, the
string DSL and structured input share one validation path, and cursors are
opaque and fail closed on a sort/schema mismatch. The one-way dependency rule is
enforced by `cargo run -p xtask -- guardrail-lints` as for every foundation
crate.

Harder: grouped pagination is new design, not a port, and lands across both
crates (`compile_grouped` here, `GroupedPage<G, T>` in the contract crate); both
must ship together to be usable. Migrating speak2app off `s2a-query-dsl` is a
real refactor (re-home onto foundation types, drop `s2a-errors`/`s2a-types`), not
a drop-in swap.

## Alternatives considered

- **Lift `s2a-query-dsl` into foundation as-is:** rejected — it is
  PostgreSQL-only and depends on `s2a-errors`/`s2a-types`, violating the one-way
  rule, and its string grammar conflicts with the contract crate's canonical
  `FilterNode`.
- **Put the executor in `awaken-api-contract`:** rejected — ADR-0003 keeps the
  contract crate free of SQL building and storage concerns; the contract (what a
  query *is* and whether it is allowed) and the execution (how it becomes a SQL
  fragment) are separable and version independently.
- **Make the string DSL the primary syntax:** rejected — ADR-0003 already chose
  bracket/structured `FilterNode` as canonical for OpenAPI, generated clients,
  field-level validation, and audit trails; the DSL stays an optional parser into
  the same AST.
- **Leave each product to build its own filter→SQL:** rejected — the same
  mechanism is already duplicated, and oversight-next would add a third copy with
  the grouping logic written from scratch with no shared security review.
- **Ship a full query executor (open connection, run SQL):** rejected — database
  paths, transactions, projection, and RLS scope are product-specific; foundation
  emits fragments and parameters only.
