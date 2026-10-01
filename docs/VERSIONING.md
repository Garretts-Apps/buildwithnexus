# Versioning

buildwithnexus uses `MAJOR.MINOR.PATCH` version numbers. Until 1.0:

- A **minor** release (0.15.0) may break the interfaces listed below. Its
  CHANGELOG entry has an "Upgrading" section that names each break and the
  setting or flag that brings the old behavior back, or why there is none.
- A **patch** release (0.15.1) only fixes defects. It does not remove or
  rename anything below or change what it means. A fix can still change
  behavior that was wrong, such as a check that let something through.

From 1.0, a break in these interfaces needs a major release.

package.json, `harness/Cargo.toml`, `bwn/Cargo.toml` (its own version and
its `buildwithnexus` dependency), `Cargo.lock` and the npm package always
carry the same version; CI fails when they differ, and fails a version with
no tag yet until CHANGELOG.md has its `## [x.y.z]` section.

## What is covered

| Interface | Breaking | Not breaking |
|---|---|---|
| CLI subcommands and flags (`run`, `plan`, `--json`, `--permission-mode`, …) | removing or renaming one; changing what an existing value does | a new subcommand, flag or value |
| Settings keys in `settings.json` | removing or renaming a key; changing the meaning of a value; a new default that changes what an existing setup does | a new key whose default keeps today's behavior |
| `--json` events on stdout | removing or renaming an event type or field; changing a field's type or meaning | a new event type; a new field |
| Session files in `~/.buildwithnexus/sessions/` | a file an earlier version wrote no longer loads | new fields |
| Exit codes (see README, Headless and CI) | a code changing meaning | a new code for a case that had none |
| Hooks (see Hooks below) | removing an event or a payload field; changing what an exit code or a decision does | a new event, payload field, hook type or decision |
| `buildwithnexus acp` | dropping the protocol version it speaks (ACP 1) or a mode id (`build`, `plan`, `brainstorm`) | a new capability, mode or kind of update |
| The GitHub Action (`action.yml`) | removing or renaming an input or output; changing what a value does | a new input or output |

Human-readable output, TUI layout, log wording, and files under
`~/.buildwithnexus/` other than sessions and settings are not covered.

## Schema versions

Every `--json` event and every session file has an integer `schema_version`,
now `1`:

```json
{"schema_version":1,"summary":"wrote the file","type":"finish"}
```

It goes up only for a breaking change to that format, and only in a minor
release (a major one from 1.0). New event types and new fields keep the same
number, so readers should ignore fields and event types they do not know.

Session files written before 0.15 have no `schema_version`. They still load,
read as version 1 (the layout is the same), and get the field the next time
they are saved. Versions before 0.15 ignore the field, so they can still
load sessions that 0.15 saved.

The event types in schema 1:

| `type` | Fields |
|---|---|
| `assistant` | `text` |
| `tool_call` | `name`, `input` |
| `tool_result` | `name`, `content`, `is_error` |
| `tool_denied` | `reason` |
| `diff` | `path`, `added`, `removed` |
| `plan` | `steps` |
| `verify` | `status`, `report` |
| `finish` | `summary` |
| `error` | `message` |
| `notice` | `message` |
| `result` | `outcome`, `exit_code`, `session_id`, `turns`, `tokens_in`, `tokens_out`, `cost_usd`, `denied`, `denials` (each `tool`, `summary`, `reason`; at most 20 listed), and `unpriced_requests` when a model had no price (last event of a headless run) |
| `finding` | `severity`, `path`, `line`, `message` (`buildwithnexus review`; `path` and `line` may be null) |
| `subagent_result` | `task`, `branch`, `commits`, `merge`, `message` (an isolated helper's work) |
| `session` | `id`, `title`, `cwd`, `model`, `created_ms`, `updated_ms`, `messages` (`--json sessions`, newest first) |
| `check` | `name`, `status` (`ok`, `warn`, `fail` or `info`), `detail` (`--json doctor`) |
| `update` | `current`, `latest`, `behind` (`--json update`) |

Events from helpers that ran at the same time (read-only or isolated
`task` calls from one reply) are written together when each helper
finishes, and each carries `helper`: its place among the helpers started
together, from 1. The `tool_result` of each `task` call follows, in call
order.

`turns` counts model requests, `tokens_in` includes cached input, and
`cost_usd` is estimated from the price table; requests to a model with no
price are counted in `unpriced_requests`, never guessed. `denied` counts every
refused call, and `denials` lists the first 20.

The `outcome` values and exit codes of a headless run:

| Exit code | `outcome` |
|---|---|
| 0 | `success` |
| 1 | `failed` |
| 2 | (usage error; no run, so no `result` event) |
| 3 | `approval_blocked` |
| 4 | `hook_blocked` |
| 5 | `budget_stop` |
| 6 | `step_limit` |
| 7 | `check_work_failed` |
| 8 | `verification_failed` |
| 9 | `review_blocking` (`buildwithnexus review` found a blocking issue) |
| 130, 143 | `interrupted` (SIGINT, SIGTERM) |

A headless run also exits 2, before any request, when the task is a
repository command that is not trusted (`bwn run '/deploy'`). A
`check_work` call nobody could approve (no terminal) is not counted as a
blocked change: it does not make the run `approval_blocked`, and the run
says the checks were not run.

Outside headless runs: `buildwithnexus update --check` exits 10 when a newer
release exists, `buildwithnexus doctor` exits 1 when a check fails,
`buildwithnexus mcp add` exits 1 when the name already exists (without
`--force`), `buildwithnexus mcp login` and `logout` exit 1 when signing in or
out fails (2 for a usage mistake), and `buildwithnexus acp` exits 2 when
anything follows it on the command line.

## Session files

A session file holds `schema_version`, `id`, `title`, `cwd`, `model`,
`created_ms`, `updated_ms` and `msgs`, plus `name` once the session is renamed
with `/rename`. Since 0.15, ids are `<16-digit milliseconds>-<8 hex digits>`;
16-digit ids from 0.14 still load and resume.

## Hooks

Hooks in `settings.json` receive one JSON object on stdin (or as the body of
an `http` hook's POST) with `hook_event_name`, `session_id`,
`transcript_path`, `permission_mode`, `cwd` and the event's own fields.
`tool_input` carries bwn's field names and Claude Code's beside them
(`file_path`, absolute, `old_string`, `new_string`, `content`, `prompt`,
`subagent_type`, `glob`, `path`, `todos`).

| Event | Fields | What its answer does |
|---|---|---|
| `SessionStart`, `SessionEnd`, `PrePrompt`, `PostResponse`, `OnError` | | nothing |
| `UserPromptSubmit` | `prompt` | exit 2 blocks the prompt (headless: exit 4); stdout is added to it as context |
| `PreToolUse` | `tool_name`, `tool_input` (once per file for `move_path`, `read_many_files` and `apply_patch`) | exit 2 or `permissionDecision: "deny"` blocks the call; `"allow"` skips the prompt (your own hooks only) |
| `PermissionRequest` | `tool_name`, `tool_input`, `message` | `decision.behavior` `allow` (your own hooks only), `deny` or `ask`; exit 2 denies |
| `PostToolUse` | `tool_name`, `tool_input`, `tool_response` | `{"decision":"block","reason":…}`, `hookSpecificOutput.additionalContext`, or stderr with exit 2 is added to the tool result |
| `Stop`, `SubagentStop` | `stop_hook_active` | exit 2 or `{"decision":"block","reason":…}` sends the reason as the next message, at most 3 rounds |
| `PreCompact` | `trigger` (`auto`, `manual`), `custom_instructions` | nothing |
| `Notification` | `notification_type` (`permission_prompt`, `question`, `done`, `idle_prompt`), `message` | nothing |

A matcher matches the tool name for tool events, the trigger for
`PreCompact` and the notification type for `Notification`. Claude Code tool
names in a matcher stand for the bwn tools that do the same thing. Hook
`type` is `command`, `python`, `script` or `http`; an `http` hook's response
body counts as its stdout and a status outside 2xx as exit 1. Exit 1 or any
other failure is a warning shown to you, never to the model, except where
`"on_error": "deny"` makes a failing `PreToolUse` guard block.
