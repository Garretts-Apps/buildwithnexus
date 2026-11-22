# Skills

The rest of the [Skills](../README.md#skills) section.

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

On a name collision a `SKILL.md` folder beats a flat file of the same name,
and your own skills beat bundled ones. A skill from the project (the `./`
roots above, a relative `skill_dirs` entry, or any `skill_dirs` entry a
project settings file adds) loads only once you trust the folder (see
*Project commands, skills and agents* below), and never replaces a bundled or
user skill: it loads as `/project:<name>` instead, with a notice at startup.
Set `"project_skills_override": true` in `~/.buildwithnexus/settings.json` to
let project skills replace them, as before 0.15. Add more roots with the
`skill_dirs` settings key (`["~/my-skills", "tools/skills"]`).
`/skills` lists every skill with its source and description.

## Custom commands

A Markdown file in a commands folder is a slash command named after the file:
`deploy.md` is `/deploy`. Its body, with the YAML frontmatter stripped, is the
prompt; `description` in the frontmatter is what the command popup shows.
`$ARGUMENTS` in the body is replaced by everything typed after the command,
and `$1`…`$9` by single words (quotes group words), so `/deploy staging` fills
in `staging`. A body with no placeholder gets the arguments added after it.
A `.sh`, `.bash` or `.py` file runs as a command instead, through the same
hooks and permission gate as `run_command`.

Commands load from `~/.buildwithnexus/commands/` and `~/.claude/commands/`,
and, once you trust them, from the project's `.buildwithnexus/commands/`
and `.claude/commands/`. A file in one of those folders with the same name
as a bundled default replaces the built-in, which loads only when the name
is still free (42 workflow-kit commands ship in the binary; `/skills` and the
`/` popup list the names). A session command (`/commit`, `/review`, `/pr`,
`/debug` and the rest of `/help`) is matched before any command file, so a
file named like one runs only headless, as in `bwn run '/commit'`. Skills and
commands work headless too: `bwn run '/deploy staging'`.

```markdown
---
description: Deploy to an environment
---
Run the deploy for $ARGUMENTS, then check the health endpoint.
```

## Helper agents

The model can hand a subtask to a helper with the `task` (or
`spawn_subagent`) tool, as the built-in `engineer` or `researcher` role or as
a helper you define. An agent file is `<name>.md` with `name`, `description`
and `tools` frontmatter and the helper's instructions as the body; `tools`
limits what it may use, and Claude Code's tool names work (`Read`, `Grep`,
`Bash`, …). A helper with a `tools` list can never delegate further (`task`
and `spawn_subagent` are left out), and a call naming an unknown role fails
with the list of roles. `/agents` lists the helpers.

Bundled read-only helpers ship as `reviewer`, `debugger`,
`security-check`, and `pipeline-reviewer`. An agent file in
`~/.buildwithnexus/agents/` or
`~/.claude/agents/` with the same name replaces the bundled one.

```markdown
---
name: test-writer
description: Writes focused unit tests for one module
tools: Read, Grep, Glob, Write
---
Write tests for the module you are given. Do not change the module itself.
```

A helper started with `read_only: true`, or from an agent file with
`read_only: true` or a `tools` list that only reads (`Read, Grep, Glob`),
can read and search but never change anything. Read-only helpers and
isolated ones (`isolate: true`) that the model starts in the same reply run
at the same time, up to `max_parallel_helpers` at once (default 3; `1`
runs them one after another). On a local server the default is one at a
time, or as many as the server reports it answers at once (llama.cpp's
slots): a server with one slot queues the others until they time out. Isolated helpers take turns while folders
added with `--add-dir` are in use: a worktree does not cover those, and each
helper may write there. Each shows its work in one labelled block
when it finishes, a helper that needs an approval says which one it is,
and Esc or Ctrl+C stops them all. Helpers that write in your folder always
run one after another. Their tokens and cost count toward the session.

Searches, tree walks and network calls that the model makes in a row in one
reply (`grep_files`, `find_paths`, `find_files`, `list_tree`, `fetch_url`,
`web_search`) run at the same time, up to `max_parallel_tools` at once
(default 4; `1` runs every call one after another). File reads and folder
listings in the same run are quick enough that they run in line. Each call is still
checked, approved and shown in order before any of them runs, the model
gets the results in the order it asked, and at most one call per website
is in flight. Anything that writes, commands, questions, helpers, MCP and
browser tools, and any call a PostToolUse hook matches, run one at a time
as before.

Agent files load from `~/.buildwithnexus/agents/` and `~/.claude/agents/`,
and, once you trust them, from the project's `.buildwithnexus/agents/`
and `.claude/agents/`. A helper that runs isolated in a git worktree shows
its branch and the merge command when it finishes (a `subagent_result` event
in `--json` mode), and commits with your git identity. To run a whole session
on its own branch, start it with `--worktree <name>`.

## Project commands, skills and agents

A repository's command files speak to the model in your name, its skills
and agents steer it, and a script command runs code, so they stay off until
you trust them. The folder trust prompt lists each one with its file
(`command /deploy (.claude/commands/deploy.md)`), also when the repository
has nothing else to trust, and trusting pins their contents: adding, editing
or removing one asks again at the next start. Until then a startup notice
names them and how to trust, and typing one says it is off. A file linked to
somewhere outside its folder never loads. `buildwithnexus trust --print`
lists them for CI. Run from your home folder, these folders are your own and
load as such.

