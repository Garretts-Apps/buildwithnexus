# Hook examples (opt-in)

bwn does **not** run these hooks until you copy them into your settings. There is no built-in hook runner in the binary; wire them yourself from `~/.buildwithnexus/settings.json` or a trusted project `.buildwithnexus/settings.json`.

## Setup

1. Copy every `*.py` from `harness/bundled_workflow/scripts/` into `~/.buildwithnexus/hooks/` (or a path you control).
2. Merge `settings.fragment.json` into your settings `hooks` object. Point each `command` at your copy of the script.
3. On Windows, use `python` (or `py -3`) in the command string; on Linux and macOS, `python3` works too. Scripts use only the Python standard library.

## What each script does

| Script | Event | Behavior |
| --- | --- | --- |
| `guard_shell.py` | PreToolUse (shell) | Blocks a short list of destructive shell patterns |
| `guard_writes.py` | PreToolUse (write/edit) | Blocks writes outside the session `cwd` |
| `screen_prompt.py` | UserPromptSubmit | Exit 2 when the prompt looks like it contains a credential |
| `strip_invisible.py` | PostToolUse (write/edit) | Removes invisible Unicode from the file just written |

Exit code `2` means deny (PreToolUse / UserPromptSubmit). PostToolUse hooks only add context; `strip_invisible.py` edits the file in place and always exits 0 on success.

## Tests

From the repo root (after copying scripts to a temp dir):

```bash
echo '{"tool_input":{"command":"rm -rf /"}}' | python3 guard_shell.py; echo exit:$?
```

A block returns exit 2.