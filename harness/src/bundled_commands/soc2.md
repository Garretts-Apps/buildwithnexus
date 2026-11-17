---
description: Repo readiness table against public SOC 2 Trust Services Criteria (not an audit)
---

# SOC 2 readiness

Scope: $ARGUMENTS

This is a **readiness check from repository evidence only**, not a SOC 2 Type II audit or certification.

1. Ask which Trust Services Categories apply (Security is always in scope; add Availability, Confidentiality, Processing Integrity, Privacy as named).
2. Gather evidence only from the repo: access control in code and config, secret handling, logging, change control (branch protection hints, PR templates, CI), dependency and vulnerability checks, backup/retention docs, TLS and encryption settings in config or infra-as-code.
3. Output a gap table: **Criterion (TSC topic)** | **Evidence (path or config)** | **Gap** | **Suggested fix**.
4. Label rows you cannot verify from the repo as "needs operator evidence" (policies, ticketing, cloud console).
5. Do not invent control IDs, auditor names, audit windows, or cloud account names.