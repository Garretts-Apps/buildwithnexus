# buildwithnexus

[![npm version](https://img.shields.io/npm/v/buildwithnexus?style=flat-square&color=blue)](https://www.npmjs.com/package/buildwithnexus)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg?style=flat-square)](https://opensource.org/licenses/MIT)

A hilariously fast, **agentic AI CLI** — written in Rust. Remote models via
API key, or local models on your machine. It plans, edits files, and runs
commands, asking before each change. One static binary, five direct
dependencies, no runtime to babysit — and a terminal UI built to feel
instant: incremental rendering with atomic frames, live autocomplete,
GitHub-grade diffs, clickable files and links, and multimodal input straight
from your clipboard.

```bash
npm install -g buildwithnexus     # prebuilt binary via platform packages
# or, with a Rust toolchain:
cargo install buildwithnexus      # installs `buildwithnexus` + the `bwn` alias
buildwithnexus
```

The first launch walks you through choosing a model. Then describe a task.
Once installed, buildwithnexus keeps itself current: a background check
updates the npm package at most once a day (set `BWN_NO_AUTO_UPDATE=1` to
opt out).

## The TUI

- **Instant startup, never a black frame** — chrome paints before anything
  else; dependency probes and connection warming run off the critical path.
- **Fast rendering** — each transcript line is wrapped once, repaints are
  frame-coalesced (~60fps) and wrapped in synchronized-output brackets, so
  streaming is smooth on kitty/iTerm2/WezTerm/Alacritty with zero tearing.
- **Live autocomplete** — type `/` for a command popup with descriptions;
  `@` completes files, `kb:` symbols. ↑/↓ navigate, Tab/Enter accept.
- **Clean diffs** — line-number gutters, background-tinted rows, word-level
  change highlighting, hunk elision. Same renderer for previews and applied
  changes.
- **Clickable everything** — file paths and links are OSC 8 hyperlinks: click
  a path in an edit header and it opens in your OS default app.
- **Multimodal input** — `Ctrl+V` pastes clipboard screenshots; `@clip.mp4`
  runs ffmpeg to sample frames + metadata for vision models (text-only models
  get a clear "not multimodal" notice instead of silent drops).
- **Claude-Code-grade ergonomics** — `Esc` interrupts the agent; messages
  typed while it works queue and auto-send; ↑ history is prefix-filtered and
  never destroys your draft; double-click selects a word, triple-click a
  line, and every copy confirms itself in the footer.

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

## Models

Two wire protocols cover everything. Pick a provider during setup (or `bwn init`):

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

## Modes

- **PLAN** — decompose the task into steps you approve or edit, then execute.
- **BUILD** — the agentic ReAct loop: read/edit files, run commands, iterate.
- **BRAINSTORM** — free-form chat, no tools.

```bash
buildwithnexus                 # full-screen interactive session
buildwithnexus run <task>      # execute a task (agentic, headless)
buildwithnexus plan <task>     # decompose, approve, then execute
buildwithnexus brainstorm <q>  # free-form chat
buildwithnexus init            # (re)configure provider / model / key
buildwithnexus providers       # list built-in providers
buildwithnexus doctor          # diagnose setup (keys, tools, connectivity)
```

Inside the interactive session:

```
/model [name]             hot-swap the AI model mid-session
/compact                  compress context (free up token budget)
/review                   AI code review of current git diff
/commit                   AI-drafted conventional commit message
/pr                       AI-drafted pull request title + description
/schedule <delay> <task>  run a task once in the background (5s, 2m, 1h)
/loop <interval> <task>   run a task repeatedly in the background
/workflows                list and manage background workflows
/btw <context>            inject context into the next agent turn
/config                   configure hooks, memory, and commands via AI
/memory                   view and edit session memory
/skills                   list skills and custom commands
/trace                    inspect hooks, tools, skills, and subagents
```

## Permissions

Every mutating tool (`write_file`, `edit_file`, `run_command`) passes a gate:
`ask` (default), `auto` (yolo), or `readonly`. Set it during setup.

## Hooks

Run your own commands at the same lifecycle points as Claude Code, configured in
`~/.buildwithnexus/settings.json` (user) and/or `.buildwithnexus/settings.json`
(project). User hooks are always active; **project hooks run only after you trust
that folder** (you're prompted once, and a project hook may *deny* a tool but
never *grant* one — so cloning a hostile repo can't run or unlock anything).
Events: `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`,
`SessionEnd`. Each hook command receives the event as JSON on stdin.

`PreToolUse` can gate a tool: exit code **2** (or a JSON
`permissionDecision: "deny"`) blocks it — even under `auto`. `"allow"` skips the
prompt; otherwise the normal gate applies. Matchers are `*`, an exact tool name,
or a `|`-separated list. See [`examples/settings.json`](./examples/settings.json).

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

## Build from source

```bash
cargo build --release --manifest-path harness/Cargo.toml   # → harness/target/release/buildwithnexus
bash scripts/vendor.sh                                      # vendor deps for offline / reproducible builds
```

The npm package is a thin, inert wrapper — **no install scripts, no network
code, no bundled sources**. The prebuilt binary ships as a per-platform
package (`buildwithnexus-<os>-<cpu>`) selected automatically via
`optionalDependencies`, SHA-256-verified when packaged, with build-provenance
attestations (`gh attestation verify`). Installing with `--omit=optional`
skips the binary — build from source and point `BWN_BIN` at the result.

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

## License

MIT
