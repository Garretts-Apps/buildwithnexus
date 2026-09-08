# buildwithnexus

[![npm version](https://img.shields.io/npm/v/buildwithnexus?style=flat-square&color=blue)](https://www.npmjs.com/package/buildwithnexus)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg?style=flat-square)](https://opensource.org/licenses/MIT)

A hilariously fast, **agentic AI CLI** — written in Rust. Remote models via
API key, or local models on your machine. It plans, edits files, and runs
commands, asking before each change. One static binary, six direct
dependencies, no runtime to babysit — and a terminal UI built to feel
instant: an incremental wrap cache with atomic frames, live autocomplete,
GitHub-grade diffs, clickable files and links, and multimodal input straight
from your clipboard.

```bash
npm install -g buildwithnexus     # prebuilt binary via platform packages
# or, with a Rust toolchain:
cargo install buildwithnexus --locked   # installs `buildwithnexus` + the `bwn` alias
buildwithnexus
```

The first launch walks you through choosing a model. Then describe a task.
A daily background check tells you when a new version is out; set
`auto_update: "install"` in settings to apply updates automatically, or
`"off"` to silence the check.

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
- **Multimodal input** — `Ctrl+V` pastes clipboard screenshots; `@clip.mp4`
  runs ffmpeg to sample frames + metadata for vision models (text-only models
  get a clear "not multimodal" notice instead of silent drops).
- **Claude-Code-grade ergonomics** — `Esc` interrupts the agent; messages
  typed while it works queue and auto-send; ↑ history is prefix-filtered and
  never destroys your draft; double-click selects a word, triple-click a
  line, and every copy confirms itself in the footer via OSC 52 (terminals
  known to ignore OSC 52 get a one-time notice instead).

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
`~/.buildwithnexus/.env.keys` (0600).

**Reasoning.** `reasoning_effort` in settings (`off` by default, or `low` / `medium` /
`high`; `--effort <level>` per run, `/effort` in-session) maps to each API's
native control: Claude 4.6+ gets adaptive thinking with `output_config.effort`,
older Claude models a `budget_tokens` thinking budget (2048 / 8192 / 16384),
OpenAI reasoning models (`o1`/`o3`/`o4`/`gpt-5`) `reasoning_effort`, and
Ollama's native API `think: true`. Other models — including anything behind a
local OpenAI-compatible server — receive no reasoning parameters at all.

**Cost.** Every request's `usage` block feeds a session ledger: `/cost` shows
input / output / cache-read / cache-write tokens, the request count, and an
estimated dollar figure from a built-in price table (local providers show
`$0.00 (local)`; an unlisted model shows tokens only, never a guessed price).
`--max-budget-usd <n>` (or `max_budget_usd` in settings) stops the agent
before the next model request once the estimate exceeds `n`, with a `notice`
event in `--json` mode.

## Modes

- **PLAN** — decompose the task into steps you approve or edit, then execute.
- **BUILD** — the agentic ReAct loop: read/edit files, run commands, iterate.
- **BRAINSTORM** — free-form chat with read-only tools (read, grep, fetch, read-only commands); never writes. Action-like prompts auto-escalate to BUILD.

```bash
buildwithnexus                 # full-screen interactive session
buildwithnexus run <task>      # execute a task (agentic, headless)
buildwithnexus plan <task>     # decompose, approve, then execute
buildwithnexus brainstorm <q>  # free-form chat (read-only tools)
buildwithnexus init            # (re)configure provider / model / key
buildwithnexus providers       # list built-in providers
buildwithnexus doctor          # diagnose setup (keys, tools, connectivity)
```

Inside the interactive session:

```
/model [name]             hot-swap the AI model mid-session
/effort [off|low|medium|high]  show or set reasoning depth (persisted to settings)
/compact                  compress context (free up token budget)
/context                  context window usage (measured from the last request when known)
/cost                     session tokens by category, request count, estimated cost
/review                   AI code review of current git diff
/commit                   AI-drafted conventional commit message
/pr                       AI-drafted pull request title + description
/schedule <delay> <task>  run a task once in the background (5s, 2m, 1h)
/loop <interval> <task>   run a task repeatedly in the background (up to max_concurrent_workflows at once, default 2)
/workflows                list and manage background workflows (i<id> shows a run's log, kept in ~/.buildwithnexus/workflows/)
/btw <context>            inject context into the next agent turn
/config                   configure hooks, memory, and commands via AI
/memory                   view and edit session memory
/skills                   list skills and custom commands
/trace                    inspect hooks, tools, skills, and subagents
```

## Permissions

Every mutating tool (`write_file`, `edit_file`, `run_command`) passes a gate:
`ask` (default), `auto` (yolo), or `readonly`. Set it during setup. In
`readonly`, mutations are refused outright — never prompted — so an approved
sensitive-path or dangerous-command confirmation can't slip one through.

Answering `a` (always allow) at a prompt remembers that tool **for the current
project only** (`project_allowed` in `~/.buildwithnexus/settings.json`, keyed by
directory). `/permissions reset` forgets those answers for the project you're in.
The legacy global `allowed_commands` list keeps working.

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
temp dirs (`/tmp`, `$TMPDIR`); everything else — including
`~/.buildwithnexus` and tool caches such as `~/.cargo` or `~/.npm` — is
read-only to the command. Set `"sandbox_network": false` to also block the
network inside the sandbox (default `true`, allowed).

**Not confined:** reads (the whole filesystem stays visible), the agent's own
file tools (already fenced to the working directory), hooks, MCP servers, and
`start_server`. The sandbox never approves anything — the permission gate is
unchanged; it only limits what an approved command can touch. Sandboxed
commands are marked `[sandboxed]` on the tool header; `/sandbox status` and
`buildwithnexus doctor` show backend availability and whether commands would
be confined. To escape for one command, switch with `/sandbox off` and back.

## Hooks

Run your own commands at the same lifecycle points as Claude Code, configured in
`~/.buildwithnexus/settings.json` (user) and/or `.buildwithnexus/settings.json`
(project). User hooks are always active; **project hooks run only after you trust
that folder** (you're prompted once, and a project hook may *deny* a tool but
never *grant* one — so cloning a hostile repo can't run or unlock anything).
Events: `SessionStart` / `SessionEnd` (once per process), `UserPromptSubmit`,
`PrePrompt` (before each model request in a BUILD turn), `PreToolUse`,
`PostToolUse`, `PostResponse`, `OnError`, `Stop` (after every BUILD, PLAN,
BRAINSTORM, or chat response), and `SubagentStop` (when a `spawn_subagent` call
returns; its payload carries the subagent's `tool_input`). Each hook command
receives the event as JSON on stdin with Claude Code's field names:
`hook_event_name`, `session_id` (the id the transcript is saved under),
`transcript_path`, `permission_mode` (`ask` | `auto` | `readonly`), `cwd`, plus
the event's own fields (`tool_name`, `tool_input`, `tool_response`, `prompt`).

`PreToolUse` can gate a tool: exit code **2** (or a JSON
`permissionDecision: "deny"`) blocks it — even under `auto`. `"allow"` skips the
prompt; otherwise the normal gate applies. Matchers are `*`, an exact tool name,
or a `|`-separated list; each segment may use `*` and `?` wildcards
(`"*_file"`, `"mcp__*"`, `"Edit|Write"`). See
[`examples/settings.json`](./examples/settings.json).

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "run_command",
        "hooks": [{ "type": "command", "command": "echo 'no shell on main' >&2; exit 2" }] }
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
total. A dim line at startup lists what was found:
`instructions: AGENTS.md, src/AGENTS.md`. `/init` offers to create a starter
`AGENTS.md` (build/test commands, conventions, do-nots) when the cwd has none.
The `instruction_files` settings key changes which names are looked up
(default `["AGENTS.md", "CLAUDE.md"]`; add `"GEMINI.md"`, or `[]` to disable).

The mixed-case `.buildwithnexus/Agents.md` is different: it defines agent
roles/capabilities (`/agents` shows it) and is loaded after the instructions.

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

On a name collision the more specific source wins: project beats user beats
bundled, and a `SKILL.md` folder beats a flat file of the same name. Add more
roots with the `skill_dirs` settings key (`["~/my-skills", "tools/skills"]`).
`/skills` lists every skill with its source and description.

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
| `url`, `headers` | http: the endpoint and extra request headers (auth tokens go here). `Mcp-Session-Id` is tracked automatically. |
| `timeout_secs` | Per-request deadline (default `30`). A server that hangs or exits is reported once and its tools drop out for the session. |
| `enabled` | `false` keeps the entry but never connects. |

Servers connect lazily in the background on the first prompt (`--json`
headless runs connect before the first request); each one logs
`mcp: <name> connected, N tools` or its error. Discovered tools are advertised
to the model as **`mcp__<server>__<tool>`** with the server's own description
and input schema, and answer over the persistent connection. They count as
mutating under the permission gate (prompted under `ask`, blocked under
`readonly`) unless the server annotates them `readOnlyHint: true`. The older
`mcp_call` tool (`server`, `tool`, `arguments`) still works over the same
connection.

```
/mcp                                   servers: transport, status, tool count
/mcp <name>                            a server's tools with descriptions
/mcp add <name> <command> [args...]    stdio server → settings.json, then reconnect
/mcp add <name> --url <url> [--header K=V]... [--timeout <secs>]
/mcp remove <name>
/mcp reload                            reconnect every server
```

`buildwithnexus mcp list|<name>|add|remove|reload` mirrors this for scripts
(`add`/`remove` only edit the settings file; `list` connects). `/doctor` and
`buildwithnexus doctor` connect to every configured server and report the
outcome. Legacy SSE-only (`type: "sse"`) servers are not supported.

## Build from source

```bash
cargo build --release --manifest-path harness/Cargo.toml   # → harness/target/release/buildwithnexus
bash scripts/vendor.sh                                      # vendor deps for offline / reproducible builds
```

The npm package is a thin, inert wrapper — **no install scripts, no network
code, no bundled sources**. The binary is not in the tarball: on first run the
launcher fetches the release asset for your platform and verifies its SHA-256
checksum; every asset carries a build-provenance attestation
(`gh attestation verify`). Per-platform packages (`buildwithnexus-<os>-<cpu>`)
are declared as `optionalDependencies` and are used automatically once
published. Non-interactive environments opt in with `bwn --bootstrap` or
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
  WSL uses the same PowerShell path with a base64 round-trip.
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

## Safety

- Default permission is **ask** — every file write, edit, and command is
  confirmed. `auto` ("yolo") and `readonly` are opt-in.
- Mutating file tools (write/edit/patch) are confined to the working directory —
  writes outside it require explicit confirmation. Reads are unconfined, but
  sensitive paths (the key store, `~/.ssh`, `.env`, `*.pem`) require
  confirmation even in `auto`. Catastrophic commands (`rm -rf /`, `mkfs`, …) too.
- API keys are never sent to a non-HTTPS endpoint, and key-like tokens are
  redacted from surfaced errors.
- In non-interactive / `--json` runs, anything that would prompt is denied
  rather than blocking.
- Every write is checkpointed before it happens; bare `/undo` reverts the
  whole last agent turn. Failure modes, checkpoint mechanics, and what is
  deliberately **not** protected: [RECOVERY.md](RECOVERY.md).

## License

MIT
