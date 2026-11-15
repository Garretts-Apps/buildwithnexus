# Performance review of buildwithnexus 0.21.0

Reviewed by measuring the machine first, then talking about the code the
measurements point at. Principles applied: performance
aware programming, non-pessimization, semantic compression, the immediate-mode
frame, a short dependency list, and a debug surface you can read while the
program runs.

Machine: Linux 6.12, 4 cores, `rustc 1.100.0`, release profile as shipped
(`opt-level = 3`, `lto = true`, `codegen-units = 1`, `strip = true`).
Binary: `target/release/buildwithnexus`, 6,442,992 bytes. Numbers below are
from that binary on this machine. They are not the 2026-11-13 figures in
`BENCHMARKS.md` (that box reported `--version` p50 1.2 ms).

The program is already closer to a small, direct program than almost any
Rust agent CLI. Eleven direct crates (`crossterm`, `ureq`, `rustls`,
`webpki-roots`, `rustls-native-certs`, `url`, `ring`, `serde` / `serde_json`,
`shlex`, `libc`). No tokio, no ratatui, no regex in the normal tree (regex
1.12 is pulled by the criterion dev-dependency only). The tool table is a
`match`, and `harness/src/tools.rs` says so on line 2. The transcript wraps a
line once (`Transcript::append_to`, `harness/src/tui.rs` 244). Output rows
are repainted only when their bytes differ (`build_output_frame`,
`harness/src/tui.rs` 2217). Frames are coalesced at 16 ms
(`FRAME_MS`, `harness/src/tui.rs` 2409). Terminal size is cached in two
atomics (`term_size`, `harness/src/tui.rs` 1148). That work should stay.

## Ranked findings

### 1. A keystroke allocates 56 times. The paint is still 22 µs.

After the prompt was up (`describe a task` visible, 100×30 PTY, `NO_COLOR=1`,
local provider pointed at a closed port), one `a` key:

| | value |
|---|---|
| malloc/calloc/realloc calls | **56** (3,112 → 3,168) |
| bytes requested | **5,355** (920,234 → 925,589) |
| time until the PTY echoed bytes | **p50 22 µs, p95 57 µs** (25 keys, min 21, max 281) |

Counter: `LD_PRELOAD` of a `malloc`/`calloc`/`realloc` interposer, sampled
with `SIGUSR1` before and after the key. The interposer counts requests, not
what the allocator reused from its cache.

The composer paint builds a new footer string on every key even when the
footer did not change. `render_composer` (`harness/src/tui.rs` 2758) calls
`queue_chrome` (1928), which always calls `queue_footer` (1946). That
function `clone`s the footer and the model (1953), then `format!`s the badge,
the context bar, the vim label and the scroll marker (1963–2074), then
`strip_ansi`s the concatenation to see if it fits (2052). `composer_glyphs`
(984) allocates another `String` for the input row. The bytes are then
compared with `painted_footer` / `painted_composer` and usually not written.
The compare skips the `write`. It does not skip the allocations.

56 allocations at 22 µs is not a frame-budget emergency. It is work the key
did not need. Cache the footer bytes against the inputs that change them
(width, model, permission text, context counts, scroll, vim state, flash),
and write the composer glyphs into a reused buffer.

Idle for 1.0 s after that key allocated **nothing** (the counter stayed at
3,168). The idle path is not allocating.

### 2. Idle CPU is 0. The ticker still wakes 64 times a second.

Same PTY, prompt up, agent not running, 2.000 s of `/proc/<pid>/stat`
`utime+stime`:

| | value |
|---|---|
| CPU | **0.00%** (0 ticks, `CLK_TCK` 100, 4 threads, VmRSS 6,716 KiB) |
| syscalls in 1.5 s, `strace -c -f -p` | **98**: 96 `clock_nanosleep`, 2 `restart_syscall` |

`start_typeahead_thread` (`harness/src/tui.rs` 3707) loops `sleep(15ms)` for
the life of the process. While the agent is stopped the body only calls
`paint_if_due`, which loads one atomic and returns (`paint_if_due`, 2441).
The sleeps show up in `strace` and do not show up in CPU time: a 15 ms sleep
is under one jiffy of work, so a 2 s sample of 0 ticks is the honest CPU
number, and 96 sleeps in 1.5 s is the honest wakeup number
(1.5 / 0.015 = 100; the attach itself ate a few).

Park that thread on a condvar. Wake it when a frame is requested or the agent
starts. A pending frame waits until `FRAME_MS`. Idle waits forever: no
`clock_nanosleep`. The 0% CPU figure will not move. The wakeup count will.

### 3. The release link is the build. The dependency list is not.

`cargo +stable build --release --locked --timings -p buildwithnexus`, clean
(`Fresh units: 0`, `Dirty units: 129`):

| | value |
|---|---|
| wall time | **82.994 s** (`Finished release profile in 1m 22s`) |
| `buildwithnexus` lib | **38.1 s** (frontend 9.0 s, codegen 29.1 s) |
| `bwn` bin link | **34.4 s** |
| `buildwithnexus` bin link | **34.2 s** |
| `rustls` 0.23.46 | 5.6 s |
| `ring` build-script run | 3.7 s |
| `ring` 0.17.14 | 1.6 s |
| `ureq` 3.5.2 | 1.8 s |
| icu_* + idna + idna_adapter, summed | **3.6 s** |

The two binaries are the same `fn main() { buildwithnexus::run(); }`
(`harness/src/main.rs` 3, `harness/src/bwn.rs` 3) and the link produces one
inode (both names are 6,442,992 bytes, link count 2). LTO with
`codegen-units = 1` still typechecks and links the image twice. That is the
dominant compile cost, and it is the profile the project chose so the
shipping binary is fast. It is not an accident and it is not a crate you can
delete without deciding you want a slower binary or a longer edit-compile
loop. `cargo check` is the loop that should stay quick; this 83 s number is
the release profile only.

Normal dependency tree (`cargo tree -e normal -p buildwithnexus --prefix none`,
names deduped): **88 packages**. Direct edges at depth 1: the crates listed
at the top plus `crossterm`'s `mio`, `signal-hook` and `parking_lot`, which
the `events` feature requires (`crossterm` 0.29 `Cargo.toml`: `events` depends
on `mio`, `signal-hook`, `signal-hook-mio`). `poll` / `read` need that
feature. Dropping it means replacing the input crate, not flipping a flag.

`url` 2.5 always depends on `idna` with `compiled_data`. That is the ICU
stack. It costs 3.6 s here, not tens of seconds. International host names are
how MCP OAuth and redirects decide what is a safe host
(`harness/src/mcp_auth.rs`, `harness/src/net.rs`). Replacing `url` to delete
ICU would be a security-sensitive parser, and the compile-time win on this
machine is a few seconds against an 83 s LTO.

Section sizes of the stripped release binary (`size -A`):

| section | bytes |
|---|---:|
| `.text` | 5,117,894 |
| `.rodata` | 721,509 |
| `.eh_frame` | 284,444 |
| `.rela.dyn` | 140,712 |
| `.data.rel.ro` | 107,960 |
| total of all sections | 6,439,376 |

Symbols are stripped, so `cargo bloat` / `llvm-lines` cannot attribute the
shipping image. `.text` is 79% of the file. `.rodata` at 721 KB is real but
it is not a multi-megabyte ICU blob.

`--version`, 5 warmups then 30 runs, parent `perf_counter` around
`subprocess.run`: **p50 0.659 ms, p95 0.941 ms** (min 0.597, max 0.981).
`strace -c` on one `--version`: **78 syscalls**, 2 errors (`access`, `fcntl`).
That is the whole process, spawn to exit.

### 4. The frame loop is already immediate-mode. One path still rebuilds the footer while the agent runs.

`build_output_frame` keeps `painted` per row and skips the write when the
padded line matches (`harness/src/tui.rs` 2297). It still *builds* the padded
line for every visible row before that compare (2268–2298), and
`rows_range` (312) plus `total_rows` (306) walk the wrap vectors from the
start of the transcript on every frame. I did not time a streamed frame in
this pass, so this is not ranked on a microsecond. The code is the kind of
work a long transcript repeats 60 times a second during a stream. A row
generation (or the source line's pointer and length, invalidated on
`append_to`) would skip the pad. Worth doing when a stream benchmark says
the walk shows up. Not worth a rewrite on a hunch.

`poll_typeahead` (`harness/src/tui.rs` 445) calls `render_queued_composer`
on every tick while the agent is running, whether or not a key was read.
`render_queued_composer` clones the typeahead buffer and the queue (1049)
and goes through `queue_footer` again. Same pattern as finding 1, on the
15 ms tick instead of the key. Not timed in this pass: the idle measurement
above is with the agent stopped, and that branch is inside
`is_agent_running` (`harness/src/tui.rs` 3712).

### 5. There is no live frame counter.

`trace::record` (`harness/src/trace.rs` 48) keeps 500 events and appends each
one to `traces/<session>.jsonl` immediately (`persist`, 189). `/trace` prints
them. Nothing on screen shows frame time, input-to-paint, allocations, or
whether the ticker is sleeping. A game-engine debug overlay is how you
notice finding 1 the next time someone adds a `format!` to the footer.
The overlay has to be off by default: one atomic load on the frame path,
no allocation and no `/proc` read while it is hidden. Turning it on is the
point of the following change, not of this note.

`/debug` today is an alias of `/doctor` (`harness/src/tui/editor.rs` 510,
`harness/src/repl.rs` 535). A panel that takes that name has to leave
`/doctor` where it is.

## What this review is not asking for

- A trait or framework around the screen. `Screen` is one struct
  (`harness/src/tui/screen.rs`). The free functions are the immediate-mode
  API. Folding them into methods would not make a frame faster.
- Deleting `serde` derives. They are how config, sessions and the wire format
  are decoded. The compile cost that showed up is `serde_derive` at 1.5 s,
  behind LTO and rustls.
- An arena for the whole process. The hot allocation is a handful of short
  `String`s per key, which a reused buffer fixes. A general arena is a new
  abstraction in front of a problem that does not have one yet.
- Splitting `config.rs` (5,560 lines) or `tui.rs` (about 5,000) for style.
  The lines are the behavior. Split a file when a change has to be reviewed,
  not because a style guide is nervous.

## Reproduce

```bash
# release build + section sizes + --version
cargo build --release --locked --timings -p buildwithnexus
size -A target/release/buildwithnexus
# 5 warmups, then 30 runs, time the child from the parent

# idle CPU, idle syscalls, key-to-bytes, allocs per key
# PTY 30x100, NEXUS_HOME with a local provider, NO_COLOR=1,
# wait until the output contains "describe a task"
# CPU: /proc/<pid>/stat utime+stime over 2s, CLK_TCK from sysconf
# syscalls: strace -c -f -p <pid> for 1.5s
# allocs: LD_PRELOAD a malloc/calloc/realloc counter, SIGUSR1 before and after one key
```
