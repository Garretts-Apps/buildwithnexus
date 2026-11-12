//! Anthropic Messages API: request bodies, extended thinking, and reply parsing.

use super::*;

// Pure request-body builder, split out from the HTTP wiring so the JSON shape
// (system caching, tool schema mapping, tool_use/tool_result, the last-message
// cache breakpoint) is unit-testable without constructing a `crate::net::Request`.
pub(super) fn anthropic_body(
    model: &str,
    msgs: &[Msg],
    tools: &[ToolDef],
    max_tokens: Option<u32>,
) -> Value {
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
                            "content": anthropic_tool_content(r), "is_error": r.is_error
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

// A tool_result's content: the text alone, or text and image blocks when
// the tool returned images (an empty text block is refused by the API).
pub(super) fn anthropic_tool_content(r: &ToolResult) -> Value {
    if r.images.is_empty() {
        return json!(r.content);
    }
    let mut parts = Vec::new();
    if !r.content.is_empty() {
        parts.push(json!({"type": "text", "text": r.content}));
    }
    parts.extend(r.images.iter().map(|(mt, data)| {
        json!({
            "type": "image",
            "source": {"type": "base64", "media_type": mt, "data": data}
        })
    }));
    json!(parts)
}

// Tool messages on the OpenAI and Ollama protocols carry text only, so the
// images a round of tools returned follow it in one user turn. Each set is
// named by the call it came from; `None` when no result had an image.
pub(super) fn tool_images_intro(
    results: &[ToolResult],
    names: &HashMap<&str, &str>,
) -> Option<String> {
    let lines: Vec<String> = results
        .iter()
        .filter(|r| !r.images.is_empty())
        .map(|r| tool_images_label(r, names))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

pub(super) fn tool_images_label(r: &ToolResult, names: &HashMap<&str, &str>) -> String {
    let n = r.images.len();
    let what = if n == 1 {
        "1 image".to_string()
    } else {
        format!("{n} images")
    };
    match names.get(r.id.as_str()) {
        Some(name) => format!("[{what} returned by the {name} call {}]", r.id),
        None => format!("[{what} returned by tool call {}]", r.id),
    }
}

pub(super) fn anthropic_request(
    p: &Provider,
    msgs: &[Msg],
    tools: &[ToolDef],
) -> Result<(crate::net::Request, Value), String> {
    let mut body = anthropic_body(&p.model, msgs, tools, p.max_tokens);
    apply_anthropic_effort(&mut body, &p.model, p.effort);
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

// Thinking budgets for the `budget_tokens` form (models before Claude 4.6).
pub(super) fn thinking_budget(effort: Effort) -> Option<u32> {
    match effort {
        Effort::Off => None,
        Effort::Low => Some(2048),
        Effort::Medium => Some(8192),
        Effort::High => Some(16384),
    }
}

// (major, minor) of a Claude model id, tolerating both layouts —
// "claude-opus-4-6", "claude-sonnet-4-20250514" (date, not a minor),
// "claude-3-7-sonnet-…". None for anything that isn't a versioned claude-* id.
pub(super) fn claude_version(model: &str) -> Option<(u32, u32)> {
    let rest = model.trim().to_ascii_lowercase();
    let rest = rest.strip_prefix("claude-")?;
    let mut parts = rest.split('-').filter_map(|seg| seg.parse::<u32>().ok());
    let major = parts.next()?;
    let minor = parts.next().filter(|m| *m < 100).unwrap_or(0);
    Some((major, minor))
}

// Reasoning on the Anthropic shape. Claude 4.6 and later take adaptive
// thinking plus `output_config.effort` (the fixed `budget_tokens` form is
// deprecated there and rejected from 4.7 on); earlier models and unversioned
// ids get `thinking.enabled` with a budget, and max_tokens is raised to stay
// strictly above it as the API requires. Thinking blocks recorded from the
// model's earlier replies are put back in front of their tool_use blocks —
// a thinking-enabled tool round is rejected without them.
pub(super) fn apply_anthropic_effort(body: &mut Value, model: &str, effort: Effort) {
    let Some(budget) = thinking_budget(effort) else {
        return;
    };
    let adaptive = claude_version(model).is_some_and(|v| v >= (4, 6));
    if adaptive {
        body["thinking"] = json!({"type": "adaptive"});
        if claude_version(model).is_some_and(|v| v >= (4, 7)) {
            // 4.7+ defaults to omitting thinking text; ask for the summary
            // so the TUI's thinking stream has something to show.
            body["thinking"]["display"] = json!("summarized");
        }
        body["output_config"] = json!({"effort": effort.as_str()});
    } else {
        let max_tokens = body["max_tokens"].as_u64().unwrap_or(8192);
        if max_tokens <= u64::from(budget) {
            body["max_tokens"] = json!(budget + 4096);
        }
        body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
    }
    replay_thinking_blocks(body);
}

// Thinking blocks from earlier replies, keyed by the id of the first tool
// call they preceded. Bounded so a long session (or parallel sub-agents)
// never grows it without limit.
pub(super) type ThinkingMemory = std::sync::Mutex<Vec<(String, Vec<Value>)>>;

pub(super) fn thinking_memory() -> &'static ThinkingMemory {
    static M: std::sync::OnceLock<ThinkingMemory> = std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

pub(super) const THINKING_MEMORY_CAP: usize = 64;

pub(super) fn remember_thinking(reply: &Reply) {
    let (Some(first), false) = (reply.calls.first(), reply.thinking_blocks.is_empty()) else {
        return;
    };
    if let Ok(mut m) = thinking_memory().lock() {
        m.push((first.id.clone(), reply.thinking_blocks.clone()));
        if m.len() > THINKING_MEMORY_CAP {
            m.remove(0);
        }
    }
}

pub(super) fn thinking_for(call_id: &str) -> Option<Vec<Value>> {
    thinking_memory()
        .lock()
        .ok()?
        .iter()
        .rev()
        .find(|(id, _)| id == call_id)
        .map(|(_, blocks)| blocks.clone())
}

pub(super) fn replay_thinking_blocks(body: &mut Value) {
    let Some(messages) = body["messages"].as_array_mut() else {
        return;
    };
    for m in messages.iter_mut() {
        if m["role"].as_str() != Some("assistant") {
            continue;
        }
        let Some(content) = m["content"].as_array_mut() else {
            continue;
        };
        let first_call = content
            .iter()
            .find(|b| b["type"].as_str() == Some("tool_use"))
            .and_then(|b| b["id"].as_str())
            .map(str::to_string);
        let Some(blocks) = first_call.and_then(|id| thinking_for(&id)) else {
            continue;
        };
        if content
            .first()
            .is_some_and(|b| matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking")))
        {
            continue;
        }
        content.splice(0..0, blocks);
    }
}

// Copy whichever usage counters an Anthropic `usage` object carries; fields
// the server omits keep their previous value (message_start reports input,
// message_delta reports output).
pub(super) fn anthropic_usage_into(u: &Value, into: &mut Usage) {
    if let Some(n) = u["input_tokens"].as_u64() {
        into.input = n;
    }
    if let Some(n) = u["cache_creation_input_tokens"].as_u64() {
        into.cache_write = n;
    }
    if let Some(n) = u["cache_read_input_tokens"].as_u64() {
        into.cache_read = n;
    }
    if let Some(n) = u["output_tokens"].as_u64() {
        into.output = n;
    }
}

// Put an ephemeral cache breakpoint on the last content block of the last
// message, normalizing a string body into a single text block if needed.
pub(super) fn cache_last_message(messages: &mut [Value]) {
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

pub(super) fn anthropic_parse(v: Value) -> Result<Reply, String> {
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut thinking_blocks = Vec::new();
    if let Some(blocks) = v["content"].as_array() {
        for b in blocks {
            match b["type"].as_str() {
                Some("text") => text.push_str(b["text"].as_str().unwrap_or_default()),
                Some("tool_use") => calls.push(ToolCall {
                    id: b["id"].as_str().unwrap_or_default().to_string(),
                    name: b["name"].as_str().unwrap_or_default().to_string(),
                    input: b["input"].clone(),
                }),
                Some("thinking" | "redacted_thinking") => thinking_blocks.push(b.clone()),
                _ => {}
            }
        }
    }
    let stop_reason = v["stop_reason"].as_str().map(normalize_stop_reason);
    let mut usage = Usage::default();
    anthropic_usage_into(&v["usage"], &mut usage);
    Ok(Reply {
        text: strip_think(&text),
        calls,
        stop_reason,
        usage,
        thinking_blocks,
    })
}

pub(super) fn anthropic_stream(
    reader: impl Read,
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) -> Result<Reply, String> {
    let mut text = String::new();
    let mut stop_reason: Option<String> = None;
    let mut stream_err: Option<String> = None;
    let mut usage = Usage::default();
    // index → (id, name, accumulated input JSON)
    let mut pending: Vec<(usize, String, String, String)> = Vec::new();
    // index → thinking/redacted_thinking block, assembled from its deltas so
    // it can be replayed verbatim (signature included) on the next request.
    let mut thinking: Vec<(usize, Value)> = Vec::new();
    let stopped = for_each_sse(reader, |data| {
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return false;
        };
        match v["type"].as_str() {
            Some("message_start") => {
                anthropic_usage_into(&v["message"]["usage"], &mut usage);
            }
            Some("content_block_start") => {
                let idx = v["index"].as_u64().unwrap_or(0) as usize;
                let cb = &v["content_block"];
                match cb["type"].as_str() {
                    Some("tool_use") => pending.push((
                        idx,
                        cb["id"].as_str().unwrap_or_default().to_string(),
                        cb["name"].as_str().unwrap_or_default().to_string(),
                        String::new(),
                    )),
                    Some("thinking") => thinking.push((
                        idx,
                        json!({"type": "thinking", "thinking": "", "signature": ""}),
                    )),
                    Some("redacted_thinking") => thinking.push((idx, cb.clone())),
                    _ => {}
                }
            }
            Some("content_block_delta") => {
                let d = &v["delta"];
                let idx = v["index"].as_u64().unwrap_or(0) as usize;
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
                        if let Some((_, block)) = thinking.iter_mut().find(|e| e.0 == idx) {
                            if let Some(acc) = block["thinking"].as_str() {
                                block["thinking"] = json!(format!("{acc}{t}"));
                            }
                        }
                        on_thinking(t);
                    }
                    Some("signature_delta") => {
                        if let Some((_, block)) = thinking.iter_mut().find(|e| e.0 == idx) {
                            block["signature"] = json!(d["signature"].as_str().unwrap_or_default());
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(e) = pending.iter_mut().find(|e| e.0 == idx) {
                            e.3.push_str(d["partial_json"].as_str().unwrap_or_default());
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                // Final metadata for the turn — carries the stop reason and
                // the output token count.
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    stop_reason = Some(normalize_stop_reason(r));
                }
                anthropic_usage_into(&v["usage"], &mut usage);
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
    // message_delta carries the stop reason, so a stream that got that far has
    // all its content even if message_stop itself was lost.
    if !stopped && stop_reason.is_none() {
        return Err(STREAM_TRUNCATED.to_string());
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
        usage,
        thinking_blocks: thinking.into_iter().map(|(_, b)| b).collect(),
    })
}
