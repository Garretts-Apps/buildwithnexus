// The model wire layer. One enum, three `match` arms — Anthropic Messages, the
// OpenAI /chat/completions shape (llama.cpp, LM Studio, OpenRouter, Groq, and
// Hugging Face), and Ollama's native /api/chat (which alone accepts num_ctx —
// the OpenAI-compat endpoint silently truncates prompts to the server default).
// No trait objects: the call site matches.
//
// Each protocol has a body builder + a request builder, shared by the blocking
// `complete()` and the streaming `stream()` so there's exactly one place that
// knows each vendor's JSON shape.

use std::io::{BufRead, BufReader, Read};
use std::time::{Duration, Instant};

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
    pub temperature: Option<f64>, // sampling temperature; None → per-protocol default
    pub max_tokens: Option<u32>, // response token cap; None → per-protocol default
    /// Ollama native only: cached /api/show probe result, filled once per
    /// session. `Some(n)` is the chosen `options.num_ctx`; `None` means the
    /// probe failed (older server) and requests fall back to the OpenAI-compat
    /// /v1 endpoint. A settings `context_tokens` override pre-seeds this cache.
    pub ollama_ctx: std::sync::OnceLock<Option<u32>>,
}

// Neutral conversation. The provider translates this into each vendor's shape;
// the rest of the program never sees vendor JSON. Serializable + cloneable so a
// transcript can be persisted to a session file and restored on resume.
#[derive(Clone, Debug, Serialize, Deserialize)]
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
    /// User message with one or more attached images (base64-encoded, with media type).
    /// Images are `(media_type, base64_data)` pairs, e.g. `("image/png", "iVBOR...")`.
    UserImages {
        text: String,
        images: Vec<(String, String)>,
    },
    Assistant {
        text: String,
        calls: Vec<ToolCall>,
    },
    Tool(Vec<ToolResult>),
}
#[derive(Debug, Default)]
pub struct Reply {
    pub text: String,
    pub calls: Vec<ToolCall>,
    /// Why the model stopped, normalized across protocols: "max_tokens"
    /// (OpenAI "length" and Ollama done_reason "length" map here), "end_turn",
    /// "tool_use" (OpenAI "tool_calls" maps here), "refusal", "stop", or None
    /// when the server didn't report one.
    pub stop_reason: Option<String>,
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
        Protocol::OllamaNative => format!("{}/api/tags", ollama_root(&p.base_url)),
    };
    let key = p.api_key.clone();
    let proto = p.protocol;
    std::thread::spawn(move || {
        let mut req = agent().get(&url).timeout(Duration::from_secs(5));
        req = match (proto, &key) {
            (Protocol::Anthropic, Some(k)) => req
                .set("x-api-key", k)
                .set("anthropic-version", "2023-06-01"),
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
    pub fn redact(s: &str) -> String {
        super::redact(s)
    }
    pub fn parse_args(s: &str) -> Value {
        super::parse_args(s)
    }
    pub fn cache_last_message(m: &mut [Value]) {
        super::cache_last_message(m)
    }
    pub fn anthropic_body(model: &str, msgs: &[Msg], tools: &[ToolDef]) -> Value {
        super::anthropic_body(model, msgs, tools, None)
    }
    pub fn openai_body(model: &str, msgs: &[Msg], tools: &[ToolDef]) -> Value {
        super::openai_body(model, msgs, tools, None, None)
    }
    pub fn openai_stream(
        reader: impl Read,
        on_text: &mut dyn FnMut(&str),
        on_thinking: &mut dyn FnMut(&str),
    ) -> Result<Reply, String> {
        super::openai_stream(reader, on_text, on_thinking)
    }
}

// Ollama's native API lives at the host root; tolerate both a bare host base
// (the current preset) and an OpenAI-style …/v1 base (older settings files,
// custom base_url overrides) by trimming a trailing /v1.
fn ollama_root(base_url: &str) -> &str {
    base_url.trim_end_matches('/').trim_end_matches("/v1")
}

// Installed models reported by a running Ollama (GET /api/tags). Empty on any
// failure (not running, wrong host, …).
pub fn ollama_models(base_url: &str) -> Vec<String> {
    let root = ollama_root(base_url);
    let resp = match agent()
        .get(&format!("{root}/api/tags"))
        .timeout(Duration::from_secs(2))
        .call()
    {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let v: Value = match resp.into_json() {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    v["models"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// ── public entry points ────────────────────────────────────────────────────
pub fn complete(p: &Provider, msgs: &[Msg], tools: &[ToolDef]) -> Result<Reply, String> {
    let mut sink = |_: &str| {};
    let mut noop = |_: &str| {};
    request(p, msgs, tools, false, &mut sink, &mut noop)
}

// Streams assistant text to `on_text` and thinking tokens (when available) to
// `on_thinking` as they arrive; tool calls are accumulated and returned on completion.
pub fn stream(
    p: &Provider,
    msgs: &[Msg],
    tools: &[ToolDef],
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) -> Result<Reply, String> {
    request(p, msgs, tools, true, on_text, on_thinking)
}

fn request(
    p: &Provider,
    msgs: &[Msg],
    tools: &[ToolDef],
    streaming: bool,
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) -> Result<Reply, String> {
    match p.protocol {
        Protocol::Anthropic => {
            let (req, mut body) = anthropic_request(p, msgs, tools)?;
            if streaming {
                body["stream"] = json!(true);
                anthropic_stream(send_raw(req, body)?.into_reader(), on_text, on_thinking)
            } else {
                anthropic_parse(send(req, body)?)
            }
        }
        Protocol::OpenAi => {
            let (req, mut body) = openai_request(p, msgs, tools);
            if streaming {
                body["stream"] = json!(true);
                openai_stream(send_raw(req, body)?.into_reader(), on_text, on_thinking)
            } else {
                openai_parse(send(req, body)?)
            }
        }
        Protocol::OllamaNative => {
            // The native path exists to set options.num_ctx (unavailable on
            // the OpenAI-compat endpoint, which silently truncates prompts)
            // and to neutralize the repeat_penalty=1.1 default. A failed
            // /api/show probe means an older server: fall back to the
            // {root}/v1 OpenAI-compat endpoint used before, with a one-time
            // heads-up about the context limitation.
            match ollama_ctx(p) {
                Some(num_ctx) => {
                    let (req, mut body) = ollama_request(p, msgs, tools, num_ctx);
                    if streaming {
                        body["stream"] = json!(true);
                        ollama_stream(send_raw(req, body)?.into_reader(), on_text, on_thinking)
                    } else {
                        ollama_parse(send(req, body)?)
                    }
                }
                None => {
                    warn_ollama_fallback();
                    let base = format!("{}/v1", ollama_root(&p.base_url));
                    let (req, mut body) = openai_request_at(p, &base, msgs, tools);
                    if streaming {
                        body["stream"] = json!(true);
                        openai_stream(send_raw(req, body)?.into_reader(), on_text, on_thinking)
                    } else {
                        openai_parse(send(req, body)?)
                    }
                }
            }
        }
    }
}

// ── HTTP plumbing ──────────────────────────────────────────────────────────
fn send(req: ureq::Request, body: Value) -> Result<Value, String> {
    send_raw(req, body)?
        .into_json::<Value>()
        .map_err(|e| format!("bad JSON from server: {e}"))
}

fn send_raw(req: ureq::Request, body: Value) -> Result<ureq::Response, String> {
    let mut attempts = 0;
    let max_attempts = 5;
    let mut delay_ms = 500;

    loop {
        attempts += 1;
        let req_clone = req.clone();
        let body_clone = body.clone();

        match req_clone.send_json(body_clone) {
            Ok(resp) => return Ok(resp),
            Err(ureq::Error::Status(code, resp)) => {
                // The status code alone decides retryability — the body is
                // never consulted, so an error message that happens to mention
                // "image" or "not supported" can't turn a transient 429/5xx
                // into a permanent failure.
                let is_transient = is_transient_status(code);
                let server_wait = retry_after_ms(resp.header("retry-after"));
                let detail = resp.into_string().unwrap_or_default();
                if is_transient && attempts < max_attempts {
                    let wait = server_wait.unwrap_or(delay_ms);
                    std::thread::sleep(Duration::from_millis(wait));
                    delay_ms = (delay_ms * 2).min(10_000);
                    continue;
                }
                return Err(format!(
                    "HTTP {code}: {}",
                    redact(&detail).chars().take(400).collect::<String>()
                ));
            }
            Err(e) => {
                if attempts < max_attempts {
                    std::thread::sleep(Duration::from_millis(delay_ms));
                    delay_ms = (delay_ms * 2).min(10_000);
                    continue;
                }
                return Err(format!("connection failed: {}", redact(&e.to_string())));
            }
        }
    }
}

// Retryable status codes: rate limits (429), Anthropic's overloaded (529), and
// transient 5xx. 501 Not Implemented is a permanent capability gap, not a blip.
fn is_transient_status(code: u16) -> bool {
    code == 429 || code == 529 || ((500..=504).contains(&code) && code != 501)
}

// Retry-After header (delta-seconds form) in milliseconds, capped at 60s. The
// HTTP-date form doesn't parse as an integer; callers fall back to backoff.
fn retry_after_ms(header: Option<&str>) -> Option<u64> {
    header
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|s| s.min(60) * 1000)
}

// Defense-in-depth: blank out anything that looks like an API key/token before
// surfacing an upstream error body to the user or logs.
fn redact(s: &str) -> String {
    s.split_inclusive(|c: char| c.is_whitespace() || "\"',:;()[]{}".contains(c))
        .map(|tok| {
            let core =
                tok.trim_end_matches(|c: char| c.is_whitespace() || "\"',:;()[]{}".contains(c));
            let secretish = core.len() >= 12
                && (core.starts_with("sk-")
                    || core.starts_with("AIza")
                    || core.starts_with("hf_")
                    || core.starts_with("Bearer")
                    || (core.len() > 32
                        && core
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')));
            if secretish {
                tok.replacen(core, "[redacted]", 1)
            } else {
                tok.to_string()
            }
        })
        .collect()
}

// Map vendor finish/stop reasons onto one shared vocabulary so the agent loop
// can branch without knowing which protocol produced the reply: OpenAI's
// "length" becomes "max_tokens" and "tool_calls" becomes "tool_use"; everything
// else ("end_turn", "stop", "refusal", …) passes through unchanged.
fn normalize_stop_reason(raw: &str) -> String {
    match raw {
        "length" => "max_tokens".to_string(),
        "tool_calls" => "tool_use".to_string(),
        other => other.to_string(),
    }
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
// A transport error mid-stream is surfaced as Err rather than treated as a
// clean end — otherwise a dropped connection silently truncates the reply.
// Natural EOF (or a terminal event before one) is still success.
fn for_each_sse(reader: impl Read, mut f: impl FnMut(&str) -> bool) -> Result<(), String> {
    // Reuse one buffer across lines instead of allocating a String per line.
    let mut reader = BufReader::with_capacity(32 * 1024, reader);
    let mut line = String::new();
    let mut last_poll = Instant::now();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(()),
            Err(e) => return Err(format!("stream read failed: {}", redact(&e.to_string()))),
            Ok(_) => {
                if last_poll.elapsed() >= Duration::from_millis(50) {
                    crate::tui::poll_typeahead();
                    last_poll = Instant::now();
                }
                if let Some(payload) = line.strip_prefix("data:") {
                    if f(payload.trim()) {
                        return Ok(());
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
fn anthropic_body(model: &str, msgs: &[Msg], tools: &[ToolDef], max_tokens: Option<u32>) -> Value {
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
            Msg::UserImages { text, images } => {
                let mut parts: Vec<Value> = images
                    .iter()
                    .map(|(mt, data)| {
                        json!({
                            "type": "image",
                            "source": {"type": "base64", "media_type": mt, "data": data}
                        })
                    })
                    .collect();
                parts.push(json!({"type": "text", "text": text}));
                messages.push(json!({"role": "user", "content": parts}));
            }
            Msg::Assistant { text, calls } => {
                let mut content: Vec<Value> = Vec::new();
                if !text.is_empty() {
                    content.push(json!({"type": "text", "text": text}));
                }
                for c in calls {
                    content.push(
                        json!({"type": "tool_use", "id": c.id, "name": c.name, "input": c.input}),
                    );
                }
                messages.push(json!({"role": "assistant", "content": content}));
            }
            Msg::Tool(results) => {
                let content: Vec<Value> = results
                    .iter()
                    .map(|r| {
                        json!({
                            "type": "tool_result", "tool_use_id": r.id,
                            "content": r.content, "is_error": r.is_error
                        })
                    })
                    .collect();
                messages.push(json!({"role": "user", "content": content}));
            }
        }
    }

    // Prompt caching: render order is tools → system → messages, so a breakpoint
    // on the system block caches the stable tools+system prefix, and one on the
    // last message caches the growing conversation. Each ReAct step then reads the
    // prior turns from cache — much lower time-to-first-token on multi-step tasks.
    cache_last_message(&mut messages);

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens.unwrap_or(8192),
        "messages": messages
    });
    if !system.is_empty() {
        body["system"] =
            json!([{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}]);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools
            .iter()
            .map(|t| json!({
                "name": t.name, "description": t.description, "input_schema": t.schema
            }))
            .collect::<Vec<_>>());
    }
    body
}

fn anthropic_request(
    p: &Provider,
    msgs: &[Msg],
    tools: &[ToolDef],
) -> Result<(ureq::Request, Value), String> {
    let body = anthropic_body(&p.model, msgs, tools, p.max_tokens);
    let key = p
        .api_key
        .as_deref()
        .ok_or("Anthropic requires an API key")?;
    let req = agent()
        .post(&url(p, "/v1/messages"))
        .set("x-api-key", key)
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json");
    Ok((req, body))
}

// Put an ephemeral cache breakpoint on the last content block of the last
// message, normalizing a string body into a single text block if needed.
fn cache_last_message(messages: &mut [Value]) {
    let Some(last) = messages.last_mut() else {
        return;
    };
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
    let stop_reason = v["stop_reason"].as_str().map(normalize_stop_reason);
    Ok(Reply {
        text: strip_think(&text),
        calls,
        stop_reason,
    })
}

fn anthropic_stream(
    reader: impl Read,
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) -> Result<Reply, String> {
    let mut text = String::new();
    let mut stop_reason: Option<String> = None;
    let mut stream_err: Option<String> = None;
    // index → (id, name, accumulated input JSON)
    let mut pending: Vec<(usize, String, String, String)> = Vec::new();
    for_each_sse(reader, |data| {
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return false;
        };
        match v["type"].as_str() {
            Some("content_block_start") => {
                let idx = v["index"].as_u64().unwrap_or(0) as usize;
                let cb = &v["content_block"];
                if cb["type"].as_str() == Some("tool_use") {
                    pending.push((
                        idx,
                        cb["id"].as_str().unwrap_or_default().to_string(),
                        cb["name"].as_str().unwrap_or_default().to_string(),
                        String::new(),
                    ));
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
                    Some("thinking_delta") => {
                        // Extended thinking tokens (claude-3-7+): surfaced separately so
                        // the caller can render them as internal monologue.
                        let t = d["thinking"].as_str().unwrap_or_default();
                        on_thinking(t);
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
            Some("message_delta") => {
                // Final metadata for the turn — carries the stop reason.
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    stop_reason = Some(normalize_stop_reason(r));
                }
            }
            Some("error") => {
                // Mid-stream server error (e.g. overloaded) — surface it so the
                // caller can retry or report instead of returning a truncated reply.
                stream_err = Some(format!(
                    "stream error: {}",
                    redact(&v["error"].to_string())
                        .chars()
                        .take(400)
                        .collect::<String>()
                ));
                return true;
            }
            Some("message_stop") => return true,
            _ => {}
        }
        false
    })?;
    if let Some(e) = stream_err {
        return Err(e);
    }
    // Same INVALID_ARGS flagging as the OpenAI path: a truncated tool call must
    // reach the agent loop as malformed, not silently execute with empty input.
    let calls = pending
        .into_iter()
        .map(|(_, id, name, args)| ToolCall {
            id,
            name,
            input: parse_args(&args),
        })
        .collect();
    Ok(Reply {
        text,
        calls,
        stop_reason,
    })
}

// ── OpenAI-compatible /chat/completions ─────────────────────────────────────
// Pure body builder (see `anthropic_body` for the rationale).
fn openai_body(
    model: &str,
    msgs: &[Msg],
    tools: &[ToolDef],
    temperature: Option<f64>,
    max_tokens: Option<u32>,
) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    let mut system = String::new();
    for m in msgs {
        if let Msg::System(s) = m {
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str(s);
        }
    }
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system}));
    }
    for m in msgs {
        match m {
            Msg::System(_) => {}
            Msg::User(t) => messages.push(json!({"role": "user", "content": t})),
            Msg::UserImages { text, images } => {
                let mut parts: Vec<Value> = images
                    .iter()
                    .map(|(mt, data)| {
                        json!({
                            "type": "image_url",
                            "image_url": {"url": format!("data:{mt};base64,{data}")}
                        })
                    })
                    .collect();
                parts.push(json!({"type": "text", "text": text}));
                messages.push(json!({"role": "user", "content": parts}));
            }
            Msg::Assistant { text, calls } => {
                let mut msg = json!({"role": "assistant", "content": text});
                if !calls.is_empty() {
                    msg["tool_calls"] = json!(calls
                        .iter()
                        .map(|c| json!({
                            "id": c.id, "type": "function",
                            "function": {"name": c.name, "arguments": c.input.to_string()}
                        }))
                        .collect::<Vec<_>>());
                }
                messages.push(msg);
            }
            Msg::Tool(results) => {
                for r in results {
                    messages
                        .push(json!({"role": "tool", "tool_call_id": r.id, "content": r.content}));
                }
            }
        }
    }

    let mut body = json!({
        "model": model,
        "messages": messages,
        "temperature": temperature.unwrap_or(0.2),
        "max_tokens": max_tokens.unwrap_or(4096)
    });
    if !tools.is_empty() {
        body["tools"] =
            json!(tools.iter().map(|t| json!({
            "type": "function",
            "function": {"name": t.name, "description": t.description, "parameters": t.schema}
        })).collect::<Vec<_>>());
    }
    body
}

fn openai_request(p: &Provider, msgs: &[Msg], tools: &[ToolDef]) -> (ureq::Request, Value) {
    openai_request_at(p, &p.base_url, msgs, tools)
}

// Same request against an explicit base URL — the Ollama fallback path targets
// {host}/v1 while the provider's base_url is the bare host root.
fn openai_request_at(
    p: &Provider,
    base: &str,
    msgs: &[Msg],
    tools: &[ToolDef],
) -> (ureq::Request, Value) {
    let body = openai_body(&p.model, msgs, tools, p.temperature, p.max_tokens);
    let mut req = agent()
        .post(&format!("{}/chat/completions", base.trim_end_matches('/')))
        .set("content-type", "application/json");
    if let Some(key) = &p.api_key {
        req = req.set("authorization", &format!("Bearer {key}"));
    }
    (req, body)
}

// Remove `<think>…</think>` reasoning blocks that local reasoning models
// (DeepSeek-R1 distills, Qwen thinking variants) emit inline in `content` on
// the non-streaming path. Handles multiple blocks and an unclosed `<think>`
// (drops the trailing leaked chain-of-thought). Text without think tags is
// returned untouched. The streaming path already routes these to on_thinking.
fn strip_think(text: &str) -> String {
    if !text.contains("<think>") {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "<think>".len()..];
        match after.find("</think>") {
            Some(end) => rest = &after[end + "</think>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

fn openai_parse(v: Value) -> Result<Reply, String> {
    let msg = &v["choices"][0]["message"];
    let text = strip_think(msg["content"].as_str().unwrap_or_default());
    let mut calls = Vec::new();
    if let Some(tcs) = msg["tool_calls"].as_array() {
        for tc in tcs {
            let args = tc["function"]["arguments"].as_str().unwrap_or("{}");
            calls.push(ToolCall {
                id: tc["id"].as_str().unwrap_or_default().to_string(),
                name: tc["function"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                input: parse_args(args),
            });
        }
    }
    let stop_reason = v["choices"][0]["finish_reason"]
        .as_str()
        .map(normalize_stop_reason);
    Ok(Reply {
        text,
        calls,
        stop_reason,
    })
}

fn route_openai_content(
    chunk: &str,
    in_think: &mut bool,
    text: &mut String,
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) {
    let mut rest = chunk;
    loop {
        if *in_think {
            if let Some(end) = rest.find("</think>") {
                let thought = &rest[..end];
                if !thought.is_empty() {
                    on_thinking(thought);
                }
                *in_think = false;
                rest = &rest[end + "</think>".len()..];
                if rest.is_empty() {
                    break;
                }
            } else {
                if !rest.is_empty() {
                    on_thinking(rest);
                }
                break;
            }
        } else if let Some(start) = rest.find("<think>") {
            let before = &rest[..start];
            if !before.is_empty() {
                text.push_str(before);
                on_text(before);
            }
            *in_think = true;
            rest = &rest[start + "<think>".len()..];
            if rest.is_empty() {
                break;
            }
        } else {
            if !rest.is_empty() {
                text.push_str(rest);
                on_text(rest);
            }
            break;
        }
    }
}

fn openai_stream(
    reader: impl Read,
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) -> Result<Reply, String> {
    let mut text = String::new();
    let mut in_think = false;
    let mut stop_reason: Option<String> = None;
    // index → (id, name, accumulated args)
    let mut pending: Vec<(String, String, String)> = Vec::new();
    for_each_sse(reader, |data| {
        if data == "[DONE]" {
            return true;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return false;
        };
        // finish_reason rides on the last content-bearing chunk (or one after).
        if let Some(r) = v["choices"][0]["finish_reason"].as_str() {
            stop_reason = Some(normalize_stop_reason(r));
        }
        let delta = &v["choices"][0]["delta"];
        if let Some(r) = delta["reasoning_content"]
            .as_str()
            .or_else(|| delta["reasoning"].as_str())
        {
            on_thinking(r);
        }
        if let Some(t) = delta["content"].as_str() {
            route_openai_content(t, &mut in_think, &mut text, on_text, on_thinking);
        }
        if let Some(tcs) = delta["tool_calls"].as_array() {
            for tc in tcs {
                let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                while pending.len() <= idx {
                    pending.push((String::new(), String::new(), String::new()));
                }
                let e = &mut pending[idx];
                if let Some(id) = tc["id"].as_str() {
                    if !id.is_empty() {
                        e.0 = id.to_string();
                    }
                }
                if let Some(name) = tc["function"]["name"].as_str() {
                    if !name.is_empty() {
                        e.1 = name.to_string();
                    }
                }
                if let Some(args) = tc["function"]["arguments"].as_str() {
                    e.2.push_str(args);
                }
            }
        }
        false
    })?;
    let calls = pending
        .into_iter()
        .filter(|e| !e.1.is_empty())
        .map(|(id, name, args)| ToolCall {
            id,
            name,
            input: parse_args(&args),
        })
        .collect();
    Ok(Reply {
        text,
        calls,
        stop_reason,
    })
}

// ── Ollama native /api/chat ─────────────────────────────────────────────────
// Used by the "ollama" preset. Two things the OpenAI-compat /v1 endpoint
// cannot express: options.num_ctx (without it Ollama silently truncates the
// prompt to the server-default window) and repeat_penalty=1.0 (the 1.1
// default corrupts tool-call JSON and code output).

// Hard cap on the requested window: larger allocations trade tokens/s and
// VRAM for headroom most local machines don't have.
const OLLAMA_CTX_CAP: u64 = 32_768;
// Assumed window when /api/show doesn't reveal the model max.
const OLLAMA_CTX_FLOOR: u32 = 8_192;

// Model max context from an /api/show response. The model_info key is
// architecture-prefixed ("llama.context_length", "qwen2.context_length", …),
// so scan for the ".context_length" suffix rather than guessing the arch.
fn show_context_length(v: &Value) -> Option<u64> {
    v["model_info"]
        .as_object()?
        .iter()
        .find(|(k, _)| k.ends_with(".context_length"))
        .and_then(|(_, val)| val.as_u64())
}

// A Modelfile `num_ctx`, if the model was created with one. /api/show's
// "parameters" field is a flat text blob, one "key value" pair per line.
fn show_num_ctx(v: &Value) -> Option<u64> {
    for line in v["parameters"].as_str()?.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() == Some("num_ctx") {
            return parts.next().and_then(|n| n.parse().ok());
        }
    }
    None
}

// Choose the num_ctx to request: an explicit Modelfile num_ctx wins (the user
// tuned it), else the model max capped at 32k, else the 8k floor when the max
// is unknown. Never exceeds a known model max.
fn ollama_pick_ctx(model_max: Option<u64>, modelfile_ctx: Option<u64>) -> u32 {
    let picked = match (modelfile_ctx, model_max) {
        (Some(n), Some(max)) => n.min(max),
        (Some(n), None) => n,
        (None, Some(max)) => max.min(OLLAMA_CTX_CAP),
        (None, None) => u64::from(OLLAMA_CTX_FLOOR),
    };
    u32::try_from(picked).unwrap_or(u32::MAX)
}

// Cached /api/show probe — queried once per Provider (the OnceLock). Returns
// the chosen num_ctx, or None when the probe failed (server down, or an
// Ollama old enough to lack /api/show); callers then use the OpenAI-compat
// fallback. lib.rs pre-seeds the cache when settings override the context.
pub fn ollama_ctx(p: &Provider) -> Option<u32> {
    *p.ollama_ctx.get_or_init(|| {
        let v: Value = agent()
            .post(&format!("{}/api/show", ollama_root(&p.base_url)))
            .timeout(Duration::from_secs(3))
            .send_json(json!({"model": p.model}))
            .ok()?
            .into_json()
            .ok()?;
        Some(ollama_pick_ctx(show_context_length(&v), show_num_ctx(&v)))
    })
}

// One-line heads-up, once per process, when falling back to /v1: without
// num_ctx the server may silently truncate long prompts.
fn warn_ollama_fallback() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        crate::report::notice(
            "ollama: /api/show unavailable; using the OpenAI-compat endpoint (long prompts may be truncated to the server-default context window)",
        );
    });
}

// Pure body builder (see `anthropic_body` for the rationale). Message roles
// mirror the OpenAI shape, but tool-call arguments are JSON *objects*, images
// are a bare base64 array on the user message, and sampling knobs nest under
// "options". "stream" defaults to true server-side, so it's always explicit.
fn ollama_body(
    model: &str,
    msgs: &[Msg],
    tools: &[ToolDef],
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    num_ctx: u32,
) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    let mut system = String::new();
    for m in msgs {
        if let Msg::System(s) = m {
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str(s);
        }
    }
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system}));
    }
    // Ollama tool-call messages carry no ids on the wire, so tool results are
    // threaded by name instead: map call ids to names while walking the
    // transcript, then stamp tool_name on each result.
    let mut call_names: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for m in msgs {
        match m {
            Msg::System(_) => {}
            Msg::User(t) => messages.push(json!({"role": "user", "content": t})),
            Msg::UserImages { text, images } => {
                // Raw base64 strings; the server sniffs the media type.
                let imgs: Vec<&str> = images.iter().map(|(_, data)| data.as_str()).collect();
                messages.push(json!({"role": "user", "content": text, "images": imgs}));
            }
            Msg::Assistant { text, calls } => {
                let mut msg = json!({"role": "assistant", "content": text});
                if !calls.is_empty() {
                    msg["tool_calls"] = json!(calls
                        .iter()
                        .map(|c| {
                            // arguments is an object here, not OpenAI's string.
                            json!({"function": {"name": c.name, "arguments": c.input}})
                        })
                        .collect::<Vec<_>>());
                    for c in calls {
                        call_names.insert(&c.id, &c.name);
                    }
                }
                messages.push(msg);
            }
            Msg::Tool(results) => {
                for r in results {
                    let mut msg = json!({"role": "tool", "content": r.content});
                    if let Some(name) = call_names.get(r.id.as_str()) {
                        msg["tool_name"] = json!(name);
                    }
                    messages.push(msg);
                }
            }
        }
    }

    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": false,
        "options": {
            // The reason the native path exists — see the section comment.
            "num_ctx": num_ctx,
            "temperature": temperature.unwrap_or(0.2),
            // Ollama's 1.1 default penalizes the repetition inherent in JSON
            // and code; 1.0 disables it.
            "repeat_penalty": 1.0,
            "num_predict": max_tokens.unwrap_or(4096),
        }
    });
    if !tools.is_empty() {
        // Same schema wrapper as the OpenAI shape.
        body["tools"] =
            json!(tools.iter().map(|t| json!({
            "type": "function",
            "function": {"name": t.name, "description": t.description, "parameters": t.schema}
        })).collect::<Vec<_>>());
    }
    body
}

fn ollama_request(
    p: &Provider,
    msgs: &[Msg],
    tools: &[ToolDef],
    num_ctx: u32,
) -> (ureq::Request, Value) {
    let body = ollama_body(&p.model, msgs, tools, p.temperature, p.max_tokens, num_ctx);
    // Local server: no auth header.
    let req = agent()
        .post(&format!("{}/api/chat", ollama_root(&p.base_url)))
        .set("content-type", "application/json");
    (req, body)
}

// Native Ollama sends tool-call arguments as an object; tolerate the string
// form some proxies emit by running it through the same INVALID_ARGS guard.
fn ollama_call_args(args: &Value) -> Value {
    match args {
        Value::String(s) => parse_args(s),
        Value::Null => json!({}),
        other => other.clone(),
    }
}

// Ollama assigns no tool-call ids; synthesize one from the call's position so
// results can be threaded back through the transcript.
fn ollama_call(i: usize, tc: &Value) -> ToolCall {
    let id = match tc["id"].as_str() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => format!("ollama-call-{i}"),
    };
    ToolCall {
        id,
        name: tc["function"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        input: ollama_call_args(&tc["function"]["arguments"]),
    }
}

fn ollama_parse(v: Value) -> Result<Reply, String> {
    let msg = &v["message"];
    let text = strip_think(msg["content"].as_str().unwrap_or_default());
    let mut calls = Vec::new();
    if let Some(tcs) = msg["tool_calls"].as_array() {
        for tc in tcs {
            calls.push(ollama_call(calls.len(), tc));
        }
    }
    // done_reason "length" normalizes to "max_tokens" like OpenAI's "length".
    let stop_reason = v["done_reason"].as_str().map(normalize_stop_reason);
    Ok(Reply {
        text,
        calls,
        stop_reason,
    })
}

// Iterate an NDJSON stream (one JSON object per line — Ollama's native
// streaming format, not SSE), handing each parsed value to `f`; `f` returns
// true to stop (the "done": true line). Same transport contract as
// `for_each_sse`: a mid-stream read error is Err, natural EOF is success.
fn for_each_ndjson(reader: impl Read, mut f: impl FnMut(&Value) -> bool) -> Result<(), String> {
    let mut reader = BufReader::with_capacity(32 * 1024, reader);
    let mut line = String::new();
    let mut last_poll = Instant::now();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(()),
            Err(e) => return Err(format!("stream read failed: {}", redact(&e.to_string()))),
            Ok(_) => {
                if last_poll.elapsed() >= Duration::from_millis(50) {
                    crate::tui::poll_typeahead();
                    last_poll = Instant::now();
                }
                let payload = line.trim();
                if payload.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(payload) {
                    if f(&v) {
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn ollama_stream(
    reader: impl Read,
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) -> Result<Reply, String> {
    let mut text = String::new();
    let mut in_think = false;
    let mut stop_reason: Option<String> = None;
    let mut stream_err: Option<String> = None;
    let mut calls: Vec<ToolCall> = Vec::new();
    for_each_ndjson(reader, |v| {
        // A mid-stream server error rides on an "error" field.
        if let Some(e) = v["error"].as_str() {
            stream_err = Some(format!(
                "stream error: {}",
                redact(e).chars().take(400).collect::<String>()
            ));
            return true;
        }
        let msg = &v["message"];
        if let Some(t) = msg["thinking"].as_str() {
            // Native thinking field (thinking-capable models); models without
            // it inline <think> tags in content, handled below.
            if !t.is_empty() {
                on_thinking(t);
            }
        }
        if let Some(t) = msg["content"].as_str() {
            if !t.is_empty() {
                route_openai_content(t, &mut in_think, &mut text, on_text, on_thinking);
            }
        }
        // Unlike OpenAI's fragmented deltas, each streamed tool call arrives
        // whole (arguments already a complete object) — append directly.
        if let Some(tcs) = msg["tool_calls"].as_array() {
            for tc in tcs {
                calls.push(ollama_call(calls.len(), tc));
            }
        }
        if v["done"].as_bool() == Some(true) {
            stop_reason = v["done_reason"].as_str().map(normalize_stop_reason);
            return true;
        }
        false
    })?;
    if let Some(e) = stream_err {
        return Err(e);
    }
    Ok(Reply {
        text,
        calls,
        stop_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn tc(id: &str, name: &str, input: Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        }
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

    // ── retry classification ────────────────────────────────────────────────
    #[test]
    fn transient_status_covers_rate_limits_and_5xx() {
        for code in [429, 500, 502, 503, 504, 529] {
            assert!(is_transient_status(code), "{code} should be transient");
        }
    }

    #[test]
    fn non_transient_status_includes_client_errors_and_501() {
        for code in [400, 401, 403, 404, 422, 501] {
            assert!(!is_transient_status(code), "{code} should not retry");
        }
    }

    #[test]
    fn retry_after_parses_seconds_and_caps_at_60s() {
        assert_eq!(retry_after_ms(Some("2")), Some(2_000));
        assert_eq!(retry_after_ms(Some(" 30 ")), Some(30_000));
        assert_eq!(retry_after_ms(Some("300")), Some(60_000));
        // HTTP-date form and garbage fall back to exponential backoff.
        assert_eq!(retry_after_ms(Some("Wed, 21 Oct 2026 07:28:00 GMT")), None);
        assert_eq!(retry_after_ms(None), None);
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
        let b = anthropic_body("m", &msgs, &[], None);
        assert_eq!(b["system"][0]["text"], "one\n\ntwo");
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["max_tokens"], 8192);
    }

    #[test]
    fn anthropic_body_max_tokens_default_and_override() {
        let msgs = vec![Msg::User("hi".into())];
        assert_eq!(anthropic_body("m", &msgs, &[], None)["max_tokens"], 8192);
        assert_eq!(
            anthropic_body("m", &msgs, &[], Some(1234))["max_tokens"],
            1234
        );
    }

    #[test]
    fn anthropic_body_no_system_no_tools_keys() {
        let b = anthropic_body("m", &[Msg::User("hi".into())], &[], None);
        assert!(b.get("system").is_none());
        assert!(b.get("tools").is_none());
    }

    #[test]
    fn anthropic_body_assistant_text_and_calls() {
        let msgs = vec![Msg::Assistant {
            text: "thinking".into(),
            calls: vec![tc("t1", "read_file", json!({"path": "a"}))],
        }];
        let b = anthropic_body("m", &msgs, &[], None);
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
        let b = anthropic_body("m", &msgs, &[], None);
        assert_eq!(b["messages"][0]["content"][0]["type"], "tool_use");
    }

    #[test]
    fn anthropic_body_tool_results() {
        let msgs = vec![Msg::Tool(vec![ToolResult {
            id: "t1".into(),
            content: "ok".into(),
            is_error: false,
        }])];
        let b = anthropic_body("m", &msgs, &[], None);
        let block = &b["messages"][0]["content"][0];
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["tool_use_id"], "t1");
        assert_eq!(block["is_error"], false);
    }

    #[test]
    fn anthropic_body_maps_tool_schemas() {
        let tools = crate::tools::defs(false);
        let b = anthropic_body("m", &[Msg::User("hi".into())], &tools, None);
        assert!(b["tools"].as_array().unwrap().len() == tools.len());
        assert!(b["tools"][0].get("input_schema").is_some());
    }

    // ── openai_body ─────────────────────────────────────────────────────────
    #[test]
    fn openai_body_roles() {
        let msgs = vec![Msg::System("sys".into()), Msg::User("u".into())];
        let b = openai_body("m", &msgs, &[], None, None);
        assert_eq!(b["messages"][0]["role"], "system");
        assert_eq!(b["messages"][1]["role"], "user");
    }

    #[test]
    fn openai_body_assistant_serializes_tool_calls_as_string_args() {
        let msgs = vec![Msg::Assistant {
            text: "x".into(),
            calls: vec![tc("c1", "run_command", json!({"cmd": "ls"}))],
        }];
        let b = openai_body("m", &msgs, &[], None, None);
        let call = &b["messages"][0]["tool_calls"][0];
        assert_eq!(call["function"]["name"], "run_command");
        // arguments must be a JSON *string*, per the OpenAI shape.
        assert!(call["function"]["arguments"].is_string());
        assert_eq!(
            call["function"]["arguments"],
            json!({"cmd": "ls"}).to_string()
        );
    }

    #[test]
    fn openai_body_tool_results_each_become_a_message() {
        let msgs = vec![Msg::Tool(vec![
            ToolResult {
                id: "a".into(),
                content: "1".into(),
                is_error: false,
            },
            ToolResult {
                id: "b".into(),
                content: "2".into(),
                is_error: true,
            },
        ])];
        let b = openai_body("m", &msgs, &[], None, None);
        assert_eq!(b["messages"].as_array().unwrap().len(), 2);
        assert_eq!(b["messages"][0]["role"], "tool");
        assert_eq!(b["messages"][1]["tool_call_id"], "b");
    }

    #[test]
    fn openai_body_sets_temperature_and_max_tokens_defaults() {
        let b = openai_body("m", &[Msg::User("hi".into())], &[], None, None);
        assert_eq!(b["temperature"], 0.2);
        assert_eq!(b["max_tokens"], 4096);
    }

    #[test]
    fn openai_body_honors_temperature_and_max_tokens_overrides() {
        let b = openai_body("m", &[Msg::User("hi".into())], &[], Some(0.7), Some(512));
        assert_eq!(b["temperature"], 0.7);
        assert_eq!(b["max_tokens"], 512);
    }

    #[test]
    fn openai_body_tool_schema_wraps_function() {
        let tools = crate::tools::defs(false);
        let b = openai_body("m", &[Msg::User("hi".into())], &tools, None, None);
        assert_eq!(b["tools"][0]["type"], "function");
        assert!(b["tools"][0]["function"].get("parameters").is_some());
    }

    // ── openai_stream ───────────────────────────────────────────────────────
    fn drain_openai(sse: &str) -> Reply {
        let mut out = String::new();
        openai_stream(
            Cursor::new(sse.as_bytes().to_vec()),
            &mut |t| out.push_str(t),
            &mut |_| {},
        )
        .unwrap()
    }

    #[test]
    fn openai_stream_extracts_reasoning_and_think_tags() {
        let sse = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"reasoning...\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"<think>deep thought\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"ing</think>Hello\"}}]}\n\
                   data: [DONE]\n";
        let mut text_out = String::new();
        let mut think_out = String::new();
        openai_stream(
            Cursor::new(sse.as_bytes().to_vec()),
            &mut |t| text_out.push_str(t),
            &mut |t| think_out.push_str(t),
        )
        .unwrap();
        assert_eq!(text_out, "Hello");
        assert_eq!(think_out, "reasoning...deep thoughting");
    }

    #[test]
    fn openai_stream_extracts_think_tag_when_single_chunk_contains_answer() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"<think>inspect first</think>Done\"}}]}\n\
                   data: [DONE]\n";
        let mut text_out = String::new();
        let mut think_out = String::new();
        openai_stream(
            Cursor::new(sse.as_bytes().to_vec()),
            &mut |t| text_out.push_str(t),
            &mut |t| think_out.push_str(t),
        )
        .unwrap();
        assert_eq!(text_out, "Done");
        assert_eq!(think_out, "inspect first");
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
        let mut _thinking = String::new();
        anthropic_stream(
            Cursor::new(sse.as_bytes().to_vec()),
            &mut |t| out.push_str(t),
            &mut |t| _thinking.push_str(t),
        )
        .unwrap()
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
    fn anthropic_stream_bad_input_json_is_flagged_invalid() {
        // A truncated/malformed streamed tool call must surface as INVALID_ARGS
        // so the agent loop reports it to the model instead of executing the
        // tool with a silently-empty input.
        let sse = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"x\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{not\"}}\n\
                   data: {\"type\":\"message_stop\"}\n";
        let r = drain_anthropic(sse);
        assert_eq!(r.calls[0].input[crate::tools::INVALID_ARGS], json!("{not"));
    }

    #[test]
    fn anthropic_stream_captures_stop_reason_from_message_delta() {
        let sse = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\
                   data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":9}}\n\
                   data: {\"type\":\"message_stop\"}\n";
        let r = drain_anthropic(sse);
        assert_eq!(r.stop_reason.as_deref(), Some("max_tokens"));
    }

    #[test]
    fn anthropic_stream_error_event_is_err() {
        let sse = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"par\"}}\n\
                   data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n";
        let r = anthropic_stream(
            Cursor::new(sse.as_bytes().to_vec()),
            &mut |_| {},
            &mut |_| {},
        );
        let err = r.unwrap_err();
        assert!(err.contains("stream error"), "got: {err}");
        assert!(err.contains("Overloaded"), "got: {err}");
    }

    // Yields its buffered bytes, then fails instead of reporting EOF —
    // simulates a connection dropped mid-stream.
    struct BrokenPipe(Cursor<Vec<u8>>);
    impl Read for BrokenPipe {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.0.read(buf)? {
                0 => Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "connection reset",
                )),
                n => Ok(n),
            }
        }
    }

    #[test]
    fn stream_read_error_before_terminal_event_is_err() {
        // No message_stop / [DONE] — the transport dies first.
        let a = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n";
        let r = anthropic_stream(
            BrokenPipe(Cursor::new(a.as_bytes().to_vec())),
            &mut |_| {},
            &mut |_| {},
        );
        assert!(r.unwrap_err().contains("stream read failed"));

        let o = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n";
        let r = openai_stream(
            BrokenPipe(Cursor::new(o.as_bytes().to_vec())),
            &mut |_| {},
            &mut |_| {},
        );
        assert!(r.unwrap_err().contains("stream read failed"));
    }

    #[test]
    fn stream_read_error_after_terminal_event_is_ok() {
        // The terminal event stops reading before the transport failure is seen.
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\
                   data: [DONE]\n";
        let r = openai_stream(
            BrokenPipe(Cursor::new(sse.as_bytes().to_vec())),
            &mut |_| {},
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(r.text, "Hi");
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

    // ── think-tag stripping ─────────────────────────────────────────────────
    #[test]
    fn strip_think_removes_blocks_and_keeps_answer() {
        assert_eq!(
            strip_think("<think>let me reason</think>The answer is 42."),
            "The answer is 42."
        );
        // Multiple blocks, with real content between them.
        assert_eq!(strip_think("<think>a</think>X<think>b</think>Y"), "XY");
        // No tags → untouched.
        assert_eq!(strip_think("plain answer"), "plain answer");
    }

    #[test]
    fn strip_think_drops_unclosed_reasoning() {
        // An unterminated <think> means the model never emitted a final answer;
        // the leaked reasoning must not become the answer.
        assert_eq!(strip_think("intro <think>reasoning with no close"), "intro");
    }

    #[test]
    fn openai_parse_strips_think_from_content() {
        let v = json!({"choices": [{"message": {"content": "<think>plan</think>done"}}]});
        let r = openai_parse(v).unwrap();
        assert_eq!(r.text, "done");
    }

    #[test]
    fn ollama_parse_strips_think_from_content() {
        let v = json!({"message": {"content": "<think>hmm</think>result"}, "done_reason": "stop"});
        let r = ollama_parse(v).unwrap();
        assert_eq!(r.text, "result");
    }

    // ── stop_reason ─────────────────────────────────────────────────────────
    #[test]
    fn anthropic_parse_captures_stop_reason() {
        let v = json!({"content": [{"type": "text", "text": "hi"}], "stop_reason": "end_turn"});
        let r = anthropic_parse(v).unwrap();
        assert_eq!(r.stop_reason.as_deref(), Some("end_turn"));
        // Absent → None, never fabricated.
        let r = anthropic_parse(json!({"content": []})).unwrap();
        assert_eq!(r.stop_reason, None);
    }

    #[test]
    fn openai_parse_normalizes_finish_reason() {
        let with = |reason: &str| json!({"choices": [{"message": {"content": "x"}, "finish_reason": reason}]});
        assert_eq!(
            openai_parse(with("length")).unwrap().stop_reason.as_deref(),
            Some("max_tokens")
        );
        assert_eq!(
            openai_parse(with("tool_calls"))
                .unwrap()
                .stop_reason
                .as_deref(),
            Some("tool_use")
        );
        assert_eq!(
            openai_parse(with("stop")).unwrap().stop_reason.as_deref(),
            Some("stop")
        );
    }

    #[test]
    fn openai_stream_captures_finish_reason() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\
                   data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\
                   data: [DONE]\n";
        let r = drain_openai(sse);
        assert_eq!(r.text, "Hi");
        assert_eq!(r.stop_reason.as_deref(), Some("max_tokens"));
    }

    // ── ollama_root ─────────────────────────────────────────────────────────
    #[test]
    fn ollama_root_trims_v1_and_slashes() {
        assert_eq!(
            ollama_root("http://localhost:11434"),
            "http://localhost:11434"
        );
        assert_eq!(
            ollama_root("http://localhost:11434/"),
            "http://localhost:11434"
        );
        assert_eq!(
            ollama_root("http://localhost:11434/v1"),
            "http://localhost:11434"
        );
        assert_eq!(
            ollama_root("http://localhost:11434/v1/"),
            "http://localhost:11434"
        );
    }

    // ── ollama_body ─────────────────────────────────────────────────────────
    #[test]
    fn ollama_body_sets_options_and_stream_off() {
        let b = ollama_body("m", &[Msg::User("hi".into())], &[], None, None, 16_384);
        assert_eq!(b["stream"], false);
        assert_eq!(b["options"]["num_ctx"], 16_384);
        assert_eq!(b["options"]["repeat_penalty"], 1.0);
        assert_eq!(b["options"]["temperature"], 0.2);
        assert_eq!(b["options"]["num_predict"], 4096);
        // Sampling knobs live under "options", never at the top level.
        assert!(b.get("temperature").is_none());
        assert!(b.get("max_tokens").is_none());
    }

    #[test]
    fn ollama_body_honors_temperature_and_max_tokens_overrides() {
        let b = ollama_body(
            "m",
            &[Msg::User("hi".into())],
            &[],
            Some(0.7),
            Some(512),
            8_192,
        );
        assert_eq!(b["options"]["temperature"], 0.7);
        assert_eq!(b["options"]["num_predict"], 512);
    }

    #[test]
    fn ollama_body_merges_system_and_maps_roles() {
        let msgs = vec![
            Msg::System("one".into()),
            Msg::System("two".into()),
            Msg::User("u".into()),
        ];
        let b = ollama_body("m", &msgs, &[], None, None, 8_192);
        assert_eq!(b["messages"][0]["role"], "system");
        assert_eq!(b["messages"][0]["content"], "one\n\ntwo");
        assert_eq!(b["messages"][1]["role"], "user");
    }

    #[test]
    fn ollama_body_assistant_tool_call_args_are_objects() {
        let msgs = vec![Msg::Assistant {
            text: "x".into(),
            calls: vec![tc("c1", "run_command", json!({"cmd": "ls"}))],
        }];
        let b = ollama_body("m", &msgs, &[], None, None, 8_192);
        let call = &b["messages"][0]["tool_calls"][0];
        assert_eq!(call["function"]["name"], "run_command");
        // arguments must be a JSON *object*, unlike the OpenAI string form.
        assert_eq!(call["function"]["arguments"], json!({"cmd": "ls"}));
    }

    #[test]
    fn ollama_body_tool_results_carry_tool_name_when_resolvable() {
        let msgs = vec![
            Msg::Assistant {
                text: String::new(),
                calls: vec![tc("c1", "read_file", json!({"path": "a"}))],
            },
            Msg::Tool(vec![
                ToolResult {
                    id: "c1".into(),
                    content: "ok".into(),
                    is_error: false,
                },
                ToolResult {
                    id: "unknown".into(),
                    content: "?".into(),
                    is_error: true,
                },
            ]),
        ];
        let b = ollama_body("m", &msgs, &[], None, None, 8_192);
        let first = &b["messages"][1];
        assert_eq!(first["role"], "tool");
        assert_eq!(first["content"], "ok");
        assert_eq!(first["tool_name"], "read_file");
        // Unresolvable id → no fabricated tool_name.
        assert!(b["messages"][2].get("tool_name").is_none());
    }

    #[test]
    fn ollama_body_images_are_bare_base64_array() {
        let msgs = vec![Msg::UserImages {
            text: "what is this".into(),
            images: vec![("image/png".into(), "aGk=".into())],
        }];
        let b = ollama_body("m", &msgs, &[], None, None, 8_192);
        let msg = &b["messages"][0];
        assert_eq!(msg["content"], "what is this");
        assert_eq!(msg["images"], json!(["aGk="]));
    }

    #[test]
    fn ollama_body_tool_schema_wraps_function() {
        let tools = crate::tools::defs(false);
        let b = ollama_body("m", &[Msg::User("hi".into())], &tools, None, None, 8_192);
        assert_eq!(b["tools"][0]["type"], "function");
        assert!(b["tools"][0]["function"].get("parameters").is_some());
        // No tools → no key.
        let b = ollama_body("m", &[Msg::User("hi".into())], &[], None, None, 8_192);
        assert!(b.get("tools").is_none());
    }

    // ── ollama_stream ───────────────────────────────────────────────────────
    fn drain_ollama(ndjson: &str) -> Reply {
        let mut out = String::new();
        ollama_stream(
            Cursor::new(ndjson.as_bytes().to_vec()),
            &mut |t| out.push_str(t),
            &mut |_| {},
        )
        .unwrap()
    }

    #[test]
    fn ollama_stream_accumulates_content_and_stops_on_done() {
        let ndjson = "{\"message\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"done\":false}\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":\"lo\"},\"done\":false}\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\"}\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":\"AFTER\"},\"done\":false}\n";
        let r = drain_ollama(ndjson);
        assert_eq!(r.text, "Hello");
        assert_eq!(r.stop_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn ollama_stream_maps_length_done_reason_to_max_tokens() {
        let ndjson = "{\"message\":{\"role\":\"assistant\",\"content\":\"Hi\"},\"done\":false}\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"done_reason\":\"length\"}\n";
        let r = drain_ollama(ndjson);
        assert_eq!(r.stop_reason.as_deref(), Some("max_tokens"));
    }

    #[test]
    fn ollama_stream_collects_whole_tool_calls_with_object_args() {
        let ndjson = "{\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"read_file\",\"arguments\":{\"path\":\"a\"}}}]},\"done\":false}\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"list_dir\",\"arguments\":{}}}]},\"done\":false}\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\"}\n";
        let r = drain_ollama(ndjson);
        assert_eq!(r.calls.len(), 2);
        assert_eq!(r.calls[0].name, "read_file");
        assert_eq!(r.calls[0].input, json!({"path": "a"}));
        // Synthesized, position-stable ids (Ollama sends none).
        assert_eq!(r.calls[0].id, "ollama-call-0");
        assert_eq!(r.calls[1].id, "ollama-call-1");
    }

    #[test]
    fn ollama_stream_string_args_go_through_invalid_args_guard() {
        let ndjson = "{\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"f\",\"arguments\":\"{bad\"}}]},\"done\":true,\"done_reason\":\"stop\"}\n";
        let r = drain_ollama(ndjson);
        assert_eq!(r.calls[0].input[crate::tools::INVALID_ARGS], json!("{bad"));
    }

    #[test]
    fn ollama_stream_routes_thinking_field_and_think_tags() {
        let ndjson = "{\"message\":{\"role\":\"assistant\",\"content\":\"\",\"thinking\":\"hmm \"},\"done\":false}\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":\"<think>deep</think>Hi\"},\"done\":true,\"done_reason\":\"stop\"}\n";
        let mut text_out = String::new();
        let mut think_out = String::new();
        ollama_stream(
            Cursor::new(ndjson.as_bytes().to_vec()),
            &mut |t| text_out.push_str(t),
            &mut |t| think_out.push_str(t),
        )
        .unwrap();
        assert_eq!(text_out, "Hi");
        assert_eq!(think_out, "hmm deep");
    }

    #[test]
    fn ollama_stream_error_line_is_err() {
        let ndjson = "{\"message\":{\"role\":\"assistant\",\"content\":\"par\"},\"done\":false}\n\
                      {\"error\":\"model runner has unexpectedly stopped\"}\n";
        let r = ollama_stream(
            Cursor::new(ndjson.as_bytes().to_vec()),
            &mut |_| {},
            &mut |_| {},
        );
        let err = r.unwrap_err();
        assert!(err.contains("stream error"), "got: {err}");
        assert!(err.contains("unexpectedly stopped"), "got: {err}");
    }

    #[test]
    fn ollama_stream_read_error_before_done_is_err() {
        let ndjson = "{\"message\":{\"role\":\"assistant\",\"content\":\"Hi\"},\"done\":false}\n";
        let r = ollama_stream(
            BrokenPipe(Cursor::new(ndjson.as_bytes().to_vec())),
            &mut |_| {},
            &mut |_| {},
        );
        assert!(r.unwrap_err().contains("stream read failed"));
    }

    #[test]
    fn ollama_stream_ignores_blank_and_malformed_lines() {
        let ndjson = "\n\
                      not-json\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":\"ok\"},\"done\":true,\"done_reason\":\"stop\"}\n";
        let r = drain_ollama(ndjson);
        assert_eq!(r.text, "ok");
    }

    // ── ollama_parse (non-stream) ───────────────────────────────────────────
    #[test]
    fn ollama_parse_extracts_text_calls_and_done_reason() {
        let v = json!({
            "message": {
                "role": "assistant",
                "content": "hello",
                "tool_calls": [{"function": {"name": "read_file", "arguments": {"path": "a"}}}]
            },
            "done": true,
            "done_reason": "length"
        });
        let r = ollama_parse(v).unwrap();
        assert_eq!(r.text, "hello");
        assert_eq!(r.calls[0].name, "read_file");
        assert_eq!(r.calls[0].input, json!({"path": "a"}));
        assert_eq!(r.stop_reason.as_deref(), Some("max_tokens"));
        // Absent done_reason → None, never fabricated.
        let r = ollama_parse(json!({"message": {"content": "x"}})).unwrap();
        assert_eq!(r.stop_reason, None);
    }

    // ── /api/show parsing + num_ctx sizing ──────────────────────────────────
    #[test]
    fn show_context_length_finds_arch_prefixed_key() {
        let v = json!({"model_info": {
            "general.architecture": "qwen2",
            "qwen2.embedding_length": 5120,
            "qwen2.context_length": 131_072
        }});
        assert_eq!(show_context_length(&v), Some(131_072));
        // No matching key or no model_info → None.
        assert_eq!(show_context_length(&json!({"model_info": {"x": 1}})), None);
        assert_eq!(show_context_length(&json!({})), None);
    }

    #[test]
    fn show_num_ctx_parses_parameters_blob() {
        let v = json!({"parameters": "stop    \"<|im_end|>\"\nnum_ctx    16384\ntemperature 0.7"});
        assert_eq!(show_num_ctx(&v), Some(16_384));
        assert_eq!(show_num_ctx(&json!({"parameters": "stop \"x\""})), None);
        assert_eq!(show_num_ctx(&json!({})), None);
    }

    #[test]
    fn ollama_pick_ctx_caps_floors_and_honors_modelfile() {
        // Known model max: capped at 32k.
        assert_eq!(ollama_pick_ctx(Some(131_072), None), 32_768);
        // Small model max wins over the cap.
        assert_eq!(ollama_pick_ctx(Some(16_384), None), 16_384);
        // Unknown max → 8k floor.
        assert_eq!(ollama_pick_ctx(None, None), 8_192);
        // Modelfile num_ctx wins (the user tuned it), even past the cap…
        assert_eq!(ollama_pick_ctx(Some(131_072), Some(65_536)), 65_536);
        assert_eq!(ollama_pick_ctx(None, Some(4_096)), 4_096);
        // …but never exceeds a known model max.
        assert_eq!(ollama_pick_ctx(Some(8_192), Some(65_536)), 8_192);
    }
}
