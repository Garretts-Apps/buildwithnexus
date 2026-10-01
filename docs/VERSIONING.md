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

Outside headless runs: `buildwithnexus update --check` exits 10 when a newer
release exists, `buildwithnexus doctor` exits 1 when a check fails, and
`buildwithnexus mcp add` exits 1 when the name already exists (without
`--force`).

## Session files

A session file holds `schema_version`, `id`, `title`, `cwd`, `model`,
`created_ms`, `updated_ms` and `msgs`, plus `name` once the session is renamed
with `/rename`. Since 0.15, ids are `<16-digit milliseconds>-<8 hex digits>`;
16-digit ids from 0.14 still load and resume.
