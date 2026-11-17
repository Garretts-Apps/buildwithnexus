---
description: Spec-driven rebuild in vertical slices with old and new paths in parallel
---

# Rebuild

Scope: $ARGUMENTS

1. **Measure pain** - List duplication, dead branches, and complexity hotspots in scope only. Skip dependency and build trees. Cite paths you read.
2. **Spec first** - Write a short behavior spec (`spec/rebuild-behavior.md`): inputs, outputs, invariants. Turn existing tests into the contract; add gaps only where behavior is undocumented.
3. **Slice plan** - Order vertical slices in `spec/rebuild-plan.md`. Each slice must keep old and new entry points working side by side until verified.
4. **Per slice** - Implement the slice, run the spec tests, quote results. Do not delete the old path in the same commit.
5. **Retire** - Remove the old path only after the slice passes and the user confirms or already asked for full migration.

Stop for approval before destructive deletes or wide refactors.