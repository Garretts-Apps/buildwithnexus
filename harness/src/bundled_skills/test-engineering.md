# Test Engineering

Use this skill for adding tests, debugging failing checks, designing test plans, or improving coverage.

## Workflow

1. Use `grep_files` to find existing tests for the affected behavior.
2. Use `read_many_files` to compare implementation and nearby tests.
3. Add focused regression tests before or with the fix.
4. Use `run_command` to run the narrowest relevant test first, then the broader suite.
5. Record multi-step test work with `todo_write`.

## Test strategy

- Cover the behavior that failed, not incidental implementation.
- Prefer deterministic tests over sleeps and network calls.
- For TUI behavior, use PTY tests when possible.
- For package/deployment behavior, test package contents or build output, not just source state.
