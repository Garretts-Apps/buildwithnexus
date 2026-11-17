# Competitor analysis

Research date: 15 November 2026. This compares `buildwithnexus` (`bwn`) at
v0.20.0, plus the additive changes on this branch, with the coding-agent CLIs
people actually reach for. Capability claims for bwn come from this
repository (README, `docs/`, and the harness). Claims for other products come
from their docs or, where noted, from secondary write-ups published in 2026.
Versions move quickly; treat model names and release numbers as a snapshot.

`docs/` is not in the npm package `files` list, and the crates.io crates are
`harness/` and `bwn/`. This file is not part of a published package.

## What bwn already is

bwn is a single Rust binary (workspace crates `harness` and `bwn`) with an
npm launcher. The release binary measured for 0.20.0 is 6.43 MB, `--version`
is about 1.2 ms native and about 27 ms through the Node launcher, and an idle
TUI is about 9 MiB resident (`BENCHMARKS.md`). Default permission is ask.
Shell sandboxing defaults to `auto` as of 0.21.0 (`"sandbox": "off"` opts out).

Already shipping, and easy to undersell when scanning feature lists:

- Providers: Anthropic Messages, OpenAI chat completions, native Ollama, plus
  OpenRouter, Groq, Hugging Face, llama.cpp, LM Studio, and any
  OpenAI-compatible `custom` endpoint. This branch adds first-class Gemini,
  xAI, Mistral, and DeepSeek presets. A bare `gemini…` model name still
  selects OpenRouter, so existing setups are not redirected.
- Local models are a primary path, not a compatibility footnote: Ollama's
  native API (context length, thinking), llama.cpp and LM Studio capability
  probes, and a GGUF scan (`/local`).
- Tools, an MCP client (stdio and Streamable HTTP, OAuth, `readOnlyHint`
  only when the user opts in), and this branch's `include_tools` /
  `exclude_tools` so a server does not have to dump every tool into the
  prompt.
- Permissions: `ask`, `accept-edits`, `auto`, `readonly`, plus allow / ask /
  deny rules, host rules, credential scrubbing of child processes, and a
  project-trust digest for CI (`--trust-project`).
- Optional OS sandbox for shell commands: bubblewrap on Linux, Seatbelt on
  macOS. None on Windows or WSL. Default `off`. `auto` confines only when a
  backend exists and says so when it does not.
- Plan, Build, and Brainstorm in one conversation. Plan can be edited before
  it runs. Headless `plan` needs `--yes` when there is no terminal.
- Helper agents (`task` / `spawn_subagent`), including read-only and isolated
  worktrees, with a concurrency cap.
- Automatic compaction when the estimate passes 80% of a known context
  window, plus `/compact`. There is no user-settable threshold.
- A project knowledge base (`/kb`) of structured entities. It is not an
  automatic tree-sitter map of the repo, and there is no embeddings index.
  Search tools honor `.gitignore`.
- Git: `/diff`, `/commit` (drafts, commits only after `c`), `/pr`, `/review`
  (exit 9 when a finding blocks), `/undo` and `/rewind` from checkpoints,
  `--worktree`. Checkpoints cover file-tool writes, not shell side effects.
- Headless `run` / `plan` / `brainstorm` / `continue` / `resume`, `--json`
  events with a stable schema, distinct exit codes, a spend cap
  (`--max-budget-usd`, exit 5), and a GitHub Action.
- `bwn acp` speaks Agent Client Protocol v1 for Zed, JetBrains, and Neovim.
  The editor's `question` tool gets no answer (see `docs/editors.md`).
- Sessions resume, export, and, on this branch, `/fork`.
- `/cost` with a built-in price table. Unknown models show tokens and are
  never given a guessed price. Local runs show `$0.00`.
- Hooks at Claude Code's lifecycle points, including HTTP hooks. A project
  hook can deny and cannot grant.
- Skills (`SKILL.md`), custom slash commands, and agent files, including
  Claude's directories once the folder is trusted.
- `doctor`, checksum-verified npm install, `cargo install --locked`.

## Comparison

Cells are yes, partial, or no. "Partial" means the product has the idea with
a narrower or different shape than the column header. Details are under the
table. Roo Code, Continue, and Plandex are included because they still come
up in comparisons; their status is the important cell.

| Product | Lang / license | Providers and local models | MCP | Sandbox and approvals | Plan mode | Sub-agents | Compaction | Repo map or index | Git | Headless and JSON | Editor | Session resume | Cost or budget | Hooks | Custom commands |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **bwn 0.20** | Rust / MIT | partial, strong local | client | permissions strong; sandbox opt-in | yes | yes | yes, fixed 80% | partial (`/kb`, no auto map) | yes, commit asks | yes, exit codes, GitHub Action | ACP agent | yes, plus `/fork` | yes, hard cap | yes, Claude-shaped | yes |
| Claude Code | not published / commercial | Claude only (API, Bedrock, Vertex, Foundry) | client | OS sandbox opt-in; permission modes | yes | yes, plus background and batch | yes, settable window | partial (LSP, not a repo map) | yes | yes | first-party IDE and desktop | yes, branch and fork | yes | yes, several hook types | yes, skills, plugins |
| Codex CLI | Rust / Apache-2.0 | OpenAI-first; Ollama, LM Studio, custom | client and experimental server | sandbox on by default | approvals more than a plan mode | partial (multi-agent in current releases) | yes | no public tree-sitter map | yes | `codex exec`, JSON | IDE extension, ACP-related | yes | yes | partial | partial |
| Gemini CLI | TypeScript / Apache-2.0 | Gemini (paid Code Assist or API key) | client, stdio / SSE / HTTP | Docker, Podman, or Seatbelt | partial | partial | yes | no | yes | yes, `-p` | terminal-first | yes | partial | partial (extensions) | partial |
| Aider | Python / Apache-2.0 | very wide, including local | no | git as the safety net, not an OS sandbox | architect / code | no | chat, not a summary pass | yes, tree-sitter, ranked | yes, auto-commit | scripting, not an agent JSON protocol | run inside an editor | chat history | tokens | lint and test loop, not lifecycle hooks | no slash-command ecosystem |
| OpenCode | TypeScript / MIT | very wide (Models.dev), including local | client, tools and resources | allow / ask / deny | yes | yes, custom agents | yes | partial | yes, undo | SDK and CLI | desktop, terminal, mobile | yes, several at once | partial | plugins | yes, skills |
| Goose | Rust / Apache-2.0 (AAIF) | wide, including local | native: built-ins are MCP servers | permission modes; not an OS sandbox by default | partial | yes | yes, threshold | no | yes | `goose run`, JSON and stream-json | desktop, CLI, API; ACP to use subscriptions | yes, fork | partial | extensions | recipes (YAML) |
| Cline | TypeScript / Apache-2.0 | bring-your-own, including local | client, stdio and HTTP, OAuth | approve / auto | Plan and Act | partial | yes | partial | yes, worktrees | CLI, `--json`-style output | VS Code, JetBrains, desktop, `--acp` | yes | partial | yes | skills, rules |
| Roo Code | TypeScript / Apache-2.0 | was bring-your-own | client | approve | several custom modes | orchestrator mode | yes | partial | yes | editor-first | VS Code | yes | partial | partial | custom modes |
| Continue | TypeScript / Apache-2.0 | wide, including Ollama and LM Studio | client | tool policy | agent mode | partial | partial | `@Codebase` index deprecated | yes | CLI | VS Code, JetBrains, CLI | yes | partial | rules | prompts |
| Cursor CLI | TypeScript / commercial | Cursor's models; not a local-model product | client | approvals | yes | yes | yes | editor index | yes | yes, CI | Cursor IDE and others | yes | usage billing | yes | slash commands |
| Amp | TypeScript / commercial | several frontier models, routed | client | approvals | yes | yes, agents can message each other | yes | partial | yes | `amp -x` | VS Code, JetBrains, Neovim, Zed, web | yes | usage grant | not a headline feature | plugins |
| Plandex | Go / MIT | bring-your-own, including local | no | changes stay in a review diff until apply | yes, versioned plans | partial | large context by model choice | yes, tree-sitter | yes | CLI | terminal | yes, plan branches | partial | no | no |
| Crush | Go / FSL-1.1-MIT (not OSI) | wide, including Ollama and llama.cpp | client | allow-list, `--yolo` | partial | partial | yes | partial | yes | CLI | terminal | yes | partial | partial | partial |
| ForgeCode | Rust / Apache-2.0 | wide, bring-your-own | client | partial | partial | partial | partial | partial | yes | CLI | VS Code and terminal | yes | partial | no | partial |

Roo Code was shut down on 15 May 2026 (repo archived, last release v3.54.0),
per the October 2026 Cline-vs-Roo write-up. Its custom modes (Code, Ask,
Architect, Debug, Orchestrator) are the idea worth remembering. Cline kept
Plan and Act and is the maintained line (v4.1.26 on 26 October 2026).

Continue's team was acquired by Cursor in June 2026. The Apache-2.0 code
stays public; the VS Code extension 2.0.0 (19 June 2026) is described as the
last release, and the hosted hub was wound down. The `@Codebase` embeddings
index is marked deprecated in its docs.

Plandex development is reported stopped (late 2025). Comparisons still cite
CLI v2.2.1 (16 July 2025). The product's bet, a cumulative diff you apply
only when you accept it, and branched plans, is not how bwn works: bwn
writes the working tree as the permission gate allows and checkpoints file
tools so `/undo` can restore them.

### Notes that do not fit a cell

**Claude Code** is the compatibility target bwn already tracks: permission
names, hook events, tool names in matchers, `CLAUDE.md`, `~/.claude/skills`,
`~/.claude/commands`, `~/.claude/agents`. It goes further on sub-agents
(background sessions, `/batch` across worktrees, dynamic workflows), a
user-settable auto-compact window, LSP-backed code intelligence, plugins,
and hook types that are themselves prompts or sub-agents. Sandbox can wrap
just the shell or the whole process (Seatbelt, bubblewrap). It does not run
local weights. Source is not published.

**Codex CLI** is the closest Rust peer. Sandboxing is the default posture:
read-only until the directory is trusted, then workspace-write with network
off unless configured, Landlock plus seccomp on Linux, Seatbelt on macOS,
experimental Windows. Approvals are `untrusted`, `on-failure`, `on-request`,
or `never`, separate from the sandbox mode. It is an MCP client and can run
as an MCP server (`codex mcp-server`). Local Ollama and LM Studio exist, but
the product is OpenAI-first. bwn's permission rules and CI exit codes are
more explicit; Codex's sandbox is on without installing bubblewrap, including
a path on Windows.

**Gemini CLI** is Gemini-only. As of 18 June 2026, unpaid and Google One
users were pointed at Antigravity CLI (`agy`); Gemini CLI remains the client
for Gemini Code Assist and API keys. Sandboxing is a container
(Docker/Podman) or Seatbelt, and tool-level sandboxing exists. MCP includes
SSE, which bwn refuses. Google's OpenAI-compatible Gemini endpoint, which
the new `gemini` preset uses, is still documented as beta.

**Aider** (v0.86.1 is the last numbered release the October 2026 guide
names; `main` has moved since) is still the reference for a ranked
tree-sitter repository map and for "every edit is a commit". It is a pair
programmer with a file list you curate, not a permissioned tool loop. No MCP
client in its docs. That map is the highest-impact thing bwn does not have.

**OpenCode** (Anomaly, MIT, TypeScript) is the widest provider catalog,
via Models.dev, plus desktop, mobile, and an SDK. MCP covers tools and
resources. Plugins are the extension point. It is the product most people
mean by "open-source Claude Code" in 2026. It is not small: it is a Node
application, not a 6 MB static binary.

**Goose** (Block, now under the Agentic AI Foundation) is the other Rust
agent. Built-in tools are MCP servers, so the same extensions run elsewhere.
Sessions live in SQLite and can be forked. Recipes are shareable YAML
workflows. Permission modes include a model-judged `smart_approve`. An
optional "adversary" reviewer can block sensitive tool calls. It will speak
ACP to use a Claude, ChatGPT, or Gemini subscription. It does not try to be
a tiny single-purpose binary.

**Cline** grew a real CLI (`cline`, `--acp`, `cline mcp`, worktrees, hooks)
beside the editor extension. Plan mode cannot edit or run commands; Act
mode can. That split is stricter than bwn's Plan mode, which is a plan you
then execute in the same conversation.

**Cursor CLI** and **Amp** are commercial. Cursor's CLI is the editor's
agent in a terminal, with MCP, hooks, multi-agent, and CI. It is not a
bring-your-own-local-model tool. Amp (`amp -x` for a headless run) routes
across models, has plugins and sub-agents that can message each other, and
integrates with VS Code, JetBrains, Neovim, and Zed. Neither publishes the
kind of OS-sandbox story Codex does.

**Crush** (Charm) is a Go TUI with broad providers, including Ollama and
llama.cpp, MCP, and an allow-list plus `--yolo`. The license is
FSL-1.1-MIT: source-available, not OSI-approved, converting to MIT two years
after each release. Fine to learn from, not to vendor.

**ForgeCode** is a smaller Rust agent (Apache-2.0) with many providers and
MCP. It shows the same shape as bwn and Goose without bwn's permission,
hook, and CI surface.

**Also worth knowing, not in the table:** OpenHands (Python, MIT) is a
platform with a Docker sandbox more than a single-user CLI. Pi (TypeScript,
MIT) deliberately ships a handful of tools and lets the agent grow its own.
Antigravity CLI (`agy`, Go, proprietary) is Google's current free-tier
coding agent, separate from Gemini CLI. Kilo Code (TypeScript, MIT) carries
the Cline/Roo multi-model lineage. None of these change the gaps below.

## Where bwn is ahead

- Startup and size. A stripped 6 MB binary that answers `--version` in about
  a millisecond is not what the TypeScript CLIs deliver. Codex and Goose are
  native too; their published sizes were not in the sources used here, so
  this is not a claim that bwn is the smallest Rust agent, only that its own
  numbers are unusually explicit.
- The permission gate: mode, rules, credential scrubbing, sensitive paths,
  and a trust digest CI can pin. That is more specific than an allow-list
  plus `--yolo`.
- Headless contract: `--json` schema, exit codes 0–9 and 130/143, budget
  stop, review findings, and a GitHub Action that posts the result.
- Local models as a designed path (native Ollama, server-reported context
  and vision, GGUF listing), which Claude Code, Cursor CLI, and Gemini CLI
  do not offer.
- Claude Code compatibility for hooks, skills, commands, and agent files,
  without being Claude-only.
- Checkpoints and `/rewind` that are not "the only undo is git".

## Where bwn is behind

- No automatic repository map. Aider and Plandex show the model the
  important symbols before it opens files. bwn depends on grep, tree walks,
  and a hand-maintained `/kb`.
- Sandbox is opt-in and missing on Windows. Codex confines by default and
  does not need bubblewrap installed.
- Provider catalog is still a short preset list plus `custom` and
  OpenRouter. OpenCode, Goose, Aider, and Crush cover Azure, Bedrock, and
  Vertex, which need more than an API key in a header.
- MCP resources and prompts are read-only tools (`mcp_resources`,
  `mcp_prompts`), offered only when a connected server published that kind,
  and not pasted into the prompt. Still no legacy SSE, and bwn is not itself
  an MCP server (Codex is).
- Sub-agents do not run as background sessions you can detach, and there is
  no fan-out across worktrees like Claude Code's `/batch`.
- Auto-compact is a fixed 80% of the known window. Claude Code lets you set
  the window.
- In ACP, the model's question tool gets no answer, so an editor session
  cannot unblock a question the terminal would ask.
- There is no plugin catalog. Hooks and skills cover a lot of the same
  ground for one user; they do not distribute.

## Landed on this branch

These three are additive. Existing flags, settings keys, event fields, and
provider ids behave as before. A headless run with no settings file still
prefers an Anthropic, OpenAI, OpenRouter, Groq, or Hugging Face key when one
is set.

| Change | Effort | Impact | Breaking |
|---|---|---|---|
| Presets `gemini`, `xai`, `mistral`, `deepseek` (OpenAI-compatible hosts, own API keys). Bare `gemini…` names still go to OpenRouter. | small | high for anyone who was hand-writing a `custom` endpoint | no |
| `mcp_servers.*.include_tools` and `exclude_tools`. Omit both and every tool is advertised, as before. | small | high: MCP schemas are a common way to blow the context window | no |
| `/fork [name]` copies the conversation into a new session and continues there. The previous file stays; `/resume` opens it. | small | medium: the missing half of resume / new | no |

No prices were added for the new models. `/cost` shows tokens until a model
is in the built-in table or under `prices`. A guessed price would be worse
than a blank one.

## Prioritized next work

Row 1 shipped in 0.21.0. Row 2 is now `mcp_resources` and `mcp_prompts`:
read-only tools, offered only when a connected server published that kind,
and not inserted into the prompt.

Ordered by impact for a coding-agent CLI, then by how contained the change
is. Effort is relative to this codebase: small is a setting or command,
medium crosses a subsystem, large is a new subsystem or a new dependency.

| # | Capability | Effort | Impact | Breaking |
|---|---|---|---|---|
| 1 | Repository map as a tool the model calls, not text stuffed into every prompt. A ranked list of files and symbols (tree-sitter, or a smaller scanner if a new native dependency is rejected). Budget it like Aider's `--map-tokens`. | large | high | no, if it is a tool with a default that does not change today's prompts |
| 2 | MCP resources (and prompts) as read-only context, beside tools. Landed: `mcp_resources` and `mcp_prompts`, not pasted into the prompt. | medium | medium | no |
| 3 | Answer the model's question tool in `bwn acp`. Terminal sessions already ask; editor sessions drop the question. | medium | medium for Zed and JetBrains | no |
| 4 | `compact_ratio` (default `0.8`, today's behavior) and a `/compact auto` readout, so the window is choosable the way Claude Code's is. | small | medium | no, while the default stays 0.8 |
| 5 | A different model for Plan than for Build, set explicitly. Cline's useful split. Default: the session's current model. | medium | medium | no |
| 6 | Opt-in git auto-commit of file-tool edits (`"auto_commit": false` by default). Aider's undo story. bwn's `/commit` stays the default. | medium | medium | no, while the default is off |
| 7 | Publish a JSON Schema for `settings.json` and point `doctor` at it. Catches typos before a run. | small | low | no |
| 8 | Azure OpenAI, Amazon Bedrock, and Vertex as presets. These need deployment names or cloud auth, not just `Authorization: Bearer`. | large | medium | no |
| 9 | `bwn mcp-server`: expose a session as an MCP server, as Codex does, so another agent can call bwn. | large | medium | no (new subcommand) |
| 10 | A Windows sandbox backend, off unless `sandbox` is `auto` or `require` and the backend works. Codex's Windows support is still experimental, which is a hint about the size of this. | large | medium | no, while `off` stays the default and `auto` only changes once a backend exists (see the breaking note) |
| 11 | Opt-in embeddings index for semantic search. Continue deprecated theirs; only do this if the repo map in row 1 is not enough. Keep it off unless configured, and keep the index local. | large | medium | no, if opt-in |
| 12 | Legacy MCP SSE. bwn already speaks Streamable HTTP and refuses `type: "sse"`. | medium | low | no |

Rows 1 and 3 are the ones that change how the agent feels day to day. Rows
4, 6, and 7 are the next small batch after this branch. Rows 8–12 are real
but they do not close a gap users hit on the first afternoon.

## Breaking proposals (need owner approval)

Approved and shipped in 0.21.0. The opt-out for each one is in the changelog
under Breaking changes. The notes below are the original proposal.

1. **Sandbox on by default**, Codex-style: read-only or workspace-write
   until the user relaxes it. Today `sandbox` defaults to `off`, and `ask`
   is the permission default. Turning sandbox on changes which commands
   succeed (network, writes outside the work tree, missing bubblewrap).
2. **Landlock plus seccomp as a backend for the existing `auto` value.**
   `auto` already means "confine when a backend works, otherwise run and
   say so". On Linux without bubblewrap, `auto` currently runs unsandboxed.
   A built-in Landlock backend would start confining those machines. That is
   a behavior change for a settings value people already saved. Ship it as
   a new mode, or accept it as a minor-release break and write it up in
   Upgrading.
3. **Inject a repository map into the system prompt unconditionally.** That
   changes tokens, cost, and answers for every session. The non-breaking
   shape is row 1 above: a tool, or a setting that defaults off.
4. **Route a bare `gemini…` model name to the `gemini` preset.** Today, and
   still after this branch, `/model gemini-2.5-pro` selects OpenRouter as
   `google/gemini-2.5-pro`. Flipping that breaks people who have
   `OPENROUTER_API_KEY` and no `GEMINI_API_KEY`.
5. **Auto-commit on by default.** It rewrites git history as a side effect of
   an edit. Opt-in only (row 6) unless the owner wants Aider's behavior as
   the default.

## Sources

Primary or project docs, checked 15 November 2026:

- Claude Code feature overview and commands: <https://code.claude.com/docs/en/features-overview>, <https://code.claude.com/docs/en/commands>, sandbox: <https://code.claude.com/docs/en/sandbox-environments>
- Codex CLI sandbox and MCP: <https://github.com/openai/codex/blob/main/docs/sandbox.md> and the `codex-rs` README; release rust-v0.161.0 on the GitHub releases page
- Gemini CLI: <https://github.com/google-gemini/gemini-cli>, sandbox <https://geminicli.com/docs/cli/sandbox/>, OpenAI compatibility <https://aistudio.google.com/docs/openai>
- Aider repo map and git: <https://aider.chat/docs/repomap.html>, <https://aider.chat/docs/git.html>
- OpenCode: <https://opencode.ai/docs/providers/>, <https://opencode.ai/docs/mcp-servers/>
- Goose: <https://github.com/block/goose>
- Cline CLI: <https://docs.cline.bot/cli/cli-reference>, Plan and Act: <https://docs.cline.bot/core-workflows/plan-and-act>
- DeepSeek's OpenAI-compatible base URL and model ids: <https://api-docs.deepseek.com/>
- xAI chat completions (`https://api.x.ai/v1`, `grok-4.7`, `XAI_API_KEY`): <https://docs.x.ai/docs/guides/chat-completions>
- Mistral Large 4 API ids (`mistral-large-4`, `https://api.mistral.ai/v1`): <https://docs.mistral.ai/models/mistral-large-4-0.md>

Secondary write-ups used for status (shutdowns, acquisitions, licenses), not
for bwn's own behavior:

- Cline vs Roo Code, including the 15 May 2026 shutdown: <https://kipt.me/guides/cline-vs-roo-code>
- Continue acquisition and Plandex status: <https://faun.dev/toolbox/continue-vs-plandex/>
- OpenCode, Crush, Goose, Codex, Aider, Plandex side by side (Crush's FSL license, release dates through July 2026): <https://builderai.tools/blog/terminal-coding-agents-opencode-crush-goose>
- Cursor CLI and Amp: <https://www.testmuai.com/blog/agentic-coding-cli-tools/>
- Catalog of newer CLIs (ForgeCode, Antigravity CLI, Pi, OpenHands): <https://michaellivs.com/blog/cli-coding-agents-compared/> and <https://terminaltrove.com/compare/ai-coding-agents/>
