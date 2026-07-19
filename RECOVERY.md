# Failure and recovery

What breaks, what the agent does about it, and what you can rewind. Every
mechanism here is exercised by the test suite; file references point at the
implementation.

## Checkpoints (`harness/src/checkpoint.rs`)

Before any mutating tool call touches a file — `write_file`, `edit_file`,
`multi_edit`, `move_path`, `remove_path`, `create_docx`, artifact publishes —
the execution layer records a checkpoint:

- **What's captured:** the file's full prior contents (UTF-8, up to 2 MiB),
  its path, the action, whether it existed, and a timestamp + sequence id.
  Files that are too large or not valid UTF-8 are recorded as
  *not snapshotted*, and restore **refuses to overwrite them** rather than
  clobbering them with an empty string.
- **Where:** `~/.buildwithnexus/checkpoints/`, one JSON file per checkpoint.
  Ids are `{ms}-{seq}-{action}` — the sequence number exists because a fast
  multi-file batch can record several checkpoints in one millisecond.
- **Recording is unconditional.** The permission gate decides whether a write
  happens; the checkpoint happens whenever it does, so an approved-but-wrong
  edit is always recoverable.

### `/undo` (alias `/rewind`)

| Form | Restores |
|---|---|
| `/undo` (bare) | **every file the last agent turn touched**, as a unit — the recovery for a partial multi-file edit (agent changed 3 files and broke 2, or Esc landed mid-batch). Restore order is newest-first, so a file edited several times in the turn ends at its pre-turn contents. |
| `/undo latest` | the single most recent checkpoint (one file) |
| `/undo <id>` | one specific checkpoint (`/checkpoints` lists them) |
| `/undo all` | every checkpoint from the last 24 hours |
| `/undo git` | `git checkout -- .` — discards **all** unstaged changes, including yours |

Restored checkpoints are consumed: a second bare `/undo` tells you the turn
has nothing left to revert instead of silently rewinding older history.

## Agent-loop failure paths (`harness/src/agent.rs`)

Every failure below is turned into feedback the model can act on — an error
that reaches the loop is a course-correction, not a crash:

- **Unknown tool name** → the result names the nearest real tool
  ("Did you mean `read_file`?") and lists the valid surface.
- **Unparseable tool arguments** → flagged (`INVALID_ARGS`) and fed back with
  the tool's required parameters. Truncated streaming tool calls get the same
  flag instead of being half-executed.
- **Malformed tagged tool calls** (local models emitting `<tool_call>{…}` with
  broken JSON) → one corrective reprompt asking for a canonical re-emit,
  bounded to a single retry so a model that can't produce valid JSON still
  terminates. Without this the raw markup would have been the "final answer".
- **Output truncated at the token limit** → the possibly-incomplete tool call
  is discarded and the model is asked to continue/re-issue with smaller
  writes; bounded to a few continuations.
- **Empty reply** → one retry, then the run ends honestly.
- **Repeated identical failures** → a loop guard notices the same tool call
  failing the same way, nudges the model to change approach, and stops the
  run if it doesn't. A stuck agent halts; it does not burn tokens in a circle.
- **Transient HTTP failures** (429/5xx/transport) → automatic retries with
  backoff, *visibly* — the UI prints what's being retried and for how long.

## Sessions (`harness/src/session.rs`)

Saves are atomic (temp file + rename): a crash mid-save can't corrupt the
session on disk. On `/resume`, corrupt session files are skipped — one bad
file never blocks access to the healthy ones.

## Background workflows (`harness/src/workflow.rs`)

Pending `/schedule` and `/loop` workflows persist to
`~/.buildwithnexus/workflows.json` on every mutation (atomic writes; the file
is removed when the queue empties). The next interactive launch restores the
queue and says so: `⟳ restored N scheduled workflows from the previous
session`. A workflow that was mid-run when the process died re-fires after
restart. Loop iteration counts and next-fire times carry over.

## Install and update failures

- The npm bootstrap downloads only from GitHub's own hosts and **refuses any
  binary whose SHA-256 doesn't match** the published checksum; a failed or
  interrupted download is cleaned up, never half-installed (temp + rename).
- Auto-update defaults to notify-only; a broken update can't be silently
  installed unless you opted into `auto_update: "install"`.
- Broken settings files are diagnosed at startup with the exact line and
  column, and the CLI **refuses to re-run onboarding over them** — a typo in
  your config is a fix, not a first run, and never a reason to overwrite it.

## What is *not* protected

Honesty section. Permission gates and checkpoints are guardrails, not a
sandbox:

- **External side effects can't rewind.** A `run_command` that called an API,
  pushed a branch, or dropped a database table is outside checkpoint reach.
  Checkpoints cover file contents in the workspace, nothing else.
- **`/undo git` is a sledgehammer** — it discards unstaged changes you made
  by hand too.
- **Non-UTF-8 and >2 MiB files** are guarded (restore refuses to clobber
  them) but not restorable from checkpoints — use git for those.
- Process isolation, network egress control, and OS-level sandboxing are not
  provided. Run untrusted tasks in a container; see SECURITY.md.
