---
description: Add tests for uncovered lines from a coverage report
---

# Coverage gaps

Report: $ARGUMENTS

1. Read the coverage artifact the project already produces.
2. List the highest-value uncovered branches, not every line.
3. Add tests that would fail if behavior regressed. Quote before/after coverage.
4. Do not chase 100% on generated or trivial code.