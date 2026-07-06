// Terminal layer: alternate screen, optional raw mode, ANSI colors, line input,
// and a spinner. Raw mode gives consistent key-driven input across platforms; in
// raw mode the kernel's line discipline is off, so every newline we emit must be
// "\r\n" and we echo keystrokes ourselves. Falls back to cooked line input when
// stdout isn't a TTY, so piped/headless use is unaffected.

use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, AtomicU8, Ordering};
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
}

fn transcript() -> &'static Mutex<Vec<String>> {
    static LINES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    LINES.get_or_init(|| Mutex::new(Vec::new()))
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

fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some()
}

fn truecolor() -> bool {
    matches!(
        std::env::var("COLORTERM").ok().as_deref(),
        Some("truecolor") | Some("24bit")
    )
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

fn paint(c: Rgb, s: &str) -> String {
    if no_color() {
        return s.to_string();
    }
    format!("\x1b[{}m{s}\x1b[39m", sgr_fg(c))
}

fn attr(code: &str, s: &str) -> String {
    if no_color() {
        return s.to_string();
    }
    let reset = if code == "1" { "22" } else { "0" };
    format!("\x1b[{code}m{s}\x1b[{reset}m")
}

pub fn bold(s: &str) -> String {
    attr("1", s)
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

// Mode-colored badge: PLAN (green), BUILD (cyan), BRAINSTORM (amber).
pub fn mode_badge(mode: &str) -> String {
    let (label, color) = match mode {
        "PLAN" => ("⚡ PLAN", MODE_PLAN),
        "BRAINSTORM" => ("💡 BRAINSTORM", MODE_BSTORM),
        _ => ("🚀 BUILD", MODE_BUILD),
    };
    if no_color() {
        format!("[{label}]")
    } else {
        format!("\x1b[{}m[{label}]\x1b[0m", sgr_fg(color))
    }
}

// ── inline diff ────────────────────────────────────────────────────────────
pub fn diff(old: &str, new: &str) -> String {
    const CTX: usize = 2;
    const MAX: usize = 60;
    let o: Vec<&str> = old.lines().collect();
    let n: Vec<&str> = new.lines().collect();

    let mut head = 0;
    while head < o.len() && head < n.len() && o[head] == n[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < o.len() - head.min(o.len())
        && tail < n.len() - head.min(n.len())
        && o[o.len() - 1 - tail] == n[n.len() - 1 - tail]
    {
        tail += 1;
    }

    let ctx_start = head.saturating_sub(CTX);
    let mut out: Vec<String> = Vec::new();
    for line in &o[ctx_start..head] {
        out.push(dim(&format!("  {line}")));
    }
    for line in &o[head..o.len() - tail] {
        out.push(paint(ERROR, &format!("- {line}")));
    }
    for line in &n[head..n.len() - tail] {
        out.push(paint(SUCCESS, &format!("+ {line}")));
    }
    let ctx_end = (o.len() - tail + CTX).min(o.len());
    for line in &o[o.len() - tail..ctx_end] {
        out.push(dim(&format!("  {line}")));
    }
    if out.len() > MAX {
        let shown = out[..MAX].join("\n");
        return format!(
            "{shown}\n{}",
            dim(&format!("  …(+{} more lines)", out.len() - MAX))
        );
    }
    out.join("\n")
}

pub fn added_preview(content: &str) -> String {
    const MAX: usize = 40;
    let lines: Vec<&str> = content.lines().collect();
    let shown: Vec<String> = lines
        .iter()
        .take(MAX)
        .map(|l| paint(SUCCESS, &format!("+ {l}")))
        .collect();
    if lines.len() > MAX {
        format!(
            "{}\n{}",
            shown.join("\n"),
            dim(&format!("  …(+{} more lines)", lines.len() - MAX))
        )
    } else {
        shown.join("\n")
    }
}

// ── streaming code-block renderer ────────────────────────────────────────────
// Feeds line-by-line through assistant streaming output. Triple-backtick fenced
// code blocks are rendered with a box border, and each block is automatically
// copied to the clipboard via OSC 52 (supported by iTerm2, kitty, Alacritty,
// WezTerm, macOS Terminal 2.12+, and most modern terminals).
//
// Usage: create one per assistant turn, call push() for each streamed chunk,
// call flush() after streaming ends.

enum StreamState {
    Normal,
    InCode { lang: String, lines: Vec<String> },
}

pub struct StreamRenderer {
    pending: String,
    state: StreamState,
    w: usize, // terminal width cap for box drawing
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
        }
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
                    self.process_line(&line_text);
                }
                None if end && !self.pending.is_empty() => {
                    let last = std::mem::take(&mut self.pending);
                    self.process_line(&last);
                    break;
                }
                None => break,
            }
        }
    }

    fn process_line(&mut self, text: &str) {
        // Pull out the current state so we can unconditionally assign self.state below.
        let state = std::mem::replace(&mut self.state, StreamState::Normal);
        match state {
            StreamState::Normal => {
                if let Some(rest) = text.strip_prefix("```") {
                    // Opening fence — draw box header and enter code mode.
                    let lang = rest.trim().to_string();
                    let header = self.box_header(&lang);
                    line(&header);
                    self.state = StreamState::InCode {
                        lang,
                        lines: Vec::new(),
                    };
                } else {
                    // Regular text: preserve blank lines; no extra prefix (matches
                    // the existing non-code streaming style).
                    line(text);
                    self.state = StreamState::Normal;
                }
            }
            StreamState::InCode { lang, mut lines } => {
                if text.trim_end_matches('\r') == "```" {
                    // Closing fence — draw footer, then copy to clipboard.
                    line(&self.box_footer());
                    let code = lines.join("\n");
                    osc52_copy(&code);
                    line(&dim("  ✓ ⎘ copied to clipboard"));
                    self.state = StreamState::Normal;
                    let _ = lang;
                } else {
                    // Code line — prefix with a dim vertical bar.
                    let row = format!("  {} {text}", dim("│"));
                    line(&row);
                    lines.push(text.to_string());
                    self.state = StreamState::InCode { lang, lines };
                }
            }
        }
    }

    fn box_header(&self, lang: &str) -> String {
        let prefix = if lang.is_empty() {
            "  ╭─".to_string()
        } else {
            format!("  ╭─ ⟨ {} ⟩ ", lang)
        };
        let used = prefix.chars().count();
        let dashes = self.w.saturating_sub(used).max(1);
        format!("{}{}", dim(&prefix), dim(&"─".repeat(dashes)))
    }

    fn box_footer(&self) -> String {
        let prefix = "  ╰";
        let dashes = self.w.saturating_sub(prefix.chars().count()).max(1);
        format!("{}{}", dim(prefix), dim(&"─".repeat(dashes)))
    }
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
}

fn typeahead() -> &'static std::sync::Mutex<TypeAheadState> {
    static TA: std::sync::OnceLock<std::sync::Mutex<TypeAheadState>> = std::sync::OnceLock::new();
    TA.get_or_init(|| {
        std::sync::Mutex::new(TypeAheadState {
            buf: Vec::new(),
            cursor: 0,
        })
    })
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
                    KeyCode::Char('c') if ctrl => {
                        TYPEAHEAD_INTERRUPTED.store(true, Ordering::Relaxed);
                        ta.buf.clear();
                        ta.cursor = 0;
                    }
                    KeyCode::Char('u') if ctrl => {
                        let d = ta.cursor;
                        ta.buf.drain(..d);
                        ta.cursor = 0;
                    }
                    KeyCode::Esc => {
                        ta.buf.clear();
                        ta.cursor = 0;
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
    let mut out = io::stdout();
    let _ = execute!(out, SavePosition);
    if let Ok(ta) = typeahead().lock() {
        let mut scroll = 0usize;
        render_composer(
            &format!("{} {} ", dim("queued"), accent("›")),
            &ta.buf,
            ta.cursor,
            &mut scroll,
        );
    }
    let _ = execute!(out, RestorePosition);
}

fn take_typeahead() -> (Vec<char>, usize) {
    match typeahead().lock() {
        Ok(mut ta) => {
            let buf = std::mem::take(&mut ta.buf);
            let cur = std::mem::replace(&mut ta.cursor, 0);
            (buf, cur)
        }
        Err(_) => (Vec::new(), 0),
    }
}

fn term_size() -> (u16, u16) {
    let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
    (if w == 0 { 80 } else { w }, if h == 0 { 24 } else { h })
}

fn reserved_rows() -> u16 {
    if ALT_SCREEN.load(Ordering::Relaxed) {
        2
    } else {
        1
    }
}

fn composer_row() -> u16 {
    let (_, h) = term_size();
    h.saturating_sub(reserved_rows()).min(h.saturating_sub(1))
}

fn footer_row() -> u16 {
    term_size().1.saturating_sub(1)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn prompt_width(prompt: &str) -> u16 {
    strip_ansi(prompt).chars().count().min(u16::MAX as usize) as u16
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
    let _ = queue!(
        out,
        MoveTo(0, composer_row()),
        Clear(ClearType::CurrentLine)
    );
    let _ = out.flush();
}

fn render_footer() {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    let Ok(footer) = footer_text().lock() else {
        return;
    };
    let (width, _) = term_size();
    let mut out = io::stdout();
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
        let text = if offset > 0 {
            format!(
                "{}{} {}",
                vim_badge,
                footer.as_str(),
                dim(&format!("· scroll +{offset}"))
            )
        } else {
            format!("{}{}", vim_badge, footer.as_str())
        };
        let _ = write!(out, "{}", clip_ansi_line(&text, width as usize));
    }
    let _ = out.flush();
}

pub fn set_permission_mode(mode: &str) {
    if let Ok(mut footer) = footer_text().lock() {
        *footer = format!(
            "{} {} {}",
            dim("permission:"),
            bold(mode),
            dim("· /permissions ask|auto|readonly · wheel/PgUp scroll · drag copies · /scroll on|off")
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
    let Ok(lines) = transcript().lock() else {
        return;
    };
    let mut visible_lines = Vec::new();
    for line in lines.iter() {
        visible_lines.extend(wrap_ansi_line(line, width));
    }
    let max_offset = visible_lines.len().saturating_sub(rows);
    let offset = SCROLL_OFFSET.load(Ordering::Relaxed).min(max_offset);
    if offset != SCROLL_OFFSET.load(Ordering::Relaxed) {
        SCROLL_OFFSET.store(offset, Ordering::Relaxed);
    }
    let start = visible_lines.len().saturating_sub(rows + offset);
    let mut out = io::stdout();
    let sel = selection().lock().ok().and_then(|g| *g);
    let mut plain_rows = Vec::with_capacity(rows);
    for row in 0..rows {
        let _ = queue!(out, MoveTo(0, row as u16), Clear(ClearType::CurrentLine));
        if let Some(line) = visible_lines.get(start + row) {
            let plain = strip_ansi(line);
            plain_rows.push(plain.clone());
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
        } else {
            plain_rows.push(String::new());
        }
    }
    if let Ok(mut rows) = visible_rows().lock() {
        *rows = plain_rows;
    }
    let _ = out.flush();
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
    ALT_SCREEN.load(Ordering::Relaxed) && row < composer_row()
}

fn selection_start(row: u16, col: u16) {
    if !in_output_region(row) {
        return;
    }
    let pos = SelectPos { row, col };
    if let Ok(mut sel) = selection().lock() {
        *sel = Some(Selection {
            anchor: pos,
            focus: pos,
        });
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
        }
    }
    render_output();
    render_queued_composer();
}

fn selection_finish(row: u16, col: u16) {
    if in_output_region(row) {
        if let Ok(mut sel) = selection().lock() {
            if let Some(s) = sel.as_mut() {
                s.focus = SelectPos { row, col };
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

fn inverse(s: &str) -> String {
    if no_color() {
        s.to_string()
    } else {
        format!("\x1b[7m{s}\x1b[27m")
    }
}

fn selected_line(line: &str, range: (usize, usize)) -> String {
    let chars: Vec<char> = line.chars().collect();
    let (from, to) = range;
    let before: String = chars.iter().take(from).collect();
    let mid: String = chars.iter().skip(from).take(to - from).collect();
    let after: String = chars.iter().skip(to).collect();
    format!("{before}{}{after}", inverse(&mid))
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
    let avail = width.saturating_sub(pwidth).max(8) as usize;
    let (s, col) = viewport(cursor, avail, *scroll);
    *scroll = s;
    let end = (s + avail).min(buf.len());
    let shown: String = buf[s..end].iter().collect();
    let mut out = io::stdout();
    let _ = queue!(
        out,
        MoveTo(0, composer_row()),
        Clear(ClearType::CurrentLine)
    );
    let _ = write!(out, "{prompt}{shown}");
    render_footer();
    let _ = queue!(
        out,
        MoveTo(pwidth.saturating_add(col as u16), composer_row())
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
    let top_bar = format!("╭{}╮", "─".repeat(w.saturating_sub(2)));
    let bot_bar = format!("╰{}╯", "─".repeat(w.saturating_sub(2)));

    line(&accent(&top_bar));
    // Wordmark row — gradient "buildwithnexus" + domain
    line(&clip_ansi_line(
        &format!(
            "  {}  {}  {}  {}  {}",
            bold(&wordmark()),
            dim("·"),
            paint(ACCENT, "buildwithnexus.dev"),
            dim("·"),
            dim(&format!("v{}", crate::VERSION)),
        ),
        w,
    ));
    // Context row — provider · model · cwd (truncated to fit)
    let cwd_display: String = cwd
        .chars()
        .rev()
        .take(w.saturating_sub(30))
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
        &dim(&format!("  ⚙ Provider: {}  ·  🤖 Model: {}  ·  📂 {}", provider, model, cwd_label)),
        w,
    ));
    // Mode row
    line(&clip_ansi_line(
        &format!(
            "  Mode: {}    {}  {}  {}",
            mode_badge(mode),
            dim("[Shift+Tab] cycle mode"),
            dim("·"),
            dim("[/help] commands"),
        ),
        w,
    ));
    line(&accent(&bot_bar));
}

// Refresh the mode indicator line in-place after a mode change (no full clear).
pub fn show_mode_change(mode: &str) {
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
    let est_cost = (used as f64 / 1000.0) * 0.003;
    line(&format!(
        "  {} [{}] {}",
        dim("telemetry"),
        colored,
        dim(&format!(
            "{pct}% sat  ·  {}k / {}k tokens  ·  est. cost: ${:.4}",
            used / 1_000,
            total / 1_000,
            est_cost
        )),
    ));
}

pub fn inference_telemetry(tokens_generated: usize, elapsed_secs: f64) {
    if elapsed_secs <= 0.0 || tokens_generated == 0 {
        return;
    }
    let tok_per_sec = tokens_generated as f64 / elapsed_secs;
    let speed_badge = if tok_per_sec >= 40.0 {
        green(&format!("{:.1} tok/s", tok_per_sec))
    } else if tok_per_sec >= 15.0 {
        yellow(&format!("{:.1} tok/s", tok_per_sec))
    } else {
        red(&format!("{:.1} tok/s", tok_per_sec))
    };
    line(&format!(
        "  {} [{}] {}",
        dim("inference"),
        speed_badge,
        dim(&format!("~{} tokens generated in {:.2}s", tokens_generated, elapsed_secs)),
    ));
}

// Enter the alternate screen and raw mode (and capture panics to restore the
// terminal even on crash). The bottom row is reserved for the composer; output
// scrolls in the region above it.
pub fn enter_alt(raw: bool) {
    if raw {
        SCROLL_OFFSET.store(0, Ordering::Relaxed);
        if let Ok(mut lines) = transcript().lock() {
            lines.clear();
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
    }
    if raw && enable_raw_mode().is_ok() {
        RAW.store(true, Ordering::Relaxed);
        let _ = execute!(io::stdout(), EnableBracketedPaste);
        MOUSE_CAPTURED.store(false, Ordering::Relaxed);
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        reset_output_region();
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
        if let Ok(mut lines) = transcript().lock() {
            lines.clear();
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
            for d in chars.by_ref() {
                out.push(d);
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if visible >= max_cols {
            break;
        }
        out.push(c);
        visible += 1;
    }
    out
}

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
            for d in chars.by_ref() {
                current.push(d);
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if visible >= max_cols {
            out.push(std::mem::take(&mut current));
            visible = 0;
        }
        current.push(c);
        visible += 1;
    }
    out.push(current);
    out
}

pub fn line(s: &str) {
    if ALT_SCREEN.load(Ordering::Relaxed) {
        if let Ok(mut lines) = transcript().lock() {
            for part in s.replace('\r', "").split('\n') {
                lines.push(part.to_string());
            }
            const MAX_LINES: usize = 2_000;
            if lines.len() > MAX_LINES {
                let extra = lines.len() - MAX_LINES;
                lines.drain(0..extra);
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
        if let Ok(mut lines) = transcript().lock() {
            if lines.is_empty() {
                lines.push(String::new());
            }
            let normalized = chunk.replace('\r', "");
            let mut parts = normalized.split('\n');
            if let Some(first) = parts.next() {
                if let Some(last) = lines.last_mut() {
                    last.push_str(first);
                }
            }
            for part in parts {
                lines.push(part.to_string());
            }
            const MAX_LINES: usize = 2_000;
            if lines.len() > MAX_LINES {
                let extra = lines.len() - MAX_LINES;
                lines.drain(0..extra);
            }
        }
        render_output();
        clear_composer();
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
    let (s, col) = viewport(cursor, avail, *scroll);
    *scroll = s;
    let end = (s + avail).min(buf.len());
    let shown: String = buf[s..end].iter().collect();
    let mut out = io::stdout();
    let _ = queue!(
        out,
        MoveTo(start.0, start.1),
        Clear(ClearType::UntilNewLine)
    );
    let _ = write!(out, "{shown}");
    let _ = queue!(out, MoveTo(start.0.saturating_add(col as u16), start.1));
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
    "/effort",
    "/mcp",
    "/plugin",
    "/marketplace",
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

fn load_slash_commands() -> Vec<String> {
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
            ) {
                if id.to_lowercase().contains(&query.to_lowercase())
                    || entity.name.to_lowercase().contains(&query.to_lowercase())
                {
                    out.push(format!("symbol:{id}"));
                }
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
                if let Ok(loaded) = crate::rules::RuleEngine::load_from_file(&e.path().to_string_lossy()) {
                    for r in loaded.rules {
                        engine.add_rule(r);
                    }
                }
            }
        }
        let mut out = Vec::new();
        for rule in &engine.rules {
            if rule.id.to_lowercase().contains(&query.to_lowercase())
                || rule.description.to_lowercase().contains(&query.to_lowercase())
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
        for special in ["diff", "status", "rules", "rules:", "kb:", "symbol:", "url:", "web:"] {
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
        "/effort" => {
            return ["low", "medium", "high", "max"]
                .iter()
                .filter(|&&s| s.starts_with(token))
                .map(|s| s.to_string())
                .collect();
        }
        "/mcp" | "/plugin" | "/marketplace" => {
            return [
                "list",
                "add",
                "remove",
                "install",
                "filesystem",
                "github",
                "git",
                "sqlite",
                "brave-search",
                "memory",
                "duckduckgo",
            ]
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
        (prompt_width(prompt), composer_row())
    } else {
        crossterm::cursor::position().unwrap_or((0, 0))
    };
    let mut buf: Vec<char> = prefill;
    let mut cursor = prefill_cur.min(buf.len());
    let mut scroll = 0usize;
    redraw(prompt, start, &buf, cursor, &mut scroll);
    let mut hist_idx: Option<usize> = None;
    let mut kill = String::new();
    let mut vim_state = if is_vim_mode() { VimState::Normal } else { VimState::Insert };
    let mut vim_undo_stack: Vec<Vec<char>> = vec![buf.clone()];
    if is_vim_mode() {
        VIM_STATE_VAL.store(0, Ordering::Relaxed);
    }

    macro_rules! reline {
        () => {{
            if ALT_SCREEN.load(Ordering::Relaxed) {
                start = (prompt_width(prompt), composer_row());
            } else {
                print!("\r{prompt}");
                flush();
                start = crossterm::cursor::position().unwrap_or(start);
            }
            redraw(prompt, start, &buf, cursor, &mut scroll);
        }};
    }

    loop {
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
                    start = (prompt_width(prompt), composer_row());
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
                for c in kill.clone().chars() {
                    buf.insert(cursor, c);
                    cursor += 1;
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
                        let _ = queue!(out, MoveTo(0, start.1), Clear(ClearType::UntilNewLine));
                        let _ = write!(
                            out,
                            "{}{}",
                            dim(&format!("(reverse-i-search)`{query}`: ")),
                            m.clone().unwrap_or_default()
                        );
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
                            if cursor < buf.len() { cursor += 1; }
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
                        'l' => if cursor < buf.len() { cursor += 1; },
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
                                if !kill.is_empty() { osc52_copy(&kill); }
                            }
                        }
                        'D' => {
                            kill = buf[cursor..].iter().collect();
                            buf.truncate(cursor);
                            if !kill.is_empty() { osc52_copy(&kill); }
                        }
                        'C' => {
                            kill = buf[cursor..].iter().collect();
                            buf.truncate(cursor);
                            if !kill.is_empty() { osc52_copy(&kill); }
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
                    let VimState::Visual(start_idx) = vim_state else { unreachable!() };
                    match c {
                        'h' => cursor = cursor.saturating_sub(1),
                        'l' => if cursor < buf.len() { cursor += 1; },
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
                                if !kill.is_empty() { osc52_copy(&kill); }
                            }
                            vim_state = VimState::Normal;
                        }
                        'y' => {
                            let min_i = start_idx.min(cursor);
                            let max_i = start_idx.max(cursor).min(buf.len().saturating_sub(1));
                            if min_i <= max_i && max_i < buf.len() {
                                kill = buf[min_i..=max_i].iter().collect();
                                if !kill.is_empty() { osc52_copy(&kill); }
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
                if let Ok(h) = history().lock() {
                    if !h.is_empty() {
                        let idx = match hist_idx {
                            None => h.len() - 1,
                            Some(0) => 0,
                            Some(i) => i - 1,
                        };
                        hist_idx = Some(idx);
                        buf = h[idx].chars().collect();
                        cursor = buf.len();
                    }
                }
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Down => {
                if let Ok(h) = history().lock() {
                    match hist_idx {
                        Some(i) if i + 1 < h.len() => {
                            hist_idx = Some(i + 1);
                            buf = h[i + 1].chars().collect();
                            cursor = buf.len();
                        }
                        _ => {
                            hist_idx = None;
                            buf.clear();
                            cursor = 0;
                        }
                    }
                }
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Enter => {
                let cont = cursor > 0 && buf[cursor - 1] == '\\';
                if cont {
                    buf.remove(cursor - 1);
                    cursor -= 1;
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                }
                let text: String = buf.iter().collect();
                if !cont {
                    echo_submitted(prompt, &text);
                }
                return Some(RawLine::Submit(text, cont));
            }
            KeyCode::Tab => {
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
                if is_vim_mode() && vim_state != VimState::Normal {
                    vim_state = VimState::Normal;
                    if cursor > 0 {
                        cursor -= 1;
                    }
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
    }
}

// ── spinner ───────────────────────────────────────────────────────────────────
pub struct Spinner {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

pub fn spinner_start(label: &str) -> Spinner {
    let running = Arc::new(AtomicBool::new(true));
    let r2 = running.clone();
    let label = label.to_string();
    let handle = thread::spawn(move || {
        let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let mut i = 0usize;
        while r2.load(Ordering::Relaxed) {
            if ALT_SCREEN.load(Ordering::Relaxed) {
                let mut out = io::stdout();
                let _ = execute!(out, SavePosition);
                let _ = queue!(
                    out,
                    MoveTo(0, composer_row()),
                    Clear(ClearType::CurrentLine)
                );
                let _ = write!(
                    out,
                    "{} {}",
                    accent(&frames[i % frames.len()].to_string()),
                    dim(&label)
                );
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
    fn diff_marks_changed_middle_with_context() {
        let old = "a\nb\nc\nd\ne";
        let new = "a\nb\nX\nd\ne";
        let d = plain(&diff(old, new));
        assert!(d.contains("- c"), "{d}");
        assert!(d.contains("+ X"), "{d}");
        assert!(d.contains("  b") && d.contains("  d"), "{d}");
        assert!(!d.contains("- a") && !d.contains("+ e"), "{d}");
    }

    #[test]
    fn diff_pure_addition() {
        let d = plain(&diff("a\nb", "a\nb\nc"));
        assert!(d.contains("+ c"), "{d}");
        assert!(!d.contains("- "), "{d}");
    }

    #[test]
    fn diff_pure_removal() {
        let d = plain(&diff("a\nb\nc", "a\nc"));
        assert!(d.contains("- b"), "{d}");
    }

    #[test]
    fn diff_identical_has_no_markers() {
        let d = plain(&diff("a\nb", "a\nb"));
        assert!(!d.contains("- ") && !d.contains("+ "), "{d}");
    }

    #[test]
    fn diff_empty_old_is_all_additions() {
        let d = plain(&diff("", "x\ny"));
        assert!(d.contains("+ x") && d.contains("+ y"), "{d}");
    }

    #[test]
    fn added_preview_clips_long_content() {
        let content: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let p = plain(&added_preview(&content));
        assert!(p.contains("+ line 0"));
        assert!(p.contains("more lines"));
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
    fn test_inference_telemetry_noop_when_zero() {
        super::inference_telemetry(0, 0.0);
        super::inference_telemetry(100, 2.0);
        super::inference_telemetry(500, 10.0);
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
        assert!(cands.iter().any(|c| c.contains("bug_fix_requires_regression_test")));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn test_end_word_helper() {
        let b: Vec<char> = "hello world foo".chars().collect();
        assert_eq!(super::end_word(&b, 0), 4);
        assert_eq!(super::end_word(&b, 4), 10);
        assert_eq!(super::end_word(&b, 10), 14);
    }
}
