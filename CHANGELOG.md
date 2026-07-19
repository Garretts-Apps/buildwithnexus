# Changelog

All notable changes to `buildwithnexus` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.12.6] - Unreleased

### Fixed
- **All file writes in the tool layer are atomic.** `write_file`,
  `edit_file`, `multi_edit`, `create_docx`, artifact writes, and the editor
  tools wrote directly to the destination — a crash or power loss mid-write
  could leave a truncated file. Every content write now goes through
  same-directory temp + rename (which also can't split a file across
  filesystems), and the destination's permissions are copied onto the
  replacement, so editing a script no longer risks silently stripping its
  executable bit.

## [0.12.5] - 2026-07-19

### Added
- **Startup tips** — one rotating dim line under the banner: half real tips
  (Shift+Tab modes, Ctrl+V screenshots, /checkpoint), half personality
  ("bwn started faster than you read this sentence"). Jokes live here and
  only here — error messages stay strictly business.
- **The letsbeheroes skill collection** — eight bundled process skills
  (`/letsbeheroes`, `/hero-brainstorm`, `/hero-plan`, `/hero-execute`,
  `/hero-debug`, `/hero-ship`, `/hero-wait`, `/hero-subagents`) that encode
  the working discipline: brainstorm → plan → execute → debug → verify,
  plus condition-based waiting and subagent delegation.
- **Any OpenAI-compatible endpoint as a provider** — new `custom` preset for
  vLLM / TGI / LiteLLM / gateways: `/model` takes `<url> <model>` directly or
  walks through URL, optional `CUSTOM_API_KEY`, and model name. A configured
  key is never sent over plain HTTP to a non-loopback host.
- **Any OpenRouter model** — `/model org/model` (e.g. `meta-llama/…`,
  `google/gemini-2.5-pro`) routes to OpenRouter automatically.

### Fixed
- **Pending `/schedule` and `/loop` workflows survive restarts.** They lived
  only in process memory, so quitting, crashing, or resuming a session
  silently discarded them. Pending workflows now persist to
  `~/.buildwithnexus/workflows.json` (atomic writes, file removed when the
  queue is empty) and are restored at the next interactive launch with a
  visible "⟳ restored N scheduled workflows" notice. Loop iteration counts
  and next-fire times carry over; headless workflow subprocesses never touch
  the store.
- **Bare `/undo` now reverts the whole last agent turn.** After a partial
  multi-file edit (the agent changed three files and broke two, or Esc landed
  mid-batch), `/undo` used to restore only the single most-recent checkpoint —
  it looked like an undo while quietly leaving the other files changed. Bare
  `/undo` now restores every file the last agent turn touched, in the right
  order even when one file was edited several times; `latest` keeps the old
  single-checkpoint behavior.
- **Rapid multi-file batches no longer lose checkpoints.** Checkpoint ids
  were timestamp-only, so several writes in the same millisecond overwrote
  each other's snapshots on disk — some files in a fast batch were silently
  unrecoverable. Ids now carry a sequence number, which also makes restore
  ordering deterministic within a millisecond.
- **Malformed tagged tool calls are reprompted, not presented as answers.**
  When a local model emits a tool call as tagged text (`<tool_call>{…}`) and
  the JSON inside is broken or cut off, the agent loop previously treated the
  raw markup as the final answer and stopped. It now detects the failed tool
  intent, feeds the model one corrective message ("re-emit as exactly one
  JSON object…"), and only then gives up — bounded to a single retry so a
  model that can't produce valid JSON still terminates.
- **`/model` now swaps providers, not just the model string.** Picking an
  Ollama or OpenAI model while on Anthropic previously sent the new name to
  the old provider's API. The picker now maps every choice to the provider
  that serves it, walks you through a missing API key on the spot, checks
  that Ollama is reachable and actually has the model (with install/pull
  steps when not), and keeps the current model on any failure instead of
  reporting a successful swap that would break the next prompt.
- **"✓ hot-swapped" is only printed after live validation.** Every swap
  (except Ollama, which is validated via its API beforehand) runs a one-token
  probe through the real request path first — a rejected key, an unknown
  model name, or an unreachable server fails the swap on the spot with a
  targeted hint, instead of surfacing as a raw HTTP error on your next
  prompt.
- **Broken settings files are now diagnosed, never silently dropped.** A JSON
  typo in any settings file previously made the CLI act as if you'd never
  configured it — and first-run onboarding could then overwrite your config.
  Startup now prints one warning per unusable file with the exact line and
  column, refuses to re-onboard while broken files exist, and
  `buildwithnexus doctor` lists every settings file with its parse status.

## [0.12.4] - 2026-07-16

### Fixed
- **The first-run `[y/N]` consent prompt now actually waits on macOS.** Node
  keeps a TTY stdin in non-blocking mode, so the launcher's synchronous read
  threw `EAGAIN` and fell through to "native binary not found" before you
  could answer. The prompt now reads from a fresh blocking `/dev/tty` handle
  (falling back to stdin where `/dev/tty` doesn't exist, e.g. Windows).

## [0.12.3] - 2026-07-14

### Changed
- **Auto-update now defaults to notify-only.** New `auto_update` setting in
  `settings.json`: `"off"` (no check, no notices), `"notify"` (daily check,
  one-line startup notice, never installs — the default), `"install"` (the
  previous behavior: silent background `npm install -g`). `BWN_NO_AUTO_UPDATE=1`
  still works and caps `"install"` back to `"notify"`. A tool that edits files
  and runs commands should not change its own executable without being asked.
- **First-run binary download requires explicit consent.** When the platform
  package is absent, the launcher now asks `[y/N]` on a TTY, or requires
  `bwn --bootstrap` / `BWN_ALLOW_BOOTSTRAP=1` in non-interactive use — it no
  longer downloads automatically.

[0.12.3]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.3

## [0.12.2] - 2026-07-14

### Fixed
- **Bundle analyzers work now.** Added a `browser` field pointing at a
  dependency-free stub entry (`index.browser.js`) — tools like bundlephobia
  that webpack-bundle the package no longer fail on `child_process` and the
  platform binary packages the Node entry resolves. The stub exposes
  `{ version }` and throws clearly if the CLI surface is called in a browser.
- Publish workflow: the crates.io "already published" check queried the local
  workspace instead of the registry (`cargo info` resolves workspace members
  locally), silently skipping real publishes. It now asks the sparse index.

[0.12.2]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.2

## [0.12.1] - 2026-07-14

Supply-chain hardening: the npm install is now inert and auditable at a glance.

### Changed
- **Per-platform binary packages.** The prebuilt binary ships as five
  `buildwithnexus-<os>-<cpu>` packages selected automatically via
  `optionalDependencies` (the esbuild pattern). The main package is ~7 readable
  files with **no install scripts, no network code, no shell-outs (beyond
  spawning the CLI itself), no bundled sources, no eval** — supply-chain
  scanners have nothing to flag. Binaries are SHA-256-verified when packaged
  and carry build-provenance attestations.
- **Auto-update moved into the binary.** The daily npm-registry check and
  silent `npm install -g` refresh now run inside the CLI (background thread,
  never blocks startup) instead of the npm wrapper. `BWN_NO_AUTO_UPDATE=1`
  still disables installs; an update notice prints on the next launch.
- Installs with `--omit=optional` skip the platform binary; point `BWN_BIN`
  at a self-built binary (documented in the launcher's error message and at
  buildwithnexus.dev/docs/install).
- Added a `main` entry point (`index.js`) with a tiny programmatic API
  ({ version, binaryPath, run }) so bundle analyzers stop erroring on the
  bin-only package.

[0.12.1]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.1

## [0.12.0] - 2026-07-14

The Ferrari release: a full UI/UX overhaul — instant, multimodal, and clean.

### Added
- **Live slash-command autocomplete.** Typing `/` (or `@`, or a sub-argument)
  opens a popup above the composer with one-line descriptions for all built-in
  commands; ↑/↓ navigate, Tab/Enter accept, Esc dismisses. Removed the ghost
  commands (`/effort`, `/plugin`, `/marketplace`) that autocomplete offered but
  no handler implemented.
- **True multimodal input.** `Ctrl+V` pastes clipboard images (Wayland/X11/
  macOS/WSL) as attachments; `@clip.mp4` (and other containers) is parsed with
  ffmpeg/ffprobe into up to 8 evenly-sampled frames plus a metadata block.
  Both are gated on a per-model vision-capability check — text-only models get
  an explicit notice instead of silently dropped images.
- **Clickable files and links (OSC 8).** Markdown links and `⏺ edit/write`
  headers are terminal hyperlinks: click a path to open the file in the OS
  default app. The ANSI scanners learned OSC strings so links cost zero
  display columns.
- **npm auto-update.** A detached background process checks the registry at
  most once a day and silently installs newer versions; the next launch prints
  a one-line notice. Opt out with `BWN_NO_AUTO_UPDATE=1`. Startup never waits
  on the network.
- **Esc interrupts the agent** (with queued prompts auto-sending next turn),
  double-click word / triple-click line selection with a soft theme highlight
  and a "⎘ copied" footer flash, prefix-filtered ↑ history that preserves the
  in-progress draft, Ctrl/Alt+←→ word jumps, and mode-aware cursor shapes
  (accent bar / vim block / visual underline) hidden while the agent works.

### Changed
- **GitHub-grade diffs.** One renderer for edit previews, write previews, and
  applied changes: dual line-number gutter, background-tinted rows, word-level
  change emphasis on replacement pairs, hunk elision, and a 40-row cap.
  `NO_COLOR` keeps signed `- / +` text.
- **Bordered composer box** (opencode-style) with the spinner inside showing
  elapsed seconds and an interrupt hint; minimal banner (gradient wordmark +
  aligned model/cwd/mode rows) replacing the emoji-heavy boxed header.
- **Grouped, auto-aligned `/help`**, rendered markdown for `/verify`, rule
  violations, `/agents`, and `/memory` (no more raw `##`/`**`), human-readable
  verification statuses, and a consistent `✓ / ✗ / ⚠ / ⟳` message vocabulary.
- Removed fabricated telemetry: the hardcoded `est. cost` figure and the
  tok/s badge derived from character counts are gone; the context meter shows
  only real used/total tokens.

### Fixed
- Tool denials rendered dim-grey (now red); failed background workflows were
  announced in success-green; a raw `eprintln!` corrupted the alt-screen
  during `/resume`; HTTP retries were silent for up to 10s (now a visible
  `⟳ retrying` line); tool previews and the permission prompt could smear a
  multi-line command across the screen (capped at 80 cols, prompt legend on
  its own line).

### Performance
- **~215× faster streaming renders.** The renderer re-wrapped the entire
  transcript on every streamed chunk/keystroke/scroll; now each line wraps
  once (incremental cache), repaints are coalesced to ~60fps and applied as
  atomic frames (DEC 2026), and multi-line blocks (diffs, code, command
  output) paint in one repaint instead of one per row.
- Instant startup: dependency probes moved off the critical path; the screen
  paints chrome immediately so there is never a black frame.

[0.12.0]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.0

## [0.11.4] - 2026-07-07

Gemma local-model support, from a real gemma-2-2b-it session on llama.cpp.

### Added
- **Parse Gemma's `tool_code` tool-call format.** Gemma emits tool calls as a
  ```` ```tool_code ```` fenced Python call — `write_file("/p", """…""")`,
  sometimes wrapped in `print(...)`. The harness only understood JSON/`<tools>`,
  so it treated the call as prose and (eventually) published an empty artifact.
  The recovery parser now handles the Python-call syntax: it unwraps `print()`,
  splits arguments while skipping triple/single/double-quoted strings, and maps
  keyword and positional args through each tool's signature. Verified: Gemma's
  `write_file` now executes on the first attempt.

### Fixed
- **Gemma multi-turn no longer crashes on the chat template.** Gemma's llama.cpp
  template rejects the `system`/`tool` roles and requires strictly alternating
  user/assistant turns, so any turn carrying a tool result 400'd and the agentic
  loop died after one step. On such a template error the OpenAI-compatible path
  now retries once with a flattened, strictly-alternating message body. (qwen's
  template accepts the standard roles, so its path is unchanged.)

[0.11.4]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.11.4

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
