# Changelog

All notable changes to `buildwithnexus` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.11.3] - 2026-07-07

Small-model BUILD reliability, from a real qwen2.5-coder-1.5b session that
looped instead of building a page.

### Fixed
- **Small models no longer stall on the clarifying-question tool.** The
  `question` tool is dropped from the compact (local/small-model) tool set — a
  1.5B model was re-asking "use a framework?" endlessly instead of building. It
  now acts on sensible defaults; larger models and PLAN mode keep the tool.
- **Question answers now echo what you type.** The answer prompt was a
  multi-line string, which mis-positioned the alt-screen composer cursor and
  hid typed input. The question prints on its own line and the answer is read
  with a single-line prompt.
- **HTML artifacts that link a local stylesheet are rejected** with an
  actionable message (name the file, inline the CSS), mirroring the existing
  local-`<script src>` check; the too-small message now explicitly demands a
  single self-contained file with no external links.

[0.11.3]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.11.3

## [0.11.2] - 2026-07-07

Document generation, web-content quality, TUI, and small-model streaming
refinements — all additive fixes, no breaking changes.

### Added
- **Rich Word document generation.** `create_docx` now renders inline markdown
  (`**bold**`, `*italic*`, `` `code` ``) as real Word runs instead of literal
  asterisks, and converts markdown tables (`| col | col |`) into bordered Word
  tables with a bold header row. Emphasis is conservative — arithmetic like
  `5 * 3`, glob patterns, and `snake_case` are left untouched.
- **Structured web search.** `web_search` returns numbered `title / url /
  snippet` results (recovering the real target URL from DuckDuckGo's redirect
  wrapper) instead of the raw page run through an HTML stripper — far easier for
  small models to read. Falls back to the stripped text if the markup changes.

### Fixed
- **Numeric HTML entities in fetched content.** `strip_html` and the search
  parser now decode decimal/hex character references (`&#8217;`, `&#x2019;`,
  `&mdash;`), so fetched pages and snippets no longer show garbled curly quotes
  and dashes.
- **TUI markdown emphasis.** An unbalanced `` ` ``, `**`, or single `*` (e.g.
  `5 * 3`) no longer styles the rest of the line — the inline renderer only
  opens a style when a matching closer exists ahead and the marker flanks
  non-space text.
- **`<think>` reasoning leakage.** Reasoning models (DeepSeek-R1 distills, Qwen
  thinking variants) that emit `<think>…</think>` inline in content no longer
  leak that into the final answer or written files: it's stripped on the
  non-streaming parse path, and a `<think>`/`</think>` tag split across streaming
  chunks (routine with token-by-token local streaming) is now reassembled
  instead of leaking a partial marker.

[0.11.2]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.11.2

## [0.11.1] - 2026-07-07

### Fixed
- **Qwen2.5-Coder tool calls now work on llama.cpp/Ollama.** Small local coder
  models emit tool calls as `<tools>{…}</tools>` / `<tool_call>{…}</tool_call>`
  text in the message content rather than as native `tool_calls`. The text
  recovery parser only understood bare or ```json-fenced JSON, so these calls
  were treated as prose and the model never acted (an end-to-end run against
  qwen2.5-coder-1.5b produced no file). The parser now strips the XML tool tags
  and extracts a string-aware balanced JSON object, so a `content` field full of
  CSS `{ }` braces no longer truncates the parse. Also recovers a bare JSON
  object embedded after a leading sentence.

[0.11.1]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.11.1

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
