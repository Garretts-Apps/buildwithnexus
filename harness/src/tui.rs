// Terminal layer: alternate screen, optional raw mode, ANSI colors, line input,
// and a spinner. Raw mode gives consistent key-driven input across platforms; in
// raw mode the kernel's line discipline is off, so every newline we emit must be
// "\r\n" and we echo keystrokes ourselves. Falls back to cooked line input when
// stdout isn't a TTY, so piped/headless use is unaffected.

use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use crossterm::cursor::{MoveTo, RestorePosition, SavePosition};
use crossterm::event::{
    poll, read, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
    EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, queue};

static RAW: AtomicBool = AtomicBool::new(false);
static ALT_SCREEN: AtomicBool = AtomicBool::new(false);
static MOUSE_CAPTURED: AtomicBool = AtomicBool::new(false);
static SCROLL_OFFSET: AtomicUsize = AtomicUsize::new(0);
// Set by poll_typeahead() when it absorbs a Ctrl+C so interrupted() still fires.
static TYPEAHEAD_INTERRUPTED: AtomicBool = AtomicBool::new(false);
static VIM_MODE: AtomicBool = AtomicBool::new(false);
static VIM_STATE_VAL: AtomicU8 = AtomicU8::new(0); // 0 = Normal, 1 = Insert, 2 = Visual

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimState {
    Insert,
    Normal,
    Visual(usize),
}

pub fn get_vim_state_label() -> &'static str {
    if !is_vim_mode() {
        return "";
    }
    match VIM_STATE_VAL.load(Ordering::Relaxed) {
        0 => "NORMAL",
        1 => "INSERT",
        2 => "VISUAL",
        _ => "NORMAL",
    }
}

pub fn toggle_vim_mode() -> bool {
    let old = VIM_MODE.load(Ordering::Relaxed);
    VIM_MODE.store(!old, Ordering::Relaxed);
    if !old {
        VIM_STATE_VAL.store(0, Ordering::Relaxed); // Default to Normal mode
    }
    !old
}

pub fn is_vim_mode() -> bool {
    VIM_MODE.load(Ordering::Relaxed)
}

#[derive(Clone, Copy)]
struct SelectPos {
    row: u16,
    col: u16,
}

#[derive(Clone, Copy)]
struct Selection {
    anchor: SelectPos,
    focus: SelectPos,
    // True for word/line selections made by double/triple click: the
    // mouse-up at the same cell must not collapse them back to one cell.
    sticky: bool,
}

// Transcript with an incremental word-wrap cache. Wrapping every line on
// every repaint is O(total chars) of ANSI parsing per streamed chunk /
// keystroke / scroll tick — the single biggest source of TUI lag on long
// sessions. Instead each line is wrapped once when it lands (or when it
// mutates), and a resize rewraps everything exactly once.
struct Transcript {
    lines: Vec<String>,
    wrapped: Vec<Vec<String>>, // parallel to `lines`, wrapped at `width`
    width: usize,              // 0 = not yet sized
}

impl Transcript {
    const fn new() -> Self {
        Transcript {
            lines: Vec::new(),
            wrapped: Vec::new(),
            width: 0,
        }
    }

    fn cur_width(&mut self) -> usize {
        if self.width == 0 {
            self.width = term_size().0 as usize;
        }
        self.width
    }

    fn len(&self) -> usize {
        self.lines.len()
    }

    fn push(&mut self, s: String) {
        let w = self.cur_width();
        self.wrapped.push(wrap_ansi_line(&s, w));
        self.lines.push(s);
    }

    fn set(&mut self, i: usize, s: String) {
        if i >= self.lines.len() {
            return;
        }
        let w = self.cur_width();
        self.wrapped[i] = wrap_ansi_line(&s, w);
        self.lines[i] = s;
    }

    // Append to line `i`, re-wrapping only that line. False when `i` is out
    // of range (no open stream line).
    fn append_to(&mut self, i: usize, chunk: &str) -> bool {
        if i >= self.lines.len() {
            return false;
        }
        self.lines[i].push_str(chunk);
        let w = self.cur_width();
        self.wrapped[i] = wrap_ansi_line(&self.lines[i], w);
        true
    }

    fn remove(&mut self, i: usize) {
        if i < self.lines.len() {
            self.lines.remove(i);
            self.wrapped.remove(i);
        }
    }

    fn drain_front(&mut self, n: usize) {
        let n = n.min(self.lines.len());
        self.lines.drain(0..n);
        self.wrapped.drain(0..n);
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.wrapped.clear();
    }

    // Rewrap everything if the terminal width changed (resize only).
    fn ensure_width(&mut self, w: usize) {
        if w != self.width {
            self.width = w;
            self.wrapped = self.lines.iter().map(|l| wrap_ansi_line(l, w)).collect();
        }
    }

    fn total_rows(&self) -> usize {
        self.wrapped.iter().map(|w| w.len()).sum()
    }

    // The visible window: `count` wrapped rows starting at row `start`,
    // without materializing the full flattened transcript.
    fn rows_range(&self, start: usize, count: usize) -> Vec<&String> {
        let mut out = Vec::with_capacity(count);
        let mut skipped = 0usize;
        for w in &self.wrapped {
            if out.len() >= count {
                break;
            }
            if skipped + w.len() <= start {
                skipped += w.len();
                continue;
            }
            let begin = start.saturating_sub(skipped);
            for row in &w[begin..] {
                out.push(row);
                if out.len() >= count {
                    break;
                }
            }
            skipped += w.len();
        }
        out
    }
}

fn transcript() -> &'static Mutex<Transcript> {
    static LINES: OnceLock<Mutex<Transcript>> = OnceLock::new();
    LINES.get_or_init(|| Mutex::new(Transcript::new()))
}

fn footer_text() -> &'static Mutex<String> {
    static FOOTER: OnceLock<Mutex<String>> = OnceLock::new();
    FOOTER.get_or_init(|| Mutex::new(String::new()))
}

fn visible_rows() -> &'static Mutex<Vec<String>> {
    static ROWS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    ROWS.get_or_init(|| Mutex::new(Vec::new()))
}

fn selection() -> &'static Mutex<Option<Selection>> {
    static SELECTION: OnceLock<Mutex<Option<Selection>>> = OnceLock::new();
    SELECTION.get_or_init(|| Mutex::new(None))
}

pub fn is_raw() -> bool {
    RAW.load(Ordering::Relaxed)
}

pub fn set_mouse_capture(enabled: bool) {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    if enabled {
        if !MOUSE_CAPTURED.swap(true, Ordering::Relaxed) {
            let _ = execute!(io::stdout(), EnableMouseCapture);
        }
    } else if MOUSE_CAPTURED.swap(false, Ordering::Relaxed) {
        let _ = execute!(io::stdout(), DisableMouseCapture);
    }
}

pub fn mouse_capture_enabled() -> bool {
    MOUSE_CAPTURED.load(Ordering::Relaxed)
}

// ── theme ────────────────────────────────────────────────────────────────
#[derive(Clone, Copy)]
pub struct Rgb(pub u8, pub u8, pub u8);

const BACKGROUND: Rgb = Rgb(0x1a, 0x1b, 0x26);
const ACCENT: Rgb = Rgb(0xbb, 0x9a, 0xf7);
const TEXT: Rgb = Rgb(0xc0, 0xca, 0xf5);
const MUTED: Rgb = Rgb(0x56, 0x5f, 0x89);
const SUCCESS: Rgb = Rgb(0x9e, 0xce, 0x6a);
const WARNING: Rgb = Rgb(0xe0, 0xaf, 0x68);
const ERROR: Rgb = Rgb(0xf7, 0x76, 0x8e);
const INFO: Rgb = Rgb(0x7d, 0xcf, 0xff);
const MODE_PLAN: Rgb = Rgb(0x9e, 0xce, 0x6a);
const MODE_BUILD: Rgb = Rgb(0x7a, 0xa2, 0xf7);
const MODE_BSTORM: Rgb = Rgb(0xe0, 0xaf, 0x68);
// Diff row tints (Tokyo Night DiffAdd/DiffDelete family): whole added/removed
// rows get a subtle background; the changed word span gets a stronger one.
const DIFF_ADD_BG: Rgb = Rgb(0x1e, 0x31, 0x26);
const DIFF_DEL_BG: Rgb = Rgb(0x37, 0x22, 0x2c);
const DIFF_ADD_EMPH_BG: Rgb = Rgb(0x2c, 0x4d, 0x38);
const DIFF_DEL_EMPH_BG: Rgb = Rgb(0x5a, 0x2e, 0x40);

fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some()
}

fn truecolor() -> bool {
    if no_color() {
        return false;
    }
    if let Ok(ct) = std::env::var("COLORTERM") {
        if ct == "truecolor" || ct == "24bit" {
            return true;
        }
        if ct == "256color" || ct == "no" || ct == "0" {
            return false;
        }
    }
    let term = std::env::var("TERM").unwrap_or_default();
    if term == "linux" || term == "vt100" || term == "dumb" {
        return false;
    }
    true
}

fn cube(c: u8) -> u32 {
    let levels = [0u8, 95, 135, 175, 215, 255];
    let mut best = 0usize;
    let mut bd = u16::MAX;
    for (i, &l) in levels.iter().enumerate() {
        let d = (l as i16 - c as i16).unsigned_abs();
        if d < bd {
            bd = d;
            best = i;
        }
    }
    best as u32
}

fn sgr_fg(c: Rgb) -> String {
    if truecolor() {
        format!("38;2;{};{};{}", c.0, c.1, c.2)
    } else {
        let idx = 16 + 36 * cube(c.0) + 6 * cube(c.1) + cube(c.2);
        format!("38;5;{idx}")
    }
}

fn sgr_bg(c: Rgb) -> String {
    if truecolor() {
        format!("48;2;{};{};{}", c.0, c.1, c.2)
    } else {
        let idx = 16 + 36 * cube(c.0) + 6 * cube(c.1) + cube(c.2);
        format!("48;5;{idx}")
    }
}

fn theme_bg() -> String {
    if no_color() {
        String::new()
    } else {
        format!("\x1b[{}m", sgr_bg(BACKGROUND))
    }
}

fn reset_all() -> String {
    if no_color() {
        String::new()
    } else if ALT_SCREEN.load(Ordering::Relaxed) {
        format!("\x1b[0m\x1b[{}m", sgr_fg(TEXT))
    } else {
        "\x1b[0m".to_string()
    }
}

fn reset_fg() -> String {
    if no_color() {
        String::new()
    } else if ALT_SCREEN.load(Ordering::Relaxed) {
        format!("\x1b[{}m", sgr_fg(TEXT))
    } else {
        "\x1b[39m".to_string()
    }
}

fn paint(c: Rgb, s: &str) -> String {
    if no_color() {
        return s.to_string();
    }
    format!("\x1b[{}m{s}{}", sgr_fg(c), reset_fg())
}

fn attr(code: &str, s: &str) -> String {
    if no_color() {
        return s.to_string();
    }
    let reset = match code {
        "1" => "\x1b[22m".to_string(),
        "3" => "\x1b[23m".to_string(),
        "4" => "\x1b[24m".to_string(),
        _ => {
            if ALT_SCREEN.load(Ordering::Relaxed) {
                format!("\x1b[0m\x1b[{}m", sgr_fg(TEXT))
            } else {
                "\x1b[0m".to_string()
            }
        }
    };
    format!("\x1b[{code}m{s}{reset}")
}

pub fn bold(s: &str) -> String {
    attr("1", s)
}
pub fn italic(s: &str) -> String {
    attr("3", s)
}
pub fn underline(s: &str) -> String {
    attr("4", s)
}
pub fn dim(s: &str) -> String {
    paint(MUTED, s)
}
pub fn red(s: &str) -> String {
    paint(ERROR, s)
}
pub fn green(s: &str) -> String {
    paint(SUCCESS, s)
}
pub fn yellow(s: &str) -> String {
    paint(WARNING, s)
}
pub fn blue(s: &str) -> String {
    paint(INFO, s)
}
pub fn cyan(s: &str) -> String {
    paint(INFO, s)
}
pub fn accent(s: &str) -> String {
    paint(ACCENT, s)
}
pub fn text(s: &str) -> String {
    paint(TEXT, s)
}

// Full-width dim horizontal rule (2-space indent, spans the terminal).
pub fn rule() -> String {
    let w = term_size().0 as usize;
    dim(&format!("  {}", "─".repeat(w.saturating_sub(4))))
}

// Mode-colored badge: PLAN (green), BUILD (blue), BRAINSTORM (amber).
pub fn mode_badge(mode: &str) -> String {
    let (label, color) = match mode {
        "PLAN" => ("PLAN", MODE_PLAN),
        "BRAINSTORM" => ("BRAINSTORM", MODE_BSTORM),
        _ => ("BUILD", MODE_BUILD),
    };
    if no_color() {
        format!("[{label}]")
    } else {
        format!("\x1b[{}m[{label}]{}", sgr_fg(color), reset_all())
    }
}

// ── diff paint helpers ───────────────────────────────────────────────────────
// Used by report's diff renderer: background-tinted spans in the GitHub /
// opencode style. The bg is restored to the theme background (alt-screen) or
// terminal default afterwards, mirroring reset_fg()'s approach.

fn reset_bg() -> String {
    if no_color() {
        String::new()
    } else if ALT_SCREEN.load(Ordering::Relaxed) {
        format!("\x1b[{}m", sgr_bg(BACKGROUND))
    } else {
        "\x1b[49m".to_string()
    }
}

fn on_bg(bg: Rgb, fg: Rgb, s: &str) -> String {
    if no_color() {
        return s.to_string();
    }
    format!(
        "\x1b[{};{}m{s}{}{}",
        sgr_bg(bg),
        sgr_fg(fg),
        reset_bg(),
        reset_fg()
    )
}

pub fn diff_add_span(s: &str) -> String {
    on_bg(DIFF_ADD_BG, SUCCESS, s)
}
pub fn diff_add_emph_span(s: &str) -> String {
    on_bg(DIFF_ADD_EMPH_BG, TEXT, s)
}
pub fn diff_del_span(s: &str) -> String {
    on_bg(DIFF_DEL_BG, ERROR, s)
}
pub fn diff_del_emph_span(s: &str) -> String {
    on_bg(DIFF_DEL_EMPH_BG, TEXT, s)
}

// Terminal width for layout done outside this module (diff gutters).
pub fn term_width() -> usize {
    term_size().0 as usize
}

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

enum StreamState {
    Normal,
    InCode { lang: String, lines: Vec<String> },
    MaybeJson { lines: Vec<String> },
}

pub struct StreamRenderer {
    pending: String,
    state: StreamState,
    w: usize, // terminal width cap for box drawing
    // Byte length of the `pending` prefix already echoed raw to the open
    // transcript line (alt-screen only; see echo_partial()).
    shown: usize,
    // Committed lines land here instead of the live transcript under test,
    // so chunk-split behaviour is assertable without a terminal.
    #[cfg(test)]
    sink: Vec<String>,
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
    fn emit(&mut self, s: &str) {
        #[cfg(test)]
        self.sink.push(s.to_string());
        #[cfg(not(test))]
        line(s);
    }

    pub fn push(&mut self, chunk: &str) {
        self.pending.push_str(chunk);
        self.drain(false);
    }

    // Call after the stream ends to flush any partial last line.
    pub fn flush(&mut self) {
        self.drain(true);
    }

    fn drain(&mut self, end: bool) {
        loop {
            match self.pending.find('\n') {
                Some(nl) => {
                    let line_text = self.pending[..nl].to_string();
                    self.pending = self.pending[nl + 1..].to_string();
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
    fn echo_partial(&mut self) {
        if !ALT_SCREEN.load(Ordering::Relaxed) || !matches!(self.state, StreamState::Normal) {
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

    fn process_line(&mut self, text: &str, had_partial: bool) {
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
                        osc52_copy(&code);
                        let notice = dim("  ✓ ⎘ copied to clipboard");
                        self.emit(&notice);
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
        }
    }

    fn finish_state(&mut self) {
        let state = std::mem::replace(&mut self.state, StreamState::Normal);
        match state {
            StreamState::Normal => {}
            StreamState::InCode { lang, lines } => {
                let code = lines.join("\n");
                if !is_tool_call_json_block(&lang, &code) {
                    self.render_code_block(&lang, &lines);
                    osc52_copy(&code);
                    let notice = dim("  ✓ ⎘ copied to clipboard");
                    self.emit(&notice);
                }
            }
            StreamState::MaybeJson { lines } => {
                self.state = StreamState::MaybeJson { lines };
                self.try_flush_maybe_json(true);
            }
        }
    }

    fn try_flush_maybe_json(&mut self, force: bool) {
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

    fn render_plain_lines(&mut self, lines: &[String]) {
        for text in lines {
            let rendered = render_md_line(text);
            self.emit(&rendered);
        }
    }

    fn render_code_block(&mut self, lang: &str, lines: &[String]) {
        // One emit for the whole block: line() splits on '\n', and a single
        // call means one repaint instead of one per code row.
        let mut rows = Vec::with_capacity(lines.len() + 2);
        rows.push(code_box_header(lang, self.w));
        rows.extend(lines.iter().map(|text| code_box_line(text)));
        rows.push(code_box_footer(self.w));
        self.emit(&rows.join("\n"));
    }
}

// Bordered code-block chrome, shared by the streaming renderer and render_md().
fn code_box_header(lang: &str, w: usize) -> String {
    let prefix = if lang.is_empty() {
        "  ╭─".to_string()
    } else {
        format!("  ╭─ ⟨ {lang} ⟩ ")
    };
    let used = str_width(&prefix);
    let dashes = w.saturating_sub(used).max(1);
    format!("{}{}", dim(&prefix), dim(&"─".repeat(dashes)))
}

fn code_box_footer(w: usize) -> String {
    let prefix = "  ╰";
    let dashes = w.saturating_sub(str_width(prefix)).max(1);
    format!("{}{}", dim(prefix), dim(&"─".repeat(dashes)))
}

fn code_box_line(text: &str) -> String {
    format!("  {} {text}", dim("│"))
}

fn starts_like_top_level_json(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with('{') || trimmed.starts_with('[')
}

// Keep the JSON lookahead short: holding many lines makes streaming look
// frozen and then dump all at once. Past ~5 lines / 2KB, give up and flush
// the buffered text as plain output.
fn maybe_json_buffer_is_too_large(lines: &[String]) -> bool {
    lines.len() > 5 || lines.iter().map(|line| line.len()).sum::<usize>() > 2 * 1024
}

fn is_tool_call_json_block(lang: &str, code: &str) -> bool {
    let lang = lang.trim().to_ascii_lowercase();
    if !lang.is_empty() && lang != "json" {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(code.trim()) else {
        return false;
    };
    json_value_looks_like_tool_call(&value)
}

fn json_value_looks_like_tool_call(value: &serde_json::Value) -> bool {
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

// Copy text to the terminal clipboard via the OSC 52 escape sequence.
// Works in raw/interactive mode only; a no-op in cooked/piped sessions.
fn osc52_copy(text: &str) {
    if !is_raw() {
        return;
    }
    let encoded = b64_encode(text.as_bytes());
    print!("\x1b]52;c;{encoded}\x07");
    let _ = io::stdout().flush();
    if crate::tools::is_wsl() {
        if let Ok(mut child) = std::process::Command::new("clip.exe")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(text.as_bytes());
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(mut child) = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(text.as_bytes());
            }
        }
    }
}

fn b64_encode(data: &[u8]) -> String {
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

// ── typeahead ─────────────────────────────────────────────────────────────────
// Buffers keystrokes typed while the agent is processing so they pre-fill the
// next input prompt — matching Claude Code's typeahead behaviour.

struct TypeAheadState {
    buf: Vec<char>,
    cursor: usize,
    // True while `buf` holds a message pulled out of the queue via Ctrl+Q, so
    // Esc can return it to the queue instead of destroying it.
    from_queue: bool,
}

fn typeahead() -> &'static std::sync::Mutex<TypeAheadState> {
    static TA: std::sync::OnceLock<std::sync::Mutex<TypeAheadState>> = std::sync::OnceLock::new();
    TA.get_or_init(|| {
        std::sync::Mutex::new(TypeAheadState {
            buf: Vec::new(),
            cursor: 0,
            from_queue: false,
        })
    })
}

fn message_queue() -> &'static std::sync::Mutex<Vec<String>> {
    static MQ: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> = std::sync::OnceLock::new();
    MQ.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Non-blocking drain of pending key events during agent processing.
/// Buffers printable input; Ctrl+C clears the buffer and signals an interrupt.
pub fn poll_typeahead() {
    if !is_raw() {
        return;
    }
    while poll(Duration::ZERO).unwrap_or(false) {
        match read() {
            Ok(Event::Key(k)) => {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                let alt = k.modifiers.contains(KeyModifiers::ALT);
                match k.code {
                    KeyCode::PageUp => {
                        scroll_page_up();
                        continue;
                    }
                    KeyCode::PageDown => {
                        scroll_page_down();
                        continue;
                    }
                    KeyCode::Up if alt => {
                        scroll_output(1);
                        continue;
                    }
                    KeyCode::Down if alt => {
                        scroll_output(-1);
                        continue;
                    }
                    KeyCode::Home if alt => {
                        scroll_output(isize::MAX / 4);
                        continue;
                    }
                    KeyCode::End if alt => {
                        scroll_to_bottom();
                        clear_composer();
                        render_footer();
                        continue;
                    }
                    _ => {}
                }
                let mut ta = match typeahead().lock() {
                    Ok(g) => g,
                    Err(_) => continue,
                };
                match k.code {
                    KeyCode::Enter => {
                        let text: String = ta.buf.iter().collect();
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            if let Ok(mut mq) = message_queue().lock() {
                                mq.push(trimmed.to_string());
                            }
                            ta.buf.clear();
                            ta.cursor = 0;
                            ta.from_queue = false;
                            // Drop the guard before rendering: render_output /
                            // render_queued_composer re-lock typeahead and the
                            // message queue, and std Mutex is not reentrant.
                            drop(ta);
                            render_output();
                            clear_composer();
                            render_footer();
                            render_queued_composer();
                        }
                        continue;
                    }
                    // Plain Up is intentionally a no-op for the queue: queue
                    // editing is Ctrl+Q (as the queued-row hint says), so a
                    // stray Up can't destructively pop the newest message.
                    KeyCode::Up if !alt => {
                        continue;
                    }
                    KeyCode::Char('q') if ctrl => {
                        // Pop under a short-lived lock, then render with no
                        // guards held (see deadlock note on Enter above).
                        let popped = message_queue().lock().ok().and_then(|mut mq| mq.pop());
                        if let Some(last) = popped {
                            ta.buf = last.chars().collect();
                            ta.cursor = ta.buf.len();
                            ta.from_queue = true;
                            drop(ta);
                            render_output();
                            clear_composer();
                            render_footer();
                            render_queued_composer();
                        }
                        continue;
                    }
                    KeyCode::Char('x') if ctrl => {
                        let removed = message_queue()
                            .lock()
                            .ok()
                            .and_then(|mut mq| mq.pop())
                            .is_some();
                        if removed {
                            drop(ta);
                            render_output();
                            clear_composer();
                            render_footer();
                            render_queued_composer();
                        }
                        continue;
                    }
                    KeyCode::Char('c') if ctrl => {
                        TYPEAHEAD_INTERRUPTED.store(true, Ordering::Relaxed);
                        ta.buf.clear();
                        ta.cursor = 0;
                        ta.from_queue = false;
                    }
                    KeyCode::Char('u') if ctrl => {
                        let d = ta.cursor;
                        ta.buf.drain(..d);
                        ta.cursor = 0;
                    }
                    KeyCode::Esc => {
                        // If the buffer holds a message dequeued via Ctrl+Q,
                        // Esc returns it to the queue instead of discarding it.
                        if ta.from_queue && !ta.buf.is_empty() {
                            let msg: String = ta.buf.iter().collect();
                            if let Ok(mut mq) = message_queue().lock() {
                                mq.push(msg);
                            }
                            ta.buf.clear();
                            ta.cursor = 0;
                            ta.from_queue = false;
                            drop(ta);
                            render_output();
                            clear_composer();
                            render_footer();
                            render_queued_composer();
                            continue;
                        }
                        if ta.buf.is_empty() {
                            // Esc with nothing typed interrupts the agent —
                            // any queued prompts auto-send at the next
                            // prompt, so Esc = "stop and move on".
                            TYPEAHEAD_INTERRUPTED.store(true, Ordering::Relaxed);
                        }
                        // Esc with a draft clears the draft only.
                        ta.buf.clear();
                        ta.cursor = 0;
                        ta.from_queue = false;
                    }
                    KeyCode::Backspace if !ctrl => {
                        if ta.cursor > 0 {
                            let i = ta.cursor - 1;
                            ta.buf.remove(i);
                            ta.cursor = i;
                        }
                    }
                    KeyCode::Delete => {
                        let i = ta.cursor;
                        if i < ta.buf.len() {
                            ta.buf.remove(i);
                        }
                    }
                    KeyCode::Left => {
                        ta.cursor = ta.cursor.saturating_sub(1);
                    }
                    KeyCode::Right => {
                        let i = ta.cursor;
                        if i < ta.buf.len() {
                            ta.cursor += 1;
                        }
                    }
                    KeyCode::Char(c) if !ctrl && !alt => {
                        let i = ta.cursor;
                        ta.buf.insert(i, c);
                        ta.cursor += 1;
                    }
                    _ => {}
                }
            }
            Ok(Event::Mouse(m)) => match m.kind {
                MouseEventKind::ScrollUp => scroll_output(3),
                MouseEventKind::ScrollDown => scroll_output(-3),
                MouseEventKind::Down(MouseButton::Left) => selection_start(m.row, m.column),
                MouseEventKind::Drag(MouseButton::Left) => selection_drag(m.row, m.column),
                MouseEventKind::Up(MouseButton::Left) => selection_finish(m.row, m.column),
                _ => {}
            },
            Ok(_) => {}
            Err(_) => break,
        }
    }
    render_queued_composer();
}

pub fn render_queued_composer() {
    if !is_raw() || !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    if let Ok(ta) = typeahead().lock() {
        if let Ok(mq) = message_queue().lock() {
            let mut out = io::stdout();
            let c_top = composer_top();
            let q_len = mq.len() as u16;
            for (i, msg) in mq.iter().enumerate() {
                let row = c_top.saturating_sub(q_len).saturating_add(i as u16);
                let _ = queue!(out, MoveTo(0, row), Clear(ClearType::CurrentLine));
                let _ = write!(
                    out,
                    "  {} {} {} {}",
                    dim("├─"),
                    dim("queued:"),
                    bold(msg),
                    dim("(Ctrl+Q edit, Ctrl+X rm)")
                );
            }
            let _ = out.flush();
        }
        let mut scroll = 0usize;
        render_composer(
            &format!("{} {} ", dim("queued"), accent("›")),
            &ta.buf,
            ta.cursor,
            &mut scroll,
        );
    }
}

fn take_typeahead() -> (Vec<char>, usize) {
    match typeahead().lock() {
        Ok(mut ta) => {
            let buf = std::mem::take(&mut ta.buf);
            let cur = std::mem::replace(&mut ta.cursor, 0);
            ta.from_queue = false;
            (buf, cur)
        }
        Err(_) => (Vec::new(), 0),
    }
}

fn term_size() -> (u16, u16) {
    let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
    (if w == 0 { 80 } else { w }, if h == 0 { 24 } else { h })
}

// Alt-screen bottom chrome: a 3-row bordered composer box plus the footer.
//   h-4  ╭───────────────────╮   composer_top()
//   h-3  │ › input text      │   composer_row()
//   h-2  ╰───────────────────╯   composer_bottom()
//   h-1  permission: ask · …     footer_row()
// Queued messages stack directly above the box.
fn reserved_rows() -> u16 {
    let q_len = message_queue().lock().map(|q| q.len()).unwrap_or(0) as u16;
    if ALT_SCREEN.load(Ordering::Relaxed) {
        4 + q_len
    } else {
        1 + q_len
    }
}

// Columns taken by the box's left border ("│ ") before the prompt.
const COMPOSER_PAD: u16 = 2;

fn composer_row() -> u16 {
    let (_, h) = term_size();
    if ALT_SCREEN.load(Ordering::Relaxed) {
        h.saturating_sub(3)
    } else {
        h.saturating_sub(1)
    }
}

fn composer_top() -> u16 {
    composer_row().saturating_sub(1)
}

fn composer_bottom() -> u16 {
    composer_row().saturating_add(1)
}

fn footer_row() -> u16 {
    term_size().1.saturating_sub(1)
}

pub fn char_width(c: char) -> usize {
    let u = c as u32;
    if (0x0300..=0x036F).contains(&u)
        || (0x1AB0..=0x1AFF).contains(&u)
        || (0x20D0..=0x20FF).contains(&u)
        || (0xFE00..=0xFE0F).contains(&u)
        || u == 0x200B
        || u == 0x200C
        || u == 0x200D
    {
        return 0;
    }
    if (0x1100..=0x115F).contains(&u)
        || (0x2329..=0x232A).contains(&u)
        || (0x2E80..=0x303E).contains(&u)
        || (0x3040..=0xA4CF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFE10..=0xFE19).contains(&u)
        || (0xFE30..=0xFE6F).contains(&u)
        || (0xFF01..=0xFF60).contains(&u)
        || (0xFFE0..=0xFFE6).contains(&u)
        || (0x1F000..=0x1FAFF).contains(&u)
        || (0x2600..=0x27BF).contains(&u)
        || (0x20000..=0x2FA1F).contains(&u)
        || (0x30000..=0x3134F).contains(&u)
    {
        return 2;
    }
    1
}

pub fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn format_links(s: &str) -> String {
    if !s.contains('[') || !s.contains("](") {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 32);
    let mut rest = s;
    while let Some(start) = rest.find('[') {
        if let Some(mid) = rest[start..].find("](") {
            let mid_abs = start + mid;
            if let Some(end) = rest[mid_abs..].find(')') {
                let end_abs = mid_abs + end;
                out.push_str(&rest[..start]);
                let label = &rest[start + 1..mid_abs];
                let url = &rest[mid_abs + 2..end_abs];
                // OSC 8: the label itself is clickable in modern terminals;
                // the dim URL suffix keeps older terminals usable.
                out.push_str(&format!(
                    "{} {}",
                    hyperlink(url, &underline(&cyan(label))),
                    dim(&format!("({url})"))
                ));
                rest = &rest[end_abs + 1..];
                continue;
            }
        }
        out.push_str(&rest[..start + 1]);
        rest = &rest[start + 1..];
    }
    out.push_str(rest);
    out
}

// Flush the pending plain-text run to `out`, wrapping it in the styles active
// when it was collected. Italic is applied inside bold so both can nest.
fn flush_styled_run(out: &mut String, buf: &mut String, bold_on: bool, italic_on: bool) {
    if buf.is_empty() {
        return;
    }
    let mut s = std::mem::take(buf);
    if italic_on {
        s = italic(&s);
    }
    if bold_on {
        s = bold(&s);
    }
    out.push_str(&s);
}

// Render inline Markdown (`` `code` ``, `**bold**`, `*italic*`) into ANSI in a
// single left-to-right pass. A marker only opens a style when a matching closer
// exists ahead and it isn't followed by whitespace, so an unmatched `` ` ``,
// `**`, or `*` — including arithmetic like `5 * 3` — stays literal instead of
// styling the rest of the line. Links are resolved first by `format_links`.
fn format_inline_md(text: &str) -> String {
    let linked = format_links(text);
    let chars: Vec<char> = linked.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(linked.len() + 32);
    let mut buf = String::new();
    let (mut bold_on, mut italic_on) = (false, false);
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if c == '`' {
            // Code span: style only if there's a closing backtick ahead.
            if let Some(rel) = chars[i + 1..].iter().position(|&x| x == '`') {
                flush_styled_run(&mut out, &mut buf, bold_on, italic_on);
                let code: String = chars[i + 1..i + 1 + rel].iter().collect();
                out.push_str(&yellow(&code));
                i += rel + 2;
                continue;
            }
            buf.push('`');
            i += 1;
            continue;
        }
        if c == '*' && i + 1 < n && chars[i + 1] == '*' {
            let toggles = if bold_on {
                i > 0 && !chars[i - 1].is_whitespace()
            } else {
                i + 2 < n
                    && !chars[i + 2].is_whitespace()
                    && chars[i + 2..]
                        .windows(2)
                        .any(|w| w[0] == '*' && w[1] == '*')
            };
            if toggles {
                flush_styled_run(&mut out, &mut buf, bold_on, italic_on);
                bold_on = !bold_on;
            } else {
                buf.push('*');
                buf.push('*');
            }
            i += 2;
            continue;
        }
        if c == '*' {
            let toggles = if italic_on {
                i > 0 && !chars[i - 1].is_whitespace()
            } else {
                i + 1 < n && !chars[i + 1].is_whitespace() && chars[i + 1..].contains(&'*')
            };
            if toggles {
                flush_styled_run(&mut out, &mut buf, bold_on, italic_on);
                italic_on = !italic_on;
            } else {
                buf.push('*');
            }
            i += 1;
            continue;
        }
        buf.push(c);
        i += 1;
    }
    flush_styled_run(&mut out, &mut buf, bold_on, italic_on);
    out
}

/// Renders a single line of Markdown into ANSI SGR terminal escape sequences.
///
/// This function parses and styles block-level constructs at the start of the line:
/// - Headers (`# `, `## `, `### `): Formatted in bold accent, cyan, and blue respectively.
/// - Blockquotes (`> `): Rendered with a dimmed vertical accent bar (`│`) and italicized text.
/// - Unordered lists (`- `, `* `): Rendered with a dimmed bullet point (`•`).
/// - Numbered lists (`1. `, `2. `): Formatted with bold cyan numbers.
///
/// It also processes inline formatting across the line:
/// - Inline code spans (`` `code` ``): Highlighted in yellow.
/// - Bold text (`**text**`): Styled with ANSI bold (`\x1b[1m`).
/// - Italic text (`*text*`): Styled with ANSI italic (`\x1b[3m`).
/// - Hyperlinks (`[label](url)`): Formatted with an underlined cyan label and dimmed URL.
///
/// If color is disabled via `NO_COLOR`, this returns the original unformatted line.
pub fn render_md_line(s: &str) -> String {
    if no_color() {
        return s.to_string();
    }
    let trimmed = s.trim_start();
    let indent = &s[..s.len().saturating_sub(trimmed.len())];
    if let Some(header) = trimmed.strip_prefix("### ") {
        return format!("{}{}", indent, bold(&blue(&format_inline_md(header))));
    }
    if let Some(header) = trimmed.strip_prefix("## ") {
        return format!("{}{}", indent, bold(&cyan(&format_inline_md(header))));
    }
    if let Some(header) = trimmed.strip_prefix("# ") {
        return format!("{}{}", indent, bold(&accent(&format_inline_md(header))));
    }
    if let Some(quote) = trimmed.strip_prefix("> ") {
        return format!(
            "{}  {} {}",
            indent,
            dim("│"),
            italic(&dim(&format_inline_md(quote)))
        );
    }
    let (prefix_span, rest) = if let Some(r) = trimmed.strip_prefix("- ") {
        (Some(dim("•")), r)
    } else if let Some(r) = trimmed.strip_prefix("* ") {
        (Some(dim("•")), r)
    } else if let Some(idx) = trimmed.find(". ") {
        if idx > 0 && idx <= 3 && trimmed[..idx].chars().all(|c| c.is_ascii_digit()) {
            let num_str = &trimmed[..idx + 1];
            (Some(bold(&cyan(num_str))), &trimmed[idx + 2..])
        } else {
            (None, trimmed)
        }
    } else {
        (None, trimmed)
    };

    let formatted_rest = format_inline_md(rest);

    if let Some(pref) = prefix_span {
        format!("{}  {} {}", indent, pref, formatted_rest)
    } else {
        format!("{}{}", indent, formatted_rest)
    }
}

/// Renders a multiline Markdown document into formatted ANSI terminal output.
///
/// Runs the same fence state machine as [`StreamRenderer`]: lines between
/// triple-backtick fences are drawn inside a bordered code block (with the
/// language label on the top border) and the fence markers themselves are
/// never shown raw. All other lines go through [`render_md_line`], so
/// headings, lists, quotes, links and inline styles render consistently
/// whether text arrives streamed or as a complete reply.
pub fn render_md(text: &str) -> String {
    let w = term_size().0 as usize;
    let mut out: Vec<String> = Vec::new();
    // Some(lang) while inside a fenced code block.
    let mut fence: Option<String> = None;
    for l in text.lines() {
        match fence {
            Some(_) => {
                if l.trim() == "```" {
                    out.push(code_box_footer(w));
                    fence = None;
                } else {
                    out.push(code_box_line(l));
                }
            }
            None => {
                if let Some(rest) = l.trim_start().strip_prefix("```") {
                    let lang = rest.trim().to_string();
                    out.push(code_box_header(&lang, w));
                    fence = Some(lang);
                } else {
                    out.push(render_md_line(l));
                }
            }
        }
    }
    // Unclosed fence: close the border so the block doesn't bleed on.
    if fence.is_some() {
        out.push(code_box_footer(w));
    }
    out.join("\n")
}

// Consume one escape sequence (the ESC itself already consumed). Handles
// CSI/SGR (ends at an ASCII letter) and OSC strings (ESC ] … BEL or ESC \),
// which OSC 8 hyperlinks use. `out` receives the consumed chars when the
// caller preserves escapes (clip/wrap); pass None to discard (strip).
fn eat_escape(chars: &mut std::iter::Peekable<std::str::Chars>, mut out: Option<&mut String>) {
    let mut push = |c: char| {
        if let Some(o) = out.as_deref_mut() {
            o.push(c);
        }
    };
    if chars.peek() == Some(&']') {
        // OSC: terminated by BEL or ST (ESC \).
        while let Some(d) = chars.next() {
            push(d);
            if d == '\x07' {
                break;
            }
            if d == '\x1b' {
                if chars.peek() == Some(&'\\') {
                    push(chars.next().unwrap_or('\\'));
                }
                break;
            }
        }
    } else {
        for d in chars.by_ref() {
            push(d);
            if d.is_ascii_alphabetic() {
                break;
            }
        }
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            eat_escape(&mut chars, None);
        } else {
            out.push(c);
        }
    }
    out
}

// OSC 8 terminal hyperlink: `label` becomes clickable (opens `url`) in
// supporting terminals — iTerm2, kitty, WezTerm, Windows Terminal, GNOME
// Terminal, foot, and most modern emulators. Plain label elsewhere.
pub fn hyperlink(url: &str, label: &str) -> String {
    if no_color() || !io::stdout().is_terminal() {
        return label.to_string();
    }
    format!("\x1b]8;;{url}\x1b\\{label}\x1b]8;;\x1b\\")
}

// Clickable file link: resolves to an absolute file:// URL so terminals can
// open the document/screenshot in the OS default app on click.
pub fn file_link(path: &str, label: &str) -> String {
    let abs = std::fs::canonicalize(path)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| {
            let p = std::path::Path::new(path);
            if p.is_absolute() {
                path.to_string()
            } else {
                std::env::current_dir()
                    .unwrap_or_default()
                    .join(p)
                    .display()
                    .to_string()
            }
        });
    hyperlink(&format!("file://{abs}"), label)
}

fn prompt_width(prompt: &str) -> u16 {
    str_width(&strip_ansi(prompt)).min(u16::MAX as usize) as u16
}

fn set_output_region() {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    let (_, h) = term_size();
    let bottom = h.saturating_sub(reserved_rows()).max(1);
    print!("\x1b[1;{bottom}r\x1b[1;1H");
    flush();
}

fn reset_output_region() {
    if ALT_SCREEN.load(Ordering::Relaxed) {
        print!("\x1b[r");
        flush();
    }
}

fn clear_composer() {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    let mut out = io::stdout();
    for row in composer_top()..=composer_bottom() {
        let _ = queue!(out, MoveTo(0, row), Clear(ClearType::CurrentLine));
    }
    let _ = out.flush();
}

// Draw the composer box borders and return with the cursor ready at the
// start of the input row's content area. The caller writes the inner line.
fn queue_composer_box(out: &mut io::Stdout) {
    let (width, _) = term_size();
    let w = width as usize;
    let top = format!("╭{}╮", "─".repeat(w.saturating_sub(2)));
    let bottom = format!("╰{}╯", "─".repeat(w.saturating_sub(2)));
    let _ = queue!(out, MoveTo(0, composer_top()), Clear(ClearType::CurrentLine));
    let _ = write!(out, "{}", dim(&top));
    let _ = queue!(
        out,
        MoveTo(0, composer_bottom()),
        Clear(ClearType::CurrentLine)
    );
    let _ = write!(out, "{}", dim(&bottom));
    let _ = queue!(out, MoveTo(0, composer_row()), Clear(ClearType::CurrentLine));
    let _ = write!(out, "{} ", dim("│"));
}

// Close the input row with the right-hand border, clipping anything that
// would collide with it.
fn queue_composer_right_border(out: &mut io::Stdout) {
    let (width, _) = term_size();
    let _ = queue!(out, MoveTo(width.saturating_sub(1), composer_row()));
    let _ = write!(out, "{}", dim("│"));
}

fn queue_footer(out: &mut io::Stdout) {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    let Ok(footer) = footer_text().lock() else {
        return;
    };
    let (width, _) = term_size();
    let _ = queue!(out, MoveTo(0, footer_row()), Clear(ClearType::CurrentLine));
    if !footer.is_empty() {
        let offset = SCROLL_OFFSET.load(Ordering::Relaxed);
        let vim_badge = if is_vim_mode() {
            let label = get_vim_state_label();
            let colored = match label {
                "NORMAL" => green(&format!("[VIM:{label}]")),
                "INSERT" => yellow(&format!("[VIM:{label}]")),
                "VISUAL" => cyan(&format!("[VIM:{label}]")),
                _ => green("[VIM]"),
            };
            format!(" {} ", bold(&colored))
        } else {
            String::new()
        };
        let mut text = if offset > 0 {
            format!(
                "{}{} {}",
                vim_badge,
                footer.as_str(),
                dim(&format!("· scroll +{offset}"))
            )
        } else {
            format!("{}{}", vim_badge, footer.as_str())
        };
        if let Some(fl) = active_flash() {
            text = format!("{text}  {}", accent(&fl));
        }
        let _ = write!(out, "{}", clip_ansi_line(&text, width as usize));
    }
}

fn render_footer() {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    let mut out = io::stdout();
    queue_footer(&mut out);
    let _ = out.flush();
}

// ── footer flash ─────────────────────────────────────────────────────────────
// Transient feedback ("⎘ copied 42 chars") appended to the footer for ~2s —
// visible confirmation without polluting the transcript.
static FLASH_UNTIL_MS: AtomicU64 = AtomicU64::new(0);

fn flash_text() -> &'static Mutex<String> {
    static FLASH: OnceLock<Mutex<String>> = OnceLock::new();
    FLASH.get_or_init(|| Mutex::new(String::new()))
}

pub fn flash_footer(msg: &str) {
    if let Ok(mut f) = flash_text().lock() {
        *f = msg.to_string();
    }
    FLASH_UNTIL_MS.store(monotonic_ms() + 2_000, Ordering::Relaxed);
    render_footer();
}

fn active_flash() -> Option<String> {
    if monotonic_ms() >= FLASH_UNTIL_MS.load(Ordering::Relaxed) {
        return None;
    }
    flash_text()
        .lock()
        .ok()
        .map(|f| f.clone())
        .filter(|f| !f.is_empty())
}

// ── cursor shape & visibility ────────────────────────────────────────────────
// DECSCUSR shapes + OSC 12 cursor color. The prompt gets a blinking accent
// bar; vim NORMAL a steady block, VISUAL a steady underline. The cursor is
// hidden while the agent works (it otherwise flickers across the screen with
// every repaint) and restored at every prompt and on exit/panic.

#[derive(Clone, Copy, PartialEq)]
pub enum CursorShape {
    Bar,
    Block,
    Underline,
}

pub fn set_cursor_shape(shape: CursorShape) {
    if !is_raw() {
        return;
    }
    let n = match shape {
        CursorShape::Bar => 5,       // blinking bar
        CursorShape::Block => 2,     // steady block
        CursorShape::Underline => 4, // steady underline
    };
    print!("\x1b[{n} q");
    flush();
}

fn cursor_color_accent() {
    if no_color() {
        return;
    }
    print!("\x1b]12;#bb9af7\x07");
    flush();
}

fn cursor_reset_style() {
    print!("\x1b[0 q\x1b]112\x07");
    flush();
}

pub fn cursor_hide() {
    if ALT_SCREEN.load(Ordering::Relaxed) {
        print!("\x1b[?25l");
        flush();
    }
}

pub fn cursor_show() {
    print!("\x1b[?25h");
    flush();
}

pub fn set_permission_mode(mode: &str) {
    if let Ok(mut footer) = footer_text().lock() {
        *footer = format!(
            "{} {} {}",
            dim("permission:"),
            bold(mode),
            dim("· /permissions · wheel/PgUp · drag-copy · /mouse")
        );
    }
    let mut out = io::stdout();
    let _ = execute!(out, SavePosition);
    render_footer();
    let _ = execute!(out, RestorePosition);
}

fn render_output() {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    let (width, height) = term_size();
    let width = width as usize;
    let rows = height.saturating_sub(reserved_rows()) as usize;
    let Ok(mut t) = transcript().lock() else {
        return;
    };
    t.ensure_width(width);
    let total = t.total_rows();
    let max_offset = total.saturating_sub(rows);
    let offset = SCROLL_OFFSET.load(Ordering::Relaxed).min(max_offset);
    if offset != SCROLL_OFFSET.load(Ordering::Relaxed) {
        SCROLL_OFFSET.store(offset, Ordering::Relaxed);
    }
    let start = total.saturating_sub(rows + offset);
    let visible = t.rows_range(start, rows);
    let mut out = io::stdout();
    // Synchronized output (DEC 2026): supporting terminals (kitty, iTerm2,
    // WezTerm, Alacritty, foot…) apply the whole repaint as one atomic frame
    // — zero tearing/flicker. Ignored elsewhere.
    let _ = write!(out, "\x1b[?2026h");
    let sel = selection().lock().ok().and_then(|g| *g);
    let mut plain_rows = Vec::with_capacity(rows);
    for row in 0..rows {
        let _ = queue!(out, MoveTo(0, row as u16), Clear(ClearType::CurrentLine));
        if let Some(line) = visible.get(row) {
            let plain = strip_ansi(line);
            if let Some(sel) = sel {
                if let Some(range) = selection_range_for(sel, row as u16, plain.chars().count()) {
                    let _ = write!(
                        out,
                        "{}",
                        clip_ansi_line(&selected_line(&plain, range), width)
                    );
                } else {
                    let _ = write!(out, "{line}");
                }
            } else {
                let _ = write!(out, "{line}");
            }
            plain_rows.push(plain);
        } else {
            plain_rows.push(String::new());
        }
    }
    drop(t);
    if let Ok(mut rows) = visible_rows().lock() {
        *rows = plain_rows;
    }
    let _ = write!(out, "\x1b[?2026l");
    let _ = out.flush();
    render_queued_composer();
}

// ── streaming frame coalescing ───────────────────────────────────────────────
// Streamed token chunks can arrive hundreds of times per second; painting the
// screen for each one wastes CPU and looks like flicker, not speed. Cap
// streaming repaints at ~60fps. Correctness doesn't depend on any single
// frame: render_output() is a stateless full repaint, and the unthrottled
// paths (line(), commit_stream_line(), assistant_end) always paint the final
// state.
const FRAME_MS: u64 = 16;
static LAST_STREAM_FRAME_MS: AtomicU64 = AtomicU64::new(0);

fn monotonic_ms() -> u64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

fn render_output_throttled() {
    let now = monotonic_ms();
    let last = LAST_STREAM_FRAME_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= FRAME_MS || last == 0 {
        LAST_STREAM_FRAME_MS.store(now.max(1), Ordering::Relaxed);
        render_output();
        clear_composer();
    }
}

fn scroll_output(delta: isize) {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    let current = SCROLL_OFFSET.load(Ordering::Relaxed);
    let next = if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize)
    };
    SCROLL_OFFSET.store(next, Ordering::Relaxed);
    render_output();
    clear_composer();
    render_footer();
}

fn scroll_page_up() {
    let rows = term_size().1.saturating_sub(reserved_rows()).max(1) as usize;
    scroll_output(rows.saturating_sub(1).max(1) as isize);
}

fn scroll_page_down() {
    let rows = term_size().1.saturating_sub(reserved_rows()).max(1) as usize;
    scroll_output(-((rows.saturating_sub(1).max(1)) as isize));
}

fn scroll_to_bottom() {
    SCROLL_OFFSET.store(0, Ordering::Relaxed);
    render_output();
}

fn in_output_region(row: u16) -> bool {
    ALT_SCREEN.load(Ordering::Relaxed) && row < composer_top()
}

// Multi-click detection: (last click ms, row, col, count). Two clicks on the
// same cell within 400ms select the word, three the whole line.
static LAST_CLICK: Mutex<(u64, u16, u16, u8)> = Mutex::new((0, 0, 0, 0));

fn click_count(row: u16, col: u16) -> u8 {
    let now = monotonic_ms();
    let mut guard = match LAST_CLICK.lock() {
        Ok(g) => g,
        Err(_) => return 1,
    };
    let (t, r, c, n) = *guard;
    let count = if now.saturating_sub(t) <= 400 && r == row && c == col {
        (n % 3) + 1
    } else {
        1
    };
    *guard = (now, row, col, count);
    count
}

// Word span (code-friendly: [A-Za-z0-9_], else a non-space run) around `col`
// in the plain text of the visible row. None when the cell is blank.
fn word_span_at(line: &str, col: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = line.chars().collect();
    let c = *chars.get(col)?;
    if c.is_whitespace() {
        return None;
    }
    let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
    let class = if ident(c) { 0 } else { 1 };
    let same = |ch: char| {
        if class == 0 {
            ident(ch)
        } else {
            !ch.is_whitespace() && !ident(ch)
        }
    };
    let mut start = col;
    while start > 0 && same(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < chars.len() && same(chars[end + 1]) {
        end += 1;
    }
    Some((start, end))
}

fn selection_start(row: u16, col: u16) {
    if !in_output_region(row) {
        return;
    }
    let clicks = click_count(row, col);
    let new_sel = match clicks {
        // Double click: select the word under the cursor.
        2 => {
            let span = visible_rows()
                .lock()
                .ok()
                .and_then(|rows| rows.get(row as usize).cloned())
                .and_then(|line| word_span_at(&line, col as usize));
            match span {
                Some((s, e)) => Selection {
                    anchor: SelectPos { row, col: s as u16 },
                    focus: SelectPos { row, col: e as u16 },
                    sticky: true,
                },
                None => Selection {
                    anchor: SelectPos { row, col },
                    focus: SelectPos { row, col },
                    sticky: false,
                },
            }
        }
        // Triple click: select the whole visible line.
        3 => {
            let len = visible_rows()
                .lock()
                .ok()
                .and_then(|rows| rows.get(row as usize).map(|l| l.chars().count()))
                .unwrap_or(0);
            Selection {
                anchor: SelectPos { row, col: 0 },
                focus: SelectPos {
                    row,
                    col: len.saturating_sub(1) as u16,
                },
                sticky: true,
            }
        }
        _ => Selection {
            anchor: SelectPos { row, col },
            focus: SelectPos { row, col },
            sticky: false,
        },
    };
    if let Ok(mut sel) = selection().lock() {
        *sel = Some(new_sel);
    }
    render_output();
    render_queued_composer();
}

fn selection_drag(row: u16, col: u16) {
    if !in_output_region(row) {
        return;
    }
    if let Ok(mut sel) = selection().lock() {
        if let Some(s) = sel.as_mut() {
            s.focus = SelectPos { row, col };
            s.sticky = false;
        }
    }
    render_output();
    render_queued_composer();
}

fn selection_finish(row: u16, col: u16) {
    if in_output_region(row) {
        if let Ok(mut sel) = selection().lock() {
            if let Some(s) = sel.as_mut() {
                // A word/line selection stays put on the release click.
                if !s.sticky {
                    s.focus = SelectPos { row, col };
                }
            }
        }
    }
    let copied = selected_text();
    if let Ok(mut sel) = selection().lock() {
        *sel = None;
    }
    render_output();
    if let Some(text) = copied.filter(|s| !s.trim().is_empty()) {
        osc52_copy(&text);
        let n = text.chars().count();
        flash_footer(&format!("⎘ copied {n} chars"));
    }
    render_queued_composer();
}

fn normalized_selection(sel: Selection) -> (SelectPos, SelectPos) {
    let a = sel.anchor;
    let b = sel.focus;
    if (a.row, a.col) <= (b.row, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

fn selection_range_for(sel: Selection, row: u16, line_len: usize) -> Option<(usize, usize)> {
    let (start, end) = normalized_selection(sel);
    if row < start.row || row > end.row {
        return None;
    }
    let from = if row == start.row {
        start.col as usize
    } else {
        0
    }
    .min(line_len);
    let to = if row == end.row {
        (end.col as usize).saturating_add(1)
    } else {
        line_len
    }
    .min(line_len);
    (to > from).then_some((from, to))
}

// Theme selection tint (Tokyo Night visual-select) — far gentler than
// inverse video, which flashed harsh white blocks over the transcript.
const SELECTION_BG: Rgb = Rgb(0x28, 0x34, 0x57);

fn selection_span(s: &str) -> String {
    if no_color() {
        return format!("\x1b[7m{s}\x1b[27m");
    }
    on_bg(SELECTION_BG, TEXT, s)
}

fn selected_line(line: &str, range: (usize, usize)) -> String {
    let chars: Vec<char> = line.chars().collect();
    let (from, to) = range;
    let before: String = chars.iter().take(from).collect();
    let mid: String = chars.iter().skip(from).take(to - from).collect();
    let after: String = chars.iter().skip(to).collect();
    format!("{before}{}{after}", selection_span(&mid))
}

fn selected_text() -> Option<String> {
    let sel = selection().lock().ok().and_then(|g| *g)?;
    let rows = visible_rows().lock().ok()?;
    let (start, end) = normalized_selection(sel);
    let mut out = Vec::new();
    for row in start.row..=end.row {
        let line = rows.get(row as usize).map(String::as_str).unwrap_or("");
        let len = line.chars().count();
        if let Some((from, to)) = selection_range_for(sel, row, len) {
            out.push(line.chars().skip(from).take(to - from).collect::<String>());
        } else if row > start.row && row < end.row {
            out.push(String::new());
        }
    }
    Some(out.join("\n"))
}

fn render_composer(prompt: &str, buf: &[char], cursor: usize, scroll: &mut usize) {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    let (width, _) = term_size();
    let pwidth = prompt_width(prompt);
    // Room inside the box: left border "│ " + prompt … text … " │" right border.
    let avail = width
        .saturating_sub(pwidth)
        .saturating_sub(COMPOSER_PAD + 2)
        .max(8) as usize;
    let (s, _col) = viewport(cursor, avail, *scroll);
    *scroll = s;
    let end = (s + avail).min(buf.len());
    let shown: String = buf[s..end].iter().collect();
    let col_width = buf[s..cursor.min(buf.len())]
        .iter()
        .copied()
        .map(char_width)
        .sum::<usize>();
    let mut out = io::stdout();
    queue_composer_box(&mut out);
    let _ = write!(out, "{prompt}{shown}");
    queue_composer_right_border(&mut out);
    queue_footer(&mut out);
    let _ = queue!(
        out,
        MoveTo(
            COMPOSER_PAD
                .saturating_add(pwidth)
                .saturating_add(col_width as u16),
            composer_row()
        )
    );
    let _ = out.flush();
}

fn echo_submitted(prompt: &str, text: &str) {
    if ALT_SCREEN.load(Ordering::Relaxed) {
        SCROLL_OFFSET.store(0, Ordering::Relaxed);
        clear_composer();
        line(&format!("{prompt}{text}"));
    } else {
        print!("\r\n");
        flush();
    }
}

// ── startup banner ───────────────────────────────────────────────────────────
// Gradient wordmark: each letter of "buildwithnexus" shifts across purple→cyan→green.
fn wordmark() -> String {
    if no_color() {
        return "buildwithnexus".to_string();
    }
    // Gradient stops: Tokyo Night purple → blue → cyan → green.
    let stops: &[(u8, u8, u8)] = &[
        (0xbb, 0x9a, 0xf7),
        (0x7a, 0xa2, 0xf7),
        (0x7d, 0xcf, 0xff),
        (0x9e, 0xce, 0x6a),
    ];
    let word = "buildwithnexus";
    let n = word.len();
    word.chars()
        .enumerate()
        .map(|(i, c)| {
            let t = i as f32 / (n - 1) as f32;
            let seg = (t * (stops.len() - 1) as f32) as usize;
            let seg = seg.min(stops.len() - 2);
            let local = t * (stops.len() - 1) as f32 - seg as f32;
            let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * local) as u8;
            let (r, g, b) = (
                lerp(stops[seg].0, stops[seg + 1].0),
                lerp(stops[seg].1, stops[seg + 1].1),
                lerp(stops[seg].2, stops[seg + 1].2),
            );
            paint(Rgb(r, g, b), &c.to_string())
        })
        .collect::<Vec<_>>()
        .join("")
}

// Print a rich full-screen-style header that establishes visual context without
// taking over the alternate screen buffer (native scroll still works).
// The UI chrome (mode badge, wordmark, keys) is identical regardless of model.
pub fn show_banner(provider: &str, model: &str, mode: &str, cwd: &str) {
    let w = term_size().0 as usize;

    line("");
    // Wordmark row — gradient "buildwithnexus" + version.
    line(&clip_ansi_line(
        &format!(
            "  {}  {}",
            bold(&wordmark()),
            dim(&format!("v{}", crate::VERSION)),
        ),
        w,
    ));
    line(&dim(&format!("  {}", "─".repeat(w.saturating_sub(4)))));
    // Aligned key/value context rows: dim keys, plain values.
    line(&clip_ansi_line(
        &format!("  {}  {provider} · {model}", dim("model")),
        w,
    ));
    let cwd_display: String = cwd
        .chars()
        .rev()
        .take(w.saturating_sub(12))
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let cwd_label = if cwd_display.len() < cwd.len() {
        format!("…{cwd_display}")
    } else {
        cwd.to_string()
    };
    line(&clip_ansi_line(
        &format!("  {}    {}", dim("cwd"), dim(&cwd_label)),
        w,
    ));
    line(&banner_mode_row(mode, w));
    line("");
}

fn banner_mode_row(mode: &str, width: usize) -> String {
    clip_ansi_line(
        &format!(
            "  {}   {}   {}",
            dim("mode"),
            mode_badge(mode),
            dim("Shift+Tab cycle · /help commands"),
        ),
        width,
    )
}

fn refresh_banner_mode(mode: &str) {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    let width = term_size().0 as usize;
    let row = banner_mode_row(mode, width);
    let Ok(mut t) = transcript().lock() else {
        return;
    };
    // "Shift+Tab cycle" is unique to the banner's mode row (the REPL hint
    // line says "Shift+Tab to change mode").
    let idx = t
        .lines
        .iter()
        .position(|l| strip_ansi(l).contains("Shift+Tab cycle"));
    if let Some(i) = idx {
        t.set(i, row);
    }
}

// Refresh the mode indicator line in-place after a mode change (no full clear).
pub fn show_mode_change(mode: &str) {
    refresh_banner_mode(mode);
    render_output();
    line(&format!(
        "  {} mode → {}",
        dim("⟳ switching"),
        mode_badge(mode)
    ));
}

// Live context-window meter — call after each API round-trip.
// Color shifts green → yellow → red as the window fills up.
pub fn context_meter(used: usize, total: usize) {
    if total == 0 {
        return;
    }
    let pct = (used * 100 / total).min(100);
    let bar_width = 20usize;
    let filled = (pct * bar_width / 100).min(bar_width);
    let bar: String = "█".repeat(filled) + &"░".repeat(bar_width - filled);
    let colored = if pct >= 80 {
        red(&bar)
    } else if pct >= 60 {
        yellow(&bar)
    } else {
        green(&bar)
    };
    line(&format!(
        "  {} [{}] {}",
        dim("context"),
        colored,
        dim(&format!(
            "{pct}% · {}k / {}k tokens",
            used / 1_000,
            total / 1_000,
        )),
    ));
}

// Enter the alternate screen and raw mode (and capture panics to restore the
// terminal even on crash). The bottom row is reserved for the composer; output
// scrolls in the region above it.
pub fn enter_alt(raw: bool) {
    if raw {
        SCROLL_OFFSET.store(0, Ordering::Relaxed);
        invalidate_stream_line();
        if let Ok(mut t) = transcript().lock() {
            t.clear();
        }
        let mut out = io::stdout();
        // Some terminals preserve the user's current scrollback viewport when
        // switching buffers. Force the normal screen to its bottom first, then
        // aggressively clear/home the alternate screen after entering it.
        let _ = write!(out, "\x1b[9999B");
        let _ = execute!(out, EnterAlternateScreen);
        let _ = write!(out, "{}\x1b[H\x1b[2J\x1b[3J", theme_bg());
        let _ = execute!(out, Clear(ClearType::All), MoveTo(0, 0));
        let _ = out.flush();
        ALT_SCREEN.store(true, Ordering::Relaxed);
        set_output_region();
        let _ = execute!(io::stdout(), MoveTo(0, 0));
        // Never show a black frame: paint the composer box and footer right
        // away, before the caller draws the banner. If anything later stalls,
        // the screen still shows chrome instead of a void.
        {
            let mut out = io::stdout();
            queue_composer_box(&mut out);
            queue_composer_right_border(&mut out);
            let _ = out.flush();
        }
        render_footer();
        let _ = execute!(io::stdout(), MoveTo(0, 0));
    }
    if raw && enable_raw_mode().is_ok() {
        RAW.store(true, Ordering::Relaxed);
        let _ = execute!(io::stdout(), EnableBracketedPaste);
        set_mouse_capture(true);
        // Accent-colored blinking bar cursor for the composer.
        cursor_color_accent();
        set_cursor_shape(CursorShape::Bar);
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        reset_output_region();
        cursor_reset_style();
        cursor_show();
        let _ = write!(io::stdout(), "\x1b[0m");
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        MOUSE_CAPTURED.store(false, Ordering::Relaxed);
        ALT_SCREEN.store(false, Ordering::Relaxed);
        let _ = disable_raw_mode();
        prev(info);
    }));
}

pub fn leave_alt() {
    clear_composer();
    reset_output_region();
    if RAW.load(Ordering::Relaxed) {
        cursor_reset_style();
        cursor_show();
    }
    if RAW.swap(false, Ordering::Relaxed) {
        let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
        MOUSE_CAPTURED.store(false, Ordering::Relaxed);
        let _ = disable_raw_mode();
    }
    if ALT_SCREEN.swap(false, Ordering::Relaxed) {
        let _ = write!(io::stdout(), "\x1b[0m");
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

pub fn clear() {
    if ALT_SCREEN.load(Ordering::Relaxed) {
        SCROLL_OFFSET.store(0, Ordering::Relaxed);
        invalidate_stream_line();
        if let Ok(mut t) = transcript().lock() {
            t.clear();
        }
        let _ = write!(io::stdout(), "{}", theme_bg());
        let _ = execute!(io::stdout(), Clear(ClearType::All), MoveTo(0, 0));
        set_output_region();
        clear_composer();
        render_footer();
    } else {
        print!("\x1b[2J\x1b[H");
        flush();
    }
}

pub fn browse_items(title: &str, items: &[(String, String)]) {
    if !is_raw() || !ALT_SCREEN.load(Ordering::Relaxed) {
        line(&accent(&format!("  {title}")));
        for (name, detail) in items {
            let first = detail.lines().next().unwrap_or("");
            line(&format!("  {}  {}", bold(name), dim(first)));
        }
        return;
    }

    reset_output_region();
    let mut selected = 0usize;
    let mut detail = false;
    loop {
        draw_browser(title, items, selected, detail);
        match read() {
            Ok(Event::Key(k)) => {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') => break,
                    KeyCode::Up | KeyCode::Char('k') if !detail => {
                        selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') if !detail => {
                        if selected + 1 < items.len() {
                            selected += 1;
                        }
                    }
                    KeyCode::Enter | KeyCode::Right if !detail => detail = true,
                    KeyCode::Left | KeyCode::Backspace if detail => detail = false,
                    _ => {}
                }
            }
            Ok(Event::Resize(_, _)) => {}
            _ => {}
        }
    }
    let _ = execute!(io::stdout(), Clear(ClearType::All), MoveTo(0, 0));
    set_output_region();
    render_output();
    clear_composer();
    render_footer();
}

fn draw_browser(title: &str, items: &[(String, String)], selected: usize, detail: bool) {
    let (width, height) = term_size();
    let mut out = io::stdout();
    let _ = queue!(out, MoveTo(0, 0), Clear(ClearType::All));
    let _ = writeln!(out, "{}", accent(&format!("  {title}")));
    let _ = writeln!(
        out,
        "{}",
        dim("  ↑↓/jk navigate · Enter inspect · ← back · Esc/q close")
    );
    let _ = writeln!(out);

    let body_rows = height.saturating_sub(4) as usize;
    if detail {
        if let Some((name, text)) = items.get(selected) {
            let _ = writeln!(out, "  {}", bold(name));
            let max = body_rows.saturating_sub(1);
            for line in text.lines().take(max) {
                let clipped: String = line
                    .chars()
                    .take(width.saturating_sub(4) as usize)
                    .collect();
                let _ = writeln!(out, "  {clipped}");
            }
        }
    } else {
        let start = selected.saturating_sub(body_rows / 2);
        for (idx, (name, detail)) in items.iter().enumerate().skip(start).take(body_rows) {
            let marker = if idx == selected {
                accent("›")
            } else {
                dim(" ")
            };
            let first = detail.lines().next().unwrap_or("");
            let row = format!("{marker} {}  {}", bold(name), dim(first));
            let clipped: String = row.chars().take(width.saturating_sub(1) as usize).collect();
            let _ = writeln!(out, "{clipped}");
        }
    }
    let _ = out.flush();
}

// Clip to at most `max_cols` display columns (not chars): emoji/CJK count as
// their char_width() so a width-2 char is never split across the boundary.
fn clip_ansi_line(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut visible = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            eat_escape(&mut chars, Some(&mut out));
            continue;
        }
        let w = char_width(c);
        if visible + w > max_cols {
            break;
        }
        out.push(c);
        visible += w;
    }
    out
}

// Wrap into rows of at most `max_cols` display columns; width-aware like
// clip_ansi_line so the alt-screen row math holds for emoji/CJK lines.
fn wrap_ansi_line(s: &str, max_cols: usize) -> Vec<String> {
    if max_cols == 0 {
        return vec![String::new()];
    }
    if s.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut visible = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            current.push(c);
            eat_escape(&mut chars, Some(&mut current));
            continue;
        }
        let w = char_width(c);
        // `visible > 0` guard: a width-2 char on a 1-column terminal still
        // gets a row of its own instead of an infinite run of empty rows.
        if visible + w > max_cols && visible > 0 {
            out.push(std::mem::take(&mut current));
            visible = 0;
        }
        current.push(c);
        visible += w;
    }
    out.push(current);
    out
}

// Transcript index of the line currently receiving streamed text, or
// usize::MAX when no stream line is open. line() (and transcript clears)
// invalidate it so interleaved notices (trace records, hook lines) never get
// streamed text welded onto them.
static OPEN_STREAM_LINE: AtomicUsize = AtomicUsize::new(usize::MAX);

fn invalidate_stream_line() {
    OPEN_STREAM_LINE.store(usize::MAX, Ordering::Relaxed);
}

// Replace the open streamed line with its rendered form and close it. Falls
// back to appending a fresh line when no stream line is open (start of turn,
// or a notice landed mid-stream and invalidated the index).
fn commit_stream_line(rendered: &str) {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        line(rendered);
        return;
    }
    let mut replaced = false;
    if let Ok(mut t) = transcript().lock() {
        let open = OPEN_STREAM_LINE.load(Ordering::Relaxed);
        if open < t.len() {
            t.set(open, rendered.to_string());
            replaced = true;
        }
    }
    // Lock released above: render_output() re-locks the transcript.
    invalidate_stream_line();
    if replaced {
        render_output();
        clear_composer();
    } else {
        line(rendered);
    }
}

// Remove the open streamed line entirely (an echoed raw partial that turned
// out to be chrome — a code fence or protocol JSON — and must not be shown).
fn retract_stream_line() {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    if let Ok(mut t) = transcript().lock() {
        let open = OPEN_STREAM_LINE.load(Ordering::Relaxed);
        t.remove(open);
    }
    invalidate_stream_line();
    render_output();
    clear_composer();
}

pub fn line(s: &str) {
    if ALT_SCREEN.load(Ordering::Relaxed) {
        invalidate_stream_line();
        if let Ok(mut t) = transcript().lock() {
            for part in s.replace('\r', "").split('\n') {
                t.push(part.to_string());
            }
            const MAX_LINES: usize = 2_000;
            if t.len() > MAX_LINES {
                let extra = t.len() - MAX_LINES;
                t.drain_front(extra);
            }
        }
        render_output();
        clear_composer();
    } else if is_raw() {
        print!("{}\r\n", s.replace('\n', "\r\n"));
        flush();
    } else {
        println!("{s}");
    }
}

pub fn write_stream(chunk: &str) {
    if ALT_SCREEN.load(Ordering::Relaxed) {
        if let Ok(mut t) = transcript().lock() {
            let normalized = chunk.replace('\r', "");
            let mut parts = normalized.split('\n');
            if let Some(first) = parts.next() {
                // Append to the tracked open stream line only; if none is
                // open (start of stream, or a line() intervened) start fresh.
                let open = OPEN_STREAM_LINE.load(Ordering::Relaxed);
                if !t.append_to(open, first) {
                    t.push(first.to_string());
                    OPEN_STREAM_LINE.store(t.len() - 1, Ordering::Relaxed);
                }
            }
            for part in parts {
                t.push(part.to_string());
                OPEN_STREAM_LINE.store(t.len() - 1, Ordering::Relaxed);
            }
            const MAX_LINES: usize = 2_000;
            if t.len() > MAX_LINES {
                let extra = t.len() - MAX_LINES;
                t.drain_front(extra);
                // Keep the open-line index in step with the drained prefix.
                let open = OPEN_STREAM_LINE.load(Ordering::Relaxed);
                if open != usize::MAX {
                    if open >= extra {
                        OPEN_STREAM_LINE.store(open - extra, Ordering::Relaxed);
                    } else {
                        invalidate_stream_line();
                    }
                }
            }
        }
        // Streaming repaints are frame-coalesced (~60fps): the transcript
        // state above is always current, so a skipped frame just means the
        // next one paints more at once. Unthrottled paths (line, commit)
        // guarantee the final state always lands on screen.
        render_output_throttled();
    } else if is_raw() && chunk.contains('\n') {
        print!("{}", chunk.replace('\n', "\r\n"));
    } else {
        print!("{chunk}");
    }
    flush();
}

pub fn flush() {
    let _ = io::stdout().flush();
}

pub fn bell() {
    if std::io::stdout().is_terminal() {
        print!("\x07");
        flush();
    }
}

// Non-blocking: drain pending key events and report whether Ctrl-C was pressed.
pub fn interrupted() -> bool {
    if !is_raw() {
        return false;
    }
    // poll_typeahead() may have consumed a Ctrl+C and set this flag.
    if TYPEAHEAD_INTERRUPTED.swap(false, Ordering::Relaxed) {
        return true;
    }
    let mut hit = false;
    while poll(Duration::ZERO).unwrap_or(false) {
        if let Ok(Event::Key(k)) = read() {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            let ctrl_c = k.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('C'));
            if ctrl_c || k.code == KeyCode::Esc {
                hit = true;
            }
        }
    }
    hit
}

// ── input event ──────────────────────────────────────────────────────────────
// Returned from ask_task so the REPL can distinguish a submitted line from a
// mode-cycle request (Shift+Tab) without passing mutable mode state into tui.
pub enum InputEvent {
    Text(String),
    CycleMode,
}

// ── single-line ask ──────────────────────────────────────────────────────────
pub fn ask(prompt: &str) -> Option<String> {
    if is_raw() {
        match read_line_raw(prompt) {
            None => None,
            Some(RawLine::Submit(s, _)) => Some(s),
            Some(RawLine::CycleMode) => None, // shouldn't cycle mode inside a y/n prompt
        }
    } else {
        print!("{prompt}");
        flush();
        let mut buf = String::new();
        let n = io::stdin().lock().read_line(&mut buf).unwrap_or(0);
        if n == 0 {
            return None;
        }
        Some(buf.trim_end_matches(['\n', '\r']).to_string())
    }
}

// Multi-line task input. A trailing `\` + Enter adds another line; plain Enter
// submits. Shift+Tab returns CycleMode without submitting.
// Pre-fills the first line with any keystrokes typed during agent processing.
pub fn ask_task(prompt: &str) -> Option<InputEvent> {
    // Take the message out inside a tight block: echo_submitted → line →
    // render_output re-locks the queue, and std Mutex is not reentrant.
    let queued = message_queue()
        .lock()
        .ok()
        .and_then(|mut mq| (!mq.is_empty()).then(|| mq.remove(0)));
    if let Some(msg) = queued {
        push_history(&msg);
        echo_submitted(prompt, &msg);
        return Some(InputEvent::Text(msg));
    }
    if !is_raw() {
        return ask(prompt).map(InputEvent::Text);
    }
    let (prefill, prefill_cur) = take_typeahead();
    let mut acc = String::new();
    let mut p = prompt.to_string();
    // Use the typeahead buffer to pre-fill only the very first read.
    let mut first = Some((prefill, prefill_cur));
    loop {
        let rl = if let Some((pf, pc)) = first.take() {
            read_line_raw_prefill(&p, pf, pc)
        } else {
            read_line_raw(&p)
        };
        match rl? {
            RawLine::CycleMode => return Some(InputEvent::CycleMode),
            RawLine::Submit(text, cont) => {
                if acc.is_empty() {
                    acc = text;
                } else {
                    acc.push('\n');
                    acc.push_str(&text);
                }
                if !cont {
                    push_history(&acc);
                    return Some(InputEvent::Text(acc));
                }
                p = format!("{} ", dim("…"));
            }
        }
    }
}

fn push_history(s: &str) {
    if s.trim().is_empty() {
        return;
    }
    if let Ok(mut h) = history().lock() {
        if h.last().map(String::as_str) != Some(s) {
            h.push(s.to_string());
            crate::config::save_history(&h);
        }
    }
}

fn history() -> &'static std::sync::Mutex<Vec<String>> {
    static H: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> = std::sync::OnceLock::new();
    H.get_or_init(|| std::sync::Mutex::new(crate::config::load_history()))
}

// ── raw-mode editor internals ────────────────────────────────────────────────
enum RawLine {
    Submit(String, bool), // text, continue (multiline)?
    CycleMode,
}

fn viewport(cursor: usize, avail: usize, scroll: usize) -> (usize, usize) {
    let avail = avail.max(1);
    let mut s = scroll;
    if cursor < s {
        s = cursor;
    } else if cursor >= s + avail {
        s = cursor + 1 - avail;
    }
    (s, cursor - s)
}

fn redraw(prompt: &str, start: (u16, u16), buf: &[char], cursor: usize, scroll: &mut usize) {
    if ALT_SCREEN.load(Ordering::Relaxed) {
        render_composer(prompt, buf, cursor, scroll);
        return;
    }
    let width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
    let avail = width.saturating_sub(start.0).max(8) as usize;
    let (s, _col) = viewport(cursor, avail, *scroll);
    *scroll = s;
    let end = (s + avail).min(buf.len());
    let shown: String = buf[s..end].iter().collect();
    let col_width = buf[s..cursor.min(buf.len())]
        .iter()
        .copied()
        .map(char_width)
        .sum::<usize>();
    let mut out = io::stdout();
    let _ = queue!(
        out,
        MoveTo(start.0, start.1),
        Clear(ClearType::UntilNewLine)
    );
    let _ = write!(out, "{shown}");
    let _ = queue!(
        out,
        MoveTo(start.0.saturating_add(col_width as u16), start.1)
    );
    let _ = out.flush();
}

fn prev_word(buf: &[char], mut i: usize) -> usize {
    while i > 0 && buf[i - 1].is_whitespace() {
        i -= 1;
    }
    while i > 0 && !buf[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}
fn next_word(buf: &[char], mut i: usize) -> usize {
    let n = buf.len();
    while i < n && buf[i].is_whitespace() {
        i += 1;
    }
    while i < n && !buf[i].is_whitespace() {
        i += 1;
    }
    i
}

fn end_word(buf: &[char], mut i: usize) -> usize {
    let n = buf.len();
    if i < n && !buf[i].is_whitespace() {
        i += 1;
    }
    while i < n && buf[i].is_whitespace() {
        i += 1;
    }
    while i + 1 < n && !buf[i + 1].is_whitespace() {
        i += 1;
    }
    i.min(n)
}

fn edit_in_editor(current: &str) -> Option<String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let path = std::env::temp_dir().join(format!("bwn-prompt-{}.txt", std::process::id()));
    std::fs::write(&path, current).ok()?;
    let was_raw = is_raw();
    if was_raw {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
    }
    let mut parts = editor.split_whitespace();
    let cmd = parts.next().unwrap_or("vi");
    let _ = std::process::Command::new(cmd)
        .args(parts)
        .arg(&path)
        .status();
    if was_raw {
        let _ = enable_raw_mode();
        let _ = execute!(io::stdout(), EnableBracketedPaste);
    }
    let content = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    content.map(|c| c.trim_end_matches(['\n', '\r']).to_string())
}

// ── Tab completion ───────────────────────────────────────────────────────────
// Slash commands the REPL handles directly. Kept in sync with the match in lib.rs.
const SLASH_COMMANDS_BASE: &[&str] = &[
    "/help",
    "/clear",
    "/new",
    "/resume",
    "/init",
    "/plan",
    "/build",
    "/brainstorm",
    "/doctor",
    "/debug",
    "/mode",
    "/model",
    "/permissions",
    "/mcp",
    "/scroll",
    "/mouse",
    "/compact",
    "/review",
    "/commit",
    "/pr",
    "/diff",
    "/context",
    "/schedule",
    "/loop",
    "/workflows",
    "/tasks",
    "/btw",
    "/config",
    "/memory",
    "/skills",
    "/tools",
    "/trace",
    "/agents",
    "/checkpoints",
    "/undo",
    "/rewind",
    "/vim",
    "/voice",
    "/local",
    "/rules",
    "/kb",
    "/index",
    "/verify",
    "/audit",
    "/grill-me",
    "/teamwork",
    "/exit",
    "/quit",
];

// Cached: the autocomplete popup consults this on every keystroke, and the
// skill/command set doesn't change within a session.
fn load_slash_commands() -> Vec<String> {
    static CMDS: OnceLock<Vec<String>> = OnceLock::new();
    CMDS.get_or_init(load_slash_commands_uncached).clone()
}

fn load_slash_commands_uncached() -> Vec<String> {
    let mut cmds: Vec<String> = SLASH_COMMANDS_BASE.iter().map(|s| s.to_string()).collect();
    for (name, _) in crate::config::bundled_skills() {
        let cmd = format!("/{name}");
        if !cmds.contains(&cmd) {
            cmds.push(cmd);
        }
    }
    // Merge user-defined commands from ~/.buildwithnexus/commands/
    if let Ok(rd) = std::fs::read_dir(crate::config::home().join("commands")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let stem = name
                .trim_end_matches(".md")
                .trim_end_matches(".sh")
                .trim_end_matches(".py");
            let cmd = format!("/{stem}");
            if !cmds.contains(&cmd) {
                cmds.push(cmd);
            }
        }
    }
    cmds
}

fn token_at(buf: &[char], cursor: usize) -> (usize, String) {
    let mut start = cursor;
    while start > 0 && !buf[start - 1].is_whitespace() {
        start -= 1;
    }
    (start, buf[start..cursor].iter().collect())
}

fn common_prefix(items: &[String]) -> String {
    let mut iter = items.iter();
    let mut prefix: Vec<char> = match iter.next() {
        Some(s) => s.chars().collect(),
        None => return String::new(),
    };
    for s in iter {
        let sc: Vec<char> = s.chars().collect();
        let n = prefix
            .iter()
            .zip(sc.iter())
            .take_while(|(a, b)| a == b)
            .count();
        prefix.truncate(n);
    }
    prefix.into_iter().collect()
}

// ── live autocomplete popup ──────────────────────────────────────────────────
// As-you-type suggestions drawn just above the composer (alt-screen only):
// slash commands with one-line descriptions, sub-arguments, and @path
// mentions. ↑/↓ move the highlight, Tab/Enter accept, Esc dismisses.

const POPUP_MAX_ROWS: usize = 8;

// One-line description shown next to each built-in command in the popup.
// User-defined commands and skills get an empty description.
fn slash_command_desc(cmd: &str) -> &'static str {
    match cmd {
        "/help" => "show all commands and keys",
        "/clear" => "clear the screen",
        "/new" => "start a fresh session",
        "/resume" => "pick a saved session to resume",
        "/init" => "reconfigure provider, model, and key",
        "/plan" => "switch to PLAN mode",
        "/build" => "switch to BUILD mode",
        "/brainstorm" => "switch to BRAINSTORM mode",
        "/doctor" | "/debug" => "diagnose setup and connectivity",
        "/mode" => "show or switch mode",
        "/model" => "hot-swap the AI model",
        "/permissions" => "tool permission level (ask/auto/readonly)",
        "/mcp" => "inspect configured MCP servers",
        "/scroll" => "wheel scrolling on/off",
        "/mouse" => "mouse capture on/off",
        "/compact" => "compress context to free token budget",
        "/review" => "AI code review of staged git diff",
        "/commit" => "AI-drafted conventional commit message",
        "/pr" => "AI-drafted PR title + description",
        "/diff" => "show current git diff summary",
        "/context" => "show context window usage",
        "/schedule" => "one-shot scheduled workflow",
        "/loop" => "repeating scheduled workflow",
        "/workflows" => "list and manage background workflows",
        "/tasks" => "list and manage background tasks",
        "/btw" => "inject context into the next agent turn",
        "/config" => "configure hooks, memory, commands via AI",
        "/memory" => "view and edit session memory",
        "/skills" => "list skills and custom commands",
        "/tools" => "browse callable tools",
        "/trace" => "inspect hooks, tools, skills, subagents",
        "/agents" => "manage subagents",
        "/checkpoints" => "list edit checkpoints",
        "/undo" | "/rewind" => "revert to an edit checkpoint",
        "/vim" => "toggle vim editing mode",
        "/voice" => "voice input",
        "/local" => "manage local models",
        "/rules" => "manage project rules",
        "/kb" | "/index" => "query or index project knowledge base",
        "/verify" | "/audit" => "verify recent changes",
        "/grill-me" => "operational alignment interview",
        "/teamwork" => "multi-agent swarm preview",
        "/exit" | "/quit" => "exit the session",
        _ => "",
    }
}

// Candidates for the popup at the current cursor position. Only "interesting"
// tokens trigger it (slash commands, their sub-arguments, and @mentions) so
// ordinary prose never spawns a popup.
fn popup_candidates(buf: &[char], cursor: usize) -> Vec<String> {
    let (tok_start, token) = token_at(buf, cursor);
    if token.is_empty() {
        return Vec::new();
    }
    let interesting =
        token.starts_with('/') || token.starts_with('@') || buf.first() == Some(&'/');
    if !interesting {
        return Vec::new();
    }
    completions(buf, tok_start, &token)
}

// Scroll window over the candidate list: (first_index, rows_shown), keeping
// the selection visible.
fn popup_window(sel: usize, len: usize, max: usize) -> (usize, usize) {
    let show = len.min(max);
    if show == 0 {
        return (0, 0);
    }
    let first = sel.saturating_sub(show - 1).min(len - show);
    (first, show)
}

// Draw the popup over the bottom rows of the output region. Cursor position
// is saved/restored so the composer caret stays put; the caller repaints the
// transcript (render_output) once the popup shrinks or closes.
fn render_suggestions(sug: &[String], sel: usize) {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    let (width, _) = term_size();
    let (first, show) = popup_window(sel, sug.len(), POPUP_MAX_ROWS);
    if show == 0 {
        return;
    }
    let pad = sug[first..first + show]
        .iter()
        .map(|c| str_width(c))
        .max()
        .unwrap_or(0);
    let mut out = io::stdout();
    let _ = execute!(out, SavePosition);
    // Sit just above the composer box's top border.
    let base = composer_top().saturating_sub(show as u16);
    for i in 0..show {
        let idx = first + i;
        let row = base + i as u16;
        let _ = queue!(out, MoveTo(0, row), Clear(ClearType::CurrentLine));
        let cand = &sug[idx];
        let padded = format!("{cand:<pad$}");
        let desc = slash_command_desc(cand);
        let counter = if sug.len() > show && idx == sel {
            dim(&format!(" ({}/{})", sel + 1, sug.len()))
        } else {
            String::new()
        };
        let entry = if idx == sel {
            format!("  {} {}  {}{}", accent("›"), bold(&padded), dim(desc), counter)
        } else {
            format!("    {padded}  {}", dim(desc))
        };
        let _ = write!(out, "{}", clip_ansi_line(&entry, width as usize));
    }
    let _ = execute!(out, RestorePosition);
    let _ = out.flush();
}

fn path_candidates(partial: &str, cwd: &std::path::Path) -> Vec<String> {
    if let Some(query) = partial.strip_prefix("kb:") {
        let kb = crate::knowledge::KnowledgeBase::new(&cwd.to_string_lossy());
        let mut out = Vec::new();
        for (id, entity) in &kb.entities {
            if id.to_lowercase().contains(&query.to_lowercase())
                || entity.name.to_lowercase().contains(&query.to_lowercase())
            {
                out.push(format!("kb:{id}"));
            }
        }
        out.sort();
        return out;
    }
    if let Some(query) = partial.strip_prefix("symbol:") {
        let kb = crate::knowledge::KnowledgeBase::new(&cwd.to_string_lossy());
        let mut out = Vec::new();
        for (id, entity) in &kb.entities {
            if matches!(
                entity.entity_type,
                crate::knowledge::EntityType::Function
                    | crate::knowledge::EntityType::Class
                    | crate::knowledge::EntityType::Interface
                    | crate::knowledge::EntityType::Module
            ) && (id.to_lowercase().contains(&query.to_lowercase())
                || entity.name.to_lowercase().contains(&query.to_lowercase()))
            {
                out.push(format!("symbol:{id}"));
            }
        }
        out.sort();
        return out;
    }
    if let Some(query) = partial.strip_prefix("rules:") {
        let mut engine = crate::rules::RuleEngine::load_defaults();
        let rules_dir = cwd.join(".buildwithnexus").join("rules");
        if let Ok(rd) = std::fs::read_dir(&rules_dir) {
            for e in rd.flatten() {
                if let Ok(loaded) =
                    crate::rules::RuleEngine::load_from_file(&e.path().to_string_lossy())
                {
                    for r in loaded.rules {
                        engine.add_rule(r);
                    }
                }
            }
        }
        let mut out = Vec::new();
        for rule in &engine.rules {
            if rule.id.to_lowercase().contains(&query.to_lowercase())
                || rule
                    .description
                    .to_lowercase()
                    .contains(&query.to_lowercase())
            {
                out.push(format!("rules:{}", rule.id));
            }
        }
        out.sort();
        return out;
    }
    let (base, dir, prefix) = match partial.rfind('/') {
        Some(i) => (&partial[..=i], cwd.join(&partial[..=i]), &partial[i + 1..]),
        None => ("", cwd.to_path_buf(), partial),
    };
    let mut out = Vec::new();
    if base.is_empty() {
        for special in [
            "diff", "status", "rules", "rules:", "kb:", "symbol:", "url:", "web:",
        ] {
            if special.starts_with(prefix) {
                out.push(special.to_string());
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(prefix) && !name.starts_with('.') {
                let mut full = format!("{base}{name}");
                if e.path().is_dir() {
                    full.push('/');
                }
                out.push(full);
            }
        }
    }
    out.sort();
    out
}

// ↑/↓ history recall: nearest entry matching `prefix` (fish/zsh style —
// type "car", ↑ cycles only "car…" entries). `from=None` starts at the ends.
fn hist_match(h: &[String], prefix: &str, from: Option<usize>, back: bool) -> Option<usize> {
    let hit = |i: &usize| prefix.is_empty() || h[*i].starts_with(prefix);
    if back {
        (0..from.unwrap_or(h.len())).rev().find(hit)
    } else {
        (from.map(|i| i + 1).unwrap_or(h.len())..h.len()).find(hit)
    }
}

fn history_search(hist: &[String], query: &str, skip: usize) -> Option<String> {
    if query.is_empty() {
        return None;
    }
    hist.iter()
        .rev()
        .filter(|e| e.contains(query))
        .nth(skip)
        .cloned()
}

fn completions(buf: &[char], start: usize, token: &str) -> Vec<String> {
    let at_line_start = buf[..start].iter().all(|c| c.is_whitespace());
    if at_line_start && token.starts_with('/') {
        let cmds = load_slash_commands();
        return cmds.into_iter().filter(|c| c.starts_with(token)).collect();
    }
    // Sub-argument completion: look at the command that precedes the current token.
    let prefix: String = buf[..start].iter().collect();
    match prefix.trim() {
        "/mode" => {
            return ["plan", "build", "brainstorm"]
                .iter()
                .filter(|&&s| s.starts_with(token))
                .map(|s| s.to_string())
                .collect();
        }
        "/permissions" => {
            return ["ask", "auto", "readonly"]
                .iter()
                .filter(|&&s| s.starts_with(token))
                .map(|s| s.to_string())
                .collect();
        }
        "/scroll" | "/mouse" => {
            return ["on", "off", "status"]
                .iter()
                .filter(|&&s| s.starts_with(token))
                .map(|s| s.to_string())
                .collect();
        }
        _ => {}
    }
    if let Some(partial) = token.strip_prefix('@') {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        return path_candidates(partial, &cwd)
            .into_iter()
            .map(|p| format!("@{p}"))
            .collect();
    }
    Vec::new()
}

fn read_line_raw(prompt: &str) -> Option<RawLine> {
    read_line_raw_prefill(prompt, vec![], 0)
}

fn read_line_raw_prefill(prompt: &str, prefill: Vec<char>, prefill_cur: usize) -> Option<RawLine> {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        print!("{prompt}");
        flush();
    }
    let mut start = if ALT_SCREEN.load(Ordering::Relaxed) {
        (prompt_width(prompt) + COMPOSER_PAD, composer_row())
    } else {
        crossterm::cursor::position().unwrap_or((0, 0))
    };
    let mut buf: Vec<char> = prefill;
    let mut cursor = prefill_cur.min(buf.len());
    let mut scroll = 0usize;
    redraw(prompt, start, &buf, cursor, &mut scroll);
    let mut hist_idx: Option<usize> = None;
    // ↑ stashes the in-progress draft (and its prefix filter); ↓ past the
    // newest matching entry restores the draft instead of clearing the line.
    let mut hist_draft: Vec<char> = Vec::new();
    let mut hist_prefix = String::new();
    let mut kill = String::new();
    let mut vim_state = if is_vim_mode() {
        VimState::Normal
    } else {
        VimState::Insert
    };
    let mut vim_undo_stack: Vec<Vec<char>> = vec![buf.clone()];
    if is_vim_mode() {
        VIM_STATE_VAL.store(0, Ordering::Relaxed);
    }
    // Cursor: visible again (the working spinner hides it), shaped for the
    // editing mode — accent bar for insert, block for vim NORMAL.
    cursor_show();
    let mut cur_shape = if is_vim_mode() && vim_state == VimState::Normal {
        CursorShape::Block
    } else {
        CursorShape::Bar
    };
    set_cursor_shape(cur_shape);
    // Live autocomplete popup state (alt-screen only).
    let mut sug: Vec<String> = Vec::new();
    let mut sug_idx = 0usize;
    let mut sug_rows = 0usize; // rows currently drawn over the output region
    let mut sug_suppressed = false; // Esc hides the popup until the buffer changes
    let mut sug_dismissed_at: Vec<char> = Vec::new();

    macro_rules! reline {
        () => {{
            if ALT_SCREEN.load(Ordering::Relaxed) {
                start = (prompt_width(prompt) + COMPOSER_PAD, composer_row());
            } else {
                print!("\r{prompt}");
                flush();
                start = crossterm::cursor::position().unwrap_or(start);
            }
            redraw(prompt, start, &buf, cursor, &mut scroll);
        }};
    }

    loop {
        // Refresh the autocomplete popup against the current buffer. Runs
        // before the blocking read so the popup tracks every edit (including
        // prefilled text on the first pass).
        if ALT_SCREEN.load(Ordering::Relaxed) {
            if sug_suppressed && buf != sug_dismissed_at {
                sug_suppressed = false;
            }
            let cands = if sug_suppressed {
                Vec::new()
            } else {
                popup_candidates(&buf, cursor)
            };
            if cands != sug {
                sug = cands;
                sug_idx = 0;
            }
            let show = sug.len().min(POPUP_MAX_ROWS);
            if show < sug_rows {
                // Popup shrank or closed: repaint the transcript rows it
                // covered, then restore the composer it clobbers.
                render_output();
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            if !sug.is_empty() {
                render_suggestions(&sug, sug_idx);
            }
            sug_rows = show;
        }
        let ev = match read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => k,
            Ok(Event::Paste(s)) => {
                for raw in s.chars() {
                    let c = match raw {
                        '\n' | '\r' | '\t' => ' ',
                        c if c.is_control() => continue,
                        c => c,
                    };
                    buf.insert(cursor, c);
                    cursor += 1;
                }
                redraw(prompt, start, &buf, cursor, &mut scroll);
                continue;
            }
            Ok(Event::Mouse(m)) => {
                match m.kind {
                    MouseEventKind::ScrollUp => {
                        scroll_output(3);
                        redraw(prompt, start, &buf, cursor, &mut scroll);
                        continue;
                    }
                    MouseEventKind::ScrollDown => {
                        scroll_output(-3);
                        redraw(prompt, start, &buf, cursor, &mut scroll);
                        continue;
                    }
                    MouseEventKind::Down(MouseButton::Left) if in_output_region(m.row) => {
                        selection_start(m.row, m.column);
                        redraw(prompt, start, &buf, cursor, &mut scroll);
                        continue;
                    }
                    MouseEventKind::Drag(MouseButton::Left) if in_output_region(m.row) => {
                        selection_drag(m.row, m.column);
                        redraw(prompt, start, &buf, cursor, &mut scroll);
                        continue;
                    }
                    MouseEventKind::Up(MouseButton::Left) if in_output_region(m.row) => {
                        selection_finish(m.row, m.column);
                        redraw(prompt, start, &buf, cursor, &mut scroll);
                        continue;
                    }
                    _ => {}
                }
                // Left-click or drag on the input row moves the cursor to the clicked/dragged column.
                if (m.kind == MouseEventKind::Down(MouseButton::Left)
                    || m.kind == MouseEventKind::Drag(MouseButton::Left))
                    && m.row == start.1
                {
                    let col = m.column as usize;
                    if col >= start.0 as usize {
                        let offset = col - start.0 as usize + scroll;
                        cursor = offset.min(buf.len());
                        redraw(prompt, start, &buf, cursor, &mut scroll);
                    }
                }
                continue;
            }
            Ok(Event::Resize(_, _)) => {
                if ALT_SCREEN.load(Ordering::Relaxed) {
                    set_output_region();
                    render_output();
                    clear_composer();
                    start = (prompt_width(prompt) + COMPOSER_PAD, composer_row());
                    scroll = 0;
                }
                redraw(prompt, start, &buf, cursor, &mut scroll);
                continue;
            }
            Ok(_) => continue,
            Err(_) => return None,
        };
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let alt = ev.modifiers.contains(KeyModifiers::ALT);
        match ev.code {
            KeyCode::PageUp => {
                scroll_page_up();
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::PageDown => {
                scroll_page_down();
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Up if alt => {
                scroll_output(1);
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Down if alt => {
                scroll_output(-1);
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Home if alt => {
                scroll_output(isize::MAX / 4);
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::End if alt => {
                scroll_to_bottom();
                clear_composer();
                render_footer();
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            // Shift+Tab → cycle mode (clear the line and signal the REPL).
            KeyCode::BackTab => {
                buf.clear();
                clear_composer();
                flush();
                return Some(RawLine::CycleMode);
            }
            KeyCode::Tab if ev.modifiers.contains(KeyModifiers::SHIFT) => {
                buf.clear();
                clear_composer();
                flush();
                return Some(RawLine::CycleMode);
            }
            KeyCode::Char('c') if ctrl => {
                if buf.is_empty() {
                    clear_composer();
                    flush();
                    return None;
                }
                buf.clear();
                cursor = 0;
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('d') if ctrl => {
                if buf.is_empty() {
                    clear_composer();
                    flush();
                    return None;
                }
            }
            KeyCode::Char('a') if ctrl => {
                cursor = 0;
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('e') if ctrl => {
                cursor = buf.len();
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('u') if ctrl => {
                kill = buf[..cursor].iter().collect();
                buf.drain(..cursor);
                cursor = 0;
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('k') if ctrl => {
                kill = buf[cursor..].iter().collect();
                buf.truncate(cursor);
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('w') if ctrl => {
                let i = prev_word(&buf, cursor);
                kill = buf[i..cursor].iter().collect();
                buf.drain(i..cursor);
                cursor = i;
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('y') if ctrl => {
                for c in kill.chars() {
                    buf.insert(cursor, c);
                    cursor += 1;
                }
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('v') if ctrl => {
                // Paste from the system clipboard: an image lands as a temp
                // png and inserts an @path attachment token; plain text
                // inserts at the cursor (for terminals that pass Ctrl+V
                // through instead of translating it to a Paste event).
                if let Some(img) = crate::media::clipboard_image_to_temp() {
                    line(&dim(&format!("  ⎘ clipboard image → {}", img.display())));
                    for ch in format!("@{} ", img.display()).chars() {
                        buf.insert(cursor, ch);
                        cursor += 1;
                    }
                } else if let Some(text) = crate::media::clipboard_text() {
                    for raw in text.chars() {
                        let ch = match raw {
                            '\n' | '\r' | '\t' => ' ',
                            c if c.is_control() => continue,
                            c => c,
                        };
                        buf.insert(cursor, ch);
                        cursor += 1;
                    }
                }
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('l') if ctrl => reline!(),
            KeyCode::Char('g') if ctrl => {
                let cur: String = buf.iter().collect();
                if let Some(edited) = edit_in_editor(&cur) {
                    buf = edited.replace('\n', " ").chars().collect();
                    cursor = buf.len();
                }
                reline!();
            }
            KeyCode::Char('r') if ctrl => {
                let snapshot = (buf.clone(), cursor);
                let mut query = String::new();
                let mut skip = 0usize;
                loop {
                    let m = {
                        let h = history().lock();
                        h.ok().and_then(|h| history_search(&h, &query, skip))
                    };
                    {
                        let mut out = io::stdout();
                        if ALT_SCREEN.load(Ordering::Relaxed) {
                            queue_composer_box(&mut out);
                            let _ = write!(
                                out,
                                "{}{}",
                                dim(&format!("(reverse-i-search)`{query}`: ")),
                                m.as_deref().unwrap_or("")
                            );
                            queue_composer_right_border(&mut out);
                        } else {
                            let _ =
                                queue!(out, MoveTo(0, start.1), Clear(ClearType::UntilNewLine));
                            let _ = write!(
                                out,
                                "{}{}",
                                dim(&format!("(reverse-i-search)`{query}`: ")),
                                m.as_deref().unwrap_or("")
                            );
                        }
                        let _ = out.flush();
                    }
                    let ev = match read() {
                        Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => k,
                        Ok(_) => continue,
                        Err(_) => {
                            buf = snapshot.0;
                            cursor = snapshot.1;
                            break;
                        }
                    };
                    let c = ev.modifiers.contains(KeyModifiers::CONTROL);
                    match ev.code {
                        KeyCode::Char('r') if c => {
                            if m.is_some() {
                                skip += 1;
                            }
                        }
                        KeyCode::Char('c') | KeyCode::Char('g') if c => {
                            buf = snapshot.0;
                            cursor = snapshot.1;
                            break;
                        }
                        KeyCode::Char(ch) if !c => {
                            query.push(ch);
                            skip = 0;
                        }
                        KeyCode::Backspace => {
                            query.pop();
                            skip = 0;
                        }
                        KeyCode::Enter => {
                            if let Some(e) = m {
                                print!("\r\n");
                                flush();
                                return Some(RawLine::Submit(e, false));
                            }
                            buf = snapshot.0;
                            cursor = snapshot.1;
                            break;
                        }
                        KeyCode::Esc | KeyCode::Tab => {
                            match m {
                                Some(e) => {
                                    buf = e.chars().collect();
                                    cursor = buf.len();
                                }
                                None => {
                                    buf = snapshot.0;
                                    cursor = snapshot.1;
                                }
                            }
                            break;
                        }
                        _ => {}
                    }
                }
                reline!();
            }
            KeyCode::Char('b') if alt => {
                cursor = prev_word(&buf, cursor);
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('f') if alt => {
                cursor = next_word(&buf, cursor);
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                if is_vim_mode() && vim_state == VimState::Normal {
                    match c {
                        'i' => vim_state = VimState::Insert,
                        'a' => {
                            if cursor < buf.len() {
                                cursor += 1;
                            }
                            vim_state = VimState::Insert;
                        }
                        'I' => {
                            cursor = 0;
                            vim_state = VimState::Insert;
                        }
                        'A' => {
                            cursor = buf.len();
                            vim_state = VimState::Insert;
                        }
                        'h' => cursor = cursor.saturating_sub(1),
                        'l' => {
                            if cursor < buf.len() {
                                cursor += 1;
                            }
                        }
                        '0' | '^' => cursor = 0,
                        '$' => cursor = buf.len(),
                        'w' => cursor = next_word(&buf, cursor),
                        'b' => cursor = prev_word(&buf, cursor),
                        'e' => cursor = end_word(&buf, cursor),
                        's' => {
                            if cursor < buf.len() {
                                kill = buf[cursor..=cursor].iter().collect();
                                buf.remove(cursor);
                            }
                            vim_state = VimState::Insert;
                        }
                        'o' => {
                            buf.push('\n');
                            cursor = buf.len();
                            vim_state = VimState::Insert;
                        }
                        'O' => {
                            buf.insert(0, '\n');
                            cursor = 0;
                            vim_state = VimState::Insert;
                        }
                        'x' => {
                            if cursor < buf.len() {
                                kill = buf[cursor..=cursor].iter().collect();
                                buf.remove(cursor);
                                if !kill.is_empty() {
                                    osc52_copy(&kill);
                                }
                            }
                        }
                        'D' => {
                            kill = buf[cursor..].iter().collect();
                            buf.truncate(cursor);
                            if !kill.is_empty() {
                                osc52_copy(&kill);
                            }
                        }
                        'C' => {
                            kill = buf[cursor..].iter().collect();
                            buf.truncate(cursor);
                            if !kill.is_empty() {
                                osc52_copy(&kill);
                            }
                            vim_state = VimState::Insert;
                        }
                        'p' => {
                            for ch in kill.chars() {
                                if cursor < buf.len() {
                                    buf.insert(cursor + 1, ch);
                                    cursor += 1;
                                } else {
                                    buf.push(ch);
                                    cursor = buf.len();
                                }
                            }
                        }
                        'u' => {
                            if let Some(prev) = vim_undo_stack.pop() {
                                buf = prev;
                                cursor = cursor.min(buf.len());
                            }
                        }
                        ':' => {
                            buf.clear();
                            buf.push('/');
                            cursor = 1;
                            vim_state = VimState::Insert;
                        }
                        'v' => vim_state = VimState::Visual(cursor),
                        _ => {}
                    }
                } else if is_vim_mode() && matches!(vim_state, VimState::Visual(_)) {
                    let VimState::Visual(start_idx) = vim_state else {
                        unreachable!()
                    };
                    match c {
                        'h' => cursor = cursor.saturating_sub(1),
                        'l' => {
                            if cursor < buf.len() {
                                cursor += 1;
                            }
                        }
                        'w' => cursor = next_word(&buf, cursor),
                        'b' => cursor = prev_word(&buf, cursor),
                        'e' => cursor = end_word(&buf, cursor),
                        '0' | '^' => cursor = 0,
                        '$' => cursor = buf.len(),
                        'd' | 'x' => {
                            let min_i = start_idx.min(cursor);
                            let max_i = start_idx.max(cursor).min(buf.len().saturating_sub(1));
                            if min_i <= max_i && max_i < buf.len() {
                                kill = buf[min_i..=max_i].iter().collect();
                                buf.drain(min_i..=max_i);
                                cursor = min_i.min(buf.len());
                                if !kill.is_empty() {
                                    osc52_copy(&kill);
                                }
                            }
                            vim_state = VimState::Normal;
                        }
                        'y' => {
                            let min_i = start_idx.min(cursor);
                            let max_i = start_idx.max(cursor).min(buf.len().saturating_sub(1));
                            if min_i <= max_i && max_i < buf.len() {
                                kill = buf[min_i..=max_i].iter().collect();
                                if !kill.is_empty() {
                                    osc52_copy(&kill);
                                }
                            }
                            vim_state = VimState::Normal;
                        }
                        _ => {}
                    }
                } else {
                    if is_vim_mode() {
                        vim_undo_stack.push(buf.clone());
                        if vim_undo_stack.len() > 50 {
                            vim_undo_stack.remove(0);
                        }
                    }
                    buf.insert(cursor, c);
                    cursor += 1;
                }
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Backspace => {
                if cursor > 0 {
                    if is_vim_mode() && vim_state == VimState::Normal {
                        cursor -= 1;
                    } else {
                        buf.remove(cursor - 1);
                        cursor -= 1;
                    }
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                }
            }
            KeyCode::Delete => {
                if cursor < buf.len() {
                    buf.remove(cursor);
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                }
            }
            // Ctrl/Alt + arrows jump by word (matching Alt+B/F).
            KeyCode::Left if ctrl || alt => {
                cursor = prev_word(&buf, cursor);
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Right if ctrl || alt => {
                cursor = next_word(&buf, cursor);
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Left => {
                cursor = cursor.saturating_sub(1);
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Right => {
                if cursor < buf.len() {
                    cursor += 1;
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                }
            }
            KeyCode::Home => {
                cursor = 0;
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::End => {
                cursor = buf.len();
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Up => {
                if !sug.is_empty() {
                    sug_idx = if sug_idx == 0 {
                        sug.len() - 1
                    } else {
                        sug_idx - 1
                    };
                    continue; // loop top re-renders the popup
                }
                if let Ok(h) = history().lock() {
                    if !h.is_empty() {
                        if hist_idx.is_none() {
                            // First ↑: stash the draft and filter recall by it.
                            hist_draft = buf.clone();
                            hist_prefix = buf.iter().collect();
                        }
                        if let Some(idx) = hist_match(&h, &hist_prefix, hist_idx, true) {
                            hist_idx = Some(idx);
                            buf = h[idx].chars().collect();
                            cursor = buf.len();
                        }
                    }
                }
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Down => {
                if !sug.is_empty() {
                    sug_idx = (sug_idx + 1) % sug.len();
                    continue; // loop top re-renders the popup
                }
                if let Ok(h) = history().lock() {
                    if let Some(cur) = hist_idx {
                        match hist_match(&h, &hist_prefix, Some(cur), false) {
                            Some(idx) => {
                                hist_idx = Some(idx);
                                buf = h[idx].chars().collect();
                                cursor = buf.len();
                            }
                            None => {
                                // Past the newest entry: the draft comes back
                                // instead of a destroyed line.
                                hist_idx = None;
                                buf = hist_draft.clone();
                                cursor = buf.len();
                            }
                        }
                    }
                }
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Enter => {
                // Accept the highlighted autocomplete entry first: a
                // line-start /command submits immediately; any other token
                // (sub-argument, @path) is inserted and editing continues.
                if !sug.is_empty() {
                    let cand = sug[sug_idx].clone();
                    let (tok_start, token) = token_at(&buf, cursor);
                    let is_cmd = token.starts_with('/')
                        && buf[..tok_start].iter().all(|c| c.is_whitespace());
                    let new: Vec<char> = cand.chars().collect();
                    buf.splice(tok_start..cursor, new.iter().copied());
                    cursor = tok_start + new.len();
                    if !is_cmd {
                        if !cand.ends_with('/') && !cand.ends_with(':') {
                            buf.insert(cursor, ' ');
                            cursor += 1;
                        }
                        redraw(prompt, start, &buf, cursor, &mut scroll);
                        continue;
                    }
                    // Fall through to submit; echo_submitted repaints the
                    // rows the popup covered.
                }
                // Only a TRAILING backslash at end-of-line continues to the
                // next line; a backslash left of the cursor mid-line (e.g. in
                // a Windows path) must not trigger continuation.
                let cont = buf.last() == Some(&'\\');
                if cont {
                    buf.pop();
                    cursor = cursor.min(buf.len());
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                }
                let text: String = buf.iter().collect();
                if !cont {
                    echo_submitted(prompt, &text);
                }
                return Some(RawLine::Submit(text, cont));
            }
            KeyCode::Tab => {
                if !sug.is_empty() {
                    let cand = sug[sug_idx].clone();
                    let (tok_start, _) = token_at(&buf, cursor);
                    let new: Vec<char> = cand.chars().collect();
                    buf.splice(tok_start..cursor, new.iter().copied());
                    cursor = tok_start + new.len();
                    if !cand.ends_with('/') && !cand.ends_with(':') {
                        buf.insert(cursor, ' ');
                        cursor += 1;
                    }
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                    continue;
                }
                let (tok_start, token) = token_at(&buf, cursor);
                let cands = completions(&buf, tok_start, &token);
                if cands.len() == 1 {
                    let cand = &cands[0];
                    let new: Vec<char> = cand.chars().collect();
                    buf.splice(tok_start..cursor, new.iter().copied());
                    cursor = tok_start + new.len();
                    if !cand.ends_with('/') {
                        buf.insert(cursor, ' ');
                        cursor += 1;
                    }
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                } else if cands.len() > 1 {
                    let common = common_prefix(&cands);
                    if common.chars().count() > token.chars().count() {
                        let new: Vec<char> = common.chars().collect();
                        buf.splice(tok_start..cursor, new.iter().copied());
                        cursor = tok_start + new.len();
                        redraw(prompt, start, &buf, cursor, &mut scroll);
                    } else {
                        clear_composer();
                        for c in &cands {
                            line(&format!("  {}", dim(c)));
                        }
                        flush();
                        reline!();
                    }
                }
            }
            KeyCode::Esc => {
                if !sug.is_empty() {
                    // Dismiss the popup only; it stays hidden until the
                    // buffer changes again.
                    sug_suppressed = true;
                    sug_dismissed_at = buf.clone();
                    continue; // loop top clears the popup rows
                }
                if is_vim_mode() && vim_state != VimState::Normal {
                    vim_state = VimState::Normal;
                    cursor = cursor.saturating_sub(1);
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                } else if !is_vim_mode() {
                    buf.clear();
                    cursor = 0;
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                }
            }
            _ => {}
        }
        if is_vim_mode() {
            let val = match vim_state {
                VimState::Normal => 0,
                VimState::Insert => 1,
                VimState::Visual(_) => 2,
            };
            VIM_STATE_VAL.store(val, Ordering::Relaxed);
        }
        // Cursor shape tracks the editing mode: accent bar for insert,
        // steady block for vim NORMAL, underline for VISUAL.
        let want = if !is_vim_mode() {
            CursorShape::Bar
        } else {
            match vim_state {
                VimState::Insert => CursorShape::Bar,
                VimState::Normal => CursorShape::Block,
                VimState::Visual(_) => CursorShape::Underline,
            }
        };
        if want != cur_shape {
            cur_shape = want;
            set_cursor_shape(want);
        }
    }
}

// ── spinner ───────────────────────────────────────────────────────────────────
pub struct Spinner {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

pub fn spinner_start(label: &str) -> Spinner {
    // The agent is working: hide the cursor so it doesn't flicker across the
    // screen with every streamed repaint. Shown again in spinner_stop.
    cursor_hide();
    let running = Arc::new(AtomicBool::new(true));
    let r2 = running.clone();
    let label = label.to_string();
    let handle = thread::spawn(move || {
        let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let started = std::time::Instant::now();
        let mut i = 0usize;
        while r2.load(Ordering::Relaxed) {
            if ALT_SCREEN.load(Ordering::Relaxed) {
                let mut out = io::stdout();
                let _ = execute!(out, SavePosition);
                queue_composer_box(&mut out);
                let _ = write!(
                    out,
                    "{} {} {}",
                    accent(&frames[i % frames.len()].to_string()),
                    dim(&label),
                    dim(&format!(
                        "· {}s · Esc to interrupt",
                        started.elapsed().as_secs()
                    ))
                );
                queue_composer_right_border(&mut out);
                let _ = execute!(out, RestorePosition);
                let _ = out.flush();
            } else {
                print!(
                    "\r{} {}",
                    accent(&frames[i % frames.len()].to_string()),
                    dim(&label)
                );
                flush();
            }
            i += 1;
            for _ in 0..8 {
                if !r2.load(Ordering::Relaxed) {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    });
    Spinner {
        running,
        handle: Some(handle),
    }
}

pub fn spinner_stop(mut s: Spinner) {
    s.running.store(false, Ordering::Relaxed);
    if let Some(h) = s.handle.take() {
        let _ = h.join();
    }
    if ALT_SCREEN.load(Ordering::Relaxed) {
        clear_composer();
    } else {
        print!("\r\x1b[2K");
        flush();
    }
    cursor_show();
}

pub fn with_spinner<T>(label: &str, work: impl FnOnce() -> T) -> T {
    let s = spinner_start(label);
    let result = work();
    spinner_stop(s);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for d in chars.by_ref() {
                    if d == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn cube_quantizes_to_nearest_step() {
        assert_eq!(cube(0), 0);
        assert_eq!(cube(255), 5);
        assert_eq!(cube(135), 2);
        assert_eq!(cube(94), 1);
    }

    #[test]
    fn word_motion_boundaries() {
        let b: Vec<char> = "foo  bar baz".chars().collect();
        assert_eq!(prev_word(&b, b.len()), 9);
        assert_eq!(prev_word(&b, 9), 5);
        assert_eq!(prev_word(&b, 0), 0);
        assert_eq!(next_word(&b, 0), 3);
        assert_eq!(next_word(&b, 3), 8);
        assert_eq!(next_word(&b, b.len()), b.len());
    }

    #[test]
    fn viewport_keeps_cursor_visible() {
        assert_eq!(viewport(3, 10, 0), (0, 3));
        assert_eq!(viewport(20, 10, 0), (11, 9));
        assert_eq!(viewport(2, 10, 11), (2, 0));
        assert_eq!(viewport(9, 10, 0), (0, 9));
        assert_eq!(viewport(15, 10, 11), (11, 4));
    }

    #[test]
    fn viewport_handles_zero_width() {
        let (s, col) = viewport(5, 0, 3);
        assert!(col < 1 || s <= 5);
        let _ = (s, col);
    }

    #[test]
    fn selection_range_handles_single_and_multi_line_drags() {
        let one = Selection {
            anchor: SelectPos { row: 2, col: 3 },
            focus: SelectPos { row: 2, col: 7 },
            sticky: false,
        };
        assert_eq!(selection_range_for(one, 2, 20), Some((3, 8)));
        assert_eq!(selection_range_for(one, 1, 20), None);

        let many = Selection {
            anchor: SelectPos { row: 1, col: 4 },
            focus: SelectPos { row: 3, col: 2 },
            sticky: false,
        };
        assert_eq!(selection_range_for(many, 1, 10), Some((4, 10)));
        assert_eq!(selection_range_for(many, 2, 10), Some((0, 10)));
        assert_eq!(selection_range_for(many, 3, 10), Some((0, 3)));
    }

    #[test]
    fn json_tool_call_blocks_are_classified_as_protocol_artifacts() {
        let tool_json = r#"{
  "name": "start_server",
  "arguments": {
    "command": "npm start",
    "port": 3000
  }
}"#;
        assert!(is_tool_call_json_block("json", tool_json));
        assert!(!is_tool_call_json_block(
            "json",
            r#"{"message":"ordinary data"}"#
        ));
        assert!(!is_tool_call_json_block("rust", tool_json));
    }

    #[test]
    fn raw_json_tool_calls_are_classified_as_protocol_artifacts() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{"tool_calls":[{"function":{"name":"write_file","arguments":"{\"path\":\"x\"}"}}]}"#,
        )
        .unwrap();
        assert!(json_value_looks_like_tool_call(&value));
        assert!(starts_like_top_level_json(
            "  {\"name\":\"read_file\",\"arguments\":{}}"
        ));
        assert!(!starts_like_top_level_json(
            "Here is JSON: {\"name\":\"read_file\"}"
        ));
    }

    #[test]
    fn maybe_json_buffer_has_short_lookahead_cap() {
        // Line cap: past 5 buffered lines the lookahead gives up and flushes.
        let lines = vec!["{".to_string(); 6];
        assert!(maybe_json_buffer_is_too_large(&lines));
        let lines = vec!["{".to_string(); 5];
        assert!(!maybe_json_buffer_is_too_large(&lines));
        // Byte cap: a single line over 2KB also flushes.
        let lines = vec!["x".repeat(2 * 1024 + 1)];
        assert!(maybe_json_buffer_is_too_large(&lines));
        let lines = vec!["{".to_string(), "\"message\":\"ordinary\"".to_string()];
        assert!(!maybe_json_buffer_is_too_large(&lines));
    }

    #[test]
    fn osc8_hyperlinks_are_zero_width_for_strip_clip_and_wrap() {
        // OSC 8 link: ESC ] 8 ; ; URL ST label ESC ] 8 ; ; ST
        let link = "\x1b]8;;https://example.com/doc\x1b\\click me\x1b]8;;\x1b\\ tail";
        assert_eq!(strip_ansi(link), "click me tail");
        // The URL inside the OSC string must cost zero display columns.
        assert_eq!(strip_ansi(&clip_ansi_line(link, 8)), "click me");
        assert_eq!(
            wrap_ansi_line(link, 100)
                .iter()
                .map(|l| strip_ansi(l))
                .collect::<String>(),
            "click me tail"
        );
        // BEL-terminated OSC variant too.
        let bel = "\x1b]8;;file:///tmp/a.png\x07shot\x1b]8;;\x07";
        assert_eq!(strip_ansi(bel), "shot");
    }

    #[test]
    fn clip_ansi_line_counts_display_columns_not_chars() {
        // ASCII: unchanged behavior.
        assert_eq!(clip_ansi_line("abcdef", 4), "abcd");
        // CJK chars are 2 columns wide each.
        assert_eq!(clip_ansi_line("ab你cd", 4), "ab你");
        // A width-2 char is never split across the boundary.
        assert_eq!(clip_ansi_line("ab你cd", 3), "ab");
        assert_eq!(clip_ansi_line("你好世界", 5), "你好");
        // ANSI escapes cost zero columns.
        assert_eq!(plain(&clip_ansi_line("\x1b[31m你好\x1b[0m", 2)), "你");
        assert_eq!(clip_ansi_line("abc", 0), "");
    }

    #[test]
    fn wrap_ansi_line_is_width_aware() {
        // ASCII: unchanged behavior.
        assert_eq!(wrap_ansi_line("abcd", 2), vec!["ab", "cd"]);
        // Four CJK chars = 8 columns → two rows of 2 chars at width 4.
        assert_eq!(wrap_ansi_line("你好世界", 4), vec!["你好", "世界"]);
        // Odd width: the next width-2 char wraps whole instead of splitting.
        assert_eq!(wrap_ansi_line("你好世界", 5), vec!["你好", "世界"]);
        assert_eq!(wrap_ansi_line("a你b", 2), vec!["a", "你", "b"]);
        // Degenerate width still terminates (one over-wide char per row).
        assert_eq!(wrap_ansi_line("你好", 1), vec!["你", "好"]);
        assert_eq!(wrap_ansi_line("", 4), vec![""]);
        assert_eq!(wrap_ansi_line("abc", 0), vec![""]);
    }

    #[test]
    fn hist_match_filters_by_prefix_and_scans_both_ways() {
        let h: Vec<String> = ["cargo test", "git push", "cargo bench", "ls"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // Backward from the end, unfiltered → newest entry.
        assert_eq!(hist_match(&h, "", None, true), Some(3));
        // Prefix filter: ↑ from a "cargo" draft skips "ls" and "git push".
        assert_eq!(hist_match(&h, "cargo", None, true), Some(2));
        assert_eq!(hist_match(&h, "cargo", Some(2), true), Some(0));
        assert_eq!(hist_match(&h, "cargo", Some(0), true), None);
        // Forward again (↓): back toward newer matches, then off the end.
        assert_eq!(hist_match(&h, "cargo", Some(0), false), Some(2));
        assert_eq!(hist_match(&h, "cargo", Some(2), false), None);
        assert_eq!(hist_match(&h, "zzz", None, true), None);
    }

    #[test]
    fn word_span_at_selects_identifiers_and_symbol_runs() {
        let line = "let total_rows = t.wrapped.len();";
        // Inside an identifier → the whole identifier.
        assert_eq!(word_span_at(line, 6), Some((4, 13))); // total_rows
        // On punctuation → the symbol run, not neighbours.
        assert_eq!(word_span_at(line, 15), Some((15, 15))); // '='
        // On whitespace → nothing.
        assert_eq!(word_span_at(line, 3), None);
        // Out of range → nothing.
        assert_eq!(word_span_at(line, 999), None);
    }

    #[test]
    fn history_search_finds_newest_first() {
        let h = vec![
            "git status".to_string(),
            "cargo test".to_string(),
            "git push".to_string(),
        ];
        assert_eq!(history_search(&h, "git", 0).as_deref(), Some("git push"));
        assert_eq!(history_search(&h, "git", 1).as_deref(), Some("git status"));
        assert_eq!(history_search(&h, "git", 2), None);
        assert_eq!(history_search(&h, "", 0), None);
        assert_eq!(
            history_search(&h, "cargo", 0).as_deref(),
            Some("cargo test")
        );
    }

    #[test]
    fn token_at_grabs_trailing_token() {
        let b: Vec<char> = "go @src/ma".chars().collect();
        let (start, tok) = token_at(&b, b.len());
        assert_eq!((start, tok.as_str()), (3, "@src/ma"));
    }

    #[test]
    fn common_prefix_works() {
        assert_eq!(common_prefix(&["/resume".into(), "/run".into()]), "/r");
        assert_eq!(common_prefix(&["abc".into()]), "abc");
        assert_eq!(common_prefix(&[]), "");
    }

    #[test]
    fn completions_slash_only_at_line_start() {
        let b: Vec<char> = "/re".chars().collect();
        assert!(completions(&b, 0, "/re").contains(&"/resume".to_string()));
        let b2: Vec<char> = "do /re".chars().collect();
        assert!(completions(&b2, 3, "/re").is_empty());
        let b3: Vec<char> = "/scr".chars().collect();
        assert!(completions(&b3, 0, "/scr").contains(&"/scroll".to_string()));
    }

    #[test]
    fn path_candidates_matches_prefix_and_marks_dirs() {
        use std::fs;
        let d = std::env::temp_dir().join(format!("bwn-comp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("alpha.txt"), "").unwrap();
        fs::write(d.join("apple.txt"), "").unwrap();
        fs::create_dir_all(d.join("assets")).unwrap();
        fs::write(d.join("beta.txt"), "").unwrap();
        assert_eq!(
            path_candidates("a", &d),
            vec![
                "alpha.txt".to_string(),
                "apple.txt".to_string(),
                "assets/".to_string()
            ]
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn no_color_strips_escapes() {
        assert_eq!(plain("\x1b[31mhi\x1b[0m"), "hi");
    }

    #[test]
    fn mode_badge_contains_label() {
        let b = plain(&mode_badge("BUILD"));
        assert!(b.contains("BUILD"), "{b}");
        let p = plain(&mode_badge("PLAN"));
        assert!(p.contains("PLAN"), "{p}");
        let bs = plain(&mode_badge("BRAINSTORM"));
        assert!(bs.contains("BRAINSTORM"), "{bs}");
    }

    #[test]
    fn test_context_meter_noop_when_zero() {
        super::context_meter(0, 0);
        super::context_meter(5000, 100000);
    }

    #[test]
    fn test_vim_mode_toggle_and_state_label() {
        let initial = super::is_vim_mode();
        if initial {
            super::toggle_vim_mode();
        }
        assert_eq!(super::get_vim_state_label(), "");
        super::toggle_vim_mode();
        assert!(super::is_vim_mode());
        assert_eq!(super::get_vim_state_label(), "NORMAL");
        super::toggle_vim_mode();
        assert!(!super::is_vim_mode());
    }

    #[test]
    fn test_path_candidates_semantic_prefixes() {
        use std::fs;
        let d = std::env::temp_dir().join(format!("bwn-sem-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let cands = super::path_candidates("rules:bug", &d);
        assert!(!cands.is_empty());
        assert!(cands
            .iter()
            .any(|c| c.contains("bug_fix_requires_regression_test")));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn test_end_word_helper() {
        let b: Vec<char> = "hello world foo".chars().collect();
        assert_eq!(super::end_word(&b, 0), 4);
        assert_eq!(super::end_word(&b, 4), 10);
        assert_eq!(super::end_word(&b, 10), 14);
    }

    #[test]
    fn inline_md_unbalanced_markers_stay_literal() {
        // A single `*` (arithmetic), a lone backtick, and an unmatched `**`
        // must not style the rest of the line — they render verbatim.
        assert_eq!(format_inline_md("5 * 3 = 15"), "5 * 3 = 15");
        assert_eq!(format_inline_md("call `foo now"), "call `foo now");
        assert_eq!(format_inline_md("a ** b"), "a ** b");
    }

    #[test]
    fn inline_md_balanced_markers_consume_delimiters() {
        // Balanced emphasis/code: the markers are consumed and the inner text
        // preserved (checked after stripping ANSI, so this holds in any color
        // mode).
        assert_eq!(plain(&format_inline_md("a *word* b")), "a word b");
        assert_eq!(plain(&format_inline_md("a **bold** c")), "a bold c");
        assert_eq!(plain(&format_inline_md("use `code` here")), "use code here");
    }

    #[test]
    fn render_md_draws_fenced_code_blocks() {
        let doc = "before\n```rust\nlet x = 1;\n```\nafter";
        let out = plain(&render_md(doc));
        assert!(out.contains("rust"), "{out}");
        assert!(out.contains("│ let x = 1;"), "{out}");
        assert!(out.contains("╭") && out.contains("╰"), "{out}");
        assert!(!out.contains("```"), "{out}");
        assert!(out.contains("before") && out.contains("after"), "{out}");
    }

    #[test]
    fn render_md_closes_unterminated_fence() {
        let out = plain(&render_md("```py\nprint(1)"));
        assert!(out.contains("py"), "{out}");
        assert!(out.contains("│ print(1)"), "{out}");
        assert!(out.contains("╰"), "{out}");
        assert!(!out.contains("```"), "{out}");
    }

    #[test]
    fn render_md_without_fences_matches_per_line_rendering() {
        let doc = "# Head\n- item\nplain";
        let expected: Vec<String> = doc.lines().map(render_md_line).collect();
        assert_eq!(render_md(doc), expected.join("\n"));
    }

    #[test]
    fn stream_renderer_fence_has_label_and_no_raw_backticks() {
        let mut r = StreamRenderer::new();
        r.push("```python\nx = 1\ny = 2\n```\nafter\n");
        r.flush();
        let joined = plain(&r.sink.join("\n"));
        assert!(joined.contains("python"), "{joined}");
        assert!(
            joined.contains("│ x = 1") && joined.contains("│ y = 2"),
            "{joined}"
        );
        assert!(joined.contains("╭") && joined.contains("╰"), "{joined}");
        assert!(!joined.contains("```"), "{joined}");
        assert!(joined.contains("after"), "{joined}");
    }

    #[test]
    fn stream_renderer_commits_rendered_lines_at_any_chunk_split() {
        let doc = "# Title\nplain **bold** text\n```rs\nfn main() {}\n```\ntail\n";
        // Whole-document reference run.
        let mut whole = StreamRenderer::new();
        whole.push(doc);
        whole.flush();
        // Split at every char boundary; the committed transcript must not
        // depend on where the stream chunks happened to land.
        for cut in 1..doc.len() {
            if !doc.is_char_boundary(cut) {
                continue;
            }
            let mut split = StreamRenderer::new();
            split.push(&doc[..cut]);
            split.push(&doc[cut..]);
            split.flush();
            assert_eq!(split.sink, whole.sink, "split at byte {cut}");
        }
        let joined = plain(&whole.sink.join("\n"));
        assert!(
            joined.contains("Title") && !joined.contains("# Title"),
            "{joined}"
        );
        assert!(joined.contains("│ fn main() {}"), "{joined}");
        assert!(!joined.contains("```"), "{joined}");
        assert!(!joined.contains("**"), "{joined}");
    }

    #[test]
    fn stream_renderer_flushes_partial_last_line_rendered() {
        let mut r = StreamRenderer::new();
        r.push("**no trailing newline**");
        r.flush();
        let joined = plain(&r.sink.join("\n"));
        assert!(joined.contains("no trailing newline"), "{joined}");
        assert!(!joined.contains("**"), "{joined}");
    }

    // The incremental wrap cache must always agree with wrapping every line
    // from scratch — for pushes, appends, in-place edits, removals, front
    // drains, and resizes.
    fn assert_cache_coherent(t: &Transcript) {
        let expect: Vec<Vec<String>> = t
            .lines
            .iter()
            .map(|l| wrap_ansi_line(l, t.width))
            .collect();
        assert_eq!(t.wrapped, expect);
    }

    #[test]
    fn transcript_cache_tracks_mutations() {
        let mut t = Transcript::new();
        t.ensure_width(10);
        t.push("short".to_string());
        t.push("a line that definitely wraps at ten cols".to_string());
        assert_cache_coherent(&t);
        assert!(t.append_to(0, " and more appended text"));
        assert!(!t.append_to(99, "nope"));
        assert_cache_coherent(&t);
        t.set(1, "replaced".to_string());
        assert_cache_coherent(&t);
        t.push("third".to_string());
        t.remove(0);
        assert_cache_coherent(&t);
        t.drain_front(1);
        assert_cache_coherent(&t);
        t.ensure_width(4); // resize rewraps everything
        assert_cache_coherent(&t);
        assert_eq!(
            t.total_rows(),
            t.wrapped.iter().map(|w| w.len()).sum::<usize>()
        );
        t.clear();
        assert_eq!(t.total_rows(), 0);
    }

    #[test]
    fn transcript_rows_range_matches_flattened_window() {
        let mut t = Transcript::new();
        t.ensure_width(6);
        for i in 0..10 {
            t.push(format!("line {i} with extra width to wrap"));
        }
        let flat: Vec<String> = t.wrapped.iter().flatten().cloned().collect();
        let total = t.total_rows();
        assert_eq!(total, flat.len());
        for start in [0usize, 1, 5, total.saturating_sub(3), total] {
            for count in [0usize, 1, 4, total] {
                let got: Vec<String> =
                    t.rows_range(start, count).into_iter().cloned().collect();
                let want: Vec<String> = flat
                    .iter()
                    .skip(start)
                    .take(count)
                    .cloned()
                    .collect();
                assert_eq!(got, want, "start={start} count={count}");
            }
        }
    }

    #[test]
    fn popup_window_keeps_selection_visible() {
        // Fewer candidates than the cap: show everything from the top.
        assert_eq!(popup_window(0, 3, 8), (0, 3));
        assert_eq!(popup_window(2, 3, 8), (0, 3));
        // More candidates than the cap: window follows the selection.
        assert_eq!(popup_window(0, 20, 8), (0, 8));
        assert_eq!(popup_window(7, 20, 8), (0, 8));
        assert_eq!(popup_window(10, 20, 8), (3, 8));
        assert_eq!(popup_window(19, 20, 8), (12, 8));
        // Empty list.
        assert_eq!(popup_window(0, 0, 8), (0, 0));
    }

    #[test]
    fn every_builtin_slash_command_has_a_description() {
        for cmd in SLASH_COMMANDS_BASE {
            assert!(
                !slash_command_desc(cmd).is_empty(),
                "missing popup description for {cmd}"
            );
        }
    }

    #[test]
    fn popup_candidates_only_for_interesting_tokens() {
        // Ordinary prose never spawns a popup.
        let prose: Vec<char> = "fix the bug".chars().collect();
        assert!(popup_candidates(&prose, prose.len()).is_empty());
        // A line-start slash token does.
        let cmd: Vec<char> = "/re".chars().collect();
        assert!(popup_candidates(&cmd, cmd.len()).contains(&"/resume".to_string()));
        // Sub-arguments of a slash command do.
        let sub: Vec<char> = "/mode pl".chars().collect();
        assert_eq!(popup_candidates(&sub, sub.len()), vec!["plan".to_string()]);
        // A slash mid-message does not.
        let mid: Vec<char> = "see /etc".chars().collect();
        assert!(popup_candidates(&mid, mid.len()).is_empty());
        // Empty token: nothing to suggest.
        let blank: Vec<char> = "/help ".chars().collect();
        assert!(popup_candidates(&blank, blank.len()).is_empty());
    }

    #[test]
    fn test_markdown_rendering_formatting() {
        let md = super::render_md_line("# Hello **world** *italic* `code`");
        let p = plain(&md);
        assert!(p.contains("Hello"), "{p}");
        assert!(p.contains("world"), "{p}");
        assert!(p.contains("italic"), "{p}");
        assert!(p.contains("code"), "{p}");

        let num_list = super::render_md_line("1. First item");
        let p_num = plain(&num_list);
        assert!(p_num.contains("1."), "{p_num}");
        assert!(p_num.contains("First item"), "{p_num}");

        let quote = super::render_md_line("> A blockquote");
        let p_quote = plain(&quote);
        assert!(p_quote.contains("│"), "{p_quote}");
        assert!(p_quote.contains("A blockquote"), "{p_quote}");

        let link = super::render_md_line("Click [Google](https://google.com) now");
        let p_link = plain(&link);
        assert!(p_link.contains("Google"), "{p_link}");
        assert!(p_link.contains("(https://google.com)"), "{p_link}");

        let bullet = super::render_md_line("- Bullet item");
        let p_bullet = plain(&bullet);
        assert!(p_bullet.contains("•"), "{p_bullet}");
        assert!(p_bullet.contains("Bullet item"), "{p_bullet}");
    }
}

