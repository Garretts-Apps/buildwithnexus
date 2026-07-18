# Benchmarks

Every performance claim this project makes, with the command that regenerates
it. Measured 2026-07-18 on a deliberately modest machine — a 4-core
Intel Xeon @ 2.80GHz cloud container, not a tuned workstation — against the
release profile (`lto = true`, `codegen-units = 1`, stripped). Your laptop is
probably faster.

## Whole-binary numbers

| Metric | Result | How |
|---|---|---|
| Binary size | 3.98 MB (3,978,576 bytes) | `cargo build --release`, `ls -l` |
| Startup, full process lifecycle | **p50 2.0 ms · p95 2.3 ms** | 30 timed runs of `buildwithnexus --version` after 3 warmups (spawn → exit, measured from the parent) |
| TUI resident memory, idle | **4.6 MiB** | launch the alternate-screen TUI in a PTY, settle 4 s, read `VmRSS` from `/proc/<pid>/status` |

## The rendering claim

The claim on the site — streaming renders got ~215× faster in 0.12.0
(966 µs → 4.5 µs per streamed chunk) — comes from the incremental wrap cache:
a streamed chunk re-wraps **one** transcript line instead of all of them.
`cargo bench` regenerates both sides on a 2,000-line transcript:

| `transcript_append` | Time per chunk |
|---|---|
| `incremental/2000lines` (what ships) | **3.58 µs** |
| `full_rewrap/2000lines` (pre-0.12.0 behavior) | **3.76 ms** |

That's ~1,050× at this transcript length — the shipped 215× figure was
measured on a shorter transcript, and the ratio grows with session length
because the old cost was O(total lines) per chunk. The incremental case is
measured as append-plus-reset (two single-line rewraps), so it *overstates*
the real per-chunk cost.

## Hot-path microbenchmarks (`cargo bench`)

Functions that run on every ReAct step or every streamed token. Criterion
medians:

| Bench | Time | Notes |
|---|---|---|
| `sse_parse/openai_stream/2000deltas` | 1.25 ms (80 MiB/s) | full streaming parse: 2,000 text deltas + fragmented tool call |
| `body_builders/anthropic_body/30turns` | 482 µs | request body for a 30-turn conversation, built every step |
| `body_builders/openai_body/30turns` | 428 µs | |
| `redact/50kb_mixed` | 458 µs (108 MiB/s) | secret scrubbing on 50 KB of mixed output |
| `truncate/multibyte_cut` | 1.35 µs | char-boundary back-off on hundreds of KB of multibyte text |
| `parse_args/valid` | 549 ns | tool-call JSON args |
| `cache_last_message/60msgs` | 125 ns | prompt-cache breakpoint pass |
| `normalize/deep` | 354 ns | path normalization |
| `classify` | 194 ns | mode classification per prompt |
| `mask` | 165 ns | key masking |
| `preset_lookup` | 2.2 ns | provider table lookup |

## Reproduce

```bash
cargo bench                                   # all microbenchmarks (criterion)
cargo build --release && ls -l target/release/buildwithnexus
# startup: run --version in a loop and time it; memory: check VmRSS of the
# running TUI in /proc — exact scripts in the commit that added this file.
```

If a number here drifts from what the site or README claims, the docs are
wrong — file an issue.
