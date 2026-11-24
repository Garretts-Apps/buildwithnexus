# Benchmarks

Every performance claim this project makes, with the command that regenerates
it. Measured 2026-11-13 (v0.20.0) on a deliberately modest machine: a 4-core
Intel Xeon @ 2.30GHz cloud container (Linux 6.18, x86_64), not a tuned
workstation, against the release profile (`lto = true`, `codegen-units = 1`,
stripped). Your laptop is probably faster. Criterion figures are medians.

## Whole-binary numbers

| Metric | Result | How |
|---|---|---|
| Binary size | 6.43 MB (6,426,352 bytes) | `cargo build --release`, `ls -l` |
| Startup of the native binary, `--version`, full process lifecycle | **p50 1.2 ms · p95 1.3 ms** | 30 timed runs of `buildwithnexus --version` after 3 warmups (spawn → exit, measured from the parent). Three repeats of the 30 runs gave p50 1.18–1.19 ms |
| Startup through the npm launcher, `bwn --version` | **p50 27 ms · p95 29 ms** | the same 30 runs of `node bin/buildwithnexus.js --version` with `BWN_BIN` set to the release binary (three repeats: p50 26.0–27.0 ms); the `node` process alone (`node -e 0`, Node 22.23) takes p50 20 ms of that |
| TUI resident memory, idle | **9.0 MiB** (median of 5 runs, 8.9–9.1) | launch the alternate-screen TUI in a PTY (120×40, an empty project folder, a configured local provider that is not running), settle 4 s, read `VmRSS` from `/proc/<pid>/status` |

The 1.2 ms figure is the native binary running `--version`. **If you installed
with npm, you do not get it**: `bwn` is then a Node launcher, and Node's own
start-up is about 20 ms here, plus the launcher's module loading and the spawn
of the binary, about 27 ms in all for `bwn --version`. A cargo-installed `bwn`
has no Node in front of it.

Not measured here: time from launch to the first interactive prompt. It
depends on the terminal and the configured provider, and there is no
repeatable benchmark for it in the repository yet.

## What a keystroke, a start and a streamed reply write

Counted with `strace` on the release build in a 100×30 PTY against a scripted
streaming model, 0.19.0 against 0.20.0:

| | 0.19.0 | 0.20.0 |
|---|---|---|
| Opens of `/dev/tty` per typed character (window-size checks) | 10 | 0 |
| Bytes written per typed character | 1,146 | 234 |
| Writes to the terminal during start-up | 128 (65 KB) | 23 (15 KB) |
| Bytes written per streamed chunk (200-chunk reply, footer ticking) | 4,457 | 807 |
| Opens of `config.json` during start-up | 19 | 1 |

`scripts/test-terminal-input.py` holds these in place: it fails if typing
opens `/dev/tty`, if a burst of lines paints more than a few frames, or if a
streamed chunk repaints more than the rows that changed.

## The rendering claim

The claim on the site (streaming renders got ~215× faster in 0.12.0,
966 µs → 4.5 µs per streamed chunk) comes from the incremental wrap cache:
a streamed chunk re-wraps **one** transcript line instead of all of them.
`cargo bench` regenerates both sides on a 2,000-line transcript:

| `transcript_append` | Time per chunk |
|---|---|
| `incremental/2000lines` (what ships) | **2.83 µs** |
| `full_rewrap/2000lines` (pre-0.12.0 behavior) | **2.92 ms** |

That is about 1,000× at this transcript length. The shipped 215× figure was
measured on a shorter transcript, and the ratio grows with session length
because the old cost was O(total lines) per chunk. The incremental case is
measured as append-plus-reset (two single-line rewraps), so it *overstates*
the real per-chunk cost.

Since 0.20.0 the cost of a chunk also no longer grows with the length of the
line being streamed. `stream_wrap` streams one unbroken paragraph in
token-sized chunks:

| `stream_wrap` | Whole line | Per chunk |
|---|---|---|
| `chunks_into_200char_line` (33 chunks) | 1.97 µs | 0.06 µs |
| `chunks_into_2000char_line` (333 chunks) | 17.2 µs | 0.05 µs |
| `chunks_into_20000char_line` (3,333 chunks) | 171 µs | 0.05 µs |

### Work that scaled with the session, or ran per character

Measured 2026-11-24 on a 4-core Intel Xeon @ 2.10GHz cloud container,
before and after the same change, back to back on the same machine (so the
"before" column is not comparable with the tables above, which ran on a
different one). Criterion medians, `cargo bench --bench perf --
"transcript_cap|wrap_line|transcript_append|stream_wrap"`.

| Benchmark | Before | After | |
|---|---|---|---|
| `transcript_cap/frame_window_40rows_of_2000lines`: a frame finds the rows at the bottom | 1.69 µs | 0.085 µs | 20× |
| `transcript_cap/push_line_at_2000`: a finished line at the 2,000-line cap | 3.51 µs | 1.16 µs | 3.0× |
| `wrap_line/plain_1300chars` | 11.7 µs | 7.43 µs | 1.6× |
| `wrap_line/coloured_1300chars` | 26.9 µs | 18.4 µs | 1.5× |
| `transcript_append/incremental/2000lines` | 3.61 µs | 2.43 µs | 1.5× |
| `stream_wrap/chunks_into_2000char_line` | 22.3 µs | 14.9 µs | 1.5× |

What changed: the transcript is kept in rings, so the line cap drops the
oldest line without moving the rest; the row total is counted as lines
change, and the bottom of the view is found from the end, so a frame costs
the lines on screen rather than the lines kept; ASCII skips the
character-width tables; a run of printable ASCII is copied onto a row in one
go; and a wrap point copies the colour state only when it changes, not at
every space.

### A long reply: the terminal scrolls, bwn sends the new rows

Once the output rows are full, every new line used to change every row (the
view moved up by one), so each frame sent the whole output area again. Now
bwn tells the terminal to scroll the output region (a line feed on its
bottom margin, as on a VT100) and paints only the rows that arrived; Sixel
images still wholly on screen move with the scroll and are not sent again.

Measured with `scripts/test-terminal-input.py` (`ScrollTests`): a scripted
model streams a 61-line reply into a 100×30 PTY, one line per chunk, debug
builds of main and of this change, three runs each (identical):

| | Before | After |
|---|---|---|
| Bytes written for the whole reply's output frames | 137,581 | 9,805 (14× less) |
| A frame after the screen is full: median bytes | 2,773 | 113 (25× less) |
| A frame after the screen is full: median rows painted | 26 | 1 |

## Tools

A generated project of 2,000 source files (8.8 MB); a 60 MB log for
`read_file`.

| Bench | Time | Notes |
|---|---|---|
| `grep_files/zero/insensitive` | 6.7 ms | no match, the default case-insensitive search |
| `grep_files/few/insensitive` | 6.4 ms | |
| `grep_files/many/insensitive` | 9.7 ms | |
| `read_file/small` | 1.7 µs | |
| `read_file/60mb/lines_100_200` | 4.9 ms | a line range near the start |
| `read_file/60mb/whole` | 28 ms | no range: the file is checked to its end, about 100 KB is returned |
| `tool_lookup/read` … `tool_lookup/mcp__server__tool` | 4.7–45 ns | finding a tool's row in the table |

For scale, `grep -rniF` over the same tree took 10–23 ms on this machine,
process start included; bwn's search runs in-process.

One reply's read-only calls, one at a time (`width1`) and side by side
(`width4`, the default). File reads run in line at either width, because
starting threads costs more than reading:

| `tool_batch` | `width1` | `width4` |
|---|---|---|
| `read6` (six small files) | 10.9 µs | 11.4 µs |
| `grep4` (four searches) | 26.4 ms | 10.8 ms |
| `fetch4_simulated_100ms` (four fetches, local servers that wait 100 ms) | 403 ms | 101 ms |

The fetch case is simulated latency, not a real network.

## Fixed costs per command, search, request and frame

Measured 2026-11-24 on a 4-core Intel Xeon @ 2.10GHz cloud container. Each
column is the same benchmark code, release profile, back to back: "Before"
is this change's benchmarks on the code it started from, "After" is the
change. Criterion medians. Every row below passed a gate that needs a gain
of at least 5% with no overlap between the two runs' samples.

| Benchmark | Before | After | |
|---|---|---|---|
| `run_command/true`: the fixed cost of every shell command | 51.6 ms | 3.17 ms | 16× |
| `grep_files/zero_insensitive` (2,000 files, no match) | 9.67 ms | 4.56 ms | 2.1× |
| `grep_files/many_insensitive` | 13.0 ms | 5.61 ms | 2.3× |
| `grep_files/gitignored_tree` (a 26-rule .gitignore) | 20.4 ms | 10.2 ms | 2.0× |
| `read_file/60mb_whole` | 35.2 ms | 30.0 ms | 1.17× |
| `read_file/60mb_last_lines` | 34.3 ms | 29.8 ms | 1.15× |
| `token_estimate/4.6MB_session`: the context estimate before each request | 5.5 µs | 0.06 µs | 90× |
| `trust/gate_10_hook_refs`: a gated tool call's project-trust check | 567 µs | 302 µs | 1.9× |
| `theme/paint_1000_spans`: 1,000 calls through the colour helpers | 1.20 ms | 28.3 µs | 42× |
| `highlight_colour/rust_271lines_fenced`: a finished fence as drawn | 2.78 ms | 0.40 ms | 7.0× |
| `stream_md/fence_chunks`: a fence streamed in 4-byte chunks | 11.0 ms | 5.11 ms | 2.2× |

All seven `grep_files` rows passed (47–57% less time). `read_file/small` and
`read_file/60mb_lines_100_200` did not move (1% and 4%).

What changed:

- The command runner waits on the child's output channel, not in 50 ms
  sleeps.
- A search checks each folder against the sensitive-path rules once, not
  once per file. It searches files on up to 8 threads and merges the results
  in path order, so output is the same. The .gitignore patterns are split
  once, and names are matched without allocating.
- `read_file` counts newlines 32 bytes at a time.
- The context estimate keeps each message's count across rounds and counts
  only what the round added.
- A permission decision works out the project's trust once, not once per
  hook reference.
- Whether to use colour is decided once, and each theme's escapes come from
  a table, not the environment read on every span.

The sandbox no longer leaks a file descriptor per command: the parent closes
its copy of the ruleset after spawning. That is a fix, not a speed claim.

Tried and not kept, because they missed the gate:

| Change | Result |
|---|---|
| one read buffer for the live stream reader | 31–41% faster with 256-byte reads in two runs, but the sample ranges overlapped; 2–3% with 4 KB reads |
| the flattened local-server prompt from borrowed pieces | 8–11% faster, ranges overlapped |
| keeping the trace file open | 15% faster, ranges overlapped |
| keyword lookup by length in the highlighter | 3–26%, ranges overlapped |
| replayed thinking blocks without a copy | 1–4% |
| a fence's style resolved once | 4% |
| OpenAI deltas routed in place | 9% slower |
| incremental streamed paragraphs | 7% slower |
| the turn's git snapshot on a thread | no benchmark, so it was not measured |

The kept changes were also run end to end: a scripted model drove `bwn run`
through searches, reads, a sandboxed command and a streamed reply with code
fences. bwn's output and every tool result it sent back were identical
before and after.

## Per step of the agent loop

| Bench | Time | Notes |
|---|---|---|
| `request_body/anthropic/30turns_50kb` | 30 µs | the conversation written to the bytes sent, once per request |
| `request_body/anthropic/120turns_1.4mb` | 0.80 ms | |
| `request_body/anthropic/300turns_4.6mb` | 2.4 ms | OpenAI and Ollama bodies are within 10% of these |
| `session_save_per_round/append/46KB` | 0.27 ms | the save after a tool round, real `fsync` |
| `session_save_per_round/append/4.6MB` | 0.48 ms | does not grow with the conversation |
| `session_save_per_round/whole_file/4.6MB` | 8.2 ms | what every save cost before 0.20.0, and what a save still costs when history is rewritten (compaction, `/rewind`) |
| `sse_parse/openai_stream/2000deltas` | 414 µs | full streaming parse: 2,000 text deltas + fragmented tool call |
| `sse_parse/anthropic_stream/2000deltas` | 536 µs | |
| `redact/50kb_mixed` | 41 µs | secret scrubbing on 50 KB of mixed output |
| `highlight/rust/271lines` | 121 µs | a fenced code block, highlighted once when it closes (the bench binary now forces colour on itself; before that, piped output had colour off and this row measured a copy) |
| `truncate/multibyte_cut` | 512 ns | char-boundary back-off on hundreds of KB of multibyte text |
| `parse_args/valid` | 294 ns | tool-call JSON args |
| `normalize/deep` | 180 ns | path normalization |
| `classify` | 121 ns | mode classification per prompt |
| `mask` | 73 ns | key masking |
| `preset_lookup` | 1.2 ns | provider table lookup |

The session-save figures include a real `fsync`, so they follow the disk;
this container's is fast. `TMPDIR` picks the folder the bench writes to.

## Claims without a benchmark

Said plainly, so nobody takes them for measured:

- "Repaints are frame-coalesced (~60fps)": output frames are capped at one
  per 16 ms and the last one always lands; it is a cap, not a frame loop, and
  the terminal tests check the behaviour, not a frame rate.
- Start-up to first prompt and `find_files` and the other tree walks have no
  benchmark here. The streamed markdown renderer has two (`stream_md/*`).
- `opt-level = 3` is what ships. On 0.19.0, `opt-level = "s"` gave a binary
  about 22% smaller with the same `--version` time; the hot paths above have
  not been compared at that level.

## Reproduce

```bash
cargo bench                                   # all microbenchmarks (criterion; colour is forced on)
cargo build --release && ls -l target/release/buildwithnexus
# startup: 3 warmups, then 30 runs of `target/release/buildwithnexus --version`,
# each timed from the parent (spawn to exit); report the median and the 95th
# percentile. Through npm: the same with
#   BWN_BIN=$PWD/target/release/buildwithnexus node bin/buildwithnexus.js --version
# memory: start the TUI in a PTY (120x40, an empty folder, NEXUS_HOME with a
# local provider configured), wait 4 s, read VmRSS in /proc/<pid>/status.
# terminal writes: run the release binary in a PTY under
#   strace -f -e trace=openat,write
# against a scripted model, and count per keystroke and per streamed chunk.
```

If a number here drifts from what the site or README claims, the docs are
wrong: file an issue.

## Other terminal agents

Startup, memory, download size, and a canned task against Claude Code, Codex
CLI, Gemini CLI, Aider, and OpenCode are a separate suite. The command, the
pinned versions, and the raw JSON are in [bench/](bench/README.md). The same
figures are published on the
[benchmarks page](https://buildwithnexus.dev/docs/benchmarks). Those numbers
are not the microbenchmarks above, and they are not estimated.
