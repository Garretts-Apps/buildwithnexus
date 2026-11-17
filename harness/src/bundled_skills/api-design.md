---
name: api-design
description: Shape stable public APIs with clear errors and versioning
---

# API design

Use this when adding or changing a public API (HTTP, RPC, or library exports).

## Contracts

1. Name resources and actions in nouns and verbs the caller already uses in the domain.
2. Return errors as machine-readable codes plus a short human message. Never only HTTP status.
3. Version breaking changes (path prefix, header, or major version). Document deprecations with a removal date.
4. Pagination: cursor or offset with a hard max page size. Include total only when cheap.
5. Idempotent writes: accept a client idempotency key or natural idempotency via PUT semantics.

## Security defaults

- Auth required unless explicitly public. Fail closed.
- Validate input at the boundary; reject unknown fields when the schema is strict.
- Never echo secrets in errors or logs.

## Documentation

Each public operation needs: inputs, outputs, auth, rate limits, and one worked example.