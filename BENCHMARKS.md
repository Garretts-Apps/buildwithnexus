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
| `highlight/rust/271lines` | 121 µs | a fenced code block, highlighted once when it closes (run with `FORCE_COLOR=1`; piped output has colour off and measures a copy) |
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
- Start-up to first prompt, `find_files` and the other tree walks, and the
  markdown renderer have no benchmark here.
- `opt-level = 3` is what ships. On 0.19.0, `opt-level = "s"` gave a binary
  about 22% smaller with the same `--version` time; the hot paths above have
  not been compared at that level.

## Reproduce

```bash
FORCE_COLOR=1 cargo bench                     # all microbenchmarks (criterion)
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
