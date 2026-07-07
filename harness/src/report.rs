// Output sink for the agent loop. Human mode renders to the TUI; JSON mode emits
// one structured event per line so the harness can be driven by an orchestrator.
// Process-global mode set once at startup — the call sites just say what happened.

use std::sync::{Mutex, OnceLock};

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

// Live token stream (human mode only). Chunks are line-buffered through the
// TUI markdown stream renderer, so committed transcript lines arrive rendered
// (headings, inline styles, bordered code blocks) instead of as raw markdown.
// assistant_end() flushes any partial last line and closes the stream.
fn stream_renderer() -> &'static Mutex<tui::StreamRenderer> {
    static RENDERER: OnceLock<Mutex<tui::StreamRenderer>> = OnceLock::new();
    RENDERER.get_or_init(|| Mutex::new(tui::StreamRenderer::new()))
}

pub fn assistant_delta(chunk: &str) {
    if mode() == Mode::Human && !chunk.is_empty() {
        if let Ok(mut r) = stream_renderer().lock() {
            r.push(chunk);
        }
    }
}
pub fn assistant_end() {
    if mode() == Mode::Human {
        if let Ok(mut r) = stream_renderer().lock() {
            r.flush();
        }
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

// ── live diff rendering ──────────────────────────────────────────────────────

const DIFF_CTX: usize = 2; // dim context lines kept around each hunk
const DIFF_MAX_LINES: usize = 40; // cap on rendered diff body lines
const NEW_FILE_MAX_LINES: usize = 20; // preview cap for brand-new files
                                      // O(n*m) LCS guard: above this many DP cells (after trimming the common
                                      // prefix/suffix) fall back to whole-file remove/add display.
const LCS_MAX_CELLS: usize = 250_000;

/// Renders a compact colored unified diff of a file mutation into the
/// transcript: a `⏺ edit path (+A -R)` header, removed lines in red / added
/// lines in green, up to two dim context lines around each hunk, capped at 40
/// body lines with a `… (+N more lines)` trailer. Brand-new files (empty
/// `old`) render as a `write` header plus the first 20 lines as additions.
/// In JSON mode emits `{"type":"diff","path":…,"added":N,"removed":M}`
/// instead. Call from the edit/write tool paths right after the change lands.
pub fn diff(path: &str, old: &str, new: &str) {
    let (body, added, removed) = diff_body(old, new);
    if mode() == Mode::Json {
        emit(json!({"type": "diff", "path": path, "added": added, "removed": removed}));
        return;
    }
    let (verb, stat) = if old.is_empty() {
        ("write", format!("(+{added})"))
    } else {
        ("edit", format!("(+{added} -{removed})"))
    };
    tui::line(&format!(
        "  {} {} {}",
        tui::accent("⏺"),
        tui::bold(&format!("{verb} {path}")),
        tui::dim(&stat)
    ));
    for l in &body {
        let painted = if l.starts_with('+') {
            tui::green(l)
        } else if l.starts_with('-') {
            tui::red(l)
        } else {
            tui::dim(l)
        };
        tui::line(&format!("    {painted}"));
    }
}

// Plain (uncolored) diff body lines plus total added/removed counts. Kept
// pure so the hunk grouping and caps are unit-testable.
fn diff_body(old: &str, new: &str) -> (Vec<String>, usize, usize) {
    if old.is_empty() {
        // Brand-new file: show the head as additions, no LCS needed.
        let lines: Vec<&str> = new.lines().collect();
        let mut body: Vec<String> = lines
            .iter()
            .take(NEW_FILE_MAX_LINES)
            .map(|l| format!("+ {l}"))
            .collect();
        if lines.len() > NEW_FILE_MAX_LINES {
            body.push(format!(
                "… (+{} more lines)",
                lines.len() - NEW_FILE_MAX_LINES
            ));
        }
        return (body, lines.len(), 0);
    }
    let o: Vec<&str> = old.lines().collect();
    let n: Vec<&str> = new.lines().collect();
    let ops = diff_ops(&o, &n);
    let added = ops.iter().filter(|(k, _)| *k == '+').count();
    let removed = ops.iter().filter(|(k, _)| *k == '-').count();
    (hunk_lines(&ops), added, removed)
}

// Line-level diff ops: ('=', l) kept, ('-', l) removed, ('+', l) added. The
// common prefix/suffix are trimmed first so the O(n*m) LCS table only covers
// the changed middle; oversized middles fall back to remove-all/add-all.
fn diff_ops<'a>(o: &[&'a str], n: &[&'a str]) -> Vec<(char, &'a str)> {
    let mut head = 0;
    while head < o.len() && head < n.len() && o[head] == n[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < o.len() - head
        && tail < n.len() - head
        && o[o.len() - 1 - tail] == n[n.len() - 1 - tail]
    {
        tail += 1;
    }
    let (om, nm) = (&o[head..o.len() - tail], &n[head..n.len() - tail]);
    let mut ops: Vec<(char, &str)> = o[..head].iter().map(|l| ('=', *l)).collect();
    if om.len().saturating_mul(nm.len()) > LCS_MAX_CELLS {
        ops.extend(om.iter().map(|l| ('-', *l)));
        ops.extend(nm.iter().map(|l| ('+', *l)));
    } else {
        ops.extend(lcs_ops(om, nm));
    }
    ops.extend(o[o.len() - tail..].iter().map(|l| ('=', *l)));
    ops
}

// Classic LCS length table + backtrack over the (already trimmed) middle.
// Ties prefer deletion, so a replacement renders as "- old" then "+ new".
fn lcs_ops<'a>(o: &[&'a str], n: &[&'a str]) -> Vec<(char, &'a str)> {
    let cols = n.len() + 1;
    let mut table = vec![0u32; (o.len() + 1) * cols];
    for i in (0..o.len()).rev() {
        for j in (0..n.len()).rev() {
            table[i * cols + j] = if o[i] == n[j] {
                table[(i + 1) * cols + j + 1] + 1
            } else {
                table[(i + 1) * cols + j].max(table[i * cols + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut ops = Vec::new();
    while i < o.len() && j < n.len() {
        if o[i] == n[j] {
            ops.push(('=', o[i]));
            i += 1;
            j += 1;
        } else if table[(i + 1) * cols + j] >= table[i * cols + j + 1] {
            ops.push(('-', o[i]));
            i += 1;
        } else {
            ops.push(('+', n[j]));
            j += 1;
        }
    }
    ops.extend(o[i..].iter().map(|l| ('-', *l)));
    ops.extend(n[j..].iter().map(|l| ('+', *l)));
    ops
}

// Group ops into hunks: changed lines plus up to DIFF_CTX kept lines of
// context on each side, elided stretches marked with a lone "…", the whole
// body capped at DIFF_MAX_LINES with a "+N more" trailer.
fn hunk_lines(ops: &[(char, &str)]) -> Vec<String> {
    let mut show = vec![false; ops.len()];
    for (i, (kind, _)) in ops.iter().enumerate() {
        if *kind != '=' {
            let from = i.saturating_sub(DIFF_CTX);
            let to = (i + DIFF_CTX + 1).min(ops.len());
            show[from..to].fill(true);
        }
    }
    let mut out = Vec::new();
    let mut last_shown: Option<usize> = None;
    for (i, (kind, text)) in ops.iter().enumerate() {
        if !show[i] {
            continue;
        }
        if last_shown.is_some_and(|prev| i > prev + 1) {
            out.push("…".to_string());
        }
        let prefix = match kind {
            '-' => "- ",
            '+' => "+ ",
            _ => "  ",
        };
        out.push(format!("{prefix}{text}"));
        last_shown = Some(i);
    }
    if out.len() > DIFF_MAX_LINES {
        let extra = out.len() - DIFF_MAX_LINES;
        out.truncate(DIFF_MAX_LINES);
        out.push(format!("… (+{extra} more lines)"));
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clip_head_short_and_exact() {
        assert_eq!(clip_head("", 5), Vec::<String>::new());
        assert_eq!(clip_head("one\ntwo", 5), vec!["one", "two"]);
        assert_eq!(clip_head("a\nb\nc", 3), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_clip_head_truncation() {
        let input = "1\n2\n3\n4\n5\n6\n7";
        let clipped = clip_head(input, 3);
        assert_eq!(clipped.len(), 4);
        assert_eq!(clipped[0], "1");
        assert_eq!(clipped[1], "2");
        assert_eq!(clipped[2], "3");
        assert_eq!(clipped[3], "…(+4 more lines)");
    }

    #[test]
    fn test_clip_tail_short_and_exact() {
        assert_eq!(clip_tail("", 5), Vec::<String>::new());
        assert_eq!(clip_tail("one\ntwo", 5), vec!["one", "two"]);
        assert_eq!(clip_tail("a\nb\nc", 3), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_clip_tail_truncation() {
        let input = "1\n2\n3\n4\n5\n6\n7";
        let clipped = clip_tail(input, 3);
        assert_eq!(clipped.len(), 4);
        assert_eq!(clipped[0], "…(+4 earlier lines)");
        assert_eq!(clipped[1], "5");
        assert_eq!(clipped[2], "6");
        assert_eq!(clipped[3], "7");
    }

    #[test]
    fn diff_body_small_edit_has_expected_lines_and_counts() {
        let old = "a\nb\nc\nd\ne\nf\ng";
        let new = "a\nb\nc\nX\ne\nf\ng";
        let (body, added, removed) = diff_body(old, new);
        assert_eq!((added, removed), (1, 1));
        assert!(body.contains(&"- d".to_string()), "{body:?}");
        assert!(body.contains(&"+ X".to_string()), "{body:?}");
        // Two context lines each side; untouched far lines are elided.
        assert!(body.contains(&"  b".to_string()), "{body:?}");
        assert!(body.contains(&"  e".to_string()), "{body:?}");
        assert!(!body.contains(&"  a".to_string()), "{body:?}");
        assert!(!body.contains(&"  g".to_string()), "{body:?}");
    }

    #[test]
    fn diff_body_separates_distant_hunks() {
        let old: String = (0..20).map(|i| format!("line{i}\n")).collect();
        let new = old
            .replace("line3\n", "changed3\n")
            .replace("line15\n", "changed15\n");
        let (body, added, removed) = diff_body(&old, &new);
        assert_eq!((added, removed), (2, 2));
        assert!(body.contains(&"…".to_string()), "{body:?}");
        assert!(body.contains(&"- line3".to_string()), "{body:?}");
        assert!(body.contains(&"+ changed15".to_string()), "{body:?}");
    }

    #[test]
    fn diff_body_new_file_previews_first_lines() {
        let content: String = (0..30).map(|i| format!("line{i}\n")).collect();
        let (body, added, removed) = diff_body("", &content);
        assert_eq!((added, removed), (30, 0));
        assert_eq!(body.len(), NEW_FILE_MAX_LINES + 1);
        assert_eq!(body[0], "+ line0");
        assert!(body.last().unwrap().contains("+10 more"), "{body:?}");
    }

    #[test]
    fn diff_body_caps_total_lines() {
        let old: String = (0..200).map(|i| format!("old{i}\n")).collect();
        let new: String = (0..200).map(|i| format!("new{i}\n")).collect();
        let (body, added, removed) = diff_body(&old, &new);
        assert_eq!((added, removed), (200, 200));
        assert_eq!(body.len(), DIFF_MAX_LINES + 1);
        assert!(body.last().unwrap().contains("more lines"), "{body:?}");
    }

    #[test]
    fn diff_body_identical_is_empty() {
        let (body, added, removed) = diff_body("a\nb", "a\nb");
        assert!(body.is_empty(), "{body:?}");
        assert_eq!((added, removed), (0, 0));
    }

    #[test]
    fn diff_ops_falls_back_for_huge_inputs() {
        // 600×600 disjoint lines exceed LCS_MAX_CELLS → remove-all/add-all.
        let o: Vec<String> = (0..600).map(|i| format!("old{i}")).collect();
        let n: Vec<String> = (0..600).map(|i| format!("new{i}")).collect();
        let or: Vec<&str> = o.iter().map(String::as_str).collect();
        let nr: Vec<&str> = n.iter().map(String::as_str).collect();
        let ops = diff_ops(&or, &nr);
        assert_eq!(ops.iter().filter(|(k, _)| *k == '-').count(), 600);
        assert_eq!(ops.iter().filter(|(k, _)| *k == '+').count(), 600);
    }

    #[test]
    fn lcs_ops_emits_delete_before_insert_on_replace() {
        let ops = lcs_ops(&["old"], &["new"]);
        assert_eq!(ops, vec![('-', "old"), ('+', "new")]);
    }
}
