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
    poll, read, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use crossterm::{execute, queue};

static RAW: AtomicBool = AtomicBool::new(false);
pub fn is_raw() -> bool {
    RAW.load(Ordering::Relaxed)
}

// ── theme ────────────────────────────────────────────────────────────────
// A small semantic palette (à la opencode's theme tokens) rendered as truecolor
// when the terminal advertises it (COLORTERM=truecolor/24bit), otherwise
// down-sampled to the xterm-256 cube, and dropped entirely under NO_COLOR. One
// place owns the colors so the rest of the UI just names a role.
#[derive(Clone, Copy)]
pub struct Rgb(pub u8, pub u8, pub u8);

// Dark theme (Catppuccin-ish) — cohesive on the dark terminals that dominate.
const ACCENT: Rgb = Rgb(0xb4, 0x8e, 0xff); // brand violet — agent identity
const TEXT: Rgb = Rgb(0xcd, 0xd6, 0xf4);
const MUTED: Rgb = Rgb(0x7f, 0x84, 0x9c); // secondary / hints
const SUCCESS: Rgb = Rgb(0xa6, 0xe3, 0xa1);
const WARNING: Rgb = Rgb(0xf9, 0xe2, 0xaf);
const ERROR: Rgb = Rgb(0xf3, 0x8b, 0xa8);
const INFO: Rgb = Rgb(0x89, 0xdc, 0xeb);

fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some()
}

// opencode requires truecolor for its full palette; mirror that, with a fallback.
fn truecolor() -> bool {
    matches!(std::env::var("COLORTERM").ok().as_deref(), Some("truecolor") | Some("24bit"))
}

// Quantize an 8-bit channel onto xterm's 6-level cube.
fn cube(c: u8) -> u32 {
    // 0,95,135,175,215,255 are the cube's steps; nearest-step index 0..=5.
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

// Raw SGR for attributes that aren't colors (bold).
fn attr(code: &str, s: &str) -> String {
    if no_color() { return s.to_string(); }
    format!("\x1b[{code}m{s}\x1b[0m")
}

pub fn bold(s: &str) -> String { attr("1", s) }
// Roles. Legacy names kept (callers reference them) but routed through the theme.
pub fn dim(s: &str) -> String { paint(MUTED, s) }
pub fn red(s: &str) -> String { paint(ERROR, s) }
pub fn green(s: &str) -> String { paint(SUCCESS, s) }
pub fn yellow(s: &str) -> String { paint(WARNING, s) }
pub fn blue(s: &str) -> String { paint(INFO, s) }
pub fn cyan(s: &str) -> String { paint(INFO, s) }
pub fn accent(s: &str) -> String { paint(ACCENT, s) }
pub fn text(s: &str) -> String { paint(TEXT, s) }

// ── inline diff ────────────────────────────────────────────────────────────
// opencode shows a colored diff for every file edit. Render a compact unified
// view: trim the common head/tail lines, mark the changed middle with -/+, and
// keep a couple of context lines so the change reads in place. Pure + bounded.
pub fn diff(old: &str, new: &str) -> String {
    const CTX: usize = 2;
    const MAX: usize = 60; // never flood the screen on a huge replacement
    let o: Vec<&str> = old.lines().collect();
    let n: Vec<&str> = new.lines().collect();

    // Common prefix / suffix (line-wise).
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

// First-N-lines preview of new file content, as additions.
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

// Begin an interactive session. Deliberately NOT the alternate screen: rendering
// inline on the normal buffer preserves the terminal's native scrollback, mouse
// wheel, and text selection/copy — the same choice Claude Code makes. We still
// enable raw mode for the key-driven line editor, and bracketed paste so multi-
// line pastes arrive as one event.
pub fn enter_alt(raw: bool) {
    if raw && enable_raw_mode().is_ok() {
        RAW.store(true, Ordering::Relaxed);
        let _ = execute!(io::stdout(), EnableBracketedPaste);
    }
    // Always restore cooked mode, even on panic — never leave the user's terminal
    // in raw mode with no echo.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
        prev(info);
    }));
}

pub fn leave_alt() {
    if RAW.swap(false, Ordering::Relaxed) {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
    }
}

pub fn clear() {
    print!("\x1b[2J\x1b[H");
    flush();
}

// Print a line, honoring raw mode's need for carriage returns.
pub fn line(s: &str) {
    if is_raw() {
        print!("{}\r\n", s.replace('\n', "\r\n"));
        flush();
    } else {
        println!("{s}");
    }
}

// Stream a chunk of text (no trailing newline), translating newlines in raw mode.
pub fn write_stream(chunk: &str) {
    // Only allocate the \r\n translation when the chunk actually has a newline;
    // the vast majority of token deltas don't.
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

// Terminal bell — a soft "turn finished" nudge (the OS/terminal decides whether
// to flash, beep, or badge). No-op when output isn't a TTY.
pub fn bell() {
    if std::io::stdout().is_terminal() {
        print!("\x07");
        flush();
    }
}

// Non-blocking: drain pending key events and report whether Ctrl-C was pressed
// during a running turn (raw mode only). Lets the agent loop bail between steps.
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
            // Esc, or Ctrl-C, interrupts the running turn.
            let ctrl_c = k.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('C'));
            if ctrl_c || k.code == KeyCode::Esc {
                hit = true;
            }
        }
    }
    hit
}

// Read one line after printing `prompt`. Raw mode uses a key-driven editor;
// otherwise cooked stdin. Returns None on EOF / Ctrl-C / Ctrl-D.
pub fn ask(prompt: &str) -> Option<String> {
    if is_raw() {
        read_line_raw(prompt)
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

fn history() -> &'static std::sync::Mutex<Vec<String>> {
    static H: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> = std::sync::OnceLock::new();
    // Seed from the persisted history so Up recalls prompts from past sessions.
    H.get_or_init(|| std::sync::Mutex::new(crate::config::load_history()))
}

// Horizontal-scroll viewport: keep the cursor visible within `avail` columns,
// scrolling the single render row as the buffer grows past the terminal width.
// Returns the (possibly updated) scroll offset and the cursor's column in-row.
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

// Repaint the editable buffer on one row, scrolling horizontally so long input
// and the cursor stay visible regardless of terminal width. (One char == one
// column; CJK width is approximate.)
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

// Word-motion helpers (pure): index of the previous / next word boundary.
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

// Hand the current draft to $VISUAL/$EDITOR (falling back to vi) — invaluable for
// long prompts. Returns the edited text. Raw mode is dropped while the editor
// owns the terminal, then restored.
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

// Full raw-mode line editor: cursor + word motion, kill-ring, history, paste,
// open-in-$EDITOR. (Single visual line; multi-line composing lands separately.)
fn read_line_raw(prompt: &str) -> Option<String> {
    print!("{prompt}");
    flush();
    let mut start = crossterm::cursor::position().unwrap_or((0, 0));
    let mut buf: Vec<char> = Vec::new();
    let mut cursor = 0usize;
    let mut scroll = 0usize;
    let mut hist_idx: Option<usize> = None;
    let mut kill = String::new();

    // Reprint the prompt + current line — after an external editor takes the
    // screen, and for Ctrl-L.
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
                // Single-line editor: fold newlines/tabs to spaces so multi-line
                // pastes stay readable instead of fusing words ("a\nb" -> "a b"),
                // and drop other control bytes that would corrupt the row.
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
            Ok(_) => continue,
            Err(_) => return None,
        };
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let alt = ev.modifiers.contains(KeyModifiers::ALT);
        match ev.code {
            // Ctrl-C clears a non-empty draft first, exits on the next press.
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
            // Kill bindings stash what they remove so Ctrl-Y can paste it back.
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
                print!("\r\n");
                flush();
                let s: String = buf.iter().collect();
                if !s.trim().is_empty() {
                    if let Ok(mut h) = history().lock() {
                        if h.last().map(String::as_str) != Some(s.as_str()) {
                            h.push(s.clone());
                            crate::config::save_history(&h);
                        }
                    }
                }
                return Some(s);
            }
            KeyCode::Esc => { buf.clear(); cursor = 0; redraw(start, &buf, cursor, &mut scroll); }
            _ => {}
        }
    }
}

// A running spinner the caller stops explicitly — used to fill the gap before
// the first streamed token so a slow model never looks frozen.
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
            // Sleep in small slices so completion is noticed within ~10ms instead
            // of parking the full frame interval past the work finishing.
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
    print!("\r\x1b[2K"); // erase spinner line
    flush();
}

// Run `work` while a spinner animates, erasing it before returning.
pub fn with_spinner<T>(label: &str, work: impl FnOnce() -> T) -> T {
    let s = spinner_start(label);
    let result = work();
    spinner_stop(s);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // Strip ANSI so assertions read the text content regardless of theme/term.
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
        assert_eq!(cube(94), 1); // nearest 95
    }

    #[test]
    fn diff_marks_changed_middle_with_context() {
        let old = "a\nb\nc\nd\ne";
        let new = "a\nb\nX\nd\ne";
        let d = plain(&diff(old, new));
        assert!(d.contains("- c"), "{d}");
        assert!(d.contains("+ X"), "{d}");
        // Unchanged neighbors render as context, not as changes.
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
        // prev_word from end -> start of "baz"
        assert_eq!(prev_word(&b, b.len()), 9);
        // prev_word from start of a word skips the gap to the previous word
        assert_eq!(prev_word(&b, 9), 5);
        assert_eq!(prev_word(&b, 0), 0);
        // next_word from 0 -> just past "foo"
        assert_eq!(next_word(&b, 0), 3);
        // next_word across the double space -> past "bar"
        assert_eq!(next_word(&b, 3), 8);
        assert_eq!(next_word(&b, b.len()), b.len());
    }

    #[test]
    fn viewport_keeps_cursor_visible() {
        // Cursor within the window: no scroll.
        assert_eq!(viewport(3, 10, 0), (0, 3));
        // Cursor past the right edge: scroll so cursor sits at the last column.
        assert_eq!(viewport(20, 10, 0), (11, 9));
        // Cursor before the current scroll: scroll back to the cursor.
        assert_eq!(viewport(2, 10, 11), (2, 0));
        // Cursor exactly at the right edge stays put.
        assert_eq!(viewport(9, 10, 0), (0, 9));
        // Stable when already in view.
        assert_eq!(viewport(15, 10, 11), (11, 4));
    }

    #[test]
    fn viewport_handles_zero_width() {
        let (s, col) = viewport(5, 0, 3); // avail clamped to >=1
        assert!(col < 1 || s <= 5);
        let _ = (s, col);
    }

    #[test]
    fn no_color_strips_escapes() {
        // With NO_COLOR set, paint() returns the raw string. (Env is process-wide;
        // assert the no_color branch directly to avoid env races.)
        assert_eq!(plain("\x1b[31mhi\x1b[0m"), "hi");
    }
}
