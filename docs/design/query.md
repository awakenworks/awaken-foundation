# Query engine — design review and target

`awaken-query` is the proposed foundation crate for the **execution half** of
list queries: compiling a validated, allowlisted query into a *parameterized,
dialect-aware* SQL fragment (filter `WHERE`, `ORDER BY`, keyset cursor condition,
limit) and decoding/encoding opaque cursors. It is the natural companion to
[`awaken-api-contract`](api-contract.md), which already owns the **contract
half** (the `FilterNode` AST, `QuerySchema` allowlist, `ListQuery`, `SortClause`,
`PaginationRequest`, `CursorPage<T>`).

This document reviews a real, proven implementation —
`s2a-query-dsl` in the speak2app repo
(`rust-services/crates/query-dsl`) — names the constraints that stop it from
being lifted as-is, and specifies the **target** a foundation crate converges to.
It is the design counterpart to [ADR-0003](../adr/0003-api-contract-primitives.md),
which already declared the contract types in scope and a query *executor* out of
scope for the contract crate; this doc is where the executor gets designed as its
**own** foundation crate. A promotion ADR (ADR-0004) should follow once the rule
of three is satisfied (speak2app already consumes the source; oversight-next is
the second consumer).

## What we have to learn from (s2a-query-dsl)

A mature, security-conscious query DSL — *both* parsing and building — about 6k
lines across five modules:

- **`filter.rs`** — parses a string grammar (`name='Alice' & active=true`,
  `id @ ('a','b')` for `IN`, `~`/`~~` for `LIKE`/`ILIKE`, parenthesized groups)
  into a `FilterExpr` AST, then `to_sql(start_param)` emits a parameterized
  `WHERE` clause. Fields are gated by an `allowed_fields` allowlist;
  `to_sql_with_mapping` maps logical field → physical column. Guarded by
  `MAX_ARRAY_SIZE` / `MAX_STRING_LENGTH`.
- **`sort.rs`** — `parse_sort` / `build_order_by` with nulls-placement and
  validation.
- **`cursor.rs`** — base64 cursor encode/decode, `build_cursor_condition`
  (compound row-value keyset comparison, forward/backward), and a
  `CursorPaginationResult<T>`.
- **`params.rs` / `include.rs`** — pagination params, relationship-`include`
  parsing and validation.

The security posture is good and is the part most worth keeping:
**parameterize every value, allowlist every field**. Two patterns to carry over
verbatim — never interpolate a field name (map it, or reject it) and bound array
and string sizes before building.

## Why it cannot be lifted as-is (four hard constraints)

### C1 — It is PostgreSQL-only; oversight-next is sqlite **and** pg

`FilterOperator::to_sql_postgres_array` emits PostgreSQL-specific set syntax
(`In` → `= ANY`, `NotIn` → `!= ALL`) and placeholders are `$1, $2, …`. SQLite
uses `?` placeholders and `IN (…)` / `NOT IN (…)` for sets. The SQL-building path
is therefore not portable, and a dual-store consumer would be back to the same
write-it-twice problem that [`scoped-migration`](scoped-migration.md) (P1) solves
with portable tokens + dialect rendering. The fix is the **same idiom**: a
`Dialect` abstraction renders placeholders and set operators; the builder is
written once.

### C2 — It depends on product crates (`s2a-errors`, `s2a-types`)

`FilterOperator` itself lives in `s2a-types`, and errors come from `s2a-errors`.
A foundation crate may depend on **nothing above it**
([AGENTS.md](../../AGENTS.md), one-way rule). The crate cannot be lifted; it must
be **re-homed** onto foundation types — and foundation *already has the operator
vocabulary*: `awaken_api_contract::FilterOperator`. The target reuses it rather
than forking s2a-types' copy, so there is exactly one operator enum in the base
tier.

### C3 — Two incompatible query philosophies in the same org

`s2a-query-dsl` is a **string grammar** (`name='Alice'`). `awaken-api-contract`
is a **structured JSON tree** (`FilterNode`). ADR-0003 already decided this:
bracket filters / structured `FilterNode` is the **canonical** wire syntax (clear
for OpenAPI, generated clients, field-level validation, audit logs), and a string
DSL is an **optional parser into the same AST** for CLI / expert search. The
target honors that — `awaken-query` builds from `FilterNode`; the string grammar
is an optional, feature-gated *front-end parser*, never the primary contract and
never a second path that can bypass the allowlist.

### C4 — No grouping; it is the core of the oversight-next need

`grep group_by` is empty. The oversight-next requirement — **group by a field,
return per-group total and a per-group cursor** — is not provided by
`s2a-query-dsl` and is genuinely new work. It must be designed fresh on top of
the shared primitives; the existing crate gives us the per-group *filter / sort /
keyset* machinery but not the grouping envelope.

## The clean convergence story

`awaken-api-contract` is the contract half (request/response shapes + the
allowlist that *rejects* a bad query). It deliberately stops short of building
SQL — ADR-0003 rejected "move a full query executor to foundation" because
database paths, RLS, and projection semantics are product-specific. The nuance
this design draws out: the **generic, product-free** part of execution — turning
a *validated* `FilterNode` + field→column map + dialect into a parameterized
fragment — is itself reusable mechanism, not product domain. That is what
`awaken-query` is, and it is what lets two product lines stop reimplementing
filter→SQL.

So:

> **`awaken-query`** = the canonical *filter / sort / cursor → SQL fragment*
> engine. Dialect-aware (pg + sqlite), driven by the `awaken-api-contract`
> contract types, allowlist-gated, and column-mapped. It emits **fragments and
> bound parameters**, never executes; the consumer owns the connection, the RLS
> scope, the `FROM`/projection, and the column map.

`awaken-query` is the executor half ADR-0003 left to the consumer, narrowed to
its product-free core; `awaken-api-contract` stays the contract half. Neither
depends on a storage driver.

## Target design

### Layering: contract crate parses & validates, query crate builds

```
HTTP/CLI request
  │
  ▼  awaken-api-contract: parse_list_query(pairs, &QuerySchema) → ListQuery   (validated, allowlisted)
  │                       (optional) string-DSL parser → FilterNode → same QuerySchema::validate
  ▼  awaken-query:        compile(&ListQuery, &FieldMap, dialect) → SqlFragment { where, order_by, limit, params }
  │                       cursor: encode/decode + keyset WHERE from the last row
  ▼  consumer:            binds params, supplies FROM/projection/RLS, executes
```

`awaken-query` **requires** an already-validated query. It does not re-implement
the allowlist; it takes `&QuerySchema` (or a `FieldMap` derived from it) and
fails closed if asked to emit a field not present in the map. Validation stays in
the contract crate so there is exactly one allowlist; building stays here so
there is exactly one SQL emitter.

### Write SQL once: a `Dialect`, mirroring scoped-migration (fixes C1)

A small `Dialect` (enum or trait) renders the two things that actually differ:

| concern | Postgres | SQLite |
|---|---|---|
| placeholder | `$1, $2, …` (positional, 1-based) | `?` (or `?NNN`) |
| set membership | `col = ANY($1)` / `col != ALL($1)` | `col IN (?, ?, …)` / `col NOT IN (?, ?, …)` |
| case-insensitive match | `ILIKE` | `LIKE` (sqlite `LIKE` is case-insensitive for ASCII) |
| nulls ordering | native `NULLS FIRST/LAST` | emulate via `CASE WHEN col IS NULL` |

The builder walks `FilterNode` once and asks the dialect for each token, exactly
as `scoped-migration` renders `{json}`/`{timestamptz}` per backend. One traversal,
both backends; adding a backend is a new dialect impl, not a second builder. The
set-membership row shows why this is not cosmetic: pg binds the whole array as one
parameter, sqlite must expand to N placeholders — the builder owns that difference
so consumers never see it.

### Canonical input is `FilterNode`; the string DSL is an optional front end (fixes C3)

`awaken-query` compiles from the structured `FilterNode`. The speak2app string
grammar is offered behind a `dsl` feature as `parse_filter_dsl(&str, &QuerySchema)
-> Result<FilterNode, _>` that produces the **same** AST and runs the **same**
`QuerySchema::validate`. It is for CLI / saved views / expert search only, and it
has no SQL path of its own — it funnels into `FilterNode` and through the one
builder, so it cannot bypass the allowlist or column map. This is ADR-0003's
"string DSLs are allowed as optional parsers into the same `FilterNode` AST",
made concrete.

### Column mapping is mandatory and total (security, fixes C2's reuse)

The builder never emits a field name from the request directly. It takes a
`FieldMap` (logical field → physical column / SQL expression) and:

- emits only mapped columns; an unmapped field is an error, not a passthrough;
- the column side of the map is **author-controlled**, never request-derived, so
  there is no path from user input to an identifier — only to a bound parameter;
- carries over `s2a-query-dsl`'s value guards (`MAX_ARRAY_SIZE`,
  `MAX_STRING_LENGTH`) as configurable bounds checked **before** building.

`FilterOperator` is `awaken_api_contract::FilterOperator` (C2): one operator enum
in the base tier, no `s2a-types` dependency, no fork.

### Keyset cursors reuse the contract's opaque-cursor convention

`awaken-query` provides the keyset machinery the contract crate intentionally
omitted: from a sort spec + the last row's values, build the compound row-value
`WHERE` (the proven `build_cursor_condition` shape — forward/backward, primary key
appended for total order) and encode/decode the opaque token. The token stays
**opaque** per ADR-0003 and the cursor must be self-describing enough to detect a
sort/schema mismatch and fail closed rather than silently skip rows. This is
*one* cursor mechanism shared with `CursorPage<T>`, not a second convention.

### Grouped pagination — designed fresh (fixes C4)

The genuinely new surface. The shape oversight-next needs:

- **group key**: a `group_by` over an allowlisted field (validated by the same
  `QuerySchema`; a `Groupable` marker on the field, distinct from filterable /
  sortable, so a column must be *explicitly* opted into grouping);
- **per-group total**: a windowed/aggregate count alongside the rows;
- **per-group cursor**: keyset pagination *within* each group, which means the
  cursor encodes `(group_key, intra-group keyset)` and the builder partitions the
  `ORDER BY` / keyset condition by the group key.

`awaken-query` owns the *fragment* generation for this (the `GROUP BY` /
`PARTITION BY`, the per-group `COUNT`, the partitioned keyset condition); the
response envelope (`GroupedPage<G, T>` carrying `{ group, total, cursor, items }`)
belongs in `awaken-api-contract` as a new contract type, because it is a wire
shape multiple products will share. Both halves land together so the feature is
usable end to end.

## Target API (illustrative, to pin the work)

```rust
// awaken-query
pub enum Dialect { Postgres, Sqlite }

pub struct FieldMap { /* logical field -> column expr; author-controlled */ }
impl FieldMap {
    pub fn from_schema(schema: &QuerySchema) -> Self;       // identity map, then override
    pub fn column(self, field: &str, column: &str) -> Self; // logical -> physical
}

pub struct SqlFragment {
    pub where_clause: Option<String>,   // "(col1 = $1 AND col2 = ANY($2))"
    pub order_by: Option<String>,       // "col1 DESC, col2 ASC"
    pub limit: u32,
    pub params: Vec<FilterParam>,        // bound values, in placeholder order
}

pub struct CompileOptions { pub max_array_size: usize, pub max_string_len: usize }

pub fn compile(
    query: &ListQuery,          // already validated by awaken-api-contract
    map: &FieldMap,
    dialect: Dialect,
    opts: &CompileOptions,
) -> Result<SqlFragment, QueryBuildError>;

// keyset cursor
pub fn encode_cursor(row: &[(&str, FilterParam)]) -> String;     // opaque
pub fn decode_cursor(token: &str) -> Result<CursorState, QueryBuildError>;
pub fn keyset_condition(sort: &[SortClause], state: &CursorState,
                        map: &FieldMap, dialect: Dialect, start_param: usize)
    -> Result<(String, Vec<FilterParam>), QueryBuildError>;

// grouping (C4)
pub fn compile_grouped(query: &GroupedQuery, map: &FieldMap,
                       dialect: Dialect, opts: &CompileOptions)
    -> Result<GroupedSqlFragment, QueryBuildError>;

// optional string front-end (feature = "dsl")
#[cfg(feature = "dsl")]
pub fn parse_filter_dsl(input: &str, schema: &QuerySchema)
    -> Result<FilterNode, QueryError>;   // -> same AST, same validation
```

Dependencies: `awaken-api-contract`, `serde`, `serde_json`, `thiserror`,
`base64` (cursor), optionally `schemars` behind `schema`. **No** storage driver,
**no** product crate, **no** s2a-* — this is what keeps it in the base tier.

## Non-goals (explicit stances)

- **No execution.** `awaken-query` returns fragments + params; the consumer binds
  and runs them. This is the line ADR-0003 drew, kept: connection, transaction,
  `FROM`, projection, and RLS scope are product-specific.
- **No RLS / authorization.** The allowlist prevents column *discovery*; it is not
  a row-security boundary. Tenancy predicates are the consumer's, added to the
  `WHERE` outside this crate.
- **No relationship `include` / joins (initially).** `s2a-query-dsl`'s
  `include.rs` is closer to product graph shape; defer until a second consumer
  needs it (rule of three), rather than porting speculatively.
- **No ORM / schema modeling.** It compiles a query against a caller-supplied
  column map; it does not know the table.

## Security checklist (carried from s2a-query-dsl, enforced here)

1. **Every value is a bound parameter** — never string-interpolated. The dialect
   only ever emits placeholders for values.
2. **Every field is mapped or rejected** — identifiers come from the
   author-controlled `FieldMap`, never from request text. Unmapped ⇒ error.
3. **Bounds before build** — array length and string length checked against
   `CompileOptions` before any SQL is produced.
4. **One validation path** — the string DSL and structured input both pass
   `QuerySchema::validate`; there is no second, unguarded route to the builder.
5. **Opaque, self-checking cursors** — a cursor whose encoded sort/schema does not
   match the current query fails closed.

## Migration path

1. **Re-home, don't fork.** Port the filter-build, sort-build, and
   cursor-condition logic onto `awaken_api_contract::FilterNode` /
   `FilterOperator` / `SortClause`; drop `s2a-errors` / `s2a-types` (C2).
2. **Introduce `Dialect`** and route every placeholder / set-op / nulls decision
   through it; add the SQLite rendering beside the existing Postgres one (C1).
   Mirror the [`scoped-migration`](scoped-migration.md) token+dialect pattern.
3. **Land grouped compilation** (`compile_grouped`) + the `GroupedPage` envelope
   in `awaken-api-contract` — the oversight-next driver (C4).
4. **Feature-gate the string DSL** as a front-end parser into `FilterNode` (C3);
   keep bracket/structured input canonical.
5. **Adopt in oversight-next** (sqlite + pg) as the first new consumer, then
   migrate speak2app off `s2a-query-dsl` to this crate — the second consumer that
   satisfies the rule of three and unlocks the promotion ADR (ADR-0004).
