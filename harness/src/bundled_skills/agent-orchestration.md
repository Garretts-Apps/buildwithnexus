---
name: agent-orchestration
description: Split work across helpers with narrow prompts and merged results
---

# Agent orchestration

Use this when delegating to helpers or subagents.

## Splitting work

1. One helper gets one outcome. Put files, constraints, and done-criteria in the prompt.
2. Prefer read-only helpers for review, research, and security passes.
3. Cap parallel helpers at what the runtime allows. Independent read-only tasks first; writers run serially.
4. Parent agent merges outputs: dedupe, resolve conflicts, cite which helper found each item.

## Prompt shape

- Goal (one line)
- Allowed tools or "read only"
- Paths in scope and paths off limits
- Output format (bullets, table, or patch plan)

## Failures

If a helper errors or times out, say what was not checked. Do not pretend full coverage.