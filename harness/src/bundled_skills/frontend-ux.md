# Frontend UX

Use this skill for web UI, TUI, CLI UX, onboarding, forms, dashboards, and user-facing flows.

## Workflow

1. Use `list_tree`, `grep_files`, and `read_many_files` to identify framework and design conventions.
2. Prefer existing components and styles.
3. For CLI/TUI UX, verify key handling, resize behavior, scrollback, alternate screen, and visible feedback.
4. For standalone browser games, canvas demos, prototypes, or simple static pages, load `/static-app` and publish a runnable HTML artifact.
5. Use `start_server` for local dev servers; call `wait_for_url` to prove readiness; inspect `read_server_log` for failures; use `open_browser` when a preview is useful.

## Standards

- Build the actual usable workflow, not a placeholder page.
- For static browser deliverables, ship a runnable artifact rather than code pasted into chat.
- Keep controls predictable and dense enough for repeated use.
- Avoid hidden state: important hooks, tools, subagents, errors, and queued input need visible evidence.
