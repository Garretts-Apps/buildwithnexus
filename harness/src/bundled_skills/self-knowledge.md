# Self-Knowledge

Use this skill whenever the user asks how buildwithnexus works, reports a bug in buildwithnexus, asks you to repair or extend buildwithnexus itself, or asks about TUI behavior, hooks, tool calls, subagents, skills, memory, sessions, npm packaging, or local model setup.

## What buildwithnexus is

buildwithnexus is a Rust CLI agent harness. The npm package is a thin Node launcher that resolves or builds the Rust binary; Next.js is not part of the CLI runtime.

Core Rust modules:

- `src/lib.rs`: CLI routing, interactive REPL, slash commands, session lifecycle.
- `src/tui.rs`: alternate screen, raw-mode input, persistent composer, Shift+Tab mode cycling, typeahead, streaming renderer.
- `src/agent.rs`: PLAN, BUILD, and BRAINSTORM orchestration, tool loops, subagents, context compaction, permission gates.
- `src/tools.rs`: built-in tool definitions and implementations.
- `src/hooks.rs`: Claude Code-compatible lifecycle hooks.
- `src/trace.rs`: navigable trace ledger for hooks, tools, skills, and subagents.
- `src/config.rs`: settings, memory, skills, commands, hooks discovery, provider presets.
- `src/provider.rs`: provider protocols, completions, streaming, tool-call parsing.
- `src/session.rs`: saved session history, resume, continue.
- `src/checkpoint.rs`: edit checkpoints and undo.
- `src/onboarding.rs`: init/setup flow, including local model choices.
- `src/report.rs`: human and JSON event reporting.

## Actual built-in tools

The agent can call these tools through model tool calls:

- `read_file`
- `read_many_files`
- `list_dir`
- `list_tree`
- `file_info`
- `find_files`
- `grep_files`
- `write_file`
- `edit_file`
- `multi_edit`
- `apply_patch`
- `create_dir`
- `move_path`
- `remove_path`
- `run_command`
- `todo_write`
- `todo_read`
- `create_docx`
- `finish`
- `save_memory`
- `fetch_url`
- `web_search`
- `spawn_subagent` in BUILD mode

The model should use these tools directly. It should not claim access to unavailable external tools unless it invokes them through `run_command` and verifies they exist.

## Repair workflow

When fixing buildwithnexus itself:

1. Reproduce with the local Rust binary first:
   `cargo run --manifest-path harness/Cargo.toml -- <args>`
2. Prefer PTY tests for TUI behavior because alternate-screen, raw mode, cursor save/restore, Shift+Tab, and composer behavior require a terminal.
3. If Computer Use cannot operate the Terminal app, say that explicitly. Do not claim a visual Terminal E2E was completed.
4. Keep fixes in Rust unless the issue is specifically the npm launcher or docs.
5. Run:
   `cargo test --manifest-path harness/Cargo.toml`
   `cargo clippy --manifest-path harness/Cargo.toml --all-targets --locked -- -D warnings`
6. For npm install issues, verify the package contents with:
   `npm pack --dry-run`

## TUI expectations

The interactive UI should:

- Enter the terminal alternate screen on launch and leave it on exit.
- Keep shell scrollback hidden while the TUI is active.
- Use raw-mode key handling for Shift+Tab, Tab completion, history, editor shortcuts, and multiline input.
- Keep a persistent composer at the bottom row.
- Preserve output cursor position when rendering queued typeahead into the composer.
- Surface trace rows when tools, hooks, skills, and subagents are used.
- Let the user inspect trace details with `/trace` and `/trace <id>`.

## Hook and trace expectations

Hooks should capture:

- Event name: `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`.
- Trigger payload, including tool name/input where applicable.
- Hook source: home or project.
- Matcher.
- Command or script path.
- Exit code, stdout, stderr.
- Deny/allow decisions when present.

Tools and subagents should capture:

- Tool name, input, result, error state, phase, and depth.
- Subagent task, role, isolate setting, working directory, and result.
- Loaded skills and Agents.md context with enough preview to debug why the model had that context.

## Packaging rules

- Bump `package.json`, `harness/Cargo.toml`, and `harness/Cargo.lock` together for a release.
- Ensure commits are authored as `geaglin09 <geaglin09@gmail.com>`.
- The npm package should not depend on Next.js.
- Do not ask users to retest a version they already reported as broken; cut a new version.
