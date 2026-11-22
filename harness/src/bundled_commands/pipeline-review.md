---
description: Review CI/CD definitions for security, reliability, and least privilege
---

# Pipeline review

Scope: $ARGUMENTS

1. Detect platforms present: Azure Pipelines (`azure-pipelines.yml`, `*.yml` templates) and/or GitHub Actions (`.github/workflows/`).
2. For each platform, review: secrets in YAML, pinned action/task versions, `pull_request_target` and fork risks, OIDC vs long-lived credentials, permissions blocks, environments and approvals, test-before-deploy ordering, timeouts, caching, concurrency, and required checks.
3. Report findings by severity with file path and line. Propose concrete YAML fixes when asked.
4. Delegate a large diff to the read-only `pipeline-reviewer` helper when useful.
5. Do not rename or apply service connections, variable groups, or org secrets.