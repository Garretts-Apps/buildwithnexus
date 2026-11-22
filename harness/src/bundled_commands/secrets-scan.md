---
description: Scan the tree for committed secrets and risky assignments
---

# Secrets scan

Scope: $ARGUMENTS

1. Run `python harness/bundled_workflow/scripts/scan_secrets.py --report` on the scope (or copy the script locally).
2. If the repo already runs a secret scanner in CI, use that command instead and quote output.
3. List hits by severity. Redact values in the report; show path and line only.
4. Do not commit fixes that rotate production credentials without the owner's process.