---
description: Start new work spec-first with tests and vertical slices
---

# Spec-driven start

Goal: $ARGUMENTS

1. **Clarify** - Restate the goal in one sentence. If requirements are missing, list open questions and stop (use `/discovery` or `/brainstorm` first).
2. **Spec** - Write `spec/<slug>.md`: behavior, numbered acceptance checks, and explicit out-of-scope.
3. **Tests first** - Add or sketch failing tests that encode each acceptance check before production code.
4. **Slice** - Break work into vertical slices in `spec/<slug>-plan.md`. No horizontal-only phases (schema-then-API-then-UI). Each slice owns a thin path to a demonstrable outcome.
5. **Build** - Implement one slice at a time. Run tests after each slice. Update the spec when behavior drifts.
6. **Gate** - Do not merge implementation until the spec checks are demonstrated. Pause for human review between spec, plan, and first code unless the user already approved.

Keep IDs traceable (`AC-001` in spec matches tests). Do not invent stakeholders or dates; mark unknowns `TBD`.