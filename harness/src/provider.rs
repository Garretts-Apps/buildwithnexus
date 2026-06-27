// The model wire layer. One enum, two `match` arms — Anthropic Messages and the
// OpenAI /chat/completions shape (which also covers Ollama, llama.cpp, LM Studio,
// OpenRouter, Groq, and Hugging Face). No trait objects: the call site matches.
//
// Each protocol has a body builder + a request builder, shared by the blocking
// `complete()` and the streaming `stream()` so there's exactly one place that
// knows each vendor's JSON shape.

use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Protocol;
use crate::tools::ToolDef;

pub struct Provider {
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub context_tokens: usize, // model context window, for compaction thresholds
}

// Neutral conversation. The provider translates this into each vendor's shape;
// the rest of the program never sees vendor JSON. Serializable + cloneable so a
// transcript can be persisted to a session file and restored on resume.
#[derive(Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub id: String,
    pub content: String,
    pub is_error: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub enum Msg {
    System(String),
    User(String),
    Assistant { text: String, calls: Vec<ToolCall> },
    Tool(Vec<ToolResult>),
}
pub struct Reply {
    pub text: String,
    pub calls: Vec<ToolCall>,
}

// One process-wide agent so the TLS connection is pooled and kept alive across
// every ReAct step — each agent iteration is an HTTP round-trip, and skipping the
// handshake after the first call is a free latency win on multi-step tasks.
fn agent() -> &'static ureq::Agent {
    static A: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    A.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout(Duration::from_secs(180))
            .build()
    })
}

fn url(p: &Provider, path: &str) -> String {
    format!("{}{}", p.base_url.trim_end_matches('/'), path)
}

// Warm the pooled TLS connection in the background so the first real request
// skips the TCP+TLS+DNS handshake — shaves latency off the first token.
pub fn prewarm(p: &Provider) {
    let url = match p.protocol {
        Protocol::Anthropic => url(p, "/v1/models"),
        Protocol::OpenAi => url(p, "/models"),
    };
    let key = p.api_key.clone();
    let proto = p.protocol;
    std::thread::spawn(move || {
        let mut req = agent().get(&url).timeout(Duration::from_secs(5));
        req = match (proto, &key) {
            (Protocol::Anthropic, Some(k)) => req.set("x-api-key", k).set("anthropic-version", "2023-06-01"),
            (Protocol::OpenAi, Some(k)) => req.set("authorization", &format!("Bearer {k}")),
            _ => req,
        };
        let _ = req.call(); // result ignored — we only want the connection warmed
    });
}

// Thin wrappers exposing the internal pure helpers to the criterion perf suite
// without widening the real API (the originals stay private). Not semver-stable.
#[doc(hidden)]
pub mod bench {
    use super::*;
    pub fn redact(s: &str) -> String { super::redact(s) }
    pub fn parse_args(s: &str) -> Value { super::parse_args(s) }
    pub fn cache_last_message(m: &mut [Value]) { super::cache_last_message(m) }
    pub fn anthropic_body(model: &str, msgs: &[Msg], tools: &[ToolDef]) -> Value {
        super::anthropic_body(model, msgs, tools)
    }
    pub fn openai_body(model: &str, msgs: &[Msg], tools: &[ToolDef]) -> Value {
        super::openai_body(model, msgs, tools)
    }
    pub fn openai_stream(reader: impl Read, on_text: &mut dyn FnMut(&str)) -> Result<Reply, String> {
        super::openai_stream(reader, on_text)
    }
}

// Installed models reported by a running Ollama (GET /api/tags). Empty on any
// failure (not running, wrong host, …). `base_url` is the OpenAI-style base
// (…/v1); Ollama's native API lives at the host root, so /v1 is trimmed.
pub fn ollama_models(base_url: &str) -> Vec<String> {
    let root = base_url.trim_end_matches('/').trim_end_matches("/v1");
    let resp = match agent().get(&format!("{root}/api/tags")).timeout(Duration::from_secs(2)).call() {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let v: Value = match resp.into_json() {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    v["models"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|m| m["name"].as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

// ── public entry points ────────────────────────────────────────────────────
pub fn complete(p: &Provider, msgs: &[Msg], tools: &[ToolDef]) -> Result<Reply, String> {
    let mut sink = |_: &str| {};
    request(p, msgs, tools, false, &mut sink)
}

// Streams assistant text to `on_text` as it arrives; tool calls are accumulated
// and returned once the turn completes.
pub fn stream(p: &Provider, msgs: &[Msg], tools: &[ToolDef], on_text: &mut dyn FnMut(&str)) -> Result<Reply, String> {
    request(p, msgs, tools, true, on_text)
}

fn request(p: &Provider, msgs: &[Msg], tools: &[ToolDef], streaming: bool, on_text: &mut dyn FnMut(&str)) -> Result<Reply, String> {
    match p.protocol {
        Protocol::Anthropic => {
            let (req, mut body) = anthropic_request(p, msgs, tools)?;
            if streaming {
                body["stream"] = json!(true);
                anthropic_stream(send_raw(req, body)?.into_reader(), on_text)
            } else {
                anthropic_parse(send(req, body)?)
            }
        }
        Protocol::OpenAi => {
            let (req, mut body) = openai_request(p, msgs, tools);
            if streaming {
                body["stream"] = json!(true);
                openai_stream(send_raw(req, body)?.into_reader(), on_text)
            } else {
                openai_parse(send(req, body)?)
            }
        }
    }
}

// ── HTTP plumbing ──────────────────────────────────────────────────────────
fn send(req: ureq::Request, body: Value) -> Result<Value, String> {
    send_raw(req, body)?.into_json::<Value>().map_err(|e| format!("bad JSON from server: {e}"))
}

fn send_raw(req: ureq::Request, body: Value) -> Result<ureq::Response, String> {
    match req.send_json(body) {
        Ok(resp) => Ok(resp),
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp.into_string().unwrap_or_default();
            Err(format!("HTTP {code}: {}", redact(&detail).chars().take(400).collect::<String>()))
        }
        Err(e) => Err(format!("connection failed: {}", redact(&e.to_string()))),
    }
}

// Defense-in-depth: blank out anything that looks like an API key/token before
// surfacing an upstream error body to the user or logs.
fn redact(s: &str) -> String {
    s.split_inclusive(|c: char| c.is_whitespace() || "\"',:;()[]{}".contains(c))
        .map(|tok| {
            let core = tok.trim_end_matches(|c: char| c.is_whitespace() || "\"',:;()[]{}".contains(c));
            let secretish = core.len() >= 12
                && (core.starts_with("sk-") || core.starts_with("AIza") || core.starts_with("hf_")
                    || core.starts_with("Bearer")
                    || (core.len() > 32 && core.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')));
            if secretish { tok.replacen(core, "[redacted]", 1) } else { tok.to_string() }
        })
        .collect()
}

// OpenAI tool-call arguments arrive as a JSON *string*; mark unparseable ones so
// the agent can tell the model instead of silently running with empty fields.
fn parse_args(args: &str) -> Value {
    if args.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str(args).unwrap_or_else(|_| json!({ crate::tools::INVALID_ARGS: args }))
}

// Iterate `data:` payloads of an SSE stream, handing each JSON value to `f`.
// `f` returns true to stop early (e.g. on [DONE]). Takes any reader so the
// streaming parsers are unit-testable against an in-memory byte slice.
fn for_each_sse(reader: impl Read, mut f: impl FnMut(&str) -> bool) {
    // Reuse one buffer across lines instead of allocating a String per line.
    let mut reader = BufReader::with_capacity(32 * 1024, reader);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if let Some(payload) = line.strip_prefix("data:") {
                    if f(payload.trim()) {
                        break;
                    }
                }
            }
        }
    }
}

// ── Anthropic Messages ──────────────────────────────────────────────────────
// Pure request-body builder, split out from the HTTP wiring so the JSON shape
// (system caching, tool schema mapping, tool_use/tool_result, the last-message
// cache breakpoint) is unit-testable without constructing a `ureq::Request`.
fn anthropic_body(model: &str, msgs: &[Msg], tools: &[ToolDef]) -> Value {
    let mut system = String::new();
    let mut messages: Vec<Value> = Vec::new();
    for m in msgs {
        match m {
            Msg::System(s) => {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(s);
            }
            Msg::User(t) => messages.push(json!({"role": "user", "content": t})),
            Msg::Assistant { text, calls } => {
                let mut content: Vec<Value> = Vec::new();
                if !text.is_empty() {
                    content.push(json!({"type": "text", "text": text}));
                }
                for c in calls {
                    content.push(json!({"type": "tool_use", "id": c.id, "name": c.name, "input": c.input}));
                }
                messages.push(json!({"role": "assistant", "content": content}));
            }
            Msg::Tool(results) => {
                let content: Vec<Value> = results.iter().map(|r| json!({
                    "type": "tool_result", "tool_use_id": r.id,
                    "content": r.content, "is_error": r.is_error
                })).collect();
                messages.push(json!({"role": "user", "content": content}));
            }
        }
    }

    // Prompt caching: render order is tools → system → messages, so a breakpoint
    // on the system block caches the stable tools+system prefix, and one on the
    // last message caches the growing conversation. Each ReAct step then reads the
    // prior turns from cache — much lower time-to-first-token on multi-step tasks.
    cache_last_message(&mut messages);

    let mut body = json!({"model": model, "max_tokens": 4096, "messages": messages});
    if !system.is_empty() {
        body["system"] = json!([{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}]);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools.iter().map(|t| json!({
            "name": t.name, "description": t.description, "input_schema": t.schema
        })).collect::<Vec<_>>());
    }
    body
}

fn anthropic_request(p: &Provider, msgs: &[Msg], tools: &[ToolDef]) -> Result<(ureq::Request, Value), String> {
    let body = anthropic_body(&p.model, msgs, tools);
    let key = p.api_key.as_deref().ok_or("Anthropic requires an API key")?;
    let req = agent().post(&url(p, "/v1/messages"))
        .set("x-api-key", key)
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json");
    Ok((req, body))
}

// Put an ephemeral cache breakpoint on the last content block of the last
// message, normalizing a string body into a single text block if needed.
fn cache_last_message(messages: &mut [Value]) {
    let Some(last) = messages.last_mut() else { return };
    let content = &mut last["content"];
    if let Some(text) = content.as_str() {
        let text = text.to_string();
        *content = json!([{"type": "text", "text": text, "cache_control": {"type": "ephemeral"}}]);
    } else if let Some(arr) = content.as_array_mut() {
        if let Some(block) = arr.last_mut() {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
    }
}

fn anthropic_parse(v: Value) -> Result<Reply, String> {
    let mut text = String::new();
    let mut calls = Vec::new();
    if let Some(blocks) = v["content"].as_array() {
        for b in blocks {
            match b["type"].as_str() {
                Some("text") => text.push_str(b["text"].as_str().unwrap_or_default()),
                Some("tool_use") => calls.push(ToolCall {
                    id: b["id"].as_str().unwrap_or_default().to_string(),
                    name: b["name"].as_str().unwrap_or_default().to_string(),
                    input: b["input"].clone(),
                }),
                _ => {}
            }
        }
    }
    Ok(Reply { text, calls })
}

fn anthropic_stream(reader: impl Read, on_text: &mut dyn FnMut(&str)) -> Result<Reply, String> {
    let mut text = String::new();
    // index → (id, name, accumulated input JSON)
    let mut pending: Vec<(usize, String, String, String)> = Vec::new();
    for_each_sse(reader, |data| {
        let Ok(v) = serde_json::from_str::<Value>(data) else { return false };
        match v["type"].as_str() {
            Some("content_block_start") => {
                let idx = v["index"].as_u64().unwrap_or(0) as usize;
                let cb = &v["content_block"];
                if cb["type"].as_str() == Some("tool_use") {
                    pending.push((idx,
                        cb["id"].as_str().unwrap_or_default().to_string(),
                        cb["name"].as_str().unwrap_or_default().to_string(),
                        String::new()));
                }
            }
            Some("content_block_delta") => {
                let d = &v["delta"];
                match d["type"].as_str() {
                    Some("text_delta") => {
                        let t = d["text"].as_str().unwrap_or_default();
                        text.push_str(t);
                        on_text(t);
                    }
                    Some("input_json_delta") => {
                        let idx = v["index"].as_u64().unwrap_or(0) as usize;
                        if let Some(e) = pending.iter_mut().find(|e| e.0 == idx) {
                            e.3.push_str(d["partial_json"].as_str().unwrap_or_default());
                        }
                    }
                    _ => {}
                }
            }
            Some("message_stop") => return true,
            _ => {}
        }
        false
    });
    let calls = pending.into_iter().map(|(_, id, name, args)| ToolCall {
        id, name, input: serde_json::from_str(&args).unwrap_or_else(|_| json!({})),
    }).collect();
    Ok(Reply { text, calls })
}

// ── OpenAI-compatible /chat/completions ─────────────────────────────────────
// Pure body builder (see `anthropic_body` for the rationale).
fn openai_body(model: &str, msgs: &[Msg], tools: &[ToolDef]) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    for m in msgs {
        match m {
            Msg::System(s) => messages.push(json!({"role": "system", "content": s})),
            Msg::User(t) => messages.push(json!({"role": "user", "content": t})),
            Msg::Assistant { text, calls } => {
                let mut msg = json!({"role": "assistant", "content": text});
                if !calls.is_empty() {
                    msg["tool_calls"] = json!(calls.iter().map(|c| json!({
                        "id": c.id, "type": "function",
                        "function": {"name": c.name, "arguments": c.input.to_string()}
                    })).collect::<Vec<_>>());
                }
                messages.push(msg);
            }
            Msg::Tool(results) => {
                for r in results {
                    messages.push(json!({"role": "tool", "tool_call_id": r.id, "content": r.content}));
                }
            }
        }
    }

    let mut body = json!({"model": model, "messages": messages});
    if !tools.is_empty() {
        body["tools"] = json!(tools.iter().map(|t| json!({
            "type": "function",
            "function": {"name": t.name, "description": t.description, "parameters": t.schema}
        })).collect::<Vec<_>>());
    }
    body
}

fn openai_request(p: &Provider, msgs: &[Msg], tools: &[ToolDef]) -> (ureq::Request, Value) {
    let body = openai_body(&p.model, msgs, tools);
    let mut req = agent().post(&url(p, "/chat/completions")).set("content-type", "application/json");
    if let Some(key) = &p.api_key {
        req = req.set("authorization", &format!("Bearer {key}"));
    }
    (req, body)
}

fn openai_parse(v: Value) -> Result<Reply, String> {
    let msg = &v["choices"][0]["message"];
    let text = msg["content"].as_str().unwrap_or_default().to_string();
    let mut calls = Vec::new();
    if let Some(tcs) = msg["tool_calls"].as_array() {
        for tc in tcs {
            let args = tc["function"]["arguments"].as_str().unwrap_or("{}");
            calls.push(ToolCall {
                id: tc["id"].as_str().unwrap_or_default().to_string(),
                name: tc["function"]["name"].as_str().unwrap_or_default().to_string(),
                input: parse_args(args),
            });
        }
    }
    Ok(Reply { text, calls })
}

fn openai_stream(reader: impl Read, on_text: &mut dyn FnMut(&str)) -> Result<Reply, String> {
    let mut text = String::new();
    // index → (id, name, accumulated args)
    let mut pending: Vec<(String, String, String)> = Vec::new();
    for_each_sse(reader, |data| {
        if data == "[DONE]" {
            return true;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { return false };
        let delta = &v["choices"][0]["delta"];
        if let Some(t) = delta["content"].as_str() {
            text.push_str(t);
            on_text(t);
        }
        if let Some(tcs) = delta["tool_calls"].as_array() {
            for tc in tcs {
                let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                while pending.len() <= idx {
                    pending.push((String::new(), String::new(), String::new()));
                }
                let e = &mut pending[idx];
                if let Some(id) = tc["id"].as_str() {
                    if !id.is_empty() { e.0 = id.to_string(); }
                }
                if let Some(name) = tc["function"]["name"].as_str() {
                    if !name.is_empty() { e.1 = name.to_string(); }
                }
                if let Some(args) = tc["function"]["arguments"].as_str() {
                    e.2.push_str(args);
                }
            }
        }
        false
    });
    let calls = pending.into_iter().filter(|e| !e.1.is_empty()).map(|(id, name, args)| ToolCall {
        id, name, input: parse_args(&args),
    }).collect();
    Ok(Reply { text, calls })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn tc(id: &str, name: &str, input: Value) -> ToolCall {
        ToolCall { id: id.into(), name: name.into(), input }
    }

    // ── redact ──────────────────────────────────────────────────────────────
    #[test]
    fn redact_blanks_known_key_prefixes() {
        // Prefix tokens must clear the 12-char length floor to be flagged.
        assert!(redact("error: sk-ABCDEFGHIJKLMNOP failed").contains("[redacted]"));
        assert!(redact("token AIzaSyABCDEFGHIJKL bad").contains("[redacted]"));
        assert!(redact("hf_ABCDEFGHIJKLMNOP nope").contains("[redacted]"));
    }

    #[test]
    fn redact_blanks_long_opaque_tokens() {
        // A whitespace-delimited token of >32 alnum chars is treated as secretish.
        let long = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEF"; // 42 alnum
        assert!(redact(&format!("token {long} end")).contains("[redacted]"));
        // The surrounding words survive.
        let out = redact(&format!("token {long} end"));
        assert!(out.starts_with("token ") && out.ends_with(" end"));
    }

    #[test]
    fn redact_leaves_ordinary_text_untouched() {
        let msg = "the quick brown fox jumps over the lazy dog";
        assert_eq!(redact(msg), msg);
        // Short sk- prefixed words are below the length floor and survive.
        assert_eq!(redact("sk-short"), "sk-short");
    }

    #[test]
    fn redact_preserves_punctuation_around_secret() {
        let out = redact("(\"sk-ABCDEFGHIJKLMNOP\")");
        assert!(out.contains("[redacted]"));
        assert!(out.starts_with("(\"") && out.ends_with("\")"));
    }

    #[test]
    fn redact_empty_string() {
        assert_eq!(redact(""), "");
    }

    // ── parse_args ──────────────────────────────────────────────────────────
    #[test]
    fn parse_args_empty_is_object() {
        assert_eq!(parse_args(""), json!({}));
        assert_eq!(parse_args("   "), json!({}));
    }

    #[test]
    fn parse_args_valid_json() {
        assert_eq!(parse_args(r#"{"a":1}"#), json!({"a": 1}));
    }

    #[test]
    fn parse_args_invalid_is_flagged() {
        let v = parse_args("not json");
        assert_eq!(v[crate::tools::INVALID_ARGS], json!("not json"));
    }

    #[test]
    fn parse_args_partial_json_is_flagged() {
        let v = parse_args(r#"{"a":"#);
        assert_eq!(v[crate::tools::INVALID_ARGS], json!(r#"{"a":"#));
    }

    // ── cache_last_message ──────────────────────────────────────────────────
    #[test]
    fn cache_last_message_empty_is_noop() {
        let mut m: Vec<Value> = vec![];
        cache_last_message(&mut m);
        assert!(m.is_empty());
    }

    #[test]
    fn cache_last_message_string_body_becomes_text_block() {
        let mut m = vec![json!({"role": "user", "content": "hi"})];
        cache_last_message(&mut m);
        let block = &m[0]["content"][0];
        assert_eq!(block["type"], "text");
        assert_eq!(block["text"], "hi");
        assert_eq!(block["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cache_last_message_array_body_marks_last_block() {
        let mut m = vec![json!({"role": "user", "content": [
            {"type": "text", "text": "a"},
            {"type": "text", "text": "b"}
        ]})];
        cache_last_message(&mut m);
        assert!(m[0]["content"][0].get("cache_control").is_none());
        assert_eq!(m[0]["content"][1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cache_last_message_only_touches_last() {
        let mut m = vec![
            json!({"role": "user", "content": "first"}),
            json!({"role": "user", "content": "second"}),
        ];
        cache_last_message(&mut m);
        assert!(m[0]["content"].is_string());
        assert_eq!(m[1]["content"][0]["cache_control"]["type"], "ephemeral");
    }

    // ── anthropic_body ──────────────────────────────────────────────────────
    #[test]
    fn anthropic_body_merges_system_blocks() {
        let msgs = vec![
            Msg::System("one".into()),
            Msg::System("two".into()),
            Msg::User("hello".into()),
        ];
        let b = anthropic_body("m", &msgs, &[]);
        assert_eq!(b["system"][0]["text"], "one\n\ntwo");
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["max_tokens"], 4096);
    }

    #[test]
    fn anthropic_body_no_system_no_tools_keys() {
        let b = anthropic_body("m", &[Msg::User("hi".into())], &[]);
        assert!(b.get("system").is_none());
        assert!(b.get("tools").is_none());
    }

    #[test]
    fn anthropic_body_assistant_text_and_calls() {
        let msgs = vec![Msg::Assistant {
            text: "thinking".into(),
            calls: vec![tc("t1", "read_file", json!({"path": "a"}))],
        }];
        let b = anthropic_body("m", &msgs, &[]);
        let content = &b["messages"][0]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["id"], "t1");
        assert_eq!(content[1]["name"], "read_file");
    }

    #[test]
    fn anthropic_body_empty_assistant_text_omits_text_block() {
        let msgs = vec![Msg::Assistant {
            text: String::new(),
            calls: vec![tc("t1", "x", json!({}))],
        }];
        let b = anthropic_body("m", &msgs, &[]);
        assert_eq!(b["messages"][0]["content"][0]["type"], "tool_use");
    }

    #[test]
    fn anthropic_body_tool_results() {
        let msgs = vec![Msg::Tool(vec![ToolResult {
            id: "t1".into(), content: "ok".into(), is_error: false,
        }])];
        let b = anthropic_body("m", &msgs, &[]);
        let block = &b["messages"][0]["content"][0];
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["tool_use_id"], "t1");
        assert_eq!(block["is_error"], false);
    }

    #[test]
    fn anthropic_body_maps_tool_schemas() {
        let tools = crate::tools::defs(false);
        let b = anthropic_body("m", &[Msg::User("hi".into())], &tools);
        assert!(b["tools"].as_array().unwrap().len() == tools.len());
        assert!(b["tools"][0].get("input_schema").is_some());
    }

    // ── openai_body ─────────────────────────────────────────────────────────
    #[test]
    fn openai_body_roles() {
        let msgs = vec![
            Msg::System("sys".into()),
            Msg::User("u".into()),
        ];
        let b = openai_body("m", &msgs, &[]);
        assert_eq!(b["messages"][0]["role"], "system");
        assert_eq!(b["messages"][1]["role"], "user");
    }

    #[test]
    fn openai_body_assistant_serializes_tool_calls_as_string_args() {
        let msgs = vec![Msg::Assistant {
            text: "x".into(),
            calls: vec![tc("c1", "run_command", json!({"cmd": "ls"}))],
        }];
        let b = openai_body("m", &msgs, &[]);
        let call = &b["messages"][0]["tool_calls"][0];
        assert_eq!(call["function"]["name"], "run_command");
        // arguments must be a JSON *string*, per the OpenAI shape.
        assert!(call["function"]["arguments"].is_string());
        assert_eq!(call["function"]["arguments"], json!({"cmd": "ls"}).to_string());
    }

    #[test]
    fn openai_body_tool_results_each_become_a_message() {
        let msgs = vec![Msg::Tool(vec![
            ToolResult { id: "a".into(), content: "1".into(), is_error: false },
            ToolResult { id: "b".into(), content: "2".into(), is_error: true },
        ])];
        let b = openai_body("m", &msgs, &[]);
        assert_eq!(b["messages"].as_array().unwrap().len(), 2);
        assert_eq!(b["messages"][0]["role"], "tool");
        assert_eq!(b["messages"][1]["tool_call_id"], "b");
    }

    #[test]
    fn openai_body_tool_schema_wraps_function() {
        let tools = crate::tools::defs(false);
        let b = openai_body("m", &[Msg::User("hi".into())], &tools);
        assert_eq!(b["tools"][0]["type"], "function");
        assert!(b["tools"][0]["function"].get("parameters").is_some());
    }

    // ── openai_stream ───────────────────────────────────────────────────────
    fn drain_openai(sse: &str) -> Reply {
        let mut out = String::new();
        openai_stream(Cursor::new(sse.as_bytes().to_vec()), &mut |t| out.push_str(t)).unwrap()
    }

    #[test]
    fn openai_stream_accumulates_text_and_stops_on_done() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\
                   data: [DONE]\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"AFTER\"}}]}\n";
        let r = drain_openai(sse);
        assert_eq!(r.text, "Hello");
    }

    #[test]
    fn openai_stream_assembles_fragmented_tool_call() {
        let sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"edit_file\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"a\\\"}\"}}]}}]}\n\
                   data: [DONE]\n";
        let r = drain_openai(sse);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].id, "c1");
        assert_eq!(r.calls[0].name, "edit_file");
        assert_eq!(r.calls[0].input, json!({"path": "a"}));
    }

    #[test]
    fn openai_stream_multiple_tool_calls_by_index() {
        let sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"f0\",\"arguments\":\"{}\"}}]}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"b\",\"function\":{\"name\":\"f1\",\"arguments\":\"{}\"}}]}}]}\n\
                   data: [DONE]\n";
        let r = drain_openai(sse);
        assert_eq!(r.calls.len(), 2);
        assert_eq!(r.calls[0].name, "f0");
        assert_eq!(r.calls[1].name, "f1");
    }

    #[test]
    fn openai_stream_ignores_blank_and_malformed_lines() {
        let sse = "\n\
                   data: not-json\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\
                   : comment\n\
                   data: [DONE]\n";
        let r = drain_openai(sse);
        assert_eq!(r.text, "ok");
    }

    #[test]
    fn openai_stream_invalid_args_flagged() {
        let sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"f\",\"arguments\":\"{bad\"}}]}}]}\n\
                   data: [DONE]\n";
        let r = drain_openai(sse);
        assert_eq!(r.calls[0].input[crate::tools::INVALID_ARGS], json!("{bad"));
    }

    #[test]
    fn openai_stream_empty_stream_is_empty_reply() {
        let r = drain_openai("");
        assert!(r.text.is_empty());
        assert!(r.calls.is_empty());
    }

    // ── anthropic_stream ────────────────────────────────────────────────────
    fn drain_anthropic(sse: &str) -> Reply {
        let mut out = String::new();
        anthropic_stream(Cursor::new(sse.as_bytes().to_vec()), &mut |t| out.push_str(t)).unwrap()
    }

    #[test]
    fn anthropic_stream_text_deltas() {
        let sse = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n\
                   data: {\"type\":\"message_stop\"}\n";
        let r = drain_anthropic(sse);
        assert_eq!(r.text, "Hi there");
    }

    #[test]
    fn anthropic_stream_assembles_input_json_delta() {
        let sse = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"read_file\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\"\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\":\\\"x\\\"}\"}}\n\
                   data: {\"type\":\"message_stop\"}\n";
        let r = drain_anthropic(sse);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].id, "t1");
        assert_eq!(r.calls[0].name, "read_file");
        assert_eq!(r.calls[0].input, json!({"path": "x"}));
    }

    #[test]
    fn anthropic_stream_bad_input_json_falls_back_to_empty_object() {
        let sse = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"x\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{not\"}}\n\
                   data: {\"type\":\"message_stop\"}\n";
        let r = drain_anthropic(sse);
        assert_eq!(r.calls[0].input, json!({}));
    }

    #[test]
    fn anthropic_stream_interleaved_text_and_tool() {
        let sse = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"go\"}}\n\
                   data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"f\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\
                   data: {\"type\":\"message_stop\"}\n";
        let r = drain_anthropic(sse);
        assert_eq!(r.text, "go");
        assert_eq!(r.calls.len(), 1);
    }

    #[test]
    fn anthropic_stream_ignores_unknown_events() {
        let sse = "data: {\"type\":\"ping\"}\n\
                   data: {\"type\":\"message_start\",\"message\":{}}\n\
                   data: {\"type\":\"message_stop\"}\n";
        let r = drain_anthropic(sse);
        assert!(r.text.is_empty());
        assert!(r.calls.is_empty());
    }

    #[test]
    fn anthropic_parse_extracts_text_and_calls() {
        let v = json!({"content": [
            {"type": "text", "text": "hello"},
            {"type": "tool_use", "id": "t1", "name": "read_file", "input": {"path": "a"}}
        ]});
        let r = anthropic_parse(v).unwrap();
        assert_eq!(r.text, "hello");
        assert_eq!(r.calls[0].name, "read_file");
    }

    #[test]
    fn openai_parse_handles_missing_tool_calls() {
        let v = json!({"choices": [{"message": {"content": "hi"}}]});
        let r = openai_parse(v).unwrap();
        assert_eq!(r.text, "hi");
        assert!(r.calls.is_empty());
    }

    #[test]
    fn openai_parse_defaults_missing_args_to_empty_object() {
        let v = json!({"choices": [{"message": {"content": "", "tool_calls": [
            {"id": "c1", "function": {"name": "f"}}
        ]}}]});
        let r = openai_parse(v).unwrap();
        assert_eq!(r.calls[0].input, json!({}));
    }
}
