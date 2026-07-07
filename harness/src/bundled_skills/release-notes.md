# Release Notes

Use this skill when writing changelogs, release notes, migration notes, npm package descriptions, or PR summaries.

## Workflow

1. Use `run_command` with `git log`, `git diff`, and `git status`.
2. Use `read_file` for package metadata and README changes.
3. Group notes by user-visible areas: Added, Changed, Fixed, Removed, Security.
4. Include upgrade notes and compatibility warnings.
5. Use `write_file` to update changelog/release artifacts when asked.

## Standard

- Do not list internal refactors unless they matter to users.
- Mention version numbers and package names exactly.
- Include verification commands that passed.
