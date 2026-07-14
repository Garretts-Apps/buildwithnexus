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

    // Inline diff for edits/writes — the user sees exactly what will change
    // before approving it, rendered by the same clean renderer as applied
    // diffs (gutter, tinted rows, word-level emphasis).
    let body = match name {
        "edit" | "edit_file" => Some(render_diff_block(
            input["old"]
                .as_str()
                .or_else(|| input["oldString"].as_str())
                .unwrap_or(""),
            input["new"]
                .as_str()
                .or_else(|| input["newString"].as_str())
                .unwrap_or(""),
        )),
        "write" | "write_file" => Some(render_diff_block(
            "",
            input["content"].as_str().unwrap_or(""),
        )),
        _ => None,
    };
    if let Some(body) = body {
        if !body.is_empty() {
            // One tui::line call for the whole body → one repaint.
            tui::line(&body);
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
        let rows: Vec<String> = clip_head(content, 12)
            .into_iter()
            .map(|l| tui::red(&format!("    {l}")))
            .collect();
        tui::line(&rows.join("\n"));
        return;
    }
    match name {
        "bash" | "run_command" | "python_tool" => {
            let rows: Vec<String> = clip_tail(content, 12)
                .into_iter()
                .map(|l| tui::dim(&format!("    {l}")))
                .collect();
            if !rows.is_empty() {
                tui::line(&rows.join("\n"));
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
    let (rows, added, removed) = diff_rows(old, new);
    if mode() == Mode::Json {
        emit(json!({"type": "diff", "path": path, "added": added, "removed": removed}));
        return;
    }
    let (verb, stat) = if old.is_empty() {
        ("write", format!("+{added}"))
    } else {
        ("edit", format!("+{added} -{removed}"))
    };
    // The path is an OSC 8 file:// link — click opens the file in the OS
    // default app on supporting terminals.
    tui::line(&format!(
        "  {} {}  {}",
        tui::accent("⏺"),
        tui::file_link(path, &tui::bold(&format!("{verb} {path}"))),
        tui::dim(&stat)
    ));
    let body = paint_diff_rows(&rows);
    if !body.is_empty() {
        // Single batched line() call → one repaint for the whole diff body.
        tui::line(&body);
    }
}

/// Renders a standalone diff body (no header) for inline previews — the
/// edit/write tool announcements share the exact visual language of applied
/// diffs.
pub fn render_diff_block(old: &str, new: &str) -> String {
    let (rows, _, _) = diff_rows(old, new);
    paint_diff_rows(&rows)
}

// ── structured diff rows ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum DiffRowKind {
    Ctx,
    Del,
    Add,
    Gap, // elided stretch or "+N more" trailer
}

#[derive(Debug, Clone, PartialEq)]
struct DiffRow {
    kind: DiffRowKind,
    old_no: Option<usize>,
    new_no: Option<usize>,
    text: String,
    // Byte range of the changed span for word-level emphasis (Del/Add rows
    // that form a replacement pair).
    emph: Option<(usize, usize)>,
}

impl DiffRow {
    fn gap(text: String) -> Self {
        DiffRow {
            kind: DiffRowKind::Gap,
            old_no: None,
            new_no: None,
            text,
            emph: None,
        }
    }
}

// Structured diff rows plus total added/removed counts. Kept pure so hunk
// grouping, numbering, caps, and word-level emphasis are unit-testable.
fn diff_rows(old: &str, new: &str) -> (Vec<DiffRow>, usize, usize) {
    if old.is_empty() {
        // Brand-new file: the head as additions, no LCS needed.
        let lines: Vec<&str> = new.lines().collect();
        let mut rows: Vec<DiffRow> = lines
            .iter()
            .take(NEW_FILE_MAX_LINES)
            .enumerate()
            .map(|(i, l)| DiffRow {
                kind: DiffRowKind::Add,
                old_no: None,
                new_no: Some(i + 1),
                text: l.to_string(),
                emph: None,
            })
            .collect();
        if lines.len() > NEW_FILE_MAX_LINES {
            rows.push(DiffRow::gap(format!(
                "+{} more lines",
                lines.len() - NEW_FILE_MAX_LINES
            )));
        }
        return (rows, lines.len(), 0);
    }
    let o: Vec<&str> = old.lines().collect();
    let n: Vec<&str> = new.lines().collect();
    let ops = diff_ops(&o, &n);
    let added = ops.iter().filter(|(k, _)| *k == '+').count();
    let removed = ops.iter().filter(|(k, _)| *k == '-').count();
    (hunk_rows(&ops), added, removed)
}

// Group ops into hunks (changed lines + DIFF_CTX context each side, elided
// stretches as Gap rows, capped at DIFF_MAX_LINES), tracking line numbers on
// both sides and marking word-level emphasis on replacement pairs.
fn hunk_rows(ops: &[(char, &str)]) -> Vec<DiffRow> {
    let mut show = vec![false; ops.len()];
    for (i, (kind, _)) in ops.iter().enumerate() {
        if *kind != '=' {
            let from = i.saturating_sub(DIFF_CTX);
            let to = (i + DIFF_CTX + 1).min(ops.len());
            show[from..to].fill(true);
        }
    }
    let mut rows = Vec::new();
    let (mut old_no, mut new_no) = (0usize, 0usize);
    let mut last_shown: Option<usize> = None;
    for (i, (kind, text)) in ops.iter().enumerate() {
        // Numbers advance whether or not the row is shown.
        match kind {
            '-' => old_no += 1,
            '+' => new_no += 1,
            _ => {
                old_no += 1;
                new_no += 1;
            }
        }
        if !show[i] {
            continue;
        }
        if last_shown.is_some_and(|prev| i > prev + 1) {
            rows.push(DiffRow::gap("⋯".to_string()));
        }
        let (kind, o, n) = match kind {
            '-' => (DiffRowKind::Del, Some(old_no), None),
            '+' => (DiffRowKind::Add, None, Some(new_no)),
            _ => (DiffRowKind::Ctx, Some(old_no), Some(new_no)),
        };
        rows.push(DiffRow {
            kind,
            old_no: o,
            new_no: n,
            text: text.to_string(),
            emph: None,
        });
        last_shown = Some(i);
    }
    mark_replacement_emphasis(&mut rows);
    if rows.len() > DIFF_MAX_LINES {
        let extra = rows.len() - DIFF_MAX_LINES;
        rows.truncate(DIFF_MAX_LINES);
        rows.push(DiffRow::gap(format!("+{extra} more lines")));
    }
    rows
}

// Word-level emphasis: pair the i-th Del with the i-th Add of each adjacent
// Del-run/Add-run block (a replacement) and mark the changed byte span when
// the lines share enough prefix+suffix to make the highlight meaningful.
fn mark_replacement_emphasis(rows: &mut [DiffRow]) {
    let mut i = 0;
    while i < rows.len() {
        if rows[i].kind != DiffRowKind::Del {
            i += 1;
            continue;
        }
        let del_start = i;
        while i < rows.len() && rows[i].kind == DiffRowKind::Del {
            i += 1;
        }
        let add_start = i;
        while i < rows.len() && rows[i].kind == DiffRowKind::Add {
            i += 1;
        }
        let pairs = (add_start - del_start).min(i - add_start);
        for k in 0..pairs {
            let (d, a) = (del_start + k, add_start + k);
            if let Some((de, ae)) = char_emphasis(&rows[d].text, &rows[a].text) {
                rows[d].emph = Some(de);
                rows[a].emph = Some(ae);
            }
        }
    }
}

// Changed byte span between two similar lines: trim the common prefix and
// suffix (char-boundary safe) and return the differing middles. None when
// the lines share too little for a span highlight to help (< 30% common).
fn char_emphasis(old: &str, new: &str) -> Option<((usize, usize), (usize, usize))> {
    let prefix: usize = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    let suffix: usize = old[prefix..]
        .chars()
        .rev()
        .zip(new[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    let shorter = old.len().min(new.len());
    if shorter == 0 || (prefix + suffix) * 10 < shorter * 3 {
        return None;
    }
    let o_end = (old.len() - suffix).max(prefix);
    let n_end = (new.len() - suffix).max(prefix);
    Some(((prefix, o_end), (prefix, n_end)))
}

// ── painting ─────────────────────────────────────────────────────────────────

// Paint structured rows into the final block: dual line-number gutter,
// background-tinted add/del rows padded to the terminal edge, word-level
// emphasis spans, dim context, centered gap markers.
fn paint_diff_rows(rows: &[DiffRow]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let max_no = rows
        .iter()
        .flat_map(|r| [r.old_no.unwrap_or(0), r.new_no.unwrap_or(0)])
        .max()
        .unwrap_or(0);
    let w = max_no.max(1).to_string().len();
    let width = tui::term_width();
    // "  {old:>w} {new:>w} │ " before content.
    let gutter_cols = 2 + w + 1 + w + 3;
    let content_cols = width.saturating_sub(gutter_cols).max(8);

    let fmt_no = |n: Option<usize>| match n {
        Some(n) => format!("{n:>w$}"),
        None => " ".repeat(w),
    };
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        match r.kind {
            DiffRowKind::Gap => {
                out.push(tui::dim(&format!("  {:>pad$}", r.text, pad = w * 2 + 2)));
            }
            DiffRowKind::Ctx => {
                out.push(format!(
                    "  {} {} {}",
                    tui::dim(&fmt_no(r.old_no)),
                    tui::dim(&fmt_no(r.new_no)),
                    tui::dim(&format!("│   {}", r.text)),
                ));
            }
            DiffRowKind::Del | DiffRowKind::Add => {
                let is_del = r.kind == DiffRowKind::Del;
                let sign = if is_del { "- " } else { "+ " };
                let pad_len = content_cols
                    .saturating_sub(2 + tui::str_width(&r.text))
                    .min(width);
                let pad = " ".repeat(pad_len);
                let span: fn(&str) -> String = if is_del {
                    tui::diff_del_span
                } else {
                    tui::diff_add_span
                };
                let emph_span: fn(&str) -> String = if is_del {
                    tui::diff_del_emph_span
                } else {
                    tui::diff_add_emph_span
                };
                let body = match r.emph {
                    Some((s, e)) if s < e && e <= r.text.len() => format!(
                        "{}{}{}",
                        span(&format!("{sign}{}", &r.text[..s])),
                        emph_span(&r.text[s..e]),
                        span(&format!("{}{pad}", &r.text[e..]))
                    ),
                    _ => span(&format!("{sign}{}{pad}", r.text)),
                };
                out.push(format!(
                    "  {} {} {}{body}",
                    tui::dim(&fmt_no(r.old_no)),
                    tui::dim(&fmt_no(r.new_no)),
                    tui::dim("│ "),
                ));
            }
        }
    }
    out.join("\n")
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


pub fn tool_denied(reason: &str) {
    match mode() {
        Mode::Human => tui::line(&tui::red(&format!("  ✗ {reason}"))),
        Mode::Json => emit(json!({"type": "tool_denied", "reason": reason})),
    }
}

pub fn finish(summary: &str) {
    match mode() {
        Mode::Human => {
            tui::line("");
            tui::line(&tui::green(&format!("  ✓ {}", tui::bold("done"))));
            tui::line(&tui::render_md(summary));
        }
        Mode::Json => emit(json!({"type": "finish", "summary": summary})),
    }
}

pub fn error(msg: &str) {
    match mode() {
        Mode::Human => tui::line(&tui::red(&format!("  ✗ {msg}"))),
        Mode::Json => emit(json!({"type": "error", "message": msg})),
    }
}

// Warnings: things the user should notice (truncation, fallbacks, retries).
pub fn notice(msg: &str) {
    match mode() {
        Mode::Human => tui::line(&tui::yellow(msg)),
        Mode::Json => emit(json!({"type": "notice", "message": msg})),
    }
}

// Informational chrome (progress, sub-steps): dim, so it never competes with
// warnings for attention.
pub fn info(msg: &str) {
    match mode() {
        Mode::Human => tui::line(&tui::dim(msg)),
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

    fn texts(rows: &[DiffRow], kind: DiffRowKind) -> Vec<String> {
        rows.iter()
            .filter(|r| r.kind == kind)
            .map(|r| r.text.clone())
            .collect()
    }

    #[test]
    fn diff_rows_small_edit_numbers_and_counts() {
        let old = "a\nb\nc\nd\ne\nf\ng";
        let new = "a\nb\nc\nX\ne\nf\ng";
        let (rows, added, removed) = diff_rows(old, new);
        assert_eq!((added, removed), (1, 1));
        let del: Vec<&DiffRow> = rows.iter().filter(|r| r.kind == DiffRowKind::Del).collect();
        let add: Vec<&DiffRow> = rows.iter().filter(|r| r.kind == DiffRowKind::Add).collect();
        assert_eq!(del[0].text, "d");
        assert_eq!(del[0].old_no, Some(4));
        assert_eq!(del[0].new_no, None);
        assert_eq!(add[0].text, "X");
        assert_eq!(add[0].new_no, Some(4));
        // Two context lines each side; far lines are elided.
        let ctx = texts(&rows, DiffRowKind::Ctx);
        assert!(ctx.contains(&"b".to_string()) && ctx.contains(&"e".to_string()));
        assert!(!ctx.contains(&"a".to_string()) && !ctx.contains(&"g".to_string()));
        // Context rows carry both line numbers.
        let b = rows.iter().find(|r| r.text == "b").unwrap();
        assert_eq!((b.old_no, b.new_no), (Some(2), Some(2)));
    }

    #[test]
    fn diff_rows_separates_distant_hunks_with_gap() {
        let old: String = (0..20).map(|i| format!("line{i}\n")).collect();
        let new = old
            .replace("line3\n", "changed3\n")
            .replace("line15\n", "changed15\n");
        let (rows, added, removed) = diff_rows(&old, &new);
        assert_eq!((added, removed), (2, 2));
        assert!(rows.iter().any(|r| r.kind == DiffRowKind::Gap), "{rows:?}");
        assert!(texts(&rows, DiffRowKind::Del).contains(&"line3".to_string()));
        assert!(texts(&rows, DiffRowKind::Add).contains(&"changed15".to_string()));
    }

    #[test]
    fn diff_rows_new_file_previews_head_with_numbers() {
        let content: String = (0..30).map(|i| format!("line{i}\n")).collect();
        let (rows, added, removed) = diff_rows("", &content);
        assert_eq!((added, removed), (30, 0));
        assert_eq!(rows.len(), NEW_FILE_MAX_LINES + 1);
        assert_eq!(rows[0].text, "line0");
        assert_eq!(rows[0].new_no, Some(1));
        assert!(rows.last().unwrap().text.contains("more lines"));
    }

    #[test]
    fn diff_rows_caps_total_rows() {
        let old: String = (0..200).map(|i| format!("old{i}\n")).collect();
        let new: String = (0..200).map(|i| format!("new{i}\n")).collect();
        let (rows, added, removed) = diff_rows(&old, &new);
        assert_eq!((added, removed), (200, 200));
        assert_eq!(rows.len(), DIFF_MAX_LINES + 1);
        assert!(rows.last().unwrap().text.contains("more lines"));
    }

    #[test]
    fn diff_rows_identical_is_empty() {
        let (rows, added, removed) = diff_rows("a\nb", "a\nb");
        assert!(rows.is_empty(), "{rows:?}");
        assert_eq!((added, removed), (0, 0));
    }

    #[test]
    fn replacement_pairs_get_word_level_emphasis() {
        let (rows, _, _) = diff_rows("let x = 1;\n", "let x = 42;\n");
        let del = rows.iter().find(|r| r.kind == DiffRowKind::Del).unwrap();
        let add = rows.iter().find(|r| r.kind == DiffRowKind::Add).unwrap();
        // The changed span covers just the value, not the whole line.
        let (ds, de) = del.emph.expect("del emphasis");
        let (as_, ae) = add.emph.expect("add emphasis");
        assert_eq!(&del.text[ds..de], "1");
        assert_eq!(&add.text[as_..ae], "42");
        // Completely different lines get no span highlight.
        let (rows, _, _) = diff_rows("aaaa\n", "zzzzzz\n");
        assert!(rows
            .iter()
            .filter(|r| r.kind != DiffRowKind::Gap)
            .all(|r| r.emph.is_none()));
    }

    #[test]
    fn char_emphasis_is_char_boundary_safe() {
        // Multibyte chars at the edit boundary must not split UTF-8.
        let e = char_emphasis("héllo wörld", "héllo wörms");
        if let Some(((s, ee), _)) = e {
            assert!("héllo wörld".is_char_boundary(s));
            assert!("héllo wörld".is_char_boundary(ee));
        }
        assert!(char_emphasis("", "").is_none());
    }

    #[test]
    fn paint_diff_rows_shows_gutter_signs_and_padding() {
        let (rows, _, _) = diff_rows("a\nb\nc", "a\nX\nc");
        let painted = paint_diff_rows(&rows);
        let plain: String = {
            // cheap ANSI strip for assertions
            let mut out = String::new();
            let mut it = painted.chars().peekable();
            while let Some(c) = it.next() {
                if c == '\x1b' {
                    for d in it.by_ref() {
                        if d.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out
        };
        assert!(plain.contains("- b"), "{plain}");
        assert!(plain.contains("+ X"), "{plain}");
        assert!(plain.contains('│'), "{plain}");
        assert!(plain.contains('1') && plain.contains('3'), "{plain}");
        assert_eq!(paint_diff_rows(&[]), "");
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
