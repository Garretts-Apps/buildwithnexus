// The value that owns the screen.
//
// The TUI's mutable state lives in one `Screen`, behind one process-wide
// mutex (`SCREEN`). The free functions in `tui.rs` stay the public surface:
// each locks through `with`, reads or edits a field group, and unlocks before
// it writes to the terminal.
//
// Lock order: KEYBOARD → RENDER → SCREEN → IMAGES (`images.rs`).
// SCREEN is a leaf for everything but the image store: `with` is never
// entered while it is already held on the same thread (debug builds panic
// instead of deadlocking), and no terminal write, render function, file read
// or `tui::` function that takes SCREEN runs inside its closure. Nothing the
// panic hook or a signal handler calls may take SCREEN either. Code that used
// to hold the typeahead or queue lock across a repaint now takes a snapshot
// and paints from that; frames are still serialized by RENDER.
// `transcript.painted` (what the terminal shows) is written while a frame is
// built and is only right because every frame is built and written under
// RENDER, in that order; anything else that draws over the output rows calls
// `forget_painted` (via `tui::invalidate_painted`).
//
// Kept outside on purpose:
//   RAW, ALT_SCREEN, MOUSE_CAPTURED,
//   LAST_REGION_BOTTOM                read or written by the signal handler,
//                                     the console control handler and the
//                                     panic hook, which can run on a thread
//                                     that holds SCREEN: lock-free atomics.
//   INTERRUPT_KIND_VAL, AGENT_RUNNING,
//   PROTOCOL_STDIN, INPUT_CLOSED      polled from worker threads on every
//                                     streamed chunk; a mutex there would
//                                     make `interrupted()` wait on a repaint.
//   STREAM_CHARS                      bumped per chunk by helper threads that
//                                     capture their output and otherwise
//                                     never touch the screen.
//   THEME, LINE_MODE (theme.rs)       read by every colour helper, including
//                                     while SCREEN is held to build a frame.
//   KEYBOARD, RENDER and their
//   per-thread *_HELD flags, CAPTURE  the locks themselves and thread-locals.
//   IMAGES (images.rs)                the decoded inline images. Wrapping a
//                                     transcript line (under SCREEN) sizes
//                                     its image, and a decode can take a
//                                     while, so they keep their own lock,
//                                     below SCREEN. Their bookkeeping is
//                                     `Screen.images`.
//   Once/OnceLock values and the
//   editor's completion caches        write-once setup and pure caches.

use super::{OpenInput, Placement, Selection, TaskDraft, Transcript, TypeAheadState};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Footer and status line: what the bottom row shows.
pub(super) struct Status {
    /// Permission-mode text (`set_permission_mode`).
    pub footer: String,
    /// Model name shown before it.
    pub model: String,
    /// Transient message and the monotonic time it expires.
    pub flash: String,
    pub flash_until_ms: u64,
    /// Context meter.
    pub context_used: usize,
    pub context_total: usize,
    /// When the current turn started, when the elapsed-time tick last
    /// repainted, and when a "done" notification last went out.
    pub work_started_ms: u64,
    pub last_status_tick_ms: u64,
    pub last_notify_ms: u64,
}

impl Status {
    pub fn set_flash(&mut self, msg: &str, now_ms: u64) {
        self.flash = msg.to_string();
        self.flash_until_ms = now_ms + 2_000;
    }

    pub fn active_flash(&self, now_ms: u64) -> Option<String> {
        if now_ms >= self.flash_until_ms || self.flash.is_empty() {
            return None;
        }
        Some(self.flash.clone())
    }
}

/// Settings and modes that are not tied to one part of the screen.
pub(super) struct Flags {
    /// Terminal focus (CSI ?1004 reports); assumed focused until told
    /// otherwise, so a terminal that never reports never gets a notification.
    pub focused: bool,
    /// `notify` setting: 0 off, 1 auto (only while unfocused), 2 always.
    pub notify_mode: u8,
    /// `images` setting: 0 off, 1 auto, 2 kitty, 3 blocks, 4 sixel.
    pub images_mode: u8,
    pub vim_mode: bool,
    /// 0 = Normal, 1 = Insert, 2 = Visual.
    pub vim_state: u8,
}

/// Lines the transcript keeps; older ones are dropped from the front.
pub(super) const MAX_LINES: usize = 2_000;

const NO_STREAM_LINE: usize = usize::MAX;

/// The output region: the transcript, where the view is scrolled to, and
/// what the last frame put on screen.
pub(super) struct TranscriptState {
    pub text: Transcript,
    /// Rows scrolled back from the bottom; 0 follows the newest output.
    pub scroll_offset: usize,
    // Transcript index of the line currently receiving streamed text, or
    // NO_STREAM_LINE. A plain line (and a clear) closes it, so interleaved
    // notices (trace records, hook lines) never get streamed text welded on.
    open_stream_line: usize,
    /// Plain text of the rows the last frame painted (selection, copy).
    pub visible_rows: Vec<String>,
    /// What is on the terminal: the bytes the output rows were last painted
    /// with, one entry per row, so a frame emits only the rows that differ
    /// (`tui::build_output_frame`). Empty = not known: the next frame
    /// paints every row.
    pub painted: Vec<String>,
    /// The composer's box borders and the footer as last painted
    /// (`tui::queue_chrome`), and the rest of the composer frame
    /// (`tui::render_queued_composer`): not written again while unchanged.
    /// Empty = not known.
    pub painted_borders: Vec<u8>,
    pub painted_footer: Vec<u8>,
    pub painted_composer: Vec<u8>,
}

impl TranscriptState {
    /// Something other than an output frame drew on the screen, or cleared
    /// it: the next frame repaints every row.
    pub fn forget_painted(&mut self) {
        self.painted.clear();
        self.painted_borders.clear();
        self.painted_footer.clear();
        self.painted_composer.clear();
    }

    pub fn invalidate_stream_line(&mut self) {
        self.open_stream_line = NO_STREAM_LINE;
    }

    fn rows_if_scrolled(&self) -> usize {
        if self.scroll_offset > 0 {
            self.text.total_rows()
        } else {
            0
        }
    }

    // Keep a scrolled-back view pinned to what the reader is looking at: rows
    // appended at the bottom push the bottom-relative offset up by the same
    // amount. (Rows dropped from the front don't move the view — the offset
    // is measured from the end.)
    fn pin_scroll(&mut self, before_rows: usize) {
        if self.scroll_offset > 0 {
            let added = self.text.total_rows().saturating_sub(before_rows);
            self.scroll_offset += added;
        }
    }

    // Drop lines over MAX_LINES from the front, keeping the open-line index
    // in step with the drained prefix.
    fn trim(&mut self) {
        if self.text.len() <= MAX_LINES {
            return;
        }
        let extra = self.text.len() - MAX_LINES;
        self.text.drain_front(extra);
        if self.open_stream_line != NO_STREAM_LINE {
            if self.open_stream_line >= extra {
                self.open_stream_line -= extra;
            } else {
                self.invalidate_stream_line();
            }
        }
    }

    /// Append `s` as finished lines; closes the open stream line.
    pub fn push_lines(&mut self, s: &str) {
        self.invalidate_stream_line();
        let before = self.rows_if_scrolled();
        for part in s.replace('\r', "").split('\n') {
            self.text.push(part.to_string());
        }
        self.pin_scroll(before);
        self.trim();
    }

    /// Append a streamed chunk to the open stream line, opening one if none
    /// is (start of stream, or a plain line intervened).
    pub fn stream(&mut self, chunk: &str) {
        let before = self.rows_if_scrolled();
        let normalized = chunk.replace('\r', "");
        let mut parts = normalized.split('\n');
        if let Some(first) = parts.next() {
            if !self.text.append_to(self.open_stream_line, first) {
                self.text.push(first.to_string());
                self.open_stream_line = self.text.len() - 1;
            }
        }
        for part in parts {
            self.text.push(part.to_string());
            self.open_stream_line = self.text.len() - 1;
        }
        self.pin_scroll(before);
        self.trim();
    }

    /// Replace the open stream line with its rendered form and close it.
    /// False when no stream line was open.
    pub fn commit_stream(&mut self, rendered: &str) -> bool {
        let replaced = self.open_stream_line < self.text.len();
        if replaced {
            self.text.set(self.open_stream_line, rendered.to_string());
        }
        self.invalidate_stream_line();
        replaced
    }

    /// Remove the open stream line, if any, and close it.
    pub fn retract_stream(&mut self) {
        self.text.remove(self.open_stream_line);
        self.invalidate_stream_line();
    }
}

/// The input box: what is typed, queued and remembered.
pub(super) struct Composer {
    /// Keys typed while the agent works; they pre-fill the next prompt.
    pub typeahead: TypeAheadState,
    /// Messages queued to send, oldest first.
    pub queue: Vec<String>,
    /// Open prompts and pickers, newest last; the newest owns the box.
    pub open_inputs: Vec<OpenInput>,
    /// Every collapsed paste of the session, by its token: a history recall
    /// of a message with a token still sends the paste.
    pub pastes: Vec<(String, String)>,
    /// A half-written message kept across a mode change or /rewind.
    pub draft: Option<TaskDraft>,
    /// Submitted messages; None until first read from disk (`with_history`).
    pub history: Option<Vec<String>>,
}

impl Composer {
    pub fn take_typeahead(&mut self) -> (Vec<char>, usize) {
        let ta = &mut self.typeahead;
        ta.from_queue = false;
        (
            std::mem::take(&mut ta.buf),
            std::mem::replace(&mut ta.cursor, 0),
        )
    }
}

/// Bookkeeping for inline images. The decoded images themselves are in
/// `images.rs`, under their own lock.
pub(super) struct ImageBook {
    /// Set when something other than render_output may have drawn over the
    /// output rows (a popup, a clear, a resize): the next frame redraws
    /// every Sixel image instead of keeping the ones that did not move.
    pub dirty: bool,
    /// Sixel images drawn by the last frame.
    pub drawn: Vec<Placement>,
    /// Paths already previewed this session.
    pub previewed: HashSet<PathBuf>,
    /// Next kitty image id; whether any image was uploaded (so leaving the
    /// screen frees them).
    pub next_id: u32,
    pub sent: bool,
}

/// Mouse selection over the output rows.
pub(super) struct SelectionState {
    pub current: Option<Selection>,
    /// Multi-click detection: (last click ms, row, col, count).
    last_click: (u64, u16, u16, u8),
}

impl SelectionState {
    /// Which click this is: two on the same cell within 400ms select the
    /// word, three the whole line.
    pub fn click_count(&mut self, now_ms: u64, row: u16, col: u16) -> u8 {
        let (t, r, c, n) = self.last_click;
        let count = if now_ms.saturating_sub(t) <= 400 && r == row && c == col {
            (n % 3) + 1
        } else {
            1
        };
        self.last_click = (now_ms, row, col, count);
        count
    }
}

pub(super) struct Screen {
    pub transcript: TranscriptState,
    pub composer: Composer,
    pub images: ImageBook,
    pub selection: SelectionState,
    pub status: Status,
    pub flags: Flags,
}

impl Screen {
    pub(super) fn new() -> Self {
        Screen {
            transcript: TranscriptState {
                text: Transcript::new(),
                scroll_offset: 0,
                open_stream_line: NO_STREAM_LINE,
                visible_rows: Vec::new(),
                painted: Vec::new(),
                painted_borders: Vec::new(),
                painted_footer: Vec::new(),
                painted_composer: Vec::new(),
            },
            composer: Composer {
                typeahead: TypeAheadState {
                    buf: Vec::new(),
                    cursor: 0,
                    from_queue: false,
                },
                queue: Vec::new(),
                open_inputs: Vec::new(),
                pastes: Vec::new(),
                draft: None,
                history: None,
            },
            images: ImageBook {
                dirty: true,
                drawn: Vec::new(),
                previewed: HashSet::new(),
                next_id: 1,
                sent: false,
            },
            selection: SelectionState {
                current: None,
                last_click: (0, 0, 0, 0),
            },
            status: Status {
                footer: String::new(),
                model: String::new(),
                flash: String::new(),
                flash_until_ms: 0,
                context_used: 0,
                context_total: 0,
                work_started_ms: 0,
                last_status_tick_ms: 0,
                last_notify_ms: 0,
            },
            flags: Flags {
                focused: true,
                notify_mode: 1,
                images_mode: 1,
                vim_mode: false,
                vim_state: 0,
            },
        }
    }
}

static SCREEN: OnceLock<Mutex<Screen>> = OnceLock::new();

#[cfg(debug_assertions)]
thread_local! {
    static SCREEN_HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct Held<'a>(MutexGuard<'a, Screen>);

impl Drop for Held<'_> {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        let _ = SCREEN_HELD.try_with(|h| h.set(false));
    }
}

fn lock() -> Held<'static> {
    #[cfg(debug_assertions)]
    {
        let again = SCREEN_HELD.try_with(|h| h.get()).unwrap_or(false);
        assert!(!again, "tui::screen::with re-entered on one thread");
        let _ = SCREEN_HELD.try_with(|h| h.set(true));
    }
    // A panic while the lock is held (tests only: release aborts) leaves the
    // state usable, as each of the old per-field locks did for the others.
    Held(
        SCREEN
            .get_or_init(|| Mutex::new(Screen::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    )
}

/// Run `f` on the screen state. Keep it short: see the lock order above.
pub(super) fn with<R>(f: impl FnOnce(&mut Screen) -> R) -> R {
    let mut held = lock();
    f(&mut held.0)
}
