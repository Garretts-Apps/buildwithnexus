# Codebase Repair

Use this skill when the user asks to fix a bug, implement a feature, clean up a regression, or investigate failing behavior in a repository.

## Tool workflow

1. Start with `list_dir`, `find_files`, and `grep_files` to understand the codebase shape.
2. Use `read_file` before `edit_file` or `write_file`; edits to existing files are expected to be read first.
3. Prefer `grep_files` and `find_files` over shell search when they are enough.
4. Use `run_command` for project-native checks such as `cargo test`, `cargo clippy`, `npm test`, `npm run build`, `git diff`, and package-specific scripts.
5. Use `finish` only after implementation and verification are complete, or after explaining a real blocker.

## Repair discipline

- Keep changes scoped to the reported behavior.
- Match existing style before adding abstractions.
- Add or update tests when behavior changes.
- If a check fails, inspect the error and fix the cause rather than masking it.
- When a terminal/TUI bug is involved, use a PTY or real terminal path for validation where possible.
- If Computer Use cannot operate the Terminal app, say so plainly and provide local terminal commands for manual verification.

## Standard verification

For Rust CLI work, prefer:

- `cargo test --manifest-path harness/Cargo.toml`
- `cargo clippy --manifest-path harness/Cargo.toml --all-targets --locked -- -D warnings`

For npm package work, prefer:

- `npm pack --dry-run`
- installing from the local tarball in a temporary directory when package contents matter.
