# Project instructions (AGENTS.md / CLAUDE.md)

The rest of the [Project instructions](../README.md#project-instructions-agentsmd--claudemd) section.

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
AGENTS.md, src/AGENTS.md`, then `use them? y yes · n no · r review [N]`. Only
`y` uses them (and is remembered); Enter, `n` or Esc keeps them out of the
prompt for this session and asks again next launch, and `r` shows them. A task
typed while the question is up is not an answer: it waits in the input box.
After a `y` the line just names them, until a file changes. A headless run, or a session with prompts piped in, cannot ask, so until
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

