// Terminal layer: alternate screen, optional raw mode, ANSI colors, line input,
// and a spinner. Raw mode gives consistent key-driven input across platforms; in
// raw mode the kernel's line discipline is off, so every newline we emit must be
// "\r\n" and we echo keystrokes ourselves. Falls back to cooked line input when
// stdout isn't a TTY, so piped/headless use is unaffected.

use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::cursor::{MoveTo, RestorePosition, SavePosition};
use crossterm::event::{
    poll, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, queue};

static RAW: AtomicBool = AtomicBool::new(false);
static ALT_SCREEN: AtomicBool = AtomicBool::new(false);
static MOUSE_CAPTURED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InterruptKind {
    None = 0,
    Escape = 1,
    CtrlC = 2,
}

static INTERRUPT_KIND_VAL: AtomicU8 = AtomicU8::new(0);
// `bwn acp`: stdin carries the protocol, so nothing may read a prompt from
// it, and a cancel from the editor raises the interrupt without raw mode.
static PROTOCOL_STDIN: AtomicBool = AtomicBool::new(false);
static AGENT_RUNNING: AtomicBool = AtomicBool::new(false);
// Streamed characters landed this turn (→ tokens/s in the footer). Bumped per
// chunk, also by helper threads that capture their output: stays lock-free.
static STREAM_CHARS: AtomicUsize = AtomicUsize::new(0);
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Model name shown in the footer (set at startup and on `/model`).
pub fn set_model_label(model: &str) {
    screen::with(|s| s.status.model = model.to_string());
    render_footer();
}

/// Apply the `images` and `notify` settings keys.
pub fn configure_ui(images: &str, notify: &str) {
    let images_mode = match images.trim().to_ascii_lowercase().as_str() {
        "off" | "false" | "0" => 0,
        "kitty" => 2,
        "blocks" | "half" => 3,
        "sixel" => 4,
        _ => 1,
    };
    let notify_mode = match notify.trim().to_ascii_lowercase().as_str() {
        "off" | "false" | "0" => 0,
        "always" => 2,
        _ => 1,
    };
    screen::with(|s| {
        s.flags.images_mode = images_mode;
        s.flags.notify_mode = notify_mode;
    });
}

/// Whether the terminal window currently has focus (per CSI ?1004 reports).
pub fn is_focused() -> bool {
    screen::with(|s| s.flags.focused)
}

fn set_focused(focused: bool) {
    screen::with(|s| s.flags.focused = focused);
}

mod screen;

mod notify;
use notify::*;
pub use notify::{notify, on_idle};

// ── shared terminal state: vim mode, transcript, selection ──────────────────
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimState {
    Insert,
    Normal,
    Visual(usize),
}

pub fn get_vim_state_label() -> &'static str {
    match screen::with(|s| (s.flags.vim_mode, s.flags.vim_state)) {
        (false, _) => "",
        (true, 1) => "INSERT",
        (true, 2) => "VISUAL",
        (true, _) => "NORMAL",
    }
}

pub fn toggle_vim_mode() -> bool {
    screen::with(|s| {
        s.flags.vim_mode = !s.flags.vim_mode;
        if s.flags.vim_mode {
            s.flags.vim_state = 0; // Default to Normal mode
        }
        s.flags.vim_mode
    })
}

pub fn is_vim_mode() -> bool {
    screen::with(|s| s.flags.vim_mode)
}

// 0 = Normal, 1 = Insert, 2 = Visual.
fn set_vim_state(state: u8) {
    screen::with(|s| s.flags.vim_state = state);
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

mod theme;
pub(crate) use theme::no_color;
use theme::*;
pub use theme::{
    accent, blue, bold, cyan, dim, green, init_console, italic, line_mode, mode_badge, red, rule,
    set_line_mode, set_theme, text, theme_name, underline, yellow, THEME_NAMES,
};

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

mod images;
use images::*;
pub use images::{attachment_token, image_preview, pasted_media_path, show_image_file, term_width};

mod markdown;
pub use markdown::StreamRenderer;
use markdown::*;

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

fn top_open_input() -> Option<OpenInput> {
    screen::with(|s| s.composer.open_inputs.last().cloned())
}

fn update_open_input(prompt: &str, buf: &[char], cursor: usize) {
    screen::with(|s| {
        if let Some(top) = s.composer.open_inputs.last_mut() {
            top.prompt.clear();
            top.prompt.push_str(prompt);
            top.buf.clear();
            top.buf.extend_from_slice(buf);
            top.cursor = cursor;
        }
    });
}

/// Registration of an open prompt, picker or composer: owns the keyboard
/// and the input box until dropped.
struct InputOwner {
    _keys: KeyboardGuard,
}

impl InputOwner {
    fn open(prompt: &str, buf: &[char], cursor: usize) -> Self {
        let keys = keyboard_lock();
        let input = OpenInput {
            prompt: prompt.to_string(),
            buf: buf.to_vec(),
            cursor,
        };
        screen::with(|s| s.composer.open_inputs.push(input));
        InputOwner { _keys: keys }
    }
}

impl Drop for InputOwner {
    fn drop(&mut self) {
        screen::with(|s| s.composer.open_inputs.pop());
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

// Every keyboard read goes through here: a resize event carries the new
// size, which is the only time the cached size changes while the TUI runs.
fn read() -> io::Result<Event> {
    let ev = crossterm::event::read()?;
    if let Event::Resize(w, h) = ev {
        store_term_size(w, h);
    }
    Ok(ev)
}

// ── terminal gone ────────────────────────────────────────────────────────────
// When the terminal goes away and no SIGHUP reaches the process (it is not
// the terminal's session leader, or the signal went elsewhere), every read
// of the keyboard returns end-of-input at once, for ever, and crossterm's
// reader loops on that without returning (a pinned core, and no `Err` for
// the callers that set INPUT_CLOSED). A watcher thread therefore sleeps in
// poll(2) on stdin asking for no events — a hangup is reported regardless —
// and ends the process the way SIGHUP would have: terminal restored, status
// 129. It never wakes otherwise.
#[cfg(unix)]
static STDIN_IS_TTY: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
static HANGUP_WATCHED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
fn watch_for_hangup() {
    STDIN_IS_TTY.store(io::stdin().is_terminal(), Ordering::Relaxed);
    if !STDIN_IS_TTY.load(Ordering::Relaxed) || HANGUP_WATCHED.swap(true, Ordering::Relaxed) {
        return;
    }
    let spawned = thread::Builder::new()
        .name("hangup".into())
        .stack_size(64 * 1024)
        .spawn(|| {
            loop {
                let mut fd = libc::pollfd {
                    fd: libc::STDIN_FILENO,
                    events: 0,
                    revents: 0,
                };
                // SAFETY: one valid pollfd; blocks until the terminal hangs up.
                let n = unsafe { libc::poll(&mut fd, 1, -1) };
                if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if n < 0 || !RAW.load(Ordering::Relaxed) {
                    // The TUI has let go of the terminal (leaving, or a
                    // child process has it): plain reads see the end of
                    // input. The next `enter_alt` starts a new watch.
                    break;
                }
                exit_if_terminal_gone();
                // Woken without a hangup: not expected; do not spin on it.
                thread::sleep(Duration::from_millis(250));
            }
            HANGUP_WATCHED.store(false, Ordering::Relaxed);
        });
    if spawned.is_err() {
        HANGUP_WATCHED.store(false, Ordering::Relaxed);
    }
}

#[cfg(not(unix))]
fn watch_for_hangup() {}

/// Whether the terminal on stdin has hung up. A poll with no wait: no lock,
/// no allocation, so the panic hook may call it.
#[cfg(unix)]
fn terminal_hung_up() -> bool {
    if !STDIN_IS_TTY.load(Ordering::Relaxed) {
        return false;
    }
    let mut fd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd, zero timeout.
    let n = unsafe { libc::poll(&mut fd, 1, 0) };
    n > 0 && fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}

/// Exit status when the terminal is gone: the one SIGHUP gives (128 + 1).
#[cfg(unix)]
const TERMINAL_GONE_STATUS: i32 = 128 + libc::SIGHUP;

// The terminal is gone: put it back as far as that still means anything and
// exit as SIGHUP would have. Never returns on Unix.
#[cfg(unix)]
fn exit_if_terminal_gone() {
    if terminal_hung_up() {
        INPUT_CLOSED.store(true, Ordering::Relaxed);
        signal_restore::restore_and_exit(TERMINAL_GONE_STATUS);
    }
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
            // The buffer and the queue are edited under the screen lock;
            // the repaint runs after it is released (render_output and
            // render_queued_composer take it again).
            let repaint = screen::with(|s| {
                let (ta, mq) = (&mut s.composer.typeahead, &mut s.composer.queue);
                match k.code {
                    KeyCode::Enter => {
                        let text: String = ta.buf.iter().collect();
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            if ta.from_queue {
                                queue_put_back(mq, trimmed.to_string());
                            } else {
                                mq.push(trimmed.to_string());
                            }
                            ta.buf.clear();
                            ta.cursor = 0;
                            ta.from_queue = false;
                            return true;
                        }
                    }
                    // Plain Up is intentionally a no-op for the queue: queue
                    // editing is Ctrl+Q (as the queued-row hint says), so a
                    // stray Up can't destructively pop the newest message.
                    KeyCode::Up if !alt => {}
                    KeyCode::Char('q') if ctrl => {
                        // Take the NEXT message (front) into the buffer.
                        if let Some(next) = queue_take_next(mq) {
                            ta.buf = next.chars().collect();
                            ta.cursor = ta.buf.len();
                            ta.from_queue = true;
                            return true;
                        }
                    }
                    KeyCode::Char('x') if ctrl => {
                        return queue_take_next(mq).is_some();
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
                            queue_put_back(mq, msg);
                            ta.buf.clear();
                            ta.cursor = 0;
                            ta.from_queue = false;
                            return true;
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
                false
            });
            if repaint {
                render_output();
                clear_composer();
                render_footer();
                render_queued_composer();
                return raised;
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
            let chars: Vec<char> = clean_paste(&text).chars().collect();
            screen::with(|s| {
                let ta = &mut s.composer.typeahead;
                let i = ta.cursor;
                ta.buf.splice(i..i, chars.iter().copied());
                ta.cursor += chars.len();
            });
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
        Event::FocusGained => set_focused(true),
        Event::FocusLost => set_focused(false),
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
// render_composer. Lock order: KEYBOARD → RENDER → SCREEN (tui/screen.rs:
// transcript, composer, selection, footer) → the image store. Never call a
// render function, or anything else that takes the screen lock, from inside
// `screen::with`.
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

// A question's answer and a picker's filter are one line: pasted line breaks
// and tabs become spaces, and every other control char (C0, DEL, C1, so no
// escape sequences) is dropped.
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
// when it is sent. Smaller pastes go in as text, line breaks kept.
const PASTE_COLLAPSE_CHARS: usize = 1_000;
const PASTE_COLLAPSE_LINES: usize = 10;

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
// and every other control character (escape sequences) is dropped. The
// composer keeps it this way too, and shows each break as `↵`.
fn clean_paste(s: &str) -> String {
    s.replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect()
}

// How the one-row composer shows its text: a line break as `↵` and a tab as
// a space, each one column wide like the character it stands for.
fn composer_glyphs(chars: &[char]) -> String {
    chars
        .iter()
        .map(|&c| match c {
            '\n' => '↵',
            '\t' => ' ',
            c => c,
        })
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
    let size = thousands(chars);
    Some(screen::with(|s| {
        let store = &mut s.composer.pastes;
        let token = match store.len() {
            0 => format!("[pasted {size} chars]"),
            n => format!("[pasted {size} chars #{}]", n + 1),
        };
        store.push((token.clone(), text));
        token
    }))
}

/// A submitted message with each paste token replaced by its paste.
fn expand_pastes(text: &str) -> String {
    let mut out = text.to_string();
    screen::with(|s| {
        for (token, full) in s.composer.pastes.iter() {
            if out.contains(token.as_str()) {
                out = out.replacen(token.as_str(), full, 1);
            }
        }
    });
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
    // Painted from a snapshot: the composer repaint takes the screen lock.
    let (buf, cursor, queued) = screen::with(|s| {
        let c = &s.composer;
        (c.typeahead.buf.clone(), c.typeahead.cursor, c.queue.clone())
    });
    let mut out: Vec<u8> = Vec::new();
    // One atomic frame (DEC 2026): the queued rows paint together with
    // no intermediate state a fast terminal could show mid-repaint.
    let _ = write!(out, "\x1b[?2026h");
    let c_top = composer_top();
    let q_len = queued.len() as u16;
    for (i, msg) in queued.iter().enumerate() {
        let row = c_top.saturating_sub(q_len).saturating_add(i as u16);
        let _ = queue!(out, MoveTo(0, row), Clear(ClearType::CurrentLine));
        let _ = write!(
            out,
            "  {} {} {} {}",
            dim("├─"),
            dim("queued:"),
            bold(&composer_glyphs(&msg.chars().collect::<Vec<_>>())),
            dim(queued_row_hint(i))
        );
    }
    let _ = write!(out, "\x1b[?2026l");
    write_frame(&out);
    let has_queued = !queued.is_empty();
    let mut scroll = 0usize;
    if let Some(open) = open {
        // A prompt or picker owns the box: repaint it as it is, never
        // the idle composer.
        render_composer(&open.prompt, &open.buf, open.cursor, &mut scroll);
        cursor_show();
        return;
    }
    let prompt_str = if is_agent_running() && (!buf.is_empty() || has_queued) {
        format!("{} {} ", dim("queued"), accent("›"))
    } else {
        format!("{} ", accent("›"))
    };
    render_composer(&prompt_str, &buf, cursor, &mut scroll);
    cursor_color_accent();
    set_cursor_shape(CursorShape::Bar);
    cursor_show();
}

fn take_typeahead() -> (Vec<char>, usize) {
    screen::with(|s| s.composer.take_typeahead())
}

// ── terminal size ────────────────────────────────────────────────────────────
// Asking the terminal for its size is an open of /dev/tty, an ioctl and a
// close, and one keystroke asks about ten times. The size is therefore kept
// in two atomics while the TUI owns the keyboard (raw mode): read once when
// it takes the screen, and again only when a resize event arrives (`read`)
// — the event is how SIGWINCH reaches us. Outside raw mode nothing reads
// events, so every call asks afresh, as before. 0 = not known.
static TERM_W: AtomicU16 = AtomicU16::new(0);
static TERM_H: AtomicU16 = AtomicU16::new(0);

fn query_term_size() -> (u16, u16) {
    let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
    (if w == 0 { 80 } else { w }, if h == 0 { 24 } else { h })
}

fn store_term_size(w: u16, h: u16) {
    TERM_W.store(if w == 0 { 80 } else { w }, Ordering::Relaxed);
    TERM_H.store(if h == 0 { 24 } else { h }, Ordering::Relaxed);
}

/// Ask the terminal and remember the answer: start-up, and after anything
/// that may have changed the size unseen (a child process had the terminal).
fn refresh_term_size() -> (u16, u16) {
    let (w, h) = query_term_size();
    store_term_size(w, h);
    (w, h)
}

fn term_size() -> (u16, u16) {
    if !RAW.load(Ordering::Relaxed) {
        return query_term_size();
    }
    match (
        TERM_W.load(Ordering::Relaxed),
        TERM_H.load(Ordering::Relaxed),
    ) {
        (0, _) | (_, 0) => refresh_term_size(),
        size => size,
    }
}

// Alt-screen bottom chrome: a 3-row bordered composer box plus the footer.
//   h-4  ╭───────────────────╮   composer_top()
//   h-3  │ › input text      │   composer_row()
//   h-2  ╰───────────────────╯   composer_bottom()
//   h-1  permission: ask · …     footer_row()
// Queued messages stack directly above the box.
fn reserved_rows() -> u16 {
    let q_len = screen::with(|s| s.composer.queue.len()) as u16;
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
                // the dim URL suffix keeps older terminals usable. Only web
                // and mail links become clickable: the model writes these,
                // and a click on file:, ssh:, or an app's own scheme can run
                // a handler the user never meant to start. The URL still
                // shows, so the user can copy it deliberately.
                let shown = underline(&cyan(label));
                let label = if model_link_clickable(url) {
                    hyperlink(url, &shown)
                } else {
                    shown
                };
                out.push_str(&format!("{label} {}", dim(&format!("({url})"))));
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

/// The braille spinner every progress indicator cycles through.
pub(crate) const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub(crate) fn strip_ansi(s: &str) -> String {
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

// Schemes a model-written markdown link may open with a click. File links
// the harness makes itself (file_link) do not pass through here.
fn model_link_clickable(url: &str) -> bool {
    let url = url.trim_start().to_ascii_lowercase();
    ["http://", "https://", "mailto:"]
        .iter()
        .any(|scheme| url.starts_with(scheme))
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
// can scroll the whole screen (a visible flash). 0 = no region set. An atomic,
// not a `Screen` field: the panic hook resets it (reset_output_region), and
// the panicking thread may be the one holding the screen lock.
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

    let (footer, model, started, used, total) = screen::with(|s| {
        let st = &s.status;
        (
            st.footer.clone(),
            st.model.clone(),
            st.work_started_ms,
            st.context_used,
            st.context_total,
        )
    });
    let model_badge = if model.is_empty() {
        String::new()
    } else {
        format!("{} ", dim(&model))
    };
    let base_text = if is_agent_running() {
        // Live readout: braille spinner, elapsed, streamed tokens/s.
        let elapsed_ms = now_ms().saturating_sub(started).max(1);
        let secs = elapsed_ms / 1000;
        let frames = SPINNER_FRAMES;
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
                crate::provider::short_tokens(used),
                crate::provider::short_tokens(total)
            ))
        )
    } else {
        String::new()
    };

    let offset = screen::with(|s| s.transcript.scroll_offset);
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

    // Folders added with /add-dir: the agent may write there too.
    let base_text = format!("{base_text}{}", dirs_badge(crate::workdirs::count()));

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

// ` · +2 dirs` with folders added to the session, else nothing.
fn dirs_badge(n: usize) -> String {
    match n {
        0 => String::new(),
        1 => format!(" {}", accent("· +1 dir")),
        n => format!(" {}", accent(&format!("· +{n} dirs"))),
    }
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
pub fn flash_footer(msg: &str) {
    let now = monotonic_ms();
    screen::with(|s| s.status.set_flash(msg, now));
    render_footer();
}

fn active_flash() -> Option<String> {
    let now = monotonic_ms();
    screen::with(|s| s.status.active_flash(now))
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
    let footer = format!(
        "{} {} {}",
        dim("permission:"),
        bold(mode),
        dim("· /permissions · wheel/PgUp · drag-copy · /mouse")
    );
    screen::with(|s| s.status.footer = footer);
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
    let mut out: Vec<u8> = Vec::with_capacity(rows * (width + 16));
    // Synchronized output (DEC 2026): supporting terminals (kitty, iTerm2,
    // WezTerm, Alacritty, foot…) apply the whole repaint as one atomic frame
    // — zero tearing/flicker. Ignored elsewhere.
    let _ = write!(out, "\x1b[?2026h");
    // Every row starts on the theme's background, whatever the row before
    // it left set, so a painted theme stays whole on any terminal.
    let bg = theme_bg();
    // The frame is built from the transcript under the screen lock; nothing
    // in here writes to the terminal or takes the lock again.
    let (placements, kept) = screen::with(|s| {
        let sel = s.selection.current;
        let view = &mut s.transcript;
        view.text.ensure_size(width, height as usize);
        let total = view.text.total_rows();
        let max_offset = total.saturating_sub(rows);
        view.scroll_offset = view.scroll_offset.min(max_offset);
        let start = total.saturating_sub(rows + view.scroll_offset);
        let visible = view.text.rows_range(start, rows);
        // Sixel images are pixels over blank rows. One that is on screen at
        // the same place as last frame, with nothing drawn over it since, is
        // left alone: rewriting its rows would erase it, and re-sending it
        // every streamed frame would be slow.
        let placements = sixel_placements(&visible, rows);
        let kept: Vec<Placement> = if std::mem::replace(&mut s.images.dirty, false) {
            Vec::new()
        } else {
            placements
                .iter()
                .filter(|p| s.images.drawn.contains(p))
                .copied()
                .collect()
        };
        let in_kept = |row: usize| {
            kept.iter()
                .any(|&(_, top, _, n)| row >= top && row < top + n)
        };
        let mut plain_rows = Vec::with_capacity(rows);
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
        view.visible_rows = plain_rows;
        (placements, kept)
    });
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
    screen::with(|s| s.images.drawn = placements);
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

fn monotonic_ms() -> u64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

fn render_output_throttled() {
    let now = monotonic_ms();
    let due = screen::with(|s| {
        let last = s.transcript.last_stream_frame_ms;
        let due = now.saturating_sub(last) >= FRAME_MS || last == 0;
        if due {
            s.transcript.last_stream_frame_ms = now.max(1);
        }
        due
    });
    if due {
        render_output();
    }
}

fn scroll_output(delta: isize) {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    let rows = if delta.is_negative() {
        0
    } else {
        term_size().1.saturating_sub(reserved_rows()) as usize
    };
    screen::with(|s| {
        let view = &mut s.transcript;
        view.scroll_offset = if delta.is_negative() {
            view.scroll_offset.saturating_sub(delta.unsigned_abs())
        } else {
            let max_offset = view.text.total_rows().saturating_sub(rows);
            view.scroll_offset
                .saturating_add(delta as usize)
                .min(max_offset)
        };
    });
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
    screen::with(|s| s.transcript.scroll_offset = 0);
    render_output();
}

fn in_output_region(row: u16) -> bool {
    ALT_SCREEN.load(Ordering::Relaxed) && row < composer_top()
}

fn click_count(row: u16, col: u16) -> u8 {
    let now = monotonic_ms();
    screen::with(|s| s.selection.click_count(now, row, col))
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
            let span = screen::with(|s| s.transcript.visible_rows.get(row as usize).cloned())
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
            let len = screen::with(|s| s.transcript.visible_rows.get(row as usize).cloned())
                .map(|l| str_width(&l))
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
    screen::with(|s| s.selection.current = Some(new_sel));
    render_output();
    render_queued_composer();
}

fn selection_drag(row: u16, col: u16) {
    if !in_output_region(row) {
        return;
    }
    screen::with(|s| {
        if let Some(sel) = s.selection.current.as_mut() {
            sel.focus = SelectPos { row, col };
            sel.sticky = false;
        }
    });
    render_output();
    render_queued_composer();
}

fn selection_finish(row: u16, col: u16) {
    if in_output_region(row) {
        screen::with(|s| {
            if let Some(sel) = s.selection.current.as_mut() {
                // A word/line selection stays put on the release click.
                if !sel.sticky {
                    sel.focus = SelectPos { row, col };
                }
            }
        });
    }
    let copied = selected_text();
    screen::with(|s| s.selection.current = None);
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
    let (sel, rows) = screen::with(|s| {
        s.selection
            .current
            .map(|sel| (sel, s.transcript.visible_rows.clone()))
    })?;
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
    let shown = composer_glyphs(&buf[s..end]);
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
        screen::with(|s| s.transcript.scroll_offset = 0);
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
    // "Shift+Tab cycle" is unique to the banner's mode row (the REPL hint
    // line says "Shift+Tab to change mode").
    screen::with(|s| {
        let t = &mut s.transcript.text;
        let idx = t
            .lines
            .iter()
            .position(|l| strip_ansi(l).contains("Shift+Tab cycle"));
        if let Some(i) = idx {
            t.set(i, row);
        }
    });
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
    screen::with(|s| {
        s.status.context_used = used;
        s.status.context_total = total;
    });
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
        restore_and_exit(128 + sig);
    }

    /// Put the terminal back and end the process with `status`. Only
    /// atomics, `write`, `tcsetattr` and `_exit`: safe in a signal handler
    /// and on any thread, whatever locks are held elsewhere.
    pub fn restore_and_exit(status: libc::c_int) -> ! {
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
            libc::_exit(status);
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
            refresh_term_size();
            RAW.store(true, Ordering::Relaxed);
            watch_for_hangup();
        }
        install_panic_hook();
        return;
    }
    if raw {
        // Before any terminal-state change: snapshot the cooked termios and
        // arm the restore-on-signal handlers.
        #[cfg(any(unix, windows))]
        signal_restore::install();
        screen::with(|s| {
            s.transcript.scroll_offset = 0;
            s.transcript.invalidate_stream_line();
            s.transcript.text.clear();
        });
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
        // The size as it is now: a child process may have had the terminal.
        refresh_term_size();
        RAW.store(true, Ordering::Relaxed);
        watch_for_hangup();
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
            // A write to a terminal that has gone away panics (`print!`):
            // that is a hangup, not a crash.
            #[cfg(unix)]
            if terminal_hung_up() {
                // SAFETY: `_exit` only ends the process.
                unsafe { libc::_exit(TERMINAL_GONE_STATUS) };
            }
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
        screen::with(|s| {
            s.transcript.scroll_offset = 0;
            s.transcript.invalidate_stream_line();
        });
        free_images();
        screen::with(|s| s.transcript.text.clear());
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
    clip_ansi(s, max_cols).0
}

/// `clip_ansi_line`, plus the display width of what it kept.
fn clip_ansi(s: &str, max_cols: usize) -> (String, usize) {
    if max_cols == 0 {
        return (String::new(), 0);
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
    (out, visible)
}

fn pad_ansi_line(s: &str, width: usize) -> String {
    let (mut out, visible) = clip_ansi(s, width);
    out.push_str(&" ".repeat(width - visible));
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

// ── helper output capture ────────────────────────────────────────────────────
// A helper running beside others (agent.rs) writes its transcript lines and
// --json events here instead of the screen, so several helpers never
// interleave mid-line; its block is shown, labelled, when it finishes. Per
// thread: each helper runs on its own.
#[derive(Default)]
struct Captured {
    lines: Vec<String>,
    // A streamed line still waiting for its newline.
    partial: String,
}

impl Captured {
    fn end_partial(&mut self) {
        if !self.partial.is_empty() {
            self.lines.push(std::mem::take(&mut self.partial));
        }
    }
}

thread_local! {
    static CAPTURE: std::cell::RefCell<Option<Captured>> = const { std::cell::RefCell::new(None) };
}

/// From now on this thread's transcript output is kept for `capture_take`.
pub fn capture_start() {
    CAPTURE.with(|c| *c.borrow_mut() = Some(Captured::default()));
}

/// Ends this thread's capture and returns what it held, one entry per line.
pub fn capture_take() -> Vec<String> {
    CAPTURE
        .with(|c| c.borrow_mut().take())
        .map(|mut c| {
            c.end_partial();
            c.lines
        })
        .unwrap_or_default()
}

pub fn capturing() -> bool {
    CAPTURE.with(|c| c.borrow().is_some())
}

/// Runs `f` with this thread's capture set aside, so what it shows (a
/// helper's approval prompt) reaches the screen at once.
pub fn uncaptured<T>(f: impl FnOnce() -> T) -> T {
    let saved = CAPTURE.with(|c| c.borrow_mut().take());
    let out = f();
    CAPTURE.with(|c| *c.borrow_mut() = saved);
    out
}

// Keeps `s` (one or more lines) when this thread is capturing.
fn capture_lines(s: &str) -> bool {
    CAPTURE.with(|c| {
        let mut c = c.borrow_mut();
        let Some(c) = c.as_mut() else {
            return false;
        };
        c.end_partial();
        c.lines
            .extend(s.replace('\r', "").split('\n').map(str::to_string));
        true
    })
}

// Keeps a streamed chunk when this thread is capturing: whole lines as they
// complete, the rest until its newline.
fn capture_stream(chunk: &str) -> bool {
    CAPTURE.with(|c| {
        let mut c = c.borrow_mut();
        let Some(c) = c.as_mut() else {
            return false;
        };
        for ch in chunk.chars() {
            match ch {
                '\r' => {}
                '\n' => c.lines.push(std::mem::take(&mut c.partial)),
                _ => c.partial.push(ch),
            }
        }
        true
    })
}

// Drops the captured partial line, or replaces it with `with` (the streamed
// line's rendered form).
fn capture_settle_partial(with: Option<&str>) -> bool {
    CAPTURE.with(|c| {
        let mut c = c.borrow_mut();
        let Some(c) = c.as_mut() else {
            return false;
        };
        c.partial.clear();
        if let Some(l) = with {
            c.lines.push(l.to_string());
        }
        true
    })
}

/// Keeps one --json event line when this thread is capturing.
pub(crate) fn capture_event(line: &str) -> bool {
    CAPTURE.with(|c| match c.borrow_mut().as_mut() {
        Some(c) => {
            c.lines.push(line.to_string());
            true
        }
        None => false,
    })
}

// Replace the open streamed line with its rendered form and close it. Falls
// back to appending a fresh line when no stream line is open (start of turn,
// or a notice landed mid-stream and invalidated the index).
fn commit_stream_line(rendered: &str) {
    if capture_settle_partial(Some(rendered)) {
        return;
    }
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        line(rendered);
        return;
    }
    // The lock is released before painting: render_output() takes it again.
    if screen::with(|s| s.transcript.commit_stream(rendered)) {
        render_output();
        clear_composer();
    } else {
        line(rendered);
    }
}

// Remove the open streamed line entirely (an echoed raw partial that turned
// out to be chrome — a code fence or protocol JSON — and must not be shown).
fn retract_stream_line() {
    if capture_settle_partial(None) {
        return;
    }
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    screen::with(|s| s.transcript.retract_stream());
    render_output();
    clear_composer();
}

pub fn line(s: &str) {
    if capture_lines(s) {
        return;
    }
    if ALT_SCREEN.load(Ordering::Relaxed) {
        screen::with(|scr| scr.transcript.push_lines(s));
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
    if capture_stream(chunk) {
        return;
    }
    if ALT_SCREEN.load(Ordering::Relaxed) {
        screen::with(|s| s.transcript.stream(chunk));
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
                let due = screen::with(|s| {
                    let due = now.saturating_sub(s.status.last_status_tick_ms) >= 250;
                    if due {
                        s.status.last_status_tick_ms = now;
                    }
                    due
                });
                if due {
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
            let now = now_ms();
            screen::with(|s| s.status.work_started_ms = now);
            STREAM_CHARS.store(0, Ordering::Relaxed);
            taskbar_progress(true);
        }
        start_typeahead_thread();
    } else if was {
        taskbar_progress(false);
        let now = now_ms();
        // A turn long enough to have walked away from, and not a burst of
        // nested start/stop pairs (sub-agents) re-announcing the same turn.
        let announce = screen::with(|s| {
            let elapsed = now.saturating_sub(s.status.work_started_ms);
            let due = elapsed >= 8_000 && now.saturating_sub(s.status.last_notify_ms) > 5_000;
            if due {
                s.status.last_notify_ms = now;
            }
            due.then_some(elapsed)
        });
        if let Some(elapsed) = announce {
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
        screen::with(|s| s.composer.queue.clear());
    }
}

pub fn get_interrupt_kind() -> InterruptKind {
    if PROTOCOL_STDIN.load(Ordering::Relaxed) {
        return interrupt_kind();
    }
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

/// For `bwn acp`: stdin is the protocol channel, so prompts answer "no
/// answer" without reading it, and `trigger_interrupt` (a cancel from the
/// editor) stops the turn without a raw terminal.
pub fn set_protocol_stdin() {
    PROTOCOL_STDIN.store(true, Ordering::Relaxed);
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

// What the input box reads while a picker is open.
const PICKER_HINT: &str = "↑↓ choose · Enter select · Esc close";

// Keyboard state of an open picker, kept apart from the terminal so it can
// be tested: typed text filters the list, a digit typed before any other
// text moves the highlight to that numbered row, and only Enter picks, and
// only a row that is shown — so a sentence typed into a forgotten picker
// ("1. add tests" at Approve Plan) never confirms anything.
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
                            // Unfiltered, row n is at position n - 1.
                            self.selected = n - 1;
                            return PickerStep::Redraw;
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

// Rows 1-9 carry their number: typing it moves to the row.
fn picker_number(i: usize) -> String {
    if i < 9 {
        format!("{}", i + 1)
    } else {
        " ".to_string()
    }
}

/// Opens a picker over `items` and returns the chosen index, or None when
/// it is closed (Esc, Ctrl+C) or the input ends. While it is open it owns
/// the keyboard: typed text filters the list, a digit or ↑/↓ moves the
/// highlight, and Enter picks the highlighted row of the filtered list.
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
            "  └── type to filter · 1-9 or ↑↓ move · Enter select ─────────────────────────────"
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
    fn digits_move_the_highlight_and_enter_selects() {
        let items = items();
        // A digit moves to that row; only Enter picks it.
        let mut p = Picker::default();
        assert_eq!(
            press(&mut p, &items, KeyCode::Char('2')),
            PickerStep::Redraw
        );
        assert_eq!(p.selected, 1);
        assert!(p.filter.is_empty());
        assert_eq!(press(&mut p, &items, KeyCode::Enter), PickerStep::Choose(1));
        // "1. add tests" typed at Approve Plan never runs the plan: the 1
        // moves the highlight, the rest filters, and Enter finds no match.
        let mut p = Picker::default();
        let steps = type_line(&mut p, &items, "1. add tests");
        assert!(
            !steps.iter().any(|s| matches!(s, PickerStep::Choose(_))),
            "{steps:?}"
        );
        // A later digit moves the highlight again.
        let mut p = Picker::default();
        press(&mut p, &items, KeyCode::Char('3'));
        press(&mut p, &items, KeyCode::Char('1'));
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn digits_past_the_list_filter_and_esc_or_ctrl_c_close() {
        let items = items();
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
    fn esc_esc_rewinds_inside_its_window_and_the_prompt_comes_back() {
        assert!(!esc_esc_rewinds(None, 5_000));
        assert!(esc_esc_rewinds(Some(5_000), 5_900));
        assert!(!esc_esc_rewinds(Some(5_000), 6_001));
        prefill_composer("fix it\r\nplease");
        let draft = screen::with(|s| s.composer.draft.take()).unwrap();
        assert_eq!(draft.buf.iter().collect::<String>(), "fix it\nplease");
        assert_eq!(draft.cursor, draft.buf.len());
        assert!(draft.lines.is_empty());
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
    /// Esc Esc on an empty input box: open /rewind.
    Rewind,
}

// ── single-line ask ──────────────────────────────────────────────────────────
/// Asks one question and returns the answer. While it is open the prompt
/// owns the keyboard and the input box. Esc, Ctrl+C, and Ctrl+D on an empty
/// line return None: callers treat None as cancel, never as an empty answer.
/// Without a terminal it reads one line from stdin (None at end of input).
pub fn ask(prompt: &str) -> Option<String> {
    ask_prefilled(prompt, "")
}

/// `ask` with `text` already in the input box to edit, the cursor at its
/// end (a plan step). Without a terminal the text cannot be shown, and an
/// empty line keeps it.
pub fn ask_prefilled(prompt: &str, text: &str) -> Option<String> {
    if PROTOCOL_STDIN.load(Ordering::Relaxed) {
        return None;
    }
    let prompt = &*sanitize_prompt(prompt);
    let _pause_guard = PauseAgentRunningGuard::new();
    // The text can be the model's (a plan step): one line, no escapes.
    let prefill = sanitize_paste(text);
    if is_raw() {
        let cursor = prefill.len();
        match read_line_raw_prefill(prompt, prefill, cursor, false) {
            Some(RawLine::Submit(s, _)) => Some(s),
            _ => None,
        }
    } else if io::stdin().is_terminal() && io::stdout().is_terminal() {
        match read_line_plain(prompt, prefill, false) {
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
        let line = buf.trim_end_matches(['\n', '\r']);
        Some(if line.is_empty() { text } else { line }.to_string())
    }
}

/// Asks for one key: Enter gives `'\n'`, one of `keys` (either case) gives
/// that key, and Esc, Ctrl+C or Ctrl+D give None; other keys do nothing.
/// Without a terminal it reads a line, where an empty line is Enter.
pub fn ask_key(prompt: &str, keys: &[char]) -> Option<char> {
    if PROTOCOL_STDIN.load(Ordering::Relaxed) {
        return None;
    }
    let prompt = &*sanitize_prompt(prompt);
    let _pause_guard = PauseAgentRunningGuard::new();
    let listed = |c: char| keys.iter().copied().find(|k| k.eq_ignore_ascii_case(&c));
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        print!("{prompt}");
        flush();
        let mut buf = String::new();
        if io::stdin().lock().read_line(&mut buf).unwrap_or(0) == 0 {
            INPUT_CLOSED.store(true, Ordering::Relaxed);
            return None;
        }
        let answer = buf.trim();
        return match answer.chars().next() {
            None => Some('\n'),
            Some(c) => listed(c),
        };
    }
    let alt = ALT_SCREEN.load(Ordering::Relaxed);
    let _raw = RawForRead::on();
    let _keys = keyboard_lock();
    let owner = alt.then(|| InputOwner::open(prompt, &[], 0));
    let mut scroll = 0usize;
    if alt {
        redraw(prompt, (0, 0), &[], 0, &mut scroll);
    } else {
        print!("{prompt}");
        flush();
    }
    cursor_show();
    let answer = loop {
        match read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                match k.code {
                    KeyCode::Enter => break Some('\n'),
                    KeyCode::Esc => break None,
                    KeyCode::Char('c' | 'd') if ctrl => break None,
                    KeyCode::Char(c) if !ctrl => {
                        if let Some(key) = listed(c) {
                            break Some(key);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Resize(_, _)) if alt => {
                set_output_region();
                render_output();
                redraw(prompt, (0, 0), &[], 0, &mut scroll);
            }
            Ok(_) => {}
            Err(_) => {
                INPUT_CLOSED.store(true, Ordering::Relaxed);
                break None;
            }
        }
    };
    drop(owner);
    let shown = match answer {
        Some('\n') => String::new(),
        Some(c) => c.to_string(),
        None => dim("cancelled"),
    };
    if alt {
        echo_submitted(prompt, &shown);
    } else {
        print!("{shown}\r\n");
        flush();
    }
    answer
}

/// Puts `text` in the input box of the next prompt, the cursor at its end,
/// line breaks kept (/rewind hands back the prompt it went back to).
pub fn prefill_composer(text: &str) {
    let buf: Vec<char> = clean_paste(text).chars().collect();
    let draft = TaskDraft {
        lines: Vec::new(),
        cursor: buf.len(),
        buf,
    };
    screen::with(|s| s.composer.draft = Some(draft));
}

/// Sends `text` as the next input, as if typed and submitted: a `/command`
/// typed at a key question runs once the question has closed.
pub fn queue_message(text: &str) {
    screen::with(|s| s.composer.queue.push(text.to_string()));
}

// ── secret ask ───────────────────────────────────────────────────────────────
/// Reads an API key without echoing it: each character shows as a dot while
/// typing, and the submitted line keeps only the masked form, so the key
/// never reaches the screen or the scrollback. Esc, Ctrl+C, and Ctrl+D on an
/// empty line return None (cancel). Without a terminal it reads one line.
pub fn ask_secret(prompt: &str) -> Option<String> {
    if PROTOCOL_STDIN.load(Ordering::Relaxed) {
        return None;
    }
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
        screen::with(|s| s.transcript.scroll_offset = 0);
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
            Event::Paste(s) => buf.extend(sanitize_paste(&s)),
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

pub fn ask_task(prompt: &str) -> Option<InputEvent> {
    let prompt = &*sanitize_prompt(prompt);
    // Take the message out first: echo_submitted → line → render_output
    // takes the screen lock again, and std Mutex is not reentrant.
    let queued = screen::with(|s| {
        let mq = &mut s.composer.queue;
        (!mq.is_empty()).then(|| mq.remove(0))
    });
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
    let mut draft = screen::with(|s| s.composer.draft.take()).unwrap_or_else(|| {
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
                screen::with(|s| s.composer.draft = Some(draft));
                return Some(InputEvent::CycleMode);
            }
            // A continued message is not left half-sent for /rewind.
            RawLine::Rewind if !draft.lines.is_empty() => {}
            RawLine::Rewind => return Some(InputEvent::Rewind),
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
    let changed = with_history(|h| {
        let new = h.last().map(String::as_str) != Some(s);
        if new {
            h.push(s.to_string());
        }
        new.then(|| h.clone())
    });
    // Written with the lock released: a slow disk must not hold up a frame.
    if let Some(h) = changed {
        crate::config::save_history(&h);
    }
}

// Run `f` on the message history, reading it from disk on first use. The
// read happens outside the screen lock; `f` runs under it and must not call
// back into the screen.
fn with_history<R>(f: impl FnOnce(&mut Vec<String>) -> R) -> R {
    if !screen::with(|s| s.composer.history.is_some()) {
        let loaded = crate::config::load_history();
        screen::with(|s| {
            s.composer.history.get_or_insert(loaded);
        });
    }
    screen::with(|s| f(s.composer.history.get_or_insert_with(Vec::new)))
}

mod editor;
#[cfg(test)]
pub(crate) use editor::builtin_slash_commands;
#[cfg(test)]
pub(crate) use editor::completions_for;
pub(crate) use editor::custom_command_desc;
use editor::*;
pub use editor::{find_command, SlashCommand, COMMANDS};

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
        let frames = SPINNER_FRAMES;
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
                    accent(frames[i % frames.len()]),
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
                print!("\r{} {}", accent(frames[i % frames.len()]), dim(&label));
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
mod tests;

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
