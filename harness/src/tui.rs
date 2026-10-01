// Terminal layer: alternate screen, optional raw mode, ANSI colors, line input,
// and a spinner. Raw mode gives consistent key-driven input across platforms; in
// raw mode the kernel's line discipline is off, so every newline we emit must be
// "\r\n" and we echo keystrokes ourselves. Falls back to cooked line input when
// stdout isn't a TTY, so piped/headless use is unaffected.

use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use crossterm::cursor::{MoveTo, RestorePosition, SavePosition};
use crossterm::event::{
    poll, read, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture,
    EnableBracketedPaste, EnableFocusChange, EnableMouseCapture, Event, KeyCode, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, queue};

static RAW: AtomicBool = AtomicBool::new(false);
static ALT_SCREEN: AtomicBool = AtomicBool::new(false);
static MOUSE_CAPTURED: AtomicBool = AtomicBool::new(false);
static SCROLL_OFFSET: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InterruptKind {
    None = 0,
    Escape = 1,
    CtrlC = 2,
}

static INTERRUPT_KIND_VAL: AtomicU8 = AtomicU8::new(0);
static AGENT_RUNNING: AtomicBool = AtomicBool::new(false);
// Live "working" readout: when the current turn started, how many streamed
// characters have landed since (→ tokens/s), and when the footer last
// repainted its elapsed-time tick.
static WORK_STARTED_MS: AtomicU64 = AtomicU64::new(0);
static STREAM_CHARS: AtomicUsize = AtomicUsize::new(0);
static LAST_STATUS_TICK_MS: AtomicU64 = AtomicU64::new(0);
static LAST_NOTIFY_MS: AtomicU64 = AtomicU64::new(0);
// Terminal focus (CSI ?1004 reports); assumed focused until told otherwise,
// so a terminal that never reports focus never gets a stray notification.
static FOCUSED: AtomicBool = AtomicBool::new(true);
// `notify` setting: 0 off, 1 auto (only while unfocused), 2 always.
static NOTIFY_MODE: AtomicU8 = AtomicU8::new(1);
// `images` setting: 0 off, 1 auto, 2 kitty (force placeholders), 3 blocks.
static IMAGES_MODE: AtomicU8 = AtomicU8::new(1);
// kitty image ids handed out this session; whether any were uploaded (so
// leaving the screen frees them).
static NEXT_IMAGE_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
static IMAGES_SENT: AtomicBool = AtomicBool::new(false);

fn model_label() -> &'static Mutex<String> {
    static M: OnceLock<Mutex<String>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(String::new()))
}

fn previewed_paths() -> &'static Mutex<std::collections::HashSet<std::path::PathBuf>> {
    static P: OnceLock<Mutex<std::collections::HashSet<std::path::PathBuf>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Model name shown in the footer (set at startup and on `/model`).
pub fn set_model_label(model: &str) {
    if let Ok(mut m) = model_label().lock() {
        *m = model.to_string();
    }
    render_footer();
}

/// Apply the `images` and `notify` settings keys.
pub fn configure_ui(images: &str, notify: &str) {
    IMAGES_MODE.store(
        match images.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "0" => 0,
            "kitty" => 2,
            "blocks" | "half" => 3,
            "sixel" => 4,
            _ => 1,
        },
        Ordering::Relaxed,
    );
    NOTIFY_MODE.store(
        match notify.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "0" => 0,
            "always" => 2,
            _ => 1,
        },
        Ordering::Relaxed,
    );
}

/// Whether the terminal window currently has focus (per CSI ?1004 reports).
pub fn is_focused() -> bool {
    FOCUSED.load(Ordering::Relaxed)
}

// ── desktop notifications ────────────────────────────────────────────────────
// Fired when a long turn ends while the window is unfocused: OSC 99 (kitty),
// OSC 777 (urxvt, VTE, WezTerm), OSC 9 (iTerm2, WezTerm, Windows Terminal —
// where OSC 9 isn't a progress code), plus BEL so terminals without any
// notification protocol still badge or bounce. Terminals ignore what they
// don't know.
fn osc9_is_notification() -> bool {
    let tp = std::env::var("TERM_PROGRAM").unwrap_or_default();
    tp == "iTerm.app" || tp == "WezTerm" || std::env::var_os("WT_SESSION").is_some()
}

fn osc9_4_is_progress() -> bool {
    let tp = std::env::var("TERM_PROGRAM").unwrap_or_default();
    tp == "ghostty"
        || std::env::var_os("WT_SESSION").is_some()
        || std::env::var_os("ConEmuANSI").is_some()
}

/// Send a desktop notification if the `notify` setting and focus state allow.
pub fn notify(title: &str, body: &str) {
    // Interactive TUI only: a headless/--json run must never emit chrome.
    if !ALT_SCREEN.load(Ordering::Relaxed) || !io::stdout().is_terminal() {
        return;
    }
    match NOTIFY_MODE.load(Ordering::Relaxed) {
        0 => return,
        1 if is_focused() => return,
        _ => {}
    }
    let clean = |s: &str| -> String { s.chars().filter(|c| !c.is_control()).take(200).collect() };
    let (title, body) = (clean(title), clean(body));
    let mut out = io::stdout();
    let _ = write!(
        out,
        "\x1b]99;i=1:d=0:p=title;{title}\x1b\\\x1b]99;i=1:d=1:p=body;{body}\x1b\\"
    );
    let _ = write!(out, "\x1b]777;notify;{title};{body}\x1b\\");
    if osc9_is_notification() {
        let _ = write!(out, "\x1b]9;{title}: {body}\x1b\\");
    }
    let _ = write!(out, "\x07");
    let _ = out.flush();
}

// Taskbar / dock progress (OSC 9;4): indeterminate while the agent works,
// cleared when it stops. Only on terminals where OSC 9;4 means progress.
fn taskbar_progress(on: bool) {
    if !ALT_SCREEN.load(Ordering::Relaxed) || !io::stdout().is_terminal() || !osc9_4_is_progress() {
        return;
    }
    let _ = write!(
        io::stdout(),
        "{}",
        if on {
            "\x1b]9;4;3\x1b\\"
        } else {
            "\x1b]9;4;0\x1b\\"
        }
    );
    flush();
}
static CONTEXT_USED: AtomicUsize = AtomicUsize::new(0);
static CONTEXT_TOTAL: AtomicUsize = AtomicUsize::new(0);

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
    height: usize,             // terminal rows the wrap was made for (images)
}

/// Bench-only surface for the transcript wrap cache — criterion can't reach
/// the private type. Regenerates the flagship rendering claim: appending a
/// streamed chunk re-wraps ONE line (incremental cache) vs re-wrapping the
/// whole transcript (the pre-0.12.0 behavior every keystroke paid for).
pub mod bench {
    use super::Transcript;

    pub struct WrapCache(Transcript);

    pub fn transcript(lines: usize, width: usize, line: &str) -> WrapCache {
        let mut t = Transcript::new();
        t.ensure_width(width);
        for _ in 0..lines {
            t.push(line.to_string());
        }
        WrapCache(t)
    }

    /// One streamed chunk landing: append to the open line, then restore it —
    /// two single-line rewraps, a strict upper bound on the real per-chunk cost.
    pub fn append_and_reset(c: &mut WrapCache, chunk: &str, base: &str) {
        let i = c.0.len() - 1;
        c.0.append_to(i, chunk);
        c.0.set(i, base.to_string());
    }

    /// The old behavior: every chunk re-wrapped the entire transcript.
    pub fn full_rewrap(c: &mut WrapCache, width: usize) {
        c.0.ensure_width(width);
    }
}

impl Transcript {
    const fn new() -> Self {
        Transcript {
            lines: Vec::new(),
            wrapped: Vec::new(),
            width: 0,
            height: 0,
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

    // Also rewrap when only the height changed: inline images size to both.
    fn ensure_size(&mut self, w: usize, h: usize) {
        if h != self.height {
            self.height = h;
            self.width = 0;
        }
        self.ensure_width(w);
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
// A colour as a theme names it: 24-bit (sent as truecolor, or the nearest
// 256-colour cube entry), one of the terminal's own 16 colours (SGR 30-37 /
// 90-97, so the user's palette decides), or the terminal default.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Col {
    Rgb(u8, u8, u8),
    Ansi(u8),
    Default,
}

struct Palette {
    name: &'static str,
    // Painted behind the alternate screen; None keeps the terminal's own.
    background: Option<Col>,
    text: Col,
    accent: Col,
    // Secondary/dim text: still at least 4.5:1 against the background.
    muted: Col,
    success: Col,
    warning: Col,
    error: Col,
    info: Col,
    mode_plan: Col,
    mode_build: Col,
    mode_bstorm: Col,
    // Diff row tints: whole added/removed rows get a subtle background; the
    // changed word span gets a stronger one.
    diff_add_bg: Col,
    diff_del_bg: Col,
    diff_add_emph_bg: Col,
    diff_del_emph_bg: Col,
    selection_bg: Col,
    // OSC 12 cursor colour, or None to leave the terminal's.
    cursor: Option<&'static str>,
    // Wordmark gradient stops, deep → pale.
    wordmark: [Col; 4],
}

// Tokyo Night, painted on its own background. The comment colour (#565f89)
// measures 2.76:1 there — below the WCAG AA 4.5:1 text minimum — so muted
// text uses #7e88b3 (4.93:1) from the same blue-violet family.
const DARK: Palette = Palette {
    name: "dark",
    background: Some(Col::Rgb(0x1a, 0x1b, 0x26)),
    text: Col::Rgb(0xc0, 0xca, 0xf5),
    accent: Col::Rgb(0xbb, 0x9a, 0xf7),
    muted: Col::Rgb(0x7e, 0x88, 0xb3),
    success: Col::Rgb(0x9e, 0xce, 0x6a),
    warning: Col::Rgb(0xe0, 0xaf, 0x68),
    error: Col::Rgb(0xf7, 0x76, 0x8e),
    info: Col::Rgb(0x7d, 0xcf, 0xff),
    mode_plan: Col::Rgb(0x9e, 0xce, 0x6a),
    mode_build: Col::Rgb(0x7a, 0xa2, 0xf7),
    mode_bstorm: Col::Rgb(0xe0, 0xaf, 0x68),
    diff_add_bg: Col::Rgb(0x1e, 0x31, 0x26),
    diff_del_bg: Col::Rgb(0x37, 0x22, 0x2c),
    diff_add_emph_bg: Col::Rgb(0x2c, 0x4d, 0x38),
    diff_del_emph_bg: Col::Rgb(0x5a, 0x2e, 0x40),
    selection_bg: Col::Rgb(0x28, 0x34, 0x57),
    cursor: Some("#bb9af7"),
    wordmark: [
        Col::Rgb(0x3d, 0x6d, 0xe0),
        Col::Rgb(0x7a, 0xa2, 0xf7),
        Col::Rgb(0x9e, 0xc9, 0xff),
        Col::Rgb(0xcf, 0xe5, 0xff),
    ],
};

// For light terminals, on the terminal's own background: every foreground
// is at least 4.5:1 against white, Solarized Light (#fdf6e3), #eeeeee and
// the diff tints (see the contrast test).
const LIGHT: Palette = Palette {
    name: "light",
    background: None,
    text: Col::Rgb(0x34, 0x3b, 0x58),
    accent: Col::Rgb(0x71, 0x40, 0xb8),
    muted: Col::Rgb(0x5a, 0x60, 0x7a),
    success: Col::Rgb(0x2d, 0x6a, 0x1f),
    warning: Col::Rgb(0x8a, 0x51, 0x00),
    error: Col::Rgb(0xb3, 0x26, 0x1e),
    info: Col::Rgb(0x0f, 0x5f, 0x8f),
    mode_plan: Col::Rgb(0x2d, 0x6a, 0x1f),
    mode_build: Col::Rgb(0x2e, 0x5c, 0xb8),
    mode_bstorm: Col::Rgb(0x8a, 0x51, 0x00),
    diff_add_bg: Col::Rgb(0xdc, 0xf5, 0xe3),
    diff_del_bg: Col::Rgb(0xfb, 0xe0, 0xe3),
    diff_add_emph_bg: Col::Rgb(0xb4, 0xe6, 0xc2),
    diff_del_emph_bg: Col::Rgb(0xf5, 0xbd, 0xc4),
    selection_bg: Col::Rgb(0xc8, 0xd3, 0xf5),
    cursor: Some("#7140b8"),
    wordmark: [
        Col::Rgb(0x1d, 0x3a, 0x8a),
        Col::Rgb(0x2e, 0x5c, 0xb8),
        Col::Rgb(0x31, 0x5a, 0xa8),
        Col::Rgb(0x0f, 0x5f, 0x8f),
    ],
};

// The terminal's own 16 colours and default text, no painted background:
// for terminals with a custom palette, or where 24-bit colour misleads.
const ANSI: Palette = Palette {
    name: "ansi",
    background: None,
    text: Col::Default,
    accent: Col::Ansi(35),
    muted: Col::Ansi(90),
    success: Col::Ansi(32),
    warning: Col::Ansi(33),
    error: Col::Ansi(31),
    info: Col::Ansi(36),
    mode_plan: Col::Ansi(32),
    mode_build: Col::Ansi(34),
    mode_bstorm: Col::Ansi(33),
    diff_add_bg: Col::Default,
    diff_del_bg: Col::Default,
    diff_add_emph_bg: Col::Ansi(32),
    diff_del_emph_bg: Col::Ansi(31),
    selection_bg: Col::Ansi(36),
    cursor: None,
    wordmark: [Col::Ansi(34), Col::Ansi(34), Col::Ansi(36), Col::Ansi(36)],
};

/// The themes `/theme` and the `theme` setting offer, besides "auto".
pub const THEME_NAMES: [&str; 3] = ["dark", "light", "ansi"];

// 0 = not decided yet (first use reads COLORFGBG), else 1 + index into
// THEME_NAMES.
static THEME: AtomicU8 = AtomicU8::new(0);

fn pal() -> &'static Palette {
    let mut t = THEME.load(Ordering::Relaxed);
    if t == 0 {
        t = if colorfgbg_is_light(std::env::var("COLORFGBG").ok().as_deref()) {
            2
        } else {
            1
        };
        THEME.store(t, Ordering::Relaxed);
    }
    match t {
        2 => &LIGHT,
        3 => &ANSI,
        _ => &DARK,
    }
}

/// The theme in use: "dark", "light" or "ansi".
pub fn theme_name() -> &'static str {
    pal().name
}

/// Switch theme: "dark", "light", "ansi", or "auto" (the terminal's
/// background colour when it answers an OSC 11 query, else COLORFGBG, else
/// dark). Returns the theme now in use.
pub fn set_theme(name: &str) -> Result<&'static str, String> {
    let idx = match theme_index(name)? {
        Some(i) => i,
        None => {
            let light = query_background_is_light()
                .unwrap_or_else(|| colorfgbg_is_light(std::env::var("COLORFGBG").ok().as_deref()));
            usize::from(light)
        }
    };
    THEME.store(idx as u8 + 1, Ordering::Relaxed);
    Ok(theme_name())
}

// Index into THEME_NAMES, None for "auto" (or no setting).
fn theme_index(name: &str) -> Result<Option<usize>, String> {
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty() || name == "auto" {
        return Ok(None);
    }
    THEME_NAMES
        .iter()
        .position(|t| *t == name)
        .map(Some)
        .ok_or_else(|| format!("unknown theme {name} — use dark, light, ansi or auto"))
}

// COLORFGBG ("fg;bg", or "fg;default;bg") is set by rxvt, Konsole and some
// others; background 7 or 15 is a light screen.
fn colorfgbg_is_light(v: Option<&str>) -> bool {
    v.and_then(|v| v.rsplit(';').next())
        .and_then(|bg| bg.trim().parse::<u8>().ok())
        .is_some_and(|bg| bg == 7 || bg == 15)
}

// OSC 11 answer: `ESC ] 11 ; rgb:RRRR/GGGG/BBBB` (1-4 hex digits each),
// ended by BEL or ST. Light when its relative luminance is above 0.4.
fn osc11_is_light(reply: &[u8]) -> Option<bool> {
    let text = String::from_utf8_lossy(reply);
    let at = text.find("]11;rgb:")?;
    let body = &text[at + "]11;rgb:".len()..];
    let end = body.find(['\x07', '\x1b']).unwrap_or(body.len());
    let mut channels = body[..end].split('/').map(|h| {
        let h = h.trim();
        let v = u32::from_str_radix(h, 16).ok()?;
        let max = (1u32 << (4 * h.len().clamp(1, 4))) - 1;
        Some(v as f64 / max as f64)
    });
    let (r, g, b) = (channels.next()??, channels.next()??, channels.next()??);
    let lin = |c: f64| {
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    Some(0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b) > 0.4)
}

// Ask the terminal for its background colour. DA1 follows the question:
// every terminal answers it, so a terminal that ignores OSC 11 costs one
// round trip, not the timeout. None without an interactive terminal, in
// line mode, or without an answer.
fn query_background_is_light() -> Option<bool> {
    if line_mode() || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return None;
    }
    let _raw = RawForRead::on();
    let reply = crate::sixel::query(b"\x1b]11;?\x1b\\\x1b[c", Duration::from_millis(250));
    osc11_is_light(&reply)
}

// Line mode (`--plain`, or TERM=dumb): no alternate screen, no cursor
// addressing, no spinner frames; the transcript is printed line by line.
static LINE_MODE: AtomicBool = AtomicBool::new(false);

/// Turn line mode on (`--plain`). TERM=dumb turns it on by itself.
pub fn set_line_mode(on: bool) {
    LINE_MODE.store(on, Ordering::Relaxed);
}

/// Whether the session prints line by line, without screen control.
pub fn line_mode() -> bool {
    LINE_MODE.load(Ordering::Relaxed) || term_is_dumb()
}

fn term_is_dumb() -> bool {
    std::env::var("TERM").is_ok_and(|t| t == "dumb")
}

/// True when colour output is off (`NO_COLOR`); highlighters skip work.
pub fn color_disabled() -> bool {
    no_color()
}

fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some() || !stdout_wants_color() || term_is_dumb()
}

// Piped or redirected output (CI logs, `bwn run … > out.txt`) gets plain text
// unless FORCE_COLOR is set. Decided once: stdout does not change mid-run.
fn stdout_wants_color() -> bool {
    static WANTS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *WANTS.get_or_init(|| {
        cfg!(test) || std::env::var_os("FORCE_COLOR").is_some() || io::stdout().is_terminal()
    })
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

fn sgr_fg(c: Col) -> String {
    match c {
        Col::Rgb(r, g, b) if truecolor() => format!("38;2;{r};{g};{b}"),
        Col::Rgb(r, g, b) => format!("38;5;{}", 16 + 36 * cube(r) + 6 * cube(g) + cube(b)),
        Col::Ansi(n) => n.to_string(),
        Col::Default => "39".to_string(),
    }
}

fn sgr_bg(c: Col) -> String {
    match c {
        Col::Rgb(r, g, b) if truecolor() => format!("48;2;{r};{g};{b}"),
        Col::Rgb(r, g, b) => format!("48;5;{}", 16 + 36 * cube(r) + 6 * cube(g) + cube(b)),
        Col::Ansi(n) => (n + 10).to_string(),
        Col::Default => "49".to_string(),
    }
}

// The theme's painted background, or nothing for themes that keep the
// terminal's own.
fn theme_bg() -> String {
    match pal().background {
        Some(bg) if !no_color() => format!("\x1b[{}m", sgr_bg(bg)),
        _ => String::new(),
    }
}

// Back to plain text after a styled span. On the alternate screen that is
// the theme's text colour on the theme's background, so a reset never
// leaves a hole of terminal-default background in a painted screen.
fn reset_all() -> String {
    if no_color() {
        String::new()
    } else if ALT_SCREEN.load(Ordering::Relaxed) {
        format!("\x1b[0m\x1b[{}m{}", sgr_fg(pal().text), theme_bg())
    } else {
        "\x1b[0m".to_string()
    }
}

fn reset_fg() -> String {
    if no_color() {
        String::new()
    } else if ALT_SCREEN.load(Ordering::Relaxed) {
        format!("\x1b[{}m", sgr_fg(pal().text))
    } else {
        "\x1b[39m".to_string()
    }
}

fn paint(c: Col, s: &str) -> String {
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
        _ => reset_all(),
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
    paint(pal().muted, s)
}
pub fn red(s: &str) -> String {
    paint(pal().error, s)
}
pub fn green(s: &str) -> String {
    paint(pal().success, s)
}
pub fn yellow(s: &str) -> String {
    paint(pal().warning, s)
}
pub fn blue(s: &str) -> String {
    paint(pal().info, s)
}
pub fn cyan(s: &str) -> String {
    paint(pal().info, s)
}
pub fn accent(s: &str) -> String {
    paint(pal().accent, s)
}
pub fn text(s: &str) -> String {
    paint(pal().text, s)
}

// Full-width dim horizontal rule (2-space indent, spans the terminal).
pub fn rule() -> String {
    let w = term_size().0 as usize;
    dim(&format!("  {}", "─".repeat(w.saturating_sub(4))))
}

// Mode-colored badge: PLAN (green), BUILD (blue), BRAINSTORM (amber).
pub fn mode_badge(mode: &str) -> String {
    let (label, color) = match mode {
        "PLAN" => ("PLAN", pal().mode_plan),
        "BRAINSTORM" => ("BRAINSTORM", pal().mode_bstorm),
        _ => ("BUILD", pal().mode_build),
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
    } else if ALT_SCREEN.load(Ordering::Relaxed) && pal().background.is_some() {
        theme_bg()
    } else {
        "\x1b[49m".to_string()
    }
}

fn on_bg(bg: Col, fg: Col, s: &str) -> String {
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
    on_bg(pal().diff_add_bg, pal().success, s)
}
pub fn diff_add_emph_span(s: &str) -> String {
    on_bg(pal().diff_add_emph_bg, pal().text, s)
}
pub fn diff_del_span(s: &str) -> String {
    on_bg(pal().diff_del_bg, pal().error, s)
}
pub fn diff_del_emph_span(s: &str) -> String {
    on_bg(pal().diff_del_emph_bg, pal().text, s)
}

// ── inline images ────────────────────────────────────────────────────────────
// Two tiers. Terminals that render kitty Unicode placeholders (kitty,
// Ghostty, WezTerm with placeholder support) get the real pixels: the PNG
// is uploaded once and the transcript carries placeholder rows that scroll,
// wrap and repaint like text (see graphics.rs). Everywhere else the image
// is drawn as truecolor half-block cells sized to the terminal, which needs
// ffmpeg for the decode. Both paths are gated by the `images` setting.

const IMAGE_EXTS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

fn images_enabled() -> bool {
    IMAGES_MODE.load(Ordering::Relaxed) != 0
}

fn use_placeholders() -> bool {
    match IMAGES_MODE.load(Ordering::Relaxed) {
        2 => true,
        3 | 4 | 0 => false,
        _ => crate::graphics::placeholders_supported(),
    }
}

// Sixel previews: the `images` setting picks them explicitly, or `auto`
// follows BWN_IMAGES and the startup probe.
fn use_sixel() -> bool {
    match IMAGES_MODE.load(Ordering::Relaxed) {
        4 => true,
        2 | 3 | 0 => false,
        _ => crate::sixel::supported(),
    }
}

// ── inline images that follow the terminal size ───────────────────────────────
// An attached image is one transcript line holding `IMG_MARK` and an id.
// Wrapping expands it to as many rows as the image needs at the current
// terminal size, so a resize re-fits it. Block art renders straight into
// those rows. A Sixel image leaves them as `IMG_MARK id:row` placeholders
// and render_output draws the pixels over them.
const IMG_MARK: &str = "\u{F8FF}bwn-img:";

#[derive(Clone, Copy, PartialEq, Debug)]
enum ImageTier {
    Blocks,
    Sixel,
}

struct InlineImage {
    path: std::path::PathBuf,
    native: (u32, u32),
    tier: ImageTier,
    // The terminal size and cell size the renderings below were made for.
    made_for: (usize, usize, (u32, u32)),
    rows: usize,
    blocks: Vec<String>,
    sixel: String,
    // Decoded pixels (width, height, rgb) and cell height behind `sixel`, kept
    // so an image partly scrolled out of view can be drawn cropped.
    pixels: (u32, u32, Vec<u8>),
    cell_h: u32,
    crop: Option<((usize, usize), String)>,
}

impl InlineImage {
    // Re-render for a new terminal size; a no-op when nothing changed.
    fn ensure(&mut self, term_w: usize, term_h: usize) {
        let cell = crate::sixel::cell_pixels();
        if self.made_for == (term_w, term_h, cell) && self.rows > 0 {
            return;
        }
        self.made_for = (term_w, term_h, cell);
        let (max_cols, max_rows) = image_cell_budget_for(term_w, term_h, 4);
        if self.tier == ImageTier::Sixel {
            let (cw, ch) = (cell.0.max(1), cell.1.max(1));
            let (nw, nh) = (self.native.0.max(1), self.native.1.max(1));
            let scale = (f64::from(max_cols as u32 * cw) / f64::from(nw))
                .min(f64::from(max_rows as u32 * ch) / f64::from(nh))
                .min(1.0);
            let w = ((f64::from(nw) * scale).round() as u32).max(1);
            let h = ((f64::from(nh) * scale).round() as u32).max(1);
            if let Some(rgb) = crate::sixel::decode(&self.path, w, h) {
                self.sixel = crate::sixel::encode(&rgb, w as usize, h as usize);
                self.rows = h.div_ceil(ch) as usize;
                self.pixels = (w, h, rgb);
                self.cell_h = ch;
                self.crop = None;
                return;
            }
            self.tier = ImageTier::Blocks;
        }
        let px_rows = ((max_rows * 2) as u32).max(2);
        self.blocks = crate::media::decode_thumbnail(&self.path, max_cols as u32, px_rows)
            .map(|(w, h, rgb)| image_preview_cells(&rgb, w, h))
            .unwrap_or_default();
        self.rows = self.blocks.len().max(1);
    }
}

impl InlineImage {
    // Sixel for image rows `first..first + count`: the whole image, or the
    // visible slice of one partly scrolled out of view.
    fn sixel_rows(&mut self, first: usize, count: usize) -> String {
        if first == 0 && count >= self.rows {
            return self.sixel.clone();
        }
        if let Some((key, data)) = &self.crop {
            if *key == (first, count) {
                return data.clone();
            }
        }
        let (w, h, rgb) = (&self.pixels.0, &self.pixels.1, &self.pixels.2);
        let (w, h) = (*w as usize, *h as usize);
        let ch = self.cell_h.max(1) as usize;
        let top = (first * ch).min(h);
        let bottom = ((first + count) * ch).min(h);
        if bottom <= top || rgb.len() < w * h * 3 {
            return String::new();
        }
        let data = crate::sixel::encode(&rgb[top * w * 3..bottom * w * 3], w, bottom - top);
        self.crop = Some(((first, count), data.clone()));
        data
    }
}

fn inline_images() -> &'static Mutex<Vec<InlineImage>> {
    static IMAGES: OnceLock<Mutex<Vec<InlineImage>>> = OnceLock::new();
    IMAGES.get_or_init(|| Mutex::new(Vec::new()))
}

// Set when something other than render_output may have drawn over the
// output rows (a popup, a clear, a resize): the next frame redraws every
// Sixel image instead of keeping the ones that did not move.
static SIXEL_DIRTY: AtomicBool = AtomicBool::new(true);

fn invalidate_inline_pixels() {
    SIXEL_DIRTY.store(true, Ordering::Relaxed);
}

// Sixel images drawn by the last frame.
fn drawn_sixels() -> &'static Mutex<Vec<Placement>> {
    static DRAWN: OnceLock<Mutex<Vec<Placement>>> = OnceLock::new();
    DRAWN.get_or_init(|| Mutex::new(Vec::new()))
}

// Rows for an image marker line at the current size, or None when `line`
// is not a registered image (a row placeholder, or an unknown id).
fn inline_image_rows(line: &str, width: usize) -> Option<Vec<String>> {
    let id: usize = line.strip_prefix(IMG_MARK)?.parse().ok()?;
    let height = term_size().1 as usize;
    let mut images = inline_images().lock().ok()?;
    let img = images.get_mut(id)?;
    img.ensure(width, height);
    Some(match img.tier {
        ImageTier::Blocks if img.blocks.is_empty() => vec![String::new()],
        ImageTier::Blocks => img.blocks.clone(),
        ImageTier::Sixel => (0..img.rows)
            .map(|r| format!("{IMG_MARK}{id}:{r}"))
            .collect(),
    })
}

// A Sixel row placeholder: (image id, row within the image).
fn sixel_row(line: &str) -> Option<(usize, usize)> {
    let (id, row) = line.strip_prefix(IMG_MARK)?.split_once(':')?;
    Some((id.parse().ok()?, row.parse().ok()?))
}

// The visible part of each Sixel image: (id, first screen row, first image
// row, rows). An image partly scrolled out of view is drawn cropped.
fn sixel_placements(visible: &[&String], rows: usize) -> Vec<Placement> {
    let mut out = Vec::new();
    let mut screen_row = 0;
    while screen_row < visible.len().min(rows) {
        let Some((id, first)) = sixel_row(visible[screen_row]) else {
            screen_row += 1;
            continue;
        };
        let mut n = 1;
        while screen_row + n < rows
            && visible
                .get(screen_row + n)
                .and_then(|l| sixel_row(l))
                .is_some_and(|(i, r)| i == id && r == first + n)
        {
            n += 1;
        }
        out.push((id, screen_row, first, n));
        screen_row += n;
    }
    out
}

type Placement = (usize, usize, usize, usize);

// Cell budget for an inline image: a thumbnail, not a full-screen picture.
// At most half the width (80 columns) and a third of the rows above the
// composer (16 rows), so the conversation stays in view; the image keeps its
// aspect ratio inside that box.
fn image_cell_budget() -> (usize, usize) {
    let (w, h) = term_size();
    image_cell_budget_for(w as usize, h as usize, reserved_rows() as usize)
}

fn image_cell_budget_for(width: usize, height: usize, reserved: usize) -> (usize, usize) {
    let cols = (width.saturating_sub(4) / 2).clamp(8, 80);
    let rows = (height.saturating_sub(reserved + 4) / 3).clamp(4, 16);
    (cols, rows)
}

fn free_images() {
    invalidate_inline_pixels();
    if IMAGES_SENT.swap(false, Ordering::Relaxed) {
        let _ = write!(io::stdout(), "{}", crate::graphics::delete_all());
        flush();
    }
}

/// Show `path` (an image, or a video's first frame via ffmpeg) inline in
/// the transcript. With `once`, a path already shown this session is
/// skipped (a pasted screenshot previews at paste time and must not repeat
/// on submit). Returns whether anything was drawn.
pub fn show_image_file(path: &std::path::Path, once: bool) -> bool {
    if !images_enabled() || no_color() {
        return false;
    }
    if let Ok(mut seen) = previewed_paths().lock() {
        if !seen.insert(path.to_path_buf()) && once {
            return true;
        }
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string());
    let name = sanitize_terminal(&name).into_owned();
    let link = file_link(&path.display().to_string(), &name);
    let (max_cols, max_rows) = image_cell_budget();
    if ALT_SCREEN.load(Ordering::Relaxed) && use_placeholders() {
        if let Some((png, iw, ih)) = crate::media::png_for_terminal(path, 1600) {
            let cell = crate::graphics::cell_pixels();
            let (cols, rows) = crate::graphics::fit_cells(iw, ih, max_cols, max_rows, cell);
            let id = NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed);
            let _ = write!(
                io::stdout(),
                "{}",
                crate::graphics::transmit(&png, id, cols, rows)
            );
            flush();
            IMAGES_SENT.store(true, Ordering::Relaxed);
            let mut rows_out = vec![format!(
                "  {} {} {}",
                dim("⎘"),
                link,
                dim(&format!("· {iw}×{ih}"))
            )];
            rows_out.extend(crate::graphics::placeholder_rows(id, cols, rows, 2));
            line(&rows_out.join("\n"));
            return true;
        }
    }
    // Real pixels over Sixel where the terminal has it, else block art. In
    // the TUI the image is one marker line that re-renders at every terminal
    // size; outside it, the image is printed once at the current size.
    let sixel = use_sixel() && crate::media::ffmpeg_available();
    if sixel || (ALT_SCREEN.load(Ordering::Relaxed) && truecolor()) {
        if let Some(native) = crate::media::probe_dims(path) {
            let mut img = InlineImage {
                path: path.to_path_buf(),
                native,
                tier: if sixel {
                    ImageTier::Sixel
                } else {
                    ImageTier::Blocks
                },
                made_for: (0, 0, (0, 0)),
                rows: 0,
                blocks: Vec::new(),
                sixel: String::new(),
                pixels: (0, 0, Vec::new()),
                cell_h: 0,
                crop: None,
            };
            let (w, h) = term_size();
            img.ensure(w as usize, h as usize);
            let header = format!(
                "  {} {} {}",
                dim("⎘"),
                link,
                dim(&format!("· {}×{}", native.0, native.1))
            );
            let drawable = match img.tier {
                ImageTier::Sixel => !img.sixel.is_empty(),
                ImageTier::Blocks => !img.blocks.is_empty(),
            };
            if drawable && ALT_SCREEN.load(Ordering::Relaxed) {
                let id = inline_images().lock().ok().map(|mut imgs| {
                    imgs.push(img);
                    imgs.len() - 1
                });
                if let Some(id) = id {
                    line(&format!("{header}\n{IMG_MARK}{id}"));
                    return true;
                }
            } else if drawable && img.tier == ImageTier::Sixel {
                line(&header);
                print!("  {}{}", img.sixel, if is_raw() { "\r\n" } else { "\n" });
                flush();
                return true;
            }
        }
    }
    // Half-block fallback: one text row shows two pixel rows.
    if !truecolor() {
        return false;
    }
    let px_rows = ((max_rows * 2) as u32).max(2);
    if let Some((tw, th, rgb)) = crate::media::decode_thumbnail(path, max_cols as u32, px_rows) {
        let rows = image_preview_cells(&rgb, tw, th);
        if !rows.is_empty() {
            let mut out = vec![format!("  {} {}", dim("⎘"), link)];
            out.extend(rows);
            line(&out.join("\n"));
            return true;
        }
    }
    false
}

/// `@path` token for the composer, quoted when the path needs it (shlex
/// splits the prompt at submit time).
pub fn attachment_token(path: &std::path::Path) -> String {
    let p = path.display().to_string();
    if p.chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\'')
    {
        format!("@\"{}\" ", p.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        format!("@{p} ")
    }
}

/// A pasted (or drag-and-dropped) single file path to an image or video:
/// quotes, `file://`, shell escapes and `~` stripped; must exist. Anything
/// else — prose, multi-line text, non-media files — is None.
pub fn pasted_media_path(text: &str) -> Option<std::path::PathBuf> {
    let t = text.trim();
    if t.is_empty() || t.len() > 4096 || t.contains('\n') || t.contains('\r') {
        return None;
    }
    let mut t = t.to_string();
    for q in ['"', '\''] {
        if t.len() >= 2 && t.starts_with(q) && t.ends_with(q) {
            t = t[1..t.len() - 1].to_string();
        }
    }
    if let Some(rest) = t.strip_prefix("file://") {
        t = rest.to_string();
    }
    // Shell-escaped spaces from a drag-and-drop (`My\ Shot.png`).
    if t.contains("\\ ") {
        t = t.replace("\\ ", " ");
    }
    if let Some(rest) = t.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            t = std::path::PathBuf::from(home)
                .join(rest)
                .display()
                .to_string();
        }
    }
    let path = std::path::PathBuf::from(&t);
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if !IMAGE_EXTS.contains(&ext.as_str()) && !crate::media::VIDEO_EXTS.contains(&ext.as_str()) {
        return None;
    }
    path.is_file().then_some(path)
}

// ── inline image preview ─────────────────────────────────────────────────────
// Renders RGB pixels as half-block cells: each character shows two vertically
// stacked pixels ('▀' with fg = top pixel, bg = bottom pixel). Works in every
// truecolor terminal — no kitty/iTerm2 graphics protocol needed — and the
// result is ordinary transcript lines, so scrolling, wrapping, and repaints
// all behave. Returns no lines when color is off or the buffer is malformed.
pub fn image_preview(rgb: &[u8], w: u32, h: u32) -> Vec<String> {
    if no_color() || !truecolor() {
        return Vec::new();
    }
    image_preview_cells(rgb, w, h)
}

// Pure renderer (no terminal-capability gate) — unit-testable.
fn image_preview_cells(rgb: &[u8], w: u32, h: u32) -> Vec<String> {
    let (w, h) = (w as usize, h as usize);
    if w == 0 || h == 0 || rgb.len() < w * h * 3 {
        return Vec::new();
    }
    let px = |x: usize, y: usize| {
        let i = (y * w + x) * 3;
        (rgb[i], rgb[i + 1], rgb[i + 2])
    };
    let mut out = Vec::with_capacity(h.div_ceil(2));
    for row in 0..h.div_ceil(2) {
        let mut line = String::with_capacity(w * 24 + 16);
        line.push_str("  ");
        for x in 0..w {
            let (tr, tg, tb) = px(x, row * 2);
            if row * 2 + 1 < h {
                let (br, bg, bb) = px(x, row * 2 + 1);
                line.push_str(&format!("\x1b[38;2;{tr};{tg};{tb};48;2;{br};{bg};{bb}m▀"));
            } else {
                line.push_str(&format!("\x1b[38;2;{tr};{tg};{tb}m▀"));
            }
        }
        line.push_str(&reset_all());
        line.push_str(&reset_bg());
        out.push(line);
    }
    out
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
    // A pipe table being collected; rendered as one aligned block when the
    // first non-table line (or the end of the stream) arrives.
    InTable { rows: Vec<String> },
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
        self.pending.push_str(&sanitize_terminal(chunk));
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

    fn finish_state(&mut self) {
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

fn is_table_row(l: &str) -> bool {
    let t = l.trim();
    t.len() > 1 && t.starts_with('|') && t[1..].contains('|')
}

fn split_table_cells(l: &str) -> Vec<String> {
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
enum Align {
    Left,
    Center,
    Right,
}

// `|---|:--:|--:|` → per-column alignment, or None when this isn't a separator.
fn table_separator(cells: &[String]) -> Option<Vec<Align>> {
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

fn render_table(rows: &[String], w: usize) -> Vec<String> {
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

// True only for terminals known to drop OSC 52: plain `xterm` (allowWindowOps
// is off by default) and VTE-based ones (GNOME Terminal & co.) that don't
// announce themselves via TERM_PROGRAM. Anything else — including an
// unadorned xterm-256color, which tmux/ssh/most emulators present — is
// "unsure" and keeps the copy confirmation. On WSL and macOS the copy also
// goes through clip.exe / pbcopy, so it works regardless of the terminal.
fn osc52_known_unsupported_for(term: &str, term_program: Option<&str>, vte: bool) -> bool {
    if term_program.is_some_and(|p| !p.is_empty()) {
        return false;
    }
    if crate::tools::is_wsl() || cfg!(target_os = "macos") {
        return false;
    }
    vte || term == "xterm" || term == "linux"
}

fn osc52_known_unsupported() -> bool {
    let term = std::env::var("TERM").unwrap_or_default();
    let term_program = std::env::var("TERM_PROGRAM").ok();
    let vte = std::env::var_os("VTE_VERSION").is_some();
    osc52_known_unsupported_for(&term, term_program.as_deref(), vte)
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
    // Native Windows and WSL both have clip.exe; the terminal may not honor
    // OSC 52, so mirror the text into the system clipboard as well.
    if cfg!(windows) || crate::tools::is_wsl() {
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

// The queue is FIFO: ask_task sends index 0 next, so Ctrl+Q / Ctrl+X must act
// on the front too — never `pop()`, which would edit or drop the newest
// message while the oldest one goes out untouched.
fn queue_take_next(mq: &mut Vec<String>) -> Option<String> {
    if mq.is_empty() {
        None
    } else {
        Some(mq.remove(0))
    }
}

// Put a message pulled out via Ctrl+Q back at the front so it is still the
// one sent next (Enter after editing, or Esc to abandon the edit).
fn queue_put_back(mq: &mut Vec<String>, msg: String) {
    mq.insert(0, msg);
}

/// Non-blocking drain of pending key events during agent processing.
/// Buffers printable input; Ctrl+C clears the buffer and signals an interrupt.
pub fn poll_typeahead() {
    if !is_raw() {
        return;
    }
    drain_typeahead();
    render_queued_composer();
}

// ── keyboard ownership ───────────────────────────────────────────────────────
// One reader owns the keyboard at a time. The typeahead drain takes it for a
// non-blocking pass; an open prompt, picker or composer holds it until it
// closes, so the drain can never take a key typed into an `allow?` prompt.
// Held per thread (like the render lock) so a prompt opened inside another
// one does not deadlock on itself.
static KEYBOARD: Mutex<()> = Mutex::new(());

thread_local! {
    static KEYBOARD_HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct KeyboardGuard(Option<std::sync::MutexGuard<'static, ()>>);

impl Drop for KeyboardGuard {
    fn drop(&mut self) {
        if self.0.is_some() {
            KEYBOARD_HELD.with(|h| h.set(false));
        }
    }
}

fn keyboard_lock() -> KeyboardGuard {
    if KEYBOARD_HELD.with(|h| h.get()) {
        return KeyboardGuard(None);
    }
    let g = KEYBOARD.lock().unwrap_or_else(|e| e.into_inner());
    KEYBOARD_HELD.with(|h| h.set(true));
    KeyboardGuard(Some(g))
}

fn keyboard_try_lock() -> Option<KeyboardGuard> {
    if KEYBOARD_HELD.with(|h| h.get()) {
        return Some(KeyboardGuard(None));
    }
    let g = match KEYBOARD.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return None,
    };
    KEYBOARD_HELD.with(|h| h.set(true));
    Some(KeyboardGuard(Some(g)))
}

// What the open prompt or picker shows in the input box: its label, its
// text and cursor. Every composer repaint draws the newest entry instead of
// the idle `›` composer, so a repaint from another thread (the typeahead
// thread, a footer tick) can never relabel an open `allow?` prompt.
#[derive(Clone)]
struct OpenInput {
    prompt: String,
    buf: Vec<char>,
    cursor: usize,
}

fn open_inputs() -> &'static Mutex<Vec<OpenInput>> {
    static OPEN: OnceLock<Mutex<Vec<OpenInput>>> = OnceLock::new();
    OPEN.get_or_init(|| Mutex::new(Vec::new()))
}

fn top_open_input() -> Option<OpenInput> {
    open_inputs().lock().ok().and_then(|o| o.last().cloned())
}

fn update_open_input(prompt: &str, buf: &[char], cursor: usize) {
    if let Ok(mut open) = open_inputs().lock() {
        if let Some(top) = open.last_mut() {
            top.prompt.clear();
            top.prompt.push_str(prompt);
            top.buf.clear();
            top.buf.extend_from_slice(buf);
            top.cursor = cursor;
        }
    }
}

/// Registration of an open prompt, picker or composer: owns the keyboard
/// and the input box until dropped.
struct InputOwner {
    _keys: KeyboardGuard,
}

impl InputOwner {
    fn open(prompt: &str, buf: &[char], cursor: usize) -> Self {
        let keys = keyboard_lock();
        if let Ok(mut open) = open_inputs().lock() {
            open.push(OpenInput {
                prompt: prompt.to_string(),
                buf: buf.to_vec(),
                cursor,
            });
        }
        InputOwner { _keys: keys }
    }
}

impl Drop for InputOwner {
    fn drop(&mut self) {
        if let Ok(mut open) = open_inputs().lock() {
            open.pop();
        }
    }
}

// Set when a read from the terminal failed (input closed): callers that
// would otherwise ask again (a quit confirmation) stop asking.
static INPUT_CLOSED: AtomicBool = AtomicBool::new(false);

/// True once reading the keyboard has failed — stdin closed or the
/// terminal went away. A `None` from a prompt is then not a person's Esc.
pub fn input_closed() -> bool {
    INPUT_CLOSED.load(Ordering::Relaxed)
}

// Returns whether any event was read. The typeahead thread and the main
// thread (via interrupted()) both drain; one at a time, so typed keys can't
// be applied out of order. A busy drain means the other side has the events,
// or a prompt owns the keyboard.
fn drain_typeahead() -> bool {
    let Some(_keys) = keyboard_try_lock() else {
        return false;
    };
    if top_open_input().is_some() {
        // This thread holds an open prompt (an interrupt check from inside
        // it): the prompt reads its own keys.
        return false;
    }
    let mut any = false;
    while poll(Duration::ZERO).unwrap_or(false) {
        match read() {
            Ok(ev) => {
                typeahead_event(ev, is_agent_running());
            }
            Err(_) => break,
        }
        any = true;
    }
    any
}

// Applies one terminal event read by poll_typeahead: Ctrl-C/Esc raise the
// interrupt flag, everything else edits the type-ahead buffer. Split out so
// the buffering is testable without a terminal.
fn typeahead_event(ev: Event, agent_running: bool) -> InterruptKind {
    let mut raised = InterruptKind::None;
    match ev {
        Event::Key(k) => {
            if k.kind == KeyEventKind::Release {
                return raised;
            }
            let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
            let alt = k.modifiers.contains(KeyModifiers::ALT);
            match k.code {
                KeyCode::PageUp => {
                    scroll_page_up();
                    return raised;
                }
                KeyCode::PageDown => {
                    scroll_page_down();
                    return raised;
                }
                KeyCode::Up if alt => {
                    scroll_output(1);
                    return raised;
                }
                KeyCode::Down if alt => {
                    scroll_output(-1);
                    return raised;
                }
                KeyCode::Home if alt => {
                    scroll_output(isize::MAX / 4);
                    return raised;
                }
                KeyCode::End if alt => {
                    scroll_to_bottom();
                    clear_composer();
                    render_footer();
                    return raised;
                }
                _ => {}
            }
            let mut ta = match typeahead().lock() {
                Ok(g) => g,
                Err(_) => return raised,
            };
            match k.code {
                KeyCode::Enter => {
                    let text: String = ta.buf.iter().collect();
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        if let Ok(mut mq) = message_queue().lock() {
                            if ta.from_queue {
                                queue_put_back(&mut mq, trimmed.to_string());
                            } else {
                                mq.push(trimmed.to_string());
                            }
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
                    return raised;
                }
                // Plain Up is intentionally a no-op for the queue: queue
                // editing is Ctrl+Q (as the queued-row hint says), so a
                // stray Up can't destructively pop the newest message.
                KeyCode::Up if !alt => {
                    return raised;
                }
                KeyCode::Char('q') if ctrl => {
                    // Take the NEXT message (front) under a short-lived
                    // lock, then render with no guards held (see deadlock
                    // note on Enter above).
                    let next = message_queue()
                        .lock()
                        .ok()
                        .and_then(|mut mq| queue_take_next(&mut mq));
                    if let Some(next) = next {
                        ta.buf = next.chars().collect();
                        ta.cursor = ta.buf.len();
                        ta.from_queue = true;
                        drop(ta);
                        render_output();
                        clear_composer();
                        render_footer();
                        render_queued_composer();
                    }
                    return raised;
                }
                KeyCode::Char('x') if ctrl => {
                    let removed = message_queue()
                        .lock()
                        .ok()
                        .and_then(|mut mq| queue_take_next(&mut mq))
                        .is_some();
                    if removed {
                        drop(ta);
                        render_output();
                        clear_composer();
                        render_footer();
                        render_queued_composer();
                    }
                    return raised;
                }
                KeyCode::Char('c') if ctrl => {
                    raised = InterruptKind::CtrlC;
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
                    if agent_running {
                        raised = InterruptKind::Escape;
                    } else if ta.from_queue && !ta.buf.is_empty() {
                        let msg: String = ta.buf.iter().collect();
                        if let Ok(mut mq) = message_queue().lock() {
                            queue_put_back(&mut mq, msg);
                        }
                        ta.buf.clear();
                        ta.cursor = 0;
                        ta.from_queue = false;
                        drop(ta);
                        render_output();
                        clear_composer();
                        render_footer();
                        render_queued_composer();
                        return raised;
                    } else if !ta.buf.is_empty() {
                        ta.buf.clear();
                        ta.cursor = 0;
                        ta.from_queue = false;
                    } else {
                        raised = InterruptKind::Escape;
                    }
                }
                KeyCode::Home | KeyCode::Char('a') if ctrl => {
                    ta.cursor = 0;
                }
                KeyCode::End | KeyCode::Char('e') if ctrl => {
                    ta.cursor = ta.buf.len();
                }
                KeyCode::Char('w') if ctrl => {
                    let d = ta.cursor;
                    while ta.cursor > 0 && ta.buf[ta.cursor - 1] == ' ' {
                        ta.cursor -= 1;
                    }
                    while ta.cursor > 0 && ta.buf[ta.cursor - 1] != ' ' {
                        ta.cursor -= 1;
                    }
                    let cur = ta.cursor;
                    ta.buf.drain(cur..d);
                }
                KeyCode::Char('k') if ctrl => {
                    let cur = ta.cursor;
                    ta.buf.truncate(cur);
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
        Event::Paste(s) => {
            // A dropped/pasted image or video path becomes an @attachment
            // token (and previews at once, even mid-turn).
            let media = pasted_media_path(&s);
            let text = match &media {
                Some(p) => attachment_token(p),
                None => collapse_paste(&s).unwrap_or_else(|| s.clone()),
            };
            let mut ta = match typeahead().lock() {
                Ok(g) => g,
                Err(_) => return raised,
            };
            let chars = sanitize_paste(&text);
            let i = ta.cursor;
            ta.buf.splice(i..i, chars.iter().copied());
            ta.cursor += chars.len();
            drop(ta);
            if let Some(p) = media {
                show_image_file(&p, false);
            }
        }
        Event::Mouse(m) => match m.kind {
            MouseEventKind::ScrollUp => scroll_output(3),
            MouseEventKind::ScrollDown => scroll_output(-3),
            MouseEventKind::Down(MouseButton::Left) => selection_start(m.row, m.column),
            MouseEventKind::Drag(MouseButton::Left) => selection_drag(m.row, m.column),
            MouseEventKind::Up(MouseButton::Left) => selection_finish(m.row, m.column),
            _ => {}
        },
        Event::Resize(_, _) => {
            if ALT_SCREEN.load(Ordering::Relaxed) {
                set_output_region();
                invalidate_inline_pixels();
                render_output();
                render_footer();
                render_queued_composer();
            }
        }
        Event::FocusGained => FOCUSED.store(true, Ordering::Relaxed),
        Event::FocusLost => FOCUSED.store(false, Ordering::Relaxed),
    }
    if raised != InterruptKind::None {
        trigger_interrupt(raised);
    }
    raised
}

// ── render lock ──────────────────────────────────────────────────────────────
// The typeahead and spinner threads paint while the main thread streams, so
// every frame (render_output, render_queued_composer, render_footer,
// render_composer) runs under one process-wide lock. Re-entrant per thread,
// since render_output ends with render_queued_composer, which calls
// render_composer. Lock order: RENDER → typeahead → message_queue → the leaf
// locks (transcript, selection, visible_rows, footer/model/flash text). Never
// call a render function while holding typeahead or message_queue.
static RENDER: Mutex<()> = Mutex::new(());

thread_local! {
    static RENDER_HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct RenderGuard(Option<std::sync::MutexGuard<'static, ()>>);

impl Drop for RenderGuard {
    fn drop(&mut self) {
        if self.0.is_some() {
            RENDER_HELD.with(|h| h.set(false));
        }
    }
}

fn render_lock() -> RenderGuard {
    if RENDER_HELD.with(|h| h.get()) {
        return RenderGuard(None);
    }
    let g = RENDER.lock().unwrap_or_else(|e| e.into_inner());
    RENDER_HELD.with(|h| h.set(true));
    RenderGuard(Some(g))
}

// Hand a fully built frame to the terminal in one write.
fn write_frame(frame: &[u8]) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(frame);
    let _ = out.flush();
}

// The composer is one line: pasted line breaks and tabs become spaces, and
// every other control char (C0, DEL, C1, so no escape sequences) is dropped.
fn sanitize_paste(s: &str) -> Vec<char> {
    s.chars()
        .filter_map(|c| match c {
            '\n' | '\r' | '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

// ── large pastes ─────────────────────────────────────────────────────────────
// A paste over these limits shows in the composer as one `[pasted 20,024
// chars]` token, and the message carries the whole paste, line breaks kept,
// when it is sent. Smaller pastes go in as text (flattened to one line).
const PASTE_COLLAPSE_CHARS: usize = 1_000;
const PASTE_COLLAPSE_LINES: usize = 10;

// Every collapsed paste of the session, by its token: a history recall of a
// message with a token still sends the paste.
fn pastes() -> &'static Mutex<Vec<(String, String)>> {
    static PASTES: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();
    PASTES.get_or_init(|| Mutex::new(Vec::new()))
}

// 20024 → "20,024".
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// A paste as the message will carry it: CRLF and CR become LF, tabs stay,
// and every other control character (escape sequences) is dropped.
fn clean_paste(s: &str) -> String {
    s.replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect()
}

/// The token a large paste is shown as, after storing the paste under it;
/// None for a paste small enough to type in as text.
fn collapse_paste(s: &str) -> Option<String> {
    let text = clean_paste(s);
    let chars = text.chars().count();
    if chars <= PASTE_COLLAPSE_CHARS && text.lines().count() <= PASTE_COLLAPSE_LINES {
        return None;
    }
    let mut store = pastes().lock().ok()?;
    let token = match store.len() {
        0 => format!("[pasted {} chars]", thousands(chars)),
        n => format!("[pasted {} chars #{}]", thousands(chars), n + 1),
    };
    store.push((token.clone(), text));
    Some(token)
}

/// A submitted message with each paste token replaced by its paste.
fn expand_pastes(text: &str) -> String {
    let Ok(store) = pastes().lock() else {
        return text.to_string();
    };
    let mut out = text.to_string();
    for (token, full) in store.iter() {
        if out.contains(token.as_str()) {
            out = out.replacen(token.as_str(), full, 1);
        }
    }
    out
}

// Only the front row (sent next) carries the edit/remove hint: Ctrl+Q and
// Ctrl+X act on that message, not on whichever row was queued last.
fn queued_row_hint(index: usize) -> &'static str {
    if index == 0 {
        "(next — Ctrl+Q edit, Ctrl+X rm)"
    } else {
        ""
    }
}

pub fn render_queued_composer() {
    if !is_raw() || !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    let _frame = render_lock();
    // The queued rows live just above the composer, inside the reserved area —
    // make sure the scroll region already excludes them so they can't be
    // scrolled away between now and the next stream frame.
    ensure_output_region();
    let open = top_open_input();
    if let Ok(ta) = typeahead().lock() {
        let has_queued = if let Ok(mq) = message_queue().lock() {
            let mut out: Vec<u8> = Vec::new();
            // One atomic frame (DEC 2026): the queued rows paint together with
            // no intermediate state a fast terminal could show mid-repaint.
            let _ = write!(out, "\x1b[?2026h");
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
                    dim(queued_row_hint(i))
                );
            }
            let _ = write!(out, "\x1b[?2026l");
            write_frame(&out);
            !mq.is_empty()
        } else {
            false
        };
        let mut scroll = 0usize;
        if let Some(open) = open {
            // A prompt or picker owns the box: repaint it as it is, never
            // the idle composer.
            render_composer(&open.prompt, &open.buf, open.cursor, &mut scroll);
            cursor_show();
            return;
        }
        let prompt_str = if is_agent_running() && (!ta.buf.is_empty() || has_queued) {
            format!("{} {} ", dim("queued"), accent("›"))
        } else {
            format!("{} ", accent("›"))
        };
        render_composer(&prompt_str, &ta.buf, ta.cursor, &mut scroll);
        cursor_color_accent();
        set_cursor_shape(CursorShape::Bar);
        cursor_show();
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
        || (u > 0x036F && u < 0x1000 && crate::graphics::is_diacritic(c))
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
        if c == '~' && i + 1 < n && chars[i + 1] == '~' {
            // ~~strike~~: styled only when a closing pair follows.
            if let Some(rel) = chars[i + 2..]
                .windows(2)
                .position(|w| w[0] == '~' && w[1] == '~')
            {
                if rel > 0 {
                    flush_styled_run(&mut out, &mut buf, bold_on, italic_on);
                    let inner: String = chars[i + 2..i + 2 + rel].iter().collect();
                    out.push_str(&format!("\x1b[9m{}\x1b[29m", dim(&inner)));
                    i += rel + 4;
                    continue;
                }
            }
            buf.push('~');
            buf.push('~');
            i += 2;
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
    // Thematic break: three or more of the same rule character.
    if trimmed.len() >= 3 {
        let mut it = trimmed.chars().filter(|c| !c.is_whitespace());
        if let Some(first) = it.next() {
            if matches!(first, '-' | '*' | '_')
                && it.all(|c| c == first)
                && trimmed.chars().filter(|c| *c == first).count() >= 3
            {
                let wd = term_size().0 as usize;
                return format!("  {}", dim(&"─".repeat(wd.saturating_sub(4).clamp(8, 72))));
            }
        }
    }
    if let Some(header) = trimmed.strip_prefix("#### ") {
        return format!("{}{}", indent, bold(&format_inline_md(header)));
    }
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
    let (prefix_span, rest) = if let Some(r) = trimmed
        .strip_prefix("- [x] ")
        .or_else(|| trimmed.strip_prefix("* [x] "))
        .or_else(|| trimmed.strip_prefix("- [X] "))
    {
        (Some(green("☑")), r)
    } else if let Some(r) = trimmed
        .strip_prefix("- [ ] ")
        .or_else(|| trimmed.strip_prefix("* [ ] "))
    {
        (Some(dim("☐")), r)
    } else if let Some(r) = trimmed.strip_prefix("- ") {
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
// Inline markdown for subdued/meta text (the thinking stream): renders
// **bold**, *italic*, and `code` as real styling while keeping the whole line
// muted. It relies on bold()/italic()/underline() using attribute-only resets
// (22/23/24) that never touch the foreground color, so the MUTED base set by
// the caller's paint() survives across every styled span — no color flashes
// back to bright mid-line.
fn format_inline_md_dim(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(text.len() + 16);
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if c == '`' {
            if let Some(rel) = chars[i + 1..].iter().position(|&x| x == '`') {
                let code: String = chars[i + 1..i + 1 + rel].iter().collect();
                // Underline (not a color) marks code so the line stays muted.
                out.push_str(&underline(&code));
                i += rel + 2;
                continue;
            }
        }
        if c == '*' && i + 1 < n && chars[i + 1] == '*' {
            if let Some(rel) = find_closer(&chars, i + 2, "**") {
                let inner: String = chars[i + 2..i + 2 + rel].iter().collect();
                out.push_str(&bold(&format_inline_md_dim(&inner)));
                i += 2 + rel + 2;
                continue;
            }
        }
        if c == '*' {
            if let Some(rel) = find_closer(&chars, i + 1, "*") {
                let inner: String = chars[i + 1..i + 1 + rel].iter().collect();
                out.push_str(&italic(&inner));
                i += 1 + rel + 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

// Index (relative to `from`) of the next `marker` that closes a span: it must
// exist and not open on whitespace. Returns None if the span never closes, so
// an unmatched `*` stays literal.
fn find_closer(chars: &[char], from: usize, marker: &str) -> Option<usize> {
    let m: Vec<char> = marker.chars().collect();
    if from >= chars.len() || chars[from].is_whitespace() {
        return None;
    }
    let mut j = from;
    while j + m.len() <= chars.len() {
        if chars[j..j + m.len()] == m[..] && j > from && !chars[j - 1].is_whitespace() {
            return Some(j - from);
        }
        j += 1;
    }
    None
}

/// One line of thinking-stream markdown: block markers (headings, list
/// bullets, quotes) and inline styling become real formatting, all kept in the
/// muted thinking palette so the reasoning stays visually quiet.
pub fn render_md_dim_line(s: &str) -> String {
    let s = &*sanitize_terminal(s);
    if no_color() {
        return s.to_string();
    }
    let trimmed = s.trim_start();
    let indent = &s[..s.len() - trimmed.len()];
    let inner = if let Some(h) = trimmed
        .strip_prefix("### ")
        .or_else(|| trimmed.strip_prefix("## "))
        .or_else(|| trimmed.strip_prefix("# "))
    {
        bold(&format_inline_md_dim(h))
    } else if let Some(q) = trimmed.strip_prefix("> ") {
        format!("│ {}", italic(&format_inline_md_dim(q)))
    } else if let Some(b) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
    {
        format!("• {}", format_inline_md_dim(b))
    } else {
        format_inline_md_dim(trimmed)
    };
    // Paint the whole assembled line MUTED once; the attribute toggles inside
    // never reset the color, so it stays dim end to end.
    paint(pal().muted, &format!("{indent}{inner}"))
}

pub fn render_md(text: &str) -> String {
    let text = &*sanitize_terminal(text);
    let w = term_size().0 as usize;
    let mut out: Vec<String> = Vec::new();
    // Some(lang) while inside a fenced code block.
    let mut fence: Option<String> = None;
    let mut code: Vec<String> = Vec::new();
    let mut table: Vec<String> = Vec::new();
    let flush_code = |lang: &str, code: &mut Vec<String>, out: &mut Vec<String>| {
        let lit = crate::highlight::highlight_block(lang, code);
        out.extend(lit.iter().map(|l| code_box_line(l)));
        out.push(code_box_footer(w));
        code.clear();
    };
    for l in text.lines() {
        match fence {
            Some(ref lang) => {
                if l.trim() == "```" {
                    flush_code(lang, &mut code, &mut out);
                    fence = None;
                } else {
                    code.push(l.to_string());
                }
            }
            None => {
                if is_table_row(l) {
                    table.push(l.to_string());
                    continue;
                }
                if !table.is_empty() {
                    out.extend(render_table(&table, w));
                    table.clear();
                }
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
    if !table.is_empty() {
        out.extend(render_table(&table, w));
    }
    // Unclosed fence: close the border so the block doesn't bleed on.
    if let Some(lang) = fence {
        flush_code(&lang, &mut code, &mut out);
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
    // OSC (ESC ]) strings end at BEL or ST (ESC \); APC (ESC _, kitty
    // graphics), DCS (ESC P, tmux passthrough / sixel), PM and SOS end at
    // ST only. Treating them as CSI would stop at the first letter and leak
    // a base64 payload into the visible text.
    let string_start = matches!(
        chars.peek(),
        Some(']') | Some('_') | Some('P') | Some('^') | Some('X')
    );
    if string_start {
        let osc = chars.peek() == Some(&']');
        while let Some(d) = chars.next() {
            push(d);
            if osc && d == '\x07' {
                break;
            }
            // Only a real ST ends the string; an ESC followed by anything
            // else (tmux doubles ESCs inside its passthrough) is payload.
            if d == '\x1b' && chars.peek() == Some(&'\\') {
                push(chars.next().unwrap_or('\\'));
                break;
            }
        }
    } else if chars.peek() == Some(&'\\') {
        // A bare ST (ESC \) outside any string: two bytes, nothing more.
        push(chars.next().unwrap_or('\\'));
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
        } else if c == crate::graphics::PLACEHOLDER {
            // An inline-image cell has no text: copy it as a space so a
            // selection across a picture stays column-aligned.
            out.push(' ');
        } else if !c.is_ascii() && crate::graphics::is_diacritic(c) {
            // Row/column marks of an image cell — zero width, no text.
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
    if no_color() || line_mode() || !io::stdout().is_terminal() {
        return label.to_string();
    }
    format!("\x1b]8;;{}\x1b\\{label}\x1b]8;;\x1b\\", osc8_url(url))
}

// Any control char inside the OSC 8 URL could terminate the sequence early and
// smuggle the rest to the terminal as live escapes.
fn osc8_url(url: &str) -> String {
    url.chars().filter(|&c| !c.is_control()).collect()
}

fn is_unsafe_control(c: char) -> bool {
    c != '\n' && c != '\t' && c.is_control()
}

// Bidi overrides/isolates and zero-width format chars render invisibly but
// can reorder or hide what the user reads (Trojan Source style spoofing).
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{FEFF}'
            | '\u{061C}'
    )
}

/// Neutralizes terminal control sequences in untrusted text (model output,
/// tool output, model-supplied paths) before the harness styles it: ESC shows
/// as a visible `␛`, every other C0 (except `\n` and `\t`), DEL and C1 char
/// is dropped, and bidi/invisible format chars show as `<U+XXXX>`. Apply
/// where text enters, never to already-styled strings.
pub fn sanitize_terminal(s: &str) -> std::borrow::Cow<'_, str> {
    // Fast path for the streaming hot loop: all unsafe chars are either a
    // byte below 0x20, DEL, a C1 char encoded as 0xC2 0x80..=0x9F, or a
    // format char whose UTF-8 lead byte is 0xE2 (U+2xxx), 0xD8 (U+061C) or
    // 0xEF (U+FEFF).
    if !s.bytes().any(|b| {
        (b < 0x20 && b != b'\n' && b != b'\t') || matches!(b, 0x7f | 0xc2 | 0xd8 | 0xe2 | 0xef)
    }) {
        return std::borrow::Cow::Borrowed(s);
    }
    if !s
        .chars()
        .any(|c| is_unsafe_control(c) || is_invisible_format(c))
    {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if c == '\x1b' {
            out.push('␛');
        } else if is_invisible_format(c) {
            out.push_str(&format!("<U+{:04X}>", c as u32));
        } else if !is_unsafe_control(c) {
            out.push(c);
        }
    }
    std::borrow::Cow::Owned(out)
}

// A prompt mixes harness styling with text a model or server can supply (a
// question's default answer, a detected model name). Keep the SGR color runs
// and neutralize everything else as sanitize_terminal does, so no caller can
// put a live escape on screen through `ask`/`ask_task`.
fn sanitize_prompt(prompt: &str) -> std::borrow::Cow<'_, str> {
    if !prompt.contains('\x1b') {
        return sanitize_terminal(prompt);
    }
    let mut out = String::with_capacity(prompt.len() + 8);
    let mut rest = prompt;
    while let Some(i) = rest.find('\x1b') {
        out.push_str(&sanitize_terminal(&rest[..i]));
        let esc = &rest[i..];
        let sgr = esc.strip_prefix("\x1b[").and_then(|body| {
            let params = body
                .bytes()
                .take_while(|b| b.is_ascii_digit() || *b == b';' || *b == b':')
                .count();
            (body.as_bytes().get(params) == Some(&b'm')).then_some(params + 3)
        });
        match sgr {
            Some(n) => {
                out.push_str(&esc[..n]);
                rest = &esc[n..];
            }
            None => {
                out.push('␛');
                rest = &esc[1..];
            }
        }
    }
    out.push_str(&sanitize_terminal(rest));
    std::borrow::Cow::Owned(out)
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

// Last DECSTBM bottom row we told the terminal about. reserved_rows() grows
// when a prompt is queued, so the scroll region must shrink to match — else
// streaming output scrolls over (or scrolls away) the queued-composer row and
// can scroll the whole screen (a visible flash). 0 = no region set.
static LAST_REGION_BOTTOM: AtomicU16 = AtomicU16::new(0);

fn set_output_region() {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    let (_, h) = term_size();
    let bottom = h.saturating_sub(reserved_rows()).max(1);
    LAST_REGION_BOTTOM.store(bottom, Ordering::Relaxed);
    print!("\x1b[1;{bottom}r\x1b[1;1H");
    flush();
}

// Re-assert the scroll region only when the reserved-row count changed (a
// queued prompt appeared/cleared, or the terminal resized). Cheap on the hot
// path — no write when nothing moved — and unlike set_output_region it never
// homes the cursor, so it's safe to call every frame.
fn ensure_output_region() {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    let (_, h) = term_size();
    let bottom = h.saturating_sub(reserved_rows()).max(1);
    if LAST_REGION_BOTTOM.swap(bottom, Ordering::Relaxed) != bottom {
        print!("\x1b[1;{bottom}r");
        flush();
    }
}

fn reset_output_region() {
    if ALT_SCREEN.load(Ordering::Relaxed) {
        LAST_REGION_BOTTOM.store(0, Ordering::Relaxed);
        print!("\x1b[r");
        flush();
    }
}

fn clear_composer() {
    render_queued_composer();
}

// Draw the composer box borders and return with the cursor ready at the
// start of the input row's content area. The caller writes the inner line.
fn queue_composer_box(out: &mut impl Write) {
    let (width, _) = term_size();
    let w = width as usize;
    let top = format!("╭{}╮", "─".repeat(w.saturating_sub(2)));
    let bottom = format!("╰{}╯", "─".repeat(w.saturating_sub(2)));
    let _ = queue!(
        out,
        MoveTo(0, composer_top()),
        Clear(ClearType::CurrentLine)
    );
    let _ = write!(out, "{}", dim(&top));
    let _ = queue!(
        out,
        MoveTo(0, composer_bottom()),
        Clear(ClearType::CurrentLine)
    );
    let _ = write!(out, "{}", dim(&bottom));
    let _ = queue!(
        out,
        MoveTo(0, composer_row()),
        Clear(ClearType::CurrentLine)
    );
    let _ = write!(out, "{} ", dim("│"));
}

// Close the input row with the right-hand border, clipping anything that
// would collide with it.
fn queue_composer_right_border(out: &mut impl Write) {
    let (width, _) = term_size();
    let _ = queue!(out, MoveTo(width.saturating_sub(1), composer_row()));
    let _ = write!(out, "{}", dim("│"));
}

fn queue_footer(out: &mut impl Write) {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    let (width, _) = term_size();
    let _ = queue!(out, MoveTo(0, footer_row()), Clear(ClearType::CurrentLine));

    let footer = footer_text().lock().map(|f| f.clone()).unwrap_or_default();
    let model = model_label().lock().map(|m| m.clone()).unwrap_or_default();
    let model_badge = if model.is_empty() {
        String::new()
    } else {
        format!("{} ", dim(&model))
    };
    let base_text = if is_agent_running() {
        // Live readout: braille spinner, elapsed, streamed tokens/s.
        let started = WORK_STARTED_MS.load(Ordering::Relaxed);
        let elapsed_ms = now_ms().saturating_sub(started).max(1);
        let secs = elapsed_ms / 1000;
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let frame = frames[((elapsed_ms / 100) % frames.len() as u64) as usize];
        let chars = STREAM_CHARS.load(Ordering::Relaxed);
        let tps = if chars > 0 && elapsed_ms >= 500 {
            format!(
                " · {:.0} tok/s",
                (chars as f64 / 4.0) / (elapsed_ms as f64 / 1000.0)
            )
        } else {
            String::new()
        };
        let clock = if secs >= 60 {
            format!("{}m {:02}s", secs / 60, secs % 60)
        } else {
            format!("{secs}s")
        };
        format!(
            "{}{} {} {}",
            model_badge,
            accent(frame),
            bold("working"),
            dim(&format!("· {clock}{tps} · Esc to interrupt"))
        )
    } else if footer.is_empty() {
        format!(
            "{}{} {} {}",
            model_badge,
            dim("permission:"),
            bold("ask"),
            dim("· /permissions · wheel/PgUp · /mouse")
        )
    } else {
        format!("{model_badge}{footer}")
    };

    let used = CONTEXT_USED.load(Ordering::Relaxed);
    let total = CONTEXT_TOTAL.load(Ordering::Relaxed);
    let ctx_badge = if let Some(raw_pct) = (used * 100).checked_div(total) {
        let pct = raw_pct.min(100);
        let bar_width = 8usize;
        let filled = (pct * bar_width / 100).min(bar_width);
        let bar: String = "█".repeat(filled) + &"░".repeat(bar_width - filled);
        let colored_bar = if pct >= 80 {
            red(&bar)
        } else if pct >= 60 {
            yellow(&bar)
        } else {
            green(&bar)
        };
        format!(
            " {} [{}] {}",
            dim("ctx:"),
            colored_bar,
            dim(&format!(
                "{pct}% {}/{}",
                if used < 1000 {
                    format!("{}", used)
                } else {
                    format!("{:.1}k", used as f64 / 1000.0)
                },
                if total < 1000 {
                    format!("{}", total)
                } else {
                    format!("{:.1}k", total as f64 / 1000.0)
                }
            ))
        )
    } else {
        String::new()
    };

    let offset = SCROLL_OFFSET.load(Ordering::Relaxed);
    let vim_badge = if is_vim_mode() {
        let label = get_vim_state_label();
        let colored = match label {
            "NORMAL" => green(&format!("[VIM:{label}]")),
            "INSERT" => yellow(&format!("[VIM:{label}]")),
            "VISUAL" => cyan(&format!("[VIM:{label}]")),
            _ => green("[VIM]"),
        };
        format!("{} ", bold(&colored))
    } else {
        String::new()
    };

    // The context gauge is the least urgent part of the footer: on a narrow
    // terminal drop it whole rather than cut it off mid-number.
    let fits = |s: &str| strip_ansi(s).chars().count() <= width as usize;
    let ctx_badge = if fits(&format!("{vim_badge}{base_text}{ctx_badge}")) {
        ctx_badge
    } else {
        String::new()
    };

    let mut text = if offset > 0 {
        format!(
            "{}{}{} {}",
            vim_badge,
            base_text,
            ctx_badge,
            dim(&format!("· scroll +{offset}"))
        )
    } else {
        format!("{}{}{}", vim_badge, base_text, ctx_badge)
    };

    if let Some(fl) = active_flash() {
        text = format!("{text}  {}", accent(&fl));
    }
    let _ = write!(out, "{}", ellipsize_ansi_line(&text, width as usize));
}

fn render_footer() {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    let _frame = render_lock();
    let mut out: Vec<u8> = Vec::new();
    let _ = write!(out, "\x1b[?2026h");
    let _ = queue!(out, SavePosition);
    queue_footer(&mut out);
    let _ = queue!(out, RestorePosition);
    let _ = write!(out, "\x1b[?2026l");
    write_frame(&out);
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
    if !is_raw() || line_mode() {
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
    let Some(color) = pal().cursor else {
        return;
    };
    if no_color() || line_mode() {
        return;
    }
    print!("\x1b]12;{color}\x07");
    flush();
}

fn cursor_reset_style() {
    if line_mode() {
        return;
    }
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
    if line_mode() {
        return;
    }
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
    render_footer();
}

fn render_output() {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    let _frame = render_lock();
    // Keep the scroll region matched to the current reserved rows before we
    // repaint — a just-queued prompt must not be scrolled over.
    ensure_output_region();
    let (width, height) = term_size();
    let width = width as usize;
    let rows = height.saturating_sub(reserved_rows()) as usize;
    let Ok(mut t) = transcript().lock() else {
        return;
    };
    t.ensure_size(width, height as usize);
    let total = t.total_rows();
    let max_offset = total.saturating_sub(rows);
    let offset = SCROLL_OFFSET.load(Ordering::Relaxed).min(max_offset);
    if offset != SCROLL_OFFSET.load(Ordering::Relaxed) {
        SCROLL_OFFSET.store(offset, Ordering::Relaxed);
    }
    let start = total.saturating_sub(rows + offset);
    let visible = t.rows_range(start, rows);
    // Sixel images are pixels over blank rows. One that is on screen at the
    // same place as last frame, with nothing drawn over it since, is left
    // alone: rewriting its rows would erase it, and re-sending it every
    // streamed frame would be slow.
    let placements = sixel_placements(&visible, rows);
    let kept: Vec<Placement> = if SIXEL_DIRTY.swap(false, Ordering::Relaxed) {
        Vec::new()
    } else {
        let drawn = drawn_sixels().lock().map(|d| d.clone()).unwrap_or_default();
        placements
            .iter()
            .filter(|p| drawn.contains(p))
            .copied()
            .collect()
    };
    let in_kept = |row: usize| {
        kept.iter()
            .any(|&(_, top, _, n)| row >= top && row < top + n)
    };
    let mut out: Vec<u8> = Vec::with_capacity(rows * (width + 16));
    // Synchronized output (DEC 2026): supporting terminals (kitty, iTerm2,
    // WezTerm, Alacritty, foot…) apply the whole repaint as one atomic frame
    // — zero tearing/flicker. Ignored elsewhere.
    let _ = write!(out, "\x1b[?2026h");
    let sel = selection().lock().ok().and_then(|g| *g);
    let mut plain_rows = Vec::with_capacity(rows);
    // Every row starts on the theme's background, whatever the row before
    // it left set, so a painted theme stays whole on any terminal.
    let bg = theme_bg();
    for row in 0..rows {
        if in_kept(row) {
            plain_rows.push(String::new());
            continue;
        }
        let _ = queue!(out, MoveTo(0, row as u16));
        out.extend_from_slice(bg.as_bytes());
        if let Some(line) = visible.get(row) {
            if line.starts_with(IMG_MARK) {
                let _ = write!(out, "{}", pad_ansi_line("", width));
                plain_rows.push(String::new());
                continue;
            }
            let plain = strip_ansi(line);
            if let Some(sel) = sel {
                if let Some(range) = selection_range_for(sel, row as u16, &plain) {
                    let _ = write!(
                        out,
                        "{}",
                        pad_ansi_line(&selected_line(&plain, range), width)
                    );
                } else {
                    let _ = write!(out, "{}", pad_ansi_line(line, width));
                }
            } else {
                let _ = write!(out, "{}", pad_ansi_line(line, width));
            }
            plain_rows.push(plain);
        } else {
            let _ = write!(out, "{}", " ".repeat(width));
            plain_rows.push(String::new());
        }
    }
    drop(t);
    // Drawing a Sixel leaves the cursor on the row below the image; with the
    // scroll region's bottom margin there, the terminal would scroll the
    // transcript. The margins are lifted while images are drawn: the rows
    // below the output are the composer's, so nothing scrolls.
    let redraw: Vec<_> = placements.iter().filter(|p| !kept.contains(p)).collect();
    if !redraw.is_empty() {
        let _ = write!(out, "\x1b[r");
    }
    for p in &redraw {
        let data = inline_images()
            .lock()
            .ok()
            .and_then(|mut imgs| imgs.get_mut(p.0).map(|img| img.sixel_rows(p.2, p.3)));
        if let Some(data) = data {
            let _ = queue!(out, MoveTo(2, p.1 as u16));
            let _ = write!(out, "{data}");
        }
    }
    if !redraw.is_empty() {
        let _ = write!(
            out,
            "\x1b[1;{}r",
            LAST_REGION_BOTTOM.load(Ordering::Relaxed).max(1)
        );
    }
    drop(redraw);
    if let Ok(mut drawn) = drawn_sixels().lock() {
        *drawn = placements;
    }
    if let Ok(mut rows) = visible_rows().lock() {
        *rows = plain_rows;
    }
    let _ = write!(out, "\x1b[?2026l");
    write_frame(&out);
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
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

fn render_output_throttled() {
    let now = monotonic_ms();
    let last = LAST_STREAM_FRAME_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= FRAME_MS || last == 0 {
        LAST_STREAM_FRAME_MS.store(now.max(1), Ordering::Relaxed);
        render_output();
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
        let total = if let Ok(t) = transcript().lock() {
            t.total_rows()
        } else {
            0
        };
        let rows = term_size().1.saturating_sub(reserved_rows()) as usize;
        let max_offset = total.saturating_sub(rows);
        current.saturating_add(delta as usize).min(max_offset)
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
    let mut cell = 0;
    let index = chars.iter().position(|&ch| {
        let width = char_width(ch);
        let hit = cell <= col && col < cell + width;
        cell += width;
        hit
    })?;
    let c = chars[index];
    if c.is_whitespace() {
        return None;
    }
    let ident = |ch: char| ch.is_alphanumeric() || ch == '_' || char_width(ch) == 0;
    let class = if ident(c) { 0 } else { 1 };
    let same = |ch: char| {
        if class == 0 {
            ident(ch)
        } else {
            !ch.is_whitespace() && !ident(ch)
        }
    };
    let mut start = index;
    while start > 0 && same(chars[start - 1]) {
        start -= 1;
    }
    let mut end = index;
    while end + 1 < chars.len() && same(chars[end + 1]) {
        end += 1;
    }
    let from: usize = chars[..start].iter().copied().map(char_width).sum();
    let to: usize = chars[..=end].iter().copied().map(char_width).sum();
    Some((from, to.saturating_sub(1)))
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
                .and_then(|rows| rows.get(row as usize).map(|l| str_width(l)))
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
        if osc52_known_unsupported() {
            // The escape went out anyway (harmless), but a "copied" flash
            // would be a lie here — say so once and stay quiet afterwards.
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                line(&dim("  clipboard: terminal does not advertise OSC 52"));
            }
        } else {
            flash_footer(&format!("⎘ copied {n} chars (OSC 52)"));
        }
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

fn selection_range_for(sel: Selection, row: u16, line: &str) -> Option<(usize, usize)> {
    let (start, end) = normalized_selection(sel);
    if row < start.row || row > end.row {
        return None;
    }
    let from = if row == start.row {
        start.col as usize
    } else {
        0
    };
    let to = if row == end.row {
        (end.col as usize).saturating_add(1)
    } else {
        str_width(line)
    };
    // Mouse positions are display columns; the renderer and clipboard slice
    // Unicode characters. Include a wide character when either cell is hit,
    // and keep combining marks attached to a selected base character.
    let mut col = 0;
    let mut first = None;
    let mut end = 0;
    for (i, ch) in line.chars().enumerate() {
        let width = char_width(ch);
        if (width > 0 && col < to && col + width > from)
            || (width == 0 && first.is_some() && end == i)
        {
            first.get_or_insert(i);
            end = i + 1;
        }
        col += width;
    }
    first.map(|start| (start, end))
}

// Theme selection tint (Tokyo Night visual-select) — far gentler than
// inverse video, which flashed harsh white blocks over the transcript.

fn selection_span(s: &str) -> String {
    if no_color() {
        return format!("\x1b[7m{s}\x1b[27m");
    }
    on_bg(pal().selection_bg, pal().text, s)
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
        if let Some((from, to)) = selection_range_for(sel, row, line) {
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
    let _frame = render_lock();
    let (width, _) = term_size();
    let pwidth = prompt_width(prompt);
    // Room inside the box: left border "│ " + prompt … text … " │" right border.
    let avail = width
        .saturating_sub(pwidth)
        .saturating_sub(COMPOSER_PAD + 2)
        .max(8) as usize;
    let (s, _col) = viewport(buf, cursor, avail, *scroll);
    *scroll = s;
    let mut end = s;
    let mut current_width = 0;
    while end < buf.len() && current_width + char_width(buf[end]) <= avail {
        current_width += char_width(buf[end]);
        end += 1;
    }
    let shown: String = buf[s..end].iter().collect();
    let col_width = buf[s..cursor.min(buf.len())]
        .iter()
        .copied()
        .map(char_width)
        .sum::<usize>();
    let mut out: Vec<u8> = Vec::new();
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
    write_frame(&out);
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
    // Gradient across the theme's stops (a blue ramp, deep → pale). The
    // 16-colour theme has no ramp to blend: each letter takes its stop.
    let stops = pal().wordmark;
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
            let col = match (stops[seg], stops[seg + 1]) {
                (Col::Rgb(r1, g1, b1), Col::Rgb(r2, g2, b2)) => {
                    Col::Rgb(lerp(r1, r2), lerp(g1, g2), lerp(b1, b2))
                }
                (a, _) => a,
            };
            paint(col, &c.to_string())
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
    line(&ellipsize_ansi_line(
        &format!("  {}  {}", bold(&wordmark()), dim(crate::VERSION),),
        w,
    ));
    line(&dim(&format!("  {}", "─".repeat(w.saturating_sub(4)))));
    // Aligned key/value context rows: dim keys, plain values.
    line(&ellipsize_ansi_line(
        &format!("  {}  {provider} · {model}", dim("model")),
        w,
    ));
    // The folder name comes from whoever made the checkout.
    let cwd = &*sanitize_terminal(cwd);
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
    line(&ellipsize_ansi_line(
        &format!("  {}    {}", dim("cwd"), dim(&cwd_label)),
        w,
    ));
    line(&banner_mode_row(mode, w));
    line("");
}

fn banner_mode_row(mode: &str, width: usize) -> String {
    ellipsize_ansi_line(
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
// Updates statusline below composer bar. Color shifts green → yellow → red as the window fills up.
pub fn context_meter(used: usize, total: usize) {
    if total == 0 {
        return;
    }
    CONTEXT_USED.store(used, Ordering::Relaxed);
    CONTEXT_TOTAL.store(total, Ordering::Relaxed);
    render_footer();
}

// Enter the alternate screen and raw mode (and capture panics to restore the
// terminal even on crash). The bottom row is reserved for the composer; output
// scrolls in the region above it.
// ── signal-safe terminal restore ──────────────────────────────────────────────
// The panic hook restores the terminal when we crash from inside, but a
// SIGTERM/SIGHUP/SIGINT from outside kills the process directly — without a
// handler the user's terminal is left in the alternate screen with raw mode
// on ("typing shows nothing") until they run `reset`. Signal handlers may only
// use async-signal-safe calls, so this is raw write(2) + tcsetattr(2) +
// _exit(2), nothing else.
// Constant bytes (async-signal-safe to write): reset the scroll margins and
// colors, show the cursor, turn off every mouse mode (1000/1002/1003 plus the
// 1015/1006 encodings), focus reporting and bracketed paste, restore the
// default cursor shape, then leave the alternate screen.
#[cfg_attr(not(any(unix, windows)), allow(dead_code))]
const RESTORE: &[u8] = b"\x1b[r\x1b[0m\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1015l\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[0 q\x1b[?1049l";

#[cfg(unix)]
mod signal_restore {
    use super::{ALT_SCREEN, RAW, RESTORE};
    use std::sync::atomic::Ordering;
    use std::sync::OnceLock;

    // Snapshot of the cooked terminal, taken before raw mode is enabled.
    // Written once before the handlers are installed; the handler only reads.
    static ORIG_TERMIOS: OnceLock<libc::termios> = OnceLock::new();

    pub fn install() {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut t) == 0 {
                let _ = ORIG_TERMIOS.set(t);
            }
            let h: extern "C" fn(libc::c_int) = handler;
            for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
                libc::signal(sig, h as usize);
            }
        }
    }

    extern "C" fn handler(sig: libc::c_int) {
        unsafe {
            if ALT_SCREEN.load(Ordering::Relaxed) {
                let _ = libc::write(
                    libc::STDOUT_FILENO,
                    RESTORE.as_ptr() as *const libc::c_void,
                    RESTORE.len(),
                );
            }
            if RAW.load(Ordering::Relaxed) {
                if let Some(t) = ORIG_TERMIOS.get() {
                    let _ = libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, t);
                }
            }
            libc::_exit(128 + sig);
        }
    }
}

// Windows counterpart: a console control handler for Ctrl+Break, closing the
// console window, logoff and shutdown. (Ctrl+C never reaches it while raw
// mode is on — the console delivers it as a key event instead.) The handler
// runs on its own thread while the process is being torn down; it restores
// the terminal and then returns FALSE so the default handler still ends the
// process. `SetConsoleCtrlHandler` is declared here directly — kernel32 is
// always linked on Windows and this avoids a Windows API crate.
#[cfg(windows)]
mod signal_restore {
    use super::{ALT_SCREEN, RAW, RESTORE};
    use std::io::Write;
    use std::sync::atomic::Ordering;
    use std::sync::Once;

    type HandlerRoutine = unsafe extern "system" fn(u32) -> i32;

    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<HandlerRoutine>, add: i32) -> i32;
    }

    pub fn install() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| unsafe {
            let _ = SetConsoleCtrlHandler(Some(handler), 1);
        });
    }

    unsafe extern "system" fn handler(_ctrl_type: u32) -> i32 {
        if ALT_SCREEN.swap(false, Ordering::Relaxed) {
            let mut out = std::io::stdout();
            let _ = out.write_all(RESTORE);
            let _ = out.flush();
        }
        if RAW.swap(false, Ordering::Relaxed) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
        0
    }
}

// The `theme` setting, applied when the session takes the screen: before
// the background is painted, and before anything else reads stdin (auto
// asks the terminal for its background colour).
fn apply_theme_setting() {
    let setting = crate::config::load_settings()
        .map(|s| s.theme)
        .unwrap_or_default();
    if let Err(e) = set_theme(&setting) {
        let _ = set_theme("auto");
        eprintln!("{}", yellow(&format!("  {e} (in settings.json)")));
    }
}

pub fn enter_alt(raw: bool) {
    if raw {
        apply_theme_setting();
    }
    if raw && line_mode() {
        // Line mode: raw keys (so Esc and Ctrl+C interrupt and prompts can be
        // cancelled) but no alternate screen and no other screen control.
        #[cfg(any(unix, windows))]
        signal_restore::install();
        if enable_raw_mode().is_ok() {
            RAW.store(true, Ordering::Relaxed);
        }
        install_panic_hook();
        return;
    }
    if raw {
        // Before any terminal-state change: snapshot the cooked termios and
        // arm the restore-on-signal handlers.
        #[cfg(any(unix, windows))]
        signal_restore::install();
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
        invalidate_inline_pixels();
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
        // Ask for Sixel support and the cell size once, before any other
        // reader of stdin starts and before focus reports can interleave.
        if io::stdin().is_terminal() && io::stdout().is_terminal() && images_enabled() {
            crate::sixel::probe();
        }
        let _ = execute!(io::stdout(), EnableBracketedPaste, EnableFocusChange);
        set_mouse_capture(true);
        // Accent-colored blinking bar cursor for the composer.
        cursor_color_accent();
        set_cursor_shape(CursorShape::Bar);
    }
    install_panic_hook();
}

// Once per process: enter_alt runs again after every suspend/editor round
// trip, and re-wrapping the previous hook each time would stack restores.
fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            reset_output_region();
            cursor_reset_style();
            cursor_show();
            let _ = write!(io::stdout(), "\x1b[0m");
            let _ = execute!(
                io::stdout(),
                DisableBracketedPaste,
                DisableFocusChange,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
            MOUSE_CAPTURED.store(false, Ordering::Relaxed);
            ALT_SCREEN.store(false, Ordering::Relaxed);
            let _ = disable_raw_mode();
            prev(info);
        }));
    });
}

pub fn leave_alt() {
    if !ALT_SCREEN.load(Ordering::Relaxed) && line_mode() {
        if RAW.swap(false, Ordering::Relaxed) {
            let _ = disable_raw_mode();
        }
        return;
    }
    clear_composer();
    reset_output_region();
    if RAW.load(Ordering::Relaxed) {
        cursor_reset_style();
        cursor_show();
    }
    if RAW.swap(false, Ordering::Relaxed) {
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableFocusChange,
            DisableMouseCapture
        );
        MOUSE_CAPTURED.store(false, Ordering::Relaxed);
        let _ = disable_raw_mode();
    }
    free_images();
    if ALT_SCREEN.swap(false, Ordering::Relaxed) {
        let _ = write!(io::stdout(), "\x1b[0m");
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

pub fn clear() {
    if ALT_SCREEN.load(Ordering::Relaxed) {
        SCROLL_OFFSET.store(0, Ordering::Relaxed);
        invalidate_stream_line();
        free_images();
        if let Ok(mut t) = transcript().lock() {
            t.clear();
        }
        let _ = write!(io::stdout(), "{}", theme_bg());
        let _ = execute!(io::stdout(), Clear(ClearType::All), MoveTo(0, 0));
        set_output_region();
        clear_composer();
        render_footer();
    } else if !line_mode() {
        print!("\x1b[2J\x1b[H");
        flush();
    }
}

pub fn browse_items(title: &str, items: &[(String, String)]) {
    let items = &browse_safe(items)[..];
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

// Skill names and descriptions come from files in the checkout, tool names
// and descriptions from MCP servers: neutralize them once for both views.
fn browse_safe(items: &[(String, String)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(name, detail)| {
            (
                sanitize_terminal(name).into_owned(),
                sanitize_terminal(detail).into_owned(),
            )
        })
        .collect()
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

fn pad_ansi_line(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut visible = 0usize;
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            eat_escape(&mut chars, Some(&mut out));
            continue;
        }
        let w = char_width(c);
        if visible + w > width {
            break;
        }
        out.push(c);
        visible += w;
    }
    if visible < width {
        out.push_str(&" ".repeat(width - visible));
    }
    out
}

// Wrap into rows of at most `max_cols` display columns; width-aware like
// clip_ansi_line so the alt-screen row math holds for emoji/CJK lines.
// Rows break after the last space that fits, and continuation rows keep the
// line's leading indent, so a wrapped hint reads `… · a always · d` /
// `<reason> deny` instead of `d <r` / `eason> deny`. A word longer than the
// row is broken where the row ends.
fn wrap_ansi_line(s: &str, max_cols: usize) -> Vec<String> {
    if max_cols == 0 {
        return vec![String::new()];
    }
    if s.starts_with(IMG_MARK) {
        if let Some(rows) = inline_image_rows(s, max_cols) {
            return rows;
        }
    }
    if s.is_empty() {
        return vec![String::new()];
    }
    // Leading indent of the visible text, repeated on continuation rows
    // while it leaves at least half the row for text.
    let lead = strip_ansi(s).chars().take_while(|c| *c == ' ').count();
    let indent = if lead * 2 <= max_cols { lead } else { 0 };
    let mut out = Vec::new();
    let mut current = String::new();
    let mut visible = 0usize;
    // SGR sequences in force at the wrap point, replayed at the start of
    // the continuation row: a terminal repaints rows independently, so a
    // colour or tint that started on row 1 would otherwise vanish on row 2
    // (and an image row's id colour would break its placeholder cells).
    let mut active = String::new();
    // Last place this row may break: byte offset just after a space that
    // follows some text, the columns used up to it, and the SGR state there.
    let mut brk: Option<(usize, usize, String)> = None;
    let mut seen_text = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            let start = current.len();
            current.push(c);
            eat_escape(&mut chars, Some(&mut current));
            let seq = &current[start..];
            if seq.ends_with('m') && seq.starts_with("\x1b[") {
                if seq == "\x1b[0m" || seq == "\x1b[m" {
                    active.clear();
                } else {
                    active.push_str(seq);
                }
            }
            continue;
        }
        let w = char_width(c);
        // `visible > 0` guard: a width-2 char on a 1-column terminal still
        // gets a row of its own instead of an infinite run of empty rows.
        if visible + w > max_cols && visible > 0 {
            let pad = " ".repeat(indent);
            match brk.take().filter(|_| c != ' ') {
                // Carry the partial word over to the next row.
                Some((at, cols, sgr)) => {
                    let rest = current.split_off(at);
                    out.push(std::mem::take(&mut current));
                    current = format!("{sgr}{pad}{rest}");
                    visible = indent + visible - cols;
                }
                None => {
                    out.push(std::mem::take(&mut current));
                    current = format!("{active}{pad}");
                    visible = indent;
                }
            }
            if c == ' ' {
                // The space that ended the row is not carried over.
                continue;
            }
        }
        current.push(c);
        visible += w;
        if c == ' ' {
            if seen_text {
                brk = Some((current.len(), visible, active.clone()));
            }
        } else {
            seen_text = true;
        }
    }
    out.push(current);
    out
}

/// Fit one line into `max_cols`: unchanged when it fits, else cut one
/// column short and ended with `…`, so a narrow terminal never shows half
/// a word as if it were whole.
fn ellipsize_ansi_line(s: &str, max_cols: usize) -> String {
    if str_width(&strip_ansi(s)) <= max_cols {
        return s.to_string();
    }
    if max_cols == 0 {
        return String::new();
    }
    let cut = clip_ansi_line(s, max_cols - 1);
    format!("{cut}{}…", reset_all())
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

// Keep a scrolled-back view pinned to what the reader is looking at: rows
// appended at the bottom push the bottom-relative offset up by the same
// amount. (Rows dropped from the front don't move the view — the offset is
// measured from the end.)
fn pin_scroll(before_rows: usize, t: &Transcript) {
    if SCROLL_OFFSET.load(Ordering::Relaxed) > 0 {
        let added = t.total_rows().saturating_sub(before_rows);
        if added > 0 {
            SCROLL_OFFSET.fetch_add(added, Ordering::Relaxed);
        }
    }
}

pub fn line(s: &str) {
    if ALT_SCREEN.load(Ordering::Relaxed) {
        invalidate_stream_line();
        if let Ok(mut t) = transcript().lock() {
            let before = if SCROLL_OFFSET.load(Ordering::Relaxed) > 0 {
                t.total_rows()
            } else {
                0
            };
            for part in s.replace('\r', "").split('\n') {
                t.push(part.to_string());
            }
            pin_scroll(before, &t);
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
    STREAM_CHARS.fetch_add(chunk.chars().count(), Ordering::Relaxed);
    if ALT_SCREEN.load(Ordering::Relaxed) {
        if let Ok(mut t) = transcript().lock() {
            let before = if SCROLL_OFFSET.load(Ordering::Relaxed) > 0 {
                t.total_rows()
            } else {
                0
            };
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
            pin_scroll(before, &t);
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

fn start_typeahead_thread() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        std::thread::spawn(|| loop {
            if is_raw() && is_agent_running() {
                poll_typeahead();
                // Elapsed-time / tokens-per-second readout: repaint the
                // footer row a few times a second while the agent works.
                let now = now_ms();
                if now.saturating_sub(LAST_STATUS_TICK_MS.load(Ordering::Relaxed)) >= 250 {
                    LAST_STATUS_TICK_MS.store(now, Ordering::Relaxed);
                    render_footer();
                }
            }
            std::thread::sleep(Duration::from_millis(15));
        });
    });
}

// Non-blocking: drain pending key events and report whether Ctrl-C or Esc was pressed.
pub fn set_agent_running(running: bool) {
    let was = AGENT_RUNNING.swap(running, Ordering::Relaxed);
    if running {
        INTERRUPT_KIND_VAL.store(0, Ordering::Relaxed);
        if !was {
            WORK_STARTED_MS.store(now_ms(), Ordering::Relaxed);
            STREAM_CHARS.store(0, Ordering::Relaxed);
            taskbar_progress(true);
        }
        start_typeahead_thread();
    } else if was {
        taskbar_progress(false);
        let started = WORK_STARTED_MS.load(Ordering::Relaxed);
        let elapsed = now_ms().saturating_sub(started);
        // A turn long enough to have walked away from, and not a burst of
        // nested start/stop pairs (sub-agents) re-announcing the same turn.
        if elapsed >= 8_000
            && now_ms().saturating_sub(LAST_NOTIFY_MS.load(Ordering::Relaxed)) > 5_000
        {
            LAST_NOTIFY_MS.store(now_ms(), Ordering::Relaxed);
            let secs = elapsed / 1000;
            let took = if secs >= 60 {
                format!("{}m {:02}s", secs / 60, secs % 60)
            } else {
                format!("{secs}s")
            };
            notify(
                "buildwithnexus",
                &format!("done after {took} — ready for your next prompt"),
            );
        }
        render_footer();
    }
}

pub fn is_agent_running() -> bool {
    AGENT_RUNNING.load(Ordering::Relaxed)
}

pub fn trigger_interrupt(kind: InterruptKind) {
    INTERRUPT_KIND_VAL.store(kind as u8, Ordering::Relaxed);
    if kind == InterruptKind::CtrlC {
        if let Ok(mut mq) = message_queue().lock() {
            mq.clear();
        }
    }
}

pub fn get_interrupt_kind() -> InterruptKind {
    if !is_raw() {
        return InterruptKind::None;
    }
    // poll_typeahead is the single event reader: it raises the flag for
    // Ctrl-C/Esc and buffers every other key, so reading events here would
    // drop what the user types while the agent streams. Repaint only when
    // something was read: this runs once per streamed chunk.
    if drain_typeahead() {
        render_queued_composer();
    }
    interrupt_kind()
}

fn interrupt_kind() -> InterruptKind {
    match INTERRUPT_KIND_VAL.load(Ordering::Relaxed) {
        1 => InterruptKind::Escape,
        2 => InterruptKind::CtrlC,
        _ => InterruptKind::None,
    }
}

pub fn interrupted() -> bool {
    get_interrupt_kind() != InterruptKind::None
}

pub fn consume_interrupt() -> InterruptKind {
    let kind = get_interrupt_kind();
    INTERRUPT_KIND_VAL.store(0, Ordering::Relaxed);
    kind
}

#[derive(Debug, Clone)]
pub struct SelectItem {
    pub label: String,
    pub detail: String,
}

// Interactive selection menu — pops a list dialog that users can navigate
// using Up/Down arrow keys (or j/k) and select with Enter, or cancel with Esc.
struct PauseAgentRunningGuard {
    was_running: bool,
}

impl PauseAgentRunningGuard {
    fn new() -> Self {
        let was_running = is_agent_running();
        if was_running {
            set_agent_running(false);
        }
        Self { was_running }
    }
}

impl Drop for PauseAgentRunningGuard {
    fn drop(&mut self) {
        if self.was_running {
            set_agent_running(true);
        }
    }
}

pub fn drain_stdin() {
    // Intentionally no-op: do NOT read and discard stdin events so typed-ahead
    // keystrokes (like "good point") are preserved 100% reliably.
}

// What the input box reads while a picker is open.
const PICKER_HINT: &str = "↑↓ choose · Enter select · Esc close";

// Keyboard state of an open picker, kept apart from the terminal so it can
// be tested: typed text filters the list, a digit typed before any other
// text picks that numbered row, and Enter picks only a row that is shown —
// so a sentence typed into a forgotten picker never confirms its default.
#[derive(Default)]
struct Picker {
    filter: String,
    // Position within `shown`, not an item index.
    selected: usize,
}

#[derive(Debug, PartialEq)]
enum PickerStep {
    Redraw,
    Choose(usize),
    Close,
    // Enter with nothing shown: the picker stays open, unchanged.
    NoMatch,
}

impl Picker {
    // Indexes of the items whose label or detail contains the filter
    // (case-insensitive), in list order.
    fn shown(&self, items: &[SelectItem]) -> Vec<usize> {
        let f = self.filter.trim().to_lowercase();
        items
            .iter()
            .enumerate()
            .filter(|(_, it)| {
                f.is_empty()
                    || it.label.to_lowercase().contains(&f)
                    || it.detail.to_lowercase().contains(&f)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn key(&mut self, k: crossterm::event::KeyEvent, items: &[SelectItem]) -> PickerStep {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let shown = self.shown(items);
        match k.code {
            KeyCode::Esc => PickerStep::Close,
            KeyCode::Char('c') | KeyCode::Char('d') if ctrl => PickerStep::Close,
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                PickerStep::Redraw
            }
            KeyCode::Down => {
                if self.selected + 1 < shown.len() {
                    self.selected += 1;
                }
                PickerStep::Redraw
            }
            KeyCode::Enter => match shown.get(self.selected) {
                Some(&i) => PickerStep::Choose(i),
                None => PickerStep::NoMatch,
            },
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
                PickerStep::Redraw
            }
            KeyCode::Char('u') if ctrl => {
                self.filter.clear();
                self.selected = 0;
                PickerStep::Redraw
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                if self.filter.is_empty() {
                    if let Some(n) = c.to_digit(10).map(|d| d as usize) {
                        if (1..=items.len().min(9)).contains(&n) {
                            return PickerStep::Choose(n - 1);
                        }
                    }
                }
                self.filter.push(c);
                self.selected = 0;
                PickerStep::Redraw
            }
            _ => PickerStep::Redraw,
        }
    }

    fn type_text(&mut self, s: &str) {
        self.filter.extend(sanitize_paste(s));
        self.selected = 0;
    }
}

// Rows 1-9 carry their number: typing it picks the row.
fn picker_number(i: usize) -> String {
    if i < 9 {
        format!("{}", i + 1)
    } else {
        " ".to_string()
    }
}

/// Opens a picker over `items` and returns the chosen index, or None when
/// it is closed (Esc, Ctrl+C) or the input ends. While it is open it owns
/// the keyboard: typed text filters the list, a digit picks that numbered
/// row, ↑/↓ move, and Enter picks the highlighted row of the filtered list.
pub fn select_item(title: &str, items: &[SelectItem]) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    let _pause_guard = PauseAgentRunningGuard::new();
    // Titles and items can carry model-supplied text (the question tool).
    let title = &*sanitize_terminal(title);
    if !is_raw() || !ALT_SCREEN.load(Ordering::Relaxed) {
        line(&accent(&format!("  {title}")));
        for (i, item) in items.iter().enumerate() {
            line(&format!(
                "  {:>2}. {} — {}",
                i + 1,
                bold(&sanitize_terminal(&item.label)),
                dim(&sanitize_terminal(&item.detail))
            ));
        }
        let idx = ask("  Select number: ")?.trim().parse::<usize>().ok()?;
        if idx > 0 && idx <= items.len() {
            return Some(idx - 1);
        }
        return None;
    }

    let prompt = format!("{} {} ", dim(PICKER_HINT), accent("›"));
    let owner = InputOwner::open(&prompt, &[], 0);
    let mut picker = Picker::default();
    let mut scroll_offset = 0usize;
    let mut drawn: Option<(u16, u16)> = None;
    let mut no_match = false;
    cursor_show();

    let clear_rows = |rows: Option<(u16, u16)>| {
        if let Some((top, bottom)) = rows {
            let mut out = io::stdout();
            for r in top..=bottom {
                let _ = queue!(out, MoveTo(0, r), Clear(ClearType::CurrentLine));
            }
            let _ = out.flush();
        }
    };

    let result = loop {
        let (width, height) = term_size();
        let shown = picker.shown(items);
        let max_items = (height.saturating_sub(6)).max(1) as usize;
        let visible_items = shown.len().clamp(1, max_items);
        let total_lines = (visible_items + 2) as u16;
        if picker.selected < scroll_offset {
            scroll_offset = picker.selected;
        } else if picker.selected >= scroll_offset + visible_items {
            scroll_offset = picker.selected + 1 - visible_items;
        }
        let base = composer_top().saturating_sub(total_lines);
        let footer_row = base + 1 + visible_items as u16;
        // The list shrank (filtering): repaint the transcript rows it no
        // longer covers before drawing it again.
        if drawn.is_some_and(|(top, _)| top < base) {
            clear_rows(drawn);
            render_output();
        }

        let mut out: Vec<u8> = Vec::new();
        let _ = write!(out, "\x1b[?2026h");
        let header = clip_ansi_line(
            &accent(&format!(
                "  ┌── {title} ─────────────────────────────────────────────────────────────"
            )),
            width as usize,
        );
        let _ = queue!(out, MoveTo(0, base), Clear(ClearType::CurrentLine));
        let _ = write!(out, "{header}");
        if shown.is_empty() {
            let _ = queue!(out, MoveTo(0, base + 1), Clear(ClearType::CurrentLine));
            let msg = format!(
                "  │    no match for “{}”",
                sanitize_terminal(&picker.filter)
            );
            let _ = write!(out, "{}", clip_ansi_line(&dim(&msg), width as usize));
        }
        for (pos, &i) in shown
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(visible_items)
        {
            let item = &items[i];
            let formatted = if pos == picker.selected {
                format!(
                    "  │  {} {} {} {}",
                    accent("❯"),
                    accent(&picker_number(i)),
                    bold(&sanitize_terminal(&item.label)),
                    green(&format!("({})", sanitize_terminal(&item.detail)))
                )
            } else {
                format!(
                    "  │    {} {} {}",
                    dim(&picker_number(i)),
                    sanitize_terminal(&item.label),
                    dim(&format!("({})", sanitize_terminal(&item.detail)))
                )
            };
            let row = base + 1 + (pos - scroll_offset) as u16;
            let _ = queue!(out, MoveTo(0, row), Clear(ClearType::CurrentLine));
            let _ = write!(out, "{}", ellipsize_ansi_line(&formatted, width as usize));
        }
        let foot_text = if no_match {
            "  └── nothing matches — Backspace edits the filter, Esc closes ─────────────────"
        } else {
            "  └── type to filter · 1-9 pick a numbered row ───────────────────────────────────"
        };
        let footer = clip_ansi_line(&dim(foot_text), width as usize);
        let _ = queue!(out, MoveTo(0, footer_row), Clear(ClearType::CurrentLine));
        let _ = write!(out, "{footer}");
        let _ = write!(out, "\x1b[?2026l");
        write_frame(&out);
        drawn = Some((base, footer_row));
        // The input box: the picker's hint and what has been typed.
        let typed: Vec<char> = picker.filter.chars().collect();
        let mut scroll = 0usize;
        redraw(&prompt, (0, 0), &typed, typed.len(), &mut scroll);

        let step = match read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => picker.key(k, items),
            Ok(Event::Paste(s)) => {
                picker.type_text(&s);
                PickerStep::Redraw
            }
            Ok(Event::Resize(_, _)) => {
                // Rows moved: repaint the transcript, then the list anew.
                set_output_region();
                render_output();
                drawn = None;
                continue;
            }
            Ok(_) => continue,
            Err(_) => {
                INPUT_CLOSED.store(true, Ordering::Relaxed);
                PickerStep::Close
            }
        };
        no_match = step == PickerStep::NoMatch;
        match step {
            PickerStep::Choose(i) => break Some(i),
            PickerStep::Close => break None,
            PickerStep::Redraw | PickerStep::NoMatch => {}
        }
    };
    // Hand the box back before the outcome line repaints it.
    clear_rows(drawn);
    drop(owner);
    render_output();
    match result {
        Some(i) => line(&green(&format!(
            "  ✓ selected: {}",
            sanitize_terminal(&items[i].label)
        ))),
        None => line(&dim("  cancelled selection")),
    }
    result
}

#[cfg(test)]
mod picker_tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn items() -> Vec<SelectItem> {
        ["Execute Plan", "Edit Step", "Cancel"]
            .iter()
            .map(|l| SelectItem {
                label: l.to_string(),
                detail: format!("{l} detail"),
            })
            .collect()
    }

    fn press(p: &mut Picker, items: &[SelectItem], code: KeyCode) -> PickerStep {
        p.key(KeyEvent::new(code, KeyModifiers::NONE), items)
    }

    fn type_line(p: &mut Picker, items: &[SelectItem], text: &str) -> Vec<PickerStep> {
        let mut steps: Vec<PickerStep> = text
            .chars()
            .map(|c| press(p, items, KeyCode::Char(c)))
            .collect();
        steps.push(press(p, items, KeyCode::Enter));
        steps
    }

    #[test]
    fn typed_text_never_confirms_the_default() {
        let items = items();
        for text in ["what does this project do?", "/clear", "yes please"] {
            let mut p = Picker::default();
            let steps = type_line(&mut p, &items, text);
            assert!(
                !steps.iter().any(|s| matches!(s, PickerStep::Choose(_))),
                "{text:?} chose something: {steps:?}"
            );
            assert_eq!(steps.last(), Some(&PickerStep::NoMatch));
        }
        // j and k are filter letters now, not navigation.
        let mut p = Picker::default();
        press(&mut p, &items, KeyCode::Char('j'));
        assert_eq!(p.filter, "j");
    }

    #[test]
    fn typed_text_filters_and_enter_picks_the_shown_row() {
        let items = items();
        let mut p = Picker::default();
        assert_eq!(
            type_line(&mut p, &items, "edit").last(),
            Some(&PickerStep::Choose(1))
        );
        // Backspace widens the filter again; ↑/↓ move within what is shown.
        let mut p = Picker::default();
        press(&mut p, &items, KeyCode::Char('x'));
        assert_eq!(p.shown(&items), vec![0]);
        press(&mut p, &items, KeyCode::Backspace);
        assert_eq!(p.shown(&items), vec![0, 1, 2]);
        press(&mut p, &items, KeyCode::Down);
        press(&mut p, &items, KeyCode::Down);
        press(&mut p, &items, KeyCode::Down);
        assert_eq!(press(&mut p, &items, KeyCode::Enter), PickerStep::Choose(2));
        press(&mut p, &items, KeyCode::Up);
        assert_eq!(press(&mut p, &items, KeyCode::Enter), PickerStep::Choose(1));
        // Plain Enter on an untouched picker still picks the highlighted row.
        assert_eq!(
            press(&mut Picker::default(), &items, KeyCode::Enter),
            PickerStep::Choose(0)
        );
    }

    #[test]
    fn digits_select_and_esc_or_ctrl_c_close() {
        let items = items();
        assert_eq!(
            press(&mut Picker::default(), &items, KeyCode::Char('2')),
            PickerStep::Choose(1)
        );
        // A number past the list is filter text, and so is a digit after text.
        let mut p = Picker::default();
        assert_eq!(
            press(&mut p, &items, KeyCode::Char('7')),
            PickerStep::Redraw
        );
        assert_eq!(p.filter, "7");
        let mut p = Picker::default();
        press(&mut p, &items, KeyCode::Char('e'));
        assert_eq!(
            press(&mut p, &items, KeyCode::Char('1')),
            PickerStep::Redraw
        );
        assert_eq!(
            press(&mut Picker::default(), &items, KeyCode::Esc),
            PickerStep::Close
        );
        let mut p = Picker::default();
        press(&mut p, &items, KeyCode::Char('e'));
        assert_eq!(press(&mut p, &items, KeyCode::Esc), PickerStep::Close);
        assert_eq!(
            Picker::default().key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &items
            ),
            PickerStep::Close
        );
        assert_eq!(picker_number(0), "1");
        assert_eq!(picker_number(9), " ");
    }

    #[test]
    fn ctrl_c_quits_only_on_a_second_press_inside_the_window() {
        assert!(!ctrl_c_quits(None, 10_000));
        assert!(ctrl_c_quits(Some(10_000), 10_400));
        assert!(ctrl_c_quits(Some(10_000), 12_000));
        assert!(!ctrl_c_quits(Some(10_000), 12_001));
        assert!(QUIT_HINT.contains("press Ctrl+C again to quit (Ctrl+D quits now)"));
    }

    #[test]
    fn an_open_prompt_owns_the_box_and_the_keyboard() {
        let owner = InputOwner::open("  allow? ", &['y'], 1);
        let open = top_open_input().expect("prompt registered");
        assert_eq!(open.prompt, "  allow? ");
        assert_eq!(open.buf, vec!['y']);
        update_open_input("  allow? ", &['y', 'e'], 2);
        assert_eq!(top_open_input().unwrap().buf, vec!['y', 'e']);
        // The typeahead thread cannot take keys while the prompt is open.
        let other = std::thread::spawn(|| keyboard_try_lock().is_some())
            .join()
            .unwrap();
        assert!(!other, "typeahead drained keys from an open prompt");
        drop(owner);
        assert!(top_open_input().is_none());
        let other = std::thread::spawn(|| keyboard_try_lock().is_some())
            .join()
            .unwrap();
        assert!(other);
    }
}

#[cfg(test)]
mod theme_tests {
    use super::*;

    fn hex(c: Col) -> (f64, f64, f64) {
        match c {
            Col::Rgb(r, g, b) => (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0),
            other => panic!("not a 24-bit colour: {other:?}"),
        }
    }

    fn contrast(a: Col, b: Col) -> f64 {
        let lum = |c: Col| {
            let (r, g, b) = hex(c);
            let f = |c: f64| {
                if c <= 0.03928 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
        };
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn light_theme_text_is_readable_on_light_backgrounds() {
        let p = &LIGHT;
        let fgs = [
            ("text", p.text),
            ("accent", p.accent),
            ("muted", p.muted),
            ("success", p.success),
            ("warning", p.warning),
            ("error", p.error),
            ("info", p.info),
            ("mode_plan", p.mode_plan),
            ("mode_build", p.mode_build),
            ("mode_bstorm", p.mode_bstorm),
        ];
        let bgs = [
            ("white", Col::Rgb(0xff, 0xff, 0xff)),
            ("solarized light", Col::Rgb(0xfd, 0xf6, 0xe3)),
            ("light grey", Col::Rgb(0xee, 0xee, 0xee)),
            ("diff add", p.diff_add_bg),
            ("diff del", p.diff_del_bg),
        ];
        let marks = p.wordmark.map(|c| ("wordmark", c));
        for (fname, fg) in fgs.iter().chain(marks.iter()) {
            for (bname, bg) in bgs {
                let r = contrast(*fg, bg);
                assert!(r >= 4.5, "{fname} on {bname}: {r:.2}:1");
            }
        }
        // Word emphasis inside a diff row, and a drag selection, are text on
        // their tint.
        assert!(contrast(p.text, p.selection_bg) >= 4.5);
        assert!(contrast(p.text, p.diff_add_emph_bg) >= 4.5);
        assert!(contrast(p.text, p.diff_del_emph_bg) >= 4.5);
        // Dark keeps its own background and the 4.5:1 floor for muted text.
        let dark_bg = DARK.background.unwrap();
        assert!(contrast(DARK.text, dark_bg) >= 4.5);
        assert!(contrast(DARK.muted, dark_bg) >= 4.5);
        assert!(LIGHT.background.is_none() && ANSI.background.is_none());
    }

    #[test]
    fn theme_names_and_background_hints_are_understood() {
        assert_eq!(theme_index("light"), Ok(Some(1)));
        assert_eq!(theme_index(" ANSI "), Ok(Some(2)));
        assert_eq!(theme_index("auto"), Ok(None));
        assert_eq!(theme_index(""), Ok(None));
        assert!(theme_index("solarized")
            .unwrap_err()
            .contains("dark, light, ansi or auto"));
        assert!(colorfgbg_is_light(Some("0;15")));
        assert!(colorfgbg_is_light(Some("0;default;7")));
        assert!(!colorfgbg_is_light(Some("15;0")));
        assert!(!colorfgbg_is_light(None));
        // OSC 11 replies: BEL or ST endings, 4- and 2-digit channels, and a
        // reply followed by the DA1 answer that ends the query.
        assert_eq!(
            osc11_is_light(b"\x1b]11;rgb:ffff/ffff/ffff\x07"),
            Some(true)
        );
        assert_eq!(
            osc11_is_light(b"\x1b]11;rgb:1a1a/1b1b/2626\x1b\\\x1b[?62;4c"),
            Some(false)
        );
        assert_eq!(osc11_is_light(b"\x1b]11;rgb:fd/f6/e3\x07"), Some(true));
        assert_eq!(osc11_is_light(b"\x1b[?62;4c"), None);
        // Each kind of colour becomes its own SGR form.
        assert_eq!(sgr_fg(Col::Ansi(32)), "32");
        assert_eq!(sgr_bg(Col::Ansi(32)), "42");
        assert_eq!(sgr_fg(Col::Default), "39");
    }

    #[test]
    fn wrapped_rows_break_between_words_and_keep_the_indent() {
        let hint = "    y yes · n no · s allow `python3 -m pytest -q` this session · a always · d <reason> deny";
        let rows = wrap_ansi_line(hint, 60);
        assert_eq!(
            rows.iter().map(|r| strip_ansi(r)).collect::<Vec<_>>(),
            vec![
                "    y yes · n no · s allow `python3 -m pytest -q` this ",
                "    session · a always · d <reason> deny",
            ]
        );
        for r in &rows {
            assert!(str_width(&strip_ansi(r)) <= 60);
        }
        // A word longer than the row is cut where the row ends.
        let rows = wrap_ansi_line("aaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbb", 10);
        assert_eq!(strip_ansi(&rows[0]), "aaaaaaaaaa");
        assert!(rows.iter().all(|r| str_width(&strip_ansi(r)) <= 10));
        assert_eq!(
            rows.iter().map(|r| strip_ansi(r)).collect::<String>(),
            "aaaaaaaaaabbbbbbbbbbbbbbbbbbbbbbbb"
        );
        // Colour that is open at the break carries over to the next row.
        let rows = wrap_ansi_line("\x1b[31mred words here\x1b[0m", 8);
        assert_eq!(strip_ansi(&rows[0]), "red ");
        assert!(rows[1].starts_with("\x1b[31m"), "{:?}", rows[1]);
        // Short lines are untouched.
        assert_eq!(wrap_ansi_line("  fits", 80), vec!["  fits".to_string()]);
    }

    #[test]
    fn lines_cut_to_the_width_end_with_an_ellipsis() {
        let foot = "mock-coder permission: ask · /permissions · wheel/PgUp · drag-copy · /mouse";
        let cut = strip_ansi(&ellipsize_ansi_line(foot, 60));
        assert_eq!(str_width(&cut), 60);
        assert!(cut.ends_with('…'), "{cut}");
        assert_eq!(ellipsize_ansi_line("short", 60), "short");
        assert_eq!(strip_ansi(&ellipsize_ansi_line("你好世界", 5)), "你好…");
    }
}

// ── todo checklist ───────────────────────────────────────────────────────────
/// The agent's todo list (task, status) as a checklist: a count line, then
/// ✓ for completed, ▸ for the item in progress, ○ for pending. Task text
/// comes from the model, so its escapes are neutralized.
pub fn todo_checklist(items: &[(String, String)]) -> String {
    let done = items.iter().filter(|(_, s)| s == "completed").count();
    let mut rows = vec![dim(&format!("  ☰ todo · {done} of {} done", items.len()))];
    for (task, status) in items {
        let task = sanitize_terminal(task);
        rows.push(match status.as_str() {
            "completed" => format!("    {} {}", green("✓"), dim(&task)),
            "in_progress" => format!("    {} {}", accent("▸"), bold(&task)),
            _ => format!("    {} {task}", dim("○")),
        });
    }
    rows.join("\n")
}

#[cfg(test)]
mod paste_tests {
    use super::*;

    #[test]
    fn a_big_paste_shows_as_a_token_and_is_sent_in_full() {
        assert_eq!(thousands(20024), "20,024");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000_000), "1,000,000");
        // Small pastes are typed in as text.
        assert_eq!(collapse_paste("a short note"), None);
        let big = format!("line one\r\n{}\x1b[31m\nend", "x".repeat(20_000));
        let token = collapse_paste(&big).expect("collapsed");
        assert!(
            token.starts_with("[pasted 20,0") && token.ends_with("chars]"),
            "{token}"
        );
        // Many short lines collapse too, and line breaks survive the trip.
        let lines = (1..=12)
            .map(|i| format!("row {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let second = collapse_paste(&lines).expect("collapsed");
        assert_ne!(token, second, "each paste gets its own token");
        let sent = expand_pastes(&format!("{token} summarize this, and {second}"));
        assert!(sent.starts_with("line one\nxxxx"), "{}", &sent[..20]);
        assert!(sent.contains("end summarize this, and row 1\nrow 2"));
        assert!(!sent.contains('\x1b') && !sent.contains('\r'));
        // Text without a token goes out unchanged.
        assert_eq!(expand_pastes("[pasted by hand]"), "[pasted by hand]");
    }
}

#[cfg(test)]
mod todo_tests {
    use super::*;

    #[test]
    fn todo_list_renders_as_a_checklist_that_ticks_items() {
        let items = |statuses: [&str; 3]| -> Vec<(String, String)> {
            ["Create pkg/ package", "Move logic", "Run the tests"]
                .iter()
                .zip(statuses)
                .map(|(t, s)| (t.to_string(), s.to_string()))
                .collect()
        };
        let first = strip_ansi(&todo_checklist(&items([
            "in_progress",
            "pending",
            "pending",
        ])));
        assert_eq!(
            first,
            "  ☰ todo · 0 of 3 done\n    ▸ Create pkg/ package\n    ○ Move logic\n    ○ Run the tests"
        );
        let later = strip_ansi(&todo_checklist(&items([
            "completed",
            "completed",
            "in_progress",
        ])));
        assert!(later.starts_with("  ☰ todo · 2 of 3 done"), "{later}");
        assert!(later.contains("✓ Create pkg/ package") && later.contains("▸ Run the tests"));
        // Escapes in model-written task text never reach the terminal.
        let hostile = todo_checklist(&[("a\x1b]0;title\x07b".into(), "pending".into())]);
        assert!(!hostile.contains("\x1b]0;"), "{hostile:?}");
    }
}

// ── input event ──────────────────────────────────────────────────────────────
// Returned from ask_task so the REPL can distinguish a submitted line from a
// mode-cycle request (Shift+Tab) without passing mutable mode state into tui.
pub enum InputEvent {
    Text(String),
    CycleMode,
}

// ── single-line ask ──────────────────────────────────────────────────────────
/// Asks one question and returns the answer. While it is open the prompt
/// owns the keyboard and the input box. Esc, Ctrl+C, and Ctrl+D on an empty
/// line return None: callers treat None as cancel, never as an empty answer.
/// Without a terminal it reads one line from stdin (None at end of input).
pub fn ask(prompt: &str) -> Option<String> {
    let prompt = &*sanitize_prompt(prompt);
    let _pause_guard = PauseAgentRunningGuard::new();
    if is_raw() {
        match read_line_raw(prompt) {
            None => None,
            Some(RawLine::Submit(s, _)) => Some(s),
            Some(RawLine::CycleMode(_, _)) => None,
        }
    } else if io::stdin().is_terminal() && io::stdout().is_terminal() {
        match read_line_plain(prompt, Vec::new(), false) {
            Some(RawLine::Submit(s, _)) => Some(s),
            _ => None,
        }
    } else {
        print!("{prompt}");
        flush();
        let mut buf = String::new();
        let n = io::stdin().lock().read_line(&mut buf).unwrap_or(0);
        if n == 0 {
            INPUT_CLOSED.store(true, Ordering::Relaxed);
            return None;
        }
        Some(buf.trim_end_matches(['\n', '\r']).to_string())
    }
}

// ── secret ask ───────────────────────────────────────────────────────────────
/// Reads an API key without echoing it: each character shows as a dot while
/// typing, and the submitted line keeps only the masked form, so the key
/// never reaches the screen or the scrollback. Esc, Ctrl+C, and Ctrl+D on an
/// empty line return None (cancel). Without a terminal it reads one line.
pub fn ask_secret(prompt: &str) -> Option<String> {
    let prompt = &*sanitize_prompt(prompt);
    let _pause_guard = PauseAgentRunningGuard::new();
    if !io::stdin().is_terminal() {
        print!("{prompt}");
        flush();
        let mut buf = String::new();
        let n = io::stdin().lock().read_line(&mut buf).unwrap_or(0);
        if n == 0 {
            return None;
        }
        return Some(buf.trim_end_matches(['\n', '\r']).to_string());
    }
    // A cooked terminal echoes every byte itself (setup runs before the
    // session enters raw mode), so raw mode is on for the read either way.
    let raw_here = !is_raw() && enable_raw_mode().is_ok();
    let alt = ALT_SCREEN.load(Ordering::Relaxed);
    if alt {
        cursor_show();
    }
    let mut scroll = 0usize;
    let answer = read_secret(
        || read().ok(),
        |n| {
            if alt {
                let dots = vec!['•'; n];
                render_composer(prompt, &dots, n, &mut scroll);
            } else {
                let width = crossterm::terminal::size().map_or(80, |(w, _)| w as usize);
                print!("{}", secret_frame(prompt, n, width));
                flush();
            }
        },
    );
    let shown = answer.as_deref().map(secret_echo).unwrap_or_default();
    if alt {
        SCROLL_OFFSET.store(0, Ordering::Relaxed);
        clear_composer();
        line(&format!("{prompt}{shown}"));
    } else {
        print!("\r{prompt}{shown}\x1b[K\r\n");
        flush();
    }
    if raw_here {
        let _ = disable_raw_mode();
    }
    answer
}

// The key loop behind ask_secret, apart from the terminal: `next` yields
// input events (None ends the read as a cancel) and `draw` is only ever told
// how many characters there are, never what they are.
fn read_secret(
    mut next: impl FnMut() -> Option<Event>,
    mut draw: impl FnMut(usize),
) -> Option<String> {
    let mut buf: Vec<char> = Vec::new();
    draw(0);
    loop {
        match next()? {
            Event::Paste(s) => buf.extend(s.chars().filter(|c| !c.is_control())),
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                match k.code {
                    KeyCode::Enter => return Some(buf.into_iter().collect()),
                    KeyCode::Esc => return None,
                    KeyCode::Char('c') if ctrl => return None,
                    KeyCode::Char('d') if ctrl && buf.is_empty() => return None,
                    KeyCode::Char('u') if ctrl => buf.clear(),
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) if !ctrl && !c.is_control() => buf.push(c),
                    _ => continue,
                }
            }
            Event::Resize(..) => {}
            _ => continue,
        }
        draw(buf.len());
    }
}

// One line-mode repaint of the secret prompt: the prompt and a dot per
// character, cut to the terminal width so a long key never wraps the row.
fn secret_frame(prompt: &str, n: usize, width: usize) -> String {
    let room = width
        .saturating_sub(prompt_width(prompt) as usize)
        .saturating_sub(1);
    format!("\r{prompt}{}\x1b[K", "•".repeat(n.min(room)))
}

// What stays on screen after Enter: the masked key, or nothing for an
// empty answer.
fn secret_echo(key: &str) -> String {
    let key = key.trim();
    if key.is_empty() {
        String::new()
    } else {
        crate::config::mask(key)
    }
}

// Multi-line task input. A trailing `\` + Enter adds another line; plain Enter
// submits. Shift+Tab returns CycleMode without submitting.
// Pre-fills the first line with any keystrokes typed during agent processing.
#[derive(Default)]
struct TaskDraft {
    lines: Vec<String>,
    buf: Vec<char>,
    cursor: usize,
}

fn task_draft() -> &'static Mutex<Option<TaskDraft>> {
    static DRAFT: std::sync::OnceLock<Mutex<Option<TaskDraft>>> = std::sync::OnceLock::new();
    DRAFT.get_or_init(|| Mutex::new(None))
}

pub fn ask_task(prompt: &str) -> Option<InputEvent> {
    let prompt = &*sanitize_prompt(prompt);
    // Take the message out inside a tight block: echo_submitted → line →
    // render_output re-locks the queue, and std Mutex is not reentrant.
    let queued = message_queue()
        .lock()
        .ok()
        .and_then(|mut mq| (!mq.is_empty()).then(|| mq.remove(0)));
    if let Some(msg) = queued {
        push_history(&msg);
        echo_submitted(prompt, &msg);
        return Some(InputEvent::Text(expand_pastes(&msg)));
    }
    if !is_raw() {
        return ask(prompt).map(InputEvent::Text);
    }
    // A mode change must preserve both the current line/cursor and any
    // completed continuation lines. Keep this separate from queued messages.
    let mut draft = task_draft()
        .lock()
        .ok()
        .and_then(|mut d| d.take())
        .unwrap_or_else(|| {
            let (buf, cursor) = take_typeahead();
            TaskDraft {
                buf,
                cursor,
                ..TaskDraft::default()
            }
        });
    let mut p = if draft.lines.is_empty() {
        prompt.to_string()
    } else {
        format!("{} ", dim("…"))
    };
    // Use the typeahead buffer to pre-fill only the very first read.
    let mut first = Some((std::mem::take(&mut draft.buf), draft.cursor));
    loop {
        let rl = if let Some((pf, pc)) = first.take() {
            read_line_raw_prefill(&p, pf, pc, true)
        } else {
            read_line_raw_prefill(&p, Vec::new(), 0, true)
        };
        match rl? {
            RawLine::CycleMode(buf, cursor) => {
                draft.buf = buf;
                draft.cursor = cursor;
                if let Ok(mut saved) = task_draft().lock() {
                    *saved = Some(draft);
                }
                return Some(InputEvent::CycleMode);
            }
            RawLine::Submit(text, cont) => {
                draft.lines.push(text);
                if !cont {
                    let acc = draft.lines.join("\n");
                    push_history(&acc);
                    return Some(InputEvent::Text(expand_pastes(&acc)));
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
    CycleMode(Vec<char>, usize),
}

fn viewport(buf: &[char], cursor: usize, avail: usize, scroll: usize) -> (usize, usize) {
    let avail = avail.max(1);
    let mut s = scroll;
    if cursor < s {
        s = cursor;
    } else {
        let mut cur_width: usize = buf[s..cursor].iter().copied().map(char_width).sum();
        while cur_width >= avail && s < cursor {
            cur_width -= char_width(buf[s]);
            s += 1;
        }
    }
    (s, buf[s..cursor].iter().copied().map(char_width).sum())
}

fn redraw(prompt: &str, start: (u16, u16), buf: &[char], cursor: usize, scroll: &mut usize) {
    if ALT_SCREEN.load(Ordering::Relaxed) {
        update_open_input(prompt, buf, cursor);
        render_composer(prompt, buf, cursor, scroll);
        return;
    }
    let width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
    let avail = width.saturating_sub(start.0).max(8) as usize;
    let (s, _col) = viewport(buf, cursor, avail, *scroll);
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
    "/login",
    "/plan",
    "/build",
    "/brainstorm",
    "/doctor",
    "/debug",
    "/mode",
    "/model",
    "/permissions",
    "/sandbox",
    "/mcp",
    "/scroll",
    "/mouse",
    "/compact",
    "/cost",
    "/effort",
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
    "/theme",
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

/// The built-in commands the popup lists (a test holds the REPL to them).
#[cfg(test)]
pub(crate) fn builtin_slash_commands() -> &'static [&'static str] {
    SLASH_COMMANDS_BASE
}

// Cached: the autocomplete popup consults this on every keystroke, and the
// skill/command set doesn't change within a session.
fn load_slash_commands() -> Vec<String> {
    #[allow(clippy::type_complexity)]
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<(u64, Vec<String>)>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(None));
    let now = monotonic_ms();
    let mut lock = cache.lock().unwrap();
    if let Some((ts, cmds)) = &*lock {
        if now.saturating_sub(*ts) < 5_000 {
            return cmds.clone();
        }
    }
    let cmds = load_slash_commands_uncached();
    *lock = Some((now, cmds.clone()));
    cmds
}

fn load_slash_commands_uncached() -> Vec<String> {
    let mut cmds: Vec<String> = SLASH_COMMANDS_BASE.iter().map(|s| s.to_string()).collect();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    for skill in crate::config::discover_skills(&cwd) {
        let cmd = format!("/{}", skill.name);
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

// Replace the whole token, including text to the right of the cursor.
// Replacing only the prefix turns /he|lp into /helplp on completion.
fn apply_completion(buf: &mut Vec<char>, cursor: &mut usize, candidate: &str, space: bool) {
    let (start, _) = token_at(buf, *cursor);
    let mut end = *cursor;
    while end < buf.len() && !buf[end].is_whitespace() {
        end += 1;
    }
    buf.splice(start..end, candidate.chars());
    *cursor = start + candidate.chars().count();
    if space && !candidate.ends_with('/') && !candidate.ends_with(':') {
        if !buf.get(*cursor).is_some_and(|c| c.is_whitespace()) {
            buf.insert(*cursor, ' ');
        }
        *cursor += 1;
    }
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
// User-defined commands and skills get an empty description here; see
// extra_command_desc for those.
fn slash_command_desc(cmd: &str) -> &'static str {
    match cmd {
        "/help" => "show all commands and keys",
        "/clear" => "clear the screen",
        "/new" => "start a fresh session",
        "/resume" => "pick a saved session to resume",
        "/init" => "reconfigure provider, model, and key",
        "/login" => "replace the API key (checked before it is saved)",
        "/plan" => "switch to PLAN mode",
        "/build" => "switch to BUILD mode",
        "/brainstorm" => "switch to BRAINSTORM mode",
        "/doctor" | "/debug" => "diagnose setup and connectivity",
        "/mode" => "show or switch mode",
        "/model" => "hot-swap the AI model",
        "/permissions" => "tool permission level (ask/auto/readonly)",
        "/sandbox" => "OS sandbox for shell commands (off/auto/require)",
        "/mcp" => "MCP servers: list, <name>, add, remove, reload",
        "/scroll" => "wheel scrolling on/off",
        "/mouse" => "mouse capture on/off",
        "/compact" => "compress context to free token budget",
        "/review" => "AI code review of staged git diff",
        "/commit" => "AI-drafted conventional commit message",
        "/pr" => "AI-drafted PR title + description",
        "/diff" => "show current git diff summary",
        "/context" => "show context window usage",
        "/cost" => "session tokens and estimated cost",
        "/effort" => "reasoning depth (off/low/medium/high)",
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
        "/undo" | "/rewind" => "revert the last agent turn (or latest/git/all/<id>)",
        "/vim" => "toggle vim editing mode",
        "/theme" => "colour theme: dark, light, ansi or auto",
        "/voice" => "voice input",
        "/local" => "probe local servers and list GGUF models",
        "/rules" => "manage project rules",
        "/kb" | "/index" => "query or index project knowledge base",
        "/verify" | "/audit" => "verify recent changes",
        "/grill-me" => "operational alignment interview",
        "/teamwork" => "multi-agent swarm preview",
        "/exit" | "/quit" => "exit the session",
        _ => "",
    }
}

// Descriptions for the non-builtin popup entries: a skill's first line (via
// the skill loader) and "custom command" for scripts. Loaded once per
// session — the popup consults this on every keystroke, so no file reads
// after the first call.
fn extra_command_desc(cmd: &str) -> String {
    static CACHE: std::sync::OnceLock<std::collections::HashMap<String, String>> =
        std::sync::OnceLock::new();
    let map = CACHE.get_or_init(|| {
        let mut map = std::collections::HashMap::new();
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        for (name, desc) in crate::config::load_skill_descriptions(&cwd) {
            if !desc.is_empty() {
                map.insert(format!("/{name}"), desc);
            }
        }
        for c in crate::config::load_custom_commands() {
            let key = format!("/{}", c.name);
            if c.script.is_some() {
                map.insert(key, "custom command".to_string());
            } else if let std::collections::hash_map::Entry::Vacant(slot) = map.entry(key) {
                slot.insert(custom_command_desc(&c));
            }
        }
        map
    });
    map.get(cmd).cloned().unwrap_or_default()
}

// A prompt command's popup text: its `description:` frontmatter, else the
// first prose line of its body (CustomCommand.description holds either; the
// body itself no longer carries the frontmatter).
fn custom_command_desc(c: &crate::config::CustomCommand) -> String {
    if c.description.trim().is_empty() {
        "custom command".to_string()
    } else {
        c.description.clone()
    }
}

// Popup description for any candidate: builtin text first, then the cached
// skill / custom-command description.
fn popup_desc(cand: &str) -> String {
    let builtin = slash_command_desc(cand);
    if !builtin.is_empty() {
        return builtin.to_string();
    }
    if cand.starts_with('/') {
        extra_command_desc(cand)
    } else {
        String::new()
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
    let interesting = token.starts_with('/') || token.starts_with('@') || buf.first() == Some(&'/');
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
    invalidate_inline_pixels();
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
        // Skill and command descriptions are read from project files.
        let desc = popup_desc(cand);
        let desc = &*sanitize_terminal(&desc);
        let counter = if sug.len() > show && idx == sel {
            dim(&format!(" ({}/{})", sel + 1, sug.len()))
        } else {
            String::new()
        };
        let entry = if idx == sel {
            format!(
                "  {} {}  {}{}",
                accent("›"),
                bold(&padded),
                dim(desc),
                counter
            )
        } else {
            format!("    {padded}  {}", dim(desc))
        };
        let _ = write!(out, "{}", ellipsize_ansi_line(&entry, width as usize));
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
    let mut completion_partial = partial;
    let mut range_suffix = "";
    if let Some(idx) = completion_partial.rfind(':') {
        if completion_partial[idx + 1..]
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-')
        {
            range_suffix = &completion_partial[idx..];
            completion_partial = &completion_partial[..idx];
        }
    }
    let (base, dir, prefix) = match completion_partial.rfind('/') {
        Some(i) => (
            &completion_partial[..=i],
            cwd.join(&completion_partial[..=i]),
            &completion_partial[i + 1..],
        ),
        None => ("", cwd.to_path_buf(), completion_partial),
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
                } else {
                    full.push_str(range_suffix);
                }
                out.push(full);
            }
        }
    }
    out.sort();
    // A bare name (no folder typed) also finds files deeper in the tree:
    // @file_4999 offers src/mod49/sub9/file_4999.py. Same walk rules as
    // find_files, so ignored and sensitive files are never offered.
    if base.is_empty() && prefix.chars().count() >= 2 {
        for deep in crate::tools::rank_by_name(&project_files_cached(cwd), prefix, 20) {
            let full = format!("{deep}{range_suffix}");
            if !out.contains(&full) {
                out.push(full);
            }
        }
    }
    out
}

// The project's file list for `@` completion, walked at most every five
// seconds: the popup asks on every keystroke.
fn project_files_cached(cwd: &std::path::Path) -> Vec<String> {
    #[allow(clippy::type_complexity)]
    static CACHE: OnceLock<Mutex<Option<(u64, std::path::PathBuf, Vec<String>)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let now = monotonic_ms();
    if let Ok(c) = cache.lock() {
        if let Some((ts, dir, files)) = &*c {
            if dir == cwd && now.saturating_sub(*ts) < 5_000 {
                return files.clone();
            }
        }
    }
    let files = crate::tools::project_files(cwd);
    if let Ok(mut c) = cache.lock() {
        *c = Some((now, cwd.to_path_buf(), files.clone()));
    }
    files
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

// Candidates come from file names, skill folders and rule ids in the
// checkout. One with escapes or bidi chars would be echoed raw by the popup
// and then by the composer once completed, so it is never offered.
fn completions(buf: &[char], start: usize, token: &str) -> Vec<String> {
    drop_unsafe_candidates(completions_unfiltered(buf, start, token))
}

fn drop_unsafe_candidates(mut cands: Vec<String>) -> Vec<String> {
    cands.retain(|c| matches!(sanitize_terminal(c), std::borrow::Cow::Borrowed(_)));
    cands
}

fn completions_unfiltered(buf: &[char], start: usize, token: &str) -> Vec<String> {
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
        "/sandbox" => {
            return ["off", "auto", "require", "status"]
                .iter()
                .filter(|&&s| s.starts_with(token))
                .map(|s| s.to_string())
                .collect();
        }
        "/mcp" => {
            return ["add", "remove", "reload"]
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
        "/theme" => {
            return THEME_NAMES
                .iter()
                .chain(&["auto"])
                .filter(|&&s| s.starts_with(token))
                .map(|s| s.to_string())
                .collect();
        }
        "/effort" => {
            return crate::config::Effort::LEVELS
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
    read_line_raw_prefill(prompt, vec![], 0, false)
}

// Ctrl+C on an empty composer: the first press says how to quit, and only a
// second one inside the window quits — so the press after "stop that turn"
// never closes the session and loses its approvals and undo marker.
const QUIT_HINT: &str = "  press Ctrl+C again to quit (Ctrl+D quits now)";
const QUIT_PRESS_WINDOW_MS: u64 = 2_000;

fn ctrl_c_quits(armed_at: Option<u64>, now: u64) -> bool {
    armed_at.is_some_and(|t| now.saturating_sub(t) <= QUIT_PRESS_WINDOW_MS)
}

// Raw mode for one read from a cooked terminal (setup, or /init after the
// session left the alternate screen), so Esc and Ctrl+C reach the prompt as
// keys instead of a SIGINT that ends the process.
struct RawForRead(bool);

impl RawForRead {
    fn on() -> Self {
        RawForRead(!is_raw() && enable_raw_mode().is_ok())
    }
}

impl Drop for RawForRead {
    fn drop(&mut self) {
        if self.0 {
            let _ = disable_raw_mode();
        }
    }
}

// Line editor for raw input outside the alternate screen (line mode, and
// prompts asked from a cooked terminal). It echoes what is typed and erases
// with backspace-space-backspace only: no cursor addressing and no other
// escape sequences, so it reads cleanly on TERM=dumb and to a screen reader.
// Keys follow the composer and prompt rules of read_line_raw_prefill.
fn read_line_plain(prompt: &str, prefill: Vec<char>, composer: bool) -> Option<RawLine> {
    let _raw = RawForRead::on();
    let _keys = keyboard_lock();
    let mut out = io::stdout();
    let mut buf = prefill;
    let _ = write!(out, "{prompt}{}", buf.iter().collect::<String>());
    let _ = out.flush();
    let mut quit_armed_at: Option<u64> = None;
    let mut hist_idx: Option<usize> = None;
    let erase = |out: &mut io::Stdout, chars: &[char]| {
        let w: usize = chars.iter().copied().map(char_width).sum();
        let _ = write!(out, "{}", "\x08 \x08".repeat(w));
    };
    loop {
        let ev = match read() {
            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => k,
            Ok(Event::Paste(s)) => {
                let chars = sanitize_paste(&s);
                let _ = write!(out, "{}", chars.iter().collect::<String>());
                buf.extend(chars);
                let _ = out.flush();
                continue;
            }
            Ok(_) => continue,
            Err(_) => {
                INPUT_CLOSED.store(true, Ordering::Relaxed);
                let _ = write!(out, "\r\n");
                let _ = out.flush();
                return None;
            }
        };
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        if !(ctrl && ev.code == KeyCode::Char('c')) {
            quit_armed_at = None;
        }
        match ev.code {
            KeyCode::Esc | KeyCode::Char('c') if !composer && (ctrl || ev.code == KeyCode::Esc) => {
                let _ = write!(out, " {}\r\n", dim("cancelled"));
                let _ = out.flush();
                return None;
            }
            KeyCode::Char('d') if ctrl && buf.is_empty() => {
                let _ = write!(out, "\r\n");
                let _ = out.flush();
                return None;
            }
            KeyCode::Char('c') if ctrl => {
                if !buf.is_empty() {
                    erase(&mut out, &buf);
                    buf.clear();
                } else if ctrl_c_quits(quit_armed_at, monotonic_ms()) {
                    let _ = write!(out, "\r\n");
                    let _ = out.flush();
                    return None;
                } else {
                    quit_armed_at = Some(monotonic_ms());
                    let _ = write!(out, "\r\n{}\r\n{prompt}", yellow(QUIT_HINT));
                }
            }
            KeyCode::Esc => {
                erase(&mut out, &buf);
                buf.clear();
            }
            KeyCode::BackTab if composer => {
                let _ = write!(out, "\r\n");
                let _ = out.flush();
                let cursor = buf.len();
                return Some(RawLine::CycleMode(buf, cursor));
            }
            KeyCode::Enter => {
                let cont = composer && buf.last() == Some(&'\\');
                if cont {
                    buf.pop();
                }
                let _ = write!(out, "\r\n");
                let _ = out.flush();
                return Some(RawLine::Submit(buf.into_iter().collect(), cont));
            }
            KeyCode::Backspace => {
                if let Some(c) = buf.pop() {
                    erase(&mut out, &[c]);
                }
            }
            KeyCode::Char('u') if ctrl => {
                erase(&mut out, &buf);
                buf.clear();
            }
            KeyCode::Char('w') if ctrl => {
                let i = prev_word(&buf, buf.len());
                erase(&mut out, &buf[i..]);
                buf.truncate(i);
            }
            KeyCode::Up | KeyCode::Down if composer => {
                let entry = history().lock().ok().map(|h| {
                    let next = match (ev.code, hist_idx) {
                        (KeyCode::Up, None) => h.len().checked_sub(1),
                        (KeyCode::Up, Some(i)) => Some(i.saturating_sub(1)),
                        (_, Some(i)) if i + 1 < h.len() => Some(i + 1),
                        _ => None,
                    };
                    hist_idx = next;
                    next.map(|i| h[i].clone()).unwrap_or_default()
                });
                if let Some(entry) = entry {
                    erase(&mut out, &buf);
                    buf = sanitize_paste(&entry);
                    let _ = write!(out, "{}", buf.iter().collect::<String>());
                }
            }
            KeyCode::Char(c) if !ctrl && !ev.modifiers.contains(KeyModifiers::ALT) => {
                buf.push(c);
                let _ = write!(out, "{c}");
            }
            _ => {}
        }
        let _ = out.flush();
    }
}

// `composer` is the session's main input (ask_task): it cycles modes, shows
// the autocomplete popup, and quits on a second Ctrl+C. Anything else is a
// prompt (ask): Esc and Ctrl+C cancel it and return None.
fn read_line_raw_prefill(
    prompt: &str,
    prefill: Vec<char>,
    prefill_cur: usize,
    composer: bool,
) -> Option<RawLine> {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return read_line_plain(prompt, prefill, composer);
    }
    let mut start = (prompt_width(prompt) + COMPOSER_PAD, composer_row());
    let mut buf: Vec<char> = prefill;
    let mut cursor = prefill_cur.min(buf.len());
    let mut scroll = 0usize;
    // From here until this returns, the keyboard and the input box belong to
    // this prompt; `release!` hands them back before the answer is echoed.
    let mut owner = Some(InputOwner::open(prompt, &buf, cursor));
    macro_rules! release {
        () => {
            drop(owner.take())
        };
    }
    // A first Ctrl+C on an empty composer only arms quitting (see ctrl_c_quits).
    let mut quit_armed_at: Option<u64> = None;
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
            start = (prompt_width(prompt) + COMPOSER_PAD, composer_row());
            redraw(prompt, start, &buf, cursor, &mut scroll);
        }};
    }

    loop {
        // Refresh the autocomplete popup against the current buffer. Runs
        // before the blocking read so the popup tracks every edit (including
        // prefilled text on the first pass). Prompts get no popup: their
        // answers are words like `y`, and Esc there means cancel.
        if ALT_SCREEN.load(Ordering::Relaxed) {
            if sug_suppressed && buf != sug_dismissed_at {
                sug_suppressed = false;
            }
            let cands = if sug_suppressed || !composer {
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
                // Drag a screenshot onto the terminal (or paste its path):
                // the path becomes an @attachment token and the image shows
                // in the transcript immediately — no need to submit first.
                if let Some(p) = pasted_media_path(&s) {
                    for ch in attachment_token(&p).chars() {
                        buf.insert(cursor, ch);
                        cursor += 1;
                    }
                    show_image_file(&p, false);
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                    continue;
                }
                let chars = match collapse_paste(&s).filter(|_| composer) {
                    Some(token) => token.chars().collect(),
                    None => sanitize_paste(&s),
                };
                buf.splice(cursor..cursor, chars.iter().copied());
                cursor += chars.len();
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
                        let target_col = col - start.0 as usize;
                        let mut current_col = 0;
                        let mut target_idx = scroll;
                        while target_idx < buf.len() {
                            let w = char_width(buf[target_idx]);
                            if current_col + w > target_col {
                                break;
                            }
                            current_col += w;
                            target_idx += 1;
                        }
                        cursor = target_idx.min(buf.len());
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
            Ok(Event::FocusGained) => {
                FOCUSED.store(true, Ordering::Relaxed);
                continue;
            }
            Ok(Event::FocusLost) => {
                FOCUSED.store(false, Ordering::Relaxed);
                continue;
            }
            Ok(_) => continue,
            Err(_) => {
                INPUT_CLOSED.store(true, Ordering::Relaxed);
                return None;
            }
        };
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let alt = ev.modifiers.contains(KeyModifiers::ALT);
        if !(ctrl && ev.code == KeyCode::Char('c')) {
            quit_armed_at = None;
        }
        // A prompt is cancelled by Esc or Ctrl+C whatever was typed, and by
        // Ctrl+D on an empty line: the caller gets None and the box says so.
        let cancel = !composer
            && match ev.code {
                KeyCode::Esc => true,
                KeyCode::Char('c') => ctrl,
                KeyCode::Char('d') => ctrl && buf.is_empty(),
                _ => false,
            };
        if cancel {
            release!();
            echo_submitted(prompt, &dim("cancelled"));
            return None;
        }
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
            // Shift+Tab changes mode while retaining the draft. Ordinary
            // questions/approval prompts do not support mode changes.
            KeyCode::BackTab if composer => {
                release!();
                clear_composer();
                flush();
                return Some(RawLine::CycleMode(buf, cursor));
            }
            KeyCode::Tab if ev.modifiers.contains(KeyModifiers::SHIFT) && composer => {
                release!();
                clear_composer();
                flush();
                return Some(RawLine::CycleMode(buf, cursor));
            }
            KeyCode::BackTab => {}
            KeyCode::Tab if ev.modifiers.contains(KeyModifiers::SHIFT) => {}
            // Composer only (a prompt was cancelled above): Ctrl+C clears the
            // draft; on an empty draft it quits only when pressed twice.
            KeyCode::Char('c') if ctrl => {
                if !buf.is_empty() {
                    buf.clear();
                    cursor = 0;
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                } else if ctrl_c_quits(quit_armed_at, monotonic_ms()) {
                    release!();
                    clear_composer();
                    flush();
                    return None;
                } else {
                    quit_armed_at = Some(monotonic_ms());
                    line(&yellow(QUIT_HINT));
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                }
            }
            KeyCode::Char('d') if ctrl => {
                if buf.is_empty() {
                    release!();
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
                    for ch in attachment_token(&img).chars() {
                        buf.insert(cursor, ch);
                        cursor += 1;
                    }
                    // Show the screenshot right now — pixel-perfect on
                    // kitty/Ghostty, half-block art elsewhere — so you can
                    // see what the model will see before you send it.
                    if !show_image_file(&img, false) {
                        line(&dim(&format!("  ⎘ clipboard image → {}", img.display())));
                    }
                } else if let Some(text) = crate::media::clipboard_text() {
                    let chars = sanitize_paste(&text);
                    buf.splice(cursor..cursor, chars.iter().copied());
                    cursor += chars.len();
                }
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('l') if ctrl => reline!(),
            KeyCode::Char('g') if ctrl => {
                let cur: String = buf.iter().collect();
                if let Some(edited) = edit_in_editor(&cur) {
                    if edited.contains('\n') {
                        // A multi-line edit is submitted as-is: the composer is
                        // single-line, and flattening the newlines to spaces
                        // silently destroys the structure the user just wrote.
                        // ask_task joins submitted lines with '\n', so the
                        // text goes through untouched.
                        buf = edited.chars().collect();
                        cursor = buf.len();
                        reline!();
                        release!();
                        echo_submitted(prompt, &edited);
                        return Some(RawLine::Submit(edited, false));
                    }
                    buf = edited.chars().collect();
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
                            let _ = queue!(out, MoveTo(0, start.1), Clear(ClearType::UntilNewLine));
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
                                release!();
                                echo_submitted(prompt, &e);
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
                        // On an empty line `/` starts a command as typed, so
                        // /vim (or any command) works from NORMAL mode.
                        '/' if buf.is_empty() => {
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
                        if let Some(cur) = hist_idx {
                            if buf != h[cur].chars().collect::<Vec<char>>() {
                                hist_idx = None;
                            }
                        }
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
                        if buf != h[cur].chars().collect::<Vec<char>>() {
                            hist_idx = None;
                        } else {
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
                }
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Enter => {
                // Accept the highlighted autocomplete entry first: a
                // line-start /command submits immediately; any other token
                // (sub-argument, @path) is inserted and editing continues.
                // A word already typed in full (`/mode build`, `/exit`) is
                // not a request to complete it: Enter submits as typed.
                let typed_in_full = {
                    let (_, token) = token_at(&buf, cursor);
                    sug.iter().any(|c| c.trim_end() == token)
                };
                if !sug.is_empty() && !typed_in_full {
                    let cand = sug[sug_idx].clone();
                    let (tok_start, token) = token_at(&buf, cursor);
                    let is_cmd = token.starts_with('/')
                        && buf[..tok_start].iter().all(|c| c.is_whitespace());
                    apply_completion(&mut buf, &mut cursor, &cand, !is_cmd);
                    if !is_cmd {
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
                release!();
                if !cont {
                    echo_submitted(prompt, &text);
                }
                return Some(RawLine::Submit(text, cont));
            }
            KeyCode::Tab => {
                if !sug.is_empty() {
                    let cand = sug[sug_idx].clone();
                    apply_completion(&mut buf, &mut cursor, &cand, true);
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                    continue;
                }
                let (tok_start, token) = token_at(&buf, cursor);
                let cands = completions(&buf, tok_start, &token);
                if cands.len() == 1 {
                    let cand = &cands[0];
                    apply_completion(&mut buf, &mut cursor, cand, true);
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                } else if cands.len() > 1 {
                    let common = common_prefix(&cands);
                    if common.chars().count() > token.chars().count() {
                        apply_completion(&mut buf, &mut cursor, &common, false);
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
                let _frame = render_lock();
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
            } else if !line_mode() {
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
    } else if !line_mode() {
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
        let buf = &['a'; 25];
        assert_eq!(viewport(buf, 3, 10, 0), (0, 3));
        assert_eq!(viewport(buf, 20, 10, 0), (11, 9));
        assert_eq!(viewport(buf, 2, 10, 11), (2, 0));
        assert_eq!(viewport(buf, 9, 10, 0), (0, 9));
        assert_eq!(viewport(buf, 15, 10, 11), (11, 4));
    }

    #[test]
    fn viewport_handles_zero_width() {
        let buf = &['a'; 10];
        let (s, col) = viewport(buf, 5, 0, 3);
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
        assert_eq!(selection_range_for(one, 2, &"x".repeat(20)), Some((3, 8)));
        assert_eq!(selection_range_for(one, 1, &"x".repeat(20)), None);

        let many = Selection {
            anchor: SelectPos { row: 1, col: 4 },
            focus: SelectPos { row: 3, col: 2 },
            sticky: false,
        };
        assert_eq!(selection_range_for(many, 1, &"x".repeat(10)), Some((4, 10)));
        assert_eq!(selection_range_for(many, 2, &"x".repeat(10)), Some((0, 10)));
        assert_eq!(selection_range_for(many, 3, &"x".repeat(10)), Some((0, 3)));
    }

    #[test]
    fn selection_uses_terminal_cells_and_keeps_combining_marks() {
        let select = |line: &str, from, to| {
            let sel = Selection {
                anchor: SelectPos { row: 0, col: from },
                focus: SelectPos { row: 0, col: to },
                sticky: false,
            };
            selection_range_for(sel, 0, line)
                .map(|(a, b)| line.chars().skip(a).take(b - a).collect::<String>())
        };
        assert_eq!(select("你 hello", 3, 7).as_deref(), Some("hello"));
        assert_eq!(select("你 hello", 1, 1).as_deref(), Some("你"));
        assert_eq!(select("🙂 ok", 3, 4).as_deref(), Some("ok"));
        assert_eq!(select("e\u{301} ok", 0, 0).as_deref(), Some("e\u{301}"));
        assert_eq!(select("e\u{301} ok", 2, 3).as_deref(), Some("ok"));
        assert_eq!(select("你 hello", 7, 3).as_deref(), Some("hello"));
        assert_eq!(select("short", 20, 25), None);
    }

    #[test]
    fn word_selection_reports_display_columns() {
        assert_eq!(word_span_at("你 hello", 3), Some((3, 7)));
        assert_eq!(word_span_at("你 hello", 1), Some((0, 1)));
        assert_eq!(word_span_at("🙂 hello", 4), Some((3, 7)));
        assert_eq!(word_span_at("e\u{301} hello", 0), Some((0, 0)));
        assert_eq!(word_span_at("你 hello", 2), None);
    }

    #[test]
    fn completion_replaces_suffix_without_duplicating_spaces() {
        let mut buf: Vec<char> = "/help".chars().collect();
        let mut cursor = 3;
        apply_completion(&mut buf, &mut cursor, "/help", true);
        assert_eq!(buf.iter().collect::<String>(), "/help ");
        assert_eq!(cursor, 6);

        let mut buf: Vec<char> = "/mode brainstorm tail".chars().collect();
        let mut cursor = 8;
        apply_completion(&mut buf, &mut cursor, "build", true);
        assert_eq!(buf.iter().collect::<String>(), "/mode build tail");
        assert_eq!(cursor, 12);

        let mut buf: Vec<char> = "read @文档.txt".chars().collect();
        let mut cursor = 7;
        apply_completion(&mut buf, &mut cursor, "@文件/", true);
        assert_eq!(buf.iter().collect::<String>(), "read @文件/");
        assert_eq!(cursor, buf.len());
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
    fn completion_skips_names_with_escapes_or_bidi() {
        use std::fs;
        let d = std::env::temp_dir().join(format!("bwn-comp-evil-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("ok.txt"), "").unwrap();
        fs::write(d.join("o\x1b]0;pwned\x07.txt"), "").unwrap();
        fs::write(d.join("o\u{202E}txt.exe"), "").unwrap();
        let raw = path_candidates("o", &d);
        assert_eq!(raw.len(), 3);
        assert_eq!(drop_unsafe_candidates(raw), vec!["ok.txt".to_string()]);
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
    fn render_md_dim_line_formats_markers_and_stays_muted() {
        // Markers are consumed — no raw ** or # or ` reaches the screen.
        assert_eq!(
            plain(&render_md_dim_line("a **bold** and `code`")),
            "a bold and code"
        );
        assert_eq!(plain(&render_md_dim_line("## a heading")), "a heading");
        assert_eq!(plain(&render_md_dim_line("- a bullet")), "• a bullet");
        assert_eq!(plain(&render_md_dim_line("> a quote")), "│ a quote");

        // Bold is emitted as an attribute toggle (1…22), and the foreground
        // never flips to the bright TEXT color mid-line — the whole line stays
        // in the muted thinking palette.
        let out = render_md_dim_line("plain **bold** plain");
        assert!(out.contains("\x1b[1m"), "bold attribute present: {out:?}");
        let text_fg = sgr_fg(pal().text);
        assert!(
            !out.contains(&text_fg),
            "dim line must never switch to bright TEXT fg: {out:?}"
        );

        // An unbalanced marker stays literal instead of eating the rest.
        assert_eq!(plain(&render_md_dim_line("2 * 3 = 6")), "2 * 3 = 6");
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
    fn sixel_placements_follow_the_visible_rows() {
        let m = |id: usize, r: usize| format!("{IMG_MARK}{id}:{r}");
        let rows = [
            "text".to_string(),
            m(3, 0),
            m(3, 1),
            m(3, 2),
            "after".to_string(),
        ];
        let visible: Vec<&String> = rows.iter().collect();
        // Whole image on screen.
        assert_eq!(sixel_placements(&visible, 5), vec![(3, 1, 0, 3)]);
        // Output area ends mid-image: only the rows that fit.
        assert_eq!(sixel_placements(&visible, 3), vec![(3, 1, 0, 2)]);
        // Scrolled so the image's first rows are above the screen.
        let top = [m(3, 2), m(3, 3), "after".to_string()];
        let visible: Vec<&String> = top.iter().collect();
        assert_eq!(sixel_placements(&visible, 3), vec![(3, 0, 2, 2)]);
        // A registered-image marker line is not a row placeholder.
        assert_eq!(sixel_row(&format!("{IMG_MARK}3")), None);
    }

    #[test]
    fn sixel_rows_crop_to_the_visible_slice() {
        // 2 x 40 pixels at 10-pixel cells: 4 rows.
        let rgb = vec![200u8; 2 * 40 * 3];
        let mut img = InlineImage {
            path: std::path::PathBuf::new(),
            native: (2, 40),
            tier: ImageTier::Sixel,
            made_for: (0, 0, (0, 0)),
            rows: 4,
            blocks: Vec::new(),
            sixel: "FULL".into(),
            pixels: (2, 40, rgb),
            cell_h: 10,
            crop: None,
        };
        assert_eq!(img.sixel_rows(0, 4), "FULL");
        assert!(img.sixel_rows(1, 2).contains("\"1;1;2;20"));
        assert!(img.sixel_rows(3, 5).contains("\"1;1;2;10"));
        assert!(img.sixel_rows(9, 1).is_empty());
    }

    #[test]
    fn image_budget_is_a_thumbnail_on_any_terminal_size() {
        // A wide, tall terminal caps at 80 columns by 16 rows.
        assert_eq!(image_cell_budget_for(240, 60, 4), (80, 16));
        // A standard 80x24 terminal gets half the width, a third of the rows.
        assert_eq!(image_cell_budget_for(80, 24, 4), (38, 5));
        // A tiny terminal still gets a readable minimum.
        assert_eq!(image_cell_budget_for(10, 8, 4), (8, 4));
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
        let expect: Vec<Vec<String>> = t.lines.iter().map(|l| wrap_ansi_line(l, t.width)).collect();
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
                let got: Vec<String> = t.rows_range(start, count).into_iter().cloned().collect();
                let want: Vec<String> = flat.iter().skip(start).take(count).cloned().collect();
                assert_eq!(got, want, "start={start} count={count}");
            }
        }
    }

    #[test]
    fn eat_escape_handles_apc_and_dcs_strings() {
        // kitty APC and tmux DCS payloads must be invisible to width math.
        let apc = "\x1b_Ga=T,f=100,i=1;QUJDRA==\x1b\\x";
        assert_eq!(strip_ansi(apc), "x");
        assert_eq!(str_width(&strip_ansi(apc)), 1);
        let dcs = "\x1bPtmux;\x1b\x1b_Ga=d\x1b\x1b\\\x1b\\y";
        assert_eq!(strip_ansi(dcs), "y");
        // OSC still terminates at BEL.
        assert_eq!(strip_ansi("\x1b]8;;http://x\x07lbl\x1b]8;;\x07"), "lbl");
        // Placeholder cells copy as spaces; their diacritics vanish.
        let row = crate::graphics::placeholder_rows(3, 2, 1, 0).remove(0);
        assert_eq!(strip_ansi(&row), "  ");
        assert_eq!(char_width('\u{0483}'), 0);
        assert_eq!(char_width(crate::graphics::PLACEHOLDER), 1);
    }

    #[test]
    fn wrap_carries_sgr_onto_continuation_rows() {
        let s = format!("{}abcdef", "\x1b[31m");
        let rows = wrap_ansi_line(&s, 3);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], "\x1b[31mabc");
        assert_eq!(rows[1], "\x1b[31mdef");
        // A reset clears the carried state.
        let s = "\x1b[31mab\x1b[0mcdef";
        let rows = wrap_ansi_line(s, 3);
        assert_eq!(rows[1], "def");
        // An image row wrapped in a narrow terminal keeps its id colour.
        let row = crate::graphics::placeholder_rows(9, 4, 1, 0).remove(0);
        let rows = wrap_ansi_line(&row, 2);
        assert_eq!(rows.len(), 2);
        assert!(rows[1].starts_with("\x1b[38;5;9m"));
    }

    #[test]
    fn pipe_tables_render_aligned() {
        std::env::remove_var("NO_COLOR");
        let rows = vec![
            "| Name | Qty | Price |".to_string(),
            "|:-----|:---:|------:|".to_string(),
            "| apple | 3 | 1.50 |".to_string(),
            "| **kiwi** | 12 | 0.25 |".to_string(),
        ];
        let out = render_table(&rows, 80);
        assert_eq!(out.len(), 4); // header, rule, two rows
        let plain: Vec<String> = out.iter().map(|l| strip_ansi(l)).collect();
        assert_eq!(plain[0], "  Name  │ Qty │ Price");
        assert_eq!(plain[1], "  ──────┼─────┼──────");
        assert_eq!(plain[2], "  apple │  3  │  1.50");
        assert_eq!(plain[3], "  kiwi  │ 12  │  0.25");
        // Every row is exactly one terminal row.
        assert!(plain.iter().all(|l| str_width(l) == str_width(&plain[0])));
        // Too wide: the widest column is cut with an ellipsis, never wrapped.
        let wide = vec![
            "| a | b |".to_string(),
            "|---|---|".to_string(),
            format!("| {} | x |", "y".repeat(100)),
        ];
        let out = render_table(&wide, 40);
        assert!(out.iter().all(|l| str_width(&strip_ansi(l)) <= 40));
        assert!(strip_ansi(&out[2]).contains('…'));
        assert!(is_table_row("| a | b |"));
        assert!(!is_table_row("|"));
        assert!(!is_table_row("a | b"));
        assert_eq!(
            split_table_cells("| a \\| b | `c|d` |"),
            vec!["a | b", "`c|d`"]
        );
    }

    #[test]
    fn stream_renderer_collects_tables() {
        std::env::remove_var("NO_COLOR");
        let mut r = StreamRenderer::new();
        r.push("| h1 | h2 |\n|--|--|\n| 1 | 2 |\nafter\n");
        r.flush();
        let plain: Vec<String> = r.sink.iter().map(|l| strip_ansi(l)).collect();
        // One emit for the whole table (three rows joined), then the line.
        assert_eq!(plain.len(), 2);
        assert!(plain[0].starts_with("  h1 │ h2\n"));
        assert_eq!(plain[1], "after");
    }

    #[test]
    fn markdown_extras() {
        std::env::remove_var("NO_COLOR");
        assert!(strip_ansi(&render_md_line("---")).trim().starts_with('─'));
        assert!(strip_ansi(&render_md_line("* * *")).trim().starts_with('─'));
        assert_eq!(strip_ansi(&render_md_line("#### Deep")), "Deep");
        assert_eq!(strip_ansi(&render_md_line("- [ ] todo")), "  ☐ todo");
        assert_eq!(strip_ansi(&render_md_line("- [x] done")), "  ☑ done");
        let s = render_md_line("a ~~gone~~ b");
        assert!(s.contains("\x1b[9m"));
        assert_eq!(strip_ansi(&s), "a gone b");
        assert_eq!(
            strip_ansi(&render_md_line("~~ not closed")),
            "~~ not closed"
        );
    }

    #[test]
    fn pasted_media_paths_become_tokens() {
        let dir = std::env::temp_dir().join(format!("bwn-paste-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("shot one.png");
        std::fs::write(&img, b"png").unwrap();
        let txt = dir.join("notes.txt");
        std::fs::write(&txt, b"t").unwrap();
        let p = img.display().to_string();
        assert_eq!(pasted_media_path(&p).as_deref(), Some(img.as_path()));
        assert_eq!(
            pasted_media_path(&format!("'{p}'")).as_deref(),
            Some(img.as_path())
        );
        assert_eq!(
            pasted_media_path(&format!("\"{p}\"\n")).as_deref(),
            Some(img.as_path())
        );
        assert_eq!(
            pasted_media_path(&format!("file://{p}")).as_deref(),
            Some(img.as_path())
        );
        assert_eq!(
            pasted_media_path(&p.replace(' ', "\\ ")).as_deref(),
            Some(img.as_path())
        );
        assert!(pasted_media_path(&txt.display().to_string()).is_none());
        assert!(pasted_media_path("just some words about a .png file").is_none());
        assert!(pasted_media_path(&format!("{p}\nsecond line")).is_none());
        assert!(pasted_media_path(&dir.join("missing.png").display().to_string()).is_none());
        // Token quoting: spaces get double quotes, plain paths don't.
        assert_eq!(attachment_token(&img), format!("@\"{p}\" "));
        let plain = std::path::Path::new("/tmp/a.png");
        assert_eq!(attachment_token(plain), "@/tmp/a.png ");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn image_preview_renders_half_blocks() {
        // 2x2 image: red/green over blue/white → one text row, two cells.
        let rgb = [255u8, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let rows = image_preview_cells(&rgb, 2, 2);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.matches('▀').count(), 2, "{r}");
        assert!(
            r.contains("38;2;255;0;0") && r.contains("48;2;0;0;255"),
            "{r}"
        );
        assert!(
            r.contains("38;2;0;255;0") && r.contains("48;2;255;255;255"),
            "{r}"
        );
        // Odd height: the last pixel row renders fg-only.
        let rgb3 = [10u8, 20, 30, 40, 50, 60, 70, 80, 90];
        let rows3 = image_preview_cells(&rgb3, 1, 3);
        assert_eq!(rows3.len(), 2);
        assert!(rows3[1].contains("38;2;70;80;90"), "{}", rows3[1]);
        // Malformed buffer → nothing.
        assert!(image_preview_cells(&[1, 2], 2, 2).is_empty());
        assert!(image_preview_cells(&rgb, 0, 2).is_empty());
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

    #[test]
    fn queue_edit_and_remove_act_on_the_message_sent_next() {
        // ask_task sends index 0 next; Ctrl+Q / Ctrl+X must take that one,
        // and an edited (or abandoned) message goes back to the front.
        let mut mq = vec!["first".to_string(), "second".to_string()];
        assert_eq!(queue_take_next(&mut mq).as_deref(), Some("first"));
        assert_eq!(mq, vec!["second".to_string()]);
        queue_put_back(&mut mq, "first (edited)".to_string());
        assert_eq!(mq, vec!["first (edited)".to_string(), "second".to_string()]);
        assert_eq!(queue_take_next(&mut mq).as_deref(), Some("first (edited)"));
        assert_eq!(queue_take_next(&mut mq).as_deref(), Some("second"));
        assert_eq!(queue_take_next(&mut mq), None);
        // Only the front row advertises the keys.
        assert!(queued_row_hint(0).contains("Ctrl+Q edit"));
        assert!(queued_row_hint(0).contains("Ctrl+X rm"));
        assert_eq!(queued_row_hint(1), "");
    }

    #[test]
    fn osc52_confirmation_suppressed_only_for_known_unsupported_terminals() {
        // WSL/macOS copy through clip.exe/pbcopy, so the check is moot there.
        if crate::tools::is_wsl() || cfg!(target_os = "macos") {
            return;
        }
        // Known-unsupported: plain xterm / VTE without a TERM_PROGRAM.
        assert!(osc52_known_unsupported_for("xterm", None, false));
        assert!(osc52_known_unsupported_for("xterm-256color", None, true));
        assert!(osc52_known_unsupported_for("linux", None, false));
        // Unsure → keep the confirmation.
        assert!(!osc52_known_unsupported_for("xterm-256color", None, false));
        assert!(!osc52_known_unsupported_for("screen-256color", None, false));
        assert!(!osc52_known_unsupported_for("xterm-kitty", None, false));
        // A TERM_PROGRAM (iTerm2, WezTerm, vscode, tmux…) wins over TERM.
        assert!(!osc52_known_unsupported_for(
            "xterm",
            Some("iTerm.app"),
            false
        ));
        assert!(!osc52_known_unsupported_for(
            "xterm-256color",
            Some("WezTerm"),
            true
        ));
        assert!(osc52_known_unsupported_for("xterm", Some(""), false));
    }

    #[test]
    fn popup_desc_covers_skills_and_custom_commands() {
        // Builtins keep their static text.
        assert_eq!(popup_desc("/help"), slash_command_desc("/help"));
        assert_eq!(
            popup_desc("/local"),
            "probe local servers and list GGUF models"
        );
        // A bundled skill shows its first line via the skill loader.
        let (name, content) = crate::config::bundled_skills()[0];
        let desc = popup_desc(&format!("/{name}"));
        assert!(!desc.is_empty(), "skill /{name} needs a popup description");
        assert_eq!(desc, crate::config::skill_description(content));
        // @mentions and unknown commands stay blank.
        assert_eq!(popup_desc("@src/lib.rs"), "");
        assert_eq!(popup_desc("/no-such-command-xyz"), "");
    }

    #[test]
    fn sanitize_terminal_neutralizes_escapes_but_keeps_newlines_and_tabs() {
        let evil = "hi\x1b]52;c;ZXZpbA==\x07 \x1b[2J\x1b[1;1H\u{9b}31m\u{85}x\x7f\x00\r\n\tend";
        let out = sanitize_terminal(evil);
        assert!(!out.contains('\x1b'));
        assert!(!out
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t'));
        assert_eq!(out, "hi␛]52;c;ZXZpbA== ␛[2J␛[1;1H31mx\n\tend");
        // No control chars: borrowed, no allocation on the streaming path.
        let clean = "plain ✓ text\n\twith ünïcode";
        assert!(matches!(
            sanitize_terminal(clean),
            std::borrow::Cow::Borrowed(_)
        ));
        // Idempotent, so sanitizing at several entry points is harmless.
        assert_eq!(sanitize_terminal(&out), out);
    }

    #[test]
    fn sanitize_prompt_keeps_harness_colors_only() {
        // `ask`/`ask_task` prompts carry harness SGR plus text a model or
        // server can supply (the question tool's default, a model name).
        for (prompt, want) in [
            (
                "  Answer \x1b[38;5;103m[a\x1b]52;c;cm0gLXJmIH4=\x07]\x1b[39m: ",
                "  Answer \x1b[38;5;103m[a␛]52;c;cm0gLXJmIH4=]\x1b[39m: ",
            ),
            (
                "\x1b[38;2;1;2;3m[BUILD]\x1b[0m \x1b[1m›\x1b[22m ",
                "\x1b[38;2;1;2;3m[BUILD]\x1b[0m \x1b[1m›\x1b[22m ",
            ),
            ("  model [x\x1b[2J\x1b[1;1H]: ", "  model [x␛[2J␛[1;1H]: "),
            (
                "  model [\x1b]0;pwned\x1b\\x]: ",
                "  model [␛]0;pwned␛\\x]: ",
            ),
            (
                "  model [x\x1bPtmux;\x1b\x1b\\]: ",
                "  model [x␛Ptmux;␛␛\\]: ",
            ),
            ("  [\u{9b}31mx\u{202E}y\x1b]: ", "  [31mx<U+202E>y␛]: "),
            ("  [trailing\x1b", "  [trailing␛"),
            ("  [\x1bé\x1b[]: ", "  [␛é␛[]: "),
        ] {
            assert_eq!(sanitize_prompt(prompt), want, "{prompt:?}");
            assert_eq!(sanitize_prompt(want), want, "idempotent: {want:?}");
        }
    }

    #[test]
    fn sanitize_terminal_marks_bidi_and_invisible_format_chars() {
        let evil = "a\u{202E}b\u{2066}c\u{200B}d\u{2028}e\u{FEFF}f\u{061C}g\u{200F}";
        let out = sanitize_terminal(evil);
        assert_eq!(
            out,
            "a<U+202E>b<U+2066>c<U+200B>d<U+2028>e<U+FEFF>f<U+061C>g<U+200F>"
        );
        assert_eq!(sanitize_terminal(&out), out);
        // Neighbours sharing a lead byte (Arabic, dashes, full-width forms)
        // are untouched and stay borrowed.
        for ok in ["\u{0627}\u{061B}", "\u{2014}\u{2026}\u{2070}", "\u{FF21}"] {
            assert!(matches!(
                sanitize_terminal(ok),
                std::borrow::Cow::Borrowed(_)
            ));
        }
    }

    #[test]
    fn browse_items_neutralize_skill_and_mcp_text() {
        // /skills and /tools rows: a skill file's name and description, an
        // MCP tool's name and description.
        let items = vec![(
            "/evil\x1b]52;c;cm0gLXJmIH4=\x07".to_string(),
            "[project] fine\u{202E}txt\n\nbody \x1b[2J".to_string(),
        )];
        let safe = browse_safe(&items);
        assert_eq!(safe[0].0, "/evil␛]52;c;cm0gLXJmIH4=");
        assert_eq!(safe[0].1, "[project] fine<U+202E>txt\n\nbody ␛[2J");
    }

    #[test]
    fn untrusted_markdown_keeps_harness_styling_only() {
        let md = "**bold** \x1b]52;c;AAAA\x07 \x1b[31mred\n\n| a | b |\n|---|---|\n| \x1b[2J | 2 |\n\n```rust\nlet x = \"\x1b[1A\";\n```";
        let out = render_md(md);
        for bad in ["\x1b]52", "\x1b[31m", "\x1b[2J", "\x1b[1A", "\x07"] {
            assert!(!out.contains(bad), "{bad:?} leaked: {out:?}");
        }
        let p = plain(&out);
        assert!(p.contains("␛]52;c;AAAA ␛[31mred"), "{p}");
        assert!(p.contains("␛[2J"), "{p}");
        assert!(p.contains("let x = \"␛[1A\";"), "{p}");
        if !no_color() {
            // Bold, table borders and the highlighted code block still style.
            assert!(out.contains('\x1b'));
        }
        let mut r = StreamRenderer::new();
        r.push("ok \x1b]52;c;QQ");
        r.push("==\x07\u{9d} done\n");
        r.flush();
        let joined = r.sink.join("\n");
        assert!(!joined.contains("\x1b]52") && !joined.contains('\x07'));
        assert!(plain(&joined).contains("ok ␛]52;c;QQ== done"));
    }

    #[test]
    fn osc8_url_drops_control_chars() {
        let url = "https://example.test/\x1b\\\x1b]52;c;AA\x07x\u{9c}y\n";
        let clean = osc8_url(url);
        assert_eq!(clean, "https://example.test/\\]52;c;AAxy");
        assert!(!clean.chars().any(char::is_control));
    }

    #[test]
    fn typeahead_keys_survive_an_interrupt_poll() {
        use crossterm::event::KeyEvent;
        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        let _ = take_typeahead();
        for c in "good point".chars() {
            assert_eq!(
                typeahead_event(key(KeyCode::Char(c)), true),
                InterruptKind::None
            );
        }
        // Esc while the agent works raises the interrupt without dropping
        // the keys buffered around it.
        assert_eq!(
            typeahead_event(key(KeyCode::Esc), true),
            InterruptKind::Escape
        );
        typeahead_event(key(KeyCode::Char('!')), true);
        typeahead_event(Event::Paste("\x1b[31m more\n".into()), true);
        let (buf, cursor) = take_typeahead();
        INTERRUPT_KIND_VAL.store(0, Ordering::Relaxed);
        assert_eq!(buf.iter().collect::<String>(), "good point![31m more ");
        assert_eq!(cursor, buf.len());
    }

    #[test]
    fn restore_resets_every_terminal_mode() {
        let restore = std::str::from_utf8(RESTORE).unwrap();
        for seq in [
            "\x1b[r",
            "\x1b[0m",
            "\x1b[?25h",
            "\x1b[?1000l",
            "\x1b[?1002l",
            "\x1b[?1003l",
            "\x1b[?1015l",
            "\x1b[?1006l",
            "\x1b[?1004l",
            "\x1b[?2004l",
            "\x1b[0 q",
        ] {
            assert!(restore.contains(seq), "RESTORE lacks {seq:?}");
        }
        // Margins reset before leaving the alternate screen, which comes last.
        assert!(restore.ends_with("\x1b[?1049l"));
    }

    #[test]
    fn sanitize_paste_flattens_lines_and_drops_controls() {
        let got: String = sanitize_paste("a\r\nb\tc\x1b[31m\u{9b}d\x07\x7f ✓")
            .into_iter()
            .collect();
        assert_eq!(got, "a  b c[31md ✓");
        assert!(sanitize_paste("").is_empty());
    }

    #[test]
    fn ask_secret_never_echoes_and_esc_or_ctrl_c_cancel() {
        use crossterm::event::KeyEvent;
        let key = |code, mods| Event::Key(KeyEvent::new(code, mods));
        let typed = |s: &str| -> Vec<Event> {
            s.chars()
                .map(|c| key(KeyCode::Char(c), KeyModifiers::NONE))
                .collect()
        };
        let secret = "sk-ant-api03-NEWCOMERfakekey0001-SECRETTAIL9";
        let run = |events: Vec<Event>| {
            let mut events = events.into_iter();
            let mut screen = String::new();
            let got = read_secret(
                || events.next(),
                |n| screen.push_str(&secret_frame("  ANTHROPIC_API_KEY: ", n, 200)),
            );
            (got, screen)
        };

        // Typed and pasted characters show only as dots; Enter returns the
        // key, and what stays on screen is the masked form.
        let mut evs = typed(&secret[..10]);
        evs.push(Event::Paste(secret[10..].into()));
        evs.push(key(KeyCode::Enter, KeyModifiers::NONE));
        let (got, screen) = run(evs);
        assert_eq!(got.as_deref(), Some(secret));
        for part in ["sk-ant", "NEWCOMER", "SECRETTAIL9", "AIL9"] {
            assert!(!screen.contains(part), "{part} echoed: {screen}");
        }
        assert!(screen.contains(&"•".repeat(secret.len())));
        assert_eq!(secret_echo(secret), "sk-a…AIL9");
        assert_eq!(secret_echo("  "), "");

        // Esc and Ctrl+C cancel even with text typed; Ctrl+D only on an
        // empty line. Backspace and Ctrl+U edit what was typed.
        let mut evs = typed("sk-partial");
        evs.push(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(run(evs).0, None);
        let mut evs = typed("sk-partial");
        evs.push(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(run(evs).0, None);
        assert_eq!(
            run(vec![key(KeyCode::Char('d'), KeyModifiers::CONTROL)]).0,
            None
        );
        let mut evs = typed("abx");
        evs.push(key(KeyCode::Backspace, KeyModifiers::NONE));
        evs.push(key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        evs.push(key(KeyCode::Char('c'), KeyModifiers::NONE));
        evs.push(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(run(evs).0.as_deref(), Some("abc"));
        let mut evs = typed("wrong");
        evs.push(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        evs.extend(typed("ok"));
        evs.push(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(run(evs).0.as_deref(), Some("ok"));
        // The input closing (a read error) is a cancel, not an empty key.
        assert_eq!(run(typed("sk-")).0, None);

        // A long key never wraps the row: the dots stop at the width.
        let frame = secret_frame("  KEY: ", 500, 40);
        assert_eq!(frame.matches('•').count(), 40 - 7 - 1);
    }
}

#[cfg(test)]
mod command_desc_tests {
    use super::*;

    #[test]
    fn a_prompt_command_shows_its_frontmatter_description() {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("bwn-cmd-desc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("commands")).unwrap();
        std::fs::write(
            home.join("commands").join("deploy.md"),
            "---\ndescription: Deploy to an environment\n---\nRun the deploy for $ARGUMENTS.\n",
        )
        .unwrap();
        std::fs::write(home.join("commands").join("tidy.md"), "Tidy the imports.\n").unwrap();
        let old = std::env::var_os("NEXUS_HOME");
        std::env::set_var("NEXUS_HOME", &home);
        let cmds = crate::config::load_custom_commands();
        match old {
            Some(v) => std::env::set_var("NEXUS_HOME", v),
            None => std::env::remove_var("NEXUS_HOME"),
        }
        let desc =
            |name: &str| custom_command_desc(cmds.iter().find(|c| c.name == name).expect(name));
        assert_eq!(desc("deploy"), "Deploy to an environment");
        assert_eq!(desc("tidy"), "Tidy the imports.");
        let _ = std::fs::remove_dir_all(&home);
    }
}
