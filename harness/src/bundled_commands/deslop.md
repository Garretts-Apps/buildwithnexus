---
description: Sweep the tree for generated-code artifacts and fix them in small commits
---

# Deslop

Scope: $ARGUMENTS

1. Load the `deslop` skill and follow it.
2. If the scope contains `--report`, `report`, or `dry-run`, scan and stop. Do not edit or commit.
3. If the scope contains `--ci`, run the Unicode scanner from the skill and stop.
4. Otherwise report by severity, then fix in small commits with tests after each.
5. Include a Kept list for anything that looks intentional.