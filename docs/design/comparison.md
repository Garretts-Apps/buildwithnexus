# bwn, T3 Code, Claude Code, Codex CLI

Facts only. A cell is either a number from a named run or a sentence
with a link. Nothing here is estimated, and nothing here is a palette.

Two startup runs are on this host (Ubuntu 24.04.5 LTS, kernel
6.12.113+, 4 CPUs, MemTotal 16,780,623,872 bytes):

- **Bench** `bench/results/2026-11-15T16-29-28Z`. Hyperfine
  `--shell=none`, 3 warmup runs then 20 measured runs for warm;
  20 cold runs each prepared with `sync` and
  `echo 3 > /proc/sys/vm/drop_caches`. Medians below are hyperfine's.
  Peak RSS is `/usr/bin/time -v` on `--version`, 5 runs, median of
  `Maximum resident set size (kbytes)`, top-level process only.
  Versions: bwn native `buildwithnexus` 0.21.0 (that run's binary,
  6,492,376 bytes), Claude Code 2.1.325, Codex CLI 0.169.0.
- **This session**, 15 Nov 2026, same hyperfine and GNU time settings.
  bwn is `target/release/buildwithnexus` at 6,718,136 bytes (the Notch
  build). T3 Code is the official CLI 0.1.9: npm package `t3@0.1.9`
  downloads `t3-0.1.9-linux-x64.tar.gz` and execs the ELF inside it.
  The ELF measured here is `/tmp/t3-home/runtime/versions/0.1.9/t3`.
  Claude Code and Codex were not installed again.

T3 Code's product is the desktop, web, and phone app at
[t3.codes](https://t3.codes/) and
[pingdotgg/t3code](https://github.com/pingdotgg/t3code). The CLI is
what can be timed the way `bench/` times a terminal command. The
desktop `.deb` was not installed.

| | bwn | T3 Code CLI 0.1.9 | Claude Code 2.1.325 | Codex CLI 0.169.0 |
|---|---|---|---|---|
| `--version` warm median | Bench: 0.557 ms. This session: 0.853 ms | This session: 571.404 ms | Bench: 6.472 ms | Bench: 28.316 ms |
| `--version` cold median | Bench: 4.872 ms. This session: 3.430 ms | This session: 688.155 ms | Bench: 42.675 ms | Bench: 54.505 ms |
| `--version` peak RSS, median | Bench: 3,456 KiB. This session: 3,636 KiB | This session: 271,520 KiB | Bench: 22,984 KiB | Bench: 52,884 KiB |
| Idle process | This session, TUI hidden, 2 s: 0 ticks, VmRSS 7,260 kB. Panel open, 2 s: 0 ticks, VmRSS 7,096 kB. Not a hyperfine run | This session, `t3 serve --no-browser --host 127.0.0.1 --port 3777`, no thread. About 5 s after start: server RSS 433,660 KiB, resource-monitor child 2,644 KiB. One sample | Not measured | Not measured |
| Install size | Bench binary and install tree: 6,492,376 bytes. This session's binary: 6,718,136 bytes | ELF 169,385,088 bytes. Tree after the first `--version` (`du -sb` of `T3CODE_HOME`): 448,699,361 bytes. After `serve` created its database: 450,361,413 bytes. npm wrapper prefix: 9,635 bytes. Desktop `T3-Code-0.1.9-amd64.deb` is 175,486,892 bytes on the [v0.1.9 release](https://github.com/pingdotgg/t3code/releases/tag/v0.1.9); that package was not installed | Bench install tree: 731,894,792 bytes. Download 169,382,845 bytes. Binary 365,842,960 bytes | Bench install tree: 422,680,010 bytes. Download 167,006,502 bytes. Binary 294,277,432 bytes |
| Download that produced the timed program | The binary itself, 6,492,376 bytes in the bench; 6,718,136 bytes this session | GitHub asset `t3-0.1.9-linux-x64.tar.gz`, 149,119,656 bytes (release listing). npm tarball `t3-0.1.9.tgz`: 3,784 bytes. The launcher's warm `--version` median, after the ELF was already on disk, was 599.407 ms | Bench download column: 169,382,845 bytes | Bench download column: 167,006,502 bytes |
| Platforms | This repository's CI runs the terminal on Linux, macOS, and Windows | Documented. README and [install.md](https://github.com/pingdotgg/t3code/blob/main/docs/user/install.md): desktop macOS, Windows, Linux; phone apps; web. The CLI launcher's supported keys are `darwin-arm64`, `linux-arm64`, `linux-x64`, `win32-arm64`, `win32-x64`. install.md says there is no `t3` executable for Intel Macs. The Linux ELF's own error text says musl (Alpine) is not supported | Bench: the npm CLI on Linux. The memory doc lists macOS, Linux/WSL, and Windows paths for `CLAUDE.md`, so those are documented hosts ([Claude Code memory](https://docs.anthropic.com/en/docs/claude-code/memory)) | Bench: the npm CLI on Linux. Other hosts were not re-surveyed |
| Diff and the project tree | In the terminal. F2 tree, F3 diff. At 120 columns and wider: tree, log, diff. Under 120: one column. `v` is side-by-side when the diff column is wide enough. `g` switches session baseline and git HEAD | Documented GUI, not timed. A diff pane, a commit list (newest 100 in the branch comparison), and a worktree per thread. A turn's file list omits files that arrived only from a pull, merge, or rebase ([source control](https://github.com/pingdotgg/t3code/blob/main/docs/user/source-control.md)). Screenshot, linked only: [hero.webp](https://github.com/pingdotgg/t3code/blob/main/.github/readme/hero.webp) | Not a column of this bench. The terminal session compared in [anti-patterns.md](anti-patterns.md) is a transcript and a composer, not a tree beside a diff | Not a column of this bench. The terminal session compared there is a startup plate, not a tree beside a diff |
| Debug visibility | F12 cycles strip, panel, fullscreen. Hidden does not record. `/debug` and `bwn debug export` | The CLI `--help` lists `--log-level` with a `debug` choice, and `t3 serve` is the server. A live panel comparable to F12 was not in the user docs fetched for this note. The README says a dropped frame is treated as a bug ([README](https://github.com/pingdotgg/t3code/blob/main/README.md)) | Not measured | Not measured |
| Claude and Codex instruction files | bwn reads them itself. Default names are `AGENTS.md`, then `CLAUDE.md`, from the git root down to the working directory, plus `~/.buildwithnexus/AGENTS.md`. Cap 32 KiB a file and 96 KiB across files (`harness/src/config.rs`) | The provider CLI is a subprocess ([AGENTS.md](https://github.com/pingdotgg/t3code/blob/main/AGENTS.md)), so that CLI's own loader is what the model sees. Commit and pull-request text can also use "Repository conventions," which the source-control doc says uses the project's instructions. No separate size cap was found in the docs fetched here | Documented by Anthropic: Claude reads `CLAUDE.md` at the start of every session, and can read a repository's `AGENTS.md` in place of `CLAUDE.md` ([memory](https://docs.anthropic.com/en/docs/claude-code/memory)). Not timed here | Documented by OpenAI: Codex CLI reads `AGENTS.md` from `~/.codex` and from each directory from the repository root to the working directory, with a size cap mentioned but not quoted as a number here ([Codex prompting guide](https://developers.openai.com/cookbook/examples/gpt-5/codex_prompting_guide)). Not timed here |

The bench run's other tools (Gemini CLI, OpenCode, Aider) are in
`bench/results/2026-11-15T16-29-28Z/summary.md`. They are omitted here
because this table is the four named above.

How the screen differs from T3 Code, without using these numbers as a
visual brief, is in [direction.md](direction.md) and
[anti-patterns.md](anti-patterns.md).
