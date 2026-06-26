# API contract primitives

`awaken-api-contract` is the foundation crate for API wire mechanisms that are
shared below product code:

- RFC 9457 Problem Details responses: `application/problem+json`
- request correlation header: `x-request-id`
- AI SDK UI stream projection: `{ "type": "error", "errorText": "..." }`
- AG-UI run-error projection: `{ "type": "RUN_ERROR", "message": "...", "code": "..." }`
- pagination: `page[size]` plus `page[cursor]`, `page[number]`, or `page[offset]`
- list query model: `filter[field]`, `filter[field][op]`, `sort`
- filter and sort allowlists per resource/read model
- credential references for connection auth; never raw secrets

The crate does not define workspace, issue, run, credential, or other product
objects. It also does not contain authorization, RLS, database translation, or
an axum adapter. Those belong in the consumer because they depend on route
scope, storage layout, and product policy.

## Query shape

Canonical list endpoints should accept:

```text
GET /api/resources
  ?filter[project_id]=019ede...
  &filter[completed_at][gt]=2026-06-26T18:12:25Z
  &filter[category][in]=done,accepted
  &sort=-completed_at,issue_id
  &page[size]=50
  &page[cursor]=...
```

Pagination supports three modes:

- cursor/keyset: `page[size]=50&page[cursor]=opaque`
- page number: `page[size]=50&page[number]=3`
- offset: `page[size]=50&page[offset]=100`

Cursor pagination is preferred for mutable read models and event/history
surfaces. Page-number or offset pagination is acceptable for stable catalog
views, admin tables, compatibility endpoints, and integrations that need random
page access. A request must not mix cursor, page-number, and offset parameters.

Each endpoint declares its own `QuerySchema`:

```rust
use awaken_api_contract::{
    CursorPageConfig, FilterOperator, QuerySchema, SortClause,
};

let schema = QuerySchema::new()
    .allow_filter_field("project_id", [FilterOperator::Eq, FilterOperator::In])
    .allow_filter_field(
        "completed_at",
        [
            FilterOperator::Gt,
            FilterOperator::Gte,
            FilterOperator::Lt,
            FilterOperator::Lte,
        ],
    )
    .allow_filter_field("category", [FilterOperator::Eq, FilterOperator::In])
    .allow_sort_field("completed_at")
    .allow_sort_field("issue_id")
    .with_default_sort(vec![
        SortClause::desc("completed_at"),
        SortClause::asc("issue_id"),
    ])
    .with_page_config(CursorPageConfig::new(20, 100));
```

The parser validates fields and operators against this schema. Unknown fields are
errors, not no-ops, so clients cannot discover or exploit storage columns by
guessing names.

## Response shape

New list/read-model APIs should use the neutral `CursorPage<T>` shape:

```json
{
  "items": [],
  "cursor": "opaque-next-page-token"
}
```

This is the default because generated clients, UI list loaders, CLI formatters,
pagination helpers, and cache keys can share one shape across resources. The
item type carries the domain meaning; the envelope should not need a
resource-specific field name.

Existing APIs may keep named item fields such as `{ "issues": [], "cursor": null
}` while they migrate. That is a compatibility exception, not the preferred
shape for new surfaces.

The cursor is always opaque and only present when another page exists.

`NumberedPage<T>` is available when a surface intentionally uses page-number
pagination. Offset responses may use the same neutral `items` envelope plus
explicit `offset`/`size` metadata in the product DTO.

## Filter syntax

The shared model is the `FilterNode` AST plus per-resource `QuerySchema`
allowlist. Wire syntaxes are parsers into that AST.

Canonical HTTP APIs should use bracket query parameters:

```text
filter[completed_at][gt]=2026-06-26T18:12:25Z
filter[category][in]=done,accepted
```

This is less elegant than a compact DSL, but it is easier to represent in
OpenAPI, generated clients, browser URL builders, validation errors, audit logs,
and per-field allowlist checks.

A string DSL such as:

```text
filter=completed_at > "2026-06-26T18:12:25Z" and category in ("done", "accepted")
```

is useful for CLI, admin consoles, saved views, and expert search boxes. If a
consumer supports it, the DSL parser must produce the same `FilterNode` AST and
must run through the same `QuerySchema` validation. It must not bypass
field/operator allowlists or authorization.

## Error shape

Consumers should map non-2xx responses to the shared `ApiError` problem detail
shape and send it with `application/problem+json`. The transport status and the
body `status` field must match.

```json
{
  "type": "urn:api-contract:problem:validation-failed",
  "title": "Validation failed",
  "status": 422,
  "detail": "Request validation failed",
  "code": "validation_failed",
  "request_id": "req_123",
  "errors": [
    {
      "path": "filter.completed_at",
      "code": "invalid_operator",
      "message": "operator is not allowed for field",
      "source": { "parameter": "filter[completed_at][contains]" }
    }
  ]
}
```

The standard Problem Details members (`type`, `title`, `status`, `detail`,
`instance`) provide broad interoperability. `type` is a stable URI reference,
not a free-form enum string; use `code` for short machine branching such as
`validation_failed`. The stable extension members (`code`, `request_id`,
`details`, `errors`) give generated clients, CLIs, and observability tools a
predictable application contract.

Streaming adapters should project the same envelope into the protocol-native
error shape rather than inventing another error object:

- AI SDK UI data streams set `x-vercel-ai-ui-message-stream: v1` and emit an
  `AiSdkErrorPart` when the stream itself fails. Because the protocol's error
  part only carries display text, adapters may also emit `data-api-error`
  using `AiSdkDataPart<ApiError>` when a client needs the stable code,
  `request_id`, and structured details.
- AG-UI adapters emit `AgUiRunErrorEvent`, mapping
  `ApiError::detail` or `ApiError::title` to `message` and `ApiError::code` to
  `code`, with the full problem detail in `rawEvent` so clients can recover the
  original structured error.

This keeps REST, generated clients, AI SDK streams, and AG-UI streams anchored
to the same internal error taxonomy without forcing those external protocols to
accept a non-native envelope.

## Connection auth

Connections that need API tokens, OAuth tokens, client secrets, or similar
credential material should share the generic auth model but never serialize the
secret itself:

```json
{
  "kind": "credential",
  "requirement": {
    "scheme": { "kind": "oauth2", "flows": ["authorization_code"], "scopes": ["repo:read"] },
    "scopes": ["repo:read"]
  },
  "credential": { "id": "cred_123", "version": "7" }
}
```

The `CredentialRef` is resolved by the product's credential store or runtime
secret resolver at the egress boundary. The foundation contract only describes:

- what scheme a connection requires,
- which scopes or key names are expected,
- which credential handle should be resolved.

OAuth refresh, token exchange, API-key injection, redaction, rotation,
authorization to use a credential, and audit logging remain product/runtime
responsibilities. This keeps the model reusable without putting secrets or
identity policy in foundation.

## TypeScript contract generation

The recommended chain for TypeScript clients is:

1. Derive JSON Schema for Rust DTOs with `schemars::JsonSchema`.
2. Export backend contract artifacts:
   - `contracts/model-schemas.json` from a model crate schema registry.
   - `contracts/openapi.json` from the server route/OpenAPI registry.
3. Generate TypeScript domain types from `model-schemas.json` with
   `json-schema-to-typescript`.
4. Generate API operation types from `openapi.json` with `openapi-typescript`.
5. Generate a small typed transport/client from the same OpenAPI document:
   operation id, method, path template, path params, query params, request body,
   response body.
6. Optionally generate AJV validators from `openapi.json` for dev-time request
   and response drift checks.

For a foundation consumer, this pattern should import the shared contract
types (`ApiError`, `PaginationRequest`, `CursorPage<T>`, `AuthRequirement`,
etc.) into product DTOs, derive schema for those product DTOs, and let OpenAPI
drive the generated TypeScript client. Generated frontend code should depend on
operation ids and generated request/response types, not hand-written endpoint
strings.
