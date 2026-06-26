// The model wire layer. One enum, two `match` arms — Anthropic Messages and the
// OpenAI /chat/completions shape (which also covers Ollama, llama.cpp, LM Studio,
// OpenRouter, Groq, and Hugging Face). No trait objects: the call site matches.
//
// Each protocol has a body builder + a request builder, shared by the blocking
// `complete()` and the streaming `stream()` so there's exactly one place that
// knows each vendor's JSON shape.

use std::io::{BufRead, BufReader};
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Protocol;
use crate::tools::ToolDef;

pub struct Provider {
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
}

// Neutral conversation. The provider translates this into each vendor's shape;
// the rest of the program never sees vendor JSON.
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}
pub struct ToolResult {
    pub id: String,
    pub content: String,
    pub is_error: bool,
}
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
            .timeout(Duration::from_secs(600))
            .build()
    })
}

fn url(p: &Provider, path: &str) -> String {
    format!("{}{}", p.base_url.trim_end_matches('/'), path)
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
                anthropic_stream(send_raw(req, body)?, on_text)
            } else {
                anthropic_parse(send(req, body)?)
            }
        }
        Protocol::OpenAi => {
            let (req, mut body) = openai_request(p, msgs, tools);
            if streaming {
                body["stream"] = json!(true);
                openai_stream(send_raw(req, body)?, on_text)
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
            Err(format!("HTTP {code}: {}", detail.chars().take(400).collect::<String>()))
        }
        Err(e) => Err(format!("connection failed: {e}")),
    }
}

// Iterate `data:` payloads of an SSE stream, handing each JSON value to `f`.
// `f` returns true to stop early (e.g. on [DONE]).
fn for_each_sse(resp: ureq::Response, mut f: impl FnMut(&str) -> bool) {
    let reader = BufReader::new(resp.into_reader());
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Some(payload) = line.strip_prefix("data:") else { continue };
        if f(payload.trim()) {
            break;
        }
    }
}

// ── Anthropic Messages ──────────────────────────────────────────────────────
fn anthropic_request(p: &Provider, msgs: &[Msg], tools: &[ToolDef]) -> Result<(ureq::Request, Value), String> {
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

    let mut body = json!({"model": p.model, "max_tokens": 4096, "messages": messages});
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools.iter().map(|t| json!({
            "name": t.name, "description": t.description, "input_schema": t.schema
        })).collect::<Vec<_>>());
    }

    let key = p.api_key.as_deref().ok_or("Anthropic requires an API key")?;
    let req = agent().post(&url(p, "/v1/messages"))
        .set("x-api-key", key)
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json");
    Ok((req, body))
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

fn anthropic_stream(resp: ureq::Response, on_text: &mut dyn FnMut(&str)) -> Result<Reply, String> {
    let mut text = String::new();
    // index → (id, name, accumulated input JSON)
    let mut pending: Vec<(usize, String, String, String)> = Vec::new();
    for_each_sse(resp, |data| {
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
fn openai_request(p: &Provider, msgs: &[Msg], tools: &[ToolDef]) -> (ureq::Request, Value) {
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

    let mut body = json!({"model": p.model, "messages": messages});
    if !tools.is_empty() {
        body["tools"] = json!(tools.iter().map(|t| json!({
            "type": "function",
            "function": {"name": t.name, "description": t.description, "parameters": t.schema}
        })).collect::<Vec<_>>());
    }

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
                input: serde_json::from_str(args).unwrap_or_else(|_| json!({})),
            });
        }
    }
    Ok(Reply { text, calls })
}

fn openai_stream(resp: ureq::Response, on_text: &mut dyn FnMut(&str)) -> Result<Reply, String> {
    let mut text = String::new();
    // index → (id, name, accumulated args)
    let mut pending: Vec<(String, String, String)> = Vec::new();
    for_each_sse(resp, |data| {
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
        id, name, input: serde_json::from_str(&args).unwrap_or_else(|_| json!({})),
    }).collect();
    Ok(Reply { text, calls })
}
