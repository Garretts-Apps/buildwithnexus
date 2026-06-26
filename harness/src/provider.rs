// The model wire layer. One enum, two `match` arms — Anthropic Messages and the
// OpenAI /chat/completions shape (which also covers Ollama, llama.cpp, LM Studio,
// OpenRouter, Groq, and Hugging Face). No trait objects: the call site matches.

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

pub fn complete(p: &Provider, msgs: &[Msg], tools: &[ToolDef]) -> Result<Reply, String> {
    match p.protocol {
        Protocol::Anthropic => anthropic(p, msgs, tools),
        Protocol::OpenAi => openai(p, msgs, tools),
    }
}

// Surface non-2xx bodies — model APIs put the actionable error there.
fn send(req: ureq::Request, body: Value) -> Result<Value, String> {
    match req.send_json(body) {
        Ok(resp) => resp.into_json::<Value>().map_err(|e| format!("bad JSON from server: {e}")),
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp.into_string().unwrap_or_default();
            Err(format!("HTTP {code}: {}", detail.chars().take(400).collect::<String>()))
        }
        Err(e) => Err(format!("connection failed: {e}")),
    }
}

// ── Anthropic Messages ────────────────────────────────────────────────────
fn anthropic(p: &Provider, msgs: &[Msg], tools: &[ToolDef]) -> Result<Reply, String> {
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

    let tool_schema: Vec<Value> = tools.iter().map(|t| json!({
        "name": t.name, "description": t.description, "input_schema": t.schema
    })).collect();

    let mut body = json!({
        "model": p.model, "max_tokens": 4096, "messages": messages,
    });
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if !tool_schema.is_empty() {
        body["tools"] = json!(tool_schema);
    }

    let key = p.api_key.as_deref().ok_or("Anthropic requires an API key")?;
    let req = agent()
        .post(&format!("{}/v1/messages", p.base_url.trim_end_matches('/')))
        .set("x-api-key", key)
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json");

    let v = send(req, body)?;
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

// ── OpenAI-compatible /chat/completions ───────────────────────────────────
fn openai(p: &Provider, msgs: &[Msg], tools: &[ToolDef]) -> Result<Reply, String> {
    let mut messages: Vec<Value> = Vec::new();
    for m in msgs {
        match m {
            Msg::System(s) => messages.push(json!({"role": "system", "content": s})),
            Msg::User(t) => messages.push(json!({"role": "user", "content": t})),
            Msg::Assistant { text, calls } => {
                let mut msg = json!({"role": "assistant", "content": text});
                if !calls.is_empty() {
                    let tc: Vec<Value> = calls.iter().map(|c| json!({
                        "id": c.id, "type": "function",
                        "function": {"name": c.name, "arguments": c.input.to_string()}
                    })).collect();
                    msg["tool_calls"] = json!(tc);
                }
                messages.push(msg);
            }
            Msg::Tool(results) => {
                for r in results {
                    messages.push(json!({
                        "role": "tool", "tool_call_id": r.id, "content": r.content
                    }));
                }
            }
        }
    }

    let tool_schema: Vec<Value> = tools.iter().map(|t| json!({
        "type": "function",
        "function": {"name": t.name, "description": t.description, "parameters": t.schema}
    })).collect();

    let mut body = json!({"model": p.model, "messages": messages});
    if !tool_schema.is_empty() {
        body["tools"] = json!(tool_schema);
    }

    let url = format!("{}/chat/completions", p.base_url.trim_end_matches('/'));
    let mut req = agent().post(&url).set("content-type", "application/json");
    if let Some(key) = &p.api_key {
        req = req.set("authorization", &format!("Bearer {key}"));
    }

    let v = send(req, body)?;
    let msg = &v["choices"][0]["message"];
    let text = msg["content"].as_str().unwrap_or_default().to_string();
    let mut calls = Vec::new();
    if let Some(tcs) = msg["tool_calls"].as_array() {
        for tc in tcs {
            let args = tc["function"]["arguments"].as_str().unwrap_or("{}");
            let input = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
            calls.push(ToolCall {
                id: tc["id"].as_str().unwrap_or_default().to_string(),
                name: tc["function"]["name"].as_str().unwrap_or_default().to_string(),
                input,
            });
        }
    }
    Ok(Reply { text, calls })
}
