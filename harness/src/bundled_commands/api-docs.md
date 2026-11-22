---
description: Document a public HTTP or library API from the code that defines it
---

# API documentation

Scope: $ARGUMENTS

1. Find routers, handlers, OpenAPI specs, or exported types that define the public surface.
2. For each endpoint or entry point: method/path or symbol, auth, request shape, response shape, and one example.
3. Call out breaking vs stable fields. Do not invent endpoints.
4. Prefer the format the repo already uses (OpenAPI, README section, or typedoc). If none exists, write markdown under `docs/api/`.