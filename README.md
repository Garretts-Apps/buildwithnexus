# buildwithnexus

[![npm version](https://img.shields.io/npm/v/buildwithnexus?style=flat-square&color=blue)](https://www.npmjs.com/package/buildwithnexus)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg?style=flat-square)](https://opensource.org/licenses/MIT)

A hilariously fast, **agentic AI CLI** — written in Rust. Remote models via
API key, or local models on your machine. It plans, edits files, and runs
commands, asking before each change. One self-contained binary, six direct
dependencies, no runtime to babysit — and a terminal UI built to feel
instant: an incremental wrap cache with atomic frames, live autocomplete,
GitHub-grade diffs, clickable files and links, and multimodal input straight
from your clipboard.

```bash
npm install -g buildwithnexus     # fetches the checksum-verified binary on first run
# or, with a Rust toolchain:
cargo install buildwithnexus --locked   # installs `buildwithnexus` + the `bwn` alias
buildwithnexus
```

The first launch walks you through choosing a model, and setup finishes only
once that model has answered. Then describe a task. What is sent to a hosted
provider, and what stays on your machine, is on the
[data and privacy page](https://buildwithnexus.dev/docs/data).

A daily background check tells you when a new version is out;
`buildwithnexus update` installs it (`buildwithnexus update --check` exits 10
when one is available). Set `auto_update: "install"` in settings to apply
patch releases automatically (a new minor version is announced, not
installed; `"install-any"` installs those too), or `"off"` to silence the
check. The check reads `BWN_UPDATE_REGISTRY`, else npm's
`npm_config_registry`, else the public registry. The npm launcher keeps the
downloaded binary in `~/.buildwithnexus/bin/<version>/` (under `NEXUS_HOME`
if set), outside the npm package, so `npm update` does not delete it.

## Requirements

- **npm install:** Node.js 18 or later.
- **cargo install / source build:** Rust 1.94 or later (`rust-version` in
  `harness/Cargo.toml`).
- **Prebuilt binaries:** Linux x64 and arm64 (glibc 2.34 or later: Ubuntu
  22.04+, Debian 12+, RHEL/Rocky/AlmaLinux 9+, Amazon Linux 2023, Fedora
  35+), macOS x64 and arm64, Windows x64 (the C runtime is linked in, so
  the Visual C++ Redistributable is not needed). Other platforms (for example
  musl/Alpine or Windows on Arm) need a source build pointed to by
  `BWN_BIN`. If the prebuilt binary cannot run, the first run says why
  (glibc too old, musl, or blocked by endpoint protection) instead of
  reporting it ready.
- **Optional tools:** `ffmpeg` for video attachments and image previews,
  `bwrap` (bubblewrap) for the Linux [sandbox](#sandbox), `tmux` for
  background dev servers, and `git`, `rg` and `python3` for the tools that
  use them. `buildwithnexus doctor` reports what is missing and prints the
  command that installs it; bwn never installs them itself.

## Try it in a sandbox

Want to kick the tires without touching your machine?

**GitHub Codespaces — one click, in the browser.**
[Open a codespace on this repo](https://codespaces.new/Garretts-Apps/buildwithnexus?quickstart=1)
— the devcontainer preinstalls the binary, so when the terminal appears just
run `bwn`. Add `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` as a Codespaces secret
(or export it in the terminal) to talk to a hosted model.

**Docker — local and fully disposable.** A scratch container that is deleted
on exit, so nothing the agent does can reach your files:

```bash
docker run -it --rm -e BWN_ALLOW_BOOTSTRAP=1 -e ANTHROPIC_API_KEY \
  node:22-slim npx -y buildwithnexus
```

Drop `--rm` to keep the sandbox between runs, or add `-v "$PWD":/work -w /work`
once you're ready to let it loose on a real project.

## The TUI

- **Instant startup, never a black frame** — chrome paints before anything
  else; dependency probes and connection warming run off the critical path.
- **Fast rendering** — each transcript line is wrapped once, repaints are
  frame-coalesced (~60fps) and wrapped in synchronized-output brackets, so
  streaming is smooth on kitty/iTerm2/WezTerm/Alacritty with zero tearing.
- **Live autocomplete** — type `/` for a command popup with descriptions;
  `@` completes files, `kb:` symbols. ↑/↓ navigate, Tab/Enter accept.
- **Clean diffs** — line-number gutters, background-tinted rows, changed-span
  highlighting, hunk elision. Same renderer for previews and applied
  changes.
- **Clickable everything** — file paths and links are OSC 8 hyperlinks: click
  a path in an edit header and it opens in your OS default app.
- **Images you can actually see** — `Ctrl+V` pastes a clipboard screenshot
  and it appears *right there in the transcript* before you send it: at real
  pixel resolution on kitty, Ghostty and WezTerm (kitty graphics protocol,
  Unicode placeholders — works through tmux), over Sixel on Windows
  Terminal 1.22+, foot, mlterm and xterm, as half-block art everywhere else.
  Previews re-fit when you resize the window. Drop a file onto the terminal and its path becomes an
  `@attachment` with the same preview. `@clip.mp4` runs ffmpeg to sample
  frames + metadata for vision models (text-only models get a clear "not
  multimodal" notice instead of silent drops).
- **Code that looks like code** — fenced blocks are syntax-highlighted for 20+
  languages by a zero-dependency lexer; markdown tables are drawn aligned,
  with header rules and column alignment; `---`, task lists and
  `~~strikethrough~~` render too.
- **Know what it's doing** — the banner names the model and the server
  answering it (`LM Studio (localhost:1234)`); the footer shows the model, a
  spinner, elapsed time and streamed tokens/s while the agent works. Each
  tool call is one plain line, and the agent's todo list is a checklist that
  ticks off items. When a long turn ends, or an approval or a question is
  waiting, while you're in another window you get a desktop notification
  (OSC 99/777/9) and a taskbar progress state on terminals that have one.
- **Claude-Code-grade ergonomics** — `Esc` interrupts the agent; messages
  typed while it works queue and auto-send; ↑ history is prefix-filtered and
  never destroys your draft; double-click selects a word, triple-click a
  line, and every copy confirms itself in the footer via OSC 52 (terminals
  known to ignore OSC 52 get a one-time notice instead).
- **Prompts own the keyboard** — while an approval, a question or a picker is
  open, the input box shows it and keys go only to it. `Esc`, `Ctrl+C`, or
  `Ctrl+D` on an empty line cancels a question. Pickers filter as you type, a
  digit moves to a numbered row, and only Enter picks, and only a row that is
  shown.
  `Ctrl+C` on an empty prompt quits only on a second press within 2 s
  (`Ctrl+D` quits at once; with workflows waiting it asks first).
- **What you type is what is sent** — quotes, tabs and line breaks are kept;
  a pasted line break shows as `↵` in the input box. A paste over 1,000
  characters or 10 lines shows as `[pasted 20,024 chars]` and is sent in full. `@` completes files by name anywhere in the project. A
  message that starts with an absolute path (a dropped screenshot) is sent
  with the image attached.
- **Themes and line mode** — `dark`, `light` (at least 4.5:1 contrast) and
  `ansi` (the terminal's own 16 colours); the `theme` setting defaults to
  `auto`, which follows the terminal's background, and `/theme` switches and
  saves it. `--plain` (or `TERM=dumb`) is line mode: no alternate screen, no
  cursor addressing, no spinner frames, for screen readers and plain
  consoles. Narrow terminals wrap between words.

## Why

The original `buildwithnexus` was a TypeScript CLI talking to a Python /
LangGraph backend over HTTP. This is a ground-up rewrite that keeps the *benefits*
of that engine — planning, a ReAct tool loop, approval gates, role-specialized
agents — as plain Rust control flow, with **none** of the framework weight. No
Python, no Docker, no tunnel. The orchestration that LangGraph did at runtime is
just code here, which is where the speed comes from.

Design bias, in order: **performance**, then **fewer lines**, then **fewer
dependencies** — never at the cost of the UX. Enums and `match` over trait
objects; flat data tables over registries; one pooled HTTP connection reused
across every step of the agent loop.

"Hilariously fast" is a measurement, not a mood: 2 ms full-process startup,
a 4.6 MiB resident TUI, and 3.6 µs to render a streamed chunk into a
2,000-line transcript. Every number and how to regenerate it:
[BENCHMARKS.md](BENCHMARKS.md).

### Package history

If you browse the npm version history you'll see the same name carrying
earlier, unrelated architectures — that's expected, not a hijack:

| npm versions | what they were |
|---|---|
| 0.1.x – 0.7.x | a VM-isolation "runtime" experiment (QEMU/Docker era) |
| 0.8.x | the TypeScript orchestrator with the Python/LangGraph backend |
| **0.10.1 and later** | **this codebase** — the ground-up Rust CLI (0.10 inline UI, 0.11+ full-screen TUI) |

The pre-0.10 versions share nothing with the current code and aren't
maintained; install `latest`. The Rust line is also the only one published
to crates.io.

## Models

Three wire protocols — Anthropic Messages, OpenAI chat completions, and native
Ollama — cover everything. Pick a provider during setup (or `bwn init`):

| Provider | Kind | Key |
|----------|------|-----|
| Anthropic (Claude) | remote | `ANTHROPIC_API_KEY` |
| OpenAI | remote | `OPENAI_API_KEY` |
| OpenRouter | remote | `OPENROUTER_API_KEY` |
| Groq | remote | `GROQ_API_KEY` |
| Hugging Face | remote | `HF_TOKEN` |
| Ollama | local | — |
| llama.cpp server | local | — |
| LM Studio | local | — |

Env vars override the stored key, so CI and one-offs Just Work. Keys live in
`~/.buildwithnexus/.env.keys` (0600). A key is typed hidden (dots, then only
the masked key) and saved only after the provider accepts it. If a key is
rejected later, `/login` in a session (or `buildwithnexus login`) replaces it
in place, checked first; the `/model` picker marks a key whose last check
failed `key rejected` (recorded as a hash in
`~/.buildwithnexus/key-checks.json`, never the key).

**Local models.** No key is needed. The Ollama app and its Linux service
start the server for you; without them (WSL without systemd, for example),
run `ollama serve` in a second terminal. Check that it answers, then pick it
in `bwn init`, or pass `--provider` on a headless run. These lines work the
same in bash, zsh and PowerShell:

```bash
ollama list                  # lists your models; "could not connect" means no server
ollama pull llama3.2         # the default Ollama model
bwn init                     # choose Ollama
bwn run --provider ollama --model llama3.2 "summarize this repo"
```

Default endpoints: Ollama `http://localhost:11434`, llama.cpp server
`http://localhost:8080/v1`, LM Studio `http://localhost:1234/v1`. For any
other OpenAI-compatible server (vLLM, TGI, LiteLLM, a gateway), choose the
`custom` provider (default `http://localhost:8000/v1`); setup, `/model` and
`/login` ask for its key if it needs one. A custom endpoint's key is saved
for that endpoint only (`CUSTOM_API_KEY@<scheme://host[:port]>` in
`.env.keys`, the port left off when it is the scheme's default), filed under
the host the request actually reaches, so `/model` to a new address asks for
that address's own key (Enter for none) and never sends another server's.
`buildwithnexus login --provider custom --base-url <url>` saves the key for
that address. `CUSTOM_API_KEY` in the environment is the key of the custom
endpoint a run starts on; a `/model` to another address does not get it. The
single `CUSTOM_API_KEY` an earlier version saved moves, the first time 0.15
reads it, to the custom endpoint in your own settings; with none it waits as
`CUSTOM_API_KEY@unbound`, is never sent, and `/model` and setup offer it
(`send it to <host>? [y/N]`) at the next new endpoint.
To change where any provider connects, set `base_url` in
`~/.buildwithnexus/settings.json`:

```json
{ "provider": "ollama", "model": "qwen2.5-coder", "base_url": "http://gpu-box:11434" }
```

`--base-url <url>` does the same for one run (with `--provider custom` it
reads `CUSTOM_API_KEY` from the environment). `/model` and setup remember the
address last used with each provider (`endpoints` in your own
`~/.buildwithnexus/settings.json`, never a project's), so switching back to a
provider, or `--provider` on a headless run, returns to that server.
`/model http://host:11434 <model>` picks Ollama's native API; a URL ending in
`/v1` is a custom OpenAI-compatible endpoint. `provider`, `model` and
`permission` may be left out of a settings file: an empty model means the
preset's default.

A configured key is only sent over HTTPS or to a loopback address.

**Local servers.** llama.cpp, LM Studio and vLLM report their context window,
and bwn uses it (`context_tokens` in settings fixes a value); `/context` shows
the total and what fills it (system prompt, tools, MCP tools, conversation,
images). A message bigger than a known window is refused before it is sent.
Whether a model takes images follows what the server reports (Ollama
capabilities, LM Studio model type, llama.cpp modalities); `"vision": true` or
`false` decides outright. `/local` lists the configured server and every local
preset with its models, then the GGUF files on disk. A busy server (429 or
5xx) is retried 3 times (`BWN_MAX_RETRIES=0`…`20`), a model that is still
loading for about two minutes. Errors say what happened and what to do, with
the server's own message underneath (`nothing is answering at <host>`, a
missing Ollama model with its `ollama pull` command).

**Reasoning.** `reasoning_effort` in settings (`off` by default, or `low` / `medium` /
`high`; `--effort <level>` per run, `/effort` in-session) maps to each API's
native control: Claude 4.6+ gets adaptive thinking with `output_config.effort`,
older Claude models a `budget_tokens` thinking budget (2048 / 8192 / 16384),
OpenAI reasoning models (`o1`/`o3`/`o4`/`gpt-5`) `reasoning_effort`, and
Ollama's native API `think: true` (an Ollama model that does not support
thinking is retried without it). Other models — including anything behind a
local OpenAI-compatible server — receive no reasoning parameters at all.

**Cost.** Every request's `usage` block feeds a session ledger: `/cost` shows
input / output / cache-read / cache-write tokens, the request count, and an
estimated dollar figure from a built-in price table (local providers show
`$0.00 (local)`; an unlisted model shows tokens only, never a guessed price).
`--max-budget-usd <n>` (or `max_budget_usd` in settings) stops the agent
before the next model request once the estimate exceeds `n`, with a `notice`
event in `--json` mode; a headless run then exits 5. With a cap set, a remote
model that has no known price stops before the first request (exit 2): give
it a price in USD per million tokens under `prices`, which wins over the
built-in table (`cache_read` and `cache_write` default to `input`):

```json
{ "prices": { "my-gateway-model": { "input": 3.0, "output": 15.0 },
              "llama3.1:70b": { "input": 0, "output": 0 } } }
```

## Modes

- **PLAN** — decompose the task into steps you approve, edit (Edit Step:
  pick a step and change its text in the input box) or push back on (Revise
  Plan: say what to change and get a revised plan), then execute.
- **BUILD** — the agentic ReAct loop: read/edit files, run commands, iterate.
- **BRAINSTORM** — free-form chat with read-only tools (read, grep, fetch, read-only commands); never writes. A task typed here gets a hint to switch; the mode changes only when you change it (Shift+Tab, `/mode`, or bare `/plan`, `/build`, `/brainstorm`).

All three modes are one conversation: each turn sees the earlier ones, the
whole conversation is saved as the session, and `/context` counts it. `/new`
starts a fresh one.

```bash
buildwithnexus                 # full-screen interactive session
buildwithnexus run <task>      # execute a task (agentic, headless)
buildwithnexus plan <task>     # decompose, approve, then execute
buildwithnexus brainstorm <q>  # free-form chat (read-only tools)
buildwithnexus continue        # reopen this folder's latest session
buildwithnexus resume <id>     # reopen a session (bwn sessions lists them)
buildwithnexus init            # (re)configure provider / model / key
buildwithnexus login           # replace the provider's API key, checked first
buildwithnexus providers       # list built-in providers
buildwithnexus doctor          # diagnose setup (keys, tools, connectivity)
buildwithnexus review          # read-only review of your changes (exit 9: blocking)
```

Inside the interactive session:

```
/model [name]             hot-swap the AI model mid-session
/login                    replace the API key (checked before it is saved)
/effort [off|low|medium|high]  show or set reasoning depth (persisted to settings)
/permissions [mode|default <mode>|list|remove <entry>|reset]  see Permissions
/theme [dark|light|ansi|auto]  colour theme (saved)
/local                    local servers, their models, and GGUF files on disk
/compact                  compress context (free up token budget)
/context                  context window usage and what fills it
/cost                     session tokens by category, request count, estimated cost
/diff [turn]              changed and new files, one summary, a file's diff; turn: the last turn's changes
/review [--base <ref>|--staged] [focus]  read-only review of uncommitted, staged or branch changes
/commit                   AI-drafted commit message; commits only after [c]ommit
/pr                       AI-drafted pull request title + description
/undo [latest|git|all|<id>]  revert the last agent turn's edits (asks before overwriting yours)
/rewind                   go back to an earlier prompt: code, conversation, or both
/checkpoints              tasks and files /undo can restore
/resume                   pick a saved session (this folder first; type to filter)
/rename <name>            name this session
/export [file]            this conversation as Markdown (default ~/.buildwithnexus/exports/<id>.md)
/copy                     the last answer to the clipboard (OSC 52)
/ask <question>           a side question that is not added to the conversation
/init                     run setup, then offer to write AGENTS.md from this repository
/agents                   helper agents from agent files, then Agents.md
/add-dir [path]           also work in another folder for this session (no path: list them)
/schedule <delay> <task>  run a task once in the background (5s, 2m, 1h)
/loop <interval> <task>   run a task repeatedly in the background (up to max_concurrent_workflows at once, default 2)
/workflows                list and manage background workflows (i<id> shows a run's log, kept in ~/.buildwithnexus/workflows/)
/btw <context>            add a note to your next message
/config                   configure hooks, memory, and commands via AI
/memory                   view and edit session memory
/skills                   list skills and custom commands
/trace                    inspect hooks, tools, skills, and subagents
```

Background workflows (`/schedule`, `/loop`) run under the session's
permission, provider and model. Nobody can answer an approval for them, so
outside `auto` a change they try is refused: a refused `/loop` run stops the
loop with `✗ workflow #N blocked: …`. Use `/permissions auto` in the session
that schedules a workflow that should edit unattended. Bare `/loop`,
`/schedule` or `/btw` print their usage; `/help` lists every command with
its arguments, the keys, and the answers to an approval prompt, and the `/`
popup and Tab complete the same commands and their subcommands
(`/permissions ` Tab offers the modes and `default`, `list`, `remove`,
`reset`). `buildwithnexus --help` lists every command, subcommand and option.
Both link [what leaves your machine](https://buildwithnexus.dev/docs/data).

## Sessions and undo

Every conversation is saved as it runs (`~/.buildwithnexus/sessions/`) and
belongs to the folder it started in.

```bash
bwn continue                   # reopen this folder's latest session in the UI
bwn continue "now add tests"   # or run one more task on it, headless
bwn resume <id>                # reopen a session; bwn resume alone opens the picker
bwn sessions                   # this folder first, with age and message count
bwn sessions export <id>       # the conversation as Markdown
bwn sessions rm <id>           # delete a saved session
```

`bwn -c` is `bwn continue`. With no session in this folder, `continue` takes
the latest one anywhere and says so. In a session, `/resume` picks a saved
session (this folder first; type words to filter), `/rename <name>` names the
current one, and `/new` starts a fresh one. `/export [file]` writes the
conversation as Markdown, `/copy` puts the last answer on the clipboard
(OSC 52, so it works over SSH), and `/ask <question>` answers a side question
from the conversation without adding it. `/btw <note>` adds a note to your
next message.

Every file write is checkpointed first. Bare `/undo` reverts the last agent
turn's edits, `/undo all` everything from the last 24 hours, and
`/undo <id>` one checkpoint from `/checkpoints`. When a file has changed since
the agent edited it, `/undo` asks `<file> changed after the agent edited it —
overwrite your changes? [y/N]`, and `n` keeps your version while the rest is
restored. `/undo` also says what it cannot undo: changes made by shell
commands, commits, and files too large to snapshot (the approval prompt warns
about those too). `/rewind` (or Esc Esc on an empty input box) goes back to
an earlier prompt and restores the code, the conversation, or both; the prompt
comes back in the input box to edit and send again. `/diff` lists every changed and new file
with one summary line and shows a chosen file's diff; `/diff turn` shows what
the last turn changed. `/commit` drafts a message and commits only when you
answer `c` (`[c]ommit · [e]dit · [n]o`). Each folder keeps its newest 500
checkpoints. [RECOVERY.md](RECOVERY.md) has the details.

## Headless and CI

`run`, `plan`, `brainstorm`, `continue` and `resume` run without the TUI. With
no settings file, a headless run uses `--provider`, or the first provider
whose API key is in the environment, and writes nothing.

| Flag | Effect |
|---|---|
| `--provider <id>` | provider for this run (`buildwithnexus providers` lists ids) |
| `--model <name>` | model for this run |
| `--base-url <url>` | model endpoint for this run, such as a gateway (`--provider custom` reads `CUSTOM_API_KEY`) |
| `--permission-mode <mode>` | `ask`, `accept-edits`, `auto` or `readonly` (see [Permissions](#permissions)); with no terminal, `ask` blocks every change, so CI usually wants `auto` or `readonly`. An unknown name exits 2. |
| `--sandbox <off\|auto\|require>` | OS sandbox for shell commands (see [Sandbox](#sandbox)) |
| `--effort <off\|low\|medium\|high>` | reasoning depth |
| `--max-budget-usd <n>` | stop before the next request once the estimated cost exceeds `n` |
| `--json` | machine-readable events on stdout instead of text |
| `--yes`, `-y` | `plan` only: approve the plan and execute it |
| `--legacy-exit-codes` | exit 0 when a run stops short without failing (codes 4 to 8, and 3 or 1 for refused or unrun calls, below) |
| `--trust-project <digest>` | trust exactly this content of the folder's project settings for this run (also `BWN_TRUST_PROJECT`; `buildwithnexus trust --print` prints the digest) |
| `--worktree <name>` | work in `.bwn/worktrees/<name>` on branch `bwn/<name>` (created from HEAD, or reused); on exit bwn prints the branch and `git merge bwn/<name>` |
| `--add-dir <path>` | also read and change files in `<path>`; repeatable (see [More folders](#more-folders)). A missing folder, a file, `/` or a folder holding your home exits 2. |
| `--plain` | line mode for the terminal UI: no alternate screen or cursor addressing (as with `TERM=dumb`) |
| `--` | everything after it is task text, even if it looks like a flag |

An unknown option anywhere before `--` is a usage error (exit 2) with a
"did you mean" hint, and nothing is sent.

**Piped input.** When stdin is not a terminal, `run`, `plan` and
`brainstorm` read it: with no task argument it is the task, and with one it
is added after the task as a `[stdin]` block (up to 1 MiB; the rest is cut
with a notice). With a task argument, a pipe that sends nothing for 3 s is
ignored. No task at all exits 2 before any request; pass `</dev/null` to keep
stdin out.

```bash
git diff | bwn run --permission-mode readonly "review this diff for bugs"
```

| Exit code | `outcome` | Meaning |
|---|---|---|
| 0 | `success` | the task finished |
| 1 | `failed` | the run failed, no provider could be set up, or the turn ended right after a call that could not run |
| 2 | | usage error: unknown option or `--provider`, a bad `--permission-mode` or `--effort`, a flag missing its value, no task, `plan` with no terminal and no `--yes`, a spend cap on a model with no known price, a repository command that is not trusted (`bwn run '/deploy'`), or words after `acp` |
| 3 | `approval_blocked` | changes were blocked for lack of approval (`ask` or `accept-edits` with no terminal; the closing line names them), or a hook, a deny rule or read-only mode refused a call (`changes were denied: …`). A `check_work` round nobody could approve is not a blocked change: the run says the checks were not run. |
| 4 | `hook_blocked` | a `UserPromptSubmit` hook blocked the task |
| 5 | `budget_stop` | `--max-budget-usd` stopped the run before the next request |
| 6 | `step_limit` | the turn used every step without finishing |
| 7 | `check_work_failed` | the model finished, but the project's checks (`check_work`) still fail |
| 8 | `verification_failed` | the model finished, but the verifier still blocks after its fix rounds |
| 9 | `review_blocking` | `buildwithnexus review` found a blocking issue |
| 130, 143 | `interrupted` | SIGINT or SIGTERM; the session is saved and the line names it |

With `--json`, the last event is the `result` event: `outcome`, `exit_code`,
`session_id`, `turns`, `tokens_in`, `tokens_out`, `cost_usd`, `denied` and
`denials` (the refused calls, each with its reason). When more than one
applies, the first reason the run stopped short is reported.
`--legacy-exit-codes` (or `BWN_LEGACY_EXIT_CODES=1`) restores the pre-0.15
behavior: codes 4 to 8, and 3 or 1 for refused or unrun calls, become 0,
while the `result` event still names the outcome. `buildwithnexus update
--check` exits 10 when a newer release exists, and `buildwithnexus doctor`
exits 1 when a check fails (`--json doctor` prints one `check` event per
check; `--json sessions` one `session` event per saved session).
`buildwithnexus mcp login` and `logout` exit 1 when the sign-in or sign-out
fails and 2 on a usage mistake.

**Reviews in CI.** `buildwithnexus review [--base <ref> | --staged] [focus]`
reviews uncommitted changes (plus the branch since `<ref>` with `--base`, or
only staged changes with `--staged`), including new files git does not track
yet; key and credential files are named but never sent. It is read-only in
every permission mode, prints a `finding` event per issue with `--json`, and
exits 9 when one is blocking. `/review` takes the same arguments in a session.

**Trusting a repository in CI.** Project hooks, MCP servers, allow rules and
the repository's commands, skills and agents need folder trust, and CI has
nobody to answer the prompt. Run `buildwithnexus trust --print` in the
checkout to see what the project settings run, the commands, skills and
agents it carries, and their digest, then pass `--trust-project <digest>` (or
set `BWN_TRUST_PROJECT`): if the files change, the run stops with exit 2 and
names them.

**Custom commands headless.** `bwn run '/deploy staging'` runs a custom command
or skill with its arguments, as in a session. A command from the repository
that is not trusted exits 2 and says how to trust it.

Each `--json` event has a `schema_version` (now `1`). What may change in a
minor or a patch release (flags, settings keys, events, session files, exit
codes) is in [docs/VERSIONING.md](./docs/VERSIONING.md).

```bash
ANTHROPIC_API_KEY=... bwn run --permission-mode auto --json "fix the failing test"
```

On npm installs in CI, add `--bootstrap` or set `BWN_ALLOW_BOOTSTRAP=1` so the
launcher can fetch the binary on first run.

**GitHub Actions.** The repository is also an action: it installs bwn from
npm, runs `run` or `review` with `--json`, turns the outcome into the step's
result with annotations (review findings land on their lines), uploads the
event log as an artifact and, with `comment: true`, posts the summary on the
pull request, updating the same comment on later pushes.
[examples/github/bwn-review.yml](./examples/github/bwn-review.yml) reviews
every pull request:

```yaml
- uses: Garretts-Apps/buildwithnexus@v0.15.0
  with:
    command: review              # or run, with prompt: <task>
    review-base: origin/${{ github.base_ref }}
    provider: anthropic
    max-budget-usd: "1"
    comment: "true"              # needs pull-requests: write
  env:
    ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}
```

`permission-mode` defaults to `readonly`. Exit 0 passes the step; every other
exit code fails it with an error annotation naming the outcome, unless the
outcome is listed in `allow-outcomes` (for example `budget_stop,step_limit`),
which makes it a warning. The outputs are `outcome`, `exit-code`, `passed`,
`session-id`, `cost-usd`, `findings`, `summary` and `events` (the log's path).
`install` takes a version, an npm package spec or a tarball from `npm pack`
(empty: the action's own version), and `BWN_BIN` in the step's environment
runs a binary you built instead. Inputs reach the scripts as environment
variables, never as script text, and the token is passed only to the comment
step. All inputs are in [action.yml](./action.yml).

## Permissions

Every mutating tool (`write_file`, `edit_file`, `run_command`) passes a gate:
`ask` (default), `accept-edits`, `auto` (yolo), or `readonly`. Set it during
setup, with `--permission-mode`, or with `/permissions`. In `readonly`,
mutations are refused outright — never prompted — so an approved
sensitive-path or dangerous-command confirmation can't slip one through.
`accept-edits` applies file edits inside the project without asking, while
commands, deletions, network access and anything inside `.git` still ask.
Claude Code's names are accepted too (`acceptEdits`, `default` for ask,
`plan` and `dontAsk` for readonly, `bypassPermissions` for auto); an unknown
name is an error, and a misspelt `permission` setting warns and uses `ask`.

A switch made in a session (`/permissions auto`, or typing "use auto") lasts
for that session. `/permissions default <mode>`, or "save as default" in the
`/permissions` picker, saves it for every new session. `/permissions list`
(and the bare picker) shows the saved approvals and the rules in force;
`/permissions remove <entry>` forgets one approval.

The prompt shows the whole command, with line breaks marked `⏎`, and names
what `s` / `a` would allow from then on: a binary (`cargo`), a subcommand
(`git status`), a host, or, for shells, interpreters and other programs that
run what they are given (`sh`, `python3`, `python3.12`, `node`, `awk`, `sed`,
`env`, `fakeroot`, …), and for programs whose arguments decide what they destroy or stop
(`rm`, `mv`, `cp`, `ln`, `chmod`, `chown`, `dd`, `truncate`, `kill`, `pkill`,
`killall`, `del`, `robocopy`, …) or git commands that discard or rewrite
(`git rm`, `git clean`, `git checkout`, `git restore`, `git reset --hard`,
`git push --force`, `git branch -D`, `git stash drop`, `git filter-branch`,
`git reflog expire`, `git submodule deinit`, `git gc --prune`, …), only that exact command. Answering `y`
allows the call once; `d <reason>` refuses it and tells the model why; `Esc`
or `Ctrl+C` refuses it and stops the turn. Answering `a` (always allow) remembers it
**for the current project only** (`project_allowed` in
`~/.buildwithnexus/settings.json`, keyed by directory). `/permissions reset`
forgets those answers for the project you're in. The legacy global
`allowed_commands` list keeps working, except for a shell or interpreter saved
by name alone (`python3`, as 0.14.3–0.14.8 stored them): those are ignored,
and bwn lists them at startup and in `/permissions`.

Network tools (`fetch_url`, `web_search`, `headless_browser`,
`wait_for_url`, `open_browser`) ask once per host and port in every mode but
`auto`, `readonly` included, since a fetch or a search query can carry data
out or reach services on your network. `web_search` sends its query to
`lite.duckduckgo.com`. `s` / `a` allow that host; an `allowed_commands` entry
`"fetch *"` allows every host.

Commands the agent runs (including `check_work` and `start_server`, sandboxed
or not) do not inherit provider keys: variables ending in `_API_KEY` or
`_API_TOKEN`, and the presets' key variables such as `HF_TOKEN`, are removed
from their environment. List any your build needs in
`"shell_env_passthrough": ["MAPS_API_KEY"]`. Hooks are your own and keep
their environment.

### More folders

`--add-dir <path>` (repeatable) or `/add-dir <path>` adds a folder to work
in besides the one you started in, for the rest of the session. The file
tools may change files there under the same permission mode (`accept-edits`
included), searches without a folder of their own (`find_files`,
`grep_files`, `find_paths`) and `@` completion cover it, the sandbox binds
it writable, and helpers and background workflows get it too. The footer
shows `+N dirs`, and `/add-dir` alone lists them.

What stays the same: sensitive paths there still ask in every mode, a link
inside the folder that leads out of it is outside, and its `.git` stays
read-only to sandboxed commands and asks in `accept-edits`. Folder trust
does not extend to it: its settings, hooks, commands, skills and agent
files never load. Its `AGENTS.md` (or the first `instruction_files` name at
its top) is sent to the model, after a notice names it, as instructions
for that folder. The filesystem root, a folder that holds your home folder,
bwn's own folder and credential stores cannot be added. A resumed session
starts without the folders; add them again.

### Allow, ask and deny rules

`permissions` and `network` in `settings.json` set rules that apply before
the mode: a deny rule refuses in every mode, an ask rule always prompts (even
in `auto` or after an `a` answer), and an allow rule skips the prompt. Deny
beats ask, ask beats allow, and all three beat the mode.

```json
{
  "permissions": {
    "allow": ["run_command(cargo test*)", "write_file(src/**)"],
    "ask":   ["write_file(migrations/**)"],
    "deny":  ["run_command(git push*)", "WebFetch(domain:pastebin.com)"]
  },
  "network": { "allow": ["docs.rs", "*.github.com"], "deny": ["*.internal.example"] }
}
```

A rule is `Tool` or `Tool(pattern)`. The tool part is matched like a hook
matcher, so Claude Code names work (`Bash`, `Edit`, `Write`, `WebFetch`), and
a rule for `run_command`, `bash` or `Bash` also covers the commands of
`check_work` and `start_server`. The pattern is a `*`/`?` wildcard matched
against the command for shell tools (Claude Code's `git push:*` means
`git push*`), the host for network tools (`domain:` is optional), the query
for `web_search`, and the touched paths for file tools: project-relative
(`migrations/**`), or absolute or `~/` for any path. An allow rule must cover
a whole plain command (no chaining or redirection) and every path; ask and
deny rules match any part of a compound command and any path. Ask and deny
rules also see the command behind a wrapper (`env`, `sudo`, `nice`,
`command`, `time`, `timeout`, `xargs`, `exec`, `eval`, `find -exec`,
`fakeroot`, `firejail`, `bwrap`, `torsocks`, `proxychains`, `numactl`,
`chronic`, `run0`, `sg`, `ssh-agent`, `systemd-inhibit`, `uv`/`poetry`/`pipenv
run`, `bundle exec`, `direnv`/`mise exec`, `nix-shell --run`, `FOO=1`,
`/usr/bin/git`), inside `sh -c '…'`, `bash -lc "…"`, `cmd /c`,
`pwsh -Command`, `$(…)` and backquotes, past git's own options (`-C`, `-c`,
`--git-dir`, `--work-tree`, `--attr-source`, `--shallow-file`), through an
alias given with `-c alias.<name>=…` and into git's `git-<command>`
programs, so
`run_command(git push*)` also refuses `git -C . push` and
`sudo sh -c 'git push'`. An alias saved in git's config is not seen. A deny rule also refuses a pipeline or compound
command that names its program anywhere (`make && git status` under
`run_command(git push*)`), since such a command can build what it runs from
parts, and so does code handed to an interpreter (`perl -e 'system "git push"'`)
or a word in bash's `$'…'` quoting; the refusal says to run that command on
its own. On Windows the rules read a command as cmd.exe passes it on
(`g^it push` is `git push`), and a command with `^` or `%` counts as
compound. Rules are a guard against mistakes, not a sandbox: a program the
list of wrappers does not know, or a script file, can still run what a rule
names. `network`
entries are host patterns (`example.com`, `*.example.com`) for the network
tools and `http` hooks. Rules and saved approvals see the host a URL
reaches: percent-escapes decoded, a trailing dot dropped and a default port
written out (`:443` on https) left off, so `https://%6Cite.example.:443/` is
`lite.example`. A refusal names the rule and whether it came from user or project
settings, and a headless run refused by one exits 3.

Rules add up across `~/.buildwithnexus/settings.json`, `settings.local.json`
and the project's `.buildwithnexus/settings.json`, so a project cannot drop
your deny rules. A project file adds ask and deny rules on its own, and allow
rules only once you trust the folder.

Headless `plan` needs a terminal to approve the plan; pass `--yes` / `-y` to
auto-approve and execute (in `--json` mode the plan is emitted as a `plan` event
first). Without a terminal and without `--yes` it exits 2 immediately.

### Sandbox

An optional OS-level layer *under* the permission gate for shell commands
(`run_command`/`bash`, `!cmd`, and `check_work`). Settings key `"sandbox"`
(or `--sandbox <mode>`, or `/sandbox <mode>` in a session):

- `"off"` (default) — commands run as today.
- `"auto"` — confine commands when a backend works; otherwise run them
  unsandboxed and say so once per session.
- `"require"` — refuse to run commands when no backend works.

Backends are external binaries, probed once per session: `bwrap` (bubblewrap)
on Linux and `sandbox-exec` (Seatbelt) on macOS. Windows and WSL have none.

**Confined:** filesystem writes anywhere except the working directory and the
temp dirs. On Linux, `/tmp` is a fresh private tmpfs (with `TMPDIR=/tmp`)
that is discarded when the command exits, so files written there are not
visible to the host or to the next command. On macOS, `/tmp` and `$TMPDIR`
are the real, shared directories. Everything else, including
`~/.buildwithnexus` and tool caches such as `~/.cargo` or `~/.npm`, is
read-only to the command. Set `"sandbox_network": false` to also block the
network inside the sandbox (default `true`, allowed).

**Not confined:** reads (the whole filesystem stays visible), the agent's own
file tools (already fenced to the working directory), hooks, MCP servers, and
`start_server`. The sandbox never approves anything — the permission gate is
unchanged; it only limits what an approved command can touch. Sandboxed
commands are marked `[sandboxed]` on the tool header; `/sandbox status` and
`buildwithnexus doctor` show backend availability and whether commands would
be confined. To escape for one command, switch with `/sandbox off` and back.

## Inline images

Attach an image three ways — `Ctrl+V` with a screenshot on the clipboard,
drag a file onto the terminal (or paste its path), or type `@shot.png` — and
it is drawn inside the transcript, above the composer, before you send it:

| Terminal | What you see |
|---|---|
| kitty ≥ 0.28, Ghostty, WezTerm (placeholder support) | the real pixels, at up to the full width of the window, scrolling with the text; works inside tmux with `set -g allow-passthrough on` |
| Sixel terminals: Windows Terminal 1.22+, WezTerm, foot, mlterm, xterm `-ti vt340` | the real pixels, detected at startup; needs `ffmpeg` on PATH |
| any other truecolor terminal (iTerm2, Alacritty, older Windows Terminal, VS Code…) | half-block art (needs `ffmpeg` on PATH to decode) |
| `NO_COLOR`, non-truecolor | no preview, never garbage |

A pasted PNG needs no external tool at all. JPEG, WebP, GIF and a video's
first frame are converted with ffmpeg when it is installed. Control it with
the `images` settings key (`"auto"` default, `"kitty"`, `"sixel"`, `"blocks"`,
`"off"`) or `BWN_IMAGES=…` for one run. Sixel and block previews take at most
half the window's width and a third of its height, and re-fit when the
window is resized. Uploads are freed when you `/clear` or exit.

### Pictures, PDFs and screenshots the agent reads

The model can look at pictures too, when it takes images (see `vision` under
[Models](#models)). `read_file` on a PNG, JPEG, GIF or WebP (up to 5 MB)
returns the picture: Anthropic gets it inside the tool result, and
OpenAI-compatible servers and Ollama get it in a user message right after
the tool results. A text-only model is told why it sees none, and the
transcript shows what the picture was (`image shot.png (image/png,
1280x800, …)`). `read_file` on a PDF returns its text through
`pdftotext` from poppler when that is installed (`apt install
poppler-utils`, `brew install poppler`), and otherwise says what to install;
a line range works as for any file.

`screenshot_url` opens a page served on this machine
(`http://localhost:3000`, a dev server the agent started) in a local
headless Chrome, Chromium or Edge and shows the model the picture (`width`
and `height` optional, default 1280×800). The browser is `BWN_CHROME` when
set, else one found on PATH, in the usual install folders, or a Playwright
build. It runs with a throwaway profile and without provider keys in its
environment. Only loopback addresses are opened unless `network.allow` (or
an allow rule for `screenshot_url`) names the host, and outside `auto` each
host is approved like a fetch. Every request the page or the browser makes
to any other host, link-local and cloud-metadata addresses included, goes to
a local proxy that refuses it, and the result lists the hosts the page
tried. The tool is offered only to models that take images.

`"vision": false` in settings.json makes bwn treat the model as text-only:
`read_file` then returns a notice instead of a picture and `screenshot_url`
is not offered. Pictures a tool returned are kept in the session file, like
attached images.

A related setting: `notify` (`"auto"` — desktop notification when a turn of
8 s or longer ends, or an approval or a question is waiting, while the window
is unfocused; `"always"`; `"off"`).

## Hooks

Run your own commands at the same lifecycle points as Claude Code, configured in
`~/.buildwithnexus/settings.json` (user) and/or `.buildwithnexus/settings.json`
(project). User hooks are always active; **project hooks run only after you trust
that folder** (you're prompted once, and a project hook may *deny* a tool but
never *grant* one — so cloning a hostile repo can't run or unlock anything).
The prompt shows each hook's command line and each project MCP server's
command and arguments. Trusting also pins the project files those commands
run: scripts they name (`./x.sh`, or a bare `setup` that `sh` or `cmd.exe`
would find in the folder), and `package.json` or the `Makefile` when they
call `npm`, `make` and the like, in the project root or in a folder the
command names (`make -C sub`, `cd web && npm test`). Editing one of those
asks again, and the check is repeated before every run of a project hook: a
script that changed since you trusted it is asked about
(`scripts/fmt.sh changed since you trusted it — run it?`), or skipped with a
warning in a headless run. The prompt asks separately before a project's
`base_url` (where your requests and key go) or `permission` takes effect.
For CI, see *Trusting a repository in CI* under [Headless and CI](#headless-and-ci).
Events: `SessionStart` / `SessionEnd` (once per process), `UserPromptSubmit`,
`PrePrompt` (before each model request in a BUILD turn), `PreToolUse`,
`PermissionRequest` (where bwn is about to ask you to approve a call),
`PostToolUse`, `PostResponse`, `OnError`, `Stop` (after every BUILD, PLAN,
BRAINSTORM, or chat response), `SubagentStop` (when a `spawn_subagent` call
returns; its payload carries the subagent's `tool_input`), `PreCompact` (before
the conversation is summarized; `trigger` is `auto`, or `manual` for
`/compact`), and `Notification` (where bwn raises a desktop notification,
whatever the `notify` setting and window focus; `notification_type` is
`permission_prompt`, `question`, `done` when a turn ends, or `idle_prompt` when
the prompt has waited `idle_notify_secs`, default 60, `0` for never). A
`PreCompact` or `Notification` matcher matches the trigger or the type. Each
hook receives the event as JSON on stdin with Claude Code's field names:
`hook_event_name`, `session_id` (the id the transcript is saved under),
`transcript_path`, `permission_mode` (`ask` | `accept-edits` | `auto` | `readonly`), `cwd`, plus
the event's own fields (`tool_name`, `tool_input`, `tool_response`, `prompt`,
`stop_hook_active`, `trigger`, `notification_type`, `message`).

`PreToolUse` can gate a tool: exit code **2** (or a JSON
`permissionDecision: "deny"`) blocks it — even under `auto`. `"allow"` skips the
prompt; otherwise the normal gate applies. A `PreToolUse` hook that gives no
answer also blocks the call, with a message naming it: one that times out
(`"timeout"` in seconds, default 10), cannot start (missing script or
interpreter, not executable, a `.rs` hook that does not compile) or is killed by
a signal. Any other non-zero exit is shown with the end of its stderr and
does not block, unless the hook sets `"on_error": "deny"`, which makes a
crashing guard block the call. Other events never block on a failed hook, but
the failure is shown. Matchers are `*`, an exact tool name, or a
`|`-separated list; each segment may use `*` and `?` wildcards (`"*_file"`,
`"mcp__*"`, or Claude Code's `"mcp__.*"`). Claude Code tool names stand for
the bwn tools that do the same thing: `Bash` (`run_command`, `bash`,
`start_server`), `Edit`, `MultiEdit`, `Write`, `Read`, `Grep`, `Glob`, `LS`,
`WebFetch`, `WebSearch`, `Task` and `TodoWrite`, so a matcher copied from a
Claude Code settings file guards the same calls. `Edit` and `Write` also cover
`multi_edit`, `remove_path`, `move_path` and `create_dir`, and a matcher
naming the shell (`Bash`, `run_command`, `bash`) also runs for `check_work`
and `start_server` calls that carry a command. `tool_input` carries Claude
Code's field names beside bwn's, with the values the tool will use:
`file_path` (absolute), `old_string`, `new_string`, `content`, `prompt`,
`subagent_type`, `glob`, `path` and `todos`. A call that touches several
files (`move_path`, `read_many_files`, `apply_patch`) is shown to
`PreToolUse` once per file, and `PostToolUse` sees a move's destination. The
legacy `mcp_call` tool is matched, ruled and approved as the
`mcp__<server>__<tool>` it calls. A `*` or empty matcher does
not run on `finish` and `exit_plan`; name them to guard them. A hook under an
event bwn does not fire, of an unknown `type`, or without its command is
reported at startup and by `doctor` instead of being ignored. See
[`examples/settings.json`](./examples/settings.json).

Other events answer as Claude Code's do. What a `PostToolUse` hook says
reaches the model with the call's result (the call already ran):
`{"decision": "block", "reason": …}`,
`{"hookSpecificOutput": {"additionalContext": …}}`, or its stderr with exit 2.
A `Stop` or `SubagentStop` hook that exits 2 or answers
`{"decision": "block", "reason": …}` keeps the agent (or the helper) going,
with the reason as the next message, at most 3 rounds in a row;
`stop_hook_active` is true while it does. It never sends on a turn you
stopped, and in PLAN the plan waits for your approval instead.
`PermissionRequest` may answer for you with
`{"hookSpecificOutput": {"decision": {"behavior": "allow" | "deny" | "ask", "message": …}}}`
(or `{"decision": …}`), and exit 2 denies. It also runs in headless runs,
where nobody could answer the prompt; only your own hooks can allow.

A hook's `type` is `command` (a shell command line), `python` or `script` (a
file; the interpreter follows the extension), or `http`:
`{"type": "http", "url": "https://…", "headers": {"Authorization": "…"}}`
POSTs the payload as JSON through bwn's HTTP client (proxy and certificate
settings apply) within the hook's `timeout`, following no redirects. The
response body counts as the hook's output, and a status outside 2xx as a
failed hook. A host that `network.deny` names is refused. The trust prompt
shows a project's http hook URLs (header names only).

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "run_command",
        "hooks": [{ "type": "command", "command": "echo 'no shell on main' >&2; exit 2" }] },
      { "matcher": "Edit|Write",
        "hooks": [{ "type": "command", "command": "./scripts/guard.sh", "on_error": "deny" }] }
    ]
  }
}
```

## Project instructions (AGENTS.md / CLAUDE.md)

The same instruction files other coding agents read are loaded into every
system prompt — BUILD, PLAN, BRAINSTORM, quick chat replies, sub-agents, and
headless `--json` runs. Discovery order (most general first, later files take
precedence):

1. `~/.buildwithnexus/AGENTS.md` (global, optional)
2. every directory from the git root (or filesystem root) down to the cwd:
   `AGENTS.md`, else `CLAUDE.md` (so a repo shipping both isn't loaded twice),
   then `.buildwithnexus/AGENTS.md`

Each file is capped at 32 KiB (cut with a visible marker) and 96 KiB in
total. A dim line at startup lists what was found. The repository's own files
are asked about once per folder and content: `instructions from this repo:
AGENTS.md, src/AGENTS.md — press r to review or Enter to use them` (`r` shows
them; Esc asks again next launch). After that the line just names them, until
a file changes. A headless run, or a session with prompts piped in, cannot ask, so until
then it prints one line (a headless run on stderr): `instructions from this repo: AGENTS.md (not reviewed — …)`.
`/init` offers to write `AGENTS.md` from the repository's own build and test
files (or to improve the one there), shown as a diff you approve;
`buildwithnexus init --agents-md` does the same headless.
The `instruction_files` settings key changes which names are looked up
(default `["AGENTS.md", "CLAUDE.md"]`; add `"GEMINI.md"`, or `[]` to disable).

The mixed-case `.buildwithnexus/Agents.md` is different: it defines agent
roles/capabilities (`/agents` shows it) and is loaded after the instructions.

`~/.buildwithnexus/system.md` adds your own text to every system prompt. A
project's `.buildwithnexus/system.md` is used only once you trust that folder
(it is listed in the same prompt as project hooks), and it is added after
yours, not in place of it. Set `"project_system_prompt": "replace"` in
`~/.buildwithnexus/settings.json` to let a trusted project's file replace yours.

## Skills

Skills are markdown instructions loaded on demand: the system prompt carries
only each skill's name and description, and the full body is loaded by
`/<name>`, the `load_skill` tool (alias `skill`), or `list_skills` → `load_skill`.
Two layouts are supported, from any of these roots:

```
~/.buildwithnexus/skills/   ./.buildwithnexus/skills/    (source: user / project)
~/.claude/skills/           ./.claude/skills/            (source: claude)
~/.agents/skills/           ./.agents/skills/            (source: agents)
```

- **`<name>/SKILL.md`** folders (the Agent Skills open standard) with YAML
  frontmatter — `name` (defaults to the folder name) and `description`
  (required for a useful listing; a missing one shows as `(no description)`
  with a one-time warning). Unknown keys are ignored. When loaded, the skill
  reports its directory so `scripts/` or `references/` inside it can be read
  with the file tools.
- **`<name>.md`** flat files, where the first prose line is the description.

On a name collision a `SKILL.md` folder beats a flat file of the same name,
and your own skills beat bundled ones. A skill from the project (the `./`
roots above, a relative `skill_dirs` entry, or any `skill_dirs` entry a
project settings file adds) loads only once you trust the folder (see
*Project commands, skills and agents* below), and never replaces a bundled or
user skill: it loads as `/project:<name>` instead, with a notice at startup.
Set `"project_skills_override": true` in `~/.buildwithnexus/settings.json` to
let project skills replace them, as before 0.15. Add more roots with the
`skill_dirs` settings key (`["~/my-skills", "tools/skills"]`).
`/skills` lists every skill with its source and description.

### Custom commands

A Markdown file in a commands folder is a slash command named after the file:
`deploy.md` is `/deploy`. Its body, with the YAML frontmatter stripped, is the
prompt; `description` in the frontmatter is what the command popup shows.
`$ARGUMENTS` in the body is replaced by everything typed after the command,
and `$1`…`$9` by single words (quotes group words), so `/deploy staging` fills
in `staging`. A body with no placeholder gets the arguments added after it.
A `.sh`, `.bash` or `.py` file runs as a command instead, through the same
hooks and permission gate as `run_command`.

Commands load from `~/.buildwithnexus/commands/` and `~/.claude/commands/`,
and, once you trust them, from the project's `.buildwithnexus/commands/`
and `.claude/commands/`. Skills and commands work headless too:
`bwn run '/deploy staging'`.

```markdown
---
description: Deploy to an environment
---
Run the deploy for $ARGUMENTS, then check the health endpoint.
```

### Helper agents

The model can hand a subtask to a helper with the `task` (or
`spawn_subagent`) tool, as the built-in `engineer` or `researcher` role or as
a helper you define. An agent file is `<name>.md` with `name`, `description`
and `tools` frontmatter and the helper's instructions as the body; `tools`
limits what it may use, and Claude Code's tool names work (`Read`, `Grep`,
`Bash`, …). A helper with a `tools` list can never delegate further (`task`
and `spawn_subagent` are left out), and a call naming an unknown role fails
with the list of roles. `/agents` lists the helpers.

```markdown
---
name: test-writer
description: Writes focused unit tests for one module
tools: Read, Grep, Glob, Write
---
Write tests for the module you are given. Do not change the module itself.
```

A helper started with `read_only: true`, or from an agent file with
`read_only: true` or a `tools` list that only reads (`Read, Grep, Glob`),
can read and search but never change anything. Read-only helpers and
isolated ones (`isolate: true`) that the model starts in the same reply run
at the same time, up to `max_parallel_helpers` at once (default 3; `1`
runs them one after another). Isolated helpers take turns while folders
added with `--add-dir` are in use: a worktree does not cover those, and each
helper may write there. Each shows its work in one labelled block
when it finishes, a helper that needs an approval says which one it is,
and Esc or Ctrl+C stops them all. Helpers that write in your folder always
run one after another. Their tokens and cost count toward the session.

Agent files load from `~/.buildwithnexus/agents/` and `~/.claude/agents/`,
and, once you trust them, from the project's `.buildwithnexus/agents/`
and `.claude/agents/`. A helper that runs isolated in a git worktree shows
its branch and the merge command when it finishes (a `subagent_result` event
in `--json` mode), and commits with your git identity. To run a whole session
on its own branch, start it with `--worktree <name>`.

### Project commands, skills and agents

A repository's command files speak to the model in your name, its skills
and agents steer it, and a script command runs code, so they stay off until
you trust them. The folder trust prompt lists each one with its file
(`command /deploy (.claude/commands/deploy.md)`), also when the repository
has nothing else to trust, and trusting pins their contents: adding, editing
or removing one asks again at the next start. Until then a startup notice
names them and how to trust, and typing one says it is off. A file linked to
somewhere outside its folder never loads. `buildwithnexus trust --print`
lists them for CI. Run from your home folder, these folders are your own and
load as such.

## MCP servers

buildwithnexus is a full [Model Context Protocol](https://modelcontextprotocol.io)
client. Configure servers under `mcp_servers` in `~/.buildwithnexus/settings.json`
(user) or `.buildwithnexus/settings.json` (project); both are merged.

```json
{
  "mcp_servers": {
    "fs":     { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."],
                "env": { "LOG_LEVEL": "warn" } },
    "remote": { "url": "https://mcp.example.com/mcp",
                "headers": { "Authorization": "Bearer …" }, "timeout_secs": 15 },
    "old":    { "command": "legacy-server", "enabled": false }
  }
}
```

| Key | Meaning |
|---|---|
| `type` | `"stdio"` or `"http"` (Streamable HTTP). Optional: a `url` means http, a `command` means stdio. |
| `command`, `args`, `env` | stdio: the process to spawn, kept alive for the whole session; stderr is captured for diagnostics. |
| `url`, `headers` | http: the endpoint and extra request headers (a static token goes here). `Mcp-Session-Id` is tracked automatically. |
| `oauth` | http: a client registered ahead of time, for a server whose authorization server has no dynamic registration: `client_id`, optional `client_secret`, `scopes`, and `callback_port` when the redirect URI must be exact. |
| `timeout_secs` | Per-request deadline (default `30`). A server that hangs or exits is reported once and its tools drop out for the session. |
| `enabled` | `false` keeps the entry but never connects. |

Servers connect lazily in the background on the first prompt (headless runs
connect before the first request, waiting at most 5 s, or a server's own
`timeout_secs`, and skip a server that is not ready with a notice); each one logs
`mcp: <name> connected, N tools` or its error. Discovered tools are advertised
to the model as **`mcp__<server>__<tool>`** with the server's own description
and input schema, and answer over the persistent connection. They count as
mutating under the permission gate (prompted under `ask`, blocked under
`readonly`). A server's `readOnlyHint: true` annotations are only honoured
when you set `"trust_read_only_hints": true` on that server, because the hint
is the server's own claim. The older
`mcp_call` tool (`server`, `tool`, `arguments`) still works over the same
connection.

```
/mcp                                   servers: transport, status, tool count
/mcp <name>                            a server's tools with descriptions
/mcp add [--force] <name> <command> [args...]   stdio server → settings.json, then reconnect
/mcp add [--force] <name> --url <url> [--header K=V]... [--timeout <secs>]
        [--client-id <id>] [--callback-port <port>]
/mcp remove <name>
/mcp login <name>                      sign in to an http server that asks for OAuth
/mcp logout <name>                     revoke and forget that sign-in
/mcp reload                            reconnect every server
```

`buildwithnexus mcp list|<name>|add|remove|login|logout|reload` mirrors this
for scripts (`add`/`remove` only edit the settings file; `list` connects).
Everything after the server name is the server's own command line, so
`buildwithnexus mcp add fs npx -y some-server --json` keeps `-y` and `--json`
for the server; a `--` before the command, as Claude Code writes it, is
accepted. `add` refuses
to replace a server that already has that name (exit 1) unless you pass
`--force`. `/doctor` and
`buildwithnexus doctor` connect to every configured server and report the
outcome. Legacy SSE-only (`type: "sse"`) servers are not supported.

**Servers that ask you to sign in (OAuth).** An http server that answers
`401` with a `Bearer` challenge is listed as `needs login`; connecting never
opens a browser, and a headless run skips the server with a notice naming
`bwn mcp login <name>`. `bwn mcp login <name>` (or `/mcp login <name>` in a
session, which reconnects it) follows the MCP authorization spec: it reads
the server's protected-resource metadata and its authorization server's
metadata, registers bwn as a client when the server allows that (otherwise
set `oauth.client_id`), and opens the browser on an authorization-code + PKCE
(S256) sign-in that comes back to a one-shot listener on `127.0.0.1`. The
URL is printed too, for a browser elsewhere. The tokens are saved owner-only
in `~/.buildwithnexus/mcp-auth/<name>.json`, only for the server's URL as
configured, are refreshed when they expire, and are sent only over HTTPS or
to this machine. A server that echoes its token back gets `[redacted]` in
place of it before the model, the transcript or the terminal sees it.
`bwn mcp logout <name>` asks the authorization server to revoke the token
and deletes the file. A server with an `Authorization` header in `headers`
is left as configured.

## Editors (Agent Client Protocol)

`buildwithnexus acp` speaks the [Agent Client Protocol](https://agentclientprotocol.com)
(version 1) on stdin and stdout, so Zed, JetBrains IDEs and Neovim plugins
that run ACP agents can drive bwn: the editor shows the streamed reply, each
tool call with its diff, the plan, and the approval questions. In Zed, add it
to `settings.json`, then pick it from the Agent Panel's new-thread menu:

```json
{
  "agent_servers": {
    "buildwithnexus": {
      "type": "custom",
      "command": "bwn",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

Other ACP clients take the same command and arguments. bwn uses the provider,
model and key you set up in the terminal (`~/.buildwithnexus`); put an API key
or `NEXUS_HOME` in `env` to use others, and the options of
[Headless and CI](#headless-and-ci) in `args` (for example
`["acp", "--permission-mode", "accept-edits"]`). If the editor cannot find
`bwn`, give the full path that `command -v bwn` prints.

What carries over from the terminal:

- **Approvals.** Every approval the terminal would ask for is asked in the
  editor with *Allow once*, *Always allow* (the terminal's `a`: remembered for
  this project in your settings) and *Reject* (told to the model as a
  denial). Permission modes, allow/ask/deny rules, hooks and the sandbox apply
  as in the terminal. The model's `question` tool gets no answer.
- **Folder trust.** A project's `.buildwithnexus` settings are asked about
  in the editor at the first prompt, with the same questions; a yes is
  remembered as in the terminal, and `--trust-project` works too. A project
  hook whose script changed since you trusted it is skipped with a warning,
  as in headless runs.
- **Modes.** Build, Plan and Brainstorm are the session's modes. In Plan the
  plan arrives as the editor's plan, and building it is a question of its own;
  approving it switches the session to Build.
- **Files.** When the editor offers it, `read_file`, `write_file`,
  `edit_file` and `multi_edit` read and write through the editor, so they see
  unsaved changes and the editor tracks each edit. Commands run in bwn's own
  shell.
- **Sessions.** Each editor thread is a bwn session saved like any other;
  the editor can reopen one (`session/load` replays it), and `bwn resume`
  continues it in the terminal.
- **Cancel** stops the turn at once, including an open question.
- **MCP servers** the editor passes (stdio or HTTP) join the ones in your
  settings; SSE servers are skipped.

One `bwn acp` process serves one folder, and runs one prompt at a time;
helpers a reply starts run one after another, so the editor can show each
one's calls under it.
Everything bwn prints besides the protocol goes to stderr, which the editor
keeps as the agent's log. Images and embedded files in a prompt are sent to
the model (images only when it takes them); audio is not.

## Build from source

```bash
cargo build --release --manifest-path harness/Cargo.toml   # → harness/target/release/buildwithnexus
bash scripts/vendor.sh                                      # vendor deps for offline / reproducible builds
```

The npm package is a thin, inert wrapper — **no install scripts, no network
code, no bundled sources**. The binary is not in the tarball: on first run the
launcher fetches the release asset for your platform and verifies its SHA-256
checksum; every asset carries a build-provenance attestation
(`gh attestation verify`). Nothing is installed from `buildwithnexus-<os>-<cpu>`
packages, and 0.12.1-0.14.2, which reference them, should not be installed. Non-interactive environments opt in with `bwn --bootstrap` or
`BWN_ALLOW_BOOTSTRAP=1`; or build from source and point `BWN_BIN` at the result.

## Platform support

Linux and macOS are the primary targets. Native Windows (PowerShell, cmd,
Windows Terminal) and WSL are supported with the differences below. Nothing
here needs extra dependencies — Windows integration shells out to the
built-in tools (`powershell.exe`, `cmd.exe`, `icacls`, `tasklist`,
`taskkill`).

Works on native Windows:

- The full TUI (alternate screen, raw mode, mouse, bracketed paste) via
  crossterm. Ctrl+Break, closing the console window, logoff and shutdown
  restore the terminal before the process ends; panics restore it too.
- Ctrl+V paste of a clipboard **image** (PNG via `powershell.exe`
  `Get-Clipboard -Format Image`) and clipboard text (`Get-Clipboard -Raw`).
  WSL uses the same PowerShell path with a base64 round-trip. Windows
  Terminal shows the half-block preview and the OSC 9;4 taskbar progress.
- `~/.buildwithnexus/.env.keys` and `settings.json` are restricted to the
  current user with `icacls /inheritance:r /grant:r` — the ACL equivalent of
  the `0600` mode used on Unix. A failure is reported as a dim warning and
  never blocks the save.
- Background dev servers (`start_server` / `list_servers` / `stop_server`)
  without tmux: liveness via `tasklist /FI "PID eq <pid>"`, shutdown via
  `taskkill /PID <pid> /T /F` (the recorded pid is the `cmd /C` wrapper;
  `/T` takes its children down with it).
- Hook scripts by extension: `.ps1` → `powershell.exe -NoProfile
  -ExecutionPolicy Bypass -File`, `.cmd`/`.bat` → `cmd.exe /C`, `.py` →
  `python3` if it runs, else `python`. `.sh`/`.bash` run with `sh`/`bash`
  from PATH (Git for Windows provides both); without them the hook fails with
  an error naming the missing interpreter instead of a silent `sh` spawn
  failure. Shell-string hooks and the `bash`/`run_command` tools run under
  `cmd /C`.
- `~` and `~/…` (also `~\…`) resolve against `%USERPROFILE%` when `HOME` is
  unset. Temp files use the system temp directory everywhere.

Still Unix-only:

- tmux-backed dev servers (Windows uses the plain background-process path).
- Copy-to-clipboard from the transcript is emitted as OSC 52 on every
  platform; the extra `clip.exe` / `pbcopy` fallback only runs on WSL and
  macOS, so on native Windows it relies on the terminal honouring OSC 52
  (Windows Terminal does).
- Auto-discovery of `~/.buildwithnexus/hooks/<Event>/` scripts still looks
  for `.sh`/`.bash`/`.py`/`.rs` only; `.ps1`/`.cmd`/`.bat` hooks must be
  listed in `settings.json` as `type: "script"`.
- The `python_tool` runner always invokes `python3`.
- `xdg-open`-style helpers, `/proc`-based WSL detection and Unix signal
  handling (`SIGTERM`/`SIGHUP`) have no Windows equivalent beyond the above.

### Managed and corporate machines

Endpoint protection such as CrowdStrike Falcon or Microsoft Defender may
block or quarantine the Windows binary: it is new, has few installs, and the
npm launcher downloads it on first run and then executes it. The file itself
matches the release checksum and attestation. On a managed machine, ask IT to
allowlist it. When a blocked binary cannot start, the launcher prints its path
and SHA-256 and names the endpoint product it finds, and it does not download
the binary again on its own. [SECURITY.md](SECURITY.md#for-it-and-security-teams) lists what
bwn runs, which hosts it connects to and which files it writes, explains how
to verify a release, and includes a request you can send. Each Windows `.exe`
carries a version resource and an `asInvoker` manifest, and each release has
a CycloneDX SBOM (`buildwithnexus.cdx.json`).

### Behind a proxy

bwn's own connections (model providers, the web tools, MCP servers over
HTTP, the update check) use the standard proxy variables: `HTTPS_PROXY`,
`HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY`, in either case. `NO_PROXY` takes
host names (`corp.example` also covers its subdomains; `.corp.example` and
`*.corp.example` mean the same), IP addresses, CIDR blocks such as
`10.0.0.0/8`, and `*`. The proxy URL must be `http://`, optionally with
`user:password@`; an `https://` proxy URL fails at once. Local model servers
on `localhost`, `127.0.0.1`, `::1` or `0.0.0.0` are always reached directly,
and so are model servers on private, link-local, CGNAT (100.64/10) or IPv6
unique-local addresses, or host names that resolve only to them, as they were
before 0.15 (`BWN_PROXY_PRIVATE=1` sends those through the proxy). A proxy
failure names the proxy and its variable: not answering, 407, rejected
credentials, or a refused tunnel.

HTTPS is checked against bwn's bundled roots plus the operating system's
certificate store, so a TLS-inspecting proxy whose root certificate IT
installed works without extra steps. If the root is not in the OS store, set
`SSL_CERT_FILE` to a PEM file holding it (or `SSL_CERT_DIR` to a directory
of them); these replace the OS store, and the bundled roots still apply. An
untrusted certificate fails at once with `UnknownIssuer` and names these
settings.

To connect as bwn 0.14 did, run it with `NO_PROXY='*'` (no proxy) and
`BWN_TLS_ROOTS=bundled` (bundled roots only).

## Safety

- Default permission is **ask** — every file write, edit, and command is
  confirmed. `accept-edits`, `auto` ("yolo") and `readonly` are opt-in, and
  deny rules hold in every mode.
- Mutating file tools (write/edit/patch) are confined to the working directory —
  writes outside it require explicit confirmation. Reads are unconfined, but
  sensitive paths (the key store, `~/.ssh`, `.env`, `*.pem`) require
  confirmation even in `auto`. Catastrophic commands (`rm -rf /`, `mkfs`, …) too.
- API keys are never sent to a non-HTTPS endpoint, and key-like tokens are
  redacted from surfaced errors.
- In non-interactive / `--json` runs, anything that would prompt is denied
  rather than blocking, and the run exits 3.
- Commands the agent runs never inherit provider keys (`*_API_KEY`,
  `*_API_TOKEN`, `HF_TOKEN`); see [Permissions](#permissions).
- The file tools (`grep_files`, `find_files`, `find_paths`, `list_tree`) and
  `@` completion skip what `.gitignore` ignores and Python virtualenvs, and
  never list `.env` and other sensitive files, even when a `.gitignore` line
  un-ignores them. `read_file` still reads an ignored file by path.
- Every write is checkpointed before it happens; bare `/undo` reverts the
  whole last agent turn and asks before overwriting your own later edits.
  Failure modes, checkpoint mechanics, and what is deliberately **not**
  protected: [RECOVERY.md](RECOVERY.md).

## License

MIT
