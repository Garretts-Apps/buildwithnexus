---
description: Measure entropy hotspots and apply small verified cleanups
---

# Entropy

Scope: $ARGUMENTS

1. **Measure** - Run `python harness/bundled_workflow/scripts/measure_entropy.py --report` from a source checkout (or copy the script locally). It ranks large files, repeated line blocks, and recent git churn. Skip vendor and build trees.
2. **Read** - Add manual review for dead exports, unused imports, and parallel patterns the script cannot see. Cite paths you opened.
3. **Rank** - Merge script output with review into blocker / should-fix / note. Prefer behavior-preserving reductions (dedupe helpers, drop dead code, simplify wrappers).
4. **Plan or fix** - Default: report only. If the user asked to clean up, change one ranked item at a time, run the project tests after each, and quote output.
5. **Stop** - Revert on test failure. No wide refactors in one commit.

For replacing an old implementation with a spec-driven migration, use `/rebuild` instead.