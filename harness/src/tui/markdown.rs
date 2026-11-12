//! Streaming Markdown: the line renderer, code blocks and pipe tables.

use super::*;

// ── streaming markdown / code-block renderer ─────────────────────────────────
// Feeds line-by-line through assistant streaming output. The open line is
// buffered until its newline arrives, then rendered through the markdown
// pipeline before being committed to the transcript; in the alt-screen TUI the
// raw in-progress line is echoed live and swapped for the rendered form on
// commit. Triple-backtick fenced code blocks are rendered with a box border
// (fence markers never shown raw), and each block is automatically copied to
// the clipboard via OSC 52 (supported by iTerm2, kitty, Alacritty, WezTerm,
// macOS Terminal 2.12+, and most modern terminals).
//
// Usage: create one per assistant turn, call push() for each streamed chunk,
// call flush() after streaming ends.

pub(super) enum StreamState {
    Normal,
    InCode { lang: String, lines: Vec<String> },
    MaybeJson { lines: Vec<String> },
    // A pipe table being collected; rendered as one aligned block when the
    // first non-table line (or the end of the stream) arrives.
    InTable { rows: Vec<String> },
}

pub struct StreamRenderer {
    pub(super) pending: String,
    pub(super) state: StreamState,
    pub(super) w: usize, // terminal width cap for box drawing
    // Byte length of the `pending` prefix already echoed raw to the open
    // transcript line (alt-screen only; see echo_partial()).
    pub(super) shown: usize,
    // Committed lines land here instead of the live transcript under test,
    // so chunk-split behaviour is assertable without a terminal.
    #[cfg(test)]
    pub(super) sink: Vec<String>,
}

impl Default for StreamRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamRenderer {
    pub fn new() -> Self {
        let w = term_size().0 as usize;
        StreamRenderer {
            pending: String::new(),
            state: StreamState::Normal,
            w,
            shown: 0,
            #[cfg(test)]
            sink: Vec::new(),
        }
    }

    // Commit one already-rendered line to the transcript.
    pub(super) fn emit(&mut self, s: &str) {
        #[cfg(test)]
        self.sink.push(s.to_string());
        #[cfg(not(test))]
        line(s);
    }

    pub fn push(&mut self, chunk: &str) {
        self.pending.push_str(&sanitize_terminal(chunk));
        self.drain(false);
    }

    // Call after the stream ends to flush any partial last line.
    pub fn flush(&mut self) {
        self.drain(true);
    }

    pub(super) fn drain(&mut self, end: bool) {
        loop {
            match self.pending.find('\n') {
                Some(nl) => {
                    // Take the line out in place; the rest is not copied.
                    let mut line_text: String = self.pending.drain(..=nl).collect();
                    line_text.pop(); // the '\n'
                                     // Any echoed raw prefix belongs to this line; the next
                                     // line starts un-echoed.
                    let had_partial = std::mem::take(&mut self.shown) > 0;
                    self.process_line(&line_text, had_partial);
                }
                None if end && !self.pending.is_empty() => {
                    let last = std::mem::take(&mut self.pending);
                    let had_partial = std::mem::take(&mut self.shown) > 0;
                    self.process_line(&last, had_partial);
                    break;
                }
                None => break,
            }
        }
        if end {
            self.finish_state();
        } else {
            self.echo_partial();
        }
    }

    // Echo the not-yet-terminated tail of the current line raw so streaming
    // stays visibly live between newlines. Alt-screen only: the transcript
    // tracks the open line there (OPEN_STREAM_LINE), so the raw text can be
    // replaced by the rendered line once the newline arrives. Lines that may
    // still classify as fence openers or protocol JSON are held back — they
    // may never be shown at all.
    pub(super) fn echo_partial(&mut self) {
        if !ALT_SCREEN.load(Ordering::Relaxed)
            || capturing()
            || !matches!(self.state, StreamState::Normal)
        {
            return;
        }
        let head = self.pending.trim_start();
        if head.starts_with('`') || head.starts_with('{') || head.starts_with('[') {
            return;
        }
        if self.pending.len() > self.shown {
            write_stream(&self.pending[self.shown..]);
            self.shown = self.pending.len();
        }
    }

    pub(super) fn process_line(&mut self, text: &str, had_partial: bool) {
        // Pull out the current state so we can unconditionally assign self.state below.
        let state = std::mem::replace(&mut self.state, StreamState::Normal);
        match state {
            StreamState::Normal => {
                if let Some(rest) = text.strip_prefix("```") {
                    // Fence markers are chrome — pull back any echoed raw prefix.
                    if had_partial {
                        retract_stream_line();
                    }
                    // Buffer fenced code until the close. Local models sometimes
                    // emit tool-call JSON as a code block; rendering only after
                    // classification keeps protocol artifacts out of the transcript.
                    let lang = rest.trim().to_string();
                    self.state = StreamState::InCode {
                        lang,
                        lines: Vec::new(),
                    };
                } else if starts_like_top_level_json(text) {
                    if had_partial {
                        retract_stream_line();
                    }
                    let lines = vec![text.to_string()];
                    self.state = StreamState::MaybeJson { lines };
                    self.try_flush_maybe_json(false);
                } else if is_table_row(text) {
                    // The raw row was echoed live; it is replaced by the
                    // aligned table once the block is complete.
                    if had_partial {
                        retract_stream_line();
                    }
                    self.state = StreamState::InTable {
                        rows: vec![text.to_string()],
                    };
                } else {
                    // Regular text: preserve blank lines; render markdown
                    // formatting. If the raw partial was echoed live, swap it
                    // for the rendered form instead of appending a duplicate.
                    let rendered = render_md_line(text);
                    if had_partial {
                        commit_stream_line(&rendered);
                    } else {
                        self.emit(&rendered);
                    }
                    self.state = StreamState::Normal;
                }
            }
            StreamState::InCode { lang, mut lines } => {
                if text.trim_end_matches('\r') == "```" {
                    let code = lines.join("\n");
                    if !is_tool_call_json_block(&lang, &code) {
                        self.render_code_block(&lang, &lines);
                    }
                    self.state = StreamState::Normal;
                } else {
                    lines.push(text.to_string());
                    self.state = StreamState::InCode { lang, lines };
                }
            }
            StreamState::MaybeJson { mut lines } => {
                lines.push(text.to_string());
                self.state = StreamState::MaybeJson { lines };
                self.try_flush_maybe_json(false);
            }
            StreamState::InTable { mut rows } => {
                if is_table_row(text) {
                    rows.push(text.to_string());
                    self.state = StreamState::InTable { rows };
                } else {
                    // The line that ends the table was echoed raw as the
                    // open stream line; pull it back so the table lands
                    // first, then render that line normally.
                    if had_partial {
                        retract_stream_line();
                    }
                    self.emit(&render_table(&rows, self.w).join("\n"));
                    self.state = StreamState::Normal;
                    self.process_line(text, false);
                }
            }
        }
    }

    pub(super) fn finish_state(&mut self) {
        let state = std::mem::replace(&mut self.state, StreamState::Normal);
        match state {
            StreamState::Normal => {}
            StreamState::InCode { lang, lines } => {
                let code = lines.join("\n");
                if !is_tool_call_json_block(&lang, &code) {
                    self.render_code_block(&lang, &lines);
                }
            }
            StreamState::MaybeJson { lines } => {
                self.state = StreamState::MaybeJson { lines };
                self.try_flush_maybe_json(true);
            }
            StreamState::InTable { rows } => {
                self.emit(&render_table(&rows, self.w).join("\n"));
            }
        }
    }

    pub(super) fn try_flush_maybe_json(&mut self, force: bool) {
        let state = std::mem::replace(&mut self.state, StreamState::Normal);
        let StreamState::MaybeJson { lines } = state else {
            self.state = state;
            return;
        };
        let joined = lines.join("\n");
        match serde_json::from_str::<serde_json::Value>(joined.trim()) {
            Ok(value) => {
                if !json_value_looks_like_tool_call(&value) {
                    self.render_plain_lines(&lines);
                }
                self.state = StreamState::Normal;
            }
            Err(_) if force || maybe_json_buffer_is_too_large(&lines) => {
                self.render_plain_lines(&lines);
                self.state = StreamState::Normal;
            }
            Err(_) => {
                self.state = StreamState::MaybeJson { lines };
            }
        }
    }

    pub(super) fn render_plain_lines(&mut self, lines: &[String]) {
        for text in lines {
            let rendered = render_md_line(text);
            self.emit(&rendered);
        }
    }

    pub(super) fn render_code_block(&mut self, lang: &str, lines: &[String]) {
        // One emit for the whole block: line() splits on '\n', and a single
        // call means one repaint instead of one per code row.
        let mut rows = Vec::with_capacity(lines.len() + 2);
        rows.push(code_box_header(lang, self.w));
        let lit = crate::highlight::highlight_block(lang, lines);
        rows.extend(lit.iter().map(|text| code_box_line(text)));
        rows.push(code_box_footer(self.w));
        self.emit(&rows.join("\n"));
    }
}

// ── markdown pipe tables ─────────────────────────────────────────────────────
// `| a | b |` rows are collected and drawn as one aligned block: bold header,
// a `─┼─` rule where the `|---|` separator was, dim column bars, and cell
// alignment from `:--` / `:-:` / `--:`. Columns shrink (widest first, cut
// with `…`) rather than wrap when the table is wider than the terminal, so
// every row stays exactly one terminal row.

pub(super) fn is_table_row(l: &str) -> bool {
    let t = l.trim();
    t.len() > 1 && t.starts_with('|') && t[1..].contains('|')
}

pub(super) fn split_table_cells(l: &str) -> Vec<String> {
    let t = l.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = t.chars().peekable();
    let mut in_code = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cur.push('|');
                chars.next();
            }
            '`' => {
                in_code = !in_code;
                cur.push(c);
            }
            '|' if !in_code => cells.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Align {
    Left,
    Center,
    Right,
}

// `|---|:--:|--:|` → per-column alignment, or None when this isn't a separator.
pub(super) fn table_separator(cells: &[String]) -> Option<Vec<Align>> {
    if cells.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(cells.len());
    for c in cells {
        let c = c.trim();
        let body = c.trim_start_matches(':').trim_end_matches(':');
        if body.is_empty() || !body.chars().all(|ch| ch == '-') {
            return None;
        }
        out.push(match (c.starts_with(':'), c.ends_with(':')) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        });
    }
    Some(out)
}

pub(super) fn render_table(rows: &[String], w: usize) -> Vec<String> {
    let parsed: Vec<Vec<String>> = rows.iter().map(|r| split_table_cells(r)).collect();
    let mut aligns: Option<Vec<Align>> = None;
    let mut sep_at: Option<usize> = None;
    for (i, cells) in parsed.iter().enumerate() {
        if i <= 1 {
            if let Some(a) = table_separator(cells) {
                aligns = Some(a);
                sep_at = Some(i);
                break;
            }
        }
    }
    let ncols = parsed.iter().map(|c| c.len()).max().unwrap_or(0);
    if ncols == 0 {
        return rows.iter().map(|r| render_md_line(r)).collect();
    }
    // Format cells once; measure their visible width.
    let fmt: Vec<Vec<(String, usize)>> = parsed
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != sep_at)
        .map(|(_, cells)| {
            (0..ncols)
                .map(|j| {
                    let raw = cells.get(j).map(String::as_str).unwrap_or("");
                    let styled = format_inline_md(raw);
                    let width = str_width(&strip_ansi(&styled));
                    (styled, width)
                })
                .collect()
        })
        .collect();
    let mut widths: Vec<usize> = (0..ncols)
        .map(|j| fmt.iter().map(|r| r[j].1).max().unwrap_or(0).max(1))
        .collect();
    // Fit: indent 2 + "│ " … " │ " separators (3 per gap) must stay ≤ w.
    let chrome = 2 + 3 * ncols.saturating_sub(1);
    let avail = w.saturating_sub(chrome).max(ncols * 3);
    while widths.iter().sum::<usize>() > avail {
        let (idx, _) = widths
            .iter()
            .enumerate()
            .max_by_key(|(_, wd)| **wd)
            .unwrap_or((0, &0));
        if widths[idx] <= 3 {
            break;
        }
        widths[idx] -= 1;
    }
    let has_header = sep_at.is_some();
    let aligns = aligns.unwrap_or_else(|| vec![Align::Left; ncols]);
    let bar = dim("│");
    let mut out = Vec::with_capacity(fmt.len() + 1);
    for (ri, row) in fmt.iter().enumerate() {
        let mut line = String::from("  ");
        for (j, (styled, width)) in row.iter().enumerate() {
            let target = widths[j];
            let (cell, cw) = if *width > target {
                let cut = clip_ansi_line(styled, target.saturating_sub(1));
                (format!("{cut}…"), target)
            } else {
                (styled.clone(), *width)
            };
            let cell = if has_header && ri == 0 {
                bold(&cell)
            } else {
                cell
            };
            let pad = target - cw;
            let (l, r) = match aligns.get(j).copied().unwrap_or(Align::Left) {
                Align::Left => (0, pad),
                Align::Right => (pad, 0),
                Align::Center => (pad / 2, pad - pad / 2),
            };
            if j > 0 {
                line.push(' ');
                line.push_str(&bar);
                line.push(' ');
            }
            line.push_str(&" ".repeat(l));
            line.push_str(&cell);
            line.push_str(&" ".repeat(r));
        }
        out.push(line);
        if has_header && ri == 0 {
            let mut rule = String::from("  ");
            for (j, wd) in widths.iter().enumerate() {
                if j > 0 {
                    rule.push_str("─┼─");
                }
                rule.push_str(&"─".repeat(*wd));
            }
            out.push(dim(&rule));
        }
    }
    out
}

// Bordered code-block chrome, shared by the streaming renderer and render_md().
pub(super) fn code_box_header(lang: &str, w: usize) -> String {
    let prefix = if lang.is_empty() {
        "  ╭─".to_string()
    } else {
        format!("  ╭─ ⟨ {lang} ⟩ ")
    };
    let used = str_width(&prefix);
    let dashes = w.saturating_sub(used).max(1);
    format!("{}{}", dim(&prefix), dim(&"─".repeat(dashes)))
}

pub(super) fn code_box_footer(w: usize) -> String {
    let prefix = "  ╰";
    let dashes = w.saturating_sub(str_width(prefix)).max(1);
    format!("{}{}", dim(prefix), dim(&"─".repeat(dashes)))
}

pub(super) fn code_box_line(text: &str) -> String {
    format!("  {} {text}", dim("│"))
}

pub(super) fn starts_like_top_level_json(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with('{') || trimmed.starts_with('[')
}

// Keep the JSON lookahead short: holding many lines makes streaming look
// frozen and then dump all at once. Past ~5 lines / 2KB, give up and flush
// the buffered text as plain output.
pub(super) fn maybe_json_buffer_is_too_large(lines: &[String]) -> bool {
    lines.len() > 5 || lines.iter().map(|line| line.len()).sum::<usize>() > 2 * 1024
}

pub(super) fn is_tool_call_json_block(lang: &str, code: &str) -> bool {
    let lang = lang.trim().to_ascii_lowercase();
    if matches!(
        lang.as_str(),
        "tool_code" | "tools" | "tool_call" | "function"
    ) {
        return true;
    }
    if !lang.is_empty() && lang != "json" {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(code.trim()) else {
        return false;
    };
    json_value_looks_like_tool_call(&value)
}

pub(super) fn json_value_looks_like_tool_call(value: &serde_json::Value) -> bool {
    if let Some(items) = value.as_array() {
        return !items.is_empty() && items.iter().all(json_value_looks_like_tool_call);
    }
    let Some(obj) = value.as_object() else {
        return false;
    };
    if obj
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .is_some_and(|items| items.iter().any(json_value_looks_like_tool_call))
    {
        return true;
    }
    let has_name = obj.get("name").and_then(|v| v.as_str()).is_some()
        || obj.get("tool_name").and_then(|v| v.as_str()).is_some();
    let has_args = obj.get("arguments").is_some()
        || obj.get("input").is_some()
        || obj
            .keys()
            .any(|k| k != "name" && k != "tool_name" && k != "type" && k != "id");
    let openai_function = obj
        .get("function")
        .and_then(|v| v.as_object())
        .is_some_and(|f| f.get("name").and_then(|v| v.as_str()).is_some());
    (has_name && has_args) || openai_function
}

// True only for terminals known to drop OSC 52: plain `xterm` (allowWindowOps
// is off by default) and VTE-based ones (GNOME Terminal & co.) that don't
// announce themselves via TERM_PROGRAM. Anything else — including an
// unadorned xterm-256color, which tmux/ssh/most emulators present — is
// "unsure" and keeps the copy confirmation. On WSL and macOS the copy also
// goes through clip.exe / pbcopy, so it works regardless of the terminal.
pub(super) fn osc52_known_unsupported_for(
    term: &str,
    term_program: Option<&str>,
    vte: bool,
) -> bool {
    if term_program.is_some_and(|p| !p.is_empty()) {
        return false;
    }
    if crate::tools::is_wsl() || cfg!(target_os = "macos") {
        return false;
    }
    vte || term == "xterm" || term == "linux"
}

pub(super) fn osc52_known_unsupported() -> bool {
    let term = std::env::var("TERM").unwrap_or_default();
    let term_program = std::env::var("TERM_PROGRAM").ok();
    let vte = std::env::var_os("VTE_VERSION").is_some();
    osc52_known_unsupported_for(&term, term_program.as_deref(), vte)
}

// Copy text to the terminal clipboard via the OSC 52 escape sequence.
// Works in raw/interactive mode only; a no-op in cooked/piped sessions.
pub(super) fn osc52_copy(text: &str) {
    if !is_raw() {
        return;
    }
    let encoded = b64_encode(text.as_bytes());
    print!("\x1b]52;c;{encoded}\x07");
    let _ = io::stdout().flush();
    // Native Windows and WSL both have clip.exe; the terminal may not honor
    // OSC 52, so mirror the text into the system clipboard as well.
    if cfg!(windows) || crate::tools::is_wsl() {
        let _ = pipe_to_clipboard_helper("clip.exe", text);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = pipe_to_clipboard_helper("pbcopy", text);
    }
}

// Hand `text` to a clipboard program on its stdin. A detached thread writes
// it, closes the pipe and waits for the program, so the TUI never blocks on
// a slow helper and each copy does not leave a zombie process behind. The
// handle yields the helper's exit status (tests join it; callers drop it).
pub(super) fn pipe_to_clipboard_helper(
    program: &str,
    text: &str,
) -> Option<std::thread::JoinHandle<Option<std::process::ExitStatus>>> {
    let mut child = std::process::Command::new(program)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let bytes = text.as_bytes().to_vec();
    Some(std::thread::spawn(move || {
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(&bytes);
        } // dropped here: the helper sees EOF and can exit
        child.wait().ok()
    }))
}

pub(super) fn b64_encode(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for ch in data.chunks(3) {
        let b0 = ch[0] as usize;
        let b1 = if ch.len() > 1 { ch[1] as usize } else { 0 };
        let b2 = if ch.len() > 2 { ch[2] as usize } else { 0 };
        out.push(A[b0 >> 2] as char);
        out.push(A[((b0 & 3) << 4) | (b1 >> 4)] as char);
        out.push(if ch.len() > 1 {
            A[((b1 & 0xf) << 2) | (b2 >> 6)] as char
        } else {
            '='
        });
        out.push(if ch.len() > 2 {
            A[b2 & 0x3f] as char
        } else {
            '='
        });
    }
    out
}
