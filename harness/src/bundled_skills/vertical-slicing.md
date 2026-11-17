---
name: vertical-slicing
description: Deliver one user-visible slice end to end before widening scope
---

# Vertical slicing

Use this when planning or implementing a feature. A slice is the thinnest path from input to outcome a user can try.

## Rules

1. Name the user outcome in one line. If you cannot demo it, the slice is too wide.
2. Pick one entry point (CLI flag, route, screen). Do not add a second until the first works.
3. Implement in order: data model or contract, core logic, persistence or IO, then UI or message.
4. Ship with one test that proves the outcome. Widen only after that test is green.
5. Defer cross-cutting work (logging polish, admin UI, bulk export) to a later slice list.

## Anti-patterns

- Horizontal layers with nothing callable at the end.
- Feature flags that hide half-built flows with no test.
- Shared abstractions before two call sites exist.