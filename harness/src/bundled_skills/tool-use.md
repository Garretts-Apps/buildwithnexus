# Tool Use

Use this skill whenever the task requires acting through buildwithnexus tools.

## Available tools

- `read_file`: read UTF-8 files, optionally with `start_line` and `end_line`.
- `read_many_files`: read several related text files in one call.
- `list_dir`: list directory entries.
- `list_tree`: recursively list a bounded tree.
- `file_info`: inspect file or directory metadata.
- `find_files`: recursively find files by simple glob pattern.
- `grep_files`: literal text search with optional file pattern.
- `write_file`: create or overwrite a file.
- `edit_file`: replace one unique text occurrence in a file.
- `multi_edit`: apply several exact replacements to one file.
- `apply_patch`: apply a unified diff through `git apply`.
- `create_dir`, `move_path`, `remove_path`: basic filesystem mutation.
- `run_command`: run shell commands and return output.
- `todo_write`, `todo_read`: maintain a structured task list.
- `create_docx`: create a simple Word document from title and body.
- `fetch_url`: HTTP GET for a specific URL.
- `web_search`: DuckDuckGo-backed web search.
- `save_memory`: persist durable user preferences or facts.
- `spawn_subagent`: delegate a self-contained task to a fresh agent context.
- `finish`: complete the task with a short summary.

## Choosing tools

- Read/search/list tools are the default first step.
- Use `run_command` for native project commands and checks.
- Use `fetch_url` for a known page and `web_search` when the target page is unknown or current information is needed.
- Use `spawn_subagent` only for clearly separable work, such as independent research or a parallel audit.
- Use mutating tools only after understanding the current file contents.

## Evidence

Tool calls, results, hook executions, skills, and subagents are visible in the TUI trace. Use `/trace` to list events and `/trace <id>` to inspect inputs, triggers, outputs, and decisions.
