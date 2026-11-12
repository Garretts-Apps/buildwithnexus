//! OpenAI-compatible /chat/completions: request bodies, reasoning effort, and reply parsing.

use super::*;

/// What shapes an OpenAI-compatible request besides the conversation.
pub(super) struct OpenAiBody<'a> {
    pub(super) model: &'a str,
    pub(super) temperature: Option<f64>,
    pub(super) max_tokens: Option<u32>,
    pub(super) effort: Effort,
    /// Also asks for `stream_options.include_usage`, which makes the server
    /// append a final chunk carrying the request's token counts.
    pub(super) stream: bool,
    /// `Some(keep_images)` for the shape restrictive chat templates accept
    /// (notably Gemma's on llama.cpp): strictly alternating user/assistant
    /// turns, no `system`/`tool` roles, and no native `tools` field. The
    /// tools are described in text at the top of the first turn instead.
    /// Images stay on the user turns that carried them unless `keep_images`
    /// is false (the server rejected them).
    pub(super) flatten: Option<bool>,
}

// The request body, written once (see `wire` and `anthropic_bytes`).
pub(super) fn openai_bytes(msgs: &[Msg], tools: &[ToolDef], o: &OpenAiBody) -> Vec<u8> {
    let mut j = Json::with_capacity(size_hint(msgs, tools));
    j.raw("{\"max_tokens\":");
    j.val(&o.max_tokens.unwrap_or(4096));
    j.raw(",\"messages\":[");
    match o.flatten {
        Some(keep_images) => flat_messages(&mut j, msgs, tools, keep_images),
        None => native_messages(&mut j, msgs),
    }
    j.raw("],\"model\":");
    j.str(o.model);
    if o.effort != Effort::Off && is_openai_reasoning_model(o.model) {
        j.raw(",\"reasoning_effort\":\"");
        j.raw(o.effort.as_str());
        j.raw("\"");
    }
    if o.stream {
        j.raw(",\"stream\":true,\"stream_options\":{\"include_usage\":true}");
    }
    j.raw(",\"temperature\":");
    j.val(&o.temperature.unwrap_or(0.2));
    if !tools.is_empty() && o.flatten.is_none() {
        function_tools(&mut j, tools);
    }
    j.raw("}");
    j.finish()
}

fn native_messages(j: &mut Json, msgs: &[Msg]) {
    let mut sep = Sep::default();
    let system = SystemText(msgs);
    if !system.is_empty() {
        sep.next(j);
        j.raw("{\"content\":");
        j.display(&system);
        j.raw(",\"role\":\"system\"}");
    }
    let mut call_names: HashMap<&str, &str> = HashMap::new();
    for m in msgs {
        match m {
            Msg::System(_) => {}
            Msg::User(t) => {
                sep.next(j);
                text_message(j, "user", t);
            }
            Msg::UserImages { text, images } => {
                sep.next(j);
                j.raw("{\"content\":[");
                for image in images {
                    image_part(j, image);
                    j.raw(",");
                }
                text_part(j, text);
                j.raw("],\"role\":\"user\"}");
            }
            Msg::Assistant { text, calls } => {
                sep.next(j);
                j.raw("{\"content\":");
                j.str(text);
                j.raw(",\"role\":\"assistant\"");
                if !calls.is_empty() {
                    j.raw(",\"tool_calls\":[");
                    let mut each = Sep::default();
                    for c in calls {
                        each.next(j);
                        // The arguments are a JSON document inside a string.
                        j.raw("{\"function\":{\"arguments\":");
                        j.display(&c.input);
                        j.raw(",\"name\":");
                        j.str(&c.name);
                        j.raw("},\"id\":");
                        j.str(&c.id);
                        j.raw(",\"type\":\"function\"}");
                        call_names.insert(&c.id, &c.name);
                    }
                    j.raw("]");
                }
                j.raw("}");
            }
            Msg::Tool(results) => {
                for r in results {
                    sep.next(j);
                    j.raw("{\"content\":");
                    j.str(&r.content);
                    j.raw(",\"role\":\"tool\",\"tool_call_id\":");
                    j.str(&r.id);
                    j.raw("}");
                }
                // Tool messages carry text only, so the images a round of
                // tools returned follow it in one user turn, each set named
                // by the call it came from. After every tool message of the
                // round: a user turn between them would orphan the calls
                // that follow it.
                if results.iter().any(|r| !r.images.is_empty()) {
                    sep.next(j);
                    j.raw("{\"content\":[");
                    let mut each = Sep::default();
                    for r in results.iter().filter(|r| !r.images.is_empty()) {
                        each.next(j);
                        text_part(j, &tool_images_label(r, &call_names));
                        for image in &r.images {
                            j.raw(",");
                            image_part(j, image);
                        }
                    }
                    j.raw("],\"role\":\"user\"}");
                }
            }
        }
    }
}

fn flat_messages(j: &mut Json, msgs: &[Msg], tools: &[ToolDef], keep_images: bool) {
    let mut turns = fold_turns(msgs, Flat::OpenAi { keep_images });
    if let (Some(guide), Some(first)) = (text_tool_guide(tools), turns.first_mut()) {
        first.text = if first.text.is_empty() {
            guide
        } else {
            format!("{guide}\n{}", first.text)
        };
    }
    let mut sep = Sep::default();
    for t in &turns {
        sep.next(j);
        if t.images.is_empty() {
            text_message(j, t.role, &t.text);
        } else {
            j.raw("{\"content\":[");
            for image in &t.images {
                image_part(j, image);
                j.raw(",");
            }
            text_part(j, &t.text);
            j.raw("],\"role\":\"");
            j.raw(t.role);
            j.raw("\"}");
        }
    }
}

fn text_part(j: &mut Json, text: &str) {
    j.raw("{\"text\":");
    j.str(text);
    j.raw(",\"type\":\"text\"}");
}

fn image_part(j: &mut Json, (media_type, data): &(String, String)) {
    j.raw("{\"image_url\":{\"url\":");
    j.display(&format_args!("data:{media_type};base64,{data}"));
    j.raw("},\"type\":\"image_url\"}");
}

pub(super) fn openai_request_at(p: &Provider, base: &str) -> crate::net::Request {
    let req = agent()
        .post(&format!("{}/chat/completions", base.trim_end_matches('/')))
        .set("content-type", "application/json");
    match &p.api_key {
        Some(key) => req.set("authorization", &format!("Bearer {key}")),
        None => req,
    }
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

// Token counts from an OpenAI-shape `usage` object. `prompt_tokens` includes
// the cached portion, which is split out so `input` means uncached tokens
// like the Anthropic shape. None when the chunk carries no usage object.
pub(super) fn openai_usage(v: &Value) -> Option<Usage> {
    let u = v.get("usage")?.as_object()?;
    let count = |k: &str| u.get(k).and_then(Value::as_u64);
    let cached = u
        .get("prompt_tokens_details")
        .and_then(|d| d["cached_tokens"].as_u64());
    Some(usage_of(
        count("prompt_tokens").unwrap_or(0),
        cached,
        count("completion_tokens").unwrap_or(0),
    ))
}

fn usage_of(prompt: u64, cached: Option<u64>, completion: u64) -> Usage {
    let cached = cached.unwrap_or(0).min(prompt);
    Usage {
        input: prompt - cached,
        output: completion,
        cache_read: cached,
        cache_write: 0,
    }
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

// One streamed chunk, with only the fields the parser reads; strings borrow
// from the line (see `borrowed`). Unknown fields are skipped unparsed.
#[derive(Deserialize, Default)]
struct Chunk<'a> {
    #[serde(default)]
    error: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    usage: Obj<ChunkUsage>,
    #[serde(borrow, default)]
    choices: Option<Vec<Obj<Choice<'a>>>>,
}

#[derive(Deserialize, Default)]
struct ChunkUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Obj<TokenDetails>,
}

#[derive(Deserialize, Default)]
struct TokenDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Deserialize, Default)]
struct Choice<'a> {
    #[serde(borrow, default, deserialize_with = "opt_str")]
    finish_reason: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    delta: Obj<Delta<'a>>,
}

#[derive(Deserialize, Default)]
struct Delta<'a> {
    #[serde(borrow, default, deserialize_with = "opt_str")]
    reasoning_content: Option<Cow<'a, str>>,
    #[serde(borrow, default, deserialize_with = "opt_str")]
    reasoning: Option<Cow<'a, str>>,
    #[serde(borrow, default, deserialize_with = "opt_str")]
    content: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    tool_calls: Option<Vec<Obj<CallDelta<'a>>>>,
}

#[derive(Deserialize, Default)]
struct CallDelta<'a> {
    #[serde(default)]
    index: Option<u64>,
    #[serde(borrow, default, deserialize_with = "opt_str")]
    id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    function: Obj<FunctionDelta<'a>>,
}

#[derive(Deserialize, Default)]
struct FunctionDelta<'a> {
    #[serde(borrow, default, deserialize_with = "opt_str")]
    name: Option<Cow<'a, str>>,
    #[serde(borrow, default, deserialize_with = "opt_str")]
    arguments: Option<Cow<'a, str>>,
}

impl<'a> Chunk<'a> {
    // The same fields out of a `Value`, for a chunk the typed read refused:
    // whatever has an unexpected type counts as absent.
    fn from_value(v: &'a Value) -> Chunk<'a> {
        let usage = v.get("usage").and_then(Value::as_object).map(|u| {
            let count = |k: &str| u.get(k).and_then(Value::as_u64);
            ChunkUsage {
                prompt_tokens: count("prompt_tokens"),
                completion_tokens: count("completion_tokens"),
                prompt_tokens_details: Obj(Some(TokenDetails {
                    cached_tokens: u
                        .get("prompt_tokens_details")
                        .and_then(|d| d["cached_tokens"].as_u64()),
                })),
            }
        });
        let choice = &v["choices"][0];
        let delta = &choice["delta"];
        let tool_calls = delta["tool_calls"].as_array().map(|tcs| {
            tcs.iter()
                .map(|tc| {
                    Obj(Some(CallDelta {
                        index: tc["index"].as_u64(),
                        id: value_str(&tc["id"]),
                        function: Obj(Some(FunctionDelta {
                            name: value_str(&tc["function"]["name"]),
                            arguments: value_str(&tc["function"]["arguments"]),
                        })),
                    }))
                })
                .collect()
        });
        Chunk {
            error: None,
            usage: Obj(usage),
            choices: Some(vec![Obj(Some(Choice {
                finish_reason: value_str(&choice["finish_reason"]),
                delta: Obj(Some(Delta {
                    reasoning_content: value_str(&delta["reasoning_content"]),
                    reasoning: value_str(&delta["reasoning"]),
                    content: value_str(&delta["content"]),
                    tool_calls,
                })),
            }))]),
        }
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
        let tolerant;
        let chunk = match serde_json::from_str::<Obj<Chunk>>(data) {
            Ok(Obj(Some(chunk))) if chunk.error.is_none() => chunk,
            // An {"error": …} chunk, or one with a field of an unexpected
            // type: read it the tolerant way.
            _ => {
                let Ok(v) = serde_json::from_str::<Value>(data) else {
                    return false;
                };
                // vLLM, LiteLLM, OpenRouter and friends report mid-stream
                // failures as an {"error": …} chunk rather than an HTTP status.
                if let Some(e) = openai_error(&v) {
                    stream_err = Some(e);
                    return true;
                }
                tolerant = v;
                Chunk::from_value(&tolerant)
            }
        };
        // With stream_options.include_usage the final chunk (empty choices)
        // carries the totals; some servers put usage on every chunk instead.
        if let Some(u) = chunk.usage.0 {
            usage = usage_of(
                u.prompt_tokens.unwrap_or(0),
                u.prompt_tokens_details.0.and_then(|d| d.cached_tokens),
                u.completion_tokens.unwrap_or(0),
            );
        }
        let Some(choice) = chunk
            .choices
            .and_then(|c| c.into_iter().next())
            .and_then(|c| c.0)
        else {
            return false;
        };
        // finish_reason rides on the last content-bearing chunk (or one after).
        if let Some(r) = &choice.finish_reason {
            stop_reason = Some(normalize_stop_reason(r));
        }
        let delta = choice.delta.0.unwrap_or_default();
        if let Some(r) = delta.reasoning_content.or(delta.reasoning) {
            on_thinking(&r);
        }
        if let Some(t) = &delta.content {
            route_openai_content(
                t,
                &mut in_think,
                &mut think_carry,
                &mut text,
                on_text,
                on_thinking,
            );
        }
        for tc in delta.tool_calls.into_iter().flatten() {
            let tc = tc.0.unwrap_or_default();
            let idx = tc.index.unwrap_or(0) as usize;
            while pending.len() <= idx {
                pending.push((String::new(), String::new(), String::new()));
            }
            let e = &mut pending[idx];
            if let Some(id) = tc.id.filter(|id| !id.is_empty()) {
                e.0 = id.into_owned();
            }
            let function = tc.function.0.unwrap_or_default();
            if let Some(name) = function.name.filter(|name| !name.is_empty()) {
                e.1 = name.into_owned();
            }
            if let Some(args) = &function.arguments {
                e.2.push_str(args);
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
