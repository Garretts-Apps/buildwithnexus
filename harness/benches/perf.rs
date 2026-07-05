// Performance suite. These microbenchmarks guard the hot paths of the agent
// loop — the functions that run on every ReAct step or every streamed token, and
// the ones whose cost scales with conversation/file size. Performance is the
// project's primary design lever, so regressions here should be visible.
//
// Run with:  cargo bench
// (Criterion's defaults are trimmed to `cargo_bench_support` — no plotters/rayon.)

use std::io::Cursor;
use std::path::Path;

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

use buildwithnexus::config;
use buildwithnexus::provider::bench as pbench;
use buildwithnexus::provider::{Msg, ToolCall, ToolResult};
use buildwithnexus::tools;
use buildwithnexus::tools::bench as tbench;

// A long, realistic multi-step conversation: system + alternating
// user/assistant(+tool_use)/tool turns. This is what the body builders and the
// cache pass walk on every single agent iteration, so its cost matters.
fn long_conversation(turns: usize) -> Vec<Msg> {
    let mut msgs = vec![Msg::System(
        "You are an autonomous senior software engineer. Read before you write. \
         Prefer small, verifiable edits. Call finish when done."
            .repeat(4),
    )];
    for i in 0..turns {
        msgs.push(Msg::User(format!(
            "step {i}: please inspect and modify the project as needed"
        )));
        msgs.push(Msg::Assistant {
            text: format!("Working on step {i}. Reading the relevant files first."),
            calls: vec![ToolCall {
                id: format!("call_{i}"),
                name: "read_file".into(),
                input: serde_json::json!({"path": format!("src/module_{i}.rs")}),
            }],
        });
        msgs.push(Msg::Tool(vec![ToolResult {
            id: format!("call_{i}"),
            content: format!("// contents of module {i}\n").repeat(20),
            is_error: false,
        }]));
    }
    msgs
}

fn bench_body_builders(c: &mut Criterion) {
    let msgs = long_conversation(30);
    let defs = tools::defs(true);

    let mut g = c.benchmark_group("body_builders");
    g.bench_function("anthropic_body/30turns", |b| {
        b.iter(|| {
            pbench::anthropic_body(
                black_box("claude-sonnet-4-6"),
                black_box(&msgs),
                black_box(&defs),
            )
        })
    });
    g.bench_function("openai_body/30turns", |b| {
        b.iter(|| pbench::openai_body(black_box("gpt-4o"), black_box(&msgs), black_box(&defs)))
    });
    g.finish();
}

fn bench_cache_pass(c: &mut Criterion) {
    // The cache breakpoint pass runs once per request on the rendered messages.
    let mut messages: Vec<serde_json::Value> = (0..60)
        .map(|i| serde_json::json!({"role": "user", "content": format!("message body number {i}")}))
        .collect();
    c.bench_function("cache_last_message/60msgs", |b| {
        b.iter(|| pbench::cache_last_message(black_box(&mut messages)))
    });
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
            let _ = pbench::openai_stream(black_box(Cursor::new(sse.as_bytes())), &mut sink);
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

criterion_group!(
    benches,
    bench_body_builders,
    bench_cache_pass,
    bench_redact,
    bench_parse_args,
    bench_sse_parse,
    bench_truncate,
    bench_path_and_classify,
);
criterion_main!(benches);
