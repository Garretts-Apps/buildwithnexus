// Terminal layer: alternate screen, optional raw mode, ANSI colors, line input,
// and a spinner. Raw mode gives consistent key-driven input across platforms; in
// raw mode the kernel's line discipline is off, so every newline we emit must be
// "\r\n" and we echo keystrokes ourselves. Falls back to cooked line input when
// stdout isn't a TTY, so piped/headless use is unaffected.

use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossterm::event::{poll, read, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};

static RAW: AtomicBool = AtomicBool::new(false);
pub fn is_raw() -> bool {
    RAW.load(Ordering::Relaxed)
}

// ANSI SGR — hand-rolled so we don't pull a color crate. Honors NO_COLOR.
fn color(code: &str, s: &str) -> String {
    if std::env::var_os("NO_COLOR").is_some() {
        return s.to_string();
    }
    format!("\x1b[{code}m{s}\x1b[0m")
}
pub fn bold(s: &str) -> String { color("1", s) }
pub fn dim(s: &str) -> String { color("2", s) }
pub fn red(s: &str) -> String { color("31", s) }
pub fn green(s: &str) -> String { color("32", s) }
pub fn yellow(s: &str) -> String { color("33", s) }
pub fn blue(s: &str) -> String { color("34", s) }
pub fn cyan(s: &str) -> String { color("36", s) }
pub fn accent(s: &str) -> String { color("38;5;141", s) } // brand violet

pub fn enter_alt(raw: bool) {
    let _ = execute!(io::stdout(), EnterAlternateScreen);
    if raw && enable_raw_mode().is_ok() {
        RAW.store(true, Ordering::Relaxed);
    }
    // Always restore the terminal, even on panic — never leave the user in a raw
    // alt buffer with no echo.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        prev(info);
    }));
    clear();
}

pub fn leave_alt() {
    if RAW.swap(false, Ordering::Relaxed) {
        let _ = disable_raw_mode();
    }
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
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
    if is_raw() {
        print!("{}", chunk.replace('\n', "\r\n"));
    } else {
        print!("{chunk}");
    }
    flush();
}

pub fn flush() {
    let _ = io::stdout().flush();
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
            if k.kind == KeyEventKind::Press
                && k.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('C'))
            {
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

fn read_line_raw(prompt: &str) -> Option<String> {
    print!("{prompt}");
    flush();
    let mut buf = String::new();
    loop {
        let ev = match read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => k,
            Ok(_) => continue, // ignore release/resize/paste-edge events
            Err(_) => return None,
        };
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        match ev.code {
            KeyCode::Char('c') if ctrl => {
                print!("\r\n");
                flush();
                return None;
            }
            KeyCode::Char('d') if ctrl => {
                if buf.is_empty() {
                    print!("\r\n");
                    flush();
                    return None;
                }
            }
            KeyCode::Char(c) if !ctrl => {
                buf.push(c);
                print!("{c}");
                flush();
            }
            KeyCode::Backspace => {
                if buf.pop().is_some() {
                    print!("\x08 \x08"); // erase the last glyph
                    flush();
                }
            }
            KeyCode::Enter => {
                print!("\r\n");
                flush();
                return Some(buf);
            }
            KeyCode::Esc => {
                print!("\r\n");
                flush();
                return Some(String::new());
            }
            _ => {}
        }
    }
}

// Run `work` while a spinner animates, so a blocking model call never looks
// frozen. Spinner is erased before returning.
pub fn with_spinner<T>(label: &str, work: impl FnOnce() -> T) -> T {
    let running = Arc::new(AtomicBool::new(true));
    let r2 = running.clone();
    let label = label.to_string();
    let spinner = thread::spawn(move || {
        let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let mut i = 0usize;
        while r2.load(Ordering::Relaxed) {
            print!("\r{} {}", cyan(&frames[i % frames.len()].to_string()), dim(&label));
            flush();
            i += 1;
            thread::sleep(Duration::from_millis(80));
        }
    });
    let result = work();
    running.store(false, Ordering::Relaxed);
    let _ = spinner.join();
    print!("\r\x1b[2K"); // erase spinner line
    flush();
    result
}
