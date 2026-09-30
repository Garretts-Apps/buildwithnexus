# Upgrading to 0.15

0.15 is a minor release, so under [VERSIONING.md](VERSIONING.md) it may change
behavior. Every change is listed here with the setting that restores the old
behavior. Where there is none, the old behavior was a defect or lost data, and
the entry says why.

## Install and update

| Change | To restore the old behavior |
|---|---|
| The npm launcher downloads the binary to `~/.buildwithnexus/bin/<version>/` (or `$NEXUS_HOME/bin/<version>/` when `NEXUS_HOME` is absolute) instead of the package's `bin/`. | `BWN_INSTALL_IN_PACKAGE=1`. For a noexec home, set `NEXUS_HOME` to an absolute path elsewhere or point `BWN_BIN` at a binary. |
| `"auto_update": "install"` installs patch releases only; a new minor or major is announced with its install command. | `"auto_update": "install-any"` in `~/.buildwithnexus/settings.json`. |
| The Windows exe links the C runtime statically. | Nothing to restore: it also runs where the Visual C++ Redistributable is installed. |

## Network

| Change | To restore the old behavior |
|---|---|
| `HTTPS_PROXY`, `HTTP_PROXY` and `ALL_PROXY` are honored (0.14 always connected directly). | `NO_PROXY='*'`, or list the hosts to reach directly in `NO_PROXY`. Loopback model servers are always direct. |
| A proxy variable that is not an `http://` URL (for example `socks5://`) makes the requests that would use it fail with an explanation. | `NO_PROXY='*'`, or point `HTTPS_PROXY`/`HTTP_PROXY` at an `http://` proxy. |
| Certificates are checked against the OS store (or `SSL_CERT_FILE`/`SSL_CERT_DIR`) as well as the bundled roots. | `BWN_TLS_ROOTS=bundled`. |
| The web tools follow at most 5 redirects themselves and check each hop. | None needed: every redirect not aimed at a blocked address still succeeds. |

## Folder trust

| Change | To restore the old behavior |
|---|---|
| A project's `.buildwithnexus/system.md` applies only after you trust the folder, and is appended to your own `system.md`. | `"project_system_prompt": "replace"` in `~/.buildwithnexus/settings.json` lets a trusted project's file replace yours. The trust requirement has no opt-out. |
| A project skill with the same name as a bundled or user skill loads as `/project:<name>`. A project `skill_dirs` entry counts as the project's own, even as an absolute path. | `"project_skills_override": true` in your own settings (ignored in project settings), or add the directory to `~/.buildwithnexus/settings.json`. |
| Folders trusted before 0.15 are asked about again once if their hooks or MCP servers run repo files, call npm/make/just (in any subfolder), name a file by a bare word, or if the folder has a `.buildwithnexus/system.md`. | None: the trust record now covers those files. Answering the prompt once re-trusts the folder. |

## Permission gate

| Change | To restore the old behavior |
|---|---|
| Windows-form sensitive paths (backslashes, drive letters, UNC, 8.3 names, wildcards, `-Path:value`) prompt. | None: skipping the prompt was a defect. Answer `s` or `a` for a specific command. |
| Windows destructive commands (see the CHANGELOG) and `rm -rf` on a drive path prompt even in auto mode. Some ordinary uses such as `rd /s /q build` or `bcdedit /enum` prompt too. | None: they are gated like `rm -rf /`. Answer `y` to let one through. |

## Headless runs and output

| Change | To restore the old behavior |
|---|---|
| Runs that stop short exit 4 (hook blocked), 5 (budget), 6 (step limit), 7 (`check_work` failed) or 8 (verifier blocked) instead of 0. | `--legacy-exit-codes` or `BWN_LEGACY_EXIT_CODES=1`. Only the exit code changes back; the summary line still names the outcome. |
| `--json` output ends with a `{"type":"result",…}` event, and every event carries `"schema_version": 1`. | None needed (additive). A reader that rejects unknown keys or expects a specific last event type should skip `result` and ignore `schema_version`. |
| Session files are saved with `"schema_version": 1`. | None needed: 0.14 ignores the field, and 0.15 reads older files as version 1. |

## Models and agents

| Change | To restore the old behavior |
|---|---|
| A same-provider `/model` swap keeps the saved `base_url`. | Remove `base_url` from `config.json` to go back to the preset default. (`/model <url>` switches to the custom provider, so it does not restore a preset.) |
| The OpenRouter preset has a current default model. | Set `"model"` in `config.json`, or pick one with `/model`. Only configs without an explicit model are affected. |
| When onboarding finds no local models it suggests pulling the Ollama preset's default model. | Type another model name at the prompt. |
| Compaction keeps earlier images while they fit the context window (at most 4). | None: the old behavior silently lost them. `/clear` still drops everything. |
| An isolated subagent that changed files leaves a `bwn-sub-<pid>-<n>` branch, named in its result. | Delete the branch with `git branch -D <branch>`. Branches with no new commits are still removed. |

## Release process (maintainers)

- `publish.yml` has no `version_bump` input; a dispatch with no inputs
  re-publishes the current version. Bump versions in a PR to main.
- GitHub releases stay drafts until every asset is attested and uploaded; the
  tag is created when the draft is published.
- `publish.yml` installs the exact npm tarball in clean containers before
  publishing it, and stops on any failure.
