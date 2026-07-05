# Rust CLI Engineering

Use this skill for Rust command-line, TUI, process, filesystem, packaging, or cross-platform terminal work.

## Tool workflow

- Use `read_file` on `Cargo.toml`, relevant modules, and tests before editing.
- Use `grep_files` for symbol discovery and `find_files` for module layout.
- Use `run_command` for `cargo fmt`, `cargo test`, `cargo check`, and `cargo clippy`.
- Use `edit_file` for surgical changes; use `write_file` for new modules or tests.

## TUI rules

- Alternate screen behavior must emit enter and leave sequences in interactive mode.
- Raw mode must be restored on exit, including error paths.
- Composer rendering must not steal the output cursor from streaming assistant text.
- Key handling should use terminal events, not line-buffered stdin, when raw mode is active.
- Shift+Tab is commonly reported as backtab; handle it separately from Tab completion.
- PTY tests are preferred for alternate screen and raw input behavior.

## Rust quality

- Avoid panics on user-controlled input and terminal dimensions.
- Keep JSON payloads structured with `serde_json::json!`.
- Clip or truncate large tool/hook outputs before storing or rendering them.
- Make clippy happy under `-D warnings`.
