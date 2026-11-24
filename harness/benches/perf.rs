// Performance suite. These microbenchmarks guard the hot paths of the agent
// loop — the functions that run on every ReAct step or every streamed token, and
// the ones whose cost scales with conversation/file size. Performance is the
// project's primary design lever, so regressions here should be visible.
//
// Run with:  cargo bench
// (Criterion's defaults are trimmed to `cargo_bench_support` — no plotters/rayon.)

use std::io::Cursor;
use std::path::Path;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::hint::black_box;

use buildwithnexus::config;
use buildwithnexus::provider::bench as pbench;
use buildwithnexus::provider::{Msg, ToolCall, ToolResult};
use buildwithnexus::tools;
use buildwithnexus::tools::bench as tbench;

// A session as the agent loop sends it: a long system prompt, then turns of
// user ask → assistant text + read_file call → tool result holding source
// text (newlines, quotes and indentation to escape) → assistant summary.
// `result_bytes` sizes each tool result; `image_bytes` is the base64 length
// of each of the two images (one pasted by the user, one returned by a
// tool), or 0 for none.
fn session(turns: usize, result_bytes: usize, image_bytes: usize) -> Vec<Msg> {
    let source_line =
        "    let item = lexer.next().expect(\"unexpected end of input\"); // advance\n";
    let base64 = |n: usize| {
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJ".repeat(n / 44 + 1)[..n].to_string()
    };
    let mut msgs = vec![Msg::System(
        "You are an autonomous senior software engineer. Read before you write. \
         Prefer small, verifiable edits. Call finish when done.\n"
            .repeat(32),
    )];
    for i in 0..turns {
        let ask = format!("step {i}: the test for module_{i} fails, find out why and fix it");
        if image_bytes > 0 && i == turns / 3 {
            msgs.push(Msg::UserImages {
                text: ask,
                images: vec![("image/png".into(), base64(image_bytes))],
            });
        } else {
            msgs.push(Msg::User(ask));
        }
        msgs.push(Msg::Assistant {
            text: format!("Working on step {i}. Reading the relevant files first."),
            calls: vec![ToolCall {
                id: format!("call_{i}"),
                name: "read_file".into(),
                input: serde_json::json!({"path": format!("src/module_{i}.rs"), "limit": 400}),
            }],
        });
        let images = if image_bytes > 0 && i == 2 * turns / 3 {
            vec![("image/png".to_string(), base64(image_bytes))]
        } else {
            Vec::new()
        };
        msgs.push(Msg::Tool(vec![ToolResult {
            id: format!("call_{i}"),
            content: source_line.repeat(result_bytes / source_line.len() + 1),
            is_error: false,
            images,
        }]));
        msgs.push(Msg::Assistant {
            text: format!(
                "module_{i} drops the last item when the input ends without a newline. \
                 I changed the loop to flush the pending item and re-ran the test: it passes."
            ),
            calls: vec![],
        });
    }
    msgs
}

fn bench_wrap_cache(c: &mut Criterion) {
    use buildwithnexus::tui::bench as uibench;
    // A realistic transcript line (~150 cols, wraps to two rows at width 100)
    // and a long session: 2,000 transcript lines.
    let line = "the agent read three files, proposed a diff with word-level highlights, \
                then streamed an explanation of the change while tests ran in the background"
        .to_string();
    let mut g = c.benchmark_group("transcript_append");
    // What 0.12.0 shipped: a streamed chunk re-wraps exactly one line
    // (append + reset = two single-line rewraps, an upper bound).
    g.bench_function("incremental/2000lines", |b| {
        let mut t = uibench::transcript(2_000, 100, &line);
        b.iter(|| uibench::append_and_reset(black_box(&mut t), black_box("token "), &line))
    });
    // A long paragraph streamed with no line break: each chunk re-wraps the
    // whole open line, so this is the worst case for a single line.
    let paragraph = line.repeat(13);
    g.bench_function("incremental/2000lines_2000char_line", |b| {
        let mut t = uibench::transcript(2_000, 100, &line);
        b.iter(|| uibench::append_and_reset(black_box(&mut t), black_box("token "), &paragraph))
    });
    // What every chunk cost before the wrap cache: re-wrap the whole
    // transcript. Width alternates so the cache can't short-circuit.
    g.bench_function("full_rewrap/2000lines", |b| {
        let mut t = uibench::transcript(2_000, 100, &line);
        let mut flip = false;
        b.iter(|| {
            flip = !flip;
            uibench::full_rewrap(black_box(&mut t), if flip { 99 } else { 100 })
        })
    });
    g.finish();
}

// A transcript at its cap, as in any long session or a long tool output:
// every finished line drops the oldest one, and every frame reads the rows
// at the bottom. Neither should cost in proportion to the 2,000 lines kept.
fn bench_transcript_cap(c: &mut Criterion) {
    use buildwithnexus::tui::bench as uibench;
    let line = "the agent read three files, proposed a diff with word-level highlights, \
                then streamed an explanation of the change while tests ran in the background";
    let mut g = c.benchmark_group("transcript_cap");
    g.bench_function("push_line_at_2000", |b| {
        let mut t = uibench::transcript(2_000, 100, line);
        b.iter(|| uibench::push_at_cap(black_box(&mut t), black_box(line)))
    });
    g.bench_function("frame_window_40rows_of_2000lines", |b| {
        let t = uibench::transcript(2_000, 100, line);
        b.iter(|| uibench::bottom_window(black_box(&t), black_box(40)))
    });
    g.finish();
}

// Wrapping one line from scratch: plain prose, and the same prose in colour
// (rendered markdown and highlighted code carry SGR runs through every word).
fn bench_wrap_line(c: &mut Criterion) {
    use buildwithnexus::tui::bench as uibench;
    let plain = "the agent read three files, proposed a diff with word-level highlights, \
                 then streamed an explanation of the change while tests ran in the background "
        .repeat(8);
    let coloured: String = plain
        .split(' ')
        .enumerate()
        .map(|(i, w)| {
            if i % 3 == 0 {
                format!("\x1b[38;2;102;209;163m{w}\x1b[39m")
            } else {
                w.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let coloured = format!("\x1b[1m{coloured}\x1b[0m");
    let mut g = c.benchmark_group("wrap_line");
    g.throughput(Throughput::Bytes(plain.len() as u64));
    g.bench_function("plain_1300chars", |b| {
        b.iter(|| uibench::wrap(black_box(&plain), 100))
    });
    g.throughput(Throughput::Bytes(coloured.len() as u64));
    g.bench_function("coloured_1300chars", |b| {
        b.iter(|| uibench::wrap(black_box(&coloured), 100))
    });
    g.finish();
}

// Msg → the bytes of one streaming request, per protocol, at three session
// sizes. Tool definitions (the real set, ~23 KB) ride on every request.
fn bench_request_body(c: &mut Criterion) {
    let defs = tools::defs(true);
    let mut g = c.benchmark_group("request_body");
    for (label, msgs) in [
        ("30turns_50kb", session(30, 330, 0)),
        ("120turns_1.4mb", session(120, 8_100, 150_000)),
        ("300turns_4.6mb", session(300, 11_200, 450_000)),
    ] {
        let wire = pbench::anthropic_request_bytes("claude-sonnet-4-5", &msgs, &defs).len();
        g.throughput(Throughput::Bytes(wire as u64));
        g.bench_function(format!("anthropic/{label}"), |b| {
            b.iter(|| {
                pbench::anthropic_request_bytes(
                    black_box("claude-sonnet-4-5"),
                    black_box(&msgs),
                    black_box(&defs),
                )
            })
        });
        g.bench_function(format!("openai/{label}"), |b| {
            b.iter(|| {
                pbench::openai_request_bytes(
                    black_box("gpt-4o"),
                    black_box(&msgs),
                    black_box(&defs),
                )
            })
        });
        g.bench_function(format!("ollama/{label}"), |b| {
            b.iter(|| {
                pbench::ollama_request_bytes(
                    black_box("qwen3:8b"),
                    black_box(&msgs),
                    black_box(&defs),
                )
            })
        });
    }
    g.finish();
}

fn bench_redact(c: &mut Criterion) {
    // Error bodies can be large; redaction scans every whitespace/punct-delimited
    // token. Mix in secret-shaped tokens so the redaction branch is exercised.
    let chunk = "connection refused for token sk-ABCDEFGHIJKLMNOPQRSTUVWX and key \
                 AIzaSyABCDEFGHIJKLMNOP at endpoint with some ordinary words here ";
    let big = chunk.repeat(400); // ~50 KB

    let mut g = c.benchmark_group("redact");
    g.throughput(Throughput::Bytes(big.len() as u64));
    g.bench_function("50kb_mixed", |b| b.iter(|| pbench::redact(black_box(&big))));
    g.finish();
}

fn bench_parse_args(c: &mut Criterion) {
    let valid = r#"{"path":"src/main.rs","old":"foo","new":"bar","count":42,"flag":true}"#;
    let invalid = "{not valid json at all, just a fragment";
    let mut g = c.benchmark_group("parse_args");
    g.bench_function("valid", |b| b.iter(|| pbench::parse_args(black_box(valid))));
    g.bench_function("invalid", |b| {
        b.iter(|| pbench::parse_args(black_box(invalid)))
    });
    g.finish();
}

fn bench_sse_parse(c: &mut Criterion) {
    // Throughput of the streaming parser: many small text deltas plus a fragmented
    // tool call, the shape a real completion produces token-by-token.
    let mut sse = String::new();
    for i in 0..2000 {
        sse.push_str(&format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"tok{i} \"}}}}]}}\n"
        ));
    }
    sse.push_str("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"finish\",\"arguments\":\"{\\\"summary\\\"\"}}]}}]}\n");
    sse.push_str("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":\\\"done\\\"}\"}}]}}]}\n");
    sse.push_str("data: [DONE]\n");

    let mut g = c.benchmark_group("sse_parse");
    g.throughput(Throughput::Bytes(sse.len() as u64));
    g.bench_function("openai_stream/2000deltas", |b| {
        b.iter(|| {
            let mut sink = |_: &str| {};
            let mut think_sink = |_: &str| {};
            let _ = pbench::openai_stream(
                black_box(Cursor::new(sse.as_bytes())),
                &mut sink,
                &mut think_sink,
            );
        })
    });
    // The Anthropic event stream for the same shape of reply.
    let mut sse = String::from(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":1200,\"cache_read_input_tokens\":900,\"output_tokens\":1}}}\n\n\
         event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    );
    for i in 0..2000 {
        sse.push_str(&format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"tok{i} \"}}}}\n\n"
        ));
    }
    sse.push_str(
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"c1\",\"name\":\"finish\",\"input\":{}}}\n\n\
         event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"summary\\\"\"}}\n\n\
         event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\":\\\"done\\\"}\"}}\n\n\
         event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":2100}}\n\n\
         event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    g.throughput(Throughput::Bytes(sse.len() as u64));
    g.bench_function("anthropic_stream/2000deltas", |b| {
        b.iter(|| {
            let mut sink = |_: &str| {};
            let mut think_sink = |_: &str| {};
            let _ = pbench::anthropic_stream(
                black_box(Cursor::new(sse.as_bytes())),
                &mut sink,
                &mut think_sink,
            );
        })
    });
    g.finish();
}

// ── request bodies and the stream reader, the paths the fixes touch ─────────
// The Anthropic body with extended thinking on: every assistant turn that
// made a call gets the thinking blocks recorded for it put back in front of
// its tool_use. The memory keeps the last 64 replies, so on the longer
// sessions only the latest turns replay, as they would live.
fn bench_request_body_thinking(c: &mut Criterion) {
    let defs = tools::defs(true);
    let thought = "The failing assertion compares the token count after the last line. \
                   If the loop exits on EOF before flushing, the final token is lost. "
        .repeat(16);
    // A base64 signature of the length the API sends with a thinking block.
    let signature = "c2lnbmVkIHRoaW5raW5nIGJsb2Nr".repeat(16);
    let mut g = c.benchmark_group("request_body_thinking");
    for (label, msgs) in [
        ("30turns_50kb", session(30, 330, 0)),
        ("120turns_1.4mb", session(120, 8_100, 150_000)),
        ("300turns_4.6mb", session(300, 11_200, 450_000)),
    ] {
        let turns = msgs.len() / 4;
        for i in 0..turns {
            pbench::remember_thinking(
                &format!("call_{i}"),
                vec![serde_json::json!({
                    "type": "thinking",
                    "thinking": format!("step {i}: {thought}"),
                    "signature": signature,
                })],
            );
        }
        let wire =
            pbench::anthropic_thinking_request_bytes("claude-sonnet-4-5", &msgs, &defs).len();
        g.throughput(Throughput::Bytes(wire as u64));
        g.bench_function(format!("anthropic/{label}"), |b| {
            b.iter(|| {
                pbench::anthropic_thinking_request_bytes(
                    black_box("claude-sonnet-4-5"),
                    black_box(&msgs),
                    black_box(&defs),
                )
            })
        });
    }
    g.finish();
}

// The OpenAI body in the flattened fallback: the transcript folded into
// alternating user/assistant turns, tool calls and results written as text,
// the tool guide at the top of the first turn.
fn bench_request_body_flat(c: &mut Criterion) {
    let defs = tools::defs(true);
    let mut g = c.benchmark_group("request_body_flat");
    for (label, msgs) in [
        ("30turns_50kb", session(30, 330, 0)),
        ("120turns_1.4mb", session(120, 8_100, 150_000)),
        ("300turns_4.6mb", session(300, 11_200, 450_000)),
    ] {
        let wire = pbench::openai_flat_request_bytes("gpt-4o", &msgs, &defs).len();
        g.throughput(Throughput::Bytes(wire as u64));
        g.bench_function(format!("openai/{label}"), |b| {
            b.iter(|| {
                pbench::openai_flat_request_bytes(
                    black_box("gpt-4o"),
                    black_box(&msgs),
                    black_box(&defs),
                )
            })
        });
    }
    g.finish();
}

// A response body that hands out at most `max` bytes per read, the way a
// socket delivers a stream in pieces, so the reader's per-read work counts.
struct Trickle {
    data: std::sync::Arc<[u8]>,
    pos: usize,
    max: usize,
}

impl std::io::Read for Trickle {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = out.len().min(self.max).min(self.data.len() - self.pos);
        out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

// The same stream as sse_parse/openai_stream, read through the
// interruptible reader (worker thread, channel, line buffer) a live
// response goes through.
fn bench_sse_stream_reader(c: &mut Criterion) {
    let mut sse = String::new();
    for i in 0..2000 {
        sse.push_str(&format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"tok{i} \"}}}}]}}\n"
        ));
    }
    sse.push_str("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"finish\",\"arguments\":\"{\\\"summary\\\"\"}}]}}]}\n");
    sse.push_str("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":\\\"done\\\"}\"}}]}}]}\n");
    sse.push_str("data: [DONE]\n");
    let data: std::sync::Arc<[u8]> = sse.as_bytes().into();

    let mut g = c.benchmark_group("sse_stream_reader");
    g.throughput(Throughput::Bytes(data.len() as u64));
    for max in [256usize, 4096] {
        g.bench_function(format!("openai_stream/2000deltas_{max}b_reads"), |b| {
            b.iter(|| {
                let mut sink = |_: &str| {};
                let mut think_sink = |_: &str| {};
                let body = Trickle {
                    data: data.clone(),
                    pos: 0,
                    max,
                };
                let _ = pbench::openai_stream_interruptible(
                    black_box(body),
                    &mut sink,
                    &mut think_sink,
                );
            })
        });
    }
    g.finish();
}

fn bench_truncate(c: &mut Criterion) {
    // Large multibyte output is the worst case for the char-boundary back-off.
    let big = "🎉日本語テキスト ".repeat(20_000); // hundreds of KB, multibyte
    let mut g = c.benchmark_group("truncate");
    g.bench_function("multibyte_cut", |b| {
        // Clone per iter since truncate consumes its String.
        b.iter_with_setup(
            || big.clone(),
            |s| tbench::truncate(black_box(s), 16 * 1024),
        )
    });
    g.finish();
}

fn bench_path_and_classify(c: &mut Criterion) {
    let deep = Path::new("a/b/./c/../d/e/../../f/g/h/./i/../j/k");
    c.bench_function("normalize/deep", |b| {
        b.iter(|| tbench::normalize(black_box(deep)))
    });

    let key = "sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    c.bench_function("mask", |b| b.iter(|| config::mask(black_box(key))));

    let task = "design and architect a new authentication subsystem with proper scoping";
    c.bench_function("classify", |b| {
        b.iter(|| buildwithnexus::classify(black_box(task)))
    });

    let id = "ollama";
    c.bench_function("preset_lookup", |b| {
        b.iter(|| config::preset(black_box(id)))
    });
}

// A project of `files` source files under a fresh temp folder, for the
// tool-batch benchmarks: real files, so reads and searches hit the disk
// cache the way they do in a session.
fn bench_project(files: usize) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("bwn-bench-batch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for i in 0..files {
        let sub = dir.join(format!("src/m{}", i % 40));
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join(format!("f{i}.rs")),
            format!("// module {i}\npub fn run_{i}() -> u32 {{ {i} }}\n").repeat(30),
        )
        .unwrap();
    }
    dir
}

// One reply's read-only calls, one at a time (width 1, what the build loop
// did before 0.19) and side by side (width 4, the default).
fn bench_tool_batch(c: &mut Criterion) {
    use buildwithnexus::agent::run_read_only_calls;
    use serde_json::json;
    let dir = bench_project(2000);
    let greps: Vec<(String, serde_json::Value)> = ["run_1", "module 2", "u32", "pub fn"]
        .iter()
        .map(|p| ("grep_files".to_string(), json!({"pattern": p})))
        .collect();
    let reads: Vec<(String, serde_json::Value)> = (0..6)
        .map(|i| {
            (
                "read_file".to_string(),
                json!({"path": format!("src/m{i}/f{i}.rs")}),
            )
        })
        .collect();
    let mut g = c.benchmark_group("tool_batch");
    g.sample_size(20);
    for (label, calls) in [("grep4", &greps), ("read6", &reads)] {
        for width in [1usize, 4] {
            g.bench_function(format!("{label}/width{width}"), |b| {
                b.iter(|| run_read_only_calls(black_box(calls), &dir, width))
            });
        }
    }
    // Four fetches to four local servers that each take 100 ms to answer:
    // simulated network latency, not a real network.
    let ports: Vec<u16> = (0..4).map(|_| slow_server(100)).collect();
    let fetches: Vec<(String, serde_json::Value)> = ports
        .iter()
        .map(|p| {
            (
                "fetch_url".to_string(),
                json!({"url": format!("http://127.0.0.1:{p}/")}),
            )
        })
        .collect();
    for width in [1usize, 4] {
        g.bench_function(format!("fetch4_simulated_100ms/width{width}"), |b| {
            b.iter(|| run_read_only_calls(black_box(&fetches), &dir, width))
        });
    }
    g.finish();
    let _ = std::fs::remove_dir_all(&dir);
}

// A local HTTP server that answers every request after `delay_ms`.
fn slow_server(delay_ms: u64) -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                let body = "<html><body>slow page</body></html>";
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    port
}

// ── session save per tool round ──────────────────────────────────────────────
// (begin: session log benches)
//
// The transcript is saved after every tool round. `append` is that save: the
// round's two messages are appended to the session log and flushed, so its
// cost follows the round, not the conversation. `whole_file` is what every
// save cost before the log, and what a save still costs when history changed
// (compaction, /rewind, a new model): the whole transcript serialized,
// written and flushed. Both include a real fsync, so run them on the kind of
// disk you care about (TMPDIR picks the folder).

// What one tool round adds, sized so 200 rounds make a 4.6 MB transcript.
fn session_round(i: usize) -> [Msg; 2] {
    [
        Msg::Assistant {
            text: format!("Reading module {i} before changing it."),
            calls: vec![ToolCall {
                id: format!("call_{i}"),
                name: "read_file".into(),
                input: serde_json::json!({"path": format!("src/module_{i}.rs")}),
            }],
        },
        Msg::Tool(vec![ToolResult {
            id: format!("call_{i}"),
            content: format!("    let value_{i} = compute(input, {i})?; // line\n").repeat(500),
            is_error: false,
            images: Vec::new(),
        }]),
    ]
}

fn session_transcript(bytes: usize) -> Vec<Msg> {
    let mut msgs = vec![
        Msg::System("You are an autonomous senior software engineer.".repeat(20)),
        Msg::User("refactor the module layout".into()),
    ];
    let mut size = 0;
    let mut i = 0;
    while size < bytes {
        let round = session_round(i);
        size += serde_json::to_string(&round[..]).unwrap().len();
        msgs.extend(round);
        i += 1;
    }
    msgs
}

fn bench_session_save(c: &mut Criterion) {
    use buildwithnexus::session;
    use std::time::{Duration, Instant};
    let home = std::env::temp_dir().join(format!("bwn-bench-session-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::env::set_var("NEXUS_HOME", &home);
    let cwd = Path::new("/bench/project");
    let mut g = c.benchmark_group("session_save_per_round");
    g.sample_size(20);
    for (label, bytes) in [("46KB", 46_000), ("1.4MB", 1_400_000), ("4.6MB", 4_600_000)] {
        let base = session_transcript(bytes);
        let id = format!("bench-{label}");
        g.bench_function(format!("append/{label}"), |b| {
            b.iter_custom(|iters| {
                let mut spent = Duration::ZERO;
                let mut left = iters;
                while left > 0 {
                    // Untimed: the file back at its starting size, and the
                    // rounds to add built ahead.
                    let mut msgs = base.clone();
                    let _ = session::remove(&id);
                    assert!(session::save(&id, cwd, "bench-model", &msgs));
                    let n = left.min(32);
                    let rounds: Vec<[Msg; 2]> = (0..n as usize).map(session_round).collect();
                    let start = Instant::now();
                    for round in rounds {
                        msgs.extend(round);
                        black_box(session::save_round(&id, cwd, "bench-model", &msgs));
                    }
                    spent += start.elapsed();
                    left -= n;
                }
                spent
            })
        });
        g.bench_function(format!("whole_file/{label}"), |b| {
            let mut msgs = base.clone();
            msgs.extend(session_round(0));
            let mut flip = false;
            b.iter(|| {
                // A new model is a new header: the file is written whole.
                flip = !flip;
                let model = if flip {
                    "bench-model-a"
                } else {
                    "bench-model-b"
                };
                black_box(session::save_round(&id, cwd, model, &msgs))
            })
        });
    }
    g.finish();
    std::env::remove_var("NEXUS_HOME");
    let _ = std::fs::remove_dir_all(&home);
}
// (end: session log benches)

// ── terminal UI: streamed-line wrap and the code highlighter ────────────────
// A paragraph streamed with no line break, in chunks the size of a token:
// the cost of a chunk must not grow with the line (it did: each chunk
// re-wrapped the open line from its start).
fn bench_stream_wrap(c: &mut Criterion) {
    use buildwithnexus::tui::bench as uibench;
    let line = "the agent read three files, proposed a diff with word-level highlights";
    let chunk = "token ";
    let mut g = c.benchmark_group("stream_wrap");
    for chars in [200usize, 2_000, 20_000] {
        let chunks = chars / chunk.len();
        g.throughput(Throughput::Elements(chunks as u64));
        if chars >= 20_000 {
            g.sample_size(20);
        }
        g.bench_function(format!("chunks_into_{chars}char_line"), |b| {
            let mut t = uibench::transcript(200, 100, line);
            b.iter(|| uibench::stream_line(black_box(&mut t), black_box(chunk), chunks))
        });
    }
    g.finish();
}

// A fenced code block is highlighted once, when its closing fence lands.
fn bench_highlight(c: &mut Criterion) {
    use buildwithnexus::highlight::highlight_block;
    let cases: [(&str, &str, &str); 4] = [
        (
            "rust",
            "rust",
            include_str!("../tests/fixtures/highlight/sample.rs.txt"),
        ),
        (
            "python",
            "python",
            include_str!("../tests/fixtures/highlight/sample.py"),
        ),
        (
            "yaml",
            "yaml",
            include_str!("../tests/fixtures/highlight/sample.yml"),
        ),
        (
            "json",
            "json",
            include_str!("../tests/fixtures/highlight/sample.json"),
        ),
    ];
    let mut g = c.benchmark_group("highlight");
    for (name, tag, text) in cases {
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        g.throughput(Throughput::Bytes(text.len() as u64));
        g.bench_function(format!("{name}/{}lines", lines.len()), |b| {
            b.iter(|| highlight_block(black_box(tag), black_box(&lines)))
        });
    }
    // One long line: minified code, or a data file pasted into a fence.
    let long = vec!["f(a, \"s\", 42) + Vec::new(x) ".repeat(400)];
    g.throughput(Throughput::Bytes(long[0].len() as u64));
    g.bench_function("rust/one_11kb_line", |b| {
        b.iter(|| highlight_block(black_box("rust"), black_box(&long)))
    });
    g.finish();
}
// ── end terminal UI ──────────────────────────────────────────────────────────
// ── perf-tools: grep_files and large-file read_file ─────────────────────
// A 2,000-file, ~8 MB tree: `bench_project` plus filler so each file is
// about 4 KB, the size the search was measured on.
fn grep_tree() -> std::path::PathBuf {
    let dir = bench_project(2000);
    for i in 0..2000 {
        let path = dir.join(format!("src/m{}/f{i}.rs", i % 40));
        let mut text = std::fs::read_to_string(&path).unwrap();
        for n in 0..48 {
            text.push_str(&format!(
                "    let value_{n} = compute(alpha, beta) + offset; // filler {n}\n"
            ));
        }
        std::fs::write(&path, text).unwrap();
    }
    dir
}

fn bench_grep_files(c: &mut Criterion) {
    use serde_json::json;
    let dir = grep_tree();
    let mut g = c.benchmark_group("grep_files");
    g.sample_size(20);
    for (label, pattern) in [
        ("zero", "zzz_not_there"),
        ("few", "run_1999()"),
        ("many", "pub fn"),
    ] {
        for (case, sensitive) in [("insensitive", false), ("sensitive", true)] {
            let input = json!({"pattern": pattern, "case_sensitive": sensitive});
            g.bench_function(format!("{label}/{case}"), |b| {
                b.iter(|| tools::run("grep_files", black_box(&input), &dir))
            });
        }
    }
    g.finish();
    let _ = std::fs::remove_dir_all(&dir);
}

fn bench_read_large_file(c: &mut Criterion) {
    use serde_json::json;
    let dir = std::env::temp_dir().join(format!("bwn-bench-read-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let line = "2026-01-01T00:00:00Z INFO worker: handled request id=123456 in 12 ms\n";
    std::fs::write(
        dir.join("big.log"),
        line.repeat(60 * 1024 * 1024 / line.len()),
    )
    .unwrap();
    std::fs::write(dir.join("small.rs"), "fn main() {}\n".repeat(40)).unwrap();
    let mut g = c.benchmark_group("read_file");
    g.sample_size(10);
    for (label, input) in [
        ("small", json!({"path": "small.rs"})),
        ("60mb/whole", json!({"path": "big.log"})),
        (
            "60mb/lines_100_200",
            json!({"path": "big.log", "start_line": 100, "end_line": 200}),
        ),
        (
            "60mb/last_lines",
            json!({"path": "big.log", "start_line": 880000, "end_line": 880100}),
        ),
    ] {
        g.bench_function(label, |b| {
            b.iter(|| tools::run("read_file", black_box(&input), &dir))
        });
    }
    g.finish();
    let _ = std::fs::remove_dir_all(&dir);
}

// The tool table's lookup by name, through `is_parallel_safe` (one lookup
// and one map probe): the first row, the last row, and names in no row,
// which scan the whole table.
fn bench_tool_lookup(c: &mut Criterion) {
    let input = serde_json::json!({"path": "a"});
    let mut g = c.benchmark_group("tool_lookup");
    for name in ["read", "grep_files", "spawn_subagent", "mcp__server__tool"] {
        g.bench_function(name, |b| {
            b.iter(|| tools::is_parallel_safe(black_box(name), black_box(&input)))
        });
    }
    g.finish();
}
// ── end perf-tools ──────────────────────────────────────────────────────

// ── per-round bookkeeping: token estimate, trust check, trace ────────────
// Work the build loop repeats on every request or every tool call, whose
// cost should follow what the round added, not the size of the session or
// how many files the project's settings name.

// The context estimate before each request, on a 4.6 MB transcript: one
// message appended, then the estimate, as the loop pays it every round.
fn bench_token_estimate(c: &mut Criterion) {
    use buildwithnexus::agent::bench::TokenMeter;
    use std::time::{Duration, Instant};
    let base = session_transcript(4_600_000);
    let mut g = c.benchmark_group("token_estimate");
    g.bench_function("4.6MB_session", |b| {
        b.iter_custom(|iters| {
            let mut spent = Duration::ZERO;
            let mut left = iters;
            while left > 0 {
                // Untimed: the transcript back at its starting size, the
                // estimate already taken once, and the messages to add.
                let mut msgs = base.clone();
                let mut meter = TokenMeter::new();
                black_box(meter.tokens(&msgs));
                let n = left.min(64);
                let adds: Vec<Msg> = (0..n as usize)
                    .flat_map(session_round)
                    .take(n as usize)
                    .collect();
                let start = Instant::now();
                for m in adds {
                    msgs.push(m);
                    black_box(meter.tokens(black_box(&msgs)));
                }
                spent += start.elapsed();
                left -= n;
            }
            spent
        })
    });
    g.finish();
}

// One gated tool call (a plain `ls` in ask mode, pre-approved by default)
// in a trusted project whose settings name 10 hook scripts: the deny, ask
// and allow rules and the saved approvals are looked up, and each lookup
// checks the project's trust.
fn bench_trust_gate(c: &mut Criterion) {
    use buildwithnexus::agent::bench as abench;
    let root = std::env::temp_dir().join(format!("bwn-bench-trust-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let project = root.join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(project.join(".buildwithnexus/hooks")).unwrap();
    std::fs::create_dir_all(project.join("src")).unwrap();
    let mut groups = Vec::new();
    for i in 0..10 {
        let script = format!(".buildwithnexus/hooks/check_{i}.sh");
        std::fs::write(
            project.join(&script),
            format!("#!/bin/sh\n# check {i}\necho '{{}}'\n").repeat(100),
        )
        .unwrap();
        groups.push(serde_json::json!({
            "matcher": "write_file",
            "hooks": [{"type": "command", "command": format!("sh {script}")}],
        }));
    }
    let settings = serde_json::to_string_pretty(&serde_json::json!({
        "hooks": {"PreToolUse": groups},
        "permissions": {
            "allow": ["run_command(cargo test)"],
            "deny": ["run_command(rm -rf /)"],
        },
    }))
    .unwrap();
    std::fs::write(project.join(".buildwithnexus/settings.json"), &settings).unwrap();
    std::env::set_var("NEXUS_HOME", &home);
    let digest = buildwithnexus::hooks::trust_digest(&project, &settings);
    let store = serde_json::json!({ config::project_key(&project): {"settings.json": digest} });
    std::fs::write(home.join("trusted.json"), store.to_string()).unwrap();
    // A session runs in its project: settings lookups that take no folder
    // use the current one.
    let was = std::env::current_dir().unwrap();
    std::env::set_current_dir(&project).unwrap();
    let input = serde_json::json!({"command": "ls src"});
    assert_eq!(abench::gate("run_command", &input, &project), None);
    let mut g = c.benchmark_group("trust");
    g.bench_function("gate_10_hook_refs", |b| {
        b.iter(|| abench::gate(black_box("run_command"), black_box(&input), &project))
    });
    g.finish();
    std::env::set_current_dir(was).unwrap();
    std::env::remove_var("NEXUS_HOME");
    let _ = std::fs::remove_dir_all(&root);
}

// 100 trace events as hooks record them: 50 hook runs, each a trigger
// carrying a 20 KB tool input and a result with 20 KB of stdout, so both
// are clipped before they are kept and written to the session's trace file.
fn bench_trace_hook_events(c: &mut Criterion) {
    use buildwithnexus::hooks::bench as hbench;
    let home = std::env::temp_dir().join(format!("bwn-bench-trace-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::env::set_var("NEXUS_HOME", &home);
    buildwithnexus::trace::set_session("bench-trace");
    let content = "    let value = compute(input)?; // a line of the file being written\n"
        .repeat(20_000 / 68 + 1);
    let payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "bench-trace",
        "cwd": "/bench/project",
        "tool_name": "write_file",
        "tool_input": {"path": "src/lib.rs", "content": content},
    });
    let stdout = "checked src/lib.rs: no findings on this line\n".repeat(20_000 / 45 + 1);
    let mut g = c.benchmark_group("trace");
    g.sample_size(20);
    g.bench_function("hook_events_100", |b| {
        b.iter(|| {
            for _ in 0..50 {
                hbench::record_hook_run(black_box(&payload), black_box(&stdout), "");
            }
        })
    });
    g.finish();
    std::env::remove_var("NEXUS_HOME");
    let _ = std::fs::remove_dir_all(&home);
}
// ── end per-round bookkeeping ────────────────────────────────────────────

criterion_group!(
    benches,
    bench_stream_wrap,
    bench_highlight,
    bench_tool_batch,
    bench_grep_files,
    bench_read_large_file,
    bench_tool_lookup,
    bench_wrap_cache,
    bench_transcript_cap,
    bench_wrap_line,
    bench_request_body,
    bench_redact,
    bench_parse_args,
    bench_sse_parse,
    bench_request_body_thinking,
    bench_request_body_flat,
    bench_sse_stream_reader,
    bench_truncate,
    bench_path_and_classify,
    bench_session_save,
    bench_token_estimate,
    bench_trust_gate,
    bench_trace_hook_events,
);
criterion_main!(benches);
