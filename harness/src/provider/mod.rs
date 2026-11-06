// The model wire layer. One enum, three `match` arms — Anthropic Messages, the
// OpenAI /chat/completions shape (llama.cpp, LM Studio, OpenRouter, Groq, and
// Hugging Face), and Ollama's native /api/chat (which alone accepts num_ctx —
// the OpenAI-compat endpoint silently truncates prompts to the server default).
// No trait objects: the call site matches.
//
// Each protocol has a body builder + a request builder, shared by the blocking
// `complete()` and the streaming `stream()` so there's exactly one place that
// knows each vendor's JSON shape.

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

// Flatten a message's content (string or multimodal parts) to plain text.
fn message_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        return arr
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(" ");
    }
    String::new()
}

// One turn of a flattened transcript: text plus the images it carried, in
// the wire shape of whichever protocol the body uses.
struct FlatTurn {
    role: &'static str,
    text: String,
    images: Vec<Value>,
}

// Image parts of an OpenAI message: the `image_url` entries of a content array.
fn openai_images(m: &Value) -> Vec<Value> {
    m["content"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p["type"] == "image_url")
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

// Images of an Ollama message: the bare base64 strings in `images`.
fn ollama_images(m: &Value) -> Vec<Value> {
    m["images"].as_array().cloned().unwrap_or_default()
}

// Fold a transcript into strictly alternating user/assistant turns with no
// `system`/`tool` roles: system and tool-result content become user turns, an
// assistant turn's tool_calls render as text, and consecutive same-role turns
// merge (keeping every image). The first turn is always `user`.
fn fold_turns(msgs: &[Value], images_of: fn(&Value) -> Vec<Value>) -> Vec<FlatTurn> {
    let mut folded: Vec<FlatTurn> = Vec::new();
    for m in msgs {
        let text = message_text(&m["content"]);
        match m["role"].as_str().unwrap_or("user") {
            "assistant" => {
                let mut content = text;
                if let Some(calls) = m["tool_calls"].as_array() {
                    for c in calls {
                        let name = c["function"]["name"].as_str().unwrap_or("");
                        // OpenAI sends a JSON string, Ollama a JSON object.
                        let args = match &c["function"]["arguments"] {
                            Value::String(a) => a.clone(),
                            other => other.to_string(),
                        };
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(&format!("`{name}({args})`"));
                    }
                }
                if !content.is_empty() {
                    folded.push(FlatTurn {
                        role: "assistant",
                        text: content,
                        images: Vec::new(),
                    });
                }
            }
            "tool" => {
                if !text.is_empty() {
                    folded.push(FlatTurn {
                        role: "user",
                        text: format!("Tool result:\n{text}"),
                        images: Vec::new(),
                    });
                }
            }
            _ => folded.push(FlatTurn {
                role: "user",
                text,
                images: images_of(m),
            }),
        }
    }
    let mut merged: Vec<FlatTurn> = Vec::new();
    for turn in folded {
        if let Some(last) = merged.last_mut() {
            if last.role == turn.role {
                if !turn.text.is_empty() {
                    if !last.text.is_empty() {
                        last.text.push_str("\n\n");
                    }
                    last.text.push_str(&turn.text);
                }
                last.images.extend(turn.images);
                continue;
            }
        }
        merged.push(turn);
    }
    if merged.first().is_some_and(|t| t.role == "assistant") {
        merged.insert(
            0,
            FlatTurn {
                role: "user",
                text: String::new(),
                images: Vec::new(),
            },
        );
    }
    merged
}

// Instructions for calling tools in plain text, built from the native `tools`
// field that a flattened request drops. Without it the model would not know
// which tools exist. The JSON shape is the one the agent's text-call parser
// reads; `None` when the request had no tools.
fn text_tool_guide(tools: &Value) -> Option<String> {
    let tools = tools.as_array().filter(|t| !t.is_empty())?;
    let mut guide = String::from(
        "This server has no native tool calling. To use a tool, reply with only \
         a JSON object and nothing else:\n\
         {\"name\": \"<tool>\", \"arguments\": {\"<param>\": <value>}}\n\
         The result comes back in the next message. When the task is done, \
         answer in plain text.\n\nTools (a ? marks an optional parameter):\n",
    );
    for t in tools {
        let f = &t["function"];
        let Some(name) = f["name"].as_str() else {
            continue;
        };
        let required: Vec<&str> = f["parameters"]["required"]
            .as_array()
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        // Required parameters first, in schema order, then the optional ones.
        let mut params: Vec<String> = required.iter().map(|r| r.to_string()).collect();
        if let Some(props) = f["parameters"]["properties"].as_object() {
            for k in props.keys() {
                if !required.contains(&k.as_str()) {
                    params.push(format!("{k}?"));
                }
            }
        }
        let desc = f["description"].as_str().unwrap_or("");
        let first = desc
            .split(". ")
            .next()
            .unwrap_or(desc)
            .trim_end_matches('.');
        let first: String = first.chars().take(160).collect();
        guide.push_str(&format!("- {name}({}): {first}\n", params.join(", ")));
    }
    Some(guide)
}

// Rewrite an OpenAI request body into the shape restrictive templates accept
// (notably Gemma's on llama.cpp): strictly alternating user/assistant turns,
// no `system`/`tool` roles, and no native `tools` field. The tools are
// described in text at the top of the first turn instead. Images stay on the
// user turns that carried them unless `keep_images` is false (the server
// rejected them).
fn flatten_openai_messages(body: &mut Value, keep_images: bool) {
    let Some(msgs) = body["messages"].as_array() else {
        return;
    };
    let images_of: fn(&Value) -> Vec<Value> = if keep_images {
        openai_images
    } else {
        |_| Vec::new()
    };
    let mut turns = fold_turns(msgs, images_of);
    if let Some(guide) = text_tool_guide(&body["tools"]) {
        if let Some(first) = turns.first_mut() {
            first.text = if first.text.is_empty() {
                guide
            } else {
                format!("{guide}\n{}", first.text)
            };
        }
    }
    body["messages"] = json!(turns
        .into_iter()
        .map(|t| {
            if t.images.is_empty() {
                json!({"role": t.role, "content": t.text})
            } else {
                let mut parts = t.images;
                parts.push(json!({"type": "text", "text": t.text}));
                json!({"role": t.role, "content": parts})
            }
        })
        .collect::<Vec<_>>());
    if let Some(obj) = body.as_object_mut() {
        obj.remove("tools");
    }
}

// The same rewrite for Ollama's native /api/chat body, for models whose
// template has no tool support (gemma3, for one). The system message stays,
// with the text tool guide appended; tool traffic folds into user/assistant
// turns and images stay in each user turn's `images` array.
fn flatten_ollama_messages(body: &mut Value) {
    let Some(msgs) = body["messages"].as_array() else {
        return;
    };
    let mut system = String::new();
    let mut rest: Vec<Value> = Vec::new();
    for m in msgs {
        if m["role"] == "system" {
            system = message_text(&m["content"]);
        } else {
            rest.push(m.clone());
        }
    }
    if let Some(guide) = text_tool_guide(&body["tools"]) {
        if !system.is_empty() {
            system.push_str("\n\n");
        }
        system.push_str(&guide);
    }
    let mut out: Vec<Value> = Vec::new();
    if !system.is_empty() {
        out.push(json!({"role": "system", "content": system}));
    }
    for t in fold_turns(&rest, ollama_images) {
        let mut m = json!({"role": t.role, "content": t.text});
        if !t.images.is_empty() {
            m["images"] = json!(t.images);
        }
        out.push(m);
    }
    body["messages"] = json!(out);
    if let Some(obj) = body.as_object_mut() {
        obj.remove("tools");
    }
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

// Streaming flags for the OpenAI shape: `stream_options.include_usage` makes
// the server append a final chunk carrying the request's token counts.
fn mark_streaming(body: &mut Value) {
    body["stream"] = json!(true);
    body["stream_options"] = json!({"include_usage": true});
}

// Send an OpenAI-compatible request. Native tools and roles go first — local
// servers (llama.cpp, LM Studio, vLLM, Ollama's /v1) mostly accept them and
// then call tools properly. On a 400 that names tools/roles/template, retry
// once with the flattened, tool-less body and remember the endpoint so the
// rest of the session skips the failing attempt.
fn openai_exchange(
    req: ureq::Request,
    mut body: Value,
    streaming: bool,
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) -> Result<Reply, String> {
    if streaming {
        mark_streaming(&mut body);
    }
    let url = req.url().to_string();
    if flatten_remembered(&url) {
        flatten_openai_messages(&mut body, true);
        let resp = send_raw(req, body)?;
        return finish(resp, streaming, OPENAI, on_text, on_thinking);
    }
    match send_raw(req.clone(), body.clone()) {
        Ok(resp) => finish(resp, streaming, OPENAI, on_text, on_thinking),
        Err(e) if needs_flatten(&e) => {
            if flatten_is_sticky(&e) {
                remember_flatten(&url);
                crate::report::info(
                    "  ⟳ server rejected native tools/roles — using flattened prompts for the rest of the session",
                );
            }
            // A one-off image rejection retries without the images.
            flatten_openai_messages(&mut body, !e.to_lowercase().contains("image"));
            let resp = send_raw(req, body)?;
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
    req: ureq::Request,
    mut body: Value,
    streaming: bool,
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) -> Result<Reply, String> {
    let key = format!("{}#{}", req.url(), p.model);
    let no_think_key = format!("no-think:{key}");
    if flatten_remembered(&no_think_key) {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("think");
        }
    }
    if flatten_remembered(&key) {
        flatten_ollama_messages(&mut body);
        return finish(
            send_raw(req, body)?,
            streaming,
            OLLAMA,
            on_text,
            on_thinking,
        );
    }
    match send_raw(req.clone(), body.clone()) {
        Ok(resp) => finish(resp, streaming, OLLAMA, on_text, on_thinking),
        // A reasoning effort asked of a model without thinking: drop the
        // option for this model for the rest of the session and retry, which
        // still gets the tools fallback below if that is needed too.
        Err(e) if body.get("think").is_some() && ollama_rejects_thinking(&e) => {
            remember_flatten(&no_think_key);
            crate::report::info(&format!(
                "  ⟳ {} does not support thinking — continuing without it",
                p.model
            ));
            ollama_exchange(p, req, body, streaming, on_text, on_thinking)
        }
        Err(e) if ollama_rejects_tools(&e) => {
            remember_flatten(&key);
            crate::report::info(&format!(
                "  ⟳ {} has no native tool calling — describing tools in the prompt instead",
                p.model
            ));
            flatten_ollama_messages(&mut body);
            finish(
                send_raw(req, body)?,
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
    resp: ureq::Response,
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
            let (req, mut body) = anthropic_request(p, msgs, tools)?;
            if streaming {
                body["stream"] = json!(true);
            }
            finish(
                send_raw(req, body)?,
                streaming,
                ANTHROPIC,
                on_text,
                on_thinking,
            )
        }
        Protocol::OpenAi => {
            let (req, body) = openai_request_at(p, &p.base_url, msgs, tools);
            openai_exchange(req, body, streaming, on_text, on_thinking)
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
                    let (req, mut body) = ollama_request(p, msgs, tools, num_ctx);
                    if streaming {
                        body["stream"] = json!(true);
                    }
                    ollama_exchange(p, req, body, streaming, on_text, on_thinking)
                }
                None => {
                    warn_ollama_fallback();
                    let base = format!("{}/v1", ollama_root(&p.base_url));
                    let (req, body) = openai_request_at(p, &base, msgs, tools);
                    openai_exchange(req, body, streaming, on_text, on_thinking)
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

fn send_raw(req: ureq::Request, body: Value) -> Result<ureq::Response, String> {
    interruptible(move || send_raw_blocking(req, body))?
}

fn send_raw_blocking(req: ureq::Request, body: Value) -> Result<ureq::Response, String> {
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
        let req_clone = req.clone();
        let body_clone = body.clone();

        match req_clone.send_json(body_clone) {
            Ok(resp) if (300..400).contains(&resp.status()) => {
                let to = resp.header("location").unwrap_or("?").to_string();
                return Err(format!(
                    "HTTP {}: server redirected to {} (not followed, so the API key is never sent to another host); point base_url at the final address",
                    resp.status(),
                    redact(&to)
                ));
            }
            Ok(resp) => return Ok(resp),
            Err(ureq::Error::Status(code, resp)) => {
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
    e: &ureq::Error,
    kind: Option<std::io::ErrorKind>,
) -> String {
    let (target, host) = target_of(url);
    let named = format!("the proxy in {} ({})", r.var, r.shown);
    let ureq::Error::Transport(t) = e else {
        return transport_failure(url, e, kind);
    };
    match t.kind() {
        ureq::ErrorKind::ProxyUnauthorized => proxy_credentials_msg(r),
        ureq::ErrorKind::ProxyConnect => format!(
            "{named} would not connect to {target} — if {host} is on your network, add it to NO_PROXY"
        ),
        ureq::ErrorKind::ConnectionFailed
            if kind == Some(std::io::ErrorKind::ConnectionRefused) =>
        {
            format!(
                "{named} refused the connection — is it running (VPN connected)? To reach {host} without it, add {host} to NO_PROXY"
            )
        }
        ureq::ErrorKind::ConnectionFailed => format!(
            "{named} did not answer — is it running and reachable? To reach {host} without it, add {host} to NO_PROXY"
        ),
        ureq::ErrorKind::Dns => {
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

fn transport_retryable(e: &ureq::Error) -> bool {
    matches!(e, ureq::Error::Transport(t)
        if matches!(t.kind(), ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Dns))
}

// The io::Error under a transport failure (refused, timed out, …), found by
// walking the source chain ureq wraps it in.
fn io_kind(e: &ureq::Error) -> Option<std::io::ErrorKind> {
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
fn transport_failure(url: &str, e: &ureq::Error, kind: Option<std::io::ErrorKind>) -> String {
    use std::io::ErrorKind;
    let text = e.to_string();
    let failed_to_connect =
        matches!(e, ureq::Error::Transport(t) if t.kind() == ureq::ErrorKind::ConnectionFailed);
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
mod ollama;
mod openai;
use anthropic::*;
use ollama::*;
pub use ollama::{ollama_ctx, startup_problem};
use openai::*;

#[cfg(test)]
mod tests {
    #[test]
    fn a_name_the_server_does_not_list_is_noted_and_the_list_is_short() {
        let names = vec!["tinycoder-7b-q4_k_m.gguf".to_string()];
        assert!(unlisted_note("TinyCoder-7B-Q4_K_M.gguf", &names).is_none());
        assert!(unlisted_note("anything", &[]).is_none());
        let m = unlisted_note("tinycoder:9b", &names).unwrap();
        assert!(
            m.starts_with("the server lists only tinycoder-7b-q4_k_m.gguf;"),
            "{m}"
        );
        let many: Vec<String> = (0..11).map(|i| format!("m{i}")).collect();
        assert_eq!(
            served_summary(&many),
            "m0, m1, m2, m3, m4, m5, m6, m7, and 3 more"
        );
    }

    use super::*;
    use std::io::Cursor;

    #[test]
    fn server_down_msg_names_host_not_path() {
        let m = server_down_msg("http://127.0.0.1:11434/v1/chat/completions");
        assert!(m.contains("http://127.0.0.1:11434 "));
        assert!(!m.contains("/v1/"));
        assert!(m.contains("ollama serve"));
        // A model server on the local network gets the same advice.
        let m = server_down_msg("http://192.168.50.10:11434/api/chat");
        assert!(m.contains("nothing is answering at http://192.168.50.10:11434 "));
        assert!(m.contains("ollama serve"));
        // A hosted API gets the network advice instead.
        let m = server_down_msg("https://api.example.com/v1/chat/completions");
        assert!(m.contains("https://api.example.com"), "{m}");
        assert!(!m.contains("ollama serve"), "{m}");
    }

    fn ctx<'a>(model: &'a str, has_key: bool, ollama: bool) -> FailureContext<'a> {
        FailureContext {
            model,
            base: "http://192.168.50.10:11434",
            has_key,
            key: None,
            ollama,
            in_session: true,
        }
    }

    fn explained(code: u16, body: &str, c: &FailureContext) -> String {
        format!(
            "{}\n  HTTP {code}: {}",
            status_advice(code, body, c),
            server_message(body)
        )
    }

    #[test]
    fn a_script_is_told_flags_and_variables_not_slash_commands() {
        let mut c = ctx("gpt-4o", true, false);
        c.in_session = false;
        let m = explained(401, "{}", &c);
        assert!(
            m.starts_with("the API key was rejected by the provider — `buildwithnexus login`")
                && m.contains("CUSTOM_API_KEY")
                && !m.contains("/login"),
            "{m}"
        );
        c.has_key = false;
        let m = explained(401, "{}", &c);
        assert!(
            m.contains("wants an API key") && !m.contains("/login"),
            "{m}"
        );
        let m = explained(404, r#"{"error":"model not found"}"#, &c);
        assert!(m.contains("--model <name>") && !m.contains("/model"), "{m}");
        let m = explained(
            500,
            r#"{"error":"model requires more system memory (5.6 GiB) than is available (3.1 GiB)"}"#,
            &ctx("m", false, true),
        );
        assert!(
            m.contains("smaller model with /model"),
            "session wording stays: {m}"
        );
        let mut o = ctx("m", false, true);
        o.in_session = false;
        let m = explained(
            500,
            r#"{"error":"model requires more system memory (5.6 GiB) than is available (3.1 GiB)"}"#,
            &o,
        );
        assert!(
            m.contains("smaller model with --model <name>") && !m.contains("with /model"),
            "{m}"
        );
        // Not in a session (the default here): the unreachable-server hint names --base-url.
        let m = server_down_msg("http://127.0.0.1:1/v1");
        assert!(
            m.contains("--base-url <url>") && !m.contains("/model"),
            "{m}"
        );
    }

    fn provider_for(base_url: &str, key: Option<&str>) -> Provider {
        Provider {
            protocol: Protocol::OpenAi,
            base_url: base_url.into(),
            api_key: key.map(Into::into),
            model: "m".into(),
            context_tokens: 8192,
            temperature: None,
            max_tokens: None,
            effort: crate::config::Effort::Off,
            ollama_ctx: std::sync::OnceLock::new(),
        }
    }

    #[test]
    fn a_rejected_key_names_where_it_came_from() {
        let mut c = ctx("claude-sonnet-4-6", true, false);
        c.in_session = false;
        c.key = Some(KeySource::Env("ANTHROPIC_API_KEY".into()));
        let m = explained(401, "{}", &c);
        assert!(
            m.starts_with(
                "ANTHROPIC_API_KEY (from your environment) was rejected (HTTP 401). Check or unset it; `buildwithnexus login` only changes the stored key, which this variable overrides."
            ),
            "{m}"
        );
        // A key that came from the environment is not fixed by `login`, and
        // CUSTOM_API_KEY is only named when it is the variable in use.
        assert!(
            !m.contains("replaces it") && !m.contains("CUSTOM_API_KEY"),
            "{m}"
        );

        c.key = Some(KeySource::Stored("OpenAI".into()));
        let m = explained(401, "{}", &c);
        assert!(
            m.starts_with(
                "The stored API key for OpenAI was rejected (HTTP 401). `buildwithnexus login` replaces it."
            ),
            "{m}"
        );
        assert!(
            !m.contains("CUSTOM_API_KEY") && !m.contains("environment"),
            "{m}"
        );

        c.key = Some(KeySource::Env("CUSTOM_API_KEY".into()));
        let m = explained(401, "{}", &c);
        assert!(
            m.starts_with("CUSTOM_API_KEY (from your environment) was rejected"),
            "{m}"
        );

        // In a session the replacement is a slash command.
        c.in_session = true;
        c.key = Some(KeySource::Env("OPENAI_API_KEY".into()));
        let m = explained(401, "{}", &c);
        assert!(
            m.contains("Check or unset it; /login only changes the stored key"),
            "{m}"
        );
        assert!(!m.contains("buildwithnexus login"), "{m}");
        c.key = Some(KeySource::Stored("Groq".into()));
        let m = explained(401, "{}", &c);
        assert!(
            m.starts_with(
                "The stored API key for Groq was rejected (HTTP 401). /login replaces it."
            ),
            "{m}"
        );

        // 403 from an environment key does not send the user to `login`.
        c.in_session = false;
        c.key = Some(KeySource::Env("OPENAI_API_KEY".into()));
        let m = explained(403, "{}", &c);
        assert!(
            m.contains("change OPENAI_API_KEY (from your environment;"),
            "{m}"
        );
        assert!(!m.contains("CUSTOM_API_KEY"), "{m}");

        // The wording the setup check reads as a rejected key still does.
        for k in [
            Some(KeySource::Env("OPENAI_API_KEY".into())),
            Some(KeySource::Stored("OpenAI".into())),
        ] {
            c.key = k;
            assert_eq!(
                crate::onboarding::Fail::from_error(&explained(401, "{}", &c)),
                crate::onboarding::Fail::KeyRejected(401)
            );
        }
    }

    #[test]
    fn the_key_source_is_the_variable_that_holds_the_key_else_the_store() {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let saved: Vec<_> = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "CUSTOM_API_KEY"]
            .map(|n| (n, std::env::var_os(n)))
            .into();
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("CUSTOM_API_KEY");

        assert_eq!(
            key_source(&provider_for("https://api.openai.com/v1", None)),
            None
        );
        let openai = provider_for("https://api.openai.com/v1", Some("sk-stored"));
        assert_eq!(
            key_source(&openai),
            Some(KeySource::Stored("OpenAI".into()))
        );
        std::env::set_var("OPENAI_API_KEY", "sk-stored");
        assert_eq!(
            key_source(&openai),
            Some(KeySource::Env("OPENAI_API_KEY".into()))
        );
        std::env::set_var("OPENAI_API_KEY", "sk-other");
        assert_eq!(
            key_source(&openai),
            Some(KeySource::Stored("OpenAI".into()))
        );

        // A gateway: the key is CUSTOM_API_KEY's, or the one saved for it.
        let gw = provider_for("http://127.0.0.1:9000/v1", Some("sk-gw"));
        assert_eq!(
            key_source(&gw),
            Some(KeySource::Stored("http://127.0.0.1:9000".into()))
        );
        std::env::set_var("CUSTOM_API_KEY", "sk-gw");
        assert_eq!(
            key_source(&gw),
            Some(KeySource::Env("CUSTOM_API_KEY".into()))
        );

        for (n, v) in saved {
            match v {
                Some(v) => std::env::set_var(n, v),
                None => std::env::remove_var(n),
            }
        }
    }

    #[test]
    fn statuses_map_to_what_happened_and_what_to_do() {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("BWN_MAX_RETRIES");
        let hosted = ctx("gpt-4o", true, false);
        let m = explained(401, r#"{"error":{"message":"invalid api key"}}"#, &hosted);
        assert!(
            m.starts_with("the API key was rejected by the provider — /login to replace it"),
            "{m}"
        );
        assert!(m.ends_with("HTTP 401: invalid api key"), "{m}");
        assert!(explained(401, "{}", &ctx("m", false, false)).contains("wants an API key"));

        let ollama = ctx("tinycoder:7b", false, true);
        let m = explained(
            404,
            r#"{"error":"model \"tinycoder:7b\" not found, try pulling it first"}"#,
            &ollama,
        );
        assert!(
            m.contains("model tinycoder:7b is not installed on http://192.168.50.10:11434"),
            "{m}"
        );
        assert!(m.contains("ollama pull tinycoder:7b"), "{m}");
        let m = explained(
            404,
            r#"{"error":{"message":"The model `gpt-9` does not exist"}}"#,
            &hosted,
        );
        assert!(m.contains("does not know model gpt-4o"), "{m}");
        assert!(explained(404, "404 page not found", &hosted).contains("check base_url"));

        let m = explained(
            400,
            r#"{"error":{"code":400,"message":"the request exceeds the available context size, try increasing it","type":"exceed_context_size_error","n_prompt_tokens":2150,"n_ctx":2048}}"#,
            &ctx("tiny.gguf", false, false),
        );
        assert!(
            m.starts_with("the prompt needs 2,150 tokens and the server holds 2,048 — restart llama-server with a larger -c, or set context_tokens"),
            "{m}"
        );
        let m = explained(
            400,
            r#"{"error":"Trying to keep the first 56390 tokens when context the overflows. However, the model is loaded with context length of only 4096 tokens, which is not enough."}"#,
            &ctx("tiny", false, false),
        );
        assert!(
            m.starts_with("the prompt needs 56,390 tokens and the server holds 4,096"),
            "{m}"
        );
        // The server's own words stay, so the agent's overflow recovery
        // still recognises the failure.
        assert!(m.contains("context length"), "{m}");
        let m = explained(
            400,
            r#"{"error":{"message":"This model's maximum context length is 128000 tokens. However, your messages resulted in 130500 tokens."}}"#,
            &hosted,
        );
        assert!(
            m.starts_with("the prompt needs 130,500 tokens and the server holds 128,000"),
            "{m}"
        );

        let m = explained(
            500,
            r#"{"error":"model requires more system memory (5.6 GiB) than is available (3.1 GiB)"}"#,
            &ollama,
        );
        assert!(
            m.starts_with(
                "the model needs 5.6 GiB but 3.1 GiB is free — pick a smaller model with /model"
            ),
            "{m}"
        );
        let m = explained(500, r#"{"error":{"message":"upstream exploded"}}"#, &hosted);
        assert!(
            m.starts_with("the server failed after 3 retries — BWN_MAX_RETRIES"),
            "{m}"
        );
        assert!(m.ends_with("HTTP 500: upstream exploded"), "{m}");
        let m = explained(503, r#"{"error":{"message":"Loading model"}}"#, &ollama);
        assert!(
            m.starts_with("the server is still loading tinycoder:7b after 14 retries"),
            "{m}"
        );
        assert!(explained(429, "{}", &hosted)
            .starts_with("rate-limited by the provider after 3 retries"));
    }

    #[test]
    fn definitive_failures_are_not_retried() {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("BWN_MAX_RETRIES");
        for (code, body) in [
            (400, "{}"),
            (401, "{}"),
            (404, r#"{"error":"model not found"}"#),
            (
                500,
                r#"{"error":"model requires more system memory (5.6 GiB) than is available (3.1 GiB)"}"#,
            ),
            (500, r#"{"error":"CUDA error: out of memory"}"#),
        ] {
            assert_eq!(retries_for(code, body), 0, "{code} {body}");
        }
        assert_eq!(retries_for(500, r#"{"error":"boom"}"#), 3);
        assert_eq!(retries_for(429, "{}"), 3);
        assert_eq!(retries_for(503, r#"{"error":"Loading model"}"#), 14);
        std::env::set_var("BWN_MAX_RETRIES", "0");
        assert_eq!(retries_for(500, r#"{"error":"boom"}"#), 0);
        assert_eq!(retries_for(503, "Loading model"), 0);
        std::env::set_var("BWN_MAX_RETRIES", "999");
        assert_eq!(retries_for(500, "{}"), MAX_RETRIES_CAP);
        std::env::remove_var("BWN_MAX_RETRIES");
    }

    // A one-shot proxy that answers its first request with `reply`.
    fn proxy_answering(reply: &'static str) -> u16 {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let _ = s.write_all(reply.as_bytes());
            }
        });
        port
    }

    fn through(proxy: &str, url: &str) -> String {
        let client = crate::net::Client::with_lookup(
            |k: &str| (k == "HTTPS_PROXY").then(|| proxy.to_string()),
            |b| b,
        );
        let e = client.get(url).call().unwrap_err();
        let route = client.proxy_route(url).unwrap();
        proxy_failure(url, &route, &e, io_kind(&e))
    }

    #[test]
    fn proxy_failures_name_the_proxy_and_the_setting() {
        let url = "https://gw.example.test/v1/chat/completions";
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let m = through(&format!("http://127.0.0.1:{closed}"), url);
        assert!(
            m.starts_with(&format!(
                "the proxy in HTTPS_PROXY (http://127.0.0.1:{closed}) refused the connection"
            )),
            "{m}"
        );
        assert!(m.contains("add gw.example.test to NO_PROXY"), "{m}");

        let port = proxy_answering(
            "HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n",
        );
        let m = through(&format!("http://127.0.0.1:{port}"), url);
        assert_eq!(
            m,
            format!("the proxy wants credentials — HTTPS_PROXY=http://user:pass@127.0.0.1:{port}")
        );

        let port = proxy_answering("HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n");
        let m = through(&format!("http://127.0.0.1:{port}"), url);
        assert!(
            m.contains("would not connect to gw.example.test:443"),
            "{m}"
        );
        assert!(m.contains("NO_PROXY"), "{m}");
    }

    #[test]
    fn a_bare_refusal_from_a_proxy_is_named_as_the_proxy() {
        let r = crate::net::ProxyRoute {
            var: "HTTP_PROXY",
            shown: "http://127.0.0.1:19109".into(),
            host_port: "127.0.0.1:19109".into(),
            has_credentials: false,
            unusable: None,
        };
        let m = proxy_status(403, "", &r, "http://vm:19100/v1").unwrap();
        assert!(
            m.starts_with("HTTP 403 from proxy http://127.0.0.1:19109 for vm:19100 — if vm is on your network, add vm to NO_PROXY"),
            "{m}"
        );
        assert!(proxy_status(403, r#"{"error":"no"}"#, &r, "http://vm:19100/v1").is_none());
        assert!(proxy_status(407, "", &r, "http://vm:19100/v1")
            .unwrap()
            .contains("HTTP_PROXY=http://user:pass@127.0.0.1:19109"));
    }

    #[test]
    fn a_stalled_response_names_the_read_timeout_setting() {
        let p = Provider {
            protocol: Protocol::OllamaNative,
            base_url: "http://localhost:11434".into(),
            api_key: None,
            model: "tinycoder:3b".into(),
            context_tokens: 8_192,
            temperature: None,
            max_tokens: None,
            effort: Effort::Off,
            ollama_ctx: std::sync::OnceLock::new(),
        };
        let m = explain_failure(
            &p,
            "stream read failed: Network Error: timed out reading response".into(),
        );
        assert!(m.contains("BWN_READ_TIMEOUT_SECS"), "{m}");
        assert!(m.contains("http://localhost:11434"), "{m}");
        // Anything that is not a status or a timeout passes through.
        assert_eq!(explain_failure(&p, "interrupted".into()), "interrupted");
    }

    type Captured = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    // One-shot HTTP server: answers `responses.len()` requests with the given
    // (status, body) pairs, then exits. Runs the real wire path end-to-end.
    fn mock_server(responses: Vec<(u16, &'static str)>) -> (String, std::thread::JoinHandle<()>) {
        let (base, handle, _) = mock_server_capture(responses);
        (base, handle)
    }

    // Same, also handing back each request body it received (in order) so a
    // test can assert on the wire shape the client actually sent.
    fn mock_server_capture(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, std::thread::JoinHandle<()>, Captured) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let captured: Captured = Default::default();
        let sink = captured.clone();
        let handle = std::thread::spawn(move || {
            for (code, body) in responses {
                let (mut sock, _) = listener.accept().unwrap();
                sock.set_read_timeout(Some(std::time::Duration::from_millis(300)))
                    .unwrap();
                // Drain the request without caring about its shape.
                let mut buf = [0u8; 16384];
                let mut seen = Vec::new();
                loop {
                    match sock.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            seen.extend_from_slice(&buf[..n]);
                            // Headers done and some body bytes read is enough.
                            if seen.windows(4).any(|w| w == b"\r\n\r\n") && seen.ends_with(b"}") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let body_start = seen
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map_or(seen.len(), |i| i + 4);
                sink.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&seen[body_start..]).into_owned());
                let resp = format!(
                    "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        (base, handle, captured)
    }

    #[test]
    fn validate_passes_on_ok_and_reports_auth_and_connect_failures() {
        let ok_body = r#"{"choices":[{"message":{"role":"assistant","content":"y"},"finish_reason":"stop"}]}"#;
        let auth_body = r#"{"error":{"message":"invalid api key"}}"#;
        let (base, handle) = mock_server(vec![(200, ok_body), (401, auth_body)]);
        let p = Provider {
            protocol: Protocol::OpenAi,
            base_url: base,
            api_key: Some("sk-test".into()),
            model: "test-model".into(),
            context_tokens: 8_192,
            temperature: None,
            max_tokens: None,
            effort: Effort::Off,
            ollama_ctx: std::sync::OnceLock::new(),
        };
        assert!(validate(&p).is_ok(), "200 must validate");
        let err = validate(&p).unwrap_err();
        assert!(
            err.contains("401"),
            "auth failure must surface the status: {err}"
        );
        handle.join().unwrap();
    }

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
            images: Vec::new(),
        }])];
        let b = anthropic_body("m", &msgs, &[], None);
        let block = &b["messages"][0]["content"][0];
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["tool_use_id"], "t1");
        assert_eq!(block["is_error"], false);
        // Text only: the content stays a plain string.
        assert_eq!(block["content"], "ok");
    }

    // A round where one tool returned an image and one did not, after the
    // assistant turn that made both calls.
    fn image_round() -> Vec<Msg> {
        vec![
            Msg::Assistant {
                text: String::new(),
                calls: vec![
                    tc("c1", "read_file", json!({"path": "a.png"})),
                    tc("c2", "read_file", json!({"path": "b.txt"})),
                ],
            },
            Msg::Tool(vec![
                ToolResult {
                    id: "c1".into(),
                    content: "image a.png".into(),
                    is_error: false,
                    images: vec![("image/png".into(), "QUJD".into())],
                },
                ToolResult {
                    id: "c2".into(),
                    content: "hello".into(),
                    is_error: false,
                    images: Vec::new(),
                },
            ]),
        ]
    }

    #[test]
    fn anthropic_body_puts_tool_images_inside_the_tool_result() {
        let b = anthropic_body("m", &image_round(), &[], None);
        let results = &b["messages"][1]["content"];
        assert_eq!(
            results[0]["content"],
            json!([
                {"type": "text", "text": "image a.png"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}}
            ])
        );
        assert_eq!(results[1]["content"], "hello");
        // No empty text block when the tool said nothing.
        let mut bare = image_round();
        if let Msg::Tool(rs) = &mut bare[1] {
            rs[0].content.clear();
        }
        let b = anthropic_body("m", &bare, &[], None);
        assert_eq!(
            b["messages"][1]["content"][0]["content"][0]["type"],
            "image"
        );
    }

    #[test]
    fn openai_body_follows_the_tool_messages_with_their_images() {
        let b = openai_body("m", &image_round(), &[], None, None);
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m.len(), 4, "{m:?}");
        assert_eq!(m[1]["role"], "tool");
        assert_eq!(m[1]["content"], "image a.png");
        assert_eq!(m[2]["role"], "tool");
        assert_eq!(m[3]["role"], "user");
        assert_eq!(
            m[3]["content"],
            json!([
                {"type": "text", "text": "[1 image returned by the read_file call c1]"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}}
            ])
        );
        // A round without images adds nothing.
        let b = openai_body("m", &image_round()[..1], &[], None, None);
        assert_eq!(b["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn ollama_body_follows_the_tool_messages_with_their_images() {
        let b = ollama_body("m", &image_round(), &[], None, None, 8_192);
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m.len(), 4, "{m:?}");
        assert_eq!(m[1]["role"], "tool");
        assert!(m[1].get("images").is_none());
        assert_eq!(m[3]["role"], "user");
        assert_eq!(
            m[3]["content"],
            "[1 image returned by the read_file call c1]"
        );
        assert_eq!(m[3]["images"], json!(["QUJD"]));
    }

    #[test]
    fn flattened_requests_keep_tool_images_on_the_user_turn() {
        let mut msgs = vec![Msg::User("look at a.png".into())];
        msgs.extend(image_round());
        let mut b = openai_body("m", &msgs, &[], None, None);
        flatten_openai_messages(&mut b, true);
        let last = b["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["role"], "user");
        let parts = last["content"].as_array().unwrap();
        assert_eq!(
            parts[0]["image_url"]["url"], "data:image/png;base64,QUJD",
            "{last}"
        );
        let mut b = ollama_body("m", &msgs, &[], None, None, 8_192);
        flatten_ollama_messages(&mut b);
        let last = b["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["images"], json!(["QUJD"]), "{last}");
    }

    #[test]
    fn tool_images_survive_a_session_round_trip_and_old_files_still_load() {
        let r: ToolResult =
            serde_json::from_str(r#"{"id":"a","content":"x","is_error":false}"#).unwrap();
        assert!(r.images.is_empty());
        let Msg::Tool(rs) = &image_round()[1] else {
            unreachable!()
        };
        let text = serde_json::to_string(&rs[..]).unwrap();
        assert_eq!(text.matches("images").count(), 1, "{text}");
        let back: Vec<ToolResult> = serde_json::from_str(&text).unwrap();
        assert_eq!(back[0].images, rs[0].images);
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
    fn template_role_error_detected() {
        assert!(is_template_role_error(
            "HTTP 400: Unable to generate parser for this template. Conversation roles must alternate user/assistant"
        ));
        assert!(!is_template_role_error("HTTP 500: internal error"));
        assert!(!is_template_role_error("HTTP 429: rate limited"));
    }

    #[test]
    fn flatten_messages_produces_alternating_user_assistant() {
        // system, user, assistant(+tool_call), tool-result → must collapse to
        // strictly alternating user/assistant with no system/tool roles.
        let mut body = json!({
            "model": "gemma",
            "tools": [{"type": "function"}],
            "messages": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "build it"},
                {"role": "assistant", "content": "", "tool_calls": [
                    {"id": "1", "type": "function", "function": {"name": "write_file", "arguments": "{\"path\":\"a\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "1", "content": "wrote a"}
            ]
        });
        flatten_openai_messages(&mut body, true);
        let m = body["messages"].as_array().unwrap();
        // The empty tool-call-only assistant turn is dropped (no call-shaped
        // text for a small model to imitate), so the remaining turns are all
        // user/assistant with none of the system/tool roles.
        let roles: Vec<&str> = m.iter().map(|x| x["role"].as_str().unwrap()).collect();
        assert!(roles.iter().all(|r| *r == "user" || *r == "assistant"));
        let joined: String = m
            .iter()
            .map(|x| x["content"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("You are helpful"));
        assert!(joined.contains("build it"));
        assert!(joined.contains("Tool result"));
        // A synthetic "[called …]" marker must never leak into the prompt.
        assert!(!joined.contains("[called"));
        // The native tools field is dropped for the restrictive template.
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn flatten_messages_prepends_user_when_starting_on_assistant() {
        let mut body = json!({
            "messages": [{"role": "assistant", "content": "hi"}]
        });
        flatten_openai_messages(&mut body, true);
        let roles: Vec<&str> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, vec!["user", "assistant"]);
    }

    #[test]
    fn interruptible_reader_passes_a_stream_through_unchanged() {
        let data: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        let mut r = InterruptibleReader::new(Box::new(Cursor::new(data.clone())));
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
        // A worker's result comes back through `interruptible` as-is.
        assert_eq!(interruptible(|| 7).unwrap(), 7);
    }

    #[test]
    fn flatten_openai_keeps_images_and_describes_tools_in_text() {
        let mut body = json!({
            "tools": [{"type": "function", "function": {
                "name": "write_file",
                "description": "Write a file. Overwrites it.",
                "parameters": {"type": "object", "required": ["path"],
                    "properties": {"path": {}, "content": {}}}
            }}],
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}},
                    {"type": "text", "text": "read this"}
                ]}
            ]
        });
        flatten_openai_messages(&mut body, true);
        let m = body["messages"].as_array().unwrap();
        assert_eq!(m.len(), 1, "system folds into the first user turn");
        let parts = m[0]["content"].as_array().unwrap();
        assert_eq!(parts[0]["image_url"]["url"], "data:image/png;base64,AAA");
        let text = parts.last().unwrap()["text"].as_str().unwrap();
        assert!(
            text.contains("- write_file(path, content?): Write a file"),
            "{text}"
        );
        assert!(text.contains(r#"{"name": "<tool>""#), "{text}");
        assert!(text.contains("Be brief.") && text.contains("read this"));
        assert!(body.get("tools").is_none());

        // A server that rejected the image gets the same turns without it.
        let mut again = json!({"messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}},
            {"type": "text", "text": "read this"}
        ]}]});
        flatten_openai_messages(&mut again, false);
        assert_eq!(again["messages"][0]["content"], "read this");
    }

    #[test]
    fn flatten_ollama_folds_tool_traffic_and_keeps_system_and_images() {
        let mut body = json!({
            "tools": [{"type": "function", "function": {
                "name": "read_file", "description": "Read a file.",
                "parameters": {"type": "object", "required": ["path"],
                    "properties": {"path": {}}}
            }}],
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "look", "images": ["QUJD"]},
                {"role": "assistant", "content": "", "tool_calls": [
                    {"function": {"name": "read_file", "arguments": {"path": "a"}}}
                ]},
                {"role": "tool", "content": "file body", "tool_name": "read_file"}
            ]
        });
        flatten_ollama_messages(&mut body);
        let m = body["messages"].as_array().unwrap();
        let roles: Vec<&str> = m.iter().map(|x| x["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "user"]);
        let sys = m[0]["content"].as_str().unwrap();
        assert!(sys.starts_with("sys") && sys.contains("- read_file(path): Read a file"));
        assert_eq!(m[1]["images"], json!(["QUJD"]));
        assert!(m[2]["content"]
            .as_str()
            .unwrap()
            .contains(r#"read_file({"path":"a"})"#));
        assert!(m[3]["content"]
            .as_str()
            .unwrap()
            .starts_with("Tool result:\nfile body"));
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn ollama_retries_without_tools_when_the_model_has_none_and_remembers() {
        let ok =
            r#"{"message":{"role":"assistant","content":"y"},"done":true,"done_reason":"stop"}"#;
        let reject = r#"{"error":"registry.ollama.ai/library/gemma3:4b does not support tools"}"#;
        let (base, handle, bodies) = mock_server_capture(vec![(400, reject), (200, ok), (200, ok)]);
        let p = Provider {
            protocol: Protocol::OllamaNative,
            base_url: base,
            api_key: None,
            model: "gemma3:4b".into(),
            context_tokens: 8_192,
            temperature: None,
            max_tokens: None,
            effort: Effort::Off,
            // Pre-seeded so no /api/show probe consumes a scripted reply.
            ollama_ctx: std::sync::OnceLock::from(Some(8_192)),
        };
        let tools = vec![ToolDef {
            name: "read_file",
            description: "Read a file.",
            schema: json!({"type": "object", "properties": {"path": {}}}),
        }];
        let msgs = [
            Msg::System("be brief".into()),
            Msg::UserImages {
                text: "what is this".into(),
                images: vec![("image/png".into(), "QUJD".into())],
            },
        ];
        complete(&p, &msgs, &tools).unwrap();
        complete(&p, &msgs, &tools).unwrap();
        handle.join().unwrap();
        let sent: Vec<Value> = bodies
            .lock()
            .unwrap()
            .iter()
            .map(|b| serde_json::from_str(b).unwrap())
            .collect();
        assert_eq!(sent.len(), 3, "400 → retry, then one remembered call");
        assert!(sent[0].get("tools").is_some());
        for retry in &sent[1..] {
            assert!(retry.get("tools").is_none());
            assert_eq!(retry["messages"][1]["images"], json!(["QUJD"]));
            assert!(retry["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("- read_file(path?)"));
        }
    }

    #[test]
    fn ollama_retries_without_thinking_when_the_model_has_none_and_remembers() {
        let ok =
            r#"{"message":{"role":"assistant","content":"y"},"done":true,"done_reason":"stop"}"#;
        let reject =
            r#"{"error":"registry.ollama.ai/library/llama3.2:latest does not support thinking"}"#;
        let (base, handle, bodies) = mock_server_capture(vec![(400, reject), (200, ok), (200, ok)]);
        let p = Provider {
            protocol: Protocol::OllamaNative,
            base_url: base,
            api_key: None,
            model: "llama3.2:latest".into(),
            context_tokens: 8_192,
            temperature: None,
            max_tokens: None,
            effort: Effort::High,
            ollama_ctx: std::sync::OnceLock::from(Some(8_192)),
        };
        let msgs = [Msg::User("hi".into())];
        assert_eq!(complete(&p, &msgs, &[]).unwrap().text, "y");
        assert_eq!(complete(&p, &msgs, &[]).unwrap().text, "y");
        handle.join().unwrap();
        let sent: Vec<Value> = bodies
            .lock()
            .unwrap()
            .iter()
            .map(|b| serde_json::from_str(b).unwrap())
            .collect();
        assert_eq!(sent.len(), 3, "400 → retry, then one remembered call");
        assert_eq!(sent[0]["think"], true);
        for retry in &sent[1..] {
            assert!(retry.get("think").is_none(), "{retry}");
        }
    }

    #[test]
    fn openai_body_tool_results_each_become_a_message() {
        let msgs = vec![Msg::Tool(vec![
            ToolResult {
                id: "a".into(),
                content: "1".into(),
                is_error: false,
                images: Vec::new(),
            },
            ToolResult {
                id: "b".into(),
                content: "2".into(),
                is_error: true,
                images: Vec::new(),
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
    fn split_trailing_partial_holds_tag_prefix() {
        assert_eq!(
            split_trailing_partial("hello <thi", "<think>"),
            ("hello ", "<thi")
        );
        assert_eq!(
            split_trailing_partial("reason</thi", "</think>"),
            ("reason", "</thi")
        );
        // No trailing partial → nothing held.
        assert_eq!(
            split_trailing_partial("plain text", "<think>"),
            ("plain text", "")
        );
        // A complete tag ends with `>`, not a tag prefix, so nothing is held —
        // the caller's find() handles complete tags first.
        assert_eq!(
            split_trailing_partial("a<think>", "<think>"),
            ("a<think>", "")
        );
    }

    #[test]
    fn openai_stream_handles_think_open_tag_split_across_chunks() {
        // "<think>" is split as "<thi" | "nk>".
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hello <thi\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"nk>reason</think>done\"}}]}\n\
                   data: [DONE]\n";
        let mut text_out = String::new();
        let mut think_out = String::new();
        openai_stream(
            Cursor::new(sse.as_bytes().to_vec()),
            &mut |t| text_out.push_str(t),
            &mut |t| think_out.push_str(t),
        )
        .unwrap();
        assert_eq!(text_out, "hello done");
        assert_eq!(think_out, "reason");
    }

    #[test]
    fn openai_stream_handles_think_close_tag_split_across_chunks() {
        // "</think>" is split as "</thi" | "nk>".
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"<think>reason</thi\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"nk>done\"}}]}\n\
                   data: [DONE]\n";
        let mut text_out = String::new();
        let mut think_out = String::new();
        openai_stream(
            Cursor::new(sse.as_bytes().to_vec()),
            &mut |t| text_out.push_str(t),
            &mut |t| think_out.push_str(t),
        )
        .unwrap();
        assert_eq!(text_out, "done");
        assert_eq!(think_out, "reason");
    }

    #[test]
    fn openai_stream_flushes_false_partial_at_stream_end() {
        // Content that merely ends with a tag-prefix and then stops must not be
        // swallowed — it wasn't a real tag.
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"answer<thi\"}}]}\n\
                   data: [DONE]\n";
        let r = drain_openai(sse);
        assert_eq!(r.text, "answer<thi");
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
    fn openai_stream_done_only_is_empty_reply() {
        let r = drain_openai("data: [DONE]\n");
        assert!(r.text.is_empty());
        assert!(r.calls.is_empty());
    }

    #[test]
    fn stream_eof_without_terminal_event_is_err() {
        let run_openai = |sse: &str| {
            openai_stream(
                Cursor::new(sse.as_bytes().to_vec()),
                &mut |_| {},
                &mut |_| {},
            )
        };
        assert_eq!(run_openai("").unwrap_err(), STREAM_TRUNCATED);
        let partial = "data: {\"choices\":[{\"delta\":{\"content\":\"Hal\"}}]}\n";
        assert_eq!(run_openai(partial).unwrap_err(), STREAM_TRUNCATED);
        // A finish_reason is as good as [DONE]: the content is all there.
        let finished =
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"finish_reason\":\"stop\"}]}\n";
        assert_eq!(run_openai(finished).unwrap().text, "Hi");

        let a = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hal\"}}\n";
        let r = anthropic_stream(Cursor::new(a.as_bytes().to_vec()), &mut |_| {}, &mut |_| {});
        assert_eq!(r.unwrap_err(), STREAM_TRUNCATED);

        let o = "{\"message\":{\"content\":\"Hal\"},\"done\":false}\n";
        let r = ollama_stream(Cursor::new(o.as_bytes().to_vec()), &mut |_| {}, &mut |_| {});
        assert_eq!(r.unwrap_err(), STREAM_TRUNCATED);
    }

    #[test]
    fn openai_error_chunk_and_error_body_surface_as_err() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n\
                   data: {\"error\":{\"message\":\"upstream overloaded, key sk-abcdefghijklmnop\"}}\n\
                   data: [DONE]\n";
        let err = openai_stream(
            Cursor::new(sse.as_bytes().to_vec()),
            &mut |_| {},
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.contains("upstream overloaded"), "got: {err}");
        assert!(
            !err.contains("sk-abcdefghijklmnop"),
            "must be redacted: {err}"
        );

        let err = openai_parse(json!({"error": {"message": "model not loaded"}})).unwrap_err();
        assert!(err.contains("model not loaded"), "got: {err}");
        let long = "x ".repeat(1000);
        let err = openai_parse(json!({ "error": long })).unwrap_err();
        assert!(err.len() < 500, "truncated: {}", err.len());
        // A null error field is not an error.
        assert!(openai_parse(json!({"error": null, "choices": []})).is_ok());
    }

    // ── transport: timeouts, retries, redirects ────────────────────────────
    #[test]
    fn shared_agent_has_no_overall_deadline_and_follows_no_redirects() {
        let dbg = format!("{:?}", agent().agent_for("https://api.example.com"));
        assert!(dbg.contains("timeout: None"), "{dbg}");
        assert!(dbg.contains("timeout_read: Some("), "{dbg}");
        assert!(dbg.contains("timeout_connect: Some(15s)"), "{dbg}");
        assert!(dbg.contains("redirects: 0"), "{dbg}");
    }

    // Accepts connections for `window`, reading each request but never
    // answering. Returns how many connections arrived.
    fn silent_server(window: std::time::Duration) -> (String, std::thread::JoinHandle<usize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let handle = std::thread::spawn(move || {
            let end = std::time::Instant::now() + window;
            let mut held = Vec::new();
            while std::time::Instant::now() < end {
                match listener.accept() {
                    Ok((sock, _)) => held.push(sock),
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
                }
            }
            held.len()
        });
        (base, handle)
    }

    #[test]
    fn read_timeout_after_post_is_not_retried() {
        let (base, handle) = silent_server(std::time::Duration::from_millis(2_000));
        let req = agent()
            .post(&format!("{base}/v1/chat/completions"))
            .timeout(Duration::from_millis(300));
        let err = send_raw(req, json!({"model": "m"})).unwrap_err();
        assert!(err.contains("BWN_READ_TIMEOUT_SECS"), "got: {err}");
        assert_eq!(handle.join().unwrap(), 1, "the POST must be sent once");
    }

    #[test]
    fn connect_failure_is_retried() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
        let e = agent().post(&url).send_json(json!({})).unwrap_err();
        assert!(transport_retryable(&e), "{e}");
        // The local-server hint is only given on the second refused attempt,
        // so seeing it proves send_raw retried.
        let err = send_raw(agent().post(&url), json!({})).unwrap_err();
        assert!(err.contains("nothing is answering"), "got: {err}");
    }

    #[test]
    fn redirect_is_refused_and_key_never_reaches_the_other_host() {
        use std::io::{Read, Write};
        let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let other_addr = other.local_addr().unwrap();
        let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", first.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = first.accept().unwrap();
            sock.set_read_timeout(Some(std::time::Duration::from_millis(300)))
                .unwrap();
            let mut buf = [0u8; 16384];
            let _ = sock.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nlocation: http://{other_addr}/v1/messages\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            let _ = sock.write_all(resp.as_bytes());
        });
        let p = Provider {
            protocol: Protocol::Anthropic,
            base_url: base,
            api_key: Some("sk-ant-test-key".into()),
            model: "claude-test".into(),
            context_tokens: 8_192,
            temperature: None,
            max_tokens: None,
            effort: Effort::Off,
            ollama_ctx: std::sync::OnceLock::new(),
        };
        let err = complete(&p, &[Msg::User("hi".into())], &[]).unwrap_err();
        handle.join().unwrap();
        assert!(err.contains("redirected"), "got: {err}");
        other.set_nonblocking(true).unwrap();
        assert!(
            other.accept().is_err(),
            "redirect target must not be contacted"
        );
    }

    #[test]
    fn jitter_stays_within_twenty_percent() {
        for _ in 0..50 {
            let j = jitter(1_000);
            assert!((800..=1_200).contains(&j), "{j}");
        }
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
                    images: Vec::new(),
                },
                ToolResult {
                    id: "unknown".into(),
                    content: "?".into(),
                    is_error: true,
                    images: Vec::new(),
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

    fn ollama_at(base: String, model: &str) -> Provider {
        Provider {
            protocol: Protocol::OllamaNative,
            base_url: base,
            api_key: None,
            model: model.into(),
            context_tokens: 8_192,
            temperature: None,
            max_tokens: None,
            effort: Effort::Off,
            ollama_ctx: std::sync::OnceLock::new(),
        }
    }

    #[test]
    fn served_windows_come_from_what_each_server_reports() {
        let served = |base: &str, model: &str| served_model(&format!("{base}/v1"), model);
        // llama.cpp: /props, per-slot n_ctx and its modalities.
        // total_slots: how many requests it answers at once.
        let props = r#"{"default_generation_settings":{"n_ctx":2048},"modalities":{"vision":false},"total_slots":2}"#;
        let (base, handle) = mock_server(vec![(200, props)]);
        assert_eq!(
            served(&base, "tiny.gguf"),
            Served {
                window: Some(2_048),
                vision: Some(false),
                slots: Some(2),
            }
        );
        handle.join().unwrap();
        // LM Studio: no /props; this model's loaded length and type.
        let lms = r#"{"data":[{"id":"other","loaded_context_length":32768},{"id":"tinycoder-7b-instruct","type":"vlm","state":"loaded","max_context_length":32768,"loaded_context_length":4096}]}"#;
        let (base, handle) = mock_server(vec![(404, "{}"), (200, lms)]);
        assert_eq!(
            served(&base, "tinycoder-7b-instruct"),
            Served {
                window: Some(4_096),
                vision: Some(true),
                slots: None,
            }
        );
        handle.join().unwrap();
        // vLLM: max_model_len on /v1/models.
        let vllm = r#"{"data":[{"id":"qwen","max_model_len":16384}]}"#;
        let (base, handle) = mock_server(vec![(404, "{}"), (404, "{}"), (200, vllm)]);
        assert_eq!(served(&base, "qwen").window, Some(16_384));
        handle.join().unwrap();
        // Nothing said: the caller keeps its guess.
        let (base, handle) = mock_server(vec![(404, "{}"), (404, "{}"), (200, r#"{"data":[]}"#)]);
        assert_eq!(served(&base, "qwen"), Served::default());
        handle.join().unwrap();
    }

    #[test]
    fn a_message_bigger_than_a_known_window_is_refused_before_sending() {
        let mut p = ollama_at("http://127.0.0.1:1".into(), "tiny");
        p.protocol = Protocol::OpenAi;
        p.context_tokens = 4_096;
        let cwd = std::env::current_dir().unwrap();
        let text = format!(
            "summarize it\n\n[attached files]\n[file: {}]\n{}",
            cwd.join("big.txt").display(),
            "x".repeat(36_000)
        );
        let msgs = [Msg::System("s".into()), Msg::User(text)];
        // A guessed window never refuses anything.
        assert!(oversized_message(&p, &msgs).is_none());
        remember_window(&p);
        let why = oversized_message(&p, &msgs).unwrap();
        assert_eq!(
            why,
            "big.txt is about 9.0k tokens and the server holds 4.1k — attach a range such as @big.txt:1-200"
        );
        // Without an attachment, the message itself is named.
        let msgs = [Msg::User("y".repeat(20_000))];
        assert!(oversized_message(&p, &msgs)
            .unwrap()
            .starts_with("this message is about 5.0k tokens and the server holds 4.1k"));
        // Small messages, and tool results, pass.
        assert!(oversized_message(&p, &[Msg::User("hi".into())]).is_none());
        assert!(oversized_message(&p, &[Msg::Tool(vec![])]).is_none());
        // complete() refuses it without a connection (port 1 would fail).
        let e = complete(&p, &[Msg::User("z".repeat(20_000))], &[]).unwrap_err();
        assert!(e.starts_with("this message is about"), "{e}");
    }

    #[test]
    fn a_failed_show_probe_is_tried_again_on_the_next_request() {
        let show = r#"{"model_info":{"llama.context_length":16384},"capabilities":["completion","tools"]}"#;
        let (base, handle) = mock_server(vec![(500, r#"{"error":"busy"}"#), (200, show)]);
        let p = ollama_at(base, "tinycoder:3b");
        // Not answered yet: the native path with the floor, nothing cached.
        assert_eq!(ollama_window(&p), Ok(Some(OLLAMA_CTX_FLOOR)));
        assert!(p.ollama_ctx.get().is_none(), "a failure is not remembered");
        // Answered now: the model's own window, remembered.
        assert_eq!(ollama_window(&p), Ok(Some(16_384)));
        assert_eq!(p.ollama_ctx.get(), Some(&Some(16_384)));
        assert_eq!(ollama_ctx(&p), Some(16_384));
        handle.join().unwrap();
    }

    #[test]
    fn a_missing_ollama_model_is_named_with_what_is_installed() {
        let missing = r#"{"error":"model 'tinycoder:7b' not found"}"#;
        let tags = r#"{"models":[{"name":"tinycoder:3b"},{"name":"plainchat:2b"}]}"#;
        let (base, handle) = mock_server(vec![(404, missing), (200, tags)]);
        let p = ollama_at(base.clone(), "tinycoder:7b");
        let e = ollama_window(&p).unwrap_err();
        handle.join().unwrap();
        assert!(
            e.starts_with(&format!("model tinycoder:7b is not installed on {base}")),
            "{e}"
        );
        assert!(e.contains("installed: tinycoder:3b, plainchat:2b"), "{e}");
        assert!(
            e.contains("ollama pull tinycoder:7b, or --model tinycoder:3b"),
            "{e}"
        );
        assert!(!e.contains("/api/show"), "{e}");
        assert!(p.ollama_ctx.get().is_none(), "asked again after a pull");

        // A server with no /api/show at all is an old Ollama: /v1, remembered.
        let (base, handle) = mock_server(vec![(404, "404 page not found")]);
        let p = ollama_at(base, "tinycoder:3b");
        assert_eq!(ollama_window(&p), Ok(None));
        assert_eq!(p.ollama_ctx.get(), Some(&None));
        handle.join().unwrap();
    }

    #[test]
    fn nobody_listening_is_said_once_without_falling_back() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let p = ollama_at(format!("http://127.0.0.1:{port}"), "tinycoder:3b");
        let e = ollama_window(&p).unwrap_err();
        assert!(
            e.starts_with("nothing is answering at http://127.0.0.1:"),
            "{e}"
        );
        assert!(p.ollama_ctx.get().is_none());
    }

    // ── usage parsing ────────────────────────────────────────────────────────
    #[test]
    fn anthropic_parse_reads_usage_and_keeps_thinking_blocks() {
        let r = anthropic_parse(json!({
            "content": [
                {"type": "thinking", "thinking": "hmm", "signature": "sig1"},
                {"type": "redacted_thinking", "data": "opaque"},
                {"type": "text", "text": "hi"},
                {"type": "tool_use", "id": "t1", "name": "read_file", "input": {}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 100, "output_tokens": 7,
                      "cache_creation_input_tokens": 20, "cache_read_input_tokens": 300}
        }))
        .unwrap();
        assert_eq!(
            r.usage,
            Usage {
                input: 100,
                output: 7,
                cache_read: 300,
                cache_write: 20
            }
        );
        assert_eq!(r.thinking_blocks.len(), 2);
        assert_eq!(r.thinking_blocks[0]["signature"], "sig1");
        assert_eq!(r.thinking_blocks[1]["type"], "redacted_thinking");
        assert_eq!(r.text, "hi");
        assert_eq!(r.calls.len(), 1);
    }

    #[test]
    fn anthropic_stream_reads_usage_from_message_start_and_delta() {
        let sse = "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":50,\"cache_read_input_tokens\":10,\"cache_creation_input_tokens\":5,\"output_tokens\":1}}}\n\
                   data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\
                   data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\
                   data: {\"type\":\"message_stop\"}\n";
        let r = drain_anthropic(sse);
        assert_eq!(
            r.usage,
            Usage {
                input: 50,
                output: 42,
                cache_read: 10,
                cache_write: 5
            }
        );
        assert_eq!(r.text, "ok");
    }

    #[test]
    fn anthropic_stream_assembles_thinking_blocks_with_signature() {
        let sse = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"let me \"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"see\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"SIG\"}}\n\
                   data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"XYZ\"}}\n\
                   data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"c1\",\"name\":\"read_file\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\
                   data: {\"type\":\"message_stop\"}\n";
        let mut thinking = String::new();
        let r = anthropic_stream(
            Cursor::new(sse.as_bytes().to_vec()),
            &mut |_| {},
            &mut |t| thinking.push_str(t),
        )
        .unwrap();
        assert_eq!(thinking, "let me see");
        assert_eq!(r.thinking_blocks.len(), 2);
        assert_eq!(r.thinking_blocks[0]["thinking"], "let me see");
        assert_eq!(r.thinking_blocks[0]["signature"], "SIG");
        assert_eq!(r.thinking_blocks[1]["data"], "XYZ");
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].id, "c1");
    }

    #[test]
    fn usage_missing_fields_are_zero_never_error() {
        assert!(anthropic_parse(json!({"content": []}))
            .unwrap()
            .usage
            .is_empty());
        assert!(openai_parse(json!({"choices": []}))
            .unwrap()
            .usage
            .is_empty());
        assert!(openai_parse(json!({"choices": [], "usage": null}))
            .unwrap()
            .usage
            .is_empty());
        assert!(ollama_parse(json!({"message": {}}))
            .unwrap()
            .usage
            .is_empty());
        assert!(drain_openai("data: [DONE]\n").usage.is_empty());
        assert!(drain_ollama("{\"done\":true}\n").usage.is_empty());
        // Wrong types are ignored, not fatal.
        let r = openai_parse(json!({"usage": {"prompt_tokens": "lots"}})).unwrap();
        assert!(r.usage.is_empty());
    }

    #[test]
    fn openai_parse_splits_cached_tokens_out_of_prompt_tokens() {
        let r = openai_parse(json!({
            "choices": [{"message": {"content": "x"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 120, "completion_tokens": 9,
                      "prompt_tokens_details": {"cached_tokens": 100}}
        }))
        .unwrap();
        assert_eq!(
            r.usage,
            Usage {
                input: 20,
                output: 9,
                cache_read: 100,
                cache_write: 0
            }
        );
        assert_eq!(r.usage.prompt_tokens(), 120);
    }

    #[test]
    fn openai_stream_reads_usage_from_final_chunk() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\
                   data: {\"choices\":[],\"usage\":{\"prompt_tokens\":30,\"completion_tokens\":2}}\n\
                   data: [DONE]\n";
        let r = drain_openai(sse);
        assert_eq!(r.text, "hi");
        assert_eq!(r.stop_reason.as_deref(), Some("stop"));
        assert_eq!(
            r.usage,
            Usage {
                input: 30,
                output: 2,
                cache_read: 0,
                cache_write: 0
            }
        );
    }

    #[test]
    fn mark_streaming_requests_usage_in_the_final_chunk() {
        let mut body = json!({"model": "m"});
        mark_streaming(&mut body);
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn ollama_parse_and_stream_read_eval_counts() {
        let r = ollama_parse(json!({
            "message": {"content": "x"}, "done": true, "done_reason": "stop",
            "prompt_eval_count": 77, "eval_count": 11
        }))
        .unwrap();
        assert_eq!(r.usage.input, 77);
        assert_eq!(r.usage.output, 11);
        let r = drain_ollama(
            "{\"message\":{\"content\":\"a\"},\"done\":false}\n\
             {\"message\":{\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":5,\"eval_count\":3}\n",
        );
        assert_eq!(r.usage.input, 5);
        assert_eq!(r.usage.output, 3);
        assert_eq!(r.usage.cache_read, 0);
    }

    #[test]
    fn request_records_every_completed_reply_into_the_ledger() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"y"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1234,"completion_tokens":56}}"#;
        let (base, handle) = mock_server(vec![(200, body)]);
        let p = Provider {
            protocol: Protocol::OpenAi,
            base_url: base,
            api_key: None,
            model: "ledger-test-model".into(),
            context_tokens: 8_192,
            temperature: None,
            max_tokens: None,
            effort: Effort::Off,
            ollama_ctx: std::sync::OnceLock::new(),
        };
        let before = crate::usage::snapshot();
        complete(&p, &[Msg::User("ping".into())], &[]).unwrap();
        handle.join().unwrap();
        let after = crate::usage::snapshot();
        // Other tests may record concurrently, so assert on lower bounds.
        assert!(after.requests > before.requests);
        assert!(after.totals.input >= before.totals.input + 1234);
        assert!(after.totals.output >= before.totals.output + 56);
        // A loopback base URL is a local provider: booked at $0, never unpriced.
        assert!(after.local_requests > before.local_requests);
    }

    // ── reasoning controls ───────────────────────────────────────────────────
    #[test]
    fn claude_version_parses_both_id_layouts() {
        assert_eq!(claude_version("claude-opus-4-6"), Some((4, 6)));
        assert_eq!(claude_version("claude-sonnet-4-6-20260101"), Some((4, 6)));
        assert_eq!(claude_version("claude-opus-5"), Some((5, 0)));
        assert_eq!(claude_version("claude-fable-5-1"), Some((5, 1)));
        assert_eq!(claude_version("claude-haiku-4-5"), Some((4, 5)));
        assert_eq!(claude_version("claude-sonnet-4-20250514"), Some((4, 0)));
        assert_eq!(claude_version("claude-3-7-sonnet-20250219"), Some((3, 7)));
        assert_eq!(claude_version("gpt-4o"), None);
        assert_eq!(claude_version("claude-proxy"), None);
    }

    #[test]
    fn anthropic_effort_off_leaves_body_untouched() {
        let mut body = anthropic_body("claude-opus-4-6", &[Msg::User("hi".into())], &[], None);
        let before = body.clone();
        apply_anthropic_effort(&mut body, "claude-opus-4-6", Effort::Off);
        assert_eq!(body, before);
    }

    #[test]
    fn anthropic_effort_uses_adaptive_thinking_on_claude_4_6_and_later() {
        for (model, wants_display) in [
            ("claude-sonnet-4-6", false),
            ("claude-opus-4-8", true),
            ("claude-opus-5", true),
        ] {
            let mut body = anthropic_body(model, &[Msg::User("hi".into())], &[], None);
            apply_anthropic_effort(&mut body, model, Effort::Medium);
            assert_eq!(body["thinking"]["type"], "adaptive", "{model}");
            assert!(body["thinking"].get("budget_tokens").is_none(), "{model}");
            assert_eq!(body["output_config"]["effort"], "medium", "{model}");
            assert_eq!(body["max_tokens"], 8192, "{model}: max_tokens untouched");
            assert_eq!(
                body["thinking"].get("display").is_some(),
                wants_display,
                "{model}"
            );
        }
    }

    #[test]
    fn anthropic_effort_uses_budget_tokens_before_4_6_and_raises_max_tokens() {
        let model = "claude-haiku-4-5";
        for (effort, budget) in [
            (Effort::Low, 2048),
            (Effort::Medium, 8192),
            (Effort::High, 16384),
        ] {
            let mut body = anthropic_body(model, &[Msg::User("hi".into())], &[], None);
            apply_anthropic_effort(&mut body, model, effort);
            assert_eq!(body["thinking"]["type"], "enabled");
            assert_eq!(body["thinking"]["budget_tokens"], budget);
            assert!(body.get("output_config").is_none());
            let max_tokens = body["max_tokens"].as_u64().unwrap();
            assert!(max_tokens > budget, "{effort}: {max_tokens} <= {budget}");
        }
        // An explicit max_tokens already above the budget is kept.
        let mut body = anthropic_body(model, &[Msg::User("hi".into())], &[], Some(30_000));
        apply_anthropic_effort(&mut body, model, Effort::High);
        assert_eq!(body["max_tokens"], 30_000);
        // Unversioned ids (a proxy alias) take the classic form too.
        let mut body = anthropic_body("my-alias", &[Msg::User("hi".into())], &[], None);
        apply_anthropic_effort(&mut body, "my-alias", Effort::Low);
        assert_eq!(body["thinking"]["budget_tokens"], 2048);
    }

    #[test]
    fn anthropic_effort_replays_recorded_thinking_before_tool_use() {
        // Memory is process-wide; a unique call id keeps this test isolated.
        let call_id = format!("replay-{}", std::process::id());
        remember_thinking(&Reply {
            calls: vec![tc(&call_id, "read_file", json!({}))],
            thinking_blocks: vec![json!({"type": "thinking", "thinking": "t", "signature": "s"})],
            ..Default::default()
        });
        let msgs = vec![
            Msg::User("go".into()),
            Msg::Assistant {
                text: "on it".into(),
                calls: vec![tc(&call_id, "read_file", json!({}))],
            },
            Msg::Tool(vec![ToolResult {
                id: call_id.clone(),
                content: "ok".into(),
                is_error: false,
                images: Vec::new(),
            }]),
        ];
        let mut body = anthropic_body("claude-opus-4-6", &msgs, &[], None);
        apply_anthropic_effort(&mut body, "claude-opus-4-6", Effort::Low);
        let content = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["signature"], "s");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[2]["type"], "tool_use");
        // Idempotent: a second pass doesn't double the block.
        replay_thinking_blocks(&mut body);
        assert_eq!(body["messages"][1]["content"].as_array().unwrap().len(), 3);
        // Off never touches the transcript, even with blocks on record.
        let mut body = anthropic_body("claude-opus-4-6", &msgs, &[], None);
        apply_anthropic_effort(&mut body, "claude-opus-4-6", Effort::Off);
        assert_eq!(body["messages"][1]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn remember_thinking_ignores_replies_without_calls_or_blocks() {
        let id = format!("noreplay-{}", std::process::id());
        remember_thinking(&Reply {
            calls: vec![tc(&id, "x", json!({}))],
            ..Default::default()
        });
        assert!(thinking_for(&id).is_none());
        remember_thinking(&Reply {
            thinking_blocks: vec![json!({"type": "thinking"})],
            ..Default::default()
        });
        assert!(thinking_for(&id).is_none());
    }

    #[test]
    fn openai_effort_only_for_reasoning_models() {
        assert!(is_openai_reasoning_model("o3"));
        assert!(is_openai_reasoning_model("o4-mini"));
        assert!(is_openai_reasoning_model("O1-preview"));
        assert!(is_openai_reasoning_model("gpt-5-mini"));
        assert!(is_openai_reasoning_model("gpt-5.1"));
        assert!(!is_openai_reasoning_model("gpt-4o"));
        assert!(!is_openai_reasoning_model("gpt-4.1"));
        assert!(!is_openai_reasoning_model("gpt-oss-20b"));
        assert!(!is_openai_reasoning_model("qwen3:8b"));
        assert!(!is_openai_reasoning_model("local-model"));
        assert!(!is_openai_reasoning_model("o10-something"));

        let mut body = json!({"model": "o3"});
        apply_openai_effort(&mut body, "o3", Effort::High);
        assert_eq!(body["reasoning_effort"], "high");
        let mut body = json!({"model": "o3"});
        apply_openai_effort(&mut body, "o3", Effort::Off);
        assert!(body.get("reasoning_effort").is_none());
        // Local OpenAI-compatible servers never see it unless the model name matches.
        let mut body = json!({"model": "qwen3:8b"});
        apply_openai_effort(&mut body, "qwen3:8b", Effort::High);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn ollama_effort_sets_think_for_any_level_but_off() {
        let mut body = json!({"model": "m"});
        apply_ollama_effort(&mut body, Effort::Off);
        assert!(body.get("think").is_none());
        for e in [Effort::Low, Effort::Medium, Effort::High] {
            let mut body = json!({"model": "m"});
            apply_ollama_effort(&mut body, e);
            assert_eq!(body["think"], true);
        }
    }

    // ── native tools on local OpenAI-compatible servers ──────────────────────
    #[test]
    fn needs_flatten_only_for_400s_naming_tools_roles_or_template() {
        assert!(needs_flatten("HTTP 400: tools are not supported"));
        assert!(needs_flatten("HTTP 400: unknown field tool_choice"));
        assert!(needs_flatten(
            "HTTP 400: Unable to generate parser for this template. Conversation roles must alternate user/assistant"
        ));
        assert!(!needs_flatten("HTTP 404: model not found"));
        assert!(!needs_flatten("HTTP 500: tools exploded"));
        assert!(!needs_flatten("HTTP 400: max_tokens must be positive"));
        assert!(!needs_flatten("connection failed: refused"));
        // Sticky only when the 400 names tools/template/roles; a capability
        // rejection retries once without poisoning the endpoint.
        assert!(flatten_is_sticky("HTTP 400: tools are not supported"));
        assert!(flatten_is_sticky(
            "HTTP 400: conversation roles must alternate"
        ));
        assert!(needs_flatten(
            "HTTP 400: this model does not support images"
        ));
        assert!(!flatten_is_sticky(
            "HTTP 400: this model does not support images"
        ));
        assert!(!flatten_is_sticky("HTTP 500: tools exploded"));
    }

    #[test]
    fn loopback_sends_native_tools_first_then_flattens_after_400_and_remembers() {
        let ok = r#"{"choices":[{"message":{"role":"assistant","content":"y"},"finish_reason":"stop"}]}"#;
        let reject = r#"{"error":{"message":"this template does not support tools"}}"#;
        let (base, handle, bodies) = mock_server_capture(vec![(400, reject), (200, ok), (200, ok)]);
        let p = Provider {
            protocol: Protocol::OpenAi,
            base_url: base,
            api_key: None,
            model: "local-model".into(),
            context_tokens: 8_192,
            temperature: None,
            max_tokens: None,
            effort: Effort::Off,
            ollama_ctx: std::sync::OnceLock::new(),
        };
        let tools = vec![ToolDef {
            name: "read_file",
            description: "read",
            schema: json!({"type": "object"}),
        }];
        let msgs = [Msg::System("be brief".into()), Msg::User("hi".into())];
        complete(&p, &msgs, &tools).unwrap();
        complete(&p, &msgs, &tools).unwrap();
        handle.join().unwrap();
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 3, "400 → retry, then one remembered call");
        let sent: Vec<Value> = bodies
            .iter()
            .map(|b| serde_json::from_str(b).unwrap())
            .collect();
        // First attempt: native tools and the real system role, even on loopback.
        assert!(sent[0].get("tools").is_some());
        assert_eq!(sent[0]["messages"][0]["role"], "system");
        // Retry after the 400: flattened, tool-less.
        assert!(sent[1].get("tools").is_none());
        assert!(sent[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["role"] != "system"));
        // Second exchange goes straight to the flattened shape — no failed
        // native attempt first.
        assert!(sent[2].get("tools").is_none());
        assert!(flatten_remembered(&format!(
            "{}/chat/completions",
            p.base_url
        )));
    }

    #[test]
    fn loopback_keeps_native_tools_when_the_server_accepts_them() {
        let ok = r#"{"choices":[{"message":{"role":"assistant","content":"y"},"finish_reason":"stop"}]}"#;
        let (base, handle, bodies) = mock_server_capture(vec![(200, ok), (200, ok)]);
        let p = Provider {
            protocol: Protocol::OpenAi,
            base_url: base,
            api_key: None,
            model: "local-model".into(),
            context_tokens: 8_192,
            temperature: None,
            max_tokens: None,
            effort: Effort::Off,
            ollama_ctx: std::sync::OnceLock::new(),
        };
        let tools = vec![ToolDef {
            name: "read_file",
            description: "read",
            schema: json!({"type": "object"}),
        }];
        complete(&p, &[Msg::User("hi".into())], &tools).unwrap();
        complete(&p, &[Msg::User("hi".into())], &tools).unwrap();
        handle.join().unwrap();
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        for b in bodies.iter() {
            let v: Value = serde_json::from_str(b).unwrap();
            assert!(v.get("tools").is_some(), "native tools stay on: {b}");
        }
        // A 404 (or any non-400) must not poison the endpoint either.
        assert!(!flatten_remembered(&format!(
            "{}/chat/completions",
            p.base_url
        )));
    }
}
