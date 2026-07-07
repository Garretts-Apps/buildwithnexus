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
        Mode::Human => tui::line(&tui::render_md(text)),
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
    if mode() == Mode::Json {
        emit(json!({"type": "tool_call", "name": name, "input": input}));
        return;
    }
    // `finish` is rendered by finish() — don't double up with a header line.
    if name == "finish" {
        return;
    }
    // A role-colored header line (icon + what it's about to do).
    let (icon, head) = match name {
        "read" | "read_file" | "list" | "list_dir" | "glob" | "find_paths" | "find_files"
        | "grep" | "grep_files" | "webfetch" | "websearch" | "fetch_url" | "web_search"
        | "headless_browser" | "list_servers" | "read_server_log" | "wait_for_url"
        | "list_python_tools" => ("◈", tui::cyan(preview)),
        "write" | "write_file" | "edit" | "edit_file" | "patch" | "apply_patch" => {
            ("✦", tui::yellow(preview))
        }
        "bash" | "run_command" | "python_tool" | "start_server" | "stop_server" => {
            ("⚡", tui::blue(preview))
        }
        "open_browser" => ("↗", tui::accent(preview)),
        "task" | "spawn_subagent" => ("❖", tui::accent(preview)),
        "question" => ("?", tui::yellow(preview)),
        _ => ("•", tui::dim(preview)),
    };
    tui::line(&format!("  {} {}", tui::accent(icon), head));

    // Inline colored diff for edits — see the change before/at the moment it lands.
    let body = match name {
        "edit" | "edit_file" => Some(tui::diff(
            input["old"]
                .as_str()
                .or_else(|| input["oldString"].as_str())
                .unwrap_or(""),
            input["new"]
                .as_str()
                .or_else(|| input["newString"].as_str())
                .unwrap_or(""),
        )),
        "write" | "write_file" => Some(tui::added_preview(input["content"].as_str().unwrap_or(""))),
        _ => None,
    };
    if let Some(body) = body {
        for l in body.lines() {
            tui::line(&format!("    {l}"));
        }
    }
}

// First `max` lines (head); appends a "+N more" marker when clipped.
fn clip_head(s: &str, max: usize) -> Vec<String> {
    let lines: Vec<&str> = s.lines().collect();
    let mut out: Vec<String> = lines.iter().take(max).map(|l| l.to_string()).collect();
    if lines.len() > max {
        out.push(format!("…(+{} more lines)", lines.len() - max));
    }
    out
}

// Last `max` lines (tail) — for command output, where the exit line and the most
// recent output matter most.
fn clip_tail(s: &str, max: usize) -> Vec<String> {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= max {
        return lines.iter().map(|l| l.to_string()).collect();
    }
    let mut out = vec![format!("…(+{} earlier lines)", lines.len() - max)];
    out.extend(lines[lines.len() - max..].iter().map(|l| l.to_string()));
    out
}

pub fn tool_result(name: &str, content: &str, is_error: bool) {
    if mode() == Mode::Json {
        emit(
            json!({"type": "tool_result", "name": name, "content": content, "is_error": is_error}),
        );
        return;
    }
    // Human mode previously showed nothing here — the user couldn't see command
    // output or errors. Surface results compactly, indented under the call.
    if is_error {
        for l in clip_head(content, 12) {
            tui::line(&tui::red(&format!("    {l}")));
        }
        return;
    }
    match name {
        "bash" | "run_command" | "python_tool" => {
            for l in clip_tail(content, 12) {
                tui::line(&tui::dim(&format!("    {l}")));
            }
        }
        "read" | "read_file" | "list" | "list_dir" | "glob" | "find_paths" | "find_files"
        | "grep" | "grep_files" | "list_python_tools" => {
            let n = content.lines().count();
            tui::line(&tui::dim(&format!(
                "    ↳ {n} line{}",
                if n == 1 { "" } else { "s" }
            )));
        }
        // write_file / edit_file / finish: the call header (+ diff) already said it.
        _ => {}
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
