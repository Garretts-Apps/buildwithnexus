# Changelog

All notable changes to `buildwithnexus` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed
- **Colours show in a cmd or PowerShell console window on Windows.** The
  first-run setup, and anything else printed before the full-screen UI,
  printed its colour codes as text (`←[38;2;187;154;247m  buildwithnexus`)
  in the console window that cmd and Windows PowerShell open. Windows
  Terminal was not affected. bwn now turns on virtual-terminal processing
  for the console before it prints anything. A console that refuses it (the
  "Use legacy console" setting) gets plain text in line mode instead of
  codes. CI opens the setup in a conhost window and fails if a code shows
  up as text.

### Release process
- The npm publish step passes the tarball as `./dist/…`. npm 12 read
  `dist/buildwithnexus-0.15.0.tgz` as GitHub shorthand and refused it
  (`EALLOWGIT`), so 0.15.0's first publish run failed after its GitHub
  release was out.

## [0.15.0] - 2026-10-01

An install IT can approve, and trust you grant explicitly: approvals, rules
and keys that mean what they say, setup that finishes only with a model that
answered, one conversation across modes, and headless runs whose exit code
tells the truth. It also brings helpers that run side by side, extra working
folders, MCP servers behind OAuth, editors over the Agent Client Protocol, a
GitHub Action, Claude Code's hook decisions and events, and pictures, PDFs
and screenshots the model can read. This minor release changes some defaults; every change and
the setting that restores the old behavior is listed in
[docs/UPGRADING-0.15.md](docs/UPGRADING-0.15.md).

### Security
- **The key bwn was started with is not in its own environment block.**
  `/proc/<pid>/environ` (Linux) and `ps eww` (macOS) show `****` for
  `*_API_KEY`, `*_API_TOKEN` and preset key variables; bwn still uses the key.
  `/proc/*/environ` (any process, globs included) is a sensitive path for the
  file tools and for commands, and so are `/proc` and a process's folder as a
  command argument (a recursive search reads every environ under them), so reading the parent shell's environment asks
  first in every mode and is refused headless.
- **Commands get no descriptor bwn inherited.** On Linux and macOS every
  descriptor above 2 that bwn starts with is made close-on-exec, so no
  command, hook, MCP server or tmux session gets it. A CI runner passes its
  own control pipe down to the job, and a command that wrote to it ended the
  job.
- **A call quoted in an answer never runs.** JSON, `<tool_call>` markup or a
  `tool_code` fence with prose after it, or after a paragraph or an
  "example"/"you could" lead-in, is the model describing a call and stays
  text in every mode. The reply's own call still runs when it opens the
  reply or ends one short lead-in line.
- **Isolated helpers run no repository hooks.** A helper's worktree is made
  with hooks and fsmonitor off, and not at all when the repository's git config
  can run programs (filters and the like). A read-only helper gets no worktree:
  making one is a write.
- **network rules match every spelling of an address.** IPv4 in any URL-parser
  form, IPv4-mapped and -compatible IPv6 and trailing dots match the rule they
  spell, and a redirect to a denied host is refused before it is followed.
- **Repo prompts and skills need folder trust.** A project's
  `.buildwithnexus/system.md` takes effect only after you trust the folder, and
  is added after your own `~/.buildwithnexus/system.md` instead of replacing it.
  A project skill can no longer replace a bundled or user skill of the same name
  (such as `security-review`); it loads as `/project:<name>` with a notice. A
  `skill_dirs` entry from project settings counts as the project's own, even as
  an absolute path.
- **The trust prompt shows what will run.** Each hook's command line, each
  project MCP server's command and arguments (the way bwn actually starts it, so
  a stdio server can't hide behind a harmless `url`), and a preview of the
  project system.md, all sanitized for the terminal.
- **Trust covers the files that run.** Scripts a hook or MCP server runs from
  the repo, files named by a bare word, and the `package.json`, `Makefile` or
  `justfile` a runner uses (including `make -C sub`, `cd web && npm test`,
  `npm --prefix api`) are pinned; editing one asks again.
- **Windows paths get the same protection.** Sensitive-path checks understand
  backslashes, drive letters, UNC and `\\?\` prefixes, any case, trailing dots,
  `::$DATA` streams, 8.3 short names, wildcards and PowerShell's `-Path:value`,
  so `rg . C:\Users\me\.ssh\id_rsa` asks first.
- **Windows destructive commands always prompt,** even in auto mode or with a
  saved approval: rd/rmdir /s, del/erase /s or /q, format, Format-Volume,
  Clear-Disk, diskpart, Remove-Item -Recurse -Force and its aliases (including
  `rm -r -fo`), `rm -rf` on a drive path, cipher /w, reg delete, bcdedit,
  takeown/icacls on system paths, Set-ExecutionPolicy and
  `powershell -EncodedCommand`.
- **Approval answers cover what they say.** `s` or `a` for `rm`, `mv`, `cp`,
  `ln`, `chmod`, `chown`, `dd`, `truncate`, `kill`, `pkill` and similar
  programs (and their Windows forms) covers that exact command, not every
  later use of the program. The prompt names what `s` and `a` will allow.
  The same holds for git commands that discard or rewrite (`rm`, `clean`,
  `checkout`, `restore`, `reset --hard`, force or deleting pushes,
  `branch -D`, `stash drop`/`clear`, `filter-branch`/`filter-repo`,
  `reflog expire`, `submodule deinit`, `notes remove`/`prune`,
  `read-tree --reset`/`-u`, `checkout-index --force`, `gc --prune` and
  others), and for wrappers that run the command after them (`fakeroot`,
  `firejail`, `torsocks`, …). A saved `git push` no longer covers
  `git push --force`.
- **A question in PLAN or BRAINSTORM is read-only.** A question or greeting
  answered there is read-only whatever the session permission, including
  rounds a `Stop` hook asks for and turns started from an editor; under
  `auto` such a turn could write, edit or delete files.
- **Custom endpoint keys stay with their endpoint.** A key for an
  OpenAI-compatible endpoint is saved as `CUSTOM_API_KEY@<origin>`, filed
  under the host the request actually reaches (so `http://a\@b/v1` no longer
  carries `b`'s key to `a`). `/model` to a new address asks for that
  address's own key (Enter for none) and never sends another endpoint's. The
  single key 0.14 saved moves once to the custom endpoint in your own
  settings, or else waits unbound, is never sent, and is offered with `y/N`
  at the next new endpoint. `CUSTOM_API_KEY` in the environment stays with
  the endpoint a run starts on, and `buildwithnexus login` honours
  `--base-url`.
- **Deny and ask rules see through wrappers.** They read the command behind
  `env`, `sudo`, `nice`, `command`, `time`, `timeout`, `xargs`, `exec`,
  `eval`, `find -exec`, `fakeroot`, `firejail`, `bwrap`, `torsocks`,
  `proxychains`, `numactl`, `chronic`, `run0`, `sg`, `ssh-agent`,
  `systemd-inhibit`, `uv`/`poetry`/`pipenv run`, `bundle exec`,
  `direnv`/`mise exec`, `nix-shell --run`, `FOO=1` and `/usr/bin/git`; inside
  `sh -c`, `bash -lc`, `cmd /c`, `pwsh -Command`, `$(…)` and backquotes; past
  git's own options (including `--attr-source` and `--shallow-file`), inline
  `-c alias.*` aliases and `git-<command>` programs. A deny rule also refuses
  a pipeline or compound command, code handed to an interpreter
  (`perl -e`, `ruby -e`) or a bash `$'…'` word that names its program. On
  Windows the rules read commands as cmd.exe passes them on.
- **Every file a call touches is checked.** `apply_patch` is checked for each
  file its diff names (rules, the sensitive-path prompt, accept-edits and
  hook guards); `move_path` on the trimmed paths it actually moves, so a
  leading or trailing space no longer escapes the working folder or the
  sensitive-path prompt; and the legacy `mcp_call` tool as the
  `mcp__<server>__<tool>` it calls.
- **Network rules see the real host.** `network` rules and approvals, for the
  web tools and `http` hooks, see the host a URL reaches: percent-escapes
  decoded, a trailing dot dropped and an explicit default port left off.
- **Repo commands, skills and agents need folder trust.** The repository's
  `.buildwithnexus/{commands,skills,agents}`, `.claude/{commands,skills,agents}`
  and `.agents/skills` load only once trusted. The trust prompt and
  `buildwithnexus trust --print` list each file, and trust is pinned to
  their content. A repository with only a command file now gets the prompt,
  and files linked from outside their folder never load.
- **Provider keys stay out of the agent's commands.** `*_API_KEY`,
  `*_API_TOKEN`, `HF_TOKEN` and the other preset key variables are removed
  from every command the agent runs, sandboxed or not, including dev servers
  started in a tmux server that was already running. `shell_env_passthrough`
  keeps the variables you name; hooks keep their environment.
- **Keys are typed hidden** (dots, then only the masked key) in setup,
  `/model` and `/login`, and a key is saved only after the provider accepts
  it. A key typed during a `/model` swap used to be saved before the check, so
  a later chat line could become `CUSTOM_API_KEY`.
- **Nothing is installed for you.** First launch no longer offers to run
  `brew` or `sudo apt-get`; `buildwithnexus doctor` prints the install command
  for each missing tool and runs none.
- **Trust is checked each time a project hook runs.** A hook script that
  changed since you trusted the folder is asked about again (`scripts/fmt.sh
  changed since you trusted it — run it?`), or skipped with a warning in a
  headless run. The trust prompt is one screen and one answer: `y`
  everything, `e` all except a repository's `base_url` and `permission`, `n`
  nothing (the default). A repository's own AGENTS.md/CLAUDE.md is on that
  screen (`r` reads it) and covered by the same answer; a folder with no
  settings to trust asks about it alone (`use them? y yes · n no · r review
  [N]`), once per folder and content. After that a dim line names it. Headless runs, and sessions with prompts piped in, print one
  line until it is acknowledged.
- **Web search asks first.** `web_search` counts as network access to
  `lite.duckduckgo.com` and asks outside `auto`, read-only mode included.
- **Every command gets the same checks.** The commands given to `check_work`
  and `start_server` go through the sensitive-path, dangerous-command and
  deny-rule checks of `run_command`, even in `auto`.
- `/commit`, `/diff` and the `@diff` and `@status` attachments ask before
  running git in a repository whose git config can name programs for git to
  run (hooks, filters, an external diff); headless runs attach nothing there
  and print a notice. `http.*` keys (such as the auth header
  actions/checkout stores) count as inert, so a CI checkout no longer asks;
  `http.cookieFile` and `http.saveCookies` do not, since git writes that file
  on its next fetch.
- **Delegation keeps its limits.** A `task` or `spawn_subagent` call with an
  unknown role fails with the list of roles instead of running as `engineer`
  with every tool, and a helper defined by an agent file never gets `task` or
  `spawn_subagent`.
- `/review` and `buildwithnexus review` are read-only even in `auto`, and
  name untracked key and credential files without sending them.
- A `.gitignore` pattern built to backtrack can no longer stall the file
  tools or `@` completion: matching takes at most pattern × name steps.

- **Instructions from the repository need a `y`.** The question is
  `use them? y yes · n no · r review [N]`. Only `y` uses a repository's
  AGENTS.md/CLAUDE.md; Enter, `n` or Esc keep them out of that session's
  prompt, and the question returns next launch. A task or `/command` typed
  while it is up is kept for the input box, and `/init`'s y/N question also
  gives typed text back.
- **`--trust-project` no longer carries `base_url` or `permission` along.** A
  digest whose project settings set either exits 2 unless
  `--trust-project-allow base_url,permission` (or `BWN_TRUST_PROJECT_ALLOW`)
  names it; a `permission` of `readonly` only tightens and needs no name.
  `trust --print` prints the exact line, the digest covers skills from a
  `skill_dirs` the settings add, and a malformed, prefix-less or mistyped
  digest gets its own message.
- **Trusted repository commands run headless, and escapes do not load.** A
  repository command or skill file that links outside its folder is listed
  as not loaded, and `/name` for it says so instead of going to the model.
- **A key is asked for only where it can safely go.** When an endpoint is plain
  http on another machine, setup and `/model` explain the https or
  ssh-tunnel options instead of asking for a key.

### Added
- **Corporate proxies and TLS inspection.** bwn's own HTTP (providers, web
  tools, MCP over HTTP, update check, local server probes) honors
  `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY` (hosts, subdomains,
  IPs, CIDR blocks, `*`); local model servers on loopback are always reached
  directly. HTTPS trusts the operating system's certificate store as well as
  the bundled roots, and `SSL_CERT_FILE`/`SSL_CERT_DIR`. An untrusted
  certificate fails at once with a message naming `SSL_CERT_FILE`.
- **Distinct exit codes for headless runs.** `bwn run`, `plan` and
  `brainstorm` exit 4 when a hook blocked the task, 5 when `--max-budget-usd`
  stopped it, 6 when it ran out of steps, 7 when `check_work` still failed and
  8 when the verifier still blocked. They used to exit 0 and print "done". The
  last `--json` event is now `{"type":"result","outcome":…,"exit_code":…}`.
- **A versioning contract.** [docs/VERSIONING.md](docs/VERSIONING.md) says
  what a minor and a patch release may change before 1.0 for CLI flags,
  settings keys, `--json` events, session files and exit codes. Every `--json`
  event and session file carries `"schema_version": 1`; older session files
  still load.
- **accept-edits permission mode.** File edits inside the project run without
  a prompt; commands, deletions, network access and changes inside `.git`
  still ask. Use `--permission-mode accept-edits`, `/permissions accept-edits`
  (or `/permissions 4`), or `"permission": "accept-edits"`.
- **Allow, ask and deny rules.** `permissions` in settings.json takes rules
  such as `run_command(git push*)`, `write_file(migrations/**)` or
  `WebFetch(domain:example.com)`, and `network` takes `allow` and `deny` host
  lists. Deny beats ask, ask beats allow, and all of them beat the mode; a
  refusal names the rule and whether it came from user or project settings.
  A rule for `run_command`, `bash` or `Bash` also covers `check_work` and
  `start_server`. Project settings add ask and deny rules on their own, allow
  rules only once the folder is trusted.
- **/permissions shows what is allowed.** It lists saved approvals and rules;
  `/permissions remove <entry>` forgets one, and `/permissions default <mode>`
  (or "save as default" in the picker) saves a mode.
- **A desktop notification when bwn waits for you** at an approval or a
  question (the `notify` setting; by default only while the terminal is
  unfocused).
- **Hooks.** Matchers take Claude Code tool names (`Bash`, `Edit`, `Write`,
  `Read`, `Grep`, `Glob`, `WebFetch`, `WebSearch`, `mcp__.*`).
  `"on_error": "deny"` makes a crashing `PreToolUse` guard block the call.
  Unknown events, handler types and `on_error` values are reported at startup
  and by `doctor`, and a failed hook shows the end of its stderr.
- **Hooks that answer as Claude Code's do.** `PostToolUse` can talk to the
  model: `{"decision":"block","reason":…}`,
  `hookSpecificOutput.additionalContext`, or stderr with exit 2 is added to
  the tool result. `Stop` and `SubagentStop` can keep the agent going: exit 2
  or a block decision sends the reason as the next message, at most 3
  rounds, with `stop_hook_active` set; a turn you stopped, or one a
  `UserPromptSubmit` hook blocked, is never continued. `tool_input` carries
  Claude Code's field names beside bwn's (`file_path`, absolute,
  `old_string`, `new_string`, `content`, `prompt`, `subagent_type`, `glob`,
  `path`, `todos`), and a move, `read_many_files` or `apply_patch` call is
  shown to `PreToolUse` once per file.
- **New hook events and an `http` hook type.** `PreCompact` (trigger `auto`
  or `manual`), `Notification` (`permission_prompt`, `question`, `done`, and
  `idle_prompt` after `idle_notify_secs`, default 60, 0 for never) and
  `PermissionRequest` (answer allow, deny or ask for the user; only your own
  hooks can allow; runs in headless runs too). `"type": "http"` POSTs the
  payload as JSON to a URL through bwn's HTTP client, within the hook
  timeout and without following redirects; `network.deny` applies, and the
  trust prompt shows a project's hook URLs.
- **Helpers side by side.** Helpers that only read and isolated helpers
  started in the same reply run at the same time, up to
  `max_parallel_helpers` (default 3), each shown in its own labelled block
  when it finishes; Esc or Ctrl+C stops them all. Helpers that write in your
  folder still run one after another, and so do isolated helpers while
  folders added with `--add-dir` are in use. Agent files and the task tools
  take `read_only: true` for a helper that may read and search but never
  change anything. With `--json`, each event of such a helper carries
  `helper`.
- **More working folders.** `--add-dir <path>` (repeatable) and `/add-dir`
  add folders to work in for the session. File tools may write there,
  searches and `@` completion cover them, the sandbox binds them writable,
  helpers and workflows inherit them, and the footer shows `+N dirs`.
  Sensitive paths still ask, links out of them stay outside, their settings,
  hooks, commands and agents never load, and their AGENTS.md is named in a
  notice before the model reads it.
- **MCP servers that ask for OAuth.** `bwn mcp login <name>` (or
  `/mcp login <name>`) reads the server's OAuth metadata, registers bwn as a
  client or uses `oauth.client_id`, signs in through the browser with PKCE
  and saves the token owner-only in `~/.buildwithnexus/mcp-auth/<name>.json`,
  bound to that server's URL. Tokens refresh on their own; `bwn mcp logout
  <name>` revokes and forgets the sign-in. `/mcp`, `bwn mcp list` and
  `doctor` show whether an http server is signed in; one that wants a login
  is listed as `needs login`, and headless runs skip it with that message
  instead of opening a browser. A token a server echoes back is shown as
  `[redacted]`. `mcp add --url` takes `--client-id` and `--callback-port`
  for authorization servers without dynamic registration.
- **Editors over the Agent Client Protocol.** `buildwithnexus acp` runs an
  ACP (v1) server on stdio, so Zed, JetBrains IDEs and Neovim plugins can
  drive bwn: streamed replies and reasoning, tool calls with diffs, plans,
  approvals as editor questions (allow once, always in this project,
  reject), project trust questions, Build/Plan/Brainstorm modes, cancel,
  session load, editor file access and editor MCP servers. Helpers from one
  reply take turns there. Commands, hooks and MCP servers never get the
  protocol's output pipe.
- **A GitHub Action.** The repository is an action (`action.yml`): it
  installs bwn from npm, runs `run` or `review`, maps the exit code to the
  step result with annotations (review findings on their lines), uploads the
  `--json` event log, and can post or update a pull request comment (only
  one its own token's account wrote).
  [examples/github/bwn-review.yml](examples/github/bwn-review.yml) reviews
  every pull request.
- **Pictures, PDFs and screenshots.** `read_file` on a PNG, JPEG, GIF or
  WebP shows the picture to a model that takes images (Anthropic inside the
  tool result, OpenAI-compatible servers and Ollama in a user turn after the
  tool messages); a text-only model is told why it sees none. `read_file`
  reads a PDF as text through `pdftotext` (poppler) when it is installed, and
  names what to install when it is not. The new `screenshot_url` tool has a
  local headless Chrome, Chromium or Edge (`BWN_CHROME`, PATH, the usual
  install folders, or a Playwright build) screenshot a page served on this
  machine. Other hosts are refused unless `network.allow` names them, and the
  host is approved like a fetch. Every request the page or the browser makes
  to another host, link-local and cloud-metadata addresses included, is
  blocked, and the page's are listed in the result.
- **Trust for CI.** `buildwithnexus trust --print` prints a digest of this
  folder's project settings; `--trust-project <digest>` or
  `BWN_TRUST_PROJECT=<digest>` trusts exactly that content for one run.
- **/login and `buildwithnexus login`** replace a rejected key in place,
  checked before it is saved. The `/model` picker marks a key whose last check
  failed `key rejected`.
- **/model remembers each provider's address** (`endpoints` in
  `~/.buildwithnexus/settings.json`, from your own settings only), switches
  models on a configured endpoint without asking for the URL or key again,
  recognises an Ollama host address, and ends long lists with `+N more — type
  a name`.
- **/init writes AGENTS.md from the repository** (or improves the one there)
  from its build and test files, shown as a diff to approve;
  `buildwithnexus init --agents-md` does the same headless.
- **Piped input.** `run`, `plan` and `brainstorm` read stdin: it is the task
  when none is given, or a `[stdin]` block after the task (up to 1 MiB).
- **A fuller result event.** The final `--json` `result` event adds
  `session_id`, `turns`, `tokens_in`, `tokens_out`, `cost_usd`, `denied` and
  `denials` (and `unpriced_requests` when a model had no price). New outcomes:
  `interrupted` (exit 130 or 143) and `review_blocking` (exit 9).
  `--json sessions` and `--json doctor` print JSON lines.
- **New flags.** `--base-url <url>` points a run at a gateway;
  `--worktree <name>` runs the session in `.bwn/worktrees/<name>` on branch
  `bwn/<name>`; `--plain` selects line mode.
- **`buildwithnexus update [--check]`** installs the latest release; `--check`
  exits 10 when one is available. `BWN_UPDATE_REGISTRY` or
  `npm_config_registry` chooses the registry.
- **Review targets.** `/review [--base <ref> | --staged] [focus]`, and
  `buildwithnexus review` for CI, which emits `finding` events and exits 9 on
  a blocking finding. Reviews include new files git does not track yet.
- **Custom commands and skills run headless** (`bwn run '/deploy staging'`),
  take `$ARGUMENTS` and `$1`…`$9`, and load from `~/.claude/commands` and, in
  a trusted folder, `.buildwithnexus/commands` and `.claude/commands`. The
  slash-command popup lists them too. A repository command that is not
  trusted exits 2 headless and says how to trust it.
- **Custom helper agents.** Agent files (`name`, `description` and `tools`
  frontmatter) in `~/.buildwithnexus/agents`, `~/.claude/agents` and, in a
  trusted folder, `.buildwithnexus/agents` and `.claude/agents` become roles
  the model can delegate to, limited to their tools. `/agents` lists them.
- **Sessions belong to a folder.** `bwn continue` continues this folder's
  latest session; `bwn continue`, `bwn -c` and `bwn resume <id>` without a task
  open the terminal UI on it; `/resume` lists this folder first, with age and
  a filter. New: `/rename`, `bwn sessions rm <id>` and
  `bwn sessions export <id> [file]`.
- **Take answers out, and ask aside.** `/export [file]` writes the
  conversation as Markdown, `/copy` puts the last answer on the clipboard
  (OSC 52), and `/ask <question>` answers a side question that is not added to
  the conversation.
- **/rewind** goes back to an earlier prompt and restores the code, the
  conversation, or both.
- **/diff** lists every changed and new file with one summary line and shows
  a chosen file's diff; `/diff turn` shows what the last turn changed.
- **Revise Plan** in the plan selector: say what to change and get a revised
  plan. Edit Step shows the step and the edited plan before Execute.
- **/context breakdown:** system prompt, tools, MCP tools, conversation and
  images, with the total.
- **`prices` setting:** USD per million tokens for any model, so the spend cap
  can count it. **`vision` setting:** whether the model takes images.
- **/local** lists the configured server (a LAN Ollama, LM Studio on another
  port) and every local preset with its models, then the GGUF files on disk.
- **Colour themes** `dark`, `light` (at least 4.5:1 contrast) and `ansi` (the
  terminal's 16 colours). The `theme` setting defaults to `auto`, which
  follows the terminal's background; `/theme` switches and saves it.
- **Line mode** with `--plain` or `TERM=dumb`: no alternate screen, cursor
  addressing or spinner frames; Esc and Ctrl+C still work.
- The agent's todo list shows as a checklist that ticks off items.
- `@` completion finds files by name anywhere in the project.
- A paste over 1,000 characters or 10 lines shows as `[pasted N chars]` and is
  sent in full, line breaks kept.
- `BWN_MAX_RETRIES` (0 to 20) sets how often a busy server is retried;
  `BWN_PROXY_PRIVATE=1` sends private addresses through the proxy.

### Changed
- **The binary lives outside the npm package,** in
  `~/.buildwithnexus/bin/<version>/` (or `$NEXUS_HOME/bin/<version>/`), so
  `npm update` no longer deletes it and the same version is not downloaded
  again. Binaries earlier releases put in the package still run.
- **Auto-update stays within your minor version.** `"auto_update": "install"`
  installs patch releases only; a new minor or major is announced with its
  install command. `"install-any"` installs any newer release.
- **The Windows exe no longer needs the Visual C++ runtime.** It links the C
  runtime statically, and CI fails if it imports a VC++ runtime DLL.
- **Isolated subagents keep their work.** Uncommitted edits were force-deleted
  when a subagent finished; they are now committed to its `bwn-sub-*` branch,
  and the result names the branch and how to review or merge it.
- The OpenRouter preset's default model is a current one; the retired default
  is gone. All built-in model ids live in one preset table.
- **A permission switch lasts for the session.** "use auto" typed in the
  conversation and `/permissions <mode>` no longer write settings.json;
  `/permissions default <mode>` does.
- **Permission names are checked.** An unknown `--permission-mode` exits 2
  (`unknown permission mode yolo — use ask, accept-edits, auto, readonly or
  plan`); a misspelt `permission` setting warns and uses `ask`. `acceptEdits`
  and `accept-edits` now mean the new mode (they meant `auto`). Claude Code's
  `default`, `plan`, `dontAsk` and `bypassPermissions` read as ask, readonly,
  readonly and auto.
- **Esc or Ctrl+C at an approval or a question stops the turn**
  (`stopped — tell me what to do instead`). `d <reason>` refuses and lets the
  model carry on. A question's suggested default is never sent as your answer.
- **Background workflows** (`/schedule`, `/loop`) run under the session's
  permission, provider, model and endpoint instead of settings.json.
  Scheduling outside `auto` warns that approvals cannot be answered, failures
  say why in one sentence, and a `/loop` run that has a change refused stops
  the loop (`✗ workflow #N blocked: …`). A run's log in `/workflows` shows
  readable lines.
- A `*` or empty hook matcher no longer runs `PreToolUse` on `finish` and
  `exit_plan`; name them to guard them.
- **`provider`, `model` and `permission` are optional in settings,** so a team
  repository's hooks-only settings file no longer blocks first run; an empty
  model means the preset default. A wrong-typed value is reported as
  `<file>: "<key>": …`.
- **Setup finishes only after the chosen model answers.** It names a dead
  address, a missing or empty Ollama and a gateway that wants a key, refuses
  an empty key, and lists Ollama once. Leaving it saves nothing, prints
  `setup not finished` and exits 1.
- **Banners name the preset and host,** such as `LM Studio (localhost:1234)`,
  instead of the wire protocol; the footer follows `/model`. `--provider X`
  runs at X's last-used or default address, and an unknown `--provider` exits
  2 with the closest name.
- **Provider errors say what to do.** The first line says what happened and
  the fix, the second is `HTTP <code>: <server message>`. A rejected key
  points at `/login`, an Ollama out-of-memory error names the sizes and is not
  retried, a context overflow gives the tokens needed and held, and a stopped
  server reads `nothing is answering at <host>`.
- **Retries:** a 429 or 5xx is retried 3 times (`BWN_MAX_RETRIES`), a model
  that is still loading for about two minutes, a refused connection once.
- **Proxies:** model servers on private, link-local, CGNAT or IPv6
  unique-local addresses (and names that resolve only to them) connect
  directly, as in 0.14.10. Proxy failures name the proxy and its variable
  (down, 407, rejected credentials, refused tunnel); an `https://` proxy URL
  fails at once.
- **Real context windows.** llama.cpp, LM Studio and vLLM report their window,
  which replaces the 8,192 guess. A message bigger than a known window is
  refused before it is sent, with a range to attach instead, and is left out
  of the conversation.
- **Image support follows the server's report** (Ollama capabilities, LM
  Studio model type, llama.cpp modalities) before the model-name guess.
- **The mode changes only when you change it.** Task-like input in BRAINSTORM
  gets a hint instead of a switch; Cancel or Esc at the plan selector stays in
  PLAN.
- **One conversation in every mode.** BRAINSTORM, PLAN and conversational
  BUILD turns share the transcript, are saved as the session and count in
  `/context`. `bwn brainstorm` saves a session and never prompts, and
  `--json brainstorm` prints only JSON.
- **/commit asks.** It shows the drafted message with
  `[c]ommit · [e]dit · [n]o` and commits only after `c`; with nothing staged
  it says so before any model request.
- **/undo asks before overwriting your edits.** `/undo`, `/undo all`,
  `/undo <id>` and `/rewind` ask `<file> changed after the agent edited it —
  overwrite your changes? [y/N]`, including hand edits between agent turns.
  `/undo` names what it cannot undo (shell-command changes, commits, files too
  large to snapshot), offers the last task in this folder after a relaunch,
  and `/checkpoints` keeps the newest 500 per folder.
- `/rewind` is no longer an alias of `/undo`; `/rewind <id>` still works like
  `/undo <id>`.
- **Session ids** are `<16-digit milliseconds>-<8 hex digits>`; 0.14 ids still
  load and resume.
- **Keys and pickers.** Ctrl+C on an empty prompt needs a second press within
  2 s to quit (Ctrl+D quits at once; quitting with workflows waiting asks).
  Esc, Ctrl+C, or Ctrl+D on an empty line cancels a question, setup and
  `/init` included. Pickers filter as you type (`j`, `k` and `q` are letters),
  a digit typed first moves the highlight to that numbered row, and only
  Enter picks, only a row that is shown, so text like `1. add tests` typed
  into Approve Plan no longer runs the plan. The
  input box always shows the open prompt or picker.
- **One line per tool call.** `• tool_call`, `• tool_result` and the other
  trace lines, and the `recovery: parsed …` notice, appear only in `/trace`
  and the trace file.
- **File tools skip ignored files.** `grep_files`, `find_files`, `find_paths`,
  `list_tree` and `@` completion skip what `.gitignore` ignores and Python
  virtualenvs; `.env` and other sensitive files stay hidden even when a
  `.gitignore` line un-ignores them.
- **Headless exit codes tell the truth.** A run in which a hook, a rule or
  read-only mode refused a call exits 3 with `changes were denied: …`; a turn
  that ends right after a call that could not run exits 1; SIGINT and SIGTERM
  exit 130 and 143 with a final `interrupted` result. `--legacy-exit-codes`
  restores 0 for the first two.
- **Command-line mistakes are usage errors.** Unknown options exit 2 with a
  "did you mean" hint and send nothing; a bad `--effort` exits 2.
- **Built-in rules match whole words of project-relative paths**
  (`AUTHORS.md` is not auth code; every form of authenticate, authorize and
  authorise still is). A violation says how to clear it: `BWN_CHECKS_DONE`, or
  `"enabled": false` in `NEXUS_HOME/rules`.
- **doctor checks what you use:** only the configured provider and its key,
  never a hosted API for a local setup. It flags a configured Ollama model
  that is not installed, lists hooks and their problems, and exits 1 when a
  check fails. `/doctor` runs the same checks.
- **Isolated helpers** show their branch and merge command, say so before
  writing when they cannot be isolated, and commit with your git identity.
- **MCP:** a headless run waits at most 5 s for servers (or a server's
  `timeout_secs`); `mcp add` refuses to replace an existing name without
  `--force`.
- The update notice names one step, `buildwithnexus update`.
- A custom command file's body (frontmatter stripped, arguments filled in) is
  the prompt, and arguments are sent once.
- `/agents` lists helper agents, `/teamwork` describes delegation as it
  works, and `/btw` explains itself.
- Upgrade notices (ignored approvals, restored workflows) are shown once
  (`NEXUS_HOME/notices.json`); the workflow queue line prints only when the
  count changes.
- BUILD turns in a git repository run `git status` at the start and end of the
  turn to report files changed by shell commands.
- Bare `/plan`, `/build` and `/brainstorm` switch the mode; bare `/loop`,
  `/schedule` and `/btw` print their usage. `/help` groups every command and
  lists the keys and the answers to an approval prompt.
- **One command table.** `/help`, the `/` popup and Tab completion list every
  command and alias (now including `/rename`, `/export`, `/copy`, `/ask`,
  `/add-dir`, `/mcp login`/`logout` and `/rewind` on its own row);
  `/permissions` completes `accept-edits`, `default`, `list`, `remove` and
  `reset`. `--help` lists every command, subcommand and option (`acp`,
  `mcp login|logout`, `trust --print`, `sessions rm`/`export`,
  `--trust-project`, `--plain`, `--add-dir`, `accept-edits`) and the
  session's commands by section. Both link
  https://buildwithnexus.dev/docs/data.
- **A diff is shown once.** A write or edit shows its diff under the applied
  `⏺` line when nothing asks first, or above the approval question (then
  the applied line shows only `+N -M`). A refused or failed change still
  shows its preview.
- **Edit Step and /rewind fill the input box.** Edit Step opens the chosen
  step's text there to edit; `/rewind` puts the chosen prompt back after a
  conversation rewind, and Esc Esc on an empty input box opens `/rewind`.
- **Esc or Ctrl+C while a helper runs ends the whole turn** instead of only
  the helper.
- **Hook matchers and permission rules:** `Edit` also covers `multi_edit`,
  `remove_path` and `move_path`, and `Write` covers `create_dir`. A matcher
  naming the shell (`Bash`, `run_command`, `bash`) also covers `check_work`
  and `start_server` calls that carry a command.
- **`mcp add <name> <command> [args...]`** keeps every word after the server
  name for the server, so `npx -y pkg --json --model m` is saved as written.
  Claude Code's `mcp add <name> -- <command>` form is accepted.
- **`/mcp` and `bwn mcp list`** show whether each http server is signed in
  (the status column is one character wider), and `mcp <name>` adds an
  `auth:` line.
- **`bwn acp`** starts the editor server instead of a session with the
  prompt "acp"; more words after it exit 2.
- **A check nobody could approve is not a blocked change.** With
  `accept-edits` or `ask` and no terminal, a `check_work` call nobody could
  approve no longer fails the run with "changes blocked for lack of
  approval": the run succeeds and says the checks were not run. The closing
  line for real blocks names the blocked calls (`N changes were blocked for
  lack of approval and not made: …`).
- Session files may hold tool-result pictures (base64, like attached
  images); older versions load them and ignore the field.

- **One key question for setup, `/login` and `/model`.** It refuses, without
  sending anything, an answer that cannot be a key: one with spaces (pasted
  line breaks become spaces, so several pasted lines are refused rather than
  joined), a leading `/`, only digits, a provider name, or more than 4096
  characters. A `/command` closes it: in a session the command runs, in setup
  it stops setup. `/model` asks again after a rejected key.
- **Setup on a keyed OpenAI-compatible gateway** lists the gateway's models
  once the key works (header `detected models:`), offers `4 accept edits` and
  refuses permission answers outside 1-4. Setup left early, including `init`
  without a terminal, writes nothing. The first session after 0.15 says once
  which endpoint a 0.14 `CUSTOM_API_KEY` is now kept for.
- **The trust prompt is one screen with one question** (`y` / `e` all except
  `base_url` and `permission` / `n`). It includes `skill_dirs` skills,
  explains `base_url` and `permission`, and names the file that changed since
  the last trust. `trusted.json` gains `<file>#files` entries that older
  versions ignore; `bwn acp` keeps its per-decision questions.
- **`doctor` names each missing tool once,** on its check line with the
  install command, in place of a separate advice block. The blocked-run line
  suggests `--permission-mode auto` only when auto would allow the calls.
- **Text tool calls need to be the reply's own.** A Python-style call outside
  a `tool_code` fence runs only when the reply is nothing but that call; JSON,
  tagged and fenced calls run only when they open the reply or end a one-line
  lead-in (see Security). A call to a tool
  that was not offered is refused before the gate with a short list of the
  offered tools. `finish` ends a BRAINSTORM turn and the first ACP prompt turn.
- **Read-only helpers are reads for the gate.** A helper started with
  `read_only: true` is a read; a call a helper is refused counts toward exit 3
  and is reported to the parent.
- **Local models.** A 32,768-token window (what bwn asks Ollama for) gets the
  full tool set; only smaller windows get the compact one. One helper at a
  time unless `max_parallel_helpers` is set;
  `/context` and `/teamwork` say when the window was not reported;
  `screenshot_url` is refused up front for a text-only model; a picture over
  5 MB in the prompt is not sent.

### Fixed
- `/model` on the same provider keeps its saved `base_url`, so a remote Ollama
  host or a LAN llama.cpp/LM Studio server stays in use; llama.cpp and LM
  Studio swaps probe the saved URL; a server that only answers as
  `local-model` is reported as such.
- Windows: `llama-server.exe` is found on PATH, so the `/model` picker and
  llama.cpp auto-start work.
- Compacting a conversation keeps earlier images while they fit the context
  window (at most 4, most recent first) and notes how many were dropped.
- Ollama models that don't support thinking are retried without it instead of
  failing with HTTP 400.
- `auto_update` recognises the downloaded binary when the home directory goes
  through a symlink.
- A proxy URL's error message no longer shows part of a password.
- Prompts reach the model exactly as typed: quotes, line breaks, tabs and
  spacing are kept around `@attachments`, in the TUI and headless. Pasted
  line breaks and tabs are kept in the message, and the one-row input box
  shows a break as `↵`; answers to questions and picker filters still take
  one line.
- Parallel runs sharing one home no longer overwrite each other's session
  file.
- Tool calls that small local models write as text work in more shapes
  (Llama 3's `parameters`, an arguments object inside the OpenAI function
  wrapper, a call followed by a sentence), and a call to an unknown tool is
  answered with the real tool list. JSON quoted in an answer, or a tool
  definition, stays text.
- The spend cap holds for models without a known price: with
  `--max-budget-usd` or `max_budget_usd` set, the run stops before the first
  request (exit 2) and asks for a `prices` entry. A model server on another
  machine counts as remote.
- Ollama: a model that is not installed is named with the installed ones and
  the `ollama pull` command, at startup and on the first message; an Ollama
  started after bwn is picked up on the next request with its real context
  window.
- `/init` switches the running session to the provider, model and address it
  saved.
- `/model http://host:11434 <model>` selects Ollama's native API; a swap to
  llama.cpp or LM Studio no longer lands on Ollama's port; `/local` no longer
  shells out to curl, and picking a `.gguf` without `llama-server` explains
  what to install.
- `BWN_TLS_ROOTS=bundled` together with `SSL_CERT_FILE` says which one wins.
- A message that starts with an absolute path, such as a dropped screenshot,
  is sent with the image attached instead of refused as an unknown command.
- On narrow terminals, transcript lines wrap between words and keep their
  indent; the footer, banner and list rows end with `…` when cut.
- In vim NORMAL mode, `/` on an empty line starts a command, so `/vim` can
  turn vim mode off.
- With the dark theme the background is painted on every transcript row.
- A headless run writes its session file before the first request.
- `/undo` right after `/commit` says that commits are not undone.
- `bwn resume` with no id and no terminal exits 2 instead of opening the
  line-mode UI and exiting 0.
- `buildwithnexus review` and `/review` keep findings a model gives in plain
  text without calling a tool. The diff in the task triggered the
  act-don't-explain nudge, the second reply replaced the findings, and the
  review reported none and exited 0.
- A final answer that opens with an example function call and then explains
  it is an answer, not a failed call to a missing tool.
- The transcript shows what a picture read or a screenshot showed, instead
  of `↳ 1 line`.

- A project `permission` of `readonly` no longer needs
  `--trust-project-allow` with a `--trust-project` digest.
- A `--trust-project` digest made before 0.15 stops matching in a folder whose
  settings add `skill_dirs`, because the digest now covers those skills
  (rerun `trust --print`).

### Release process
- Releases are drafts until every binary, checksum and the SBOM are uploaded
  and attested and crates.io is published. v0.14.9 was public for about three
  minutes with no binaries.
- Before npm publish, the exact tarball is installed in clean containers
  (Ubuntu 22.04, Debian bookworm, Debian bullseye below the glibc floor,
  Alpine); any failure stops the publish, and the tested file is the one
  published.
- CI fails when the version differs between package.json, both Cargo.toml
  files and Cargo.lock, or when an untagged version has no CHANGELOG section.
- publish.yml no longer has a version-bump input, which could tag a version
  release.yml never built.

### Changed (after 0.14.10)
- **npm publishing is OIDC only.** The steps that tried to register the
  `buildwithnexus-<os>-<cpu>` names and deprecate old versions are removed:
  both need a long-lived npm token, and npm's OIDC publishing can do neither.
  0.14.10's notes overstated this; the names are not registered, so do not
  install 0.12.1-0.14.2 (see SECURITY.md).

## [0.14.10] - 2026-09-30

Fixes for the security problems reproduced on 0.14.9, a first run that says
why the binary can't start, the npm names old versions pointed at now
reserved, and release checks that test what users actually install.

### Security
- **Broken hooks block instead of allowing.** A PreToolUse hook that times
  out, cannot start (missing script or interpreter, permission denied, a Rust
  hook that does not compile) or is killed now blocks the tool call, with a
  message naming the hook, what happened and how to fix or remove it. Exit
  codes keep their Claude Code meaning. Other hook events show a failure but
  never block.
- **Old approvals for interpreters no longer run code.** 0.14.3-0.14.8 saved
  "always allow" for a program that runs the code it is given by its bare
  name, so a saved `python3` approved `python3 -c ...`. Such approvals are now
  ignored (listed at startup and in `/permissions`; the settings file is left
  as is), and new ones cover one exact command. The list now also includes
  awk and its variants, sed, nodejs, tsx, ts-node, bunx, uvx, tclsh, Rscript,
  julia, versioned names such as `python3.12` and `node18`, and wrappers such
  as stdbuf, setsid, flock, su and strace.
- **Read-only mode closes more ways to write or run programs.**
  - Abbreviated long options, down to one letter (`sort --compress-prog=./x`,
    `sort --o out`).
  - Quoted or escaped flags (`rg "--pre" ./x`), and globs or braces that the
    shell can expand into a flag (`sort *` beside a file named `-o`).
  - A program run by path (`./cat`, `/tmp/x/ls`, `.\cat.exe`) is never
    treated as the allowlisted binary, and an approval for `cat` does not
    cover `./cat`.
  - `rg --hostname-bin`, and find actions written after `--`.
  - On Windows, commands using cmd.exe's `^` escape or `%VAR%` expansion, and
    sort.exe's `/O` output switch.
  - One test table holds every bypass found so far; each future fix adds its
    case there.
- **Text from repos, models and servers can't drive your terminal in more
  places.** Terminal escape codes (such as an OSC 52 clipboard write) are shown
  as visible markers in `/rules`, `/skills`, `/tools`, `/checkpoints`,
  `/undo`, `/workflows`, `/diff`, the plan approval list, question prompts
  and their default answers, menu selections, `buildwithnexus mcp`,
  `buildwithnexus sessions`, `doctor`, settings warnings, hook messages and
  the project folder name.
- **The knowledge base is never written or read through a symlink.** A linked
  `entities.json`, knowledge folder or `.buildwithnexus` folder let
  `kb_record`, `/kb` and `/grill-me` overwrite a file outside the project;
  `kb_record` now returns an error instead. `publish_artifact` also refuses to
  write through a symlink.
- **The npm names 0.12.1-0.14.2 pointed at are reserved.** Those versions list
  `buildwithnexus-<os>-<cpu>` packages that were never published, so anyone
  could have registered the names and had their code installed. The publish
  workflow now owns all five as empty placeholders and fails if any belongs to
  someone else. It also deprecates 0.10.0-0.14.8 (security fixes in 0.14.3
  and 0.14.9) and everything before 0.10.0 (the earlier VM/Docker products,
  with their `destroy` command).
- **CI scans for committed secrets** (gitleaks, redacted) in the tree and in
  every commit a pull request or push adds.

### Fixed
- **The first run says "ready" only when the binary runs.** After the
  checksum check, the npm launcher now runs the new binary with `--version`.
  On a glibc older than 2.34 (Debian 11, Ubuntu 20.04, RHEL 8), on musl
  (Alpine), or when the file cannot be executed, it prints what to do and
  exits 1. It used to print "buildwithnexus is ready" and then a raw
  `GLIBC_2.34 not found` or `spawnSync ... ENOENT`.
- **A binary blocked by endpoint protection gets guidance, not a bare
  `EPERM`.** The launcher names the security products it finds on Windows
  (CrowdStrike Falcon, Microsoft Defender, SentinelOne, Cylance, Carbon
  Black) from their install and driver folders, without starting a process,
  prints the file's path and SHA-256 for IT, and links the IT guide in
  SECURITY.md.
- **No re-download loop.** Once a verified binary has been downloaded and then
  removed, the launcher no longer downloads it again on every run; it
  explains the removal instead. This includes security software that
  quarantines the file as soon as it is written, before it is moved into
  place. `bwn --bootstrap` downloads it again.
- **A failed first-run download says why.** It used to print "prebuilt
  unavailable" and then "no terminal here, so the launcher will not download
  it", even when the download had been allowed. It now prints the error, and
  on a network or TLS error explains `NODE_USE_ENV_PROXY=1` (Node's `https`
  ignores `HTTPS_PROXY` without it; Node 22.21+ and 24.5+) and
  `NODE_EXTRA_CA_CERTS`. A request that gets no data for 30 s fails instead of
  hanging.
- **No `--bootstrap` advice where the binary cannot run.** Without a
  terminal on musl or on a glibc older than 2.34, the launcher explains the
  platform instead of suggesting a download.
- **Tool checks work on stock Windows.** `bwn doctor` and the start-up
  "Missing ... tool(s)" notice searched with `which`, which Windows does not
  have, so without Git's Unix tools on PATH every tool showed as missing.
  They now search PATH directly and honour PATHEXT.
- **The documented glibc floor is the real one: 2.34**, not 2.35, which adds
  RHEL/Rocky/AlmaLinux 9, Amazon Linux 2023 and Fedora 35+. CI builds the
  x86_64 release binary on every PR and fails if it needs a newer glibc than
  the floor the launcher and docs state; the release checks each Linux binary
  again.

- **The `bwn` crate's install block no longer installs both crates.** Both
  provide a `bwn` binary, so the second install failed.

### Release process
- **A release runs the full CI on its own commit first,** with a blocking
  dependency audit, and tags, builds and publishes nothing unless it passes.
  CI no longer cancels runs on main.
- **Field testing.**
  - An install matrix installs the published package, or a packed tarball
    before release, in clean Linux containers (Debian, Ubuntu, RHEL family,
    Amazon Linux, Fedora, Alpine, nvm, no terminal), natively on x64 and
    arm64. A row passes only if bwn runs or the launcher explains why it
    can't. It runs after each publish and daily, with Windows Server 2022
    (PowerShell 5.1) and macOS jobs.
  - A lint for command blocks in docs catches the mistakes that broke real
    setups (Windows PowerShell 5.1 syntax, unguarded winget, msiexec without
    an exit-code check, Machine-scope PATH from an unelevated shell, and more).
  - A scheduled sentinel checks every 15 minutes that main's version reached
    npm and that CI on main is green, re-dispatching workflows GitHub dropped,
    and daily runs the dependency audit and checks labels and runner images.
    Failures open issues.
  - A rehearsal skill has every command handed to the maintainer run first in
    a matching clean machine (`field-adhoc.yml`), so nobody tests for us.
- macOS builds and tests moved from `macos-14` to `macos-15`.
- The publish summary reports what each step actually did.

## [0.14.9] - 2026-09-29

A full security audit of the harness, the npm launcher and the release
pipeline. Every finding below is fixed and covered by a test.

### Security
- **A cloned repo can no longer leak secrets through its instruction files.**
  `AGENTS.md`, `CLAUDE.md`, `.buildwithnexus/Agents.md`, `system.md` and
  project skills are read only when they resolve to a regular file inside the
  project. A symlink to `/proc/self/environ`, `~/.buildwithnexus/.env.keys` or
  anything else outside the tree (or to a sensitive file inside it) is
  skipped. In-tree links such as `CLAUDE.md -> AGENTS.md` still work.
- **The permission prompt shows the whole command.** It used to show an
  80-character, single-line preview, so a harmful tail or second line could
  hide behind `…`. Line breaks now show as `⏎`, and the prompt names what
  `s` / `a` would allow from then on.
- **"Allow this session" no longer covers every shell or interpreter call.**
  For `sh`, `bash`, `python3`, `node`, `env`, `xargs` and similar, an approval
  covers only that exact command.
- **Network tools ask per host.** `fetch_url`, `headless_browser`,
  `wait_for_url` and `open_browser` ask once per host and port under `ask`
  and `readonly`, because a fetch can send data out or reach local services
  (a Docker API on `127.0.0.1:2375`, a router). `"fetch *"` in
  `allowed_commands` restores the old behaviour. `open_browser` now opens
  only `http(s)` URLs and viewer file types, and no longer goes through
  `cmd /C start` on Windows.
- **Terminal escape sequences from untrusted text are neutralised** in the
  project-trust prompt, startup skill warnings, the completion popup, MCP
  notices and `/mcp`, `!cmd` and custom-script output, traces, and model
  lists from local servers. Bidi and invisible format characters now show as
  `<U+202E>`-style markers, so a command cannot be visually reordered.
- **The read-only command check sees the binary the OS runs**: `RG --pre …`
  or `find.exe -exec …` on a case-insensitive filesystem no longer pass as
  read-only. `git -C`, `--git-dir` and `--work-tree` are refused, and
  `git branch` counts as read-only only when it lists branches.
- **Git commands ask again when the repository's own config can run
  programs** (`core.fsmonitor`, `core.pager`, `diff.external`, filter or
  textconv drivers, `include.path`), since a repo that arrives with its
  `.git` could turn `git status` into code execution.
- **The Linux sandbox protects `.git` and `.buildwithnexus` even before they
  exist**, so a sandboxed command cannot create a `.git/config` or hook that
  runs later outside the sandbox.
- **MCP `readOnlyHint` is ignored unless you trust the server's hints**
  (`"trust_read_only_hints": true` on that server). A malicious server could
  otherwise mark a destructive tool read-only and skip the prompt.
- **A project settings file can no longer change the model** without being
  trusted. The model decides what you pay, and an unpriced model slipped past
  `max_budget_usd`.
- `/verify` and `/audit` go through the permission gate and hooks, so a
  read-only session no longer runs the project's build and test commands.
- `@url:` attachments only fetch `http(s)` URLs and pass them after `--`, and
  `@symbol:` passes the query as a pattern, so neither can inject curl or
  grep options. `@` paths to sensitive files are not attached.
- ffmpeg and ffprobe open only local files (`file:` input, file protocol
  only), so a repo file named like a URL cannot make them reach the network.
- Checkpoint undo refuses to write through a link that now points outside
  the working tree.
- Crafted web pages no longer crash the web tools (Unicode case folding
  shifted string offsets).
- Rust hooks compile into `~/.buildwithnexus/cache` instead of a shared temp
  directory, and temp media files are created exclusively.
- Workflow child runs pass the task after `--`, so a task cannot be read as
  a flag.

### Supply chain
- **The first-run download checks against checksums shipped in the npm
  package.** `publish.yml` verifies each release binary's build-provenance
  attestation before publishing and records the hashes in `checksums.json`;
  the platform packages are built from the same verified files. An asset
  replaced on the GitHub Release later is refused.
- The launcher ignores a platform package found in a parent directory's
  `node_modules` (another user could plant one) and a relative `BWN_BIN`.
- Release build jobs, which run dependency build scripts, now hold a
  read-only token. A separate job attests and uploads the binaries.

## [0.14.8] - 2026-09-29

### Added
- **Easier to approve on managed Windows machines.** The Windows `.exe` now
  carries a version resource (product name, publisher, version, original
  file name) and an application manifest that runs it with the caller's own
  rights (`asInvoker`) and never asks for elevation. Security tools and IT
  teams no longer see an anonymous binary. The release workflow fails if the
  resource is missing.
- **CycloneDX SBOM with every release** (`buildwithnexus.cdx.json`), listing
  every crate compiled into the binary.
- **Code signing, ready to switch on.** The release workflow can
  Authenticode-sign the Windows `.exe` through SignPath (free for open-source
  projects via the SignPath Foundation). It stays off until the project's
  SignPath account is configured; releases until then are unsigned, as
  before. Checksums and attestations are now computed after signing.
- **A guide for IT and security teams** in
  [SECURITY.md](SECURITY.md#for-it-and-security-teams): what bwn launches,
  which hosts it connects to, which files it writes, why EDR tools may block
  it, how to allowlist it by hash, signer or path, and a request an employee
  can send. Also a code signing policy and a README note for corporate
  machines.

## [0.14.7] - 2026-09-29

### Added
- **Sharp, full-resolution image previews over Sixel.** On terminals that
  support Sixel graphics (Windows Terminal 1.22+, WezTerm, foot, mlterm,
  xterm with `-ti vt340`), an attached image is drawn with its real pixels
  instead of coloured block characters, which made text in screenshots
  unreadable at thumbnail size. bwn asks the terminal at startup whether it
  supports Sixel and how large a character cell is, so the image fills
  exactly the rows it reserves. Works for native Windows (`bwn.exe` in
  Windows Terminal) as well as Linux, macOS and WSL. Set `BWN_IMAGES=sixel` to
  force it on, or `BWN_IMAGES=blocks` to keep block art. Needs ffmpeg, like
  block previews.

### Changed
- **Image previews follow the terminal size.** Resizing the window re-fits
  every image already in the transcript, for both Sixel and block previews.
  An image partly scrolled out of view is drawn cropped rather than
  overlapping the composer, and block previews shrink with area averaging so
  small text stays legible.
- Dependencies: crossterm 0.29, libc 0.2.189, serde 1.0.229, serde_json
  1.0.151, and criterion 0.8 for the benchmarks (which now use
  `std::hint::black_box`).

## [0.14.6] - 2026-09-29

### Changed
- **Image previews are thumbnails.** An attached image was drawn across
  nearly the whole terminal (up to 160 columns by 48 rows), pushing the
  conversation off screen. Previews now fit in half the terminal width (at
  most 80 columns) and a third of its height (at most 16 rows), keeping the
  image's proportions. The model still gets the full-size image.

## [0.14.5] - 2026-09-29

### Fixed
- **Esc and Ctrl+C stop the agent while it waits on the model.** The request
  ran on the UI thread, so an interrupt only took effect once the server sent
  data, and on Ollama not until the reply finished. With a local model loading
  or reading an image that meant minutes of an unresponsive Esc. Requests and
  streamed replies are now read on a worker thread, and an interrupt returns
  to the prompt within about 50 ms. Dropping the abandoned connection stops
  the server's generation once it starts sending.

## [0.14.4] - 2026-09-29

### Fixed
- **Images work in every mode.** An attached image used to reach the model
  only in a BUILD task; chat answers, PLAN and BRAINSTORM dropped it with
  "only BUILD mode tasks take images". Every mode now sends it, including
  the `/plan`, `/build` and `/brainstorm` commands and headless `bwn run`,
  `bwn plan` and `bwn brainstorm`. An approved plan carries the images into
  the build that executes it.
- **Ollama vision models without tool support work.** Models such as
  `gemma3` failed every task with `HTTP 400: … does not support tools`. bwn
  now retries without native tools, lists the tools in the system prompt with
  the JSON shape to call them, and parses the calls from the reply. The
  model is remembered, so later requests skip the failed attempt.
- **The same fallback on OpenAI-compatible servers keeps images** and
  describes the tools in text. It used to strip both, so a vision model
  behind llama.cpp or LM Studio lost the image and the tool list.
- **A path followed by punctuation attaches.** `what is in @shot.png?` read
  the file name as `shot.png?` and attached nothing.
- **The Linux binaries run on Ubuntu 22.04.** They were built on Ubuntu 24.04
  and needed glibc 2.39, so installs on 22.04 and Debian 12 failed with
  "GLIBC_2.39 not found". They are now built on 22.04 (glibc 2.35), and the
  release fails if a binary needs anything newer.

## [0.14.3] - 2026-09-29

### Security
- **Project settings need your trust before they can change anything that
  matters.** A repository's `.buildwithnexus/settings.json` and
  `settings.local.json` used to be merged over your own settings with no
  prompt, so a cloned repo could send your API key to its own `base_url`,
  switch to `permission: auto` and `sandbox: off`, or start an MCP server
  command. Without trust, a project file may now set only `model`,
  `reasoning_effort`, `temperature`, `max_tokens`, `context_tokens`,
  `instruction_files`, `images` and `notify`, may tighten `permission` and
  `sandbox`, and may lower `max_budget_usd`. Every other key is ignored until
  you answer one prompt that lists the keys the project wants. Headless runs
  never prompt: they ignore those keys and print one warning naming them.
  A trusted project can add MCP servers but can never change one defined in
  your own settings.
- **Hook trust covers the scripts hooks run.** Trust is recorded per project
  folder and per file name (settings.json and settings.local.json no longer
  share one entry) as a SHA-256 digest of the file plus every script inside
  the project that its hooks reference. Editing any of them asks again. The
  prompt appears only when a file defines hooks or other security keys, and
  names the right file.
- **"Always allow", `/permissions`, `/sandbox`, `/effort` and `/model` no
  longer copy project settings into your global settings.** They used to save
  the merged settings to `~/.buildwithnexus/settings.json`. They now change
  only their own keys in that file.
- Release workflow: permissions are now granted per job instead of
  workflow-wide, and checkouts no longer store the token in `.git/config`,
  so the Rust build (including dependencies' build scripts and proc-macros)
  has no token on disk to read. Releases run one at a time, a manual release is refused unless it starts
  from `main`, and a new tag points at the commit the workflow built.
- Every third-party GitHub Action is pinned to a full commit SHA. The
  publish workflow installs a pinned npm version instead of `npm@latest`,
  and CI downloads a pinned actionlint release and checks its SHA-256
  instead of piping an install script into bash.
- The publish workflow no longer places the `version_bump` input directly
  into a shell script, and it keeps the push token on disk only when it has
  to push a version bump.
- Dependabot now proposes weekly updates for the Rust crates as well as for
  Actions and npm.
- SECURITY.md no longer promises a `[y/N]` prompt before the first-run
  download. It now describes what actually happens (automatic in a
  terminal, opt-in with `--bootstrap` or `BWN_ALLOW_BOOTSTRAP=1` elsewhere),
  the full download host allowlist, and the optional OS sandbox.
- **Model text and tool output can no longer drive the terminal.** Escape
  sequences in assistant replies, thinking, tool output, tool-call previews,
  edit/write diffs and selection menus used to reach the terminal intact, so
  a reply could overwrite the clipboard (OSC 52), move the cursor to fake an
  `allow?` prompt, or disguise a link. ESC now shows as a visible `␛` and
  other control characters are dropped before the harness adds its own
  styling. Control characters are also stripped from OSC 8 link targets,
  the `allow?` approval line, and saved sessions replayed by `/resume`.
- **Ask mode no longer auto-approves compound commands.** A command whose
  binary was in `allowed_commands`, the project's "always allow" list or the
  session's approvals used to run without a prompt even when it chained,
  piped, redirected or substituted another command (`cat x; rm -rf ~`). Only
  a single plain command now skips the prompt: no `;`, `&`, `|`, `<`, `>`,
  backticks, `$(`, variable expansion, control characters or newlines. New
  "always allow" and "allow this session" answers are stored per binary and
  subcommand for multi-verb tools (`git status`, `npm test`); an existing
  single-word entry still covers plain commands of that binary. Session
  approvals are scoped to the project and `/permissions reset` clears them.
- **Read-only commands reject write and exec flags.** `rg --pre`,
  `sort -o`, `find -exec`/`-delete`/`-fprint`, `git -c`/`--output`/
  `--ext-diff`, `tree -o`, `uniq IN OUT` and similar no longer count as
  read-only in PLAN and BRAINSTORM, and are never auto-approved.
- **Secrets are harder to reach without a prompt.** The sensitive-path list
  now covers `.kube`, `.docker/config.json`, `.config/gh`, `.netrc`,
  `.npmrc`, `.pypirc`, `.git-credentials` and more, matches by path
  component (so `~/.ssh` itself counts), and follows symlinks. It applies to
  every path of `read_many_files` and to path arguments of shell commands, so
  `cat ~/.aws/credentials` always asks. `grep_files`, `list_tree` and
  `find_paths` skip credential directories.
- **Fetch tools refuse link-local and cloud metadata addresses.**
  `fetch_url`, `webfetch`, `headless_browser`, `wait_for_url` and web search
  never connect to 169.254.0.0/16, fe80::/10 or metadata hostnames, including
  through DNS or a redirect. Localhost still works for dev servers.
- **Hooks can no longer approve writes in read-only phases.** In PLAN,
  BRAINSTORM and readonly sessions a PreToolUse hook may deny a call but
  never allow past the read-only gate. Hooks see `plan` or `readonly` as the
  permission mode during those phases.
- **Harness recovery calls go through hooks and the gate.** The `write_file`
  and HTML artifact recoveries and the automatic `check_work` round are
  checked like model calls; a denied call is skipped and reported.
- **The sandbox protects `.git` and `.buildwithnexus`.** Under bubblewrap
  they are bound read-only inside the workspace and `/run` is a fresh tmpfs;
  under Seatbelt writes to them and Apple Events are denied. Sandboxed
  children no longer inherit `DBUS_SESSION_BUS_ADDRESS`, `SSH_AUTH_SOCK`,
  `*_API_KEY` or `HF_TOKEN`. `start_server` and `python_tool` now run under
  the sandbox policy, and `sandbox: require` refuses them without a backend.
  `start_server` commands also get the dangerous-command check.
### Fixed
- **`bwn init` keeps your existing settings.** Setup used to rewrite
  `settings.json` from defaults, dropping `allowed_commands`,
  `project_allowed`, `max_budget_usd`, `auto_update`, hooks and any other
  keys. It now changes only the provider, model, permission and base URL,
  and reports a failed save instead of printing "ready".
- **Long replies are no longer cut off at three minutes.** The HTTP client had
  a 180 second deadline that also covered the streamed reply, so a slow or long
  generation was dropped part way. There is now a 15 second connect timeout and
  a 300 second idle read timeout (set `BWN_READ_TIMEOUT_SECS` to change it).
  Model listing and warm-up probes keep their short timeouts.
- **A request is no longer sent twice after a read timeout.** Only failures to
  connect (refused connection, DNS) and HTTP 429 or 5xx are retried. A timeout
  or reset after the request went out is reported instead, since the server
  may still be working on the first one. Retry delays now carry 20% jitter.
- **API keys are never sent to a redirect target.** Redirects are no longer
  followed; a 3xx reply is an error that names where the server tried to send
  the request.
- **Truncated streams are errors.** A stream that ends without its finish
  marker (`message_stop`, `[DONE]`, a finish reason, or Ollama's `done`) now
  fails with "stream ended before completion" instead of returning a partial
  reply as if it were whole.
- **OpenAI-compatible servers' error payloads surface.** An `{"error": ...}`
  chunk in a stream, or an error body with HTTP 200, is now reported (redacted
  and truncated) instead of being read as an empty reply.
- **`/undo` no longer half-restores.** When any checkpoint in the set cannot
  be restored, nothing is touched and every blocking file is named. Write
  failures are reported per file, and real errors are no longer reported as
  "made no file changes". Restores keep the file's original permissions and
  write through a symlink to its target instead of replacing the link.
- **`/undo git` only runs `git checkout -- .`**, as its prompt says. It no
  longer runs `git clean -fd`, which deleted untracked files, and a failing
  git command is now reported as an error.
- **An image in the first prompt no longer drops the system prompt.** Images
  are attached to the build turn's own message after the system prompt, with
  `/btw` and hook context included. Chat, PLAN and BRAINSTORM turns say the
  images were not sent instead of leaking them into a later turn.
- **Keys typed while the agent streams are no longer lost.** The interrupt
  check read pending key events and kept only Ctrl+C and Esc. It now goes
  through the same reader as type-ahead, which buffers every other key.
- **The composer and footer no longer get stray text.** The type-ahead and
  spinner threads painted at the same time as the main thread. Each frame is
  now drawn under one render lock and written in one piece.
- **A crash or a kill signal restores the terminal fully.** Mouse tracking,
  focus reporting, bracketed paste, scroll margins and the cursor shape are
  now reset too, so the shell no longer prints `^[[<35;...M` on every mouse
  move afterwards. The panic hook is installed once instead of once per
  screen switch.
- **Pasting mid-turn cleans text the same way as at the prompt.** Pasted
  line breaks become spaces and control characters are dropped in all three
  paste paths, which now share one function.
- **Symlinks followed by `..` can no longer escape the workspace.** Paths
  are resolved through the deepest existing directory before `..` is folded,
  so `link/../file` is checked where the OS would write it.

### Changed
- crates.io publishing in the release workflow now fails the run when it
  cannot get an OIDC token or a publish fails, instead of passing with a
  warning. Versions already on crates.io are still skipped.
- A failed push of a manual version bump now stops the publish workflow
  instead of publishing a version that has no commit or tag on `main`.
- The published npm manifest lists a per-platform package in
  `optionalDependencies` only when that exact version exists on npm, and
  the run warns with the one-time command that publishes the missing ones
  (`scripts/first-publish-platform-packages.sh`, which now accepts
  `NPM_TOKEN`). The committed `package.json` no longer pins them to 0.12.7,
  a version that was never published. Installs are unaffected: the launcher
  downloads the checksum-verified binary on first run.
- CI runs the Rust test suite on macOS as well as Linux, compiles the tests
  on Windows, and caches Rust builds.
- README: added Requirements, local model setup, and a Headless and CI
  section with flags and exit codes, and corrected how the Linux sandbox
  treats `/tmp`. RECOVERY.md now describes the optional sandbox.
- Removed `docs/AGENT_DEFINITIONS.md`, `docs/DEEP_AGENTS_CLI_UX.md` and
  `.env.example`, which described code that no longer exists, and stopped
  tracking the `.omc/` tool state directory.

[Unreleased]: https://github.com/Garretts-Apps/buildwithnexus/compare/v0.15.0...HEAD
[0.15.0]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.15.0
[0.14.10]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.14.10
[0.14.9]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.14.9
[0.14.8]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.14.8
[0.14.7]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.14.7
[0.14.6]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.14.6
[0.14.5]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.14.5
[0.14.4]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.14.4
[0.14.3]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.14.3


## [0.14.2] - 2026-09-29

The "first hour" release: fixes from walking the first run, the TUI and
headless runs as a new user would.

### Fixed
- **Headless first run no longer hangs on setup.** With no settings file and
  no terminal (CI, pipes, `docker run` without `-t`), `bwn run` used to print
  the interactive provider menu into the pipe and exit with "setup
  cancelled", even when `ANTHROPIC_API_KEY` was set or `--provider` was
  passed. It now uses `--provider`, or the first provider whose API key is in
  the environment, without writing any settings. With neither, it exits with
  a message that names the three ways to fix it.
- **The launcher's missing-binary message leads with the fix that works.**
  In a non-interactive shell it now points to `bwn --bootstrap` first,
  instead of telling you to reinstall a platform package that may not be
  published.
- **Slash commands typed after a BRAINSTORM answer run instead of going to
  the model.** BRAINSTORM (the starting mode) kept its own follow-up prompt
  open after each answer, so `/exit`, `/help`, `/model`, `/undo` and `!cmd`
  typed there were sent to the model as questions, and `/exit` did not exit.
  They now go to the main prompt.
- **The footer no longer says "working · Esc to interrupt" while it waits
  for you.** The spinner stayed up after every BRAINSTORM answer.
- **Enter runs a command you typed in full.** With the autocomplete popup
  open, `/mode build` + Enter inserted the highlighted suggestion and waited
  for a second Enter.
- **A local model server that is not running fails in about a second.** A
  refused connection to localhost used to be retried 14 times over roughly
  two minutes; it now stops after one retry and names the fix
  (`ollama serve`, start LM Studio's server, or `/model` for a new address).
- **Setup lists the models an OpenAI-compatible server offers.** For LM
  Studio, llama.cpp and custom endpoints, setup asked for a model name and
  suggested pulling Ollama models even when the server was running; it now
  reads the server's `/models` first.
- **Headless runs that could not apply changes no longer report success.**
  With `--permission-mode ask` and no terminal, every edit and command is
  blocked; the run now warns before it starts and exits with code 3 instead
  of printing "done" and exiting 0.
- **Piped output is plain text.** Colour codes are dropped when stdout is not
  a terminal (CI logs, redirects); set `FORCE_COLOR=1` to keep them.
- **PLAN and BRAINSTORM say why an edit was skipped.** The model and the
  user saw "read-only mode: mutation skipped" even with `ask` permission; the
  message now names the mode and how to leave it.
- `help` or `?` on its own opens `/help`; `bwn sessions` says when there are
  none and how to resume one; the footer drops the context gauge instead of
  cutting it off on narrow terminals.

[0.14.2]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.14.2

## [0.14.1] - 2026-09-24

### Security
- **rustls 0.23.41 → 0.23.45** (and rustls-webpki 0.103.13 → 0.103.15):
  fixes RUSTSEC-2026-0285, where TLS 1.3 handshake messages were accepted
  across encryption level boundaries (CVSS 5.3, medium). buildwithnexus
  talks to every hosted model provider over this TLS stack, so the fix is
  shipped as a patch release. No code changes.

[0.14.1]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.14.1

## [0.14.0] - 2026-09-23

The "see it" release: the terminal shows you the actual pixels of what you
attach, code and tables render the way an editor would, and the footer tells
you what the agent is doing while it works — without giving up a microsecond
of the streaming path.

### Added
- **Pixel-perfect inline images.** On kitty, Ghostty, and WezTerm builds with
  Unicode-placeholder support, an attached screenshot is uploaded once through
  the kitty graphics protocol and drawn at real resolution inside the
  transcript. The image rows are ordinary text (U+10EEEE placeholder cells),
  so they scroll, wrap, and repaint like any other line, work through tmux
  (`allow-passthrough on`), and are freed when you leave the screen. Every
  other truecolor terminal gets the half-block preview, now sized to the
  terminal (up to 160 columns × 48 rows) instead of a 64×36 thumbnail. PNGs
  are read natively — no ffmpeg needed for a pasted screenshot; JPEG, WebP,
  GIF and a video's first frame go through ffmpeg when it is installed.
  Settings key `images`: `auto` (default), `kitty`, `blocks`, `off`;
  `BWN_IMAGES` overrides it.
- **Images show the moment you paste.** `Ctrl+V` renders the clipboard
  screenshot immediately, above the composer, so you see what the model will
  see before you send — and it is not drawn a second time on submit.
- **Drop a file onto the terminal.** A pasted or drag-and-dropped path to an
  image or video (quoted, `file://`, `~/`, or shell-escaped, as macOS, iTerm2,
  and WezTerm produce) becomes an `@attachment` token and previews at once.
  Paths with spaces are quoted for you.
- **Syntax highlighting in code blocks.** Streamed and replied fenced blocks
  are highlighted for Rust, C/C++, JavaScript/TypeScript, Python, Go,
  Java/Kotlin/Swift, shell, Ruby/PHP/Lua, SQL, JSON, YAML, TOML, CSS, HTML
  and diffs — a zero-dependency lexer that runs once per block when the
  closing fence lands, never per token.
- **Markdown tables.** `| a | b |` blocks are collected while streaming and
  drawn as one aligned table: bold header, `─┼─` rule, `:--`/`:-:`/`--:`
  alignment, and columns that shrink with `…` instead of wrapping.
- **More markdown.** `---` rules, `####` headings, `- [ ]`/`- [x]` task
  lists, and `~~strikethrough~~`.
- **Live footer while the agent works.** A spinner, elapsed time, streamed
  tokens per second, and `Esc to interrupt` replace the idle footer text for
  the duration of a turn; the model name is always shown.
- **Desktop notification when a long turn ends.** After a turn of 8 s or
  more finishes while the terminal window is unfocused, buildwithnexus emits
  OSC 99 (kitty), OSC 777 (urxvt, VTE, WezTerm) and OSC 9 (iTerm2, WezTerm,
  Windows Terminal) plus BEL. Settings key `notify`: `auto` (default),
  `always`, `off`. Focus is tracked with CSI ?1004.
- **Taskbar progress.** Windows Terminal, Ghostty and ConEmu show an
  indeterminate progress state while the agent runs (OSC 9;4).

### Fixed
- **Scrolling back stays put.** Reading earlier output while the model
  streams no longer yanks the view: new rows raise the scroll offset by the
  same amount, so the row you were reading stays where it is.
- **Colour survives wrapping.** A styled span that wrapped onto a second row
  lost its colour on that row; the active SGR state is now replayed at each
  continuation row (this also keeps image rows intact on narrow terminals).
- The escape-sequence scanner understands APC, DCS, PM and SOS strings, so a
  graphics payload or a tmux passthrough never counts as visible width.
- Selection copy across an image row yields spaces, not placeholder bytes.
- The Ctrl+V attachment token is quoted when the temp path contains spaces
  (Windows).

[0.14.0]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.14.0

## [0.13.0] - 2026-09-08

The audit release: every claim on buildwithnexus.dev was checked against the
code, the gaps against other terminal agents were closed where they were
closable, and twelve bugs found along the way were fixed.

### Added
- **Full MCP client.** Servers under `mcp_servers` connect over stdio or
  Streamable HTTP, complete the `initialize` handshake, and discover tools
  with `tools/list`. Discovered tools are offered to the model as
  `mcp__<server>__<tool>` with the server's schema, pass through the
  permission gate (read-only when the server says `readOnlyHint`), and answer
  over a persistent connection. `/mcp`, `/mcp <name>`, `/mcp add|remove|reload`
  and `buildwithnexus mcp …` manage them; `doctor` connects to each.
- **Project instructions.** `AGENTS.md` (or `CLAUDE.md`) files from the git
  root down to the cwd, plus `~/.buildwithnexus/AGENTS.md`, are injected into
  every mode, sub-agent and headless run (32 KiB per file, 96 KiB total).
  `instruction_files` in settings changes the names. `/init` offers a starter.
- **Agent Skills.** `<name>/SKILL.md` folders with `name`/`description`
  frontmatter are discovered in `.buildwithnexus/skills`, `.claude/skills` and
  `.agents/skills` (project and home) and via `skill_dirs`; flat `.md` skills
  still work. Only name and description enter the context until loaded.
- **Token and cost accounting.** Server-reported usage from all three wire
  protocols feeds a session ledger: `/cost` shows tokens and an estimated
  price for known models (never a made-up one), `/context` uses measured
  prompt sizes, and `--max-budget-usd` / `max_budget_usd` stops the agent
  before the next request once the estimate passes the limit.
- **Reasoning depth.** `reasoning_effort` (`off` by default), `--effort`, and
  `/effort` map to adaptive thinking or thinking budgets on Claude,
  `reasoning_effort` on OpenAI reasoning models, and `think` on Ollama.
- **Opt-in OS sandbox for shell commands.** `"sandbox": "auto"|"require"`,
  `--sandbox`, `/sandbox`: `bwrap` on Linux, `sandbox-exec` on macOS; the
  filesystem is read-only outside the workspace and `/tmp`,
  `sandbox_network: false` cuts the network. Sandboxed commands are marked.
- **Headless plan.** `plan --yes|-y` auto-approves; without a TTY and without
  `--yes` it exits 2 at once. `--json` emits a `plan` event before executing.
- **check_work is enforced.** A BUILD turn that mutated files cannot finish
  without a build/test/lint pass; failures go back to the model for one more
  round. The verifier also runs for `--json` runs and emits a `verify` event.
- **Hooks.** Glob matchers (`*_file`, `mcp__*`), real `session_id`,
  `transcript_path` and `permission_mode` in every payload, `PrePrompt` and
  `SubagentStop` events, `Stop` in every mode, `SessionStart`/`SessionEnd`
  once per process.
- **Workflows** run while you are idle (a 1 s scheduler), up to
  `max_concurrent_workflows` (default 2) at once, and keep their logs in
  `~/.buildwithnexus/workflows/` across restarts.
- **Windows.** Clipboard image paste and text via PowerShell, `icacls` on the
  key file, `tasklist`/`taskkill` for background servers, Ctrl+Break and
  console-close terminal restore, PowerShell/cmd/Python/Git Bash hook scripts.
- `--` , `--yes`, `--effort`, `--max-budget-usd`, `--sandbox` in `--help`.

### Changed
- **BRAINSTORM is read-only**, as documented: it gets the read-only tool set
  and refuses mutations under every permission level; action requests still
  switch you to BUILD.
- **"Always allow" is per project** (`project_allowed` in user settings);
  `/permissions reset` forgets the current project's answers.
- **Local OpenAI-compatible servers get native tool schemas** (llama.cpp,
  LM Studio, vLLM, Ollama `/v1`); a 400 that names tools or templates triggers
  one tool-less retry that is remembered for the session.
- `auto_update: "install"` only applies to npm installs; cargo and source
  builds are capped to `notify`.
- Text attachments are capped at 256 KiB with a visible truncation marker and
  a warning instead of silently pasting the `@` token.
- `/verify` runs `check_work` and reports the test verdict explicitly.
- The `effort` settings key is retired (it was never read); reasoning depth
  lives in `reasoning_effort`.

### Fixed
- Readonly mode could execute an approved destructive command through the
  confirmation path.
- Ctrl+Q / Ctrl+X edited the newest queued message while auto-send took the
  oldest; both now act on the message that sends next.
- Ctrl+G flattened newlines from `$EDITOR`; multi-line results submit as a
  multi-line prompt.
- `/model http://host/v1 model` was routed to OpenRouter; URLs now select the
  custom endpoint.
- Unknown `--options` launched the TUI; they now exit 2 with a message.
- `/rules` claimed YAML support; JSON only, and unreadable rule files warn.
- Selection copy confirmation names OSC 52 and is suppressed on terminals
  known to drop it.
- Slash-command popup shows descriptions for skills and custom commands.
- Checkpoint directory comment named the wrong path.

### Documentation
- README and buildwithnexus.dev corrected: ~4 MB binary, incremental wrap
  cache (not incremental rendering), changed-span diffs, three wire
  protocols, update notices by default, first-run download install story,
  and the new features above.

[0.13.0]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.13.0

## [0.12.15] - 2026-09-08

### Fixed
- **Shift+Tab keeps your draft.** Cycling modes used to discard the composer,
  including the cursor position and any continuation lines entered with a
  trailing `\`. The draft, its lines, and the cursor now survive the mode
  change. Shift+Tab is ignored inside ordinary y/N and question prompts
  instead of cancelling them.
- **Autocomplete replaces the whole token.** Accepting a completion with the
  cursor mid-token (for example `/he|lp`) produced `/helplp`. Tab and Enter
  completion now replace the entire token and reuse an existing separator
  instead of inserting a second space.
- **Mouse selection around wide characters.** Drag and double-click selection
  now map terminal columns to characters, so text after CJK characters or
  emoji highlights and copies the right span, and combining marks stay
  attached to their base character.
- **npm launcher consumes `--bootstrap` consistently.** When a binary was
  already installed, `bwn --bootstrap ...` forwarded the flag to the native
  CLI, where it could be read as task text. The launcher now strips it in
  every case and preserves a literal `--bootstrap` after `--`.
- **CLI argument parsing.** Options that take a value (`--provider`,
  `--model`, `--permission-mode`, `--prompt`) now fail with exit code 2 when
  the value is missing or empty instead of silently consuming the next
  argument. `--` ends option parsing so a task can contain literal option
  names (including `--json`), and `--help` lists the global options.

### Added
- Regression coverage for the fixes above: Rust unit tests for the parser,
  completion, and selection helpers; a Node test for the npm launcher; and a
  PTY test that drives the real TUI. All three run in CI.

[0.12.15]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.15

## [0.12.14] - 2026-07-30

### Fixed
- **PLAN mode approval menu.** The approval popup renders in place above the
  composer (pre-reserved height, synchronized updates) instead of duplicating
  on Up/Down navigation. Esc or closing the menu cancels the plan instead of
  executing it, and the `exit_plan` log noise is gone. PLAN mode switches to
  BUILD automatically once the plan completes.
- **PLAN prompt quality.** Raw tool lists are rejected as plan steps, every
  valid step is accepted, and the PLAN system prompt requires a tech stack and
  file specs and asks about design choices and edge cases up front.
- **Composer.** Full editing keys, cursor movement, key repeat, and paste
  restored in the queued composer; flicker and missing character repaints
  eliminated; the composer stays interactive at all times and the
  "bwn is working" text is gone. Keystrokes typed during questions and menus
  are no longer delayed, duplicated, or lost.
- **`/model` menu** no longer duplicates entries, and the local server probe
  timeout is longer.
- Streamed code blocks no longer auto-copy to the clipboard.
- A full TUI audit pass closed 35 rendering, cursor, and input-handling
  findings.

[0.12.14]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.14

## [0.12.13] - 2026-07-29

### Fixed
- **Slash Commands Trailing Whitespace Guard (`/model`, `/mode`, `/permissions`):**
  Fixed whitespace handling so typing `/model ` (with trailing spaces) opens the interactive
  model selection dialog instead of skipping execution.

[0.12.13]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.13

## [0.12.12] - 2026-07-29

### Fixed & Enhanced
- **In-Place Menu Rendering (`/model`, `/mode`, `/permissions`):**
  TUI selection menus now update in-place using cursor movement, eliminating duplicated
  lists when navigating with Up/Down arrow keys. Upon selection, menus clear cleanly into a single confirmation line.
- **Auto `llama-server` Background Launcher:**
  Selecting a local GGUF model automatically checks ports 8080/1234/11434/8000. If no server is running, it finds `llama-server` on the host machine and launches it automatically in the background.
- **Cloud Provider Base URL Reset:**
  Swapping between cloud model providers automatically resets `base_url` to the target provider's default API endpoint.

[0.12.12]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.12

## [0.12.11] - 2026-07-29

### Added & Fixed
- **Interactive Slash Command Dialogs (`/model`, `/mode`, `/permissions`):**
  Commands like `/model`, `/mode`, and `/permissions` now pop interactive TUI dialogs
  navigable with `Up`/`Down` arrow keys (`j`/`k`) and `Enter` to select.
- **Strict Slash Command Handling:** Unrecognized commands (e.g. `/unknown`) show an error
  message instead of falling through as natural language prompts to the AI.
- **Immediate Cancellation (`Esc` & `Ctrl+C`):** Pressing `Esc` or `Ctrl+C` now aborts HTTP stream
  chunk reading and running child processes instantly. `Esc` preserves the message queue
  for the next queued task, while `Ctrl+C` clears the queue for a full stop.
- **Clean Footer Statusline:** Context & token usage (`ctx: 25% 32k/128k`) move to the footer
  statusline below the composer box, eliminating log clutter in the chat thread.

[0.12.11]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.11

## [0.12.10] - 2026-07-29

### Changed
- **First-run binary download is now automatic for interactive sessions.**
  When the platform binary is missing (platform packages not yet published,
  or installed with `--omit=optional`), `bwn` now downloads the
  checksum-verified binary from the GitHub release immediately instead of
  prompting `[y/N]`. The user already opted in by running
  `npm install -g buildwithnexus`. Non-TTY environments (CI, scripts, pipes)
  are unaffected — they still require `--bootstrap` or `BWN_ALLOW_BOOTSTRAP=1`.

[0.12.10]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.10

## [0.12.9] - 2026-07-29

### Fixed
- **Brainstorm mode no longer auto-switches to Build on greetings.** Saying
  "hi" (or any short greeting) while in Brainstorm mode previously fell through
  to `run_brainstorm`, where the LLM could emit `[SUGGEST:BUILD]` and trigger
  a mode change. Simple conversational inputs are now detected before the
  Brainstorm early-return and are handled by the lightweight chat loop
  regardless of current mode.
- **Sessions no longer start in Build mode.** The default mode is now
  Brainstorm, so launching bwn and typing a casual message no longer kicks
  off an agentic build run.

[0.12.9]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.9

## [0.12.8] - 2026-07-29

### Changed
- **bwn has a personality now — it's a partner, not a job title.** In PLAN
  mode a casual aside like "do you know any jokes?" used to get a stiff,
  robotic refusal ("I am a planning engineer and I don't have the ability to
  tell jokes"). A shared voice is now injected ahead of every mode — build,
  plan, brainstorm, chat — establishing bwn as a coding *partner* (it builds
  autonomously, brainstorms, or plans *with* you), plain-spoken and dry, that
  answers a greeting or a joke like a person would before getting back to
  work. The old "planning engineer" / "senior software engineer" job-title
  framing is gone. Wit stays in the conversation and out of error paths.
- **PLAN mode no longer forces a plan onto non-tasks.** A greeting, a joke,
  an identity question, or a creative aside gets a short natural reply instead
  of being pushed through the plan-or-refuse machinery; only concrete
  workspace tasks are decomposed into steps.

### Added
- **`check_work` — a one-call "does my change actually work?" button.** It
  auto-detects the project (Cargo, npm/pnpm/yarn, Python, Go) and runs its
  build, tests, and linter, returning a concise pass/fail report. A missing
  linter is reported as skipped, never a false failure. BUILD mode is now told
  to check its work before finishing, and never to claim a check passed that
  it didn't run.

### Fixed
- **A long BUILD turn lands gracefully instead of erroring at the step
  budget.** Reaching the per-turn step limit used to surface a raw
  "reached the 30-step limit without finishing" error. The budget is larger
  now, a wrap-up nudge fires one step before it runs out, and if the work is
  still going it ends with an honest summary of what got done and the exact
  next step — never a scary error for simply doing a lot.

[0.12.8]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.8

## [0.12.7] - 2026-07-21

### Changed
- **The thinking stream renders markdown as formatted text.** Headings,
  `**bold**`, `*italic*`, `` `code` ``, bullets, and quotes in the model's
  reasoning showed their raw markers; they now render as real styling, kept
  in the muted thinking palette so reasoning stays visually quiet. Lines are
  buffered to their newline before rendering, so a half-streamed span never
  flashes as raw text.

### Fixed
- **No more whole-screen flashes, and the queued-prompt bar always shows.**
  The terminal's scroll region is sized from the reserved rows, which grow
  when you queue a prompt — but it was only re-applied on launch and resize,
  never when a prompt was queued mid-stream. The stale region let streaming
  output scroll over the queued-composer row (it would vanish, notably on
  Ghostty) and occasionally scroll the whole screen (a visible flash). The
  region is now re-asserted whenever the reserved-row count changes, and the
  queued-composer row paints as one atomic frame.

[0.12.7]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.7

## [0.12.6] - 2026-07-18

### Added
- **A killed TUI restores your terminal.** SIGTERM, SIGHUP, SIGINT, and
  SIGQUIT now run an async-signal-safe handler that leaves the alternate
  screen, shows the cursor, disables mouse/paste reporting, and restores the
  pre-raw terminal settings before exiting — no more `reset` after an external
  kill. Adds `libc` as a unix-only direct dependency (it was already in the
  tree via crossterm), making it six direct dependencies.
- **`doctor` now probes your configured provider live.** A `provider` line
  runs the same one-token validation `/model` uses — a present-but-rejected
  key, a wrong model name, or an unreachable server shows up in `doctor`
  instead of on your next prompt. Ollama is probed for free via its API;
  hosted providers pay one output token, which is what a diagnostic command
  is for.

### Changed
- **`/undo git` and `/undo all` now confirm before destroying work.** Every
  file write asks first — but the two most destructive commands didn't:
  `/undo git` silently discarded ALL unstaged changes (including your hand
  edits), and `/undo all` rewound 24 hours of checkpoints unprompted. Both
  now state exactly what they're about to do (`/undo all` lists the files)
  and require a `[y/N]`.
- **Readable dim text.** Tokyo Night's classic comment color (#565f89)
  measured 2.76:1 against the background — below the WCAG AA minimum and
  genuinely hard to read for tips, hints, paths, and help text. Secondary
  text is now #7e88b3 (4.93:1): same blue-violet comment family, still
  clearly quieter than body text. Status meaning never relies on color
  alone (✓/✗/⚠ glyphs, +/- diff gutters), and the error red is Tokyo
  Night's pink-red, which stays distinct under red-green color blindness.
- **"Write a poem" no longer gets nudged into writing files.** The
  explain-vs-act nudge now requires workspace evidence in the task (a path,
  file, project, bug, …) on top of an imperative verb — a chat deliverable
  like "write a poem about pirates" is answered in the transcript, while
  "write a poem to pirates.txt" still acts on the file.

### Fixed
- **Settings, API keys, memory, history, and checkpoints are written
  atomically too** — and the key store's permissions are now tightened on
  the temp file *before* it becomes visible, so `.env.keys` is never
  world-readable, even for an instant. Checkpoint restores are atomic as
  well: recovery can't truncate the file it's recovering.
- **All file writes in the tool layer are atomic.** `write_file`,
  `edit_file`, `multi_edit`, `create_docx`, artifact writes, and the editor
  tools wrote directly to the destination — a crash or power loss mid-write
  could leave a truncated file. Every content write now goes through
  same-directory temp + rename (which also can't split a file across
  filesystems), and the destination's permissions are copied onto the
  replacement, so editing a script no longer risks silently stripping its
  executable bit.

[0.12.6]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.6

## [0.12.5] - 2026-07-19

### Added
- **Startup tips** — one rotating dim line under the banner: half real tips
  (Shift+Tab modes, Ctrl+V screenshots, /checkpoint), half personality
  ("bwn started faster than you read this sentence"). Jokes live here and
  only here — error messages stay strictly business.
- **The letsbeheroes skill collection** — eight bundled process skills
  (`/letsbeheroes`, `/hero-brainstorm`, `/hero-plan`, `/hero-execute`,
  `/hero-debug`, `/hero-ship`, `/hero-wait`, `/hero-subagents`) that encode
  the working discipline: brainstorm → plan → execute → debug → verify,
  plus condition-based waiting and subagent delegation.
- **Any OpenAI-compatible endpoint as a provider** — new `custom` preset for
  vLLM / TGI / LiteLLM / gateways: `/model` takes `<url> <model>` directly or
  walks through URL, optional `CUSTOM_API_KEY`, and model name. A configured
  key is never sent over plain HTTP to a non-loopback host.
- **Any OpenRouter model** — `/model org/model` (e.g. `meta-llama/…`,
  `google/gemini-2.5-pro`) routes to OpenRouter automatically.

### Fixed
- **Pending `/schedule` and `/loop` workflows survive restarts.** They lived
  only in process memory, so quitting, crashing, or resuming a session
  silently discarded them. Pending workflows now persist to
  `~/.buildwithnexus/workflows.json` (atomic writes, file removed when the
  queue is empty) and are restored at the next interactive launch with a
  visible "⟳ restored N scheduled workflows" notice. Loop iteration counts
  and next-fire times carry over; headless workflow subprocesses never touch
  the store.
- **Bare `/undo` now reverts the whole last agent turn.** After a partial
  multi-file edit (the agent changed three files and broke two, or Esc landed
  mid-batch), `/undo` used to restore only the single most-recent checkpoint —
  it looked like an undo while quietly leaving the other files changed. Bare
  `/undo` now restores every file the last agent turn touched, in the right
  order even when one file was edited several times; `latest` keeps the old
  single-checkpoint behavior.
- **Rapid multi-file batches no longer lose checkpoints.** Checkpoint ids
  were timestamp-only, so several writes in the same millisecond overwrote
  each other's snapshots on disk — some files in a fast batch were silently
  unrecoverable. Ids now carry a sequence number, which also makes restore
  ordering deterministic within a millisecond.
- **Malformed tagged tool calls are reprompted, not presented as answers.**
  When a local model emits a tool call as tagged text (`<tool_call>{…}`) and
  the JSON inside is broken or cut off, the agent loop previously treated the
  raw markup as the final answer and stopped. It now detects the failed tool
  intent, feeds the model one corrective message ("re-emit as exactly one
  JSON object…"), and only then gives up — bounded to a single retry so a
  model that can't produce valid JSON still terminates.
- **`/model` now swaps providers, not just the model string.** Picking an
  Ollama or OpenAI model while on Anthropic previously sent the new name to
  the old provider's API. The picker now maps every choice to the provider
  that serves it, walks you through a missing API key on the spot, checks
  that Ollama is reachable and actually has the model (with install/pull
  steps when not), and keeps the current model on any failure instead of
  reporting a successful swap that would break the next prompt.
- **"✓ hot-swapped" is only printed after live validation.** Every swap
  (except Ollama, which is validated via its API beforehand) runs a one-token
  probe through the real request path first — a rejected key, an unknown
  model name, or an unreachable server fails the swap on the spot with a
  targeted hint, instead of surfacing as a raw HTTP error on your next
  prompt.
- **Broken settings files are now diagnosed, never silently dropped.** A JSON
  typo in any settings file previously made the CLI act as if you'd never
  configured it — and first-run onboarding could then overwrite your config.
  Startup now prints one warning per unusable file with the exact line and
  column, refuses to re-onboard while broken files exist, and
  `buildwithnexus doctor` lists every settings file with its parse status.

[0.12.5]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.5

## [0.12.4] - 2026-07-16

### Fixed
- **The first-run `[y/N]` consent prompt now actually waits on macOS.** Node
  keeps a TTY stdin in non-blocking mode, so the launcher's synchronous read
  threw `EAGAIN` and fell through to "native binary not found" before you
  could answer. The prompt now reads from a fresh blocking `/dev/tty` handle
  (falling back to stdin where `/dev/tty` doesn't exist, e.g. Windows).

[0.12.4]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.4

## [0.12.3] - 2026-07-14

### Changed
- **Auto-update now defaults to notify-only.** New `auto_update` setting in
  `settings.json`: `"off"` (no check, no notices), `"notify"` (daily check,
  one-line startup notice, never installs — the default), `"install"` (the
  previous behavior: silent background `npm install -g`). `BWN_NO_AUTO_UPDATE=1`
  still works and caps `"install"` back to `"notify"`. A tool that edits files
  and runs commands should not change its own executable without being asked.
- **First-run binary download requires explicit consent.** When the platform
  package is absent, the launcher now asks `[y/N]` on a TTY, or requires
  `bwn --bootstrap` / `BWN_ALLOW_BOOTSTRAP=1` in non-interactive use — it no
  longer downloads automatically.

[0.12.3]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.3

## [0.12.2] - 2026-07-14

### Fixed
- **Bundle analyzers work now.** Added a `browser` field pointing at a
  dependency-free stub entry (`index.browser.js`) — tools like bundlephobia
  that webpack-bundle the package no longer fail on `child_process` and the
  platform binary packages the Node entry resolves. The stub exposes
  `{ version }` and throws clearly if the CLI surface is called in a browser.
- Publish workflow: the crates.io "already published" check queried the local
  workspace instead of the registry (`cargo info` resolves workspace members
  locally), silently skipping real publishes. It now asks the sparse index.

[0.12.2]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.2

## [0.12.1] - 2026-07-14

Supply-chain hardening: the npm install is now inert and auditable at a glance.

### Changed
- **Per-platform binary packages.** The prebuilt binary ships as five
  `buildwithnexus-<os>-<cpu>` packages selected automatically via
  `optionalDependencies` (the esbuild pattern). The main package is ~7 readable
  files with **no install scripts, no network code, no shell-outs (beyond
  spawning the CLI itself), no bundled sources, no eval** — supply-chain
  scanners have nothing to flag. Binaries are SHA-256-verified when packaged
  and carry build-provenance attestations.
- **Auto-update moved into the binary.** The daily npm-registry check and
  silent `npm install -g` refresh now run inside the CLI (background thread,
  never blocks startup) instead of the npm wrapper. `BWN_NO_AUTO_UPDATE=1`
  still disables installs; an update notice prints on the next launch.
- Installs with `--omit=optional` skip the platform binary; point `BWN_BIN`
  at a self-built binary (documented in the launcher's error message and at
  buildwithnexus.dev/docs/install).
- Added a `main` entry point (`index.js`) with a tiny programmatic API
  ({ version, binaryPath, run }) so bundle analyzers stop erroring on the
  bin-only package.

[0.12.1]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.1

## [0.12.0] - 2026-07-14

The Ferrari release: a full UI/UX overhaul — instant, multimodal, and clean.

### Added
- **Live slash-command autocomplete.** Typing `/` (or `@`, or a sub-argument)
  opens a popup above the composer with one-line descriptions for all built-in
  commands; ↑/↓ navigate, Tab/Enter accept, Esc dismisses. Removed the ghost
  commands (`/effort`, `/plugin`, `/marketplace`) that autocomplete offered but
  no handler implemented.
- **True multimodal input.** `Ctrl+V` pastes clipboard images (Wayland/X11/
  macOS/WSL) as attachments; `@clip.mp4` (and other containers) is parsed with
  ffmpeg/ffprobe into up to 8 evenly-sampled frames plus a metadata block.
  Both are gated on a per-model vision-capability check — text-only models get
  an explicit notice instead of silently dropped images.
- **Clickable files and links (OSC 8).** Markdown links and `⏺ edit/write`
  headers are terminal hyperlinks: click a path to open the file in the OS
  default app. The ANSI scanners learned OSC strings so links cost zero
  display columns.
- **npm auto-update.** A detached background process checks the registry at
  most once a day and silently installs newer versions; the next launch prints
  a one-line notice. Opt out with `BWN_NO_AUTO_UPDATE=1`. Startup never waits
  on the network.
- **Esc interrupts the agent** (with queued prompts auto-sending next turn),
  double-click word / triple-click line selection with a soft theme highlight
  and a "⎘ copied" footer flash, prefix-filtered ↑ history that preserves the
  in-progress draft, Ctrl/Alt+←→ word jumps, and mode-aware cursor shapes
  (accent bar / vim block / visual underline) hidden while the agent works.

### Changed
- **GitHub-grade diffs.** One renderer for edit previews, write previews, and
  applied changes: dual line-number gutter, background-tinted rows, word-level
  change emphasis on replacement pairs, hunk elision, and a 40-row cap.
  `NO_COLOR` keeps signed `- / +` text.
- **Bordered composer box** (opencode-style) with the spinner inside showing
  elapsed seconds and an interrupt hint; minimal banner (gradient wordmark +
  aligned model/cwd/mode rows) replacing the emoji-heavy boxed header.
- **Grouped, auto-aligned `/help`**, rendered markdown for `/verify`, rule
  violations, `/agents`, and `/memory` (no more raw `##`/`**`), human-readable
  verification statuses, and a consistent `✓ / ✗ / ⚠ / ⟳` message vocabulary.
- Removed fabricated telemetry: the hardcoded `est. cost` figure and the
  tok/s badge derived from character counts are gone; the context meter shows
  only real used/total tokens.

### Fixed
- Tool denials rendered dim-grey (now red); failed background workflows were
  announced in success-green; a raw `eprintln!` corrupted the alt-screen
  during `/resume`; HTTP retries were silent for up to 10s (now a visible
  `⟳ retrying` line); tool previews and the permission prompt could smear a
  multi-line command across the screen (capped at 80 cols, prompt legend on
  its own line).

### Performance
- **~215× faster streaming renders.** The renderer re-wrapped the entire
  transcript on every streamed chunk/keystroke/scroll; now each line wraps
  once (incremental cache), repaints are coalesced to ~60fps and applied as
  atomic frames (DEC 2026), and multi-line blocks (diffs, code, command
  output) paint in one repaint instead of one per row.
- Instant startup: dependency probes moved off the critical path; the screen
  paints chrome immediately so there is never a black frame.

[0.12.0]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.12.0

## [0.11.4] - 2026-07-07

Gemma local-model support, from a real gemma-2-2b-it session on llama.cpp.

### Added
- **Parse Gemma's `tool_code` tool-call format.** Gemma emits tool calls as a
  ```` ```tool_code ```` fenced Python call — `write_file("/p", """…""")`,
  sometimes wrapped in `print(...)`. The harness only understood JSON/`<tools>`,
  so it treated the call as prose and (eventually) published an empty artifact.
  The recovery parser now handles the Python-call syntax: it unwraps `print()`,
  splits arguments while skipping triple/single/double-quoted strings, and maps
  keyword and positional args through each tool's signature. Verified: Gemma's
  `write_file` now executes on the first attempt.

### Fixed
- **Gemma multi-turn no longer crashes on the chat template.** Gemma's llama.cpp
  template rejects the `system`/`tool` roles and requires strictly alternating
  user/assistant turns, so any turn carrying a tool result 400'd and the agentic
  loop died after one step. On such a template error the OpenAI-compatible path
  now retries once with a flattened, strictly-alternating message body. (qwen's
  template accepts the standard roles, so its path is unchanged.)

[0.11.4]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.11.4

## [0.11.3] - 2026-07-07

Small-model BUILD reliability, from a real qwen2.5-coder-1.5b session that
looped instead of building a page.

### Fixed
- **Small models no longer stall on the clarifying-question tool.** The
  `question` tool is dropped from the compact (local/small-model) tool set — a
  1.5B model was re-asking "use a framework?" endlessly instead of building. It
  now acts on sensible defaults; larger models and PLAN mode keep the tool.
- **Question answers now echo what you type.** The answer prompt was a
  multi-line string, which mis-positioned the alt-screen composer cursor and
  hid typed input. The question prints on its own line and the answer is read
  with a single-line prompt.
- **HTML artifacts that link a local stylesheet are rejected** with an
  actionable message (name the file, inline the CSS), mirroring the existing
  local-`<script src>` check; the too-small message now explicitly demands a
  single self-contained file with no external links.

[0.11.3]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.11.3

## [0.11.2] - 2026-07-07

Document generation, web-content quality, TUI, and small-model streaming
refinements — all additive fixes, no breaking changes.

### Added
- **Rich Word document generation.** `create_docx` now renders inline markdown
  (`**bold**`, `*italic*`, `` `code` ``) as real Word runs instead of literal
  asterisks, and converts markdown tables (`| col | col |`) into bordered Word
  tables with a bold header row. Emphasis is conservative — arithmetic like
  `5 * 3`, glob patterns, and `snake_case` are left untouched.
- **Structured web search.** `web_search` returns numbered `title / url /
  snippet` results (recovering the real target URL from DuckDuckGo's redirect
  wrapper) instead of the raw page run through an HTML stripper — far easier for
  small models to read. Falls back to the stripped text if the markup changes.

### Fixed
- **Numeric HTML entities in fetched content.** `strip_html` and the search
  parser now decode decimal/hex character references (`&#8217;`, `&#x2019;`,
  `&mdash;`), so fetched pages and snippets no longer show garbled curly quotes
  and dashes.
- **TUI markdown emphasis.** An unbalanced `` ` ``, `**`, or single `*` (e.g.
  `5 * 3`) no longer styles the rest of the line — the inline renderer only
  opens a style when a matching closer exists ahead and the marker flanks
  non-space text.
- **`<think>` reasoning leakage.** Reasoning models (DeepSeek-R1 distills, Qwen
  thinking variants) that emit `<think>…</think>` inline in content no longer
  leak that into the final answer or written files: it's stripped on the
  non-streaming parse path, and a `<think>`/`</think>` tag split across streaming
  chunks (routine with token-by-token local streaming) is now reassembled
  instead of leaking a partial marker.

[0.11.2]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.11.2

## [0.11.1] - 2026-07-07

### Fixed
- **Qwen2.5-Coder tool calls now work on llama.cpp/Ollama.** Small local coder
  models emit tool calls as `<tools>{…}</tools>` / `<tool_call>{…}</tool_call>`
  text in the message content rather than as native `tool_calls`. The text
  recovery parser only understood bare or ```json-fenced JSON, so these calls
  were treated as prose and the model never acted (an end-to-end run against
  qwen2.5-coder-1.5b produced no file). The parser now strips the XML tool tags
  and extracts a string-aware balanced JSON object, so a `content` field full of
  CSS `{ }` braces no longer truncates the parse. Also recovers a bare JSON
  object embedded after a leading sentence.

[0.11.1]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.11.1

## [0.11.0] - 2026-07-06

Reliability, small-open-weight-model support, and TUI UX overhaul. Grounded in a
full-codebase review plus field research on what opencode / vtcode / aider /
cline ship for weak models.

### Added
- **Native Ollama protocol** (`/api/chat`): detects the model's real context
  window via `/api/show` and sets `num_ctx`, eliminating Ollama's silent
  front-of-prompt truncation — the single biggest measured quality loss for
  local models. Sends `repeat_penalty: 1.0` and a low default temperature.
  `…/v1` base URLs stay on the OpenAI-compatible path for backward compatibility.
- **Live colored diffs** rendered in the TUI as files are written and edited
  (`+`/`-` hunks with add/remove counts), replacing opaque tool-call lines.
- **Markdown rendering on every output path**, including streaming — headings,
  bold/italic, inline code, lists, and fenced **code blocks** now render instead
  of showing raw markdown.
- **Auto mode-switching**: a build/plan request made in BRAINSTORM escalates to
  the appropriate mode instead of only chatting about it.
- **"Act, don't explain" nudge**: an imperative task answered with prose-only
  instructions is pushed to actually use its tools.
- **Tiered lenient edit apply**: whitespace/indentation-tolerant matching
  auto-rescues near-miss edits from weak models; similarity matches remain
  diagnostic-only.
- Configurable `temperature`, `max_tokens`, and `context_tokens` settings.
- CI now runs `cargo fmt --check` and `cargo audit`.

### Changed
- `Reply` carries a normalized `stop_reason` across both wire protocols;
  `max_tokens` truncation now triggers a bounded continuation instead of
  processing a truncated response.
- Retry policy is status-code-based and covers HTTP 529, honors `Retry-After`.
- Loop guard counts only repeated errors/no-ops (threshold 3), nudges once, then
  stops honestly — legitimate re-reads no longer end sessions.
- Compaction can no longer sever `tool_use`/`tool_result` pairs, always pins the
  original task, and preserves the recent tail; context-overflow errors
  force-compact and retry.
- Verifier is wired to real tool-call and changed-file data and feeds violations
  back to the model.
- System prompt restructured: role/mode contract first, deduplicated,
  game/artifact guidance gated on task type, standing rules dump removed.
- Compact tool set now applies to all local-context models.
- Command output truncation keeps head **and** tail so failing-test summaries and
  exit codes survive; `run_command`/`python_tool` gain a 120s timeout.
- Ask-mode auto-allow list trimmed from 44 commands to 15 read-only ones.
- Rewrote `SECURITY.md` (it described the removed Python/NEXUS package) and
  corrected `README.md` drift.

### Fixed
- **Truncated streamed tool-call JSON no longer executes with empty input** — it
  surfaces as an invalid-arguments error fed back to the model.
- **Removed the auto-repair hijacks** that overwrote correct model output with
  literal task substrings and rewrote shell commands into file writes.
- **Removed the hardcoded "fallback canvas game"** that shipped unrelated code as
  a successful result; artifact rejections now report the exact reason and retry.
- **Artifact validator** no longer rejects valid apps over `...`/`todo`
  substrings; rejections quote the offending snippet and the exact rule.
- **Queued-message TUI deadlock** on edit/remove/consume during streaming.
- **`/undo` no longer truncates** binary or oversized files.
- Display-width-aware wrapping (emoji/CJK), atomic session saves, hook-execution
  watchdog with distinct failure codes, and edit-mismatch diagnostics that point
  at the closest near-match.
- Read-only command classifier hardened against `;`/`|`/redirection smuggling,
  `sed -i`, `find -delete`, and `git clean`; mutating file tools are confined to
  the working directory.

[0.11.0]: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v0.11.0
