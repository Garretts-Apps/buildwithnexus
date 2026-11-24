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

// Notch, the dark page. Field #1c1712 is a warm brown-black (hue 28°).
// Paper, quiet, mark, wait, and cut are documented with their contrast
// ratios in docs/design/direction.md. The four wordmark stops are the
// same paper color: the name is not a gradient.
pub(super) const DARK: Palette = Palette {
    name: "dark",
    background: Some(Col::Rgb(0x1c, 0x17, 0x12)),
    text: Col::Rgb(0xf1, 0xea, 0xdf),
    accent: Col::Rgb(0x44, 0xa7, 0x93),
    muted: Col::Rgb(0xaa, 0x9e, 0x92),
    // Bronze and sage are syntax inks, shifted off the mark so a keyword,
    // a string, and a type are not the same cell. They are not chrome.
    success: Col::Rgb(0xb0, 0x8a, 0x4e),
    warning: Col::Rgb(0x77, 0x89, 0x4d),
    error: Col::Rgb(0xd2, 0x66, 0x56),
    info: Col::Rgb(0x6e, 0x8f, 0x6a),
    mode_plan: Col::Rgb(0x44, 0xa7, 0x93),
    mode_build: Col::Rgb(0xf1, 0xea, 0xdf),
    mode_bstorm: Col::Rgb(0x77, 0x89, 0x4d),
    diff_add_bg: Col::Rgb(0x1a, 0x2a, 0x26),
    diff_del_bg: Col::Rgb(0x2e, 0x1c, 0x18),
    diff_add_emph_bg: Col::Rgb(0x24, 0x3e, 0x36),
    diff_del_emph_bg: Col::Rgb(0x4a, 0x28, 0x22),
    selection_bg: Col::Rgb(0x3a, 0x32, 0x28),
    cursor: Some("#44a793"),
    wordmark: [
        Col::Rgb(0xf1, 0xea, 0xdf),
        Col::Rgb(0xf1, 0xea, 0xdf),
        Col::Rgb(0xf1, 0xea, 0xdf),
        Col::Rgb(0xf1, 0xea, 0xdf),
    ],
};

// Notch on a light page. Same hues as the dark field, darkened until every
// foreground clears 4.5:1 on white, Solarized Light, #eeeeee, and the tints
// below (see the contrast test). No blue-violet accent.
pub(super) const LIGHT: Palette = Palette {
    name: "light",
    background: None,
    text: Col::Rgb(0x28, 0x1e, 0x15),
    accent: Col::Rgb(0x1e, 0x67, 0x58),
    muted: Col::Rgb(0x67, 0x5c, 0x51),
    success: Col::Rgb(0x6a, 0x48, 0x14),
    warning: Col::Rgb(0x4d, 0x5d, 0x28),
    error: Col::Rgb(0x8b, 0x28, 0x18),
    info: Col::Rgb(0x3d, 0x52, 0x28),
    mode_plan: Col::Rgb(0x1e, 0x67, 0x58),
    mode_build: Col::Rgb(0x28, 0x1e, 0x15),
    mode_bstorm: Col::Rgb(0x4d, 0x5d, 0x28),
    diff_add_bg: Col::Rgb(0xe7, 0xf0, 0xea),
    diff_del_bg: Col::Rgb(0xf6, 0xe8, 0xe4),
    diff_add_emph_bg: Col::Rgb(0xd5, 0xe6, 0xdb),
    diff_del_emph_bg: Col::Rgb(0xf0, 0xd4, 0xcc),
    selection_bg: Col::Rgb(0xe4, 0xdd, 0xd2),
    cursor: Some("#1e6758"),
    wordmark: [
        Col::Rgb(0x28, 0x1e, 0x15),
        Col::Rgb(0x28, 0x1e, 0x15),
        Col::Rgb(0x28, 0x1e, 0x15),
        Col::Rgb(0x28, 0x1e, 0x15),
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
    wordmark: [Col::Default, Col::Default, Col::Default, Col::Default],
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
    static DUMB: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DUMB.get_or_init(|| std::env::var("TERM").is_ok_and(|t| t == "dumb"))
}

/// True when colour output is off (`NO_COLOR`); highlighters skip work.
pub(crate) fn no_color() -> bool {
    // Decided once, like stdout_wants_color: the environment and stdout do
    // not change mid-run, and every colour helper asks. Tests take NO_COLOR
    // out of their environment as they go, so under test it is read each
    // time.
    if cfg!(test) {
        return colour_off();
    }
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(colour_off)
}

fn colour_off() -> bool {
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

// 24-bit colour, or the nearest 256-colour cube entry. Off whenever colour
// is off; otherwise COLORTERM, then TERM, read once.
pub(super) fn truecolor() -> bool {
    !no_color() && truecolor_term()
}

fn truecolor_term() -> bool {
    static TC: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TC.get_or_init(|| {
        truecolor_for(
            std::env::var("COLORTERM").ok().as_deref(),
            &std::env::var("TERM").unwrap_or_default(),
        )
    })
}

fn truecolor_for(colorterm: Option<&str>, term: &str) -> bool {
    match colorterm {
        Some("truecolor" | "24bit") => true,
        Some("256color" | "no" | "0") => false,
        _ => !(term == "linux" || term == "vt100" || term == "dumb"),
    }
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
    sgr_fg_at(c, truecolor())
}

pub(super) fn sgr_bg(c: Col) -> String {
    sgr_bg_at(c, truecolor())
}

fn sgr_fg_at(c: Col, tc: bool) -> String {
    match c {
        Col::Rgb(r, g, b) if tc => format!("38;2;{r};{g};{b}"),
        Col::Rgb(r, g, b) => format!("38;5;{}", 16 + 36 * cube(r) + 6 * cube(g) + cube(b)),
        Col::Ansi(n) => n.to_string(),
        Col::Default => "39".to_string(),
    }
}

fn sgr_bg_at(c: Col, tc: bool) -> String {
    match c {
        Col::Rgb(r, g, b) if tc => format!("48;2;{r};{g};{b}"),
        Col::Rgb(r, g, b) => format!("48;5;{}", 16 + 36 * cube(r) + 6 * cube(g) + cube(b)),
        Col::Ansi(n) => (n + 10).to_string(),
        Col::Default => "49".to_string(),
    }
}

// ── SGR tables ───────────────────────────────────────────────────────────
// Every escape the colour helpers send, made once per theme at the colour
// depth in use, so a span costs one allocation (its own text) instead of a
// format! per escape. The helpers only reach a table with colour on. All
// three themes are built together on first use; a theme switch picks
// another table, and the alternate screen picks the other half of the
// pairs below, so neither has anything to rebuild.
pub(super) struct Sgr {
    // SGR parameters ("38;2;r;g;b" / "48;…") for each colour the palette
    // names, foreground and background.
    cols: Vec<(Col, String, String)>,
    // [main screen, alternate screen].
    reset_fg: [String; 2],
    reset_all: [String; 2],
    reset_bg: [String; 2],
    theme_bg: String,
    span: [SpanPaint; 2],
}

impl Sgr {
    fn new(p: &Palette, tc: bool) -> Self {
        let mut list = vec![
            p.text,
            p.accent,
            p.muted,
            p.success,
            p.warning,
            p.error,
            p.info,
            p.mode_plan,
            p.mode_build,
            p.mode_bstorm,
            p.diff_add_bg,
            p.diff_del_bg,
            p.diff_add_emph_bg,
            p.diff_del_emph_bg,
            p.selection_bg,
        ];
        list.extend(p.background);
        list.extend(p.wordmark);
        let mut cols: Vec<(Col, String, String)> = Vec::new();
        for c in list {
            if !cols.iter().any(|(k, _, _)| *k == c) {
                cols.push((c, sgr_fg_at(c, tc), sgr_bg_at(c, tc)));
            }
        }
        let theme_bg = match p.background {
            Some(bg) => format!("\x1b[{}m", sgr_bg_at(bg, tc)),
            None => String::new(),
        };
        let text_fg = sgr_fg_at(p.text, tc);
        let reset_fg = ["\x1b[39m".to_string(), format!("\x1b[{text_fg}m")];
        let reset_all = [
            "\x1b[0m".to_string(),
            format!("\x1b[0m\x1b[{text_fg}m{theme_bg}"),
        ];
        let reset_bg = [
            "\x1b[49m".to_string(),
            if p.background.is_some() {
                theme_bg.clone()
            } else {
                "\x1b[49m".to_string()
            },
        ];
        let on = |c: Col| format!("\x1b[{}m", sgr_fg_at(c, tc));
        let span = |reset: &str| SpanPaint {
            accent: on(p.accent),
            info: on(p.info),
            success: on(p.success),
            warning: on(p.warning),
            muted: on(p.muted),
            reset: reset.to_string(),
        };
        let span = [span(&reset_fg[0]), span(&reset_fg[1])];
        Sgr {
            cols,
            reset_fg,
            reset_all,
            reset_bg,
            theme_bg,
            span,
        }
    }

    fn fg(&self, c: Col) -> Option<&str> {
        self.cols
            .iter()
            .find(|(k, _, _)| *k == c)
            .map(|(_, fg, _)| fg.as_str())
    }

    fn bg(&self, c: Col) -> Option<&str> {
        self.cols
            .iter()
            .find(|(k, _, _)| *k == c)
            .map(|(_, _, bg)| bg.as_str())
    }

    // "\x1b[<fg>m{s}<reset>", from the table when the palette names `c`.
    fn paint(&self, alt: bool, c: Col, s: &str) -> String {
        let reset = &self.reset_fg[usize::from(alt)];
        match self.fg(c) {
            Some(fg) => {
                let mut out = String::with_capacity(fg.len() + s.len() + reset.len() + 3);
                out.push_str("\x1b[");
                out.push_str(fg);
                out.push('m');
                out.push_str(s);
                out.push_str(reset);
                out
            }
            None => format!("\x1b[{}m{s}{reset}", sgr_fg(c)),
        }
    }

    fn attr(&self, alt: bool, code: &str, s: &str) -> String {
        let reset = match code {
            "1" => "\x1b[22m",
            "3" => "\x1b[23m",
            "4" => "\x1b[24m",
            _ => &self.reset_all[usize::from(alt)],
        };
        let mut out = String::with_capacity(code.len() + s.len() + reset.len() + 3);
        out.push_str("\x1b[");
        out.push_str(code);
        out.push('m');
        out.push_str(s);
        out.push_str(reset);
        out
    }

    // A background behind `s`, then the background reset (code_bg).
    fn on_bg_only(&self, alt: bool, bg: Col, s: &str) -> String {
        let reset = &self.reset_bg[usize::from(alt)];
        match self.bg(bg) {
            Some(b) => {
                let mut out = String::with_capacity(b.len() + s.len() + reset.len() + 3);
                out.push_str("\x1b[");
                out.push_str(b);
                out.push('m');
                out.push_str(s);
                out.push_str(reset);
                out
            }
            None => format!("\x1b[{}m{s}{reset}", sgr_bg(bg)),
        }
    }

    // Background and foreground behind `s`, then both resets (on_bg).
    fn on_bg(&self, alt: bool, bg: Col, fg: Col, s: &str) -> String {
        let a = usize::from(alt);
        let (reset_bg, reset_fg) = (&self.reset_bg[a], &self.reset_fg[a]);
        match (self.bg(bg), self.fg(fg)) {
            (Some(b), Some(f)) => {
                let mut out = String::with_capacity(
                    b.len() + f.len() + s.len() + reset_bg.len() + reset_fg.len() + 4,
                );
                out.push_str("\x1b[");
                out.push_str(b);
                out.push(';');
                out.push_str(f);
                out.push('m');
                out.push_str(s);
                out.push_str(reset_bg);
                out.push_str(reset_fg);
                out
            }
            _ => format!("\x1b[{};{}m{s}{reset_bg}{reset_fg}", sgr_bg(bg), sgr_fg(fg)),
        }
    }
}

fn sgr_tables_at(tc: bool) -> [Sgr; 3] {
    [
        Sgr::new(&DARK, tc),
        Sgr::new(&LIGHT, tc),
        Sgr::new(&ANSI, tc),
    ]
}

// The table for the theme in use. Only called with colour on, where the
// depth is truecolor_term()'s.
pub(super) fn sgr() -> &'static Sgr {
    static TABLES: std::sync::OnceLock<[Sgr; 3]> = std::sync::OnceLock::new();
    let tables = TABLES.get_or_init(|| sgr_tables_at(truecolor_term()));
    // pal() settles the theme on first use; the index follows its mapping.
    pal();
    match THEME.load(Ordering::Relaxed) {
        2 => &tables[1],
        3 => &tables[2],
        _ => &tables[0],
    }
}

fn alt() -> bool {
    ALT_SCREEN.load(Ordering::Relaxed)
}

// The theme's painted background, or nothing for themes that keep the
// terminal's own.
pub(super) fn theme_bg() -> &'static str {
    if no_color() {
        ""
    } else {
        &sgr().theme_bg
    }
}

// Back to plain text after a styled span. On the alternate screen that is
// the theme's text colour on the theme's background, so a reset never
// leaves a hole of terminal-default background in a painted screen.
pub(super) fn reset_all() -> &'static str {
    if no_color() {
        ""
    } else {
        &sgr().reset_all[usize::from(alt())]
    }
}

// The background back to the theme's (alternate screen, painted themes) or
// the terminal default, as a span's foreground reset does for text.
pub(super) fn reset_bg() -> &'static str {
    if no_color() {
        ""
    } else {
        &sgr().reset_bg[usize::from(alt())]
    }
}

pub(super) fn paint(c: Col, s: &str) -> String {
    if no_color() {
        return s.to_string();
    }
    sgr().paint(alt(), c, s)
}

// `s` on a background tint, the background then reset (code_bg).
pub(super) fn paint_bg(bg: Col, s: &str) -> String {
    if no_color() {
        return s.to_string();
    }
    sgr().on_bg_only(alt(), bg, s)
}

// `s` in `fg` on `bg`, both reset afterwards (the diff spans, selection).
pub(super) fn paint_on(bg: Col, fg: Col, s: &str) -> String {
    if no_color() {
        return s.to_string();
    }
    sgr().on_bg(alt(), bg, fg, s)
}

/// What `paint` wraps text in, per colour role, for a caller that paints
/// many spans (the code highlighter): looked up once, not once per span.
/// All empty when colour is off.
#[derive(Default)]
pub(crate) struct SpanPaint {
    pub accent: String,
    pub info: String,
    pub success: String,
    pub warning: String,
    pub muted: String,
    pub reset: String,
}

/// The span escapes for the theme and screen in use; a lookup, not a build.
pub(crate) fn span_paint() -> &'static SpanPaint {
    static OFF: SpanPaint = SpanPaint {
        accent: String::new(),
        info: String::new(),
        success: String::new(),
        warning: String::new(),
        muted: String::new(),
        reset: String::new(),
    };
    if no_color() {
        return &OFF;
    }
    &sgr().span[usize::from(alt())]
}

pub(super) fn attr(code: &str, s: &str) -> String {
    if no_color() {
        return s.to_string();
    }
    sgr().attr(alt(), code, s)
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

// The colour helpers as they were before the SGR tables: every escape
// formatted per call. Kept to check the tables send the same bytes, with
// the palette, the screen and the colour depth passed in rather than read
// from the globals, so the check covers every theme and mode without
// switching the theme under other tests.
#[cfg(test)]
pub(super) mod before_tables {
    use super::{cube, Col, Palette};

    pub fn sgr_fg(c: Col, tc: bool) -> String {
        match c {
            Col::Rgb(r, g, b) if tc => format!("38;2;{r};{g};{b}"),
            Col::Rgb(r, g, b) => format!("38;5;{}", 16 + 36 * cube(r) + 6 * cube(g) + cube(b)),
            Col::Ansi(n) => n.to_string(),
            Col::Default => "39".to_string(),
        }
    }

    pub fn sgr_bg(c: Col, tc: bool) -> String {
        match c {
            Col::Rgb(r, g, b) if tc => format!("48;2;{r};{g};{b}"),
            Col::Rgb(r, g, b) => format!("48;5;{}", 16 + 36 * cube(r) + 6 * cube(g) + cube(b)),
            Col::Ansi(n) => (n + 10).to_string(),
            Col::Default => "49".to_string(),
        }
    }

    pub fn theme_bg(p: &Palette, tc: bool) -> String {
        match p.background {
            Some(bg) => format!("\x1b[{}m", sgr_bg(bg, tc)),
            _ => String::new(),
        }
    }

    pub fn reset_all(p: &Palette, alt: bool, tc: bool) -> String {
        if alt {
            format!("\x1b[0m\x1b[{}m{}", sgr_fg(p.text, tc), theme_bg(p, tc))
        } else {
            "\x1b[0m".to_string()
        }
    }

    pub fn reset_fg(p: &Palette, alt: bool, tc: bool) -> String {
        if alt {
            format!("\x1b[{}m", sgr_fg(p.text, tc))
        } else {
            "\x1b[39m".to_string()
        }
    }

    pub fn reset_bg(p: &Palette, alt: bool, tc: bool) -> String {
        if alt && p.background.is_some() {
            theme_bg(p, tc)
        } else {
            "\x1b[49m".to_string()
        }
    }

    pub fn paint(p: &Palette, alt: bool, tc: bool, c: Col, s: &str) -> String {
        format!("\x1b[{}m{s}{}", sgr_fg(c, tc), reset_fg(p, alt, tc))
    }

    pub fn attr(p: &Palette, alt: bool, tc: bool, code: &str, s: &str) -> String {
        let reset = match code {
            "1" => "\x1b[22m".to_string(),
            "3" => "\x1b[23m".to_string(),
            "4" => "\x1b[24m".to_string(),
            _ => reset_all(p, alt, tc),
        };
        format!("\x1b[{code}m{s}{reset}")
    }

    pub fn code_bg(p: &Palette, alt: bool, tc: bool, s: &str) -> String {
        if s.is_empty() {
            return s.to_string();
        }
        format!(
            "\x1b[{}m{s}{}",
            sgr_bg(p.selection_bg, tc),
            reset_bg(p, alt, tc)
        )
    }

    pub fn on_bg(p: &Palette, alt: bool, tc: bool, bg: Col, fg: Col, s: &str) -> String {
        format!(
            "\x1b[{};{}m{s}{}{}",
            sgr_bg(bg, tc),
            sgr_fg(fg, tc),
            reset_bg(p, alt, tc),
            reset_fg(p, alt, tc)
        )
    }

    // span_paint's fields: accent, info, success, warning, muted, reset.
    pub fn span(p: &Palette, alt: bool, tc: bool) -> [String; 6] {
        let on = |c: Col| format!("\x1b[{}m", sgr_fg(c, tc));
        [
            on(p.accent),
            on(p.info),
            on(p.success),
            on(p.warning),
            on(p.muted),
            reset_fg(p, alt, tc),
        ]
    }

    pub fn truecolor(colorterm: Option<&str>, term: &str) -> bool {
        if let Some(ct) = colorterm {
            if ct == "truecolor" || ct == "24bit" {
                return true;
            }
            if ct == "256color" || ct == "no" || ct == "0" {
                return false;
            }
        }
        !(term == "linux" || term == "vt100" || term == "dumb")
    }
}

#[cfg(test)]
mod tests {
    use super::before_tables as old;
    use super::*;

    fn palettes() -> [&'static Palette; 3] {
        let by_name = |n: &str| -> &'static Palette {
            match n {
                "dark" => &DARK,
                "light" => &LIGHT,
                "ansi" => &ANSI,
                other => panic!("no palette for theme {other}"),
            }
        };
        THEME_NAMES.map(by_name)
    }

    fn cols(p: &Palette) -> Vec<Col> {
        let mut v = vec![
            p.text,
            p.accent,
            p.muted,
            p.success,
            p.warning,
            p.error,
            p.info,
            p.mode_plan,
            p.mode_build,
            p.mode_bstorm,
            p.diff_add_bg,
            p.diff_del_bg,
            p.diff_add_emph_bg,
            p.diff_del_emph_bg,
            p.selection_bg,
        ];
        v.extend(p.background);
        v.extend(p.wordmark);
        v
    }

    const WORDS: [&str; 5] = ["", "x", "fn main()", "naïve 中文", "\x1b[1mnested\x1b[0m"];

    // Every escape the tables hold, for every theme, both screens and both
    // colour depths, against the per-call formatting they replace.
    #[test]
    fn sgr_tables_send_the_bytes_the_per_call_helpers_sent() {
        for p in palettes() {
            for tc in [false, true] {
                let t = Sgr::new(p, tc);
                assert_eq!(t.theme_bg, old::theme_bg(p, tc), "{} tc={tc}", p.name);
                for alt in [false, true] {
                    let a = usize::from(alt);
                    let ctx = format!("{} alt={alt} tc={tc}", p.name);
                    assert_eq!(t.reset_all[a], old::reset_all(p, alt, tc), "{ctx}");
                    assert_eq!(t.reset_fg[a], old::reset_fg(p, alt, tc), "{ctx}");
                    assert_eq!(t.reset_bg[a], old::reset_bg(p, alt, tc), "{ctx}");
                    let sp = &t.span[a];
                    let got = [
                        sp.accent.clone(),
                        sp.info.clone(),
                        sp.success.clone(),
                        sp.warning.clone(),
                        sp.muted.clone(),
                        sp.reset.clone(),
                    ];
                    assert_eq!(got, old::span(p, alt, tc), "{ctx}");
                    for s in WORDS {
                        for code in ["1", "3", "4", "2", "7", "9"] {
                            assert_eq!(
                                t.attr(alt, code, s),
                                old::attr(p, alt, tc, code, s),
                                "{ctx}"
                            );
                        }
                        for c in cols(p) {
                            assert_eq!(t.paint(alt, c, s), old::paint(p, alt, tc, c, s), "{ctx}");
                            for fg in cols(p) {
                                assert_eq!(
                                    t.on_bg(alt, c, fg, s),
                                    old::on_bg(p, alt, tc, c, fg, s),
                                    "{ctx}"
                                );
                            }
                        }
                        if !s.is_empty() {
                            assert_eq!(
                                t.on_bg_only(alt, p.selection_bg, s),
                                old::code_bg(p, alt, tc, s),
                                "{ctx}"
                            );
                        }
                    }
                }
            }
        }
    }

    // The helpers themselves, as the renderers call them, against the old
    // formatting at the theme, screen and depth in use, including colours
    // the palette does not name (they are formatted, not looked up).
    #[test]
    fn colour_helpers_match_the_per_call_formatting() {
        std::env::remove_var("NO_COLOR");
        let (p, alt, tc) = (pal(), ALT_SCREEN.load(Ordering::Relaxed), truecolor());
        for s in WORDS {
            assert_eq!(dim(s), old::paint(p, alt, tc, p.muted, s));
            assert_eq!(red(s), old::paint(p, alt, tc, p.error, s));
            assert_eq!(green(s), old::paint(p, alt, tc, p.success, s));
            assert_eq!(yellow(s), old::paint(p, alt, tc, p.warning, s));
            assert_eq!(blue(s), old::paint(p, alt, tc, p.info, s));
            assert_eq!(cyan(s), old::paint(p, alt, tc, p.info, s));
            assert_eq!(accent(s), old::paint(p, alt, tc, p.accent, s));
            assert_eq!(text(s), old::paint(p, alt, tc, p.text, s));
            assert_eq!(bold(s), old::attr(p, alt, tc, "1", s));
            assert_eq!(italic(s), old::attr(p, alt, tc, "3", s));
            assert_eq!(underline(s), old::attr(p, alt, tc, "4", s));
            for c in [Col::Rgb(1, 2, 3), Col::Ansi(95), Col::Default] {
                assert_eq!(paint(c, s), old::paint(p, alt, tc, c, s));
                assert_eq!(
                    paint_on(Col::Ansi(44), c, s),
                    old::on_bg(p, alt, tc, Col::Ansi(44), c, s)
                );
            }
            assert_eq!(
                paint_on(p.diff_add_bg, p.success, s),
                old::on_bg(p, alt, tc, p.diff_add_bg, p.success, s)
            );
        }
        assert_eq!(theme_bg(), old::theme_bg(p, tc));
        assert_eq!(reset_all(), old::reset_all(p, alt, tc));
        assert_eq!(reset_bg(), old::reset_bg(p, alt, tc));
        let sp = span_paint();
        assert_eq!(sp.muted, old::span(p, alt, tc)[4]);
        assert_eq!(sp.reset, old::span(p, alt, tc)[5]);
        assert_eq!(
            truecolor_term(),
            old::truecolor(
                std::env::var("COLORTERM").ok().as_deref(),
                &std::env::var("TERM").unwrap_or_default()
            )
        );
    }

    #[test]
    fn colour_depth_reads_colorterm_then_term_as_before() {
        let cts = [
            None,
            Some("truecolor"),
            Some("24bit"),
            Some("256color"),
            Some("no"),
            Some("0"),
            Some("yes"),
            Some(""),
        ];
        for ct in cts {
            for term in ["", "xterm-256color", "linux", "vt100", "dumb", "screen"] {
                assert_eq!(
                    truecolor_for(ct, term),
                    old::truecolor(ct, term),
                    "{ct:?} {term}"
                );
            }
        }
    }
}
