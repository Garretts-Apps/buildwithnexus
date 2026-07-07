# Code Review

Use this skill when reviewing changes, auditing a PR, or checking for regressions.

## Workflow

1. Use `run_command` with `git status`, `git diff`, and `git diff --staged`.
2. Use `read_file` for files where line context matters.
3. Prioritize findings by severity.
4. Mention tests that were not run or risk that remains.

## Findings standard

- Lead with bugs, security risks, data loss risks, broken UX, or missing tests.
- Cite file paths and exact behavior.
- Do not pad the review with style preferences.
- If no issues are found, say that clearly and list residual test gaps.
