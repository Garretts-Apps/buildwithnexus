// Output sink for the agent loop. Human mode renders to the TUI; JSON mode emits
// one structured event per line so the harness can be driven by an orchestrator.
// Process-global mode set once at startup — the call sites just say what happened.

use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::tui;

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Human,
    Json,
}

static MODE: OnceLock<Mode> = OnceLock::new();

pub fn set(m: Mode) {
    let _ = MODE.set(m);
}
fn mode() -> Mode {
    *MODE.get().unwrap_or(&Mode::Human)
}
pub fn is_json() -> bool {
    mode() == Mode::Json
}

fn emit(v: Value) {
    println!("{v}");
}

pub fn assistant(text: &str) {
    if text.trim().is_empty() {
        return;
    }
    match mode() {
        Mode::Human => tui::line(text),
        Mode::Json => emit(json!({"type": "assistant", "text": text})),
    }
}

// Live token stream (human mode only). assistant_end() closes the line.
pub fn assistant_delta(chunk: &str) {
    if mode() == Mode::Human && !chunk.is_empty() {
        tui::write_stream(chunk);
    }
}
pub fn assistant_end() {
    if mode() == Mode::Human {
        tui::line("");
    }
}

pub fn tool_call(name: &str, preview: &str, input: &Value) {
    match mode() {
        Mode::Human => tui::line(&tui::dim(&format!("  • {preview}"))),
        Mode::Json => emit(json!({"type": "tool_call", "name": name, "input": input})),
    }
}

pub fn tool_result(name: &str, content: &str, is_error: bool) {
    if mode() == Mode::Json {
        emit(json!({"type": "tool_result", "name": name, "content": content, "is_error": is_error}));
    }
}

pub fn tool_denied(reason: &str) {
    match mode() {
        Mode::Human => tui::line(&tui::dim(&format!("  ✗ {reason}"))),
        Mode::Json => emit(json!({"type": "tool_denied", "reason": reason})),
    }
}

pub fn finish(summary: &str) {
    match mode() {
        Mode::Human => {
            tui::line("");
            tui::line(&tui::green(&format!("✨ {summary}")));
        }
        Mode::Json => emit(json!({"type": "finish", "summary": summary})),
    }
}

pub fn error(msg: &str) {
    match mode() {
        Mode::Human => tui::line(&tui::red(&format!("  {msg}"))),
        Mode::Json => emit(json!({"type": "error", "message": msg})),
    }
}

pub fn notice(msg: &str) {
    match mode() {
        Mode::Human => tui::line(&tui::yellow(msg)),
        Mode::Json => emit(json!({"type": "notice", "message": msg})),
    }
}
