# Benchmarks

Every performance claim this project makes, with the command that regenerates
it. Measured 2026-10-27 (v0.15.1) on a deliberately modest machine — a 4-core
Intel Xeon @ 2.10GHz cloud container (Linux 6.18, x86_64), not a tuned
workstation — against the release profile (`lto = true`, `codegen-units = 1`,
stripped). Your laptop is probably faster.

## Whole-binary numbers

| Metric | Result | How |
|---|---|---|
| Binary size | 6.07 MB (6,070,536 bytes) | `cargo build --release`, `ls -l` |
| Startup of the native binary, `--version`, full process lifecycle | **p50 2.1 ms · p95 2.5 ms** | 30 timed runs of `buildwithnexus --version` after 3 warmups (spawn → exit, measured from the parent). Three repeats of the 30 runs gave p50 2.0–2.3 ms |
| Startup through the npm launcher, `bwn --version` | **p50 47 ms · p95 57 ms** | the same 30 runs of `node bin/buildwithnexus.js --version` with `BWN_BIN` set to the release binary; the `node` process alone (`node -e 0`) takes p50 26 ms of that |
| TUI resident memory, idle | **7.7 MiB** (median of 5 runs, 7.6–8.1) | launch the alternate-screen TUI in a PTY (120×40, an empty project folder, a configured local provider), settle 4 s, read `VmRSS` from `/proc/<pid>/status` |

The 2 ms figure is the native binary running `--version`. Going through the npm
launcher (`npm install -g buildwithnexus`, then `bwn`) adds the startup time of
Node itself, about 26 ms here, plus the launcher's own module loading and the
spawn of the binary: about 47 ms in all for `bwn --version`. A cargo-installed
`bwn` has no Node in front of it.

## The rendering claim

The claim on the site — streaming renders got ~215× faster in 0.12.0
(966 µs → 4.5 µs per streamed chunk) — comes from the incremental wrap cache:
a streamed chunk re-wraps **one** transcript line instead of all of them.
`cargo bench` regenerates both sides on a 2,000-line transcript:

| `transcript_append` | Time per chunk |
|---|---|
| `incremental/2000lines` (what ships) | **4.21 µs** |
| `full_rewrap/2000lines` (pre-0.12.0 behavior) | **4.53 ms** |

That's ~1,080× at this transcript length — the shipped 215× figure was
measured on a shorter transcript, and the ratio grows with session length
because the old cost was O(total lines) per chunk. The incremental case is
measured as append-plus-reset (two single-line rewraps), so it *overstates*
the real per-chunk cost.

## Hot-path microbenchmarks (`cargo bench`)

Functions that run on every ReAct step or every streamed token. Criterion
medians:

| Bench | Time | Notes |
|---|---|---|
| `sse_parse/openai_stream/2000deltas` | 1.15 ms (87 MiB/s) | full streaming parse: 2,000 text deltas + fragmented tool call |
| `body_builders/anthropic_body/30turns` | 401 µs | request body for a 30-turn conversation, built every step |
| `body_builders/openai_body/30turns` | 383 µs | |
| `redact/50kb_mixed` | 358 µs (138 MiB/s) | secret scrubbing on 50 KB of mixed output |
| `truncate/multibyte_cut` | 970 ns | char-boundary back-off on hundreds of KB of multibyte text |
| `parse_args/valid` | 435 ns | tool-call JSON args |
| `cache_last_message/60msgs` | 101 ns | prompt-cache breakpoint pass |
| `normalize/deep` | 276 ns | path normalization |
| `classify` | 189 ns | mode classification per prompt |
| `mask` | 141 ns | key masking |
| `preset_lookup` | 2.1 ns | provider table lookup |

## Reproduce

```bash
cargo bench                                   # all microbenchmarks (criterion)
cargo build --release && ls -l target/release/buildwithnexus
# startup: 3 warmups, then 30 runs of `target/release/buildwithnexus --version`,
# each timed from the parent (spawn to exit); report the median and the 95th
# percentile. Through npm: the same with
#   BWN_BIN=$PWD/target/release/buildwithnexus node bin/buildwithnexus.js --version
# memory: start the TUI in a PTY (120x40, an empty folder, NEXUS_HOME with a
# local provider configured), wait 4 s, read VmRSS in /proc/<pid>/status.
```

If a number here drifts from what the site or README claims, the docs are
wrong — file an issue.
