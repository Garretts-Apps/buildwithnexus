# Frontend UX

Use this skill for web UI, TUI, CLI UX, onboarding, forms, dashboards, and user-facing flows.

## Workflow

1. Use `list_tree`, `grep_files`, and `read_many_files` to identify framework and design conventions.
2. Prefer existing components and styles.
3. For CLI/TUI UX, verify key handling, resize behavior, scrollback, alternate screen, and visible feedback.
4. Use `run_command` for builds/tests and screenshot tooling only if the project already has it.

## Standards

- Build the actual usable workflow, not a placeholder page.
- Keep controls predictable and dense enough for repeated use.
- Avoid hidden state: important hooks, tools, subagents, errors, and queued input need visible evidence.
