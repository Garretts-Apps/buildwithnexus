//! Desktop notifications and taskbar progress: OSC 9, 99 and 777, and the bell.

use super::*;

// ── desktop notifications ────────────────────────────────────────────────────
// Fired when a long turn ends while the window is unfocused: OSC 99 (kitty),
// OSC 777 (urxvt, VTE, WezTerm), OSC 9 (iTerm2, WezTerm, Windows Terminal —
// where OSC 9 isn't a progress code), plus BEL so terminals without any
// notification protocol still badge or bounce. Terminals ignore what they
// don't know.
pub(super) fn osc9_is_notification() -> bool {
    let tp = std::env::var("TERM_PROGRAM").unwrap_or_default();
    tp == "iTerm.app" || tp == "WezTerm" || std::env::var_os("WT_SESSION").is_some()
}

pub(super) fn osc9_4_is_progress() -> bool {
    let tp = std::env::var("TERM_PROGRAM").unwrap_or_default();
    tp == "ghostty"
        || std::env::var_os("WT_SESSION").is_some()
        || std::env::var_os("ConEmuANSI").is_some()
}

// What the prompt does when left waiting (the Notification hook's
// `idle_prompt`): after how long, and what to call. Set once by the REPL.
pub(super) static IDLE_NOTIFIER: OnceLock<(Duration, Box<dyn Fn() + Send + Sync>)> =
    OnceLock::new();

/// Calls `f` once each time the prompt has waited `after` without a key.
pub fn on_idle(after: Duration, f: impl Fn() + Send + Sync + 'static) {
    let _ = IDLE_NOTIFIER.set((after, Box::new(f)));
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
pub(super) fn taskbar_progress(on: bool) {
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
