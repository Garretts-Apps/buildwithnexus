---
description: Run dependency vulnerability audits for this repo's ecosystem
---

# Dependency audit

Scope: $ARGUMENTS

1. Detect manifests (`Cargo.lock`, `package-lock.json`, `pnpm-lock.yaml`, `pyproject.toml`, `go.sum`, etc.).
2. Run the audit tool the project already documents. If none, use: `cargo audit`, `npm audit --audit-level=high`, or `pip-audit` when the matching tool exists.
3. Summarize reachable vs dev-only findings. Propose upgrades one package at a time when fixes are requested.
4. Do not bump major versions without a named test plan.