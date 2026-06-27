# ADR-0003: Shared API contract primitives

- **Status:** Proposed
- **Implementation:** in-progress
- **Date:** 2026-06-27

## Context

Multiple product lines expose HTTP APIs, generated clients, CLI transports, and
web clients. Their current API mechanics overlap but are implemented in
different places:

- typed non-2xx error payloads and `x-request-id` propagation,
- opaque cursor pagination on mutable list surfaces,
- offset/limit envelopes on existing admin and compatibility APIs,
- bracket filters, sorting, and explicit field/operator allowlists.

These are generic API mechanisms, not product domain objects. They are needed by
multiple components and should not force one product to depend on another.

## Decision

We will add `awaken-api-contract` as a foundation crate for shared API wire
mechanisms:

1. an RFC 9457 Problem Details error shape, `application/problem+json`, and
   request-id header constant,
2. protocol-native projections for AI SDK UI data-stream errors and AG-UI
   `RUN_ERROR` events,
3. pagination request/response primitives for cursor, page-number, and offset
   modes,
4. list-query parsing for bracket filters, sorting, and pagination parameters,
5. per-resource filter and sort allowlist validation,
6. generic auth requirement and credential-reference DTOs that never carry raw
   credential material.

The crate will not depend on an HTTP framework, storage driver, generated-client
tool, product model, authorization layer, RLS implementation, or database query
executor. Product crates must still declare their own resource DTOs, route
authorization, projection semantics, cursor encoding, credential storage, token
refresh, and query execution.

## Consequences

API consumers can converge on one error, pagination, sorting, and filtering
shape without coupling to a product repository. Error responses follow the
standard Problem Details members (`type`, `title`, `status`, `detail`,
`instance`) with stable extension members for application `code`, `request_id`,
structured `details`, and field-level `errors`. Read-model APIs can reject
unsupported fields and operators before a storage executor sees the query.
Streaming adapters can expose errors in the native shape expected by AI SDK UI
and AG-UI clients while retaining the original structured `ApiError` for
generated clients, audit correlation, and CLI output.

The harder part is migration: existing APIs have resource-specific response
envelopes and mixed pagination conventions. This ADR does not require a
flag-day rewrite. New list/read-model APIs should prefer the neutral
`{ items, cursor }` envelope, while legacy APIs may keep named fields such as
`{ issues, cursor }` until their generated clients migrate. Consumers can adopt
the crate first at the adapter boundary and then migrate individual list
endpoints to the canonical `page[...]`, `filter[...]`, and `sort` parameters.
Cursor pagination remains preferred for mutable projections and event/history
streams; page-number and offset modes are supported for stable catalogs,
admin tables, compatibility endpoints, and integrations that need random page
access.

String filter DSLs are allowed as optional parsers into the same `FilterNode`
AST, primarily for CLI and expert UI search. They are not the primary public
HTTP syntax because bracket filters are clearer for OpenAPI, generated clients,
field-level validation, and audit trails.

## Alternatives considered

- **Keep API primitives in each product:** rejected because the same mechanics
  are already repeated across products and clients.
- **Move a full query executor to foundation:** rejected because database paths,
  authorization scope, RLS, and projection semantics are product-specific.
- **Store OAuth/API tokens in foundation DTOs:** rejected because foundation API
  contracts must not serialize credential material. Foundation may model auth
  requirements, credential handles, and narrowly-scoped opaque handshake-material
  carriers under ADR-0001's amendment, but secret acquisition, refresh,
  persistence, authority, and permission checks stay in the consumer.
- **Force every list response to `{ items, cursor }`:** rejected because existing
  generated clients benefit from named fields such as `issues` or `runs`. The
  foundation crate makes `{ items, cursor }` the default for new APIs but does
  not force a flag-day migration for existing endpoints.
