---
description: Run the security-review skill on a change or path
---

# Security review

Scope: $ARGUMENTS

1. Load the `security-review` skill and follow it.
2. For a new feature surface, also run `/threat-model` on the same scope.
3. Delegate a read-only pass to the `security-check` agent when the diff is large.
4. Report by severity with path and line. Do not edit unless the user asked to fix.