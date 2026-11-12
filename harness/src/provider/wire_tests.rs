//! Characterization of the request bodies each protocol puts on the wire.
//!
//! `wire_golden.txt` holds the exact bytes for a set of representative
//! conversations, captured from the `serde_json::Value` builders before the
//! bodies were serialised directly. Every case must stay byte-identical.
//! Regenerate (only when a body is meant to change) with
//! `BWN_BLESS_WIRE=1 cargo test -p buildwithnexus --lib wire_bodies`.

use super::*;

const GOLDEN: &str = include_str!("wire_golden.txt");

fn provider(protocol: Protocol, model: &str) -> Provider {
    Provider {
        protocol,
        base_url: "http://127.0.0.1:9".into(),
        api_key: Some("k".into()),
        model: model.into(),
        context_tokens: 100_000,
        temperature: None,
        max_tokens: None,
        effort: Effort::Off,
        ollama_ctx: std::sync::OnceLock::new(),
    }
}

fn defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "read_file",
            description: "Read a file from disk. Returns its text, or an image for image files.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File \"path\""},
                    "limit": {"type": "integer"},
                    "offset": {"type": "integer"}
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "finish",
            description: "Say the task is done",
            schema: json!({"type": "object", "properties": {"summary": {"type": "string"}}}),
        },
    ]
}

fn call(id: &str, name: &str, input: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        input,
    }
}

fn result(id: &str, content: &str, is_error: bool, images: &[(&str, &str)]) -> ToolResult {
    ToolResult {
        id: id.into(),
        content: content.into(),
        is_error,
        images: images
            .iter()
            .map(|(m, d)| (m.to_string(), d.to_string()))
            .collect(),
    }
}

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
const JPG: &str = "/9j/4AAQSkZJRgABAQEASABIAAD/2wBDAP//////////////////////wgALCAABAAEBAREA/8QAFBABAAAAAAAAAAAAAAAAAAAAAP/aAAgBAQABPxA=";

// System prompt in two parts, text with every kind of character that needs
// escaping, an image message, a round of three tool calls (one failing, one
// returning images, one returning only an image), and a closing user turn.
fn conversation(tag: &str) -> Vec<Msg> {
    let (a, b, c, d) = (
        format!("{tag}-a"),
        format!("{tag}-b"),
        format!("{tag}-c"),
        format!("{tag}-d"),
    );
    vec![
        Msg::System("You are a careful engineer.\nRead before you write.".into()),
        Msg::User(
            "Fix the \"parser\" in src\\lex.rs — tabs\tand\u{1}control, é, 日本語, 🚀".into(),
        ),
        Msg::System("Project notes: use </tags> & <think> literally.".into()),
        Msg::Assistant {
            text: "I'll look at the file first.".into(),
            calls: vec![call(
                &a,
                "read_file",
                json!({"path": "src/lex.rs", "limit": 200, "nested": {"z": [1, 2.5, null, true], "a": "q\"uote"}}),
            )],
        },
        Msg::Tool(vec![result(
            &a,
            "fn lex() {\n    let s = \"x\";\n}\n",
            false,
            &[],
        )]),
        Msg::UserImages {
            text: "Here is a screenshot of the failure".into(),
            images: vec![
                ("image/png".into(), PNG.into()),
                ("image/jpeg".into(), JPG.into()),
            ],
        },
        Msg::Assistant {
            text: String::new(),
            calls: vec![
                call(&b, "read_file", json!({"path": "missing.rs"})),
                call(&c, "read_file", json!({"path": "shot.png"})),
                call(&d, "read_file", json!({})),
            ],
        },
        Msg::Tool(vec![
            result(&b, "no such file: missing.rs", true, &[]),
            result(&c, "shot.png (1x1)", false, &[("image/png", PNG)]),
            result(&d, "", false, &[("image/png", PNG), ("image/jpeg", JPG)]),
            result(
                "orphan",
                "result of a call that is not in the transcript",
                false,
                &[("image/png", PNG)],
            ),
        ]),
        Msg::Assistant {
            text: "The lexer drops the last token.".into(),
            calls: vec![],
        },
        Msg::User("Go on.".into()),
    ]
}

// Conversations ending on each kind of message (the Anthropic cache
// breakpoint goes on the last block of the last one).
fn ending(kind: &str) -> Vec<Msg> {
    let mut msgs = conversation(kind);
    match kind {
        "end-user" => {}
        "end-images" => msgs.push(Msg::UserImages {
            text: "and this".into(),
            images: vec![("image/png".into(), PNG.into())],
        }),
        "end-assistant-text" => {
            msgs.pop();
        }
        "end-assistant-calls" => {
            msgs.truncate(7);
        }
        "end-assistant-empty" => msgs.push(Msg::Assistant {
            text: String::new(),
            calls: vec![],
        }),
        "end-tool" => {
            msgs.truncate(8);
        }
        "end-tool-text" => {
            msgs.truncate(5);
        }
        "end-tool-empty" => msgs.push(Msg::Tool(vec![])),
        "empty-systems" => {
            msgs = vec![
                Msg::System(String::new()),
                Msg::System("late".into()),
                Msg::System(String::new()),
                Msg::Assistant {
                    text: "assistant speaks first".into(),
                    calls: vec![call("s1", "finish", Value::String("not an object".into()))],
                },
                Msg::Tool(vec![result("s1", "", false, &[])]),
                Msg::User(String::new()),
                Msg::User("two user turns in a row".into()),
            ]
        }
        "no-system" => {
            msgs = vec![Msg::User("hi".into())];
        }
        "nothing" => msgs = vec![],
        other => panic!("unknown ending {other}"),
    }
    msgs
}

const ENDINGS: [&str; 11] = [
    "end-user",
    "end-images",
    "end-assistant-text",
    "end-assistant-calls",
    "end-assistant-empty",
    "end-tool",
    "end-tool-text",
    "end-tool-empty",
    "empty-systems",
    "no-system",
    "nothing",
];

// ── the bytes each protocol sends ──────────────────────────────────────────

fn anthropic(p: &Provider, msgs: &[Msg], tools: &[ToolDef], stream: bool) -> Vec<u8> {
    let (_, mut body) = anthropic_request(p, msgs, tools).unwrap();
    if stream {
        body["stream"] = json!(true);
    }
    serde_json::to_vec(&body).unwrap()
}

// `flatten`: None for the native shape, Some(keep_images) for the fallback.
fn openai(
    p: &Provider,
    msgs: &[Msg],
    tools: &[ToolDef],
    stream: bool,
    flatten: Option<bool>,
) -> Vec<u8> {
    let (_, mut body) = openai_request_at(p, &p.base_url, msgs, tools);
    if stream {
        mark_streaming(&mut body);
    }
    if let Some(keep_images) = flatten {
        flatten_openai_messages(&mut body, keep_images);
    }
    serde_json::to_vec(&body).unwrap()
}

fn ollama(
    p: &Provider,
    msgs: &[Msg],
    tools: &[ToolDef],
    stream: bool,
    think_allowed: bool,
    flatten: bool,
) -> Vec<u8> {
    let (_, mut body) = ollama_request(p, msgs, tools, 16_384);
    if stream {
        body["stream"] = json!(true);
    }
    if !think_allowed {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("think");
        }
    }
    if flatten {
        flatten_ollama_messages(&mut body);
    }
    serde_json::to_vec(&body).unwrap()
}

fn cases() -> Vec<(String, Vec<u8>)> {
    let tools = defs();
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();

    // Anthropic: every ending, with and without tools/streaming.
    for kind in ENDINGS {
        let msgs = ending(kind);
        let p = provider(Protocol::Anthropic, "claude-sonnet-4-5");
        out.push((
            format!("anthropic/{kind}/tools"),
            anthropic(&p, &msgs, &tools, false),
        ));
        out.push((
            format!("anthropic/{kind}/stream"),
            anthropic(&p, &msgs, &[], true),
        ));
    }
    // Anthropic effort: adaptive (4.6), adaptive + summary (4.7+), budget
    // form with the max_tokens bump, budget form under an explicit cap,
    // an unversioned alias. Thinking blocks are replayed in front of the
    // tool_use they preceded.
    let replay = conversation("replay");
    remember_thinking(&Reply {
        calls: vec![call("replay-a", "read_file", json!({}))],
        thinking_blocks: vec![
            json!({"type": "thinking", "thinking": "Let me read it.", "signature": "sig=="}),
            json!({"type": "redacted_thinking", "data": "b3BhcXVl"}),
        ],
        ..Default::default()
    });
    for (model, effort, max_tokens) in [
        ("claude-opus-4-6", Effort::Low, None),
        ("claude-opus-4-7", Effort::High, Some(20_000)),
        ("claude-sonnet-4-5", Effort::Medium, None),
        ("claude-sonnet-4-5", Effort::High, Some(64_000)),
        ("claude-3-7-sonnet-20250219", Effort::Low, Some(1_000)),
        ("my-alias", Effort::High, None),
        ("claude-opus-4-6", Effort::Off, Some(1_234)),
    ] {
        let mut p = provider(Protocol::Anthropic, model);
        p.effort = effort;
        p.max_tokens = max_tokens;
        let name = format!(
            "anthropic/effort/{model}/{}/{max_tokens:?}",
            effort.as_str()
        );
        out.push((name.clone(), anthropic(&p, &replay, &tools, true)));
        // Ending on the assistant turn that gets the replayed blocks.
        out.push((
            format!("{name}/end-calls"),
            anthropic(&p, &replay[..4], &tools, false),
        ));
    }

    // OpenAI-compatible: native shape, streaming, reasoning models, explicit
    // sampling settings, and the flattened fallback with and without images.
    for kind in ENDINGS {
        let msgs = ending(kind);
        let p = provider(Protocol::OpenAi, "gpt-4o");
        out.push((
            format!("openai/{kind}/tools"),
            openai(&p, &msgs, &tools, false, None),
        ));
        out.push((
            format!("openai/{kind}/stream"),
            openai(&p, &msgs, &[], true, None),
        ));
        out.push((
            format!("openai/{kind}/flat"),
            openai(&p, &msgs, &tools, true, Some(true)),
        ));
        out.push((
            format!("openai/{kind}/flat-no-images"),
            openai(&p, &msgs, &tools, false, Some(false)),
        ));
        out.push((
            format!("openai/{kind}/flat-no-tools"),
            openai(&p, &msgs, &[], false, Some(true)),
        ));
    }
    let msgs = conversation("oa");
    for (model, effort, temperature, max_tokens) in [
        ("gpt-5-mini", Effort::High, None, None),
        ("o3", Effort::Low, Some(1.0), Some(9_000)),
        ("o3", Effort::Off, None, None),
        ("gpt-4o", Effort::High, Some(0.0), Some(512)),
        ("qwen3:8b", Effort::Medium, Some(0.75), None),
    ] {
        let mut p = provider(Protocol::OpenAi, model);
        p.effort = effort;
        p.temperature = temperature;
        p.max_tokens = max_tokens;
        let name = format!(
            "openai/opts/{model}/{}/{temperature:?}/{max_tokens:?}",
            effort.as_str()
        );
        out.push((name.clone(), openai(&p, &msgs, &tools, true, None)));
        out.push((
            format!("{name}/flat"),
            openai(&p, &msgs, &tools, true, Some(true)),
        ));
    }

    // Ollama native: plain, streaming, thinking on / refused, flattened.
    for kind in ENDINGS {
        let msgs = ending(kind);
        let p = provider(Protocol::OllamaNative, "qwen3:8b");
        out.push((
            format!("ollama/{kind}/tools"),
            ollama(&p, &msgs, &tools, false, true, false),
        ));
        out.push((
            format!("ollama/{kind}/stream"),
            ollama(&p, &msgs, &[], true, true, false),
        ));
        out.push((
            format!("ollama/{kind}/flat"),
            ollama(&p, &msgs, &tools, true, true, true),
        ));
        out.push((
            format!("ollama/{kind}/flat-no-tools"),
            ollama(&p, &msgs, &[], false, true, true),
        ));
    }
    let msgs = conversation("ol");
    for (effort, temperature, max_tokens, think_allowed) in [
        (Effort::High, None, None, true),
        (Effort::Low, Some(0.0), Some(256), true),
        (Effort::Medium, Some(1.5), None, false),
    ] {
        let mut p = provider(Protocol::OllamaNative, "gemma3:4b");
        p.effort = effort;
        p.temperature = temperature;
        p.max_tokens = max_tokens;
        let name = format!(
            "ollama/opts/{}/{temperature:?}/{max_tokens:?}/{think_allowed}",
            effort.as_str()
        );
        out.push((
            name.clone(),
            ollama(&p, &msgs, &tools, true, think_allowed, false),
        ));
        out.push((
            format!("{name}/flat"),
            ollama(&p, &msgs, &tools, false, think_allowed, true),
        ));
    }
    out
}

fn render(cases: &[(String, Vec<u8>)]) -> String {
    let mut s = String::new();
    for (name, bytes) in cases {
        s.push_str("## ");
        s.push_str(name);
        s.push('\n');
        s.push_str(std::str::from_utf8(bytes).expect("request bodies are UTF-8"));
        s.push('\n');
    }
    s
}

#[test]
fn wire_bodies_are_byte_identical_to_the_recorded_ones() {
    let cases = cases();
    if std::env::var_os("BWN_BLESS_WIRE").is_some() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/provider/wire_golden.txt");
        std::fs::write(path, render(&cases)).unwrap();
        return;
    }
    let mut golden: HashMap<&str, &str> = HashMap::new();
    let mut lines = GOLDEN.lines();
    while let (Some(name), Some(body)) = (lines.next(), lines.next()) {
        golden.insert(name.strip_prefix("## ").expect("a case name"), body);
    }
    assert_eq!(
        golden.len(),
        cases.len(),
        "cases added or removed: re-record"
    );
    for (name, bytes) in &cases {
        let want = golden
            .get(name.as_str())
            .unwrap_or_else(|| panic!("no recorded body for {name}"));
        let got = std::str::from_utf8(bytes).unwrap();
        assert!(
            got == *want,
            "{name}: wire bytes changed\n got: {got}\nwant: {want}"
        );
        // And each is one well-formed JSON document.
        serde_json::from_slice::<Value>(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}
