//! Ollama's native /api/chat: context window, request bodies, and reply parsing.

use super::*;

// Used by the "ollama" preset. Two things the OpenAI-compat /v1 endpoint
// cannot express: options.num_ctx (without it Ollama silently truncates the
// prompt to the server-default window) and repeat_penalty=1.0 (the 1.1
// default corrupts tool-call JSON and code output).

// Hard cap on the requested window: larger allocations trade tokens/s and
// VRAM for headroom most local machines don't have.
pub(super) const OLLAMA_CTX_CAP: u64 = 32_768;
// Assumed window when /api/show doesn't reveal the model max.
pub(super) const OLLAMA_CTX_FLOOR: u32 = 8_192;

// Model max context from an /api/show response. The model_info key is
// architecture-prefixed ("llama.context_length", "qwen2.context_length", …),
// so scan for the ".context_length" suffix rather than guessing the arch.
pub(super) fn show_context_length(v: &Value) -> Option<u64> {
    v["model_info"]
        .as_object()?
        .iter()
        .find(|(k, _)| k.ends_with(".context_length"))
        .and_then(|(_, val)| val.as_u64())
}

// A Modelfile `num_ctx`, if the model was created with one. /api/show's
// "parameters" field is a flat text blob, one "key value" pair per line.
pub(super) fn show_num_ctx(v: &Value) -> Option<u64> {
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
pub(super) fn ollama_pick_ctx(model_max: Option<u64>, modelfile_ctx: Option<u64>) -> u32 {
    let picked = match (modelfile_ctx, model_max) {
        (Some(n), Some(max)) => n.min(max),
        (Some(n), None) => n,
        (None, Some(max)) => max.min(OLLAMA_CTX_CAP),
        (None, None) => u64::from(OLLAMA_CTX_FLOOR),
    };
    u32::try_from(picked).unwrap_or(u32::MAX)
}

// What /api/show says about the configured model.
pub(super) enum Show {
    /// The num_ctx to request.
    Known(u32),
    /// The server answered: it has no model by that name.
    Missing,
    /// No /api/show at all: an Ollama old enough to need /v1.
    Unsupported,
    /// Nothing listens at the address.
    Refused,
    /// No usable answer this time (busy, slow): ask again next request.
    NoAnswer,
}

pub(super) fn probe_show(p: &Provider) -> Show {
    let root = ollama_root(&p.base_url);
    let res = agent()
        .post(&format!("{root}/api/show"))
        .timeout(Duration::from_secs(3))
        .send_json(json!({"model": p.model}));
    match res {
        Ok(resp) => match resp.into_json::<Value>() {
            Ok(v) => {
                // Ollama 0.6+ lists what the model can do ("completion",
                // "tools", "vision", …); older servers send no list.
                if let Some(caps) = v["capabilities"].as_array() {
                    remember_vision(p, caps.iter().any(|c| c == "vision"));
                }
                Show::Known(ollama_pick_ctx(show_context_length(&v), show_num_ctx(&v)))
            }
            Err(_) => Show::NoAnswer,
        },
        // Ollama's own 404 names the model; a router's "404 page not
        // found" means the endpoint itself is missing.
        Err(crate::net::Error::Status(404, resp)) => {
            if model_missing(&resp.into_string().unwrap_or_default().to_ascii_lowercase()) {
                Show::Missing
            } else {
                Show::Unsupported
            }
        }
        Err(e) if io_kind(&e) == Some(std::io::ErrorKind::ConnectionRefused) => Show::Refused,
        Err(_) => Show::NoAnswer,
    }
}

// The num_ctx for /api/chat: Some(n) for the native path, None for an old
// server that only has /v1, Err when there is nothing to send to (the model
// is not installed, or nothing listens). Only a definitive answer is kept
// for the session (the OnceLock, which lib.rs pre-seeds when settings set
// context_tokens): a server that is down or busy is asked again on the next
// request, so one started after bwn gets the native path and its window.
pub(super) fn ollama_window(p: &Provider) -> Result<Option<u32>, String> {
    if let Some(known) = p.ollama_ctx.get() {
        return Ok(*known);
    }
    match probe_show(p) {
        Show::Known(n) => {
            let _ = p.ollama_ctx.set(Some(n));
            Ok(Some(n))
        }
        Show::Unsupported => {
            let _ = p.ollama_ctx.set(None);
            Ok(None)
        }
        Show::Missing => Err(missing_model_msg(p)),
        Show::Refused => Err(server_down_msg(&p.base_url)),
        Show::NoAnswer => Ok(Some(OLLAMA_CTX_FLOOR)),
    }
}

/// The window the server reported for this model, probing it if nobody has
/// yet; None when it is not known (yet).
pub fn ollama_ctx(p: &Provider) -> Option<u32> {
    let _ = ollama_window(p);
    p.ollama_ctx.get().copied().flatten()
}

/// A problem worth a line under the startup banner: the configured Ollama
/// model is not installed, or nothing answers at the address yet.
pub fn startup_problem(p: &Provider) -> Option<String> {
    if p.protocol != Protocol::OllamaNative {
        return None;
    }
    ollama_window(p).err()
}

pub(super) fn missing_model_msg(p: &Provider) -> String {
    let base = origin(&p.base_url);
    // Names come from whatever answers on the Ollama port.
    let installed: Vec<String> = ollama_models(&p.base_url)
        .iter()
        .map(|m| crate::tui::sanitize_terminal(m).into_owned())
        .collect();
    match installed.first() {
        Some(first) => {
            let shown = installed
                .iter()
                .take(8)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "model {m} is not installed on {base} — installed: {shown} · ollama pull {m}, or {use_}{first}",
                m = p.model,
                use_ = if in_session() { "/model " } else { "--model " }
            )
        }
        None => format!(
            "model {m} is not installed on {base}, and Ollama has no models yet — ollama pull {m}",
            m = p.model
        ),
    }
}

// One-line heads-up, once per process, when falling back to /v1: without
// num_ctx the server may silently truncate long prompts.
pub(super) fn warn_ollama_fallback() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        crate::report::notice(
            "  ⚠ ollama: /api/show unavailable — using the OpenAI-compat endpoint (long prompts may be truncated to the server-default context window)",
        );
    });
}

/// What shapes an Ollama /api/chat request besides the conversation.
pub(super) struct OllamaBody<'a> {
    pub(super) model: &'a str,
    pub(super) temperature: Option<f64>,
    pub(super) max_tokens: Option<u32>,
    pub(super) num_ctx: u32,
    /// The native API has one switch for thinking-capable models: any effort
    /// other than Off turns it on (the model then streams a `thinking`
    /// field), unless the model refused it earlier in the session.
    pub(super) think: bool,
    pub(super) stream: bool,
    /// For models whose template has no tool support (gemma3, for one): the
    /// system message stays, with the text tool guide appended; tool traffic
    /// folds into user/assistant turns and images stay in each user turn's
    /// `images` array.
    pub(super) flatten: bool,
}

// The request body, written once (see `wire` and `anthropic_bytes`). Message
// roles mirror the OpenAI shape, but tool-call arguments are JSON *objects*,
// images are a bare base64 array on the user message, and sampling knobs
// nest under "options". "stream" defaults to true server-side, so it's
// always explicit.
pub(super) fn ollama_bytes(msgs: &[Msg], tools: &[ToolDef], o: &OllamaBody) -> Vec<u8> {
    let mut j = Json::with_capacity(size_hint(msgs, tools));
    j.raw("{\"messages\":[");
    if o.flatten {
        flat_messages(&mut j, msgs, tools);
    } else {
        native_messages(&mut j, msgs);
    }
    j.raw("],\"model\":");
    j.str(o.model);
    // num_ctx is the reason the native path exists — see the section
    // comment. Ollama's 1.1 repeat_penalty default penalizes the repetition
    // inherent in JSON and code; 1.0 disables it.
    j.raw(",\"options\":{\"num_ctx\":");
    j.val(&o.num_ctx);
    j.raw(",\"num_predict\":");
    j.val(&o.max_tokens.unwrap_or(4096));
    j.raw(",\"repeat_penalty\":1.0,\"temperature\":");
    j.val(&o.temperature.unwrap_or(0.2));
    j.raw(if o.stream {
        "},\"stream\":true"
    } else {
        "},\"stream\":false"
    });
    if o.think {
        j.raw(",\"think\":true");
    }
    if !tools.is_empty() && !o.flatten {
        // Same schema wrapper as the OpenAI shape.
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
    // Ollama tool-call messages carry no ids on the wire, so tool results are
    // threaded by name instead: map call ids to names while walking the
    // transcript, then stamp tool_name on each result.
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
                // Raw base64 strings; the server sniffs the media type.
                image_message(j, "user", text, images.iter());
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
                        // arguments is an object here, not OpenAI's string.
                        j.raw("{\"function\":{\"arguments\":");
                        j.val(&c.input);
                        j.raw(",\"name\":");
                        j.str(&c.name);
                        j.raw("}}");
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
                    j.raw(",\"role\":\"tool\"");
                    if let Some(name) = call_names.get(r.id.as_str()) {
                        j.raw(",\"tool_name\":");
                        j.str(name);
                    }
                    j.raw("}");
                }
                // Images go on a user turn, the role every vision template
                // renders them for.
                if let Some(intro) = tool_images_intro(results, &call_names) {
                    sep.next(j);
                    let images = results.iter().flat_map(|r| r.images.iter());
                    image_message(j, "user", &intro, images);
                }
            }
        }
    }
}

fn flat_messages(j: &mut Json, msgs: &[Msg], tools: &[ToolDef]) {
    let mut system = SystemText(msgs).to_string();
    if let Some(guide) = text_tool_guide(tools) {
        if !system.is_empty() {
            system.push_str("\n\n");
        }
        system.push_str(&guide);
    }
    let mut sep = Sep::default();
    if !system.is_empty() {
        sep.next(j);
        text_message(j, "system", &system);
    }
    for t in fold_turns(msgs, Flat::Ollama) {
        sep.next(j);
        if t.images.is_empty() {
            text_message(j, t.role, &t.text);
        } else {
            image_message(j, t.role, &t.text, t.images.iter().copied());
        }
    }
}

// `{"content":…,"images":[…],"role":…}`.
fn image_message<'a>(
    j: &mut Json,
    role: &str,
    content: &str,
    images: impl Iterator<Item = &'a (String, String)>,
) {
    j.raw("{\"content\":");
    j.str(content);
    j.raw(",\"images\":[");
    let mut sep = Sep::default();
    for (_, data) in images {
        sep.next(j);
        j.str(data);
    }
    j.raw("],\"role\":\"");
    j.raw(role);
    j.raw("\"}");
}

pub(super) fn ollama_request(p: &Provider) -> crate::net::Request {
    // Local server: no auth header.
    agent()
        .post(&format!("{}/api/chat", ollama_root(&p.base_url)))
        .set("content-type", "application/json")
}

// Ollama reports prompt/response token counts on the final object.
pub(super) fn ollama_usage_into(v: &Value, into: &mut Usage) {
    if let Some(n) = v["prompt_eval_count"].as_u64() {
        into.input = n;
    }
    if let Some(n) = v["eval_count"].as_u64() {
        into.output = n;
    }
}

// Native Ollama sends tool-call arguments as an object; tolerate the string
// form some proxies emit by running it through the same INVALID_ARGS guard.
pub(super) fn ollama_call_args(args: &Value) -> Value {
    match args {
        Value::String(s) => parse_args(s),
        Value::Null => json!({}),
        other => other.clone(),
    }
}

// Ollama assigns no tool-call ids; synthesize one from the call's position so
// results can be threaded back through the transcript.
pub(super) fn ollama_call(i: usize, tc: &Value) -> ToolCall {
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

pub(super) fn ollama_parse(v: Value) -> Result<Reply, String> {
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
    let mut usage = Usage::default();
    ollama_usage_into(&v, &mut usage);
    Ok(Reply {
        text,
        calls,
        stop_reason,
        usage,
        ..Default::default()
    })
}

// Iterate an NDJSON stream (one JSON object per line — Ollama's native
// streaming format, not SSE), handing each parsed value to `f`; `f` returns
// true to stop (the "done": true line). Same transport contract as
// `for_each_sse`: a mid-stream read error is Err, natural EOF is success.
pub(super) fn for_each_ndjson(
    reader: impl Read,
    mut f: impl FnMut(&Value) -> bool,
) -> Result<(), String> {
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

pub(super) fn ollama_stream(
    reader: impl Read,
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) -> Result<Reply, String> {
    let mut text = String::new();
    let mut in_think = false;
    let mut think_carry = String::new();
    let mut stop_reason: Option<String> = None;
    let mut stream_err: Option<String> = None;
    let mut usage = Usage::default();
    let mut calls: Vec<ToolCall> = Vec::new();
    let mut done = false;
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
                route_openai_content(
                    t,
                    &mut in_think,
                    &mut think_carry,
                    &mut text,
                    on_text,
                    on_thinking,
                );
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
            done = true;
            stop_reason = v["done_reason"].as_str().map(normalize_stop_reason);
            ollama_usage_into(v, &mut usage);
            return true;
        }
        false
    })?;
    if let Some(e) = stream_err {
        return Err(e);
    }
    if !done {
        return Err(STREAM_TRUNCATED.to_string());
    }
    flush_think_carry(&think_carry, in_think, &mut text, on_text);
    Ok(Reply {
        text,
        calls,
        stop_reason,
        usage,
        ..Default::default()
    })
}
