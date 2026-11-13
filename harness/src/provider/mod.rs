// The model wire layer. One enum, three `match` arms — Anthropic Messages, the
// OpenAI /chat/completions shape (llama.cpp, LM Studio, OpenRouter, Groq, and
// Hugging Face), and Ollama's native /api/chat (which alone accepts num_ctx —
// the OpenAI-compat endpoint silently truncates prompts to the server default).
// No trait objects: the call site matches.
//
// Each protocol has a body builder + a request builder, shared by the blocking
// `complete()` and the streaming `stream()` so there's exactly one place that
// knows each vendor's JSON shape.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{Effort, Protocol};
use crate::tools::ToolDef;
use crate::usage::Usage;

pub struct Provider {
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub context_tokens: usize, // model context window, for compaction thresholds
    pub temperature: Option<f64>, // sampling temperature; None → per-protocol default
    pub max_tokens: Option<u32>, // response token cap; None → per-protocol default
    /// Reasoning depth; `Off` leaves every request body exactly as before.
    pub effort: Effort,
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
    /// Images the tool returned (a file read, a screenshot), as
    /// `(media_type, base64_data)` pairs like `Msg::UserImages`. Anthropic
    /// takes them inside the tool_result; the other protocols get them in a
    /// user turn right after the round's tool messages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<(String, String)>,
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
    /// Token counts from the server's `usage` block; zero when it sent none.
    pub usage: Usage,
    /// Anthropic `thinking`/`redacted_thinking` blocks (with signatures),
    /// verbatim, so a tool-use turn can be replayed with its reasoning — the
    /// API rejects a thinking-enabled tool round whose assistant turn lost them.
    pub thinking_blocks: Vec<Value>,
}

// One process-wide agent so the TLS connection is pooled and kept alive across
// every ReAct step — each agent iteration is an HTTP round-trip, and skipping the
// handshake after the first call is a free latency win on multi-step tasks.
//
// No overall deadline: it would also cover the streamed body and cut off any
// reply that takes longer to generate. The read timeout is per read, so it only
// fires on a stalled stream. Probes set their own short per-request timeouts.
// Redirects are off because ureq forwards custom headers such as x-api-key to
// whatever host a 3xx names; send_raw turns a 3xx into an error instead.
fn agent() -> &'static crate::net::Client {
    static A: std::sync::OnceLock<crate::net::Client> = std::sync::OnceLock::new();
    A.get_or_init(|| {
        crate::net::Client::new(|b| {
            b.timeout_connect(Duration::from_secs(15))
                .timeout_read(Duration::from_secs(read_timeout_secs()))
                .redirects(0)
        })
    })
}

fn read_timeout_secs() -> u64 {
    std::env::var("BWN_READ_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(300)
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
    pub fn openai_stream(
        reader: impl Read,
        on_text: &mut dyn FnMut(&str),
        on_thinking: &mut dyn FnMut(&str),
    ) -> Result<Reply, String> {
        super::openai_stream(reader, on_text, on_thinking)
    }
    pub fn anthropic_stream(
        reader: impl Read,
        on_text: &mut dyn FnMut(&str),
        on_thinking: &mut dyn FnMut(&str),
    ) -> Result<Reply, String> {
        super::anthropic_stream(reader, on_text, on_thinking)
    }
    // The bytes a streaming request puts on the wire, as the request path
    // builds them.
    pub fn anthropic_request_bytes(model: &str, msgs: &[Msg], tools: &[ToolDef]) -> Vec<u8> {
        let shape = AnthropicBody {
            model,
            max_tokens: None,
            effort: Effort::Off,
            stream: true,
        };
        super::anthropic_bytes(msgs, tools, &shape)
    }
    pub fn openai_request_bytes(model: &str, msgs: &[Msg], tools: &[ToolDef]) -> Vec<u8> {
        let shape = OpenAiBody {
            model,
            temperature: None,
            max_tokens: None,
            effort: Effort::Off,
            stream: true,
            flatten: None,
        };
        super::openai_bytes(msgs, tools, &shape)
    }
    pub fn ollama_request_bytes(model: &str, msgs: &[Msg], tools: &[ToolDef]) -> Vec<u8> {
        let shape = OllamaBody {
            model,
            temperature: None,
            max_tokens: None,
            num_ctx: 32_768,
            think: false,
            stream: true,
            flatten: false,
        };
        super::ollama_bytes(msgs, tools, &shape)
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
    ollama_models_checked(base_url).unwrap_or_default()
}

/// `ollama_models`, with None when nothing (or not Ollama) answered, so a
/// server with no models can be told from one that is not running.
pub fn ollama_models_checked(base_url: &str) -> Option<Vec<String>> {
    let root = ollama_root(base_url);
    let v: Value = agent()
        .get(&format!("{root}/api/tags"))
        .timeout(Duration::from_secs(2))
        .call()
        .ok()?
        .into_json()
        .ok()?;
    Some(
        v["models"]
            .as_array()?
            .iter()
            .filter_map(|m| m["name"].as_str().map(str::to_string))
            .collect(),
    )
}

// Models a running OpenAI-compatible server offers (GET {base}/models): LM
// Studio, llama.cpp, vLLM and friends. Empty on any failure.
pub fn openai_models(base_url: &str) -> Vec<String> {
    openai_models_checked(base_url).unwrap_or_default()
}

/// `openai_models`, with None when nothing answered.
pub fn openai_models_checked(base_url: &str) -> Option<Vec<String>> {
    openai_models_keyed(base_url, None)
}

/// `openai_models_checked` for a gateway that lists its models only to a
/// caller with its key.
pub fn openai_models_keyed(base_url: &str, key: Option<&str>) -> Option<Vec<String>> {
    let mut req = agent()
        .get(&format!("{}/models", base_url.trim_end_matches('/')))
        .timeout(Duration::from_secs(2));
    if let Some(k) = key {
        req = req.set("Authorization", &format!("Bearer {k}"));
    }
    let v: Value = req.call().ok()?.into_json().ok()?;
    Some(
        v["data"]
            .as_array()?
            .iter()
            .filter_map(|m| m["id"].as_str().map(str::to_string))
            .collect(),
    )
}

// ── what a local server reports about its model ───────────────────────────

/// What a local OpenAI-compatible server reports about a model it serves.
#[derive(Debug, Default, PartialEq)]
pub struct Served {
    /// The context window it loaded the model with.
    pub window: Option<usize>,
    /// Whether the model takes images, when the server says.
    pub vision: Option<bool>,
    /// How many requests the server answers at once (llama.cpp's slots).
    pub slots: Option<usize>,
}

/// Ask a local OpenAI-compatible server about `model`: llama.cpp's /props
/// (per-slot n_ctx, `modalities.vision`), LM Studio's /api/v0/models
/// (loaded context length, `type` vlm or llm), or vLLM's /models
/// (max_model_len). Empty when the server says nothing.
pub fn served_model(base_url: &str, model: &str) -> Served {
    let root = ollama_root(base_url);
    let get = |url: String| -> Option<Value> {
        agent()
            .get(&url)
            .timeout(Duration::from_secs(1))
            .call()
            .ok()?
            .into_json()
            .ok()
    };
    let entry = |v: Value| -> Option<Value> {
        v["data"]
            .as_array()?
            .iter()
            .find(|m| {
                m["id"]
                    .as_str()
                    .is_some_and(|id| id.eq_ignore_ascii_case(model))
            })
            .cloned()
    };
    let size = |n: Option<u64>| n.and_then(|n| usize::try_from(n).ok()).filter(|n| *n > 0);
    if let Some(v) = get(format!("{root}/props")) {
        let n = v["default_generation_settings"]["n_ctx"]
            .as_u64()
            .or_else(|| v["n_ctx"].as_u64());
        if n.is_some() {
            return Served {
                window: size(n),
                vision: v["modalities"]["vision"].as_bool(),
                slots: size(v["total_slots"].as_u64()),
            };
        }
    }
    if let Some(m) = get(format!("{root}/api/v0/models")).and_then(entry) {
        return Served {
            window: size(m["loaded_context_length"].as_u64()),
            vision: match m["type"].as_str() {
                Some("vlm") => Some(true),
                Some("llm") => Some(false),
                _ => None,
            },
            slots: None,
        };
    }
    match get(format!("{}/models", base_url.trim_end_matches('/'))).and_then(entry) {
        Some(m) => Served {
            window: size(m["max_model_len"].as_u64()),
            vision: None,
            slots: None,
        },
        None => Served::default(),
    }
}

// Image support a server reported, by "origin#model".
fn reported_visions() -> &'static std::sync::Mutex<std::collections::HashMap<String, bool>> {
    static V: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, bool>>> =
        std::sync::OnceLock::new();
    V.get_or_init(Default::default)
}

/// Record what the server said about image support for `p`'s model.
pub fn remember_vision(p: &Provider, vision: bool) {
    if let Ok(mut m) = reported_visions().lock() {
        m.insert(window_key(p), vision);
    }
}

/// Whether the server says `p`'s model takes images: Ollama's /api/show
/// capabilities (asked now if nobody has yet), or what a llama.cpp or LM
/// Studio server reported at startup. None when it never said.
pub fn reported_vision(p: &Provider) -> Option<bool> {
    let known = || reported_visions().lock().ok()?.get(&window_key(p)).copied();
    if p.protocol == Protocol::OllamaNative && known().is_none() {
        let _ = ollama_window(p);
    }
    known()
}

// Windows a server reported, or settings set, by "origin#model": only a
// window known this way may refuse a message before it is sent.
fn known_windows() -> &'static std::sync::Mutex<std::collections::HashMap<String, usize>> {
    static W: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, usize>>> =
        std::sync::OnceLock::new();
    W.get_or_init(Default::default)
}

fn window_key(p: &Provider) -> String {
    format!("{}#{}", origin(&p.base_url), p.model)
}

/// Record that `p.context_tokens` is the real window (reported by the
/// server or set in settings), not a preset's guess.
pub fn remember_window(p: &Provider) {
    if let Ok(mut m) = known_windows().lock() {
        m.insert(window_key(p), p.context_tokens);
    }
}

// Requests a local server reported it answers at once, by "origin#model".
fn reported_slots() -> &'static std::sync::Mutex<std::collections::HashMap<String, usize>> {
    static S: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, usize>>> =
        std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

/// Record how many requests the server said it answers at once.
pub fn remember_slots(p: &Provider, n: usize) {
    if let Ok(mut m) = reported_slots().lock() {
        m.insert(window_key(p), n);
    }
}

/// How many requests `p`'s server said it answers at once, if it did.
pub fn served_slots(p: &Provider) -> Option<usize> {
    reported_slots().lock().ok()?.get(&window_key(p)).copied()
}

/// Whether `p.context_tokens` is the real window, not a preset's guess.
pub fn window_is_known(p: &Provider) -> bool {
    known_window(p).is_some()
}

fn known_window(p: &Provider) -> Option<usize> {
    if p.protocol == Protocol::OllamaNative {
        if let Some(Some(n)) = p.ollama_ctx.get() {
            return Some(*n as usize);
        }
    }
    known_windows().lock().ok()?.get(&window_key(p)).copied()
}

/// "4.1k" style, as the footer shows token counts.
pub fn short_tokens(n: usize) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

/// Why the last message cannot be sent, when it cannot fit the server's
/// window on its own: compacting the history cannot make room for it, and
/// the server's own refusal arrives only after the upload. `request` refuses
/// such a message; callers drop it from the conversation.
pub fn oversized_message(p: &Provider, msgs: &[Msg]) -> Option<String> {
    let window = known_window(p)?;
    let text = match msgs.last()? {
        Msg::User(t) => t,
        Msg::UserImages { text, .. } => text,
        _ => return None,
    };
    let tokens = text.len() / 4;
    if tokens <= window {
        return None;
    }
    let held = short_tokens(window);
    Some(match biggest_attachment(text) {
        Some((path, chars)) if chars / 4 > window / 2 => format!(
            "{path} is about {} tokens and the server holds {held} — attach a range such as @{path}:1-200",
            short_tokens(chars / 4)
        ),
        _ => format!(
            "this message is about {} tokens and the server holds {held} — send less, or attach part of a file (@file:1-200)",
            short_tokens(tokens)
        ),
    })
}

// The largest `[file: <path>]` block in a message (how attachments are
// written into it), as the path to show and its length in characters.
fn biggest_attachment(text: &str) -> Option<(String, usize)> {
    let starts: Vec<usize> = text.match_indices("[file: ").map(|(i, _)| i).collect();
    let cwd = std::env::current_dir().ok();
    starts
        .iter()
        .enumerate()
        .filter_map(|(k, &at)| {
            let end = starts.get(k + 1).copied().unwrap_or(text.len());
            let header = &text[at + "[file: ".len()..end];
            let path = header.split(']').next()?.trim();
            let shown = cwd
                .as_deref()
                .and_then(|c| Path::new(path).strip_prefix(c).ok())
                .map_or_else(|| path.to_string(), |r| r.display().to_string());
            Some((shown, end - at))
        })
        .max_by_key(|(_, len)| *len)
}

// ── public entry points ────────────────────────────────────────────────────
pub fn complete(p: &Provider, msgs: &[Msg], tools: &[ToolDef]) -> Result<Reply, String> {
    let mut sink = |_: &str| {};
    let mut noop = |_: &str| {};
    request(p, msgs, tools, false, &mut sink, &mut noop)
}

fn is_local_url(url: &str) -> bool {
    url.contains("localhost")
        || url.contains("127.0.0.1")
        || url.contains("::1")
        || url.contains("0.0.0.0")
}

/// The model names a server says it serves (Ollama's installed list, or an
/// OpenAI-compatible /models); empty when it says nothing.
pub fn served_names(p: &Provider) -> Vec<String> {
    match p.protocol {
        Protocol::OllamaNative => ollama_models(&p.base_url),
        _ => openai_models(&p.base_url),
    }
}

/// The names to offer for a model the server does not know: up to eight.
pub fn served_summary(names: &[String]) -> String {
    let shown: Vec<String> = names
        .iter()
        .take(8)
        .map(|n| crate::tui::sanitize_terminal(n).into_owned())
        .collect();
    match names.len().saturating_sub(shown.len()) {
        0 => shown.join(", "),
        more => format!("{}, and {more} more", shown.join(", ")),
    }
}

/// A local server that lists what it serves, none of it the model asked
/// for, yet answered the probe: llama.cpp's server runs whatever it loaded
/// and ignores the name, so the footer would name a model that is not
/// answering.
pub fn unlisted_note(model: &str, names: &[String]) -> Option<String> {
    if names.is_empty() || names.iter().any(|n| n.eq_ignore_ascii_case(model)) {
        return None;
    }
    Some(format!(
        "the server lists only {}; it answered the probe, but a server like llama.cpp's answers with whatever it loaded, whatever name is sent",
        served_summary(names)
    ))
}

/// One-token probe through the real completion path — proves the key is
/// accepted, the model exists, and the server is reachable, so a model swap
/// can be validated before it's declared successful. Costs ≤1 output token.
pub fn validate(p: &Provider) -> Result<Option<String>, String> {
    let probe = Provider {
        protocol: p.protocol,
        base_url: p.base_url.clone(),
        api_key: p.api_key.clone(),
        model: p.model.clone(),
        context_tokens: p.context_tokens,
        temperature: None,
        max_tokens: Some(1),
        effort: Effort::Off,
        ollama_ctx: std::sync::OnceLock::new(),
    };
    match complete(&probe, &[Msg::User("ping".into())], &[]) {
        Ok(_) => Ok(None),
        Err(e)
            if p.protocol == Protocol::OpenAi && (e.contains("404") || e.contains("not found")) =>
        {
            if p.model != "local-model" && is_local_url(&p.base_url) {
                let mut fallback_probe = probe;
                fallback_probe.model = "local-model".to_string();
                if complete(&fallback_probe, &[Msg::User("ping".into())], &[]).is_ok() {
                    return Ok(Some("local-model".to_string()));
                }
            }
            Err(e)
        }
        Err(e) => Err(e),
    }
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

// A restrictive chat template (notably Gemma's on llama.cpp) rejected the
// standard message roles: it can't render the `system`/`tool` roles or requires
// strictly alternating user/assistant turns. Detected from the server's 400 so
// the retry only fires for models that need it (qwen's template accepts them).
fn is_template_role_error(e: &str) -> bool {
    let l = e.to_lowercase();
    l.contains("roles must alternate")
        || l.contains("conversation roles")
        || l.contains("unable to generate parser")
        || l.contains("does not support")
        || (l.contains("template") && l.contains("role"))
}

// A 400 that means "this server can't take the native request shape": either
// the template/role rejection above, or a server that rejects the `tools` /
// `tool_choice` fields outright. Only a 400 qualifies — 5xx/429 are retried
// upstream, and a 401/404 is never fixed by reshaping the prompt.
fn needs_flatten(e: &str) -> bool {
    is_template_role_error(e) || (e.starts_with("HTTP 400") && e.to_lowercase().contains("tool"))
}

// Whether that rejection is worth remembering for the whole session: only a
// 400 that names tools, the chat template, or message roles. A one-off
// "does not support …" (an image, say) still gets the single retry but
// mustn't strip native tools from every later request.
fn flatten_is_sticky(e: &str) -> bool {
    let l = e.to_lowercase();
    e.starts_with("HTTP 400")
        && (l.contains("tool") || l.contains("template") || l.contains("role"))
}

// Endpoints that rejected the native shape this session, so later calls go
// straight to the flattened body instead of paying a failed request each time.
fn flattened_endpoints() -> &'static std::sync::Mutex<Vec<String>> {
    static F: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> = std::sync::OnceLock::new();
    F.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn flatten_remembered(url: &str) -> bool {
    flattened_endpoints()
        .lock()
        .map(|v| v.iter().any(|u| u == url))
        .unwrap_or(false)
}

fn remember_flatten(url: &str) {
    if let Ok(mut v) = flattened_endpoints().lock() {
        if !v.iter().any(|u| u == url) {
            v.push(url.to_string());
        }
    }
}

// Send an OpenAI-compatible request. Native tools and roles go first — local
// servers (llama.cpp, LM Studio, vLLM, Ollama's /v1) mostly accept them and
// then call tools properly. On a 400 that names tools/roles/template, retry
// once with the flattened, tool-less body and remember the endpoint so the
// rest of the session skips the failing attempt. `base` is explicit because
// the Ollama fallback path targets {host}/v1 while the provider's base_url
// is the bare host root.
fn openai_exchange(
    p: &Provider,
    base: &str,
    msgs: &[Msg],
    tools: &[ToolDef],
    streaming: bool,
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) -> Result<Reply, String> {
    let req = openai_request_at(p, base);
    let body = |flatten: Option<bool>| {
        let shape = OpenAiBody {
            model: &p.model,
            temperature: p.temperature,
            max_tokens: p.max_tokens,
            effort: p.effort,
            stream: streaming,
            flatten,
        };
        openai_bytes(msgs, tools, &shape)
    };
    let url = req.url().to_string();
    if flatten_remembered(&url) {
        let resp = send_raw(req, body(Some(true)))?;
        return finish(resp, streaming, OPENAI, on_text, on_thinking);
    }
    match send_raw(req.clone(), body(None)) {
        Ok(resp) => finish(resp, streaming, OPENAI, on_text, on_thinking),
        Err(e) if needs_flatten(&e) => {
            if flatten_is_sticky(&e) {
                remember_flatten(&url);
                crate::report::info(
                    "  ⟳ server rejected native tools/roles — using flattened prompts for the rest of the session",
                );
            }
            // A one-off image rejection retries without the images.
            let keep_images = !e.to_lowercase().contains("image");
            let resp = send_raw(req, body(Some(keep_images)))?;
            finish(resp, streaming, OPENAI, on_text, on_thinking)
        }
        Err(e) => Err(e),
    }
}

// Send an Ollama /api/chat request. Native tools go first; a model whose
// template has no tool support ("<model> does not support tools", gemma3 for
// one) gets one retry with the flattened body, and the model is remembered so
// the rest of the session skips the failing attempt.
fn ollama_exchange(
    p: &Provider,
    msgs: &[Msg],
    tools: &[ToolDef],
    num_ctx: u32,
    streaming: bool,
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) -> Result<Reply, String> {
    let req = ollama_request(p);
    let key = format!("{}#{}", req.url(), p.model);
    let no_think_key = format!("no-think:{key}");
    let think = p.effort != Effort::Off && !flatten_remembered(&no_think_key);
    let body = |flatten: bool| {
        let shape = OllamaBody {
            model: &p.model,
            temperature: p.temperature,
            max_tokens: p.max_tokens,
            num_ctx,
            think,
            stream: streaming,
            flatten,
        };
        ollama_bytes(msgs, tools, &shape)
    };
    if flatten_remembered(&key) {
        return finish(
            send_raw(req, body(true))?,
            streaming,
            OLLAMA,
            on_text,
            on_thinking,
        );
    }
    match send_raw(req.clone(), body(false)) {
        Ok(resp) => finish(resp, streaming, OLLAMA, on_text, on_thinking),
        // A reasoning effort asked of a model without thinking: drop the
        // option for this model for the rest of the session and retry, which
        // still gets the tools fallback below if that is needed too.
        Err(e) if think && ollama_rejects_thinking(&e) => {
            remember_flatten(&no_think_key);
            crate::report::info(&format!(
                "  ⟳ {} does not support thinking — continuing without it",
                p.model
            ));
            ollama_exchange(p, msgs, tools, num_ctx, streaming, on_text, on_thinking)
        }
        Err(e) if ollama_rejects_tools(&e) => {
            remember_flatten(&key);
            crate::report::info(&format!(
                "  ⟳ {} has no native tool calling — describing tools in the prompt instead",
                p.model
            ));
            finish(
                send_raw(req, body(true))?,
                streaming,
                OLLAMA,
                on_text,
                on_thinking,
            )
        }
        Err(e) => Err(e),
    }
}

// Ollama's 400 for `think` sent to a model without thinking support.
fn ollama_rejects_thinking(e: &str) -> bool {
    e.starts_with("HTTP 400") && e.to_lowercase().contains("does not support thinking")
}

// Ollama's 400 for a model without tool support.
fn ollama_rejects_tools(e: &str) -> bool {
    e.starts_with("HTTP 400") && e.to_lowercase().contains("does not support tools")
}

type StreamParser =
    fn(InterruptibleReader, &mut dyn FnMut(&str), &mut dyn FnMut(&str)) -> Result<Reply, String>;

/// Reads a model reply with the protocol's own parser: its stream parser
/// over an interruptible reader when streaming, else its JSON parser.
fn finish(
    resp: crate::net::Response,
    streaming: bool,
    (stream, parse): Parsers,
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) -> Result<Reply, String> {
    if streaming {
        stream(
            InterruptibleReader::new(resp.into_reader()),
            on_text,
            on_thinking,
        )
    } else {
        let v = resp
            .into_json::<Value>()
            .map_err(|e| format!("bad JSON from server: {e}"))?;
        parse(v)
    }
}

type Parsers = (StreamParser, fn(Value) -> Result<Reply, String>);
const ANTHROPIC: Parsers = (anthropic_stream, anthropic_parse);
const OPENAI: Parsers = (openai_stream, openai_parse);
const OLLAMA: Parsers = (ollama_stream, ollama_parse);

// The one choke point every model call passes through (blocking, streaming,
// sub-agents, the /model probe): each completed reply is booked into the
// session ledger and its thinking blocks are kept for replay.
fn request(
    p: &Provider,
    msgs: &[Msg],
    tools: &[ToolDef],
    streaming: bool,
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) -> Result<Reply, String> {
    // A spend cap that cannot be counted is not sent past (a /model swap
    // to an unpriced model mid-session lands here; startup exits earlier).
    if let Some(why) = budget_guard(p) {
        return Err(why);
    }
    if let Some(why) = oversized_message(p, msgs) {
        return Err(why);
    }
    let reply = request_inner(p, msgs, tools, streaming, on_text, on_thinking)
        .map_err(|e| explain_failure(p, e))?;
    crate::usage::record(&p.model, is_free(p), reply.usage);
    remember_thinking(&reply);
    Ok(reply)
}

// Local servers cost nothing whatever the model is called.
fn is_free(p: &Provider) -> bool {
    p.protocol == Protocol::OllamaNative || is_local_url(&p.base_url)
}

/// Why the session's spend cap cannot hold for this provider's model, if it
/// cannot (see `usage::unenforceable_budget`).
pub fn budget_guard(p: &Provider) -> Option<String> {
    crate::usage::unenforceable_budget(&p.model, is_free(p))
}

fn request_inner(
    p: &Provider,
    msgs: &[Msg],
    tools: &[ToolDef],
    streaming: bool,
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) -> Result<Reply, String> {
    match p.protocol {
        Protocol::Anthropic => {
            let req = anthropic_request(p)?;
            let shape = AnthropicBody {
                model: &p.model,
                max_tokens: p.max_tokens,
                effort: p.effort,
                stream: streaming,
            };
            let body = anthropic_bytes(msgs, tools, &shape);
            finish(
                send_raw(req, body)?,
                streaming,
                ANTHROPIC,
                on_text,
                on_thinking,
            )
        }
        Protocol::OpenAi => {
            openai_exchange(p, &p.base_url, msgs, tools, streaming, on_text, on_thinking)
        }
        Protocol::OllamaNative => {
            // The native path exists to set options.num_ctx (unavailable on
            // the OpenAI-compat endpoint, which silently truncates prompts)
            // and to neutralize the repeat_penalty=1.1 default. A server
            // without /api/show is an older one: fall back to the {root}/v1
            // OpenAI-compat endpoint used before, with a one-time heads-up
            // about the context limitation.
            match ollama_window(p)? {
                Some(num_ctx) => {
                    ollama_exchange(p, msgs, tools, num_ctx, streaming, on_text, on_thinking)
                }
                None => {
                    warn_ollama_fallback();
                    let base = format!("{}/v1", ollama_root(&p.base_url));
                    openai_exchange(p, &base, msgs, tools, streaming, on_text, on_thinking)
                }
            }
        }
    }
}

// ── HTTP plumbing ──────────────────────────────────────────────────────────
// Esc and Ctrl-C only raise a flag; a socket read on the calling thread can't
// be cancelled. So the request runs on a worker thread and this thread waits
// in 50 ms slices, checking the flag. Without it an interrupt waited for the
// server's reply, which for a local model loading or reading an image takes
// minutes. An abandoned worker's response is dropped when it arrives, which
// closes the connection and stops the server's generation.
fn interruptible<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    use std::sync::mpsc::RecvTimeoutError;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(v) => return Ok(v),
            Err(RecvTimeoutError::Timeout) => {
                if crate::tui::interrupted() {
                    return Err("interrupted".to_string());
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err("request failed: worker thread exited".to_string())
            }
        }
    }
}

// A streamed response body read on a worker thread, so a stream that goes
// quiet (a local model still thinking) can still be interrupted. Dropping the
// reader ends the worker at its next chunk, closing the connection.
struct InterruptibleReader {
    rx: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    buf: Vec<u8>,
    pos: usize,
}

impl InterruptibleReader {
    fn new(mut inner: Box<dyn Read + Send + Sync>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || loop {
            let mut chunk = vec![0u8; 16 * 1024];
            match inner.read(&mut chunk) {
                Ok(0) => {
                    let _ = tx.send(Ok(Vec::new()));
                    break;
                }
                Ok(n) => {
                    chunk.truncate(n);
                    if tx.send(Ok(chunk)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        });
        Self {
            rx,
            buf: Vec::new(),
            pos: 0,
        }
    }
}

impl Read for InterruptibleReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        use std::sync::mpsc::RecvTimeoutError;
        while self.pos >= self.buf.len() {
            match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Ok(chunk)) if chunk.is_empty() => return Ok(0),
                Ok(Ok(chunk)) => {
                    self.buf = chunk;
                    self.pos = 0;
                }
                Ok(Err(e)) => return Err(e),
                Err(RecvTimeoutError::Timeout) => {
                    if crate::tui::interrupted() {
                        return Err(std::io::Error::other("interrupted"));
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

// `body` is the serialised request: it moves to the worker, and every attempt
// sends those same bytes.
fn send_raw(req: crate::net::Request, body: Vec<u8>) -> Result<crate::net::Response, String> {
    interruptible(move || send_raw_blocking(req, &body))?
}

fn send_raw_blocking(
    req: crate::net::Request,
    body: &[u8],
) -> Result<crate::net::Response, String> {
    let mut retries = 0;
    let mut delay_ms = 500;
    // A proxy URL bwn cannot use fails every attempt the same way.
    let route = agent().proxy_route(req.url());
    if let Some(r) = &route {
        if let Some(why) = &r.unusable {
            return Err(format!("{}: {why}", r.var));
        }
    }

    loop {
        match req.clone().send_json_bytes(body) {
            Ok(resp) if (300..400).contains(&resp.status()) => {
                let to = resp.header("location").unwrap_or("?").to_string();
                return Err(format!(
                    "HTTP {}: server redirected to {} (not followed, so the API key is never sent to another host); point base_url at the final address",
                    resp.status(),
                    redact(&to)
                ));
            }
            Ok(resp) => return Ok(resp),
            Err(crate::net::Error::Status(code, resp)) => {
                let server_wait = retry_after_ms(resp.header("retry-after"));
                let detail = resp.into_string().unwrap_or_default();
                let budget = retries_for(code, &detail);
                if retries < budget {
                    retries += 1;
                    let wait = server_wait.unwrap_or_else(|| jitter(delay_ms));
                    // Say what's happening — a silent 10s backoff reads as a
                    // frozen UI.
                    let why = if code == 429 {
                        "rate-limited"
                    } else {
                        "server busy"
                    };
                    crate::report::info(&format!(
                        "  ⟳ {why} (HTTP {code}) — retrying in {:.1}s ({retries}/{budget})",
                        wait as f64 / 1000.0,
                    ));
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
                if let Some(hint) = crate::net::cert_error_hint(&e) {
                    return Err(format!(
                        "connection failed: {}\n{hint}",
                        redact(&e.to_string())
                    ));
                }
                let kind = io_kind(&e);
                // A refused connection means nothing listens on that port,
                // on this machine or another; one retry covers a server that
                // is restarting, more will not start it.
                let refused = kind == Some(std::io::ErrorKind::ConnectionRefused);
                let budget = if refused { 1 } else { max_retries() };
                // Only failures before the request went out are safe to
                // resend. A read timeout or reset after the POST body was
                // delivered may mean the server is still generating (and
                // billing) the first attempt.
                if transport_retryable(&e) && retries < budget {
                    retries += 1;
                    let wait = jitter(delay_ms);
                    crate::report::info(&format!(
                        "  ⟳ connection error — retrying in {:.1}s ({retries}/{budget})",
                        wait as f64 / 1000.0,
                    ));
                    std::thread::sleep(Duration::from_millis(wait));
                    delay_ms = (delay_ms * 2).min(10_000);
                    continue;
                }
                return Err(match &route {
                    Some(r) => proxy_failure(req.url(), r, &e, kind),
                    None => transport_failure(req.url(), &e, kind),
                });
            }
        }
    }
}

// A transport failure on a proxied request: name the proxy and the setting,
// since the server itself was never reached.
fn proxy_failure(
    url: &str,
    r: &crate::net::ProxyRoute,
    e: &crate::net::Error,
    kind: Option<std::io::ErrorKind>,
) -> String {
    let (target, host) = target_of(url);
    let named = format!("the proxy in {} ({})", r.var, r.shown);
    let crate::net::Error::Transport(t) = e else {
        return transport_failure(url, e, kind);
    };
    match t.kind() {
        crate::net::ErrorKind::ProxyUnauthorized => proxy_credentials_msg(r),
        crate::net::ErrorKind::ProxyConnect => format!(
            "{named} would not connect to {target} — if {host} is on your network, add it to NO_PROXY"
        ),
        crate::net::ErrorKind::ConnectionFailed
            if kind == Some(std::io::ErrorKind::ConnectionRefused) =>
        {
            format!(
                "{named} refused the connection — is it running (VPN connected)? To reach {host} without it, add {host} to NO_PROXY"
            )
        }
        crate::net::ErrorKind::ConnectionFailed => format!(
            "{named} did not answer — is it running and reachable? To reach {host} without it, add {host} to NO_PROXY"
        ),
        crate::net::ErrorKind::Dns => {
            format!("{named} could not be found — check the host name in {}", r.var)
        }
        _ => format!("{}\n  through {named}", transport_failure(url, e, kind)),
    }
}

fn proxy_credentials_msg(r: &crate::net::ProxyRoute) -> String {
    if r.has_credentials {
        format!(
            "the proxy in {} ({}) rejected its user name and password — check them",
            r.var, r.shown
        )
    } else {
        format!(
            "the proxy wants credentials — {}=http://user:pass@{}",
            r.var, r.host_port
        )
    }
}

// "host:port" a request was for, and the host alone.
fn target_of(url: &str) -> (String, String) {
    match url::Url::parse(url) {
        Ok(u) => {
            let host = u.host_str().unwrap_or("").to_string();
            let port = u.port_or_known_default().unwrap_or(0);
            (format!("{host}:{port}"), host)
        }
        Err(_) => (origin(url).to_string(), origin(url).to_string()),
    }
}

fn transport_retryable(e: &crate::net::Error) -> bool {
    matches!(e, crate::net::Error::Transport(t)
        if matches!(t.kind(), crate::net::ErrorKind::ConnectionFailed | crate::net::ErrorKind::Dns))
}

// The io::Error under a transport failure (refused, timed out, …), found by
// walking the source chain ureq wraps it in.
fn io_kind(e: &crate::net::Error) -> Option<std::io::ErrorKind> {
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(s) = src {
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            return Some(io.kind());
        }
        src = s.source();
    }
    None
}

// What a transport failure means and what to do about it. Library text
// ("Network Error: … status line") stays only as the last resort.
fn transport_failure(url: &str, e: &crate::net::Error, kind: Option<std::io::ErrorKind>) -> String {
    use std::io::ErrorKind;
    let text = e.to_string();
    let failed_to_connect = matches!(e, crate::net::Error::Transport(t) if t.kind() == crate::net::ErrorKind::ConnectionFailed);
    match kind {
        Some(ErrorKind::ConnectionRefused) if failed_to_connect => server_down_msg(url),
        Some(ErrorKind::TimedOut | ErrorKind::WouldBlock) if failed_to_connect => format!(
            "nothing answered at {} within 15 s — is the machine on and the server listening on that address?",
            origin(url)
        ),
        Some(ErrorKind::TimedOut | ErrorKind::WouldBlock) => read_timeout_msg(url),
        _ if text.contains("timed out reading") => read_timeout_msg(url),
        _ => format!("connection failed: {}", redact(&text)),
    }
}

/// A stalled response: the server took the request but sent nothing back
/// within the read timeout.
fn read_timeout_msg(url: &str) -> String {
    let secs = read_timeout_secs();
    format!(
        "no answer from {} within {secs} s — a slow machine or a long prompt can need more time: \
         set BWN_READ_TIMEOUT_SECS (now {secs}) higher, e.g. BWN_READ_TIMEOUT_SECS={}",
        origin(url),
        (secs * 3).max(900)
    )
}

// scheme://host:port of a URL; the request path means nothing to the user.
fn origin(url: &str) -> &str {
    match url.find("://") {
        Some(i) => url[i + 3..].find('/').map_or(url, |j| &url[..i + 3 + j]),
        None => url,
    }
}

/// Retries for a transient failure: BWN_MAX_RETRIES, else 3.
fn max_retries() -> u32 {
    retries_env().unwrap_or(DEFAULT_RETRIES)
}

fn retries_env() -> Option<u32> {
    std::env::var("BWN_MAX_RETRIES")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map(|n| n.min(MAX_RETRIES_CAP))
}

const DEFAULT_RETRIES: u32 = 3;
const MAX_RETRIES_CAP: u32 = 20;
// A local server answers 503 while it loads a model, which can take minutes
// on a slow disk: about two minutes of backoff before giving up.
const LOADING_RETRIES: u32 = 14;

/// How many times a failed status is worth retrying. Only the status decides
/// whether a failure is transient; the body can only make it definitive
/// (out of memory never fixes itself) or patient (a model still loading).
fn retries_for(code: u16, body: &str) -> u32 {
    if !is_transient_status(code) || out_of_memory(body) {
        return 0;
    }
    if code == 503 && body.to_ascii_lowercase().contains("loading") {
        return retries_env().unwrap_or(LOADING_RETRIES);
    }
    max_retries()
}

fn out_of_memory(body: &str) -> bool {
    let l = body.to_ascii_lowercase();
    l.contains("requires more system memory")
        || l.contains("out of memory")
        || l.contains("insufficient memory")
        || l.contains("failed to allocate")
}

// ±20% so clients that failed together don't all retry in the same instant.
fn jitter(ms: u64) -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let pct = 80 + u64::from(nanos % 41);
    ms * pct / 100
}

// A refused connection. Model servers (anything not https on a public name)
// get the start-the-server advice; a hosted API gets the network one.
fn server_down_msg(url: &str) -> String {
    let base = origin(url);
    let wrong = if in_session() {
        "/model <name> or `buildwithnexus init` sets a new one."
    } else {
        "--base-url <url> (or base_url in settings.json) sets another; `buildwithnexus init` sets it up again."
    };
    if url.starts_with("https://") && !is_local_url(url) {
        return format!(
            "nothing is answering at {base} — check the address (base_url) and your network or VPN"
        );
    }
    format!(
        "nothing is answering at {base} — is the model server running?\n  \
         Ollama: `ollama serve` · LM Studio / llama.cpp: start its server\n  \
         Wrong address? {wrong}"
    )
}

// ── failure wording ────────────────────────────────────────────────────────

static IN_SESSION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The interactive UI is running: failure hints may name its slash commands.
/// Elsewhere (`bwn run`, `review`, the Action) they name flags, settings and
/// environment variables, which a script can use.
pub fn set_in_session() {
    IN_SESSION.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn in_session() -> bool {
    IN_SESSION.load(std::sync::atomic::Ordering::Relaxed)
}

/// Where the key a request sent came from. A variable in the environment
/// wins over the stored key, so `login` does not replace it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum KeySource {
    /// The environment variable that holds it (`ANTHROPIC_API_KEY`).
    Env(String),
    /// The key saved by setup or `login`, for this provider or endpoint.
    Stored(String),
}

/// Which variable or store `p`'s key came from, found the way `build_provider`
/// chose it: a key equal to a key variable's value is that variable's (the
/// provider's own first), else it is the stored one.
pub(crate) fn key_source(p: &Provider) -> Option<KeySource> {
    let key = p.api_key.as_deref()?;
    let base = crate::config::endpoint_origin(&p.base_url);
    let preset = crate::config::PRESETS
        .iter()
        .find(|x| crate::config::endpoint_origin(x.base_url) == base && !x.env_key.is_empty());
    let own = preset.map(|x| x.env_key);
    let others = crate::config::PRESETS
        .iter()
        .map(|x| x.env_key)
        .chain([crate::config::CUSTOM_KEY]);
    let from_env = own
        .into_iter()
        .chain(others)
        .filter(|n| !n.is_empty())
        .find(|n| std::env::var(n).is_ok_and(|v| v == key));
    Some(match from_env {
        Some(name) => KeySource::Env(name.to_string()),
        None => KeySource::Stored(match preset {
            Some(x) => x.label.to_string(),
            None => base,
        }),
    })
}

/// Who was asked, for the wording of a failure.
struct FailureContext<'a> {
    model: &'a str,
    base: &'a str,
    has_key: bool,
    /// Where that key came from, when known.
    key: Option<KeySource>,
    ollama: bool,
    /// Hints name slash commands (the terminal UI) rather than flags.
    in_session: bool,
}

/// A failed request in the person's terms: what happened and what to do on
/// the first line, the status and the server's own message on the second.
/// Errors that are not an HTTP status pass through unchanged.
fn explain_failure(p: &Provider, e: String) -> String {
    let ctx = FailureContext {
        model: &p.model,
        base: origin(&p.base_url),
        has_key: p.api_key.is_some(),
        key: key_source(p),
        ollama: p.protocol == Protocol::OllamaNative,
        in_session: in_session(),
    };
    let Some((code, body)) = e
        .strip_prefix("HTTP ")
        .and_then(|rest| rest.split_once(": "))
        .and_then(|(code, body)| Some((code.parse::<u16>().ok()?, body)))
    else {
        if e.starts_with("stream read failed") && e.contains("timed out") {
            return read_timeout_msg(&p.base_url);
        }
        return e;
    };
    if (300..400).contains(&code) {
        return e;
    }
    match agent().proxy_route(&p.base_url) {
        Some(r) => match proxy_status(code, body, &r, &p.base_url) {
            Some(own) => own,
            None => format!(
                "{}\n  HTTP {code}: {}\n  sent through the proxy in {} ({})",
                status_advice(code, body, &ctx),
                server_message(body),
                r.var,
                r.shown
            ),
        },
        None => format!(
            "{}\n  HTTP {code}: {}",
            status_advice(code, body, &ctx),
            server_message(body)
        ),
    }
}

// A status the proxy answered itself: 407, or a bare refusal with no body
// (an API server explains its errors; a proxy's 403 usually does not).
fn proxy_status(code: u16, body: &str, r: &crate::net::ProxyRoute, url: &str) -> Option<String> {
    if code == 407 {
        return Some(proxy_credentials_msg(r));
    }
    if !body.trim().is_empty() {
        return None;
    }
    let (target, host) = target_of(url);
    Some(format!(
        "HTTP {code} from proxy {} for {target} — if {host} is on your network, add {host} to NO_PROXY ({} is set)",
        r.shown, r.var
    ))
}

/// The first line of a failed status: what it means for this request.
fn status_advice(code: u16, body: &str, ctx: &FailureContext) -> String {
    let lower = body.to_ascii_lowercase();
    let retried = |n: u32| match n {
        0 => String::new(),
        1 => " after 1 retry".to_string(),
        n => format!(" after {n} retries"),
    };
    // What replaces a key or picks a model: a slash command in a session,
    // otherwise what a script can use.
    let (replace, add, pick) = if ctx.in_session {
        (
            "/login to replace it",
            "/login to add one",
            "/model to pick an installed one",
        )
    } else {
        (
            "`buildwithnexus login` replaces it, or set the provider's API key variable (CUSTOM_API_KEY for --base-url)",
            "`buildwithnexus login` adds one, or set CUSTOM_API_KEY",
            "--model <name> to use an installed one",
        )
    };
    let login = if ctx.in_session {
        "/login"
    } else {
        "`buildwithnexus login`"
    };
    match code {
        401 => match &ctx.key {
            // The variable overrides the stored key, so `login` would not help.
            Some(KeySource::Env(name)) if ctx.has_key => format!(
                "{name} (from your environment) was rejected (HTTP 401). Check or unset it; {login} only changes the stored key, which this variable overrides."
            ),
            Some(KeySource::Stored(what)) if ctx.has_key => {
                format!("The stored API key for {what} was rejected (HTTP 401). {login} replaces it.")
            }
            _ if ctx.has_key => format!("the API key was rejected by the provider — {replace}"),
            _ => format!("the server at {} wants an API key — {add}", ctx.base),
        },
        403 => format!(
            "the provider refused access to {} — check what the key is allowed to use, or {}",
            ctx.model,
            match &ctx.key {
                Some(KeySource::Env(name)) =>
                    format!("change {name} (from your environment; {login} does not replace it)"),
                Some(KeySource::Stored(_)) if ctx.in_session => replace.to_string(),
                Some(KeySource::Stored(_)) => format!("{login} replaces it"),
                None => replace.to_string(),
            }
        ),
        404 if model_missing(&lower) && ctx.ollama => format!(
            "model {} is not installed on {} — ollama pull {}, or {pick}",
            ctx.model, ctx.base, ctx.model
        ),
        404 if model_missing(&lower) => format!(
            "the server does not know model {} — {}, or check the name",
            ctx.model,
            if ctx.in_session { "/model lists what it serves" } else { "--model <name> picks one it serves" }
        ),
        404 => format!(
            "the server has nothing at this address — check base_url ({}); OpenAI-compatible servers usually end in /v1",
            ctx.base
        ),
        400 | 413 if context_overflow(&lower) => overflow_advice(body),
        413 => "the request is too large for the server — attach less, or /compact".to_string(),
        429 => format!(
            "rate-limited by the provider{} — wait a minute and try again (BWN_MAX_RETRIES sets the retries)",
            retried(retries_for(code, body))
        ),
        _ if out_of_memory(body) => memory_advice(body, ctx.model, ctx.in_session),
        503 if lower.contains("loading") => format!(
            "the server is still loading {}{} — wait for it to finish, then send again",
            ctx.model,
            retried(retries_for(code, body))
        ),
        _ if is_transient_status(code) => format!(
            "the server failed{} — BWN_MAX_RETRIES changes how often bwn retries",
            retried(retries_for(code, body))
        ),
        _ => "the server refused the request".to_string(),
    }
}

fn model_missing(lower: &str) -> bool {
    lower.contains("model")
        && (lower.contains("not found")
            || lower.contains("does not exist")
            || lower.contains("not installed")
            || lower.contains("no such model"))
}

fn context_overflow(lower: &str) -> bool {
    lower.contains("exceed_context_size")
        || lower.contains("context size")
        || lower.contains("context length")
        || lower.contains("context window")
        || lower.contains("prompt is too long")
        || lower.contains("too many tokens")
}

// "the prompt needs N tokens and the server holds M", from whichever numbers
// the server put in its message: llama.cpp's n_prompt_tokens/n_ctx fields,
// LM Studio's "first N tokens … context length of only M", OpenAI's
// "maximum context length is M … resulted in N", Anthropic's "N tokens > M".
fn overflow_advice(body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let err = if v["error"].is_object() {
        &v["error"]
    } else {
        &v
    };
    let field = |k: &str| err[k].as_u64().or_else(|| v[k].as_u64());
    let text = server_message(body);
    let nums = numbers_in(&text);
    let lower = text.to_ascii_lowercase();
    let (needed, held, fix) = if let (Some(n), Some(c)) = (field("n_prompt_tokens"), field("n_ctx"))
    {
        (
            Some(n),
            Some(c),
            "restart llama-server with a larger -c, or set context_tokens",
        )
    } else if lower.contains("trying to keep the first") {
        (
            nums.first().copied(),
            nums.get(1).copied(),
            "load the model in LM Studio with a larger context length, or set context_tokens",
        )
    } else if lower.contains("maximum context length") {
        (
            nums.get(1).copied(),
            nums.first().copied(),
            "/compact, or start a new session",
        )
    } else if lower.contains("prompt is too long") {
        (
            nums.first().copied(),
            nums.get(1).copied(),
            "/compact, or start a new session",
        )
    } else {
        (None, None, "/compact, or set context_tokens")
    };
    match (needed, held) {
        (Some(n), Some(c)) => format!(
            "the prompt needs {} tokens and the server holds {} — {fix}",
            group_digits(n),
            group_digits(c)
        ),
        _ => format!("the prompt is larger than the model's context window — {fix}"),
    }
}

// "model requires more system memory (5.6 GiB) than is available (3.1 GiB)"
// is Ollama's; anything else that says out of memory gets the general line.
fn memory_advice(body: &str, model: &str, in_session: bool) -> String {
    let smaller = if in_session {
        "/model"
    } else {
        "--model <name>"
    };
    let text = server_message(body);
    let sizes: Vec<&str> = text
        .split('(')
        .skip(1)
        .filter_map(|s| s.split(')').next())
        .filter(|s| s.ends_with("iB") || s.ends_with("GB") || s.ends_with("MB"))
        .collect();
    match sizes.as_slice() {
        [need, free, ..] => format!(
            "the model needs {need} but {free} is free — pick a smaller model with {smaller}"
        ),
        _ => format!(
            "the server ran out of memory running {model} — pick a smaller model with {smaller}, or close other programs"
        ),
    }
}

fn numbers_in(text: &str) -> Vec<u64> {
    text.split(|c: char| !c.is_ascii_digit() && c != ',')
        .filter_map(|w| w.replace(',', "").parse::<u64>().ok())
        .collect()
}

fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The server's own words from an error body: `error.message`, a string
/// `error`, or `message`; the raw text when it is not JSON.
fn server_message(body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let msg = v["error"]["message"]
        .as_str()
        .or_else(|| v["error"].as_str())
        .or_else(|| v["message"].as_str())
        .map_or_else(|| body.trim().to_string(), str::to_string);
    let msg = if msg.is_empty() {
        "(no message)".to_string()
    } else {
        msg
    };
    redact(&msg).chars().take(300).collect()
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
pub(crate) fn redact(s: &str) -> String {
    // One pass: a token runs up to the next delimiter (whitespace or one of
    // "',:;()[]{}), and is judged from what the scan saw on the way. Text
    // between secrets is copied in whole runs.
    const DELIMITER: u8 = 1;
    const KEY_CHAR: u8 = 2;
    const CLASS: [u8; 128] = {
        let mut t = [0u8; 128];
        let mut i = 0;
        while i < 128 {
            let b = i as u8;
            t[i] = match b {
                b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | b' ' => DELIMITER,
                b'"' | b'\'' | b',' | b':' | b';' | b'(' | b')' | b'[' | b']' | b'{' | b'}' => {
                    DELIMITER
                }
                b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'-' | b'_' => KEY_CHAR,
                _ => 0,
            };
            i += 1;
        }
        t
    };
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copied = 0; // everything before this is already in `out`
    let mut start = 0; // where the current token begins
    let mut opaque = true; // the token so far is only key characters
    let mut i = 0;
    loop {
        // The width of the delimiter at `i`; None at the end of the text.
        let delimiter = match bytes.get(i) {
            None => None,
            Some(&b) if b < 128 => {
                let class = CLASS[b as usize];
                if class != DELIMITER {
                    opaque &= class == KEY_CHAR;
                    i += 1;
                    continue;
                }
                Some(1)
            }
            Some(_) => {
                let c = s[i..].chars().next().unwrap_or(' ');
                if !c.is_whitespace() {
                    opaque = false;
                    i += c.len_utf8();
                    continue;
                }
                Some(c.len_utf8())
            }
        };
        let token = &bytes[start..i];
        let secretish = token.len() >= 12
            && (token.starts_with(b"sk-")
                || token.starts_with(b"AIza")
                || token.starts_with(b"hf_")
                || token.starts_with(b"Bearer")
                || (token.len() > 32 && opaque));
        if secretish {
            out.push_str(&s[copied..start]);
            out.push_str("[redacted]");
            copied = i;
        }
        let Some(width) = delimiter else { break };
        i += width;
        start = i;
        opaque = true;
    }
    out.push_str(&s[copied..]);
    out
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
// Returns whether `f` stopped the stream; false means it hit EOF first, which
// callers treat as truncation unless they already saw a finish marker.
fn for_each_sse(reader: impl Read, mut f: impl FnMut(&str) -> bool) -> Result<bool, String> {
    // Reuse one buffer across lines instead of allocating a String per line.
    let mut reader = BufReader::with_capacity(32 * 1024, reader);
    let mut line = String::new();
    let mut last_poll = Instant::now();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(false),
            Err(e) => return Err(format!("stream read failed: {}", redact(&e.to_string()))),
            Ok(_) => {
                if last_poll.elapsed() >= Duration::from_millis(50) {
                    crate::tui::poll_typeahead();
                    last_poll = Instant::now();
                }
                if crate::tui::interrupted() {
                    return Err("interrupted".to_string());
                }
                if let Some(payload) = line.strip_prefix("data:") {
                    if f(payload.trim()) {
                        return Ok(true);
                    }
                }
            }
        }
    }
}

mod anthropic;
mod borrowed;
mod ollama;
mod openai;
mod wire;
use anthropic::*;
use borrowed::*;
use ollama::*;
pub use ollama::{ollama_ctx, startup_problem};
use openai::*;
use wire::*;

#[cfg(test)]
mod stream_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod wire_tests;
