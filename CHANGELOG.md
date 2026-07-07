# Changelog

All notable changes to `buildwithnexus` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.11.0] - 2026-07-06

Reliability, small-open-weight-model support, and TUI UX overhaul. Grounded in a
full-codebase review plus field research on what opencode / vtcode / aider /
cline ship for weak models.

### Added
- **Native Ollama protocol** (`/api/chat`): detects the model's real context
  window via `/api/show` and sets `num_ctx`, eliminating Ollama's silent
  front-of-prompt truncation — the single biggest measured quality loss for
  local models. Sends `repeat_penalty: 1.0` and a low default temperature.
  `…/v1` base URLs stay on the OpenAI-compatible path for backward compatibility.
- **Live colored diffs** rendered in the TUI as files are written and edited
  (`+`/`-` hunks with add/remove counts), replacing opaque tool-call lines.
- **Markdown rendering on every output path**, including streaming — headings,
  bold/italic, inline code, lists, and fenced **code blocks** now render instead
  of showing raw markdown.
- **Auto mode-switching**: a build/plan request made in BRAINSTORM escalates to
  the appropriate mode instead of only chatting about it.
- **"Act, don't explain" nudge**: an imperative task answered with prose-only
  instructions is pushed to actually use its tools.
- **Tiered lenient edit apply**: whitespace/indentation-tolerant matching
  auto-rescues near-miss edits from weak models; similarity matches remain
  diagnostic-only.
- Configurable `temperature`, `max_tokens`, and `context_tokens` settings.
- CI now runs `cargo fmt --check` and `cargo audit`.

### Changed
- `Reply` carries a normalized `stop_reason` across both wire protocols;
  `max_tokens` truncation now triggers a bounded continuation instead of
  processing a truncated response.
- Retry policy is status-code-based and covers HTTP 529, honors `Retry-After`.
- Loop guard counts only repeated errors/no-ops (threshold 3), nudges once, then
  stops honestly — legitimate re-reads no longer end sessions.
- Compaction can no longer sever `tool_use`/`tool_result` pairs, always pins the
  original task, and preserves the recent tail; context-overflow errors
  force-compact and retry.
- Verifier is wired to real tool-call and changed-file data and feeds violations
  back to the model.
- System prompt restructured: role/mode contract first, deduplicated,
  game/artifact guidance gated on task type, standing rules dump removed.
- Compact tool set now applies to all local-context models.
- Command output truncation keeps head **and** tail so failing-test summaries and
  exit codes survive; `run_command`/`python_tool` gain a 120s timeout.
- Ask-mode auto-allow list trimmed from 44 commands to 15 read-only ones.
- Rewrote `SECURITY.md` (it described the removed Python/NEXUS package) and
  corrected `README.md` drift.

### Fixed
- **Truncated streamed tool-call JSON no longer executes with empty input** — it
  surfaces as an invalid-arguments error fed back to the model.
- **Removed the auto-repair hijacks** that overwrote correct model output with
  literal task substrings and rewrote shell commands into file writes.
- **Removed the hardcoded "fallback canvas game"** that shipped unrelated code as
  a successful result; artifact rejections now report the exact reason and retry.
- **Artifact validator** no longer rejects valid apps over `...`/`todo`
  substrings; rejections quote the offending snippet and the exact rule.
- **Queued-message TUI deadlock** on edit/remove/consume during streaming.
- **`/undo` no longer truncates** binary or oversized files.
- Display-width-aware wrapping (emoji/CJK), atomic session saves, hook-execution
  watchdog with distinct failure codes, and edit-mismatch diagnostics that point
  at the closest near-match.
- Read-only command classifier hardened against `;`/`|`/redirection smuggling,
  `sed -i`, `find -delete`, and `git clean`; mutating file tools are confined to
  the working directory.

[0.11.0]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.11.0
