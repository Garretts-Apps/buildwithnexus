// Terminal layer: alternate screen, optional raw mode, ANSI colors, line input,
// and a spinner. Raw mode gives consistent key-driven input across platforms; in
// raw mode the kernel's line discipline is off, so every newline we emit must be
// "\r\n" and we echo keystrokes ourselves. Falls back to cooked line input when
// stdout isn't a TTY, so piped/headless use is unaffected.

use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossterm::cursor::MoveTo;
use crossterm::event::{
    poll, read, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use crossterm::{execute, queue};

static RAW: AtomicBool = AtomicBool::new(false);
pub fn is_raw() -> bool {
    RAW.load(Ordering::Relaxed)
}

// ── theme ────────────────────────────────────────────────────────────────
#[derive(Clone, Copy)]
pub struct Rgb(pub u8, pub u8, pub u8);

const ACCENT: Rgb = Rgb(0xb4, 0x8e, 0xff);
const TEXT: Rgb = Rgb(0xcd, 0xd6, 0xf4);
const MUTED: Rgb = Rgb(0x7f, 0x84, 0x9c);
const SUCCESS: Rgb = Rgb(0xa6, 0xe3, 0xa1);
const WARNING: Rgb = Rgb(0xf9, 0xe2, 0xaf);
const ERROR: Rgb = Rgb(0xf3, 0x8b, 0xa8);
const INFO: Rgb = Rgb(0x89, 0xdc, 0xeb);
const MODE_PLAN: Rgb = Rgb(0xa6, 0xe3, 0xa1);   // green — planning
const MODE_BUILD: Rgb = Rgb(0x89, 0xdc, 0xeb);  // cyan — building
const MODE_BSTORM: Rgb = Rgb(0xf9, 0xe2, 0xaf); // amber — thinking

fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some()
}

fn truecolor() -> bool {
    matches!(std::env::var("COLORTERM").ok().as_deref(), Some("truecolor") | Some("24bit"))
}

fn cube(c: u8) -> u32 {
    let levels = [0u8, 95, 135, 175, 215, 255];
    let mut best = 0usize;
    let mut bd = u16::MAX;
    for (i, &l) in levels.iter().enumerate() {
        let d = (l as i16 - c as i16).unsigned_abs();
        if d < bd { bd = d; best = i; }
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

fn paint(c: Rgb, s: &str) -> String {
    if no_color() { return s.to_string(); }
    format!("\x1b[{}m{s}\x1b[0m", sgr_fg(c))
}

fn attr(code: &str, s: &str) -> String {
    if no_color() { return s.to_string(); }
    format!("\x1b[{code}m{s}\x1b[0m")
}

pub fn bold(s: &str) -> String { attr("1", s) }
pub fn dim(s: &str) -> String { paint(MUTED, s) }
pub fn red(s: &str) -> String { paint(ERROR, s) }
pub fn green(s: &str) -> String { paint(SUCCESS, s) }
pub fn yellow(s: &str) -> String { paint(WARNING, s) }
pub fn blue(s: &str) -> String { paint(INFO, s) }
pub fn cyan(s: &str) -> String { paint(INFO, s) }
pub fn accent(s: &str) -> String { paint(ACCENT, s) }
pub fn text(s: &str) -> String { paint(TEXT, s) }

// Mode-colored badge: PLAN (green), BUILD (cyan), BRAINSTORM (amber).
pub fn mode_badge(mode: &str) -> String {
    let (label, color) = match mode {
        "PLAN"       => ("PLAN", MODE_PLAN),
        "BRAINSTORM" => ("BRAINSTORM", MODE_BSTORM),
        _            => ("BUILD", MODE_BUILD),
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
    while tail < o.len() - head.min(o.len()) && tail < n.len() - head.min(n.len())
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
        return format!("{shown}\n{}", dim(&format!("  …(+{} more lines)", out.len() - MAX)));
    }
    out.join("\n")
}

pub fn added_preview(content: &str) -> String {
    const MAX: usize = 40;
    let lines: Vec<&str> = content.lines().collect();
    let shown: Vec<String> = lines.iter().take(MAX).map(|l| paint(SUCCESS, &format!("+ {l}"))).collect();
    if lines.len() > MAX {
        format!("{}\n{}", shown.join("\n"), dim(&format!("  …(+{} more lines)", lines.len() - MAX)))
    } else {
        shown.join("\n")
    }
}

// ── startup banner ───────────────────────────────────────────────────────────
// Gradient wordmark: each letter of "buildwithnexus" shifts across purple→cyan→green.
fn wordmark() -> String {
    if no_color() {
        return "buildwithnexus".to_string();
    }
    // Gradient stops: ACCENT(purple) → INFO(cyan) → SUCCESS(green)
    let stops: &[(u8, u8, u8)] = &[
        (0xb4, 0x8e, 0xff), // purple  (b u i)
        (0xa0, 0xa0, 0xff), //         (l d)
        (0x89, 0xdc, 0xeb), // cyan    (w i t)
        (0x89, 0xdc, 0xeb), //         (h n)
        (0xa6, 0xe3, 0xa1), // green   (e x u s)
    ];
    let word = "buildwithnexus";
    let n = word.len();
    word.chars().enumerate().map(|(i, c)| {
        let t = i as f32 / (n - 1) as f32;
        let seg = (t * (stops.len() - 1) as f32) as usize;
        let seg = seg.min(stops.len() - 2);
        let local = t * (stops.len() - 1) as f32 - seg as f32;
        let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * local) as u8;
        let (r, g, b) = (lerp(stops[seg].0, stops[seg + 1].0),
                         lerp(stops[seg].1, stops[seg + 1].1),
                         lerp(stops[seg].2, stops[seg + 1].2));
        paint(Rgb(r, g, b), &c.to_string())
    }).collect::<Vec<_>>().join("")
}

// Print a rich full-screen-style header that establishes visual context without
// taking over the alternate screen buffer (native scroll still works).
// The UI chrome (mode badge, wordmark, keys) is identical regardless of model.
pub fn show_banner(provider: &str, model: &str, mode: &str, cwd: &str) {
    let width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80) as usize;
    let w = width.min(80);
    let bar = "─".repeat(w);

    line(&accent(&bar));
    // Wordmark row — gradient "buildwithnexus" + domain
    line(&format!("  {}  {}  {}",
        bold(&wordmark()),
        dim("·"),
        paint(Rgb(0xcb, 0xa6, 0xf7), "buildwithnexus.dev"),  // lavender
    ));
    // Context row — provider · model · cwd (truncated to fit)
    let cwd_display: String = cwd.chars().rev().take(w.saturating_sub(30)).collect::<String>()
        .chars().rev().collect();
    let cwd_label = if cwd_display.len() < cwd.len() { format!("…{cwd_display}") } else { cwd.to_string() };
    line(&dim(&format!("  {} · {} · {}", provider, model, cwd_label)));
    // Mode row
    line(&format!("  Mode: {}    {}  {}  {}",
        mode_badge(mode),
        dim("Shift+Tab to cycle"),
        dim("·"),
        dim("/help for commands"),
    ));
    line(&accent(&bar));
}

// Refresh the mode indicator line in-place after a mode change (no full clear).
pub fn show_mode_change(mode: &str) {
    line(&format!("  {} mode → {}", dim("switching"), mode_badge(mode)));
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
    let colored = if pct >= 80 { red(&bar) } else if pct >= 60 { yellow(&bar) } else { green(&bar) };
    line(&format!(
        "  {} [{}] {}",
        dim("ctx"),
        colored,
        dim(&format!("{pct}%  ·  {}k / {}k tokens", used / 1_000, total / 1_000)),
    ));
}

// Enter raw mode (and capture panics to restore the terminal even on crash).
// We render inline on the primary screen so scrollback and text selection work.
pub fn enter_alt(raw: bool) {
    if raw && enable_raw_mode().is_ok() {
        RAW.store(true, Ordering::Relaxed);
        let _ = execute!(io::stdout(), EnableBracketedPaste, EnableMouseCapture);
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
        let _ = disable_raw_mode();
        prev(info);
    }));
}

pub fn leave_alt() {
    if RAW.swap(false, Ordering::Relaxed) {
        let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
        let _ = disable_raw_mode();
    }
}

pub fn clear() {
    print!("\x1b[2J\x1b[H");
    flush();
}

pub fn line(s: &str) {
    if is_raw() {
        print!("{}\r\n", s.replace('\n', "\r\n"));
        flush();
    } else {
        println!("{s}");
    }
}

pub fn write_stream(chunk: &str) {
    if is_raw() && chunk.contains('\n') {
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
pub fn ask_task(prompt: &str) -> Option<InputEvent> {
    if !is_raw() {
        return ask(prompt).map(InputEvent::Text);
    }
    let mut acc = String::new();
    let mut p = prompt.to_string();
    loop {
        match read_line_raw(&p)? {
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

fn redraw(start: (u16, u16), buf: &[char], cursor: usize, scroll: &mut usize) {
    let width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
    let avail = width.saturating_sub(start.0).max(8) as usize;
    let (s, col) = viewport(cursor, avail, *scroll);
    *scroll = s;
    let end = (s + avail).min(buf.len());
    let shown: String = buf[s..end].iter().collect();
    let mut out = io::stdout();
    let _ = queue!(out, MoveTo(start.0, start.1), Clear(ClearType::UntilNewLine));
    let _ = write!(out, "{shown}");
    let _ = queue!(out, MoveTo(start.0.saturating_add(col as u16), start.1));
    let _ = out.flush();
}

fn prev_word(buf: &[char], mut i: usize) -> usize {
    while i > 0 && buf[i - 1].is_whitespace() { i -= 1; }
    while i > 0 && !buf[i - 1].is_whitespace() { i -= 1; }
    i
}
fn next_word(buf: &[char], mut i: usize) -> usize {
    let n = buf.len();
    while i < n && buf[i].is_whitespace() { i += 1; }
    while i < n && !buf[i].is_whitespace() { i += 1; }
    i
}

fn edit_in_editor(current: &str) -> Option<String> {
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".to_string());
    let path = std::env::temp_dir().join(format!("bwn-prompt-{}.txt", std::process::id()));
    std::fs::write(&path, current).ok()?;
    let was_raw = is_raw();
    if was_raw {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
    }
    let mut parts = editor.split_whitespace();
    let cmd = parts.next().unwrap_or("vi");
    let _ = std::process::Command::new(cmd).args(parts).arg(&path).status();
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
    "/help", "/clear", "/new", "/resume", "/init",
    "/mode", "/permissions", "/config", "/memory", "/skills", "/exit", "/quit",
];

fn load_slash_commands() -> Vec<String> {
    let mut cmds: Vec<String> = SLASH_COMMANDS_BASE.iter().map(|s| s.to_string()).collect();
    // Merge user-defined commands from ~/.buildwithnexus/commands/
    if let Ok(rd) = std::fs::read_dir(crate::config::home().join("commands")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let stem = name.trim_end_matches(".md").trim_end_matches(".sh").trim_end_matches(".py");
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
        let n = prefix.iter().zip(sc.iter()).take_while(|(a, b)| a == b).count();
        prefix.truncate(n);
    }
    prefix.into_iter().collect()
}

fn path_candidates(partial: &str, cwd: &std::path::Path) -> Vec<String> {
    let (base, dir, prefix) = match partial.rfind('/') {
        Some(i) => (&partial[..=i], cwd.join(&partial[..=i]), &partial[i + 1..]),
        None => ("", cwd.to_path_buf(), partial),
    };
    let mut out = Vec::new();
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
    hist.iter().rev().filter(|e| e.contains(query)).nth(skip).cloned()
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
        _ => {}
    }
    if let Some(partial) = token.strip_prefix('@') {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        return path_candidates(partial, &cwd).into_iter().map(|p| format!("@{p}")).collect();
    }
    Vec::new()
}

fn read_line_raw(prompt: &str) -> Option<RawLine> {
    print!("{prompt}");
    flush();
    let mut start = crossterm::cursor::position().unwrap_or((0, 0));
    let mut buf: Vec<char> = Vec::new();
    let mut cursor = 0usize;
    let mut scroll = 0usize;
    let mut hist_idx: Option<usize> = None;
    let mut kill = String::new();

    macro_rules! reline {
        () => {{
            print!("\r{prompt}");
            flush();
            start = crossterm::cursor::position().unwrap_or(start);
            redraw(start, &buf, cursor, &mut scroll);
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
                redraw(start, &buf, cursor, &mut scroll);
                continue;
            }
            Ok(Event::Mouse(m)) => {
                // Left-click on the input row moves the cursor to the clicked column.
                if m.kind == MouseEventKind::Down(MouseButton::Left) && m.row == start.1 {
                    let col = m.column as usize;
                    if col >= start.0 as usize {
                        let offset = col - start.0 as usize + scroll;
                        cursor = offset.min(buf.len());
                        redraw(start, &buf, cursor, &mut scroll);
                    }
                }
                continue;
            }
            Ok(_) => continue,
            Err(_) => return None,
        };
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let alt = ev.modifiers.contains(KeyModifiers::ALT);
        match ev.code {
            // Shift+Tab → cycle mode (clear the line and signal the REPL).
            KeyCode::BackTab => {
                buf.clear();
                print!("\r\n");
                flush();
                return Some(RawLine::CycleMode);
            }
            KeyCode::Char('c') if ctrl => {
                if buf.is_empty() {
                    print!("\r\n");
                    flush();
                    return None;
                }
                buf.clear();
                cursor = 0;
                redraw(start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('d') if ctrl => {
                if buf.is_empty() {
                    print!("\r\n");
                    flush();
                    return None;
                }
            }
            KeyCode::Char('a') if ctrl => { cursor = 0; redraw(start, &buf, cursor, &mut scroll); }
            KeyCode::Char('e') if ctrl => { cursor = buf.len(); redraw(start, &buf, cursor, &mut scroll); }
            KeyCode::Char('u') if ctrl => {
                kill = buf[..cursor].iter().collect();
                buf.drain(..cursor);
                cursor = 0;
                redraw(start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('k') if ctrl => {
                kill = buf[cursor..].iter().collect();
                buf.truncate(cursor);
                redraw(start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('w') if ctrl => {
                let i = prev_word(&buf, cursor);
                kill = buf[i..cursor].iter().collect();
                buf.drain(i..cursor);
                cursor = i;
                redraw(start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('y') if ctrl => {
                for c in kill.clone().chars() { buf.insert(cursor, c); cursor += 1; }
                redraw(start, &buf, cursor, &mut scroll);
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
                        let _ = write!(out, "{}{}",
                            dim(&format!("(reverse-i-search)`{query}`: ")),
                            m.clone().unwrap_or_default());
                        let _ = out.flush();
                    }
                    let ev = match read() {
                        Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => k,
                        Ok(_) => continue,
                        Err(_) => { buf = snapshot.0; cursor = snapshot.1; break; }
                    };
                    let c = ev.modifiers.contains(KeyModifiers::CONTROL);
                    match ev.code {
                        KeyCode::Char('r') if c => { if m.is_some() { skip += 1; } }
                        KeyCode::Char('c') | KeyCode::Char('g') if c => { buf = snapshot.0; cursor = snapshot.1; break; }
                        KeyCode::Char(ch) if !c => { query.push(ch); skip = 0; }
                        KeyCode::Backspace => { query.pop(); skip = 0; }
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
                                Some(e) => { buf = e.chars().collect(); cursor = buf.len(); }
                                None => { buf = snapshot.0; cursor = snapshot.1; }
                            }
                            break;
                        }
                        _ => {}
                    }
                }
                reline!();
            }
            KeyCode::Char('b') if alt => { cursor = prev_word(&buf, cursor); redraw(start, &buf, cursor, &mut scroll); }
            KeyCode::Char('f') if alt => { cursor = next_word(&buf, cursor); redraw(start, &buf, cursor, &mut scroll); }
            KeyCode::Char(c) if !ctrl && !alt => { buf.insert(cursor, c); cursor += 1; redraw(start, &buf, cursor, &mut scroll); }
            KeyCode::Backspace => { if cursor > 0 { buf.remove(cursor - 1); cursor -= 1; redraw(start, &buf, cursor, &mut scroll); } }
            KeyCode::Delete => { if cursor < buf.len() { buf.remove(cursor); redraw(start, &buf, cursor, &mut scroll); } }
            KeyCode::Left => { cursor = cursor.saturating_sub(1); redraw(start, &buf, cursor, &mut scroll); }
            KeyCode::Right => { if cursor < buf.len() { cursor += 1; redraw(start, &buf, cursor, &mut scroll); } }
            KeyCode::Home => { cursor = 0; redraw(start, &buf, cursor, &mut scroll); }
            KeyCode::End => { cursor = buf.len(); redraw(start, &buf, cursor, &mut scroll); }
            KeyCode::Up => {
                if let Ok(h) = history().lock() {
                    if !h.is_empty() {
                        let idx = match hist_idx { None => h.len() - 1, Some(0) => 0, Some(i) => i - 1 };
                        hist_idx = Some(idx);
                        buf = h[idx].chars().collect();
                        cursor = buf.len();
                    }
                }
                redraw(start, &buf, cursor, &mut scroll);
            }
            KeyCode::Down => {
                if let Ok(h) = history().lock() {
                    match hist_idx {
                        Some(i) if i + 1 < h.len() => { hist_idx = Some(i + 1); buf = h[i + 1].chars().collect(); cursor = buf.len(); }
                        _ => { hist_idx = None; buf.clear(); cursor = 0; }
                    }
                }
                redraw(start, &buf, cursor, &mut scroll);
            }
            KeyCode::Enter => {
                let cont = cursor > 0 && buf[cursor - 1] == '\\';
                if cont {
                    buf.remove(cursor - 1);
                    cursor -= 1;
                    redraw(start, &buf, cursor, &mut scroll);
                }
                print!("\r\n");
                flush();
                return Some(RawLine::Submit(buf.iter().collect(), cont));
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
                    redraw(start, &buf, cursor, &mut scroll);
                } else if cands.len() > 1 {
                    let common = common_prefix(&cands);
                    if common.chars().count() > token.chars().count() {
                        let new: Vec<char> = common.chars().collect();
                        buf.splice(tok_start..cursor, new.iter().copied());
                        cursor = tok_start + new.len();
                        redraw(start, &buf, cursor, &mut scroll);
                    } else {
                        print!("\r\n");
                        for c in &cands {
                            print!("  {}\r\n", dim(c));
                        }
                        flush();
                        reline!();
                    }
                }
            }
            KeyCode::Esc => { buf.clear(); cursor = 0; redraw(start, &buf, cursor, &mut scroll); }
            _ => {}
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
            print!("\r{} {}", accent(&frames[i % frames.len()].to_string()), dim(&label));
            flush();
            i += 1;
            for _ in 0..8 {
                if !r2.load(Ordering::Relaxed) {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    });
    Spinner { running, handle: Some(handle) }
}

pub fn spinner_stop(mut s: Spinner) {
    s.running.store(false, Ordering::Relaxed);
    if let Some(h) = s.handle.take() {
        let _ = h.join();
    }
    print!("\r\x1b[2K");
    flush();
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
                    if d == 'm' { break; }
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
        let h = vec!["git status".to_string(), "cargo test".to_string(), "git push".to_string()];
        assert_eq!(history_search(&h, "git", 0).as_deref(), Some("git push"));
        assert_eq!(history_search(&h, "git", 1).as_deref(), Some("git status"));
        assert_eq!(history_search(&h, "git", 2), None);
        assert_eq!(history_search(&h, "", 0), None);
        assert_eq!(history_search(&h, "cargo", 0).as_deref(), Some("cargo test"));
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
            vec!["alpha.txt".to_string(), "apple.txt".to_string(), "assets/".to_string()]
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
}
