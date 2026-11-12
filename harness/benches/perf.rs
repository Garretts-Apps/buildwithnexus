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

criterion_group!(
    benches,
    bench_tool_batch,
    bench_wrap_cache,
    bench_request_body,
    bench_redact,
    bench_parse_args,
    bench_sse_parse,
    bench_truncate,
    bench_path_and_classify,
);
criterion_main!(benches);
