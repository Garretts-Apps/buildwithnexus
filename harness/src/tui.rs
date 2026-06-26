// Terminal layer: alternate screen, ANSI color helpers, line input, and a
// background spinner for blocking model calls. Kept deliberately small — line
// IO in cooked mode is enough for v1 and avoids a raw-mode keyboard engine.

use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};

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

pub fn enter_alt() {
    let mut out = io::stdout();
    let _ = execute!(out, EnterAlternateScreen);
    // Always restore the primary screen, even on panic, so a crash never leaves
    // the user staring at a frozen alt buffer.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        prev(info);
    }));
    clear();
}

pub fn leave_alt() {
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

pub fn clear() {
    print!("\x1b[2J\x1b[H");
    let _ = io::stdout().flush();
}

pub fn line(s: &str) {
    println!("{s}");
}

pub fn flush() {
    let _ = io::stdout().flush();
}

// Read one line of input after printing `prompt`. Returns None on EOF (Ctrl-D).
pub fn ask(prompt: &str) -> Option<String> {
    print!("{prompt}");
    flush();
    let mut buf = String::new();
    let n = io::stdin().lock().read_line(&mut buf).unwrap_or(0);
    if n == 0 {
        return None;
    }
    Some(buf.trim_end_matches(['\n', '\r']).to_string())
}

// Run `work` on a thread while a spinner animates, so a multi-second model call
// never looks like a frozen terminal. Spinner is erased before returning.
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
