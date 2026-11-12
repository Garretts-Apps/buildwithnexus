//! The colour theme: dark, light and ansi palettes, auto detection, and the colour helpers.

use super::*;

// ── theme ────────────────────────────────────────────────────────────────
// A colour as a theme names it: 24-bit (sent as truecolor, or the nearest
// 256-colour cube entry), one of the terminal's own 16 colours (SGR 30-37 /
// 90-97, so the user's palette decides), or the terminal default.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Col {
    Rgb(u8, u8, u8),
    Ansi(u8),
    Default,
}

pub(super) struct Palette {
    pub(super) name: &'static str,
    // Painted behind the alternate screen; None keeps the terminal's own.
    pub(super) background: Option<Col>,
    pub(super) text: Col,
    pub(super) accent: Col,
    // Secondary/dim text: still at least 4.5:1 against the background.
    pub(super) muted: Col,
    pub(super) success: Col,
    pub(super) warning: Col,
    pub(super) error: Col,
    pub(super) info: Col,
    pub(super) mode_plan: Col,
    pub(super) mode_build: Col,
    pub(super) mode_bstorm: Col,
    // Diff row tints: whole added/removed rows get a subtle background; the
    // changed word span gets a stronger one.
    pub(super) diff_add_bg: Col,
    pub(super) diff_del_bg: Col,
    pub(super) diff_add_emph_bg: Col,
    pub(super) diff_del_emph_bg: Col,
    pub(super) selection_bg: Col,
    // OSC 12 cursor colour, or None to leave the terminal's.
    pub(super) cursor: Option<&'static str>,
    // Wordmark gradient stops, deep → pale.
    pub(super) wordmark: [Col; 4],
}

// Tokyo Night, painted on its own background. The comment colour (#565f89)
// measures 2.76:1 there — below the WCAG AA 4.5:1 text minimum — so muted
// text uses #7e88b3 (4.93:1) from the same blue-violet family.
pub(super) const DARK: Palette = Palette {
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
pub(super) const LIGHT: Palette = Palette {
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
pub(super) const ANSI: Palette = Palette {
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
pub(super) static THEME: AtomicU8 = AtomicU8::new(0);

pub(super) fn pal() -> &'static Palette {
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
pub(super) fn theme_index(name: &str) -> Result<Option<usize>, String> {
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
pub(super) fn colorfgbg_is_light(v: Option<&str>) -> bool {
    v.and_then(|v| v.rsplit(';').next())
        .and_then(|bg| bg.trim().parse::<u8>().ok())
        .is_some_and(|bg| bg == 7 || bg == 15)
}

// OSC 11 answer: `ESC ] 11 ; rgb:RRRR/GGGG/BBBB` (1-4 hex digits each),
// ended by BEL or ST. Light when its relative luminance is above 0.4.
pub(super) fn osc11_is_light(reply: &[u8]) -> Option<bool> {
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
pub(super) fn query_background_is_light() -> Option<bool> {
    if line_mode() || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return None;
    }
    let _raw = RawForRead::on();
    let reply = crate::sixel::query(b"\x1b]11;?\x1b\\\x1b[c", Duration::from_millis(250));
    osc11_is_light(&reply)
}

// Line mode (`--plain`, or TERM=dumb): no alternate screen, no cursor
// addressing, no spinner frames; the transcript is printed line by line.
pub(super) static LINE_MODE: AtomicBool = AtomicBool::new(false);

/// Turn line mode on (`--plain`). TERM=dumb turns it on by itself.
pub fn set_line_mode(on: bool) {
    LINE_MODE.store(on, Ordering::Relaxed);
}

/// Whether the session prints line by line, without screen control.
pub fn line_mode() -> bool {
    LINE_MODE.load(Ordering::Relaxed) || term_is_dumb()
}

pub(super) fn term_is_dumb() -> bool {
    std::env::var("TERM").is_ok_and(|t| t == "dumb")
}

/// True when colour output is off (`NO_COLOR`); highlighters skip work.
pub(crate) fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some() || !stdout_wants_color() || term_is_dumb()
}

// Piped or redirected output (CI logs, `bwn run … > out.txt`) gets plain text
// unless FORCE_COLOR is set. Decided once: stdout does not change mid-run.
pub(super) fn stdout_wants_color() -> bool {
    static WANTS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *WANTS.get_or_init(|| {
        cfg!(test)
            || std::env::var_os("FORCE_COLOR").is_some()
            || (io::stdout().is_terminal() && console_vt())
    })
}

/// Sets up the console before anything is printed. A Windows console shows
/// colour and cursor codes as text (`←[38;2;…m`) until the program turns on
/// virtual-terminal processing: Windows Terminal has it on for every
/// program, the console window that cmd and PowerShell open does not. A
/// console that refuses it (the "Use legacy console" setting) gets line mode
/// without colour, so it shows plain text instead of codes.
pub fn init_console() {
    if !console_vt() {
        set_line_mode(true);
    }
}

#[cfg(windows)]
pub(super) fn console_vt() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(win_console::enable_vt)
}

#[cfg(not(windows))]
pub(super) fn console_vt() -> bool {
    true
}

// kernel32 is always linked on Windows; declaring the three calls here
// avoids a Windows API crate, as signal_restore does.
#[cfg(windows)]
pub(super) mod win_console {
    type Handle = *mut std::ffi::c_void;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> Handle;
        fn GetConsoleMode(h: Handle, mode: *mut u32) -> i32;
        fn SetConsoleMode(h: Handle, mode: u32) -> i32;
    }

    /// Turns on virtual-terminal processing for stdout and stderr where they
    /// are consoles. False only when stdout is a console that refuses it; a
    /// pipe or a file is not a console and needs nothing. The mode stays on
    /// after bwn exits, as it does for other programs that set it.
    pub fn enable_vt() -> bool {
        let mut ok = true;
        for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: plain kernel32 calls on the process's own standard
            // handles; `mode` is a local the call writes.
            unsafe {
                let h = GetStdHandle(which);
                let mut mode = 0u32;
                if h.is_null() || h as isize == -1 || GetConsoleMode(h, &mut mode) == 0 {
                    continue;
                }
                if mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING == 0
                    && SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0
                    && which == STD_OUTPUT_HANDLE
                {
                    ok = false;
                }
            }
        }
        ok
    }
}

pub(super) fn truecolor() -> bool {
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
    !(term == "linux" || term == "vt100" || term == "dumb")
}

pub(super) fn cube(c: u8) -> u32 {
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

pub(super) fn sgr_fg(c: Col) -> String {
    match c {
        Col::Rgb(r, g, b) if truecolor() => format!("38;2;{r};{g};{b}"),
        Col::Rgb(r, g, b) => format!("38;5;{}", 16 + 36 * cube(r) + 6 * cube(g) + cube(b)),
        Col::Ansi(n) => n.to_string(),
        Col::Default => "39".to_string(),
    }
}

pub(super) fn sgr_bg(c: Col) -> String {
    match c {
        Col::Rgb(r, g, b) if truecolor() => format!("48;2;{r};{g};{b}"),
        Col::Rgb(r, g, b) => format!("48;5;{}", 16 + 36 * cube(r) + 6 * cube(g) + cube(b)),
        Col::Ansi(n) => (n + 10).to_string(),
        Col::Default => "49".to_string(),
    }
}

// The theme's painted background, or nothing for themes that keep the
// terminal's own.
pub(super) fn theme_bg() -> String {
    match pal().background {
        Some(bg) if !no_color() => format!("\x1b[{}m", sgr_bg(bg)),
        _ => String::new(),
    }
}

// Back to plain text after a styled span. On the alternate screen that is
// the theme's text colour on the theme's background, so a reset never
// leaves a hole of terminal-default background in a painted screen.
pub(super) fn reset_all() -> String {
    if no_color() {
        String::new()
    } else if ALT_SCREEN.load(Ordering::Relaxed) {
        format!("\x1b[0m\x1b[{}m{}", sgr_fg(pal().text), theme_bg())
    } else {
        "\x1b[0m".to_string()
    }
}

pub(super) fn reset_fg() -> String {
    if no_color() {
        String::new()
    } else if ALT_SCREEN.load(Ordering::Relaxed) {
        format!("\x1b[{}m", sgr_fg(pal().text))
    } else {
        "\x1b[39m".to_string()
    }
}

pub(super) fn paint(c: Col, s: &str) -> String {
    if no_color() {
        return s.to_string();
    }
    format!("\x1b[{}m{s}{}", sgr_fg(c), reset_fg())
}

pub(super) fn attr(code: &str, s: &str) -> String {
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
