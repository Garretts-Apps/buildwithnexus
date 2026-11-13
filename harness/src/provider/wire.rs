//! Request bodies are written straight from the conversation into the one
//! buffer that goes on the wire: no `serde_json::Value` tree in between, and
//! a retry re-sends the same bytes. Everything that shapes a body (effort,
//! streaming, the cache breakpoint, the flattened fallback) is an input to
//! the writer, so nothing is patched after the fact.
//!
//! Object keys are written in alphabetical order, the order `Value` maps
//! serialise in, which keeps the bytes identical to the ones recorded in
//! `wire_golden.txt`.

use super::*;

pub(super) struct Json {
    out: Vec<u8>,
}

impl Json {
    pub(super) fn with_capacity(n: usize) -> Json {
        Json {
            out: Vec::with_capacity(n),
        }
    }

    /// JSON syntax and literal keys, written as they are.
    pub(super) fn raw(&mut self, s: &str) {
        self.out.extend_from_slice(s.as_bytes());
    }

    /// A string, escaped as it is written.
    pub(super) fn str(&mut self, s: &str) {
        self.val(s);
    }

    /// Any value serde can write. Strings, numbers, booleans and `Value`s
    /// have no failure path when the sink is a `Vec`.
    pub(super) fn val<T: Serialize + ?Sized>(&mut self, v: &T) {
        let written = serde_json::to_writer(&mut self.out, v);
        debug_assert!(written.is_ok());
    }

    /// A string made of several pieces, escaped as each piece is written, so
    /// a megabyte of base64 is never joined into a temporary first.
    pub(super) fn display(&mut self, d: &dyn std::fmt::Display) {
        use serde::Serializer;
        let written = serde_json::Serializer::new(&mut self.out).collect_str(d);
        debug_assert!(written.is_ok());
    }

    pub(super) fn finish(self) -> Vec<u8> {
        self.out
    }
}

/// Writes the comma between the items of an array or object.
#[derive(Default)]
pub(super) struct Sep(bool);

impl Sep {
    pub(super) fn next(&mut self, j: &mut Json) {
        if self.0 {
            j.raw(",");
        }
        self.0 = true;
    }
}

/// The system prompt: every `Msg::System` joined by a blank line.
pub(super) struct SystemText<'a>(pub(super) &'a [Msg]);

impl SystemText<'_> {
    pub(super) fn is_empty(&self) -> bool {
        self.0
            .iter()
            .all(|m| !matches!(m, Msg::System(s) if !s.is_empty()))
    }
}

impl std::fmt::Display for SystemText<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut started = false;
        for m in self.0 {
            if let Msg::System(s) = m {
                if started {
                    f.write_str("\n\n")?;
                }
                f.write_str(s)?;
                started |= !s.is_empty();
            }
        }
        Ok(())
    }
}

/// Roughly how many bytes a body for this conversation takes, so the buffer
/// is allocated once rather than grown (and copied) as it fills.
pub(super) fn size_hint(msgs: &[Msg], tools: &[ToolDef]) -> usize {
    let images = |imgs: &[(String, String)]| -> usize {
        imgs.iter().map(|(m, d)| m.len() + d.len() + 96).sum()
    };
    let text: usize = msgs
        .iter()
        .map(|m| {
            64 + match m {
                Msg::System(s) | Msg::User(s) => s.len(),
                Msg::UserImages { text, images: i } => text.len() + images(i),
                Msg::Assistant { text, calls } => {
                    text.len() + calls.iter().map(|c| c.id.len() + 256).sum::<usize>()
                }
                Msg::Tool(results) => results
                    .iter()
                    .map(|r| r.id.len() + r.content.len() + images(&r.images) + 96)
                    .sum(),
            }
        })
        .sum();
    let tools: usize = tools.iter().map(|t| t.description.len() + 1024).sum();
    // Escaping adds a byte per newline and quote.
    text + text / 8 + tools + 1024
}

/// `{"type":"function","function":{…}}` tool definitions, the schema
/// wrapper the OpenAI and Ollama shapes share.
pub(super) fn function_tools(j: &mut Json, tools: &[ToolDef]) {
    j.raw(",\"tools\":[");
    let mut sep = Sep::default();
    for t in tools {
        sep.next(j);
        j.raw("{\"function\":{\"description\":");
        j.str(t.description);
        j.raw(",\"name\":");
        j.str(t.name);
        j.raw(",\"parameters\":");
        j.val(&t.schema);
        j.raw("},\"type\":\"function\"}");
    }
    j.raw("]");
}

/// `{"content":…,"role":…}` with a plain string content.
pub(super) fn text_message(j: &mut Json, role: &str, content: &str) {
    j.raw("{\"content\":");
    j.str(content);
    j.raw(",\"role\":\"");
    j.raw(role);
    j.raw("\"}");
}

// ── the flattened fallback ─────────────────────────────────────────────────

/// Which protocol's flattened transcript to fold for.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Flat {
    /// `keep_images` is false when the server rejected them.
    OpenAi {
        keep_images: bool,
    },
    Ollama,
}

// One turn of a flattened transcript: text plus the images it carried.
pub(super) struct FlatTurn<'a> {
    pub(super) role: &'static str,
    pub(super) text: String,
    pub(super) images: Vec<&'a (String, String)>,
}

// Fold a transcript into strictly alternating user/assistant turns with no
// `system`/`tool` roles: tool-result content becomes user turns, an
// assistant turn's tool calls render as text, and consecutive same-role
// turns merge (keeping every image). The first turn is always `user`. On
// the OpenAI shape the system prompt becomes the first user turn; Ollama's
// stays a system message, which the caller writes.
pub(super) fn fold_turns(msgs: &[Msg], flat: Flat) -> Vec<FlatTurn<'_>> {
    let user = |text: String| FlatTurn {
        role: "user",
        text,
        images: Vec::new(),
    };
    let keep_images = flat != Flat::OpenAi { keep_images: false };
    let mut folded: Vec<FlatTurn> = Vec::new();
    let system = SystemText(msgs);
    if matches!(flat, Flat::OpenAi { .. }) && !system.is_empty() {
        folded.push(user(system.to_string()));
    }
    let mut call_names: HashMap<&str, &str> = HashMap::new();
    for m in msgs {
        match m {
            Msg::System(_) => {}
            Msg::User(t) => folded.push(user(t.clone())),
            Msg::UserImages { text, images } => folded.push(FlatTurn {
                role: "user",
                text: text.clone(),
                images: if keep_images {
                    images.iter().collect()
                } else {
                    Vec::new()
                },
            }),
            Msg::Assistant { text, calls } => {
                let mut content = text.clone();
                for c in calls {
                    call_names.insert(&c.id, &c.name);
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    // Ollama carries arguments as a JSON value, so a bare
                    // string there reads without its quotes.
                    match (&c.input, flat) {
                        (Value::String(a), Flat::Ollama) => {
                            content.push_str(&format!("`{}({a})`", c.name))
                        }
                        (input, _) => content.push_str(&format!("`{}({input})`", c.name)),
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
            Msg::Tool(results) => {
                for r in results.iter().filter(|r| !r.content.is_empty()) {
                    folded.push(user(format!("Tool result:\n{}", r.content)));
                }
                let with_images = || results.iter().filter(|r| !r.images.is_empty());
                if with_images().next().is_some() {
                    let labels: Vec<String> = with_images()
                        .map(|r| tool_images_label(r, &call_names))
                        .collect();
                    folded.push(FlatTurn {
                        role: "user",
                        text: labels.join(if flat == Flat::Ollama { "\n" } else { " " }),
                        images: if keep_images {
                            with_images().flat_map(|r| r.images.iter()).collect()
                        } else {
                            Vec::new()
                        },
                    });
                }
            }
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
        merged.insert(0, user(String::new()));
    }
    merged
}

// Instructions for calling tools in plain text, built from the tool
// definitions a flattened request does not send. Without it the model would
// not know which tools exist. The JSON shape is the one the agent's
// text-call parser reads; `None` when the request had no tools.
pub(super) fn text_tool_guide(tools: &[ToolDef]) -> Option<String> {
    if tools.is_empty() {
        return None;
    }
    let mut guide = String::from(
        "This server has no native tool calling. To use a tool, reply with only \
         a JSON object and nothing else:\n\
         {\"name\": \"<tool>\", \"arguments\": {\"<param>\": <value>}}\n\
         The result comes back in the next message. When the task is done, \
         answer in plain text.\n\nTools (a ? marks an optional parameter):\n",
    );
    for t in tools {
        let required: Vec<&str> = t.schema["required"]
            .as_array()
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        // Required parameters first, in schema order, then the optional ones.
        let mut params: Vec<String> = required.iter().map(|r| r.to_string()).collect();
        if let Some(props) = t.schema["properties"].as_object() {
            for k in props.keys() {
                if !required.contains(&k.as_str()) {
                    params.push(format!("{k}?"));
                }
            }
        }
        let desc = t.description;
        let first = desc
            .split(". ")
            .next()
            .unwrap_or(desc)
            .trim_end_matches('.');
        let first: String = first.chars().take(160).collect();
        guide.push_str(&format!("- {}({}): {first}\n", t.name, params.join(", ")));
    }
    Some(guide)
}
