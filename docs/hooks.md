# Hooks

The rest of the [Hooks](../README.md#hooks) section.

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
warning in a headless run. The prompt is one screen with one question; when
the project sets `base_url` (where your requests and key go) or `permission`,
`e` trusts everything except those. A later prompt names the file that
changed since you trusted the folder.
For CI, see *Trusting a repository in CI* under [Headless and CI](../README.md#headless-and-ci).
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
`transcript_path` (a JSON Lines file since 0.20: a header line, then one
`{"msg": …}` line per message; see [docs/VERSIONING.md](VERSIONING.md)), `permission_mode` (`ask` | `accept-edits` | `auto` | `readonly`), `cwd`, plus
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
[`examples/settings.json`](../examples/settings.json).

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
failed hook (reported as `answered HTTP <status>`). A host that `network.deny`
names is refused. The trust prompt shows a project's http hook URLs (header
names only). A header value can take a secret from the environment:
`"Authorization": "Bearer $HOOK_TOKEN"` with `"allowed_env_vars": ["HOOK_TOKEN"]`
fills in only the variables that list names, so a repository's settings cannot
read the rest of your environment.

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

