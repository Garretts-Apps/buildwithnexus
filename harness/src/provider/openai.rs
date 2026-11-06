//! OpenAI-compatible /chat/completions: request bodies, reasoning effort, and reply parsing.

use super::*;

// Pure body builder (see `anthropic_body` for the rationale).
pub(super) fn openai_body(
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
    let mut call_names: HashMap<&str, &str> = HashMap::new();
    for m in msgs {
        match m {
            Msg::System(_) => {}
            Msg::User(t) => messages.push(json!({"role": "user", "content": t})),
            Msg::UserImages { text, images } => {
                let mut parts: Vec<Value> = images.iter().map(openai_image_part).collect();
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
                    for c in calls {
                        call_names.insert(&c.id, &c.name);
                    }
                }
                messages.push(msg);
            }
            Msg::Tool(results) => {
                for r in results {
                    messages
                        .push(json!({"role": "tool", "tool_call_id": r.id, "content": r.content}));
                }
                // After every tool message of the round: a user turn between
                // them would orphan the calls that follow it.
                if tool_images_intro(results, &call_names).is_some() {
                    let mut parts: Vec<Value> = Vec::new();
                    for r in results.iter().filter(|r| !r.images.is_empty()) {
                        parts.push(
                            json!({"type": "text", "text": tool_images_label(r, &call_names)}),
                        );
                        parts.extend(r.images.iter().map(openai_image_part));
                    }
                    messages.push(json!({"role": "user", "content": parts}));
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

pub(super) fn openai_image_part((mt, data): &(String, String)) -> Value {
    json!({
        "type": "image_url",
        "image_url": {"url": format!("data:{mt};base64,{data}")}
    })
}

// Same request against an explicit base URL — the Ollama fallback path targets
// {host}/v1 while the provider's base_url is the bare host root.
pub(super) fn openai_request_at(
    p: &Provider,
    base: &str,
    msgs: &[Msg],
    tools: &[ToolDef],
) -> (ureq::Request, Value) {
    let mut body = openai_body(&p.model, msgs, tools, p.temperature, p.max_tokens);
    apply_openai_effort(&mut body, &p.model, p.effort);
    let mut req = agent()
        .post(&format!("{}/chat/completions", base.trim_end_matches('/')))
        .set("content-type", "application/json");
    if let Some(key) = &p.api_key {
        req = req.set("authorization", &format!("Bearer {key}"));
    }
    (req, body)
}

// Model names that accept `reasoning_effort` on the OpenAI shape: the o-series
// and gpt-5 family. Everything else (gpt-4o, local models behind an
// OpenAI-compatible server, …) rejects or ignores it, so it's never sent.
pub(super) fn is_openai_reasoning_model(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    ["o1", "o3", "o4", "gpt-5"].iter().any(|p| {
        m.starts_with(p)
            && m[p.len()..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_ascii_digit())
    })
}

pub(super) fn apply_openai_effort(body: &mut Value, model: &str, effort: Effort) {
    if effort != Effort::Off && is_openai_reasoning_model(model) {
        body["reasoning_effort"] = json!(effort.as_str());
    }
}

// Token counts from an OpenAI-shape `usage` object. `prompt_tokens` includes
// the cached portion, which is split out so `input` means uncached tokens
// like the Anthropic shape. None when the chunk carries no usage object.
pub(super) fn openai_usage(v: &Value) -> Option<Usage> {
    let u = v.get("usage")?.as_object()?;
    let prompt = u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
    let cached = u
        .get("prompt_tokens_details")
        .and_then(|d| d["cached_tokens"].as_u64())
        .unwrap_or(0)
        .min(prompt);
    Some(Usage {
        input: prompt - cached,
        output: u
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read: cached,
        cache_write: 0,
    })
}

// Remove `<think>…</think>` reasoning blocks that local reasoning models
// (DeepSeek-R1 distills, Qwen thinking variants) emit inline in `content` on
// the non-streaming path. Handles multiple blocks and an unclosed `<think>`
// (drops the trailing leaked chain-of-thought). Text without think tags is
// returned untouched. The streaming path already routes these to on_thinking.
pub(super) fn strip_think(text: &str) -> String {
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

pub(super) const STREAM_TRUNCATED: &str = "stream ended before completion";

// An {"error": …} payload (in a 200 body or a stream chunk), redacted and
// truncated like HTTP error bodies.
pub(super) fn openai_error(v: &Value) -> Option<String> {
    let e = v.get("error").filter(|e| !e.is_null())?;
    let detail = e["message"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| e.to_string());
    Some(format!(
        "server error: {}",
        redact(&detail).chars().take(400).collect::<String>()
    ))
}

pub(super) fn openai_parse(v: Value) -> Result<Reply, String> {
    if let Some(e) = openai_error(&v) {
        return Err(e);
    }
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
        usage: openai_usage(&v).unwrap_or_default(),
        ..Default::default()
    })
}

// If `s` ends with a non-empty *proper* prefix of `tag` (e.g. "a<thi" for
// "<think>"), split so the prefix can be held until the next chunk completes it.
// Returns (emit, hold). No trailing partial → (s, "").
pub(super) fn split_trailing_partial<'a>(s: &'a str, tag: &str) -> (&'a str, &'a str) {
    let max = (tag.len() - 1).min(s.len());
    for k in (1..=max).rev() {
        if s.is_char_boundary(s.len() - k) && s.as_bytes().ends_with(&tag.as_bytes()[..k]) {
            return (&s[..s.len() - k], &s[s.len() - k..]);
        }
    }
    (s, "")
}

// Route streamed OpenAI/Ollama `content` deltas: `<think>…</think>` spans go to
// `on_thinking`, everything else to `text`/`on_text`. `carry` holds a `<think>`
// or `</think>` tag that was split across chunk boundaries (routine with
// token-by-token local streaming) so the partial marker isn't leaked as output.
pub(super) fn route_openai_content(
    chunk: &str,
    in_think: &mut bool,
    carry: &mut String,
    text: &mut String,
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) {
    let combined = if carry.is_empty() {
        chunk.to_string()
    } else {
        format!("{carry}{chunk}")
    };
    carry.clear();
    let mut rest = combined.as_str();
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
                // Hold a possible partial `</think>` at the tail for next chunk.
                let (emit, hold) = split_trailing_partial(rest, "</think>");
                if !emit.is_empty() {
                    on_thinking(emit);
                }
                *carry = hold.to_string();
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
            // Hold a possible partial `<think>` at the tail for the next chunk.
            let (emit, hold) = split_trailing_partial(rest, "<think>");
            if !emit.is_empty() {
                text.push_str(emit);
                on_text(emit);
            }
            *carry = hold.to_string();
            break;
        }
    }
}

// Flush a leftover `carry` at stream end: if we're not mid-think it was ordinary
// text that merely resembled a tag prefix, so surface it; mid-think residue
// (a dangling `</think>` prefix with no answer after) is dropped.
pub(super) fn flush_think_carry(
    carry: &str,
    in_think: bool,
    text: &mut String,
    on_text: &mut dyn FnMut(&str),
) {
    if !in_think && !carry.is_empty() {
        text.push_str(carry);
        on_text(carry);
    }
}

pub(super) fn openai_stream(
    reader: impl Read,
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) -> Result<Reply, String> {
    let mut text = String::new();
    let mut in_think = false;
    let mut think_carry = String::new();
    let mut stop_reason: Option<String> = None;
    let mut usage = Usage::default();
    // index → (id, name, accumulated args)
    let mut pending: Vec<(String, String, String)> = Vec::new();
    let mut stream_err: Option<String> = None;
    let stopped = for_each_sse(reader, |data| {
        if data == "[DONE]" {
            return true;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return false;
        };
        // vLLM, LiteLLM, OpenRouter and friends report mid-stream failures as
        // an {"error": …} chunk rather than an HTTP status.
        if let Some(e) = openai_error(&v) {
            stream_err = Some(e);
            return true;
        }
        // With stream_options.include_usage the final chunk (empty choices)
        // carries the totals; some servers put usage on every chunk instead.
        if let Some(u) = openai_usage(&v) {
            usage = u;
        }
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
            route_openai_content(
                t,
                &mut in_think,
                &mut think_carry,
                &mut text,
                on_text,
                on_thinking,
            );
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
    if let Some(e) = stream_err {
        return Err(e);
    }
    if !stopped && stop_reason.is_none() {
        return Err(STREAM_TRUNCATED.to_string());
    }
    flush_think_carry(&think_carry, in_think, &mut text, on_text);
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
        usage,
        ..Default::default()
    })
}
