# Competitor speed benchmarks

Startup, memory, download size and (where a tool can be pointed at one) a
canned end-to-end task, for bwn and the other terminal coding agents that
install headlessly on Linux.

The numbers in `results/` are the output of `python3 bench/run.py` on the
machine described in that run's `hardware.json`. Nothing in those files is
estimated. If a tool cannot be measured, the JSON says why and leaves the
number out.

## What is timed

| Metric | How | Runs |
|---|---|---|
| `--version` and `--help`, warm | `hyperfine --shell=none --warmup 3 --runs 20` | 20 |
| `--version` and `--help`, cold | the same, with `--prepare` running `sync` and `echo 3 > /proc/sys/vm/drop_caches` before every run | 20 |
| Peak RSS of `--version` | `/usr/bin/time -v`, median of `Maximum resident set size (kbytes)` | 5 |
| Download size | the bytes of each npm tarball, the GitHub release asset, or the pip wheels this run actually downloaded | |
| Install size | byte size of the installed tree | |
| Package size | the published package artifact (npm tarball, release binary, or the aider wheel alone) | |
| Binary size | the native executable the command runs. A Python or JavaScript entry script is recorded as a script and is not ranked against native binaries | |
| Ready prompt | a PTY, only when `pins.json` has a transcript pattern that this tool actually prints | 5 |
| Fixed task | a local OpenAI-compatible mock (`mock_openai.py`) that replies `BENCHMARK_OK` | 5 |

Hyperfine's mean, median and standard deviation are copied into `summary.json`.
Warm runs share one empty `HOME` per tool, primed by a single untimed
`--version`, so the timed runs are process start rather than first-time
config creation. Cold runs drop the page cache. `--shell=none` keeps the
shell's own startup out of the number.

Two bwn rows are measured on purpose:

- **bwn native** is the release binary, invoked directly. That is what
  `cargo install` and the native install scripts run.
- **bwn (npm)** is the `bwn` command from `npm install -g buildwithnexus`.
  The first run copies the release binary and points that command at the
  copy. Later runs start the copy and do not start Node. `--version` is
  timed after that copy is in place. The download itself is in the download
  size, not in the startup time. The run `2026-11-15T16-29-28Z` measured the
  launcher before this handoff (registry `buildwithnexus@0.21.0`, warm
  `--version` median 21.236 ms). `2026-11-15T17-29-49Z` re-measured the npm
  command from this checkout with `python3 bench/run.py --npm-handoff`.
  `hardware.json` in that directory records `git_dirty: true` because the
  pack included `scripts/native-handoff.js`, which `package.json` `files`
  did not list until this change. The registry tarball still does not.

GNU time reports the top-level process. For a Node launcher that is the Node
process, not a child binary.

## The mock task

`mock_openai.py` answers `/v1/chat/completions` and `/v1/responses`, streaming
or not, with the same text every time. There is no model. A published task
time exists only when the process exited 0 and its output contained
`BENCHMARK_OK`.

- bwn uses `--provider custom --base-url` (chat completions).
- Aider uses `--openai-api-base`. The model name is one Aider already
  accepts; the completion bytes come from the mock.
- Codex CLI 0.169 rejects `wire_api = "chat"`. It is pointed at the mock's
  Responses API (`wire_api = "responses"`), which is the OpenAI endpoint that
  version can use. `requires_openai_auth` is false so it does not try to log
  in to ChatGPT.
- OpenCode is pointed at the mock with `provider.options.baseURL` and
  `OPENAI_BASE_URL`. Catalog fetch and auto-update are off so the run cannot
  leave the mock.
- Claude Code speaks the Anthropic Messages API. Gemini CLI's base URL is
  the Gemini API. Neither is given a task time.

Approval flags (`--permission-mode readonly`, Aider `--yes-always`, Codex
`-a never`) are there so the run cannot stop on a question. They are named
in `summary.json`.

Ready-prompt patterns in `pins.json` come from each tool's own PTY transcript:

- bwn: the footer text `permission:`, after a settings file has selected a
  provider (so setup does not run) and with auto-update off.
- Codex: `Ask Codex to do anything`, after `CODEX_HOME` points at a local
  Responses provider so the sign-in screen is not the first screen.
- OpenCode: `Ask anything`.
- Aider: the `>` input line, with the release-notes and analytics questions
  turned off and `--openai-api-base` set to a closed local port so it does
  not wait on a browser login.
- Claude Code's first screen is a theme picker. Gemini CLI's first screen
  waits on authentication. Those two are not given a prompt time.

## Reproduce

Linux, with passwordless `sudo` (the cache drop needs it), Node.js 18 or
later, npm, Python 3.11 or later, `python3-venv`, curl, git, hyperfine, and
GNU time (`/usr/bin/time`, Debian package `time`).

```bash
sudo apt-get install -y hyperfine time python3-venv python3-pip
python3 bench/run.py
```

That installs the versions in `pins.json` under `bench/.tools/` (gitignored),
then writes `bench/results/<UTC timestamp>/`:

| File | Contents |
|---|---|
| `hardware.json` | CPU, RAM, OS, kernel, date, tool versions used to measure |
| `versions.json` | each CLI's own `--version` text |
| `summary.json` | every published figure, plus rankings |
| `summary.md` | the same figures, generated from `summary.json` |
| `startup.json`, `memory.json`, `sizes.json`, `prompt.json`, `task.json` | per-metric detail |
| `raw/startup-*.json` | hyperfine's own JSON, including every run |

A partial rerun for debugging is `python3 bench/run.py --only startup,memory --results /tmp/bench-debug`. Partial runs are not a published result (`complete` is false).

GitHub Actions (`.github/workflows/bench.yml`) runs this on `ubuntu-latest`
when dispatched, and every Monday at 06:00 UTC, and uploads `bench/results`
as an artifact. It does not commit. The committed directory is one specific
machine, named in its `hardware.json`.

Pinned versions change only by editing `pins.json` and rerunning. The runner
exits if a binary's `--version` text does not contain its pin.

## Security and engineering benchmarks

These follow `docs/roadmap.md` (items 1.13 and 6.1) and the design in
section 9 of `docs/competitor-analysis.md`.

- `python3 bench/invariants.py` runs every `invariant_*` test once per
  sandbox backend this machine has and writes `invariants.json` and
  `invariants.md`: passed/total per acceptance case (AU-1, IN-2, ...) and
  backend. That is the "unauthorised actions blocked" metric, for bwn.
- `python3 bench/longitudinal/select_tasks.py <repo>` scans a repository's
  first-parent history for 20-change windows that suit the longitudinal
  benchmark: every change carries tests, at least half build on files an
  earlier change in the window touched, and none is a release-sized merge.
  It ranks the windows; writing the requirements and running the harnesses
  are later steps (roadmap 6.1).
