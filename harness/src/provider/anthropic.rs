//! Anthropic Messages API: request bodies, extended thinking, and reply parsing.

use super::*;

/// What shapes an Anthropic request besides the conversation itself.
pub(super) struct AnthropicBody<'a> {
    pub(super) model: &'a str,
    pub(super) max_tokens: Option<u32>,
    pub(super) effort: Effort,
    pub(super) stream: bool,
}

const CACHE_CONTROL: &str = "\"cache_control\":{\"type\":\"ephemeral\"},";

// The request body, written once (see `wire`). Split from the HTTP wiring so
// the JSON shape (system caching, tool schema mapping, tool_use/tool_result,
// the last-message cache breakpoint, the thinking forms) is unit-testable
// without a server.
//
// Prompt caching: render order is tools → system → messages, so a breakpoint
// on the system block caches the stable tools+system prefix, and one on the
// last content block of the last message caches the growing conversation.
// Each ReAct step then reads the prior turns from cache — much lower
// time-to-first-token on multi-step tasks. A last message that is a plain
// string becomes a single text block so it can carry the marker.
//
// Reasoning: Claude 4.6 and later take adaptive thinking plus
// `output_config.effort` (the fixed `budget_tokens` form is deprecated there
// and rejected from 4.7 on); earlier models and unversioned ids get
// `thinking.enabled` with a budget, and max_tokens is raised to stay strictly
// above it as the API requires. Thinking blocks recorded from the model's
// earlier replies are put back in front of their tool_use blocks — a
// thinking-enabled tool round is rejected without them.
pub(super) fn anthropic_bytes(msgs: &[Msg], tools: &[ToolDef], o: &AnthropicBody) -> Vec<u8> {
    let budget = thinking_budget(o.effort);
    let version = claude_version(o.model);
    let adaptive = version.is_some_and(|v| v >= (4, 6));
    let mut max_tokens = o.max_tokens.unwrap_or(8192);
    if let (Some(budget), false) = (budget, adaptive) {
        if max_tokens <= budget {
            max_tokens = budget + 4096;
        }
    }

    let mut j = Json::with_capacity(size_hint(msgs, tools));
    j.raw("{\"max_tokens\":");
    j.val(&max_tokens);
    j.raw(",\"messages\":[");
    let last = msgs.iter().rposition(|m| !matches!(m, Msg::System(_)));
    let mut sep = Sep::default();
    for (i, m) in msgs.iter().enumerate() {
        let cached = Some(i) == last;
        match m {
            Msg::System(_) => {}
            Msg::User(t) if cached => {
                sep.next(&mut j);
                j.raw("{\"content\":[");
                text_block(&mut j, t, true);
                j.raw("],\"role\":\"user\"}");
            }
            Msg::User(t) => {
                sep.next(&mut j);
                text_message(&mut j, "user", t);
            }
            Msg::UserImages { text, images } => {
                sep.next(&mut j);
                j.raw("{\"content\":[");
                for image in images {
                    image_block(&mut j, image);
                    j.raw(",");
                }
                text_block(&mut j, text, cached);
                j.raw("],\"role\":\"user\"}");
            }
            Msg::Assistant { text, calls } => {
                sep.next(&mut j);
                j.raw("{\"content\":[");
                let mut blocks = Sep::default();
                let replayed = budget
                    .and(calls.first())
                    .and_then(|first| thinking_for(&first.id));
                for block in replayed.iter().flatten() {
                    blocks.next(&mut j);
                    j.val(block);
                }
                if !text.is_empty() {
                    blocks.next(&mut j);
                    text_block(&mut j, text, cached && calls.is_empty());
                }
                for (k, c) in calls.iter().enumerate() {
                    blocks.next(&mut j);
                    j.raw("{");
                    if cached && k + 1 == calls.len() {
                        j.raw(CACHE_CONTROL);
                    }
                    j.raw("\"id\":");
                    j.str(&c.id);
                    j.raw(",\"input\":");
                    j.val(&c.input);
                    j.raw(",\"name\":");
                    j.str(&c.name);
                    j.raw(",\"type\":\"tool_use\"}");
                }
                j.raw("],\"role\":\"assistant\"}");
            }
            Msg::Tool(results) => {
                sep.next(&mut j);
                j.raw("{\"content\":[");
                let mut blocks = Sep::default();
                for (k, r) in results.iter().enumerate() {
                    blocks.next(&mut j);
                    j.raw("{");
                    if cached && k + 1 == results.len() {
                        j.raw(CACHE_CONTROL);
                    }
                    j.raw("\"content\":");
                    tool_content(&mut j, r);
                    j.raw(if r.is_error {
                        ",\"is_error\":true,\"tool_use_id\":"
                    } else {
                        ",\"is_error\":false,\"tool_use_id\":"
                    });
                    j.str(&r.id);
                    j.raw(",\"type\":\"tool_result\"}");
                }
                j.raw("],\"role\":\"user\"}");
            }
        }
    }
    j.raw("],\"model\":");
    j.str(o.model);
    if budget.is_some() && adaptive {
        j.raw(",\"output_config\":{\"effort\":\"");
        j.raw(o.effort.as_str());
        j.raw("\"}");
    }
    if o.stream {
        j.raw(",\"stream\":true");
    }
    let system = SystemText(msgs);
    if !system.is_empty() {
        j.raw(",\"system\":[{");
        j.raw(CACHE_CONTROL);
        j.raw("\"text\":");
        j.display(&system);
        j.raw(",\"type\":\"text\"}]");
    }
    match budget {
        None => {}
        // 4.7+ defaults to omitting thinking text; ask for the summary so
        // the TUI's thinking stream has something to show.
        Some(_) if version.is_some_and(|v| v >= (4, 7)) => {
            j.raw(",\"thinking\":{\"display\":\"summarized\",\"type\":\"adaptive\"}")
        }
        Some(_) if adaptive => j.raw(",\"thinking\":{\"type\":\"adaptive\"}"),
        Some(budget) => {
            j.raw(",\"thinking\":{\"budget_tokens\":");
            j.val(&budget);
            j.raw(",\"type\":\"enabled\"}");
        }
    }
    if !tools.is_empty() {
        j.raw(",\"tools\":[");
        let mut sep = Sep::default();
        for t in tools {
            sep.next(&mut j);
            j.raw("{\"description\":");
            j.str(t.description);
            j.raw(",\"input_schema\":");
            j.val(&t.schema);
            j.raw(",\"name\":");
            j.str(t.name);
            j.raw("}");
        }
        j.raw("]");
    }
    j.raw("}");
    j.finish()
}

fn text_block(j: &mut Json, text: &str, cached: bool) {
    j.raw("{");
    if cached {
        j.raw(CACHE_CONTROL);
    }
    j.raw("\"text\":");
    j.str(text);
    j.raw(",\"type\":\"text\"}");
}

fn image_block(j: &mut Json, (media_type, data): &(String, String)) {
    j.raw("{\"source\":{\"data\":");
    j.str(data);
    j.raw(",\"media_type\":");
    j.str(media_type);
    j.raw(",\"type\":\"base64\"},\"type\":\"image\"}");
}

// A tool_result's content: the text alone, or text and image blocks when
// the tool returned images (an empty text block is refused by the API).
fn tool_content(j: &mut Json, r: &ToolResult) {
    if r.images.is_empty() {
        return j.str(&r.content);
    }
    j.raw("[");
    let mut sep = Sep::default();
    if !r.content.is_empty() {
        sep.next(j);
        text_block(j, &r.content, false);
    }
    for image in &r.images {
        sep.next(j);
        image_block(j, image);
    }
    j.raw("]");
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

pub(super) fn anthropic_request(p: &Provider) -> Result<crate::net::Request, String> {
    let key = p
        .api_key
        .as_deref()
        .ok_or("Anthropic requires an API key")?;
    Ok(agent()
        .post(&url(p, "/v1/messages"))
        .set("x-api-key", key)
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json"))
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
