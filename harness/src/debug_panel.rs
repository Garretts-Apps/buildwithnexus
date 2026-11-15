//! Expandable debug panel for the TUI.
//!
//! Hidden is the default. While it is hidden the frame path does not allocate,
//! does not read `/proc`, and does not record. The only added cost on a frame
//! is [`FrameWatch::start`] loading one atomic and returning. Recording starts
//! the first time the panel is opened (`F12`, `/debug`, `BWN_DEBUG=1`) and
//! stays on so a later reopen still has the turn. Per-frame samples are taken
//! only while the panel is on screen.
//!
//! `F12` cycles hidden → strip → panel → fullscreen → hidden.
//! `F8` breaks before the next model request or tool call, `F10` steps one
//! checkpoint, `F5` continues. Up/Down, Left/Right and Enter navigate sections
//! while the panel or fullscreen view is up. Esc drops back to the strip.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::provider::Msg;

const TOOLS_CAP: usize = 64;
const FILES_CAP: usize = 64;
const PERMS_CAP: usize = 48;
const HOOKS_CAP: usize = 48;
const EVENTS_CAP: usize = 128;
const FRAMES_CAP: usize = 48;
const PROMPT_CAP: usize = 24;
const TEXT_CAP: usize = 4_000;
const OUT_CAP: usize = 8_000;
const SECTION_COUNT: usize = 8;

static ARMED: AtomicBool = AtomicBool::new(false);
static MODE_BITS: AtomicU64 = AtomicU64::new(0);
static GEN: AtomicU64 = AtomicU64::new(1);
static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static FRAME_BASE: AtomicU64 = AtomicU64::new(0);
static INPUT_AT: AtomicU64 = AtomicU64::new(0);

struct CountAlloc;

unsafe impl std::alloc::GlobalAlloc for CountAlloc {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        let ptr = std::alloc::System.alloc(layout);
        note_alloc();
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        let ptr = std::alloc::System.alloc_zeroed(layout);
        note_alloc();
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        std::alloc::System.dealloc(ptr, layout);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        let p = std::alloc::System.realloc(ptr, layout, new_size);
        note_alloc();
        p
    }
}

#[global_allocator]
static ALLOC: CountAlloc = CountAlloc;

#[inline]
fn note_alloc() {
    if COUNTING.load(Ordering::Relaxed) {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Hidden = 0,
    Strip = 1,
    Panel = 2,
    Full = 3,
}

impl Mode {
    fn from_bits(v: u64) -> Self {
        match v {
            1 => Mode::Strip,
            2 => Mode::Panel,
            3 => Mode::Full,
            _ => Mode::Hidden,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Thinking,
    Streaming,
    Tool,
    Permission,
    Paused,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Thinking => "thinking",
            Phase::Streaming => "streaming",
            Phase::Tool => "tool call",
            Phase::Permission => "waiting for permission",
            Phase::Paused => "paused",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugKey {
    F12,
    F8,
    F10,
    F5,
    Up,
    Down,
    Left,
    Right,
    Enter,
    Esc,
}

#[derive(Clone, Debug)]
struct ToolRec {
    name: String,
    args: String,
    ms: u64,
    ok: bool,
    code: Option<i32>,
    output: String,
    cwd: String,
    env_removed: String,
    stdout: String,
    stderr: String,
    open: bool,
}

#[derive(Clone, Debug)]
struct FileRec {
    path: String,
    op: String,
    diff: String,
}

#[derive(Clone, Debug)]
struct PermRec {
    tool: String,
    decision: String,
    detail: String,
}

#[derive(Clone, Debug)]
struct HookRec {
    event: String,
    command: String,
}

#[derive(Clone, Debug)]
struct EventRec {
    title: String,
}

#[derive(Clone, Debug)]
struct PromptLine {
    role: String,
    text: String,
}

#[derive(Clone, Debug)]
pub struct View {
    phase: Phase,
    turns: u32,
    plan: Vec<String>,
    protocol: String,
    model: String,
    context_tokens: usize,
    inflight: bool,
    req_at: Option<Instant>,
    ttft_ms: Option<u64>,
    latency_ms: Option<u64>,
    last_in: u64,
    last_out: u64,
    retries: u32,
    prompt: Vec<PromptLine>,
    tools: Vec<ToolRec>,
    files: Vec<FileRec>,
    perms: Vec<PermRec>,
    hooks: Vec<HookRec>,
    events: Vec<EventRec>,
    frames_us: Vec<u64>,
    last_allocs: u64,
    last_input_us: u64,
    cpu_pct: f64,
    rss_bytes: u64,
    rss_label: &'static str,
    open: [bool; SECTION_COUNT],
    focus: usize,
    tool_cursor: usize,
    file_cursor: usize,
    scroll: usize,
}

impl Default for View {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            turns: 0,
            plan: Vec::new(),
            protocol: String::new(),
            model: String::new(),
            context_tokens: 0,
            inflight: false,
            req_at: None,
            ttft_ms: None,
            latency_ms: None,
            last_in: 0,
            last_out: 0,
            retries: 0,
            prompt: Vec::new(),
            tools: Vec::new(),
            files: Vec::new(),
            perms: Vec::new(),
            hooks: Vec::new(),
            events: Vec::new(),
            frames_us: Vec::new(),
            last_allocs: 0,
            last_input_us: 0,
            cpu_pct: 0.0,
            rss_bytes: 0,
            rss_label: "rss",
            // Agent, model, tools and perf start open. The rest stay collapsed
            // so an 80-column panel is a summary, not a wall of text.
            open: [true, true, true, false, false, false, false, true],
            focus: 0,
            tool_cursor: 0,
            file_cursor: 0,
            scroll: 0,
        }
    }
}

struct Store {
    view: View,
    cpu_prev: Option<(Instant, Duration)>,
}

struct Brake {
    hold: bool,
    step: bool,
}

struct Painted {
    gen: u64,
    width: u16,
    rows: u16,
    mode: u8,
    bytes: Vec<u8>,
}

fn store() -> MutexGuard<'static, Store> {
    static STORE: std::sync::OnceLock<Mutex<Store>> = std::sync::OnceLock::new();
    STORE
        .get_or_init(|| {
            Mutex::new(Store {
                view: View::default(),
                cpu_prev: None,
            })
        })
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn brake() -> (&'static Mutex<Brake>, &'static Condvar) {
    static BRAKE: Mutex<Brake> = Mutex::new(Brake {
        hold: false,
        step: false,
    });
    static CV: Condvar = Condvar::new();
    (&BRAKE, &CV)
}

fn painted() -> MutexGuard<'static, Painted> {
    static PAINTED: Mutex<Painted> = Mutex::new(Painted {
        gen: 0,
        width: 0,
        rows: 0,
        mode: 0,
        bytes: Vec::new(),
    });
    PAINTED.lock().unwrap_or_else(|e| e.into_inner())
}

fn bump() {
    GEN.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn armed() -> bool {
    ARMED.load(Ordering::Relaxed)
}

#[inline]
pub fn mode() -> Mode {
    Mode::from_bits(MODE_BITS.load(Ordering::Relaxed))
}

#[inline]
pub fn visible() -> bool {
    mode() != Mode::Hidden
}

pub fn cycle(mode: Mode) -> Mode {
    match mode {
        Mode::Hidden => Mode::Strip,
        Mode::Strip => Mode::Panel,
        Mode::Panel => Mode::Full,
        Mode::Full => Mode::Hidden,
    }
}

fn set_mode(next: Mode) {
    MODE_BITS.store(next as u64, Ordering::Relaxed);
    COUNTING.store(next != Mode::Hidden, Ordering::Relaxed);
    painted().gen = 0;
    bump();
}

pub fn init() {
    if std::env::var_os("BWN_DEBUG").is_some_and(|v| v == "1") {
        ARMED.store(true, Ordering::Relaxed);
        set_mode(Mode::Strip);
    }
}

/// One frame. When the panel is hidden this is a single atomic load and a
/// `None`: no allocation, no counter increment, no sample.
pub struct FrameWatch {
    started: Option<Instant>,
}

impl FrameWatch {
    pub fn start() -> Self {
        if !visible() {
            return Self { started: None };
        }
        FRAME_BASE.store(ALLOCS.load(Ordering::Relaxed), Ordering::Relaxed);
        Self {
            started: Some(Instant::now()),
        }
    }
}

impl Drop for FrameWatch {
    fn drop(&mut self) {
        let Some(t0) = self.started.take() else {
            return;
        };
        if !visible() {
            return;
        }
        let us = t0.elapsed().as_micros() as u64;
        let allocs = ALLOCS
            .load(Ordering::Relaxed)
            .saturating_sub(FRAME_BASE.load(Ordering::Relaxed));
        let mut g = store();
        g.view.last_allocs = allocs;
        push_cap_num(&mut g.view.frames_us, FRAMES_CAP, us);
        bump();
    }
}

/// Stamp the moment a key arrived, so the next paint can record input-to-paint.
pub fn mark_input() {
    if !visible() {
        return;
    }
    INPUT_AT.store(mono_us(), Ordering::Relaxed);
}

pub fn mark_paint() {
    if !visible() {
        return;
    }
    let t = INPUT_AT.swap(0, Ordering::Relaxed);
    if t == 0 {
        return;
    }
    let dt = mono_us().saturating_sub(t);
    let mut g = store();
    g.view.last_input_us = dt;
    bump();
}

fn mono_us() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

pub fn on_key(key: DebugKey) -> bool {
    match key {
        DebugKey::F12 => {
            ARMED.store(true, Ordering::Relaxed);
            let next = cycle(mode());
            let need_label = next != Mode::Hidden && store().view.model.is_empty();
            if need_label {
                let label = crate::tui::model_label();
                store().view.model = label;
            }
            set_mode(next);
            true
        }
        DebugKey::F8 => {
            ARMED.store(true, Ordering::Relaxed);
            let (lock, cv) = brake();
            lock.lock().unwrap_or_else(|e| e.into_inner()).hold = true;
            cv.notify_all();
            let mut g = store();
            g.view.phase = Phase::Paused;
            push_event(&mut g.view, "break armed");
            bump();
            true
        }
        DebugKey::F10 => {
            if !armed() {
                return false;
            }
            let (lock, cv) = brake();
            let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
            g.hold = true;
            g.step = true;
            cv.notify_all();
            true
        }
        DebugKey::F5 => {
            if !armed() {
                return false;
            }
            let (lock, cv) = brake();
            let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
            g.hold = false;
            g.step = false;
            cv.notify_all();
            let mut view = store();
            if view.view.phase == Phase::Paused {
                view.view.phase = Phase::Idle;
            }
            push_event(&mut view.view, "continue");
            bump();
            true
        }
        DebugKey::Esc => {
            if matches!(mode(), Mode::Panel | Mode::Full) {
                set_mode(Mode::Strip);
                true
            } else {
                false
            }
        }
        DebugKey::Up | DebugKey::Down | DebugKey::Left | DebugKey::Right | DebugKey::Enter => {
            if !matches!(mode(), Mode::Panel | Mode::Full) {
                return false;
            }
            let open = {
                let mut g = store();
                apply_nav(&mut g.view, key);
                let path = if key == DebugKey::Enter && g.view.focus == 3 && g.view.open[3] {
                    g.view.files.get(g.view.file_cursor).map(|f| f.path.clone())
                } else {
                    None
                };
                bump();
                path
            };
            if let Some(path) = open {
                crate::workspace::open_diff(&path);
            }
            true
        }
    }
}

fn apply_nav(view: &mut View, key: DebugKey) {
    match key {
        DebugKey::Up => {
            if view.focus == 2 && view.open[2] && view.tool_cursor > 0 {
                view.tool_cursor -= 1;
            } else if view.focus == 3 && view.open[3] && view.file_cursor > 0 {
                view.file_cursor -= 1;
            } else {
                view.focus = view.focus.saturating_sub(1);
            }
        }
        DebugKey::Down => {
            if view.focus == 2 && view.open[2] && view.tool_cursor + 1 < view.tools.len() {
                view.tool_cursor += 1;
            } else if view.focus == 3 && view.open[3] && view.file_cursor + 1 < view.files.len() {
                view.file_cursor += 1;
            } else if view.focus + 1 < SECTION_COUNT {
                view.focus += 1;
            }
        }
        DebugKey::Left => view.open[view.focus] = false,
        DebugKey::Right => view.open[view.focus] = true,
        DebugKey::Enter => {
            if view.focus == 2 && view.open[2] {
                if let Some(t) = view.tools.get_mut(view.tool_cursor) {
                    t.open = !t.open;
                }
            } else if view.focus == 3 && view.open[3] {
                // The caller opens this path in the diff pane after the lock drops.
            } else {
                view.open[view.focus] = !view.open[view.focus];
            }
        }
        _ => {}
    }
}

/// Block before a model request or a tool call when a break is armed.
/// Returns immediately when the debugger has never been opened.
pub fn checkpoint(what: &str) {
    if !armed() {
        return;
    }
    {
        let mut g = store();
        push_event(&mut g.view, what);
        bump();
    }
    let (lock, cv) = brake();
    let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
    if !g.hold {
        return;
    }
    drop(g);
    {
        let mut view = store();
        view.view.phase = Phase::Paused;
        bump();
    }
    g = lock.lock().unwrap_or_else(|e| e.into_inner());
    while g.hold && !g.step {
        g = cv.wait(g).unwrap_or_else(|e| e.into_inner());
    }
    if g.step {
        g.step = false;
    }
}

pub fn begin_turn() {
    if !armed() {
        return;
    }
    let mut g = store();
    g.view.turns = g.view.turns.saturating_add(1);
    g.view.phase = Phase::Thinking;
    push_event(&mut g.view, "turn");
    bump();
}

pub fn set_plan(steps: &[String]) {
    if !armed() || steps.is_empty() {
        return;
    }
    let mut g = store();
    g.view.plan = steps.iter().take(12).map(|s| clip(s, 160)).collect();
    push_event(&mut g.view, "plan");
    bump();
}

pub fn note_session(protocol: &str, model: &str, context: usize) {
    let mut g = store();
    if !protocol.is_empty() {
        g.view.protocol = protocol.to_string();
    }
    if !model.is_empty() {
        g.view.model = model.to_string();
    }
    if context > 0 {
        g.view.context_tokens = context;
    }
    bump();
}

pub fn model_start(protocol: &str, model: &str, context: usize, msgs: &[Msg]) -> Option<Instant> {
    if !armed() {
        return None;
    }
    checkpoint("model request");
    let now = Instant::now();
    let mut g = store();
    g.view.protocol = protocol.to_string();
    g.view.model = model.to_string();
    if context > 0 {
        g.view.context_tokens = context;
    }
    g.view.inflight = true;
    g.view.req_at = Some(now);
    g.view.ttft_ms = None;
    g.view.phase = Phase::Thinking;
    g.view.prompt.clear();
    for m in msgs
        .iter()
        .rev()
        .take(PROMPT_CAP)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let (role, text) = msg_preview(m);
        g.view.prompt.push(PromptLine {
            role: role.to_string(),
            text: clip(text, TEXT_CAP),
        });
    }
    push_event(&mut g.view, "model request");
    bump();
    append_log(&json!({
        "kind": "prompt",
        "protocol": protocol,
        "model": model,
        "messages": g.view.prompt.iter().map(|p| json!({"role": p.role, "text": p.text})).collect::<Vec<_>>(),
    }));
    Some(now)
}

pub fn model_token() {
    if !armed() {
        return;
    }
    let mut g = store();
    if g.view.ttft_ms.is_none() {
        if let Some(t0) = g.view.req_at {
            g.view.ttft_ms = Some(t0.elapsed().as_millis() as u64);
        }
    }
    if g.view.phase == Phase::Thinking {
        g.view.phase = Phase::Streaming;
        bump();
    }
}

pub fn model_end(watch: Option<Instant>, input: u64, output: u64) {
    let Some(t0) = watch else {
        return;
    };
    let mut g = store();
    g.view.inflight = false;
    g.view.req_at = None;
    g.view.latency_ms = Some(t0.elapsed().as_millis() as u64);
    g.view.last_in = input;
    g.view.last_out = output;
    if g.view.phase != Phase::Paused {
        g.view.phase = Phase::Idle;
    }
    push_event(&mut g.view, "model done");
    bump();
}

pub fn note_retry() {
    if !armed() {
        return;
    }
    let mut g = store();
    g.view.retries = g.view.retries.saturating_add(1);
    push_event(&mut g.view, "retry");
    bump();
}

pub struct ToolWatch {
    t: Instant,
}

pub fn tool_start(name: &str, input: &Value, cwd: &Path) -> Option<ToolWatch> {
    if !armed() {
        return None;
    }
    checkpoint(&format!("tool {name}"));
    {
        let mut g = store();
        g.view.phase = Phase::Tool;
        bump();
    }
    let _ = (input, cwd);
    Some(ToolWatch { t: Instant::now() })
}

pub fn tool_end(
    watch: Option<ToolWatch>,
    name: &str,
    input: &Value,
    cwd: &Path,
    output: &str,
    is_error: bool,
) {
    let Some(watch) = watch else {
        return;
    };
    let ms = watch.t.elapsed().as_millis() as u64;
    let args = clip(&serde_json::to_string(input).unwrap_or_default(), 240);
    let shell = matches!(name, "bash" | "run_command" | "start_server");
    let (stdout, stderr, code) = if shell {
        split_shell(output)
    } else {
        (String::new(), String::new(), None)
    };
    let env_removed = if shell {
        crate::sandbox::credentials_to_unset(std::env::vars().map(|(k, _)| k)).join(",")
    } else {
        String::new()
    };
    let mut g = store();
    push_cap(
        &mut g.view.tools,
        TOOLS_CAP,
        ToolRec {
            name: name.to_string(),
            args,
            ms,
            ok: !is_error,
            code,
            output: clip(output, OUT_CAP),
            cwd: cwd.display().to_string(),
            env_removed,
            stdout,
            stderr,
            open: false,
        },
    );
    if let Some(path) = crate::tools::read_tracking_path(name, input, cwd) {
        push_cap(
            &mut g.view.files,
            FILES_CAP,
            FileRec {
                path: path.display().to_string(),
                op: "read".into(),
                diff: format!("{} bytes", output.len()),
            },
        );
    }
    if let Some(path) = crate::tools::edit_tracking_path(name, input, cwd) {
        push_cap(
            &mut g.view.files,
            FILES_CAP,
            FileRec {
                path: path.display().to_string(),
                op: "write".into(),
                diff: edit_diff(input),
            },
        );
    }
    push_event(&mut g.view, &format!("tool {name}"));
    if g.view.phase != Phase::Paused {
        g.view.phase = Phase::Idle;
    }
    bump();
    let rec = g.view.tools.last();
    if let Some(rec) = rec {
        append_log(&json!({
            "kind": "tool",
            "name": rec.name,
            "args": rec.args,
            "ms": rec.ms,
            "ok": rec.ok,
            "code": rec.code,
            "cwd": rec.cwd,
            "env_removed": rec.env_removed,
            "stdout": rec.stdout,
            "stderr": rec.stderr,
            "output": rec.output,
        }));
    }
}

pub fn permission(tool: &str, denied: Option<&str>) {
    if !armed() {
        return;
    }
    let mut g = store();
    let (decision, detail) = match denied {
        None => ("allow", String::new()),
        Some(r) => ("deny", clip(r, 240)),
    };
    push_cap(
        &mut g.view.perms,
        PERMS_CAP,
        PermRec {
            tool: tool.to_string(),
            decision: decision.into(),
            detail,
        },
    );
    push_event(&mut g.view, &format!("permission {decision} {tool}"));
    bump();
}

pub fn hook_fired(event: &str, command: &str) {
    if !armed() {
        return;
    }
    let mut g = store();
    push_cap(
        &mut g.view.hooks,
        HOOKS_CAP,
        HookRec {
            event: event.to_string(),
            command: clip(command, 160),
        },
    );
    push_event(&mut g.view, &format!("hook {event}"));
    bump();
}

pub fn enter_permission(label: &str) -> bool {
    if !armed() {
        return false;
    }
    let mut g = store();
    g.view.phase = Phase::Permission;
    push_event(&mut g.view, &format!("permission prompt {label}"));
    bump();
    true
}

pub fn leave_permission(active: bool) {
    if !active {
        return;
    }
    let mut g = store();
    if g.view.phase == Phase::Permission {
        g.view.phase = Phase::Idle;
        bump();
    }
}

/// Host CPU and RSS. Called from the ticker only while the panel is visible.
pub fn sample_host() {
    if !visible() {
        return;
    }
    let (cpu, rss, label) = host_now();
    let mut g = store();
    let now = Instant::now();
    if let Some((then, prev)) = g.cpu_prev {
        let wall = now.saturating_duration_since(then).as_secs_f64();
        if wall >= 0.1 {
            let used = cpu.saturating_sub(prev).as_secs_f64();
            g.view.cpu_pct = if wall > 0.0 {
                (used / wall) * 100.0
            } else {
                0.0
            };
            g.cpu_prev = Some((now, cpu));
            g.view.rss_bytes = rss;
            g.view.rss_label = label;
            bump();
        }
    } else {
        g.cpu_prev = Some((now, cpu));
        g.view.rss_bytes = rss;
        g.view.rss_label = label;
    }
}

pub fn command(arg: &str) {
    ARMED.store(true, Ordering::Relaxed);
    let arg = arg.trim();
    match arg {
        "" => {
            set_mode(Mode::Panel);
            crate::tui::line(&crate::tui::accent(
                "  debug panel — F12 cycles strip/panel/full, F8 break, F10 step, F5 run, Esc strip",
            ));
        }
        "pause" | "break" => {
            on_key(DebugKey::F8);
            crate::tui::line("  debug: break armed before the next model request or tool call");
        }
        "step" => {
            on_key(DebugKey::F10);
            crate::tui::line("  debug: step");
        }
        "continue" | "run" => {
            on_key(DebugKey::F5);
            crate::tui::line("  debug: continue");
        }
        "export" => match export_to(&default_export_path()) {
            Ok(p) => crate::tui::line(&format!("  debug export {}", p.display())),
            Err(e) => crate::tui::line(&crate::tui::red(&format!("  debug export: {e}"))),
        },
        other if let Some(path) = other.strip_prefix("export ") => {
            match export_to(Path::new(path.trim())) {
                Ok(p) => crate::tui::line(&format!("  debug export {}", p.display())),
                Err(e) => crate::tui::line(&crate::tui::red(&format!("  debug export: {e}"))),
            }
        }
        other if let Some(path) = other.strip_prefix("replay ") => {
            match load_replay(Path::new(path.trim())) {
                Ok(n) => {
                    set_mode(Mode::Full);
                    crate::tui::line(&format!("  debug replay: {n} records"));
                }
                Err(e) => crate::tui::line(&crate::tui::red(&format!("  debug replay: {e}"))),
            }
        }
        other => crate::tui::line(&crate::tui::red(&format!(
            "  unknown /debug {other} — try: export, pause, step, continue, replay <file>"
        ))),
    }
}

pub fn cli(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        None | Some("help") | Some("--help") | Some("-h") => {
            println!(
                "buildwithnexus debug export [file.jsonl]   write the session trace\n\
                 buildwithnexus debug replay <file.jsonl>   step through it in the panel\n\
                 \n\
                 Inside a session: F12, or /debug. F8 breaks, F10 steps, F5 continues."
            );
            Ok(())
        }
        Some("export") => {
            let path = args
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(default_export_path);
            let path = export_to(&path)?;
            println!("{}", path.display());
            Ok(())
        }
        Some("replay") => {
            let path = args.get(1).ok_or("debug replay needs a jsonl file")?;
            replay(Path::new(path))
        }
        Some(other) => Err(format!("unknown debug command '{other}'")),
    }
}

fn default_export_path() -> PathBuf {
    PathBuf::from("bwn-debug.jsonl")
}

fn live_log() -> PathBuf {
    crate::config::home().join("debug-last.jsonl")
}

pub fn export_to(path: &Path) -> Result<PathBuf, String> {
    let body = {
        let g = store();
        if armed() && (!g.view.events.is_empty() || !g.view.tools.is_empty()) {
            Some(export_jsonl(&g.view))
        } else {
            None
        }
    };
    let text = if let Some(body) = body {
        body
    } else if live_log().is_file() {
        fs::read_to_string(live_log()).map_err(|e| e.to_string())?
    } else if let Some(src) = newest_trace() {
        fs::read_to_string(src).map_err(|e| e.to_string())?
    } else {
        return Err(
            "no debug trace yet — open the panel with F12 during a session, or pass a jsonl file"
                .into(),
        );
    };
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    fs::write(path, text).map_err(|e| e.to_string())?;
    Ok(path.to_path_buf())
}

fn newest_trace() -> Option<PathBuf> {
    let dir = crate::config::home().join("traces");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let rd = fs::read_dir(&dir).ok()?;
    for ent in rd.flatten() {
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let modified = ent.metadata().and_then(|m| m.modified()).ok()?;
        if best.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, p)| p)
}

fn load_replay(path: &Path) -> Result<usize, String> {
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let view = import_jsonl(&text)?;
    let n = view.events.len() + view.tools.len();
    let mut g = store();
    g.view = view;
    ARMED.store(true, Ordering::Relaxed);
    bump();
    Ok(n)
}

pub fn replay(path: &Path) -> Result<(), String> {
    let n = load_replay(path)?;
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let g = store();
        for line in render_lines(&g.view, Mode::Full, 80, 24) {
            println!("{line}");
        }
        eprintln!("{n} records");
        return Ok(());
    }
    set_mode(Mode::Full);
    crate::tui::enter_alt(true);
    loop {
        crate::tui::paint_debug();
        let ev = match crossterm::event::read() {
            Ok(crossterm::event::Event::Key(k))
                if k.kind == crossterm::event::KeyEventKind::Press =>
            {
                k
            }
            Ok(_) => continue,
            Err(_) => break,
        };
        if ev.code == crossterm::event::KeyCode::Char('q') {
            break;
        }
        if let Some(key) = DebugKey::from_code(ev.code) {
            if !on_key(key) && key == DebugKey::Esc && mode() == Mode::Strip {
                break;
            }
        }
    }
    crate::tui::leave_alt();
    Ok(())
}

impl DebugKey {
    pub fn from_code(code: crossterm::event::KeyCode) -> Option<Self> {
        match code {
            crossterm::event::KeyCode::F(12) => Some(DebugKey::F12),
            crossterm::event::KeyCode::F(8) => Some(DebugKey::F8),
            crossterm::event::KeyCode::F(10) => Some(DebugKey::F10),
            crossterm::event::KeyCode::F(5) => Some(DebugKey::F5),
            crossterm::event::KeyCode::Up => Some(DebugKey::Up),
            crossterm::event::KeyCode::Down => Some(DebugKey::Down),
            crossterm::event::KeyCode::Left => Some(DebugKey::Left),
            crossterm::event::KeyCode::Right => Some(DebugKey::Right),
            crossterm::event::KeyCode::Enter => Some(DebugKey::Enter),
            crossterm::event::KeyCode::Esc => Some(DebugKey::Esc),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub row: u16,
    pub col: u16,
    pub text: String,
}

pub fn render_lines(view: &View, mode: Mode, width: u16, rows: u16) -> Vec<String> {
    layout(view, mode, width, rows)
        .into_iter()
        .map(|p| p.text)
        .collect()
}

pub fn layout(view: &View, mode: Mode, width: u16, rows: u16) -> Vec<Placement> {
    if mode == Mode::Hidden || width == 0 || rows == 0 {
        return Vec::new();
    }
    let width = width as usize;
    let rows = rows as usize;
    match mode {
        Mode::Hidden => Vec::new(),
        Mode::Strip => {
            let text = fit(&strip_line(view), width);
            vec![Placement {
                row: (rows - 1) as u16,
                col: 0,
                text,
            }]
        }
        Mode::Panel if width >= 120 => place_side(view, width, rows),
        Mode::Panel => place_bottom(view, width, rows),
        Mode::Full => place_block(view, width, rows, 0, rows),
    }
}

fn place_bottom(view: &View, width: usize, rows: usize) -> Vec<Placement> {
    let h = (rows * 2 / 5).clamp(6.min(rows), rows);
    let top = rows - h;
    place_block(view, width, h, top, rows)
}

fn place_side(view: &View, width: usize, rows: usize) -> Vec<Placement> {
    let pw = 46.min(width / 2).max(20);
    let col = width - pw;
    let lines = panel_lines(view, pw);
    let lines = scroll_lines(lines, view.scroll, rows);
    lines
        .into_iter()
        .enumerate()
        .map(|(i, text)| Placement {
            row: i as u16,
            col: col as u16,
            text: fit(&text, pw),
        })
        .collect()
}

fn place_block(
    view: &View,
    width: usize,
    height: usize,
    top: usize,
    _rows: usize,
) -> Vec<Placement> {
    let lines = panel_lines(view, width);
    let lines = scroll_lines(lines, view.scroll, height);
    lines
        .into_iter()
        .enumerate()
        .map(|(i, text)| Placement {
            row: (top + i) as u16,
            col: 0,
            text: fit(&text, width),
        })
        .collect()
}

fn scroll_lines(mut lines: Vec<String>, scroll: usize, height: usize) -> Vec<String> {
    if lines.len() > height {
        let max = lines.len() - height;
        let skip = scroll.min(max);
        lines = lines.into_iter().skip(skip).take(height).collect();
    }
    while lines.len() < height {
        lines.push(String::new());
    }
    lines.truncate(height);
    lines
}

fn strip_line(view: &View) -> String {
    let snap = usage_line(view);
    format!(
        " debug {}  turn {}  {}  frame {}us  in->paint {}us  allocs {}  cpu {:.1}%  {}  F12",
        view.phase.label(),
        view.turns,
        snap,
        view.frames_us.last().copied().unwrap_or(0),
        view.last_input_us,
        view.last_allocs,
        view.cpu_pct,
        mem_label(view),
    )
}

fn usage_line(view: &View) -> String {
    let model = if view.model.is_empty() {
        "no model".to_string()
    } else {
        view.model.clone()
    };
    format!("{model} in {} out {}", view.last_in, view.last_out)
}

fn mem_label(view: &View) -> String {
    format!(
        "{} {:.1}MiB",
        view.rss_label,
        view.rss_bytes as f64 / (1024.0 * 1024.0)
    )
}

const SECTIONS: [&str; SECTION_COUNT] = [
    "Agent",
    "Model",
    "Tools",
    "Files",
    "Permissions",
    "Hooks",
    "Events",
    "Perf",
];

fn panel_lines(view: &View, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!(
        " debug  {}  F12 cycle  F8 break  F10 step  F5 run  Esc strip",
        view.phase.label()
    ));
    for (i, name) in SECTIONS.iter().enumerate() {
        let mark = if view.focus == i { ">" } else { " " };
        let flag = if view.open[i] { "-" } else { "+" };
        lines.push(format!("{mark}[{flag}] {name}{}", section_summary(view, i)));
        if view.open[i] {
            for body in section_body(view, i) {
                lines.push(format!("    {body}"));
            }
        }
    }
    lines.push(" up/down section  left/right fold  enter expand".into());
    let _ = width;
    lines
}

fn section_summary(view: &View, i: usize) -> String {
    match i {
        0 => format!("  {}", view.phase.label()),
        1 => {
            let who = if view.model.is_empty() {
                "no request yet".to_string()
            } else {
                format!("{} {}", view.protocol, view.model)
            };
            format!("  {who}")
        }
        2 => format!("  {}", view.tools.len()),
        3 => format!("  {}", view.files.len()),
        4 => format!("  {}", view.perms.len()),
        5 => format!("  {}", view.hooks.len()),
        6 => format!("  {}", view.events.len()),
        _ => format!(
            "  {}us  {} allocs",
            view.frames_us.last().copied().unwrap_or(0),
            view.last_allocs
        ),
    }
}

fn section_body(view: &View, i: usize) -> Vec<String> {
    match i {
        0 => agent_body(view),
        1 => model_body(view),
        2 => tool_body(view),
        3 => view
            .files
            .iter()
            .enumerate()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|(i, f)| {
                let mark = if i == view.file_cursor { "*" } else { " " };
                format!("{mark}{} {}  {}", f.op, f.path, one_line(&f.diff))
            })
            .collect(),
        4 => view
            .perms
            .iter()
            .rev()
            .take(8)
            .map(|p| format!("{} {} {}", p.decision, p.tool, one_line(&p.detail)))
            .collect(),
        5 => view
            .hooks
            .iter()
            .rev()
            .take(8)
            .map(|h| format!("{}  {}", h.event, h.command))
            .collect(),
        6 => view
            .events
            .iter()
            .rev()
            .take(12)
            .map(|e| e.title.clone())
            .collect(),
        _ => perf_body(view),
    }
}

fn agent_body(view: &View) -> Vec<String> {
    let mut lines = vec![format!(
        "phase {}   turns {}",
        view.phase.label(),
        view.turns
    )];
    if view.plan.is_empty() {
        lines.push("plan: none this session".into());
    } else {
        lines.push("plan:".into());
        for (i, step) in view.plan.iter().enumerate() {
            lines.push(format!("{}. {step}", i + 1));
        }
    }
    lines
}

fn model_body(view: &View) -> Vec<String> {
    let snap = crate::usage::snapshot();
    let cost = if snap.requests == 0 && view.last_in == 0 {
        "cost n/a".to_string()
    } else if snap.unpriced_requests > 0 && snap.cost_usd == 0.0 {
        "cost unknown".to_string()
    } else {
        format!("cost ${:.4}", snap.cost_usd)
    };
    let window = if view.context_tokens == 0 {
        "window unknown".to_string()
    } else {
        let used = crate::usage::last_context_tokens().unwrap_or(0);
        format!("ctx {used}/{}", view.context_tokens)
    };
    let flight = if view.inflight {
        "request in flight"
    } else {
        "idle"
    };
    let ttft = view
        .ttft_ms
        .map(|n| format!("{n}ms"))
        .unwrap_or_else(|| "-".into());
    let lat = view
        .latency_ms
        .map(|n| format!("{n}ms"))
        .unwrap_or_else(|| "-".into());
    let mut lines = vec![
        format!("{} {}  {flight}", view.protocol, view.model),
        format!(
            "in {} out {}  {window}  {cost}",
            snap.totals.input, snap.totals.output
        ),
        format!("latency {lat}  ttft {ttft}  retries {}", view.retries),
    ];
    if view.prompt.is_empty() {
        lines.push("prompt: not captured".into());
    } else {
        lines.push("prompt sent:".into());
        for p in view.prompt.iter().rev().take(6).rev() {
            lines.push(format!("{}: {}", p.role, one_line(&p.text)));
        }
    }
    lines
}

fn tool_body(view: &View) -> Vec<String> {
    if view.tools.is_empty() {
        return vec!["no tool calls yet".into()];
    }
    let mut lines = Vec::new();
    for (i, t) in view
        .tools
        .iter()
        .enumerate()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let cursor = if i == view.tool_cursor { "*" } else { " " };
        let code = t.code.map(|c| format!(" exit {c}")).unwrap_or_default();
        lines.push(format!(
            "{cursor}{:>5}ms {} {}{code} {}",
            t.ms,
            if t.ok { "ok " } else { "err" },
            t.name,
            one_line(&t.args)
        ));
        if t.open {
            if !t.cwd.is_empty() {
                lines.push(format!("cwd {}", t.cwd));
            }
            if !t.env_removed.is_empty() {
                lines.push(format!("env removed {}", t.env_removed));
            }
            if !t.stdout.is_empty() {
                lines.push(format!("stdout {}", one_line(&t.stdout)));
            }
            if !t.stderr.is_empty() {
                lines.push(format!("stderr {}", one_line(&t.stderr)));
            }
            if t.stdout.is_empty() && t.stderr.is_empty() {
                lines.push(format!("output {}", one_line(&t.output)));
            }
        }
    }
    lines
}

fn perf_body(view: &View) -> Vec<String> {
    vec![
        format!(
            "frame {}us  in->paint {}us  allocs {}  cpu {:.1}%  {}",
            view.frames_us.last().copied().unwrap_or(0),
            view.last_input_us,
            view.last_allocs,
            view.cpu_pct,
            mem_label(view)
        ),
        graph(&view.frames_us),
    ]
}

fn graph(samples: &[u64]) -> String {
    const RAMP: &[u8] = b" .:-=+*#";
    if samples.is_empty() {
        return "graph (no frames yet)".into();
    }
    let max = samples.iter().copied().max().unwrap_or(1).max(1);
    let mut out = String::from("graph ");
    for s in samples
        .iter()
        .rev()
        .take(32)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let idx = ((*s as u128 * (RAMP.len() as u128 - 1)) / max as u128) as usize;
        out.push(RAMP[idx] as char);
    }
    out
}

pub fn append_overlay(out: &mut Vec<u8>, width: u16, rows: u16, force: bool) {
    if !visible() || width == 0 || rows == 0 {
        return;
    }
    let mode = mode();
    let gen = GEN.load(Ordering::Relaxed);
    {
        let painted = painted();
        if painted.gen == gen
            && painted.width == width
            && painted.rows == rows
            && painted.mode == mode as u8
        {
            if force {
                out.extend_from_slice(&painted.bytes);
            }
            return;
        }
    }
    let bytes = {
        let g = store();
        let placed = layout(&g.view, mode, width, rows);
        placements_bytes(&placed)
    };
    out.extend_from_slice(&bytes);
    *painted() = Painted {
        gen,
        width,
        rows,
        mode: mode as u8,
        bytes,
    };
}

fn placements_bytes(placed: &[Placement]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in placed {
        let _ = write!(out, "\x1b[{};{}H{}", p.row + 1, p.col + 1, p.text);
    }
    out
}

fn push_json(out: &mut String, v: Value) {
    if let Ok(line) = serde_json::to_string(&v) {
        out.push_str(&line);
        out.push('\n');
    }
}

pub fn export_jsonl(view: &View) -> String {
    let mut out = String::new();
    push_json(
        &mut out,
        json!({
            "kind": "agent",
            "phase": view.phase.label(),
            "turns": view.turns,
            "plan": view.plan,
        }),
    );
    for p in &view.prompt {
        push_json(
            &mut out,
            json!({"kind": "prompt", "role": p.role, "text": p.text}),
        );
    }
    for t in &view.tools {
        push_json(
            &mut out,
            json!({
                "kind": "tool",
                "name": t.name,
                "args": t.args,
                "ms": t.ms,
                "ok": t.ok,
                "code": t.code,
                "cwd": t.cwd,
                "env_removed": t.env_removed,
                "stdout": t.stdout,
                "stderr": t.stderr,
                "output": t.output,
            }),
        );
    }
    for f in &view.files {
        push_json(
            &mut out,
            json!({"kind": "file", "path": f.path, "op": f.op, "diff": f.diff}),
        );
    }
    for p in &view.perms {
        push_json(
            &mut out,
            json!({"kind": "perm", "tool": p.tool, "decision": p.decision, "detail": p.detail}),
        );
    }
    for h in &view.hooks {
        push_json(
            &mut out,
            json!({"kind": "hook", "event": h.event, "command": h.command}),
        );
    }
    for e in &view.events {
        push_json(&mut out, json!({"kind": "event", "title": e.title}));
    }
    push_json(
        &mut out,
        json!({
            "kind": "model",
            "protocol": view.protocol,
            "model": view.model,
            "context_tokens": view.context_tokens,
            "latency_ms": view.latency_ms,
            "ttft_ms": view.ttft_ms,
            "retries": view.retries,
            "last_in": view.last_in,
            "last_out": view.last_out,
        }),
    );
    out
}

pub fn import_jsonl(text: &str) -> Result<View, String> {
    let mut view = View {
        open: [true; SECTION_COUNT],
        ..View::default()
    };
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line).map_err(|e| format!("line {}: {e}", n + 1))?;
        apply_record(&mut view, &v);
    }
    Ok(view)
}

fn apply_record(view: &mut View, v: &Value) {
    let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    match kind {
        "agent" => {
            view.turns = v.get("turns").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
            if let Some(steps) = v.get("plan").and_then(|p| p.as_array()) {
                view.plan = steps
                    .iter()
                    .filter_map(|s| s.as_str().map(|s| s.to_string()))
                    .collect();
            }
        }
        "prompt" => {
            if let (Some(role), Some(text)) = (
                v.get("role").and_then(|s| s.as_str()),
                v.get("text").and_then(|s| s.as_str()),
            ) {
                push_cap(
                    &mut view.prompt,
                    PROMPT_CAP,
                    PromptLine {
                        role: role.into(),
                        text: text.into(),
                    },
                );
            }
        }
        "tool" => push_cap(
            &mut view.tools,
            TOOLS_CAP,
            ToolRec {
                name: v.get("name").and_then(|s| s.as_str()).unwrap_or("").into(),
                args: v.get("args").and_then(|s| s.as_str()).unwrap_or("").into(),
                ms: v.get("ms").and_then(|n| n.as_u64()).unwrap_or(0),
                ok: v.get("ok").and_then(|b| b.as_bool()).unwrap_or(true),
                code: v.get("code").and_then(|n| n.as_i64()).map(|n| n as i32),
                output: v
                    .get("output")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .into(),
                cwd: v.get("cwd").and_then(|s| s.as_str()).unwrap_or("").into(),
                env_removed: v
                    .get("env_removed")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .into(),
                stdout: v
                    .get("stdout")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .into(),
                stderr: v
                    .get("stderr")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .into(),
                open: false,
            },
        ),
        "file" => push_cap(
            &mut view.files,
            FILES_CAP,
            FileRec {
                path: v.get("path").and_then(|s| s.as_str()).unwrap_or("").into(),
                op: v.get("op").and_then(|s| s.as_str()).unwrap_or("").into(),
                diff: v.get("diff").and_then(|s| s.as_str()).unwrap_or("").into(),
            },
        ),
        "perm" => push_cap(
            &mut view.perms,
            PERMS_CAP,
            PermRec {
                tool: v.get("tool").and_then(|s| s.as_str()).unwrap_or("").into(),
                decision: v
                    .get("decision")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .into(),
                detail: v
                    .get("detail")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .into(),
            },
        ),
        "hook" => push_cap(
            &mut view.hooks,
            HOOKS_CAP,
            HookRec {
                event: v.get("event").and_then(|s| s.as_str()).unwrap_or("").into(),
                command: v
                    .get("command")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .into(),
            },
        ),
        "event" => push_event(view, v.get("title").and_then(|s| s.as_str()).unwrap_or("")),
        "model" => {
            view.protocol = v
                .get("protocol")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .into();
            view.model = v.get("model").and_then(|s| s.as_str()).unwrap_or("").into();
            view.context_tokens = v
                .get("context_tokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as usize;
            view.latency_ms = v.get("latency_ms").and_then(|n| n.as_u64());
            view.ttft_ms = v.get("ttft_ms").and_then(|n| n.as_u64());
            view.retries = v.get("retries").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
            view.last_in = v.get("last_in").and_then(|n| n.as_u64()).unwrap_or(0);
            view.last_out = v.get("last_out").and_then(|n| n.as_u64()).unwrap_or(0);
        }
        // Existing trace.rs rows: {id, created_ms, kind, title, detail}.
        _ if v.get("title").is_some() => {
            let kind = v.get("kind").and_then(|s| s.as_str()).unwrap_or("event");
            let title = v.get("title").and_then(|s| s.as_str()).unwrap_or("");
            push_event(view, &format!("{kind} {title}"));
        }
        _ => {}
    }
}

fn push_event(view: &mut View, title: &str) {
    push_cap(
        &mut view.events,
        EVENTS_CAP,
        EventRec {
            title: clip(title, 160),
        },
    );
}

fn push_cap<T>(buf: &mut Vec<T>, cap: usize, item: T) {
    if buf.len() == cap {
        buf.remove(0);
    }
    buf.push(item);
}

fn push_cap_num(buf: &mut Vec<u64>, cap: usize, item: u64) {
    if buf.len() == cap {
        buf.remove(0);
    }
    buf.push(item);
}

fn append_log(v: &Value) {
    let path = live_log();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        if let Ok(line) = serde_json::to_string(v) {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn msg_preview(m: &Msg) -> (&'static str, &str) {
    match m {
        Msg::System(s) => ("system", s),
        Msg::User(s) => ("user", s),
        Msg::UserImages { text, .. } => ("user", text),
        Msg::Assistant { text, .. } => ("assistant", text),
        Msg::Tool(rs) => ("tool", rs.first().map(|r| r.content.as_str()).unwrap_or("")),
    }
}

fn edit_diff(input: &Value) -> String {
    let old = input["old_str"]
        .as_str()
        .or_else(|| input["old_string"].as_str());
    let new = input["new_str"]
        .as_str()
        .or_else(|| input["new_string"].as_str());
    match (old, new) {
        (Some(o), Some(n)) => {
            let mut s = String::new();
            for line in o.lines().take(6) {
                s.push_str("- ");
                s.push_str(&clip(line, 80));
                s.push('\n');
            }
            for line in n.lines().take(6) {
                s.push_str("+ ");
                s.push_str(&clip(line, 80));
                s.push('\n');
            }
            if s.is_empty() {
                clip(input["content"].as_str().unwrap_or(""), 200)
            } else {
                s
            }
        }
        _ => clip(input["content"].as_str().unwrap_or(""), 200),
    }
}

fn split_shell(output: &str) -> (String, String, Option<i32>) {
    let (stdout, stderr) = output.split_once("\n[stderr]\n").unwrap_or((output, ""));
    let code = output.lines().rev().find_map(|line| {
        let line = line.trim();
        let rest = line.strip_prefix("[exit ")?;
        let n = rest.strip_suffix(']')?;
        n.parse().ok()
    });
    (clip(stdout, OUT_CAP), clip(stderr, OUT_CAP), code)
}

fn one_line(s: &str) -> String {
    let flat: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    clip(&flat, 120)
}

fn clip(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

fn fit(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(width).collect();
    if s.chars().count() > width && width >= 1 {
        out = s.chars().take(width - 1).collect();
        out.push('…');
    }
    while out.chars().count() < width {
        out.push(' ');
    }
    out
}

fn host_now() -> (Duration, u64, &'static str) {
    #[cfg(target_os = "linux")]
    {
        let cpu = unix_cpu();
        let rss = linux_rss().unwrap_or(0);
        (cpu, rss, "rss")
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let (cpu, rss) = unix_rusage();
        (cpu, rss, "peak")
    }
    #[cfg(windows)]
    {
        windows_host()
    }
    #[cfg(not(any(unix, windows)))]
    {
        (Duration::ZERO, 0, "rss")
    }
}

#[cfg(unix)]
fn unix_cpu() -> Duration {
    let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
    // SAFETY: rusage is zeroed and the pointer is valid for the call.
    unsafe {
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
    }
    timeval(usage.ru_utime) + timeval(usage.ru_stime)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn unix_rusage() -> (Duration, u64) {
    let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
    unsafe {
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
    }
    // macOS reports ru_maxrss in bytes. Other unixes follow Linux and use KB.
    #[cfg(target_os = "macos")]
    let rss = usage.ru_maxrss as u64;
    #[cfg(not(target_os = "macos"))]
    let rss = usage.ru_maxrss as u64 * 1024;
    (timeval(usage.ru_utime) + timeval(usage.ru_stime), rss)
}

#[cfg(unix)]
fn timeval(tv: libc::timeval) -> Duration {
    Duration::new(tv.tv_sec as u64, (tv.tv_usec as u32) * 1000)
}

#[cfg(target_os = "linux")]
fn linux_rss() -> Option<u64> {
    let f = fs::File::open("/proc/self/status").ok()?;
    for line in BufReader::new(f).lines() {
        let line = line.ok()?;
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

#[cfg(windows)]
fn windows_host() -> (Duration, u64, &'static str) {
    #[repr(C)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set: usize,
        working_set: usize,
        quota_peak_paged: usize,
        quota_paged: usize,
        quota_peak_nonpaged: usize,
        quota_nonpaged: usize,
        pagefile: usize,
        peak_pagefile: usize,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn GetProcessTimes(
            h: *mut core::ffi::c_void,
            create: *mut u64,
            exit: *mut u64,
            kernel: *mut u64,
            user: *mut u64,
        ) -> i32;
        fn K32GetProcessMemoryInfo(h: *mut core::ffi::c_void, pmc: *mut Counters, cb: u32) -> i32;
    }
    unsafe {
        let h = GetCurrentProcess();
        let mut c = 0u64;
        let mut e = 0u64;
        let mut k = 0u64;
        let mut u = 0u64;
        let cpu = if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) != 0 {
            Duration::from_nanos(k.saturating_add(u).saturating_mul(100))
        } else {
            Duration::ZERO
        };
        let mut pmc = Counters {
            cb: std::mem::size_of::<Counters>() as u32,
            page_fault_count: 0,
            peak_working_set: 0,
            working_set: 0,
            quota_peak_paged: 0,
            quota_paged: 0,
            quota_peak_nonpaged: 0,
            quota_nonpaged: 0,
            pagefile: 0,
            peak_pagefile: 0,
        };
        let rss = if K32GetProcessMemoryInfo(h, &mut pmc, pmc.cb) != 0 {
            pmc.working_set as u64
        } else {
            0
        };
        (cpu, rss, "rss")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::field_reassign_with_default)]
    fn sample() -> View {
        let mut v = View::default();
        v.phase = Phase::Thinking;
        v.turns = 2;
        v.plan = vec!["Create the loop".into(), "Handle Ctrl+C".into()];
        v.protocol = "OpenAI".into();
        v.model = "gpt-test".into();
        v.context_tokens = 32_000;
        v.last_in = 1200;
        v.last_out = 340;
        v.retries = 1;
        v.latency_ms = Some(80);
        v.ttft_ms = Some(20);
        v.prompt = vec![PromptLine {
            role: "user".into(),
            text: "fix the build".into(),
        }];
        v.tools.push(ToolRec {
            name: "run_command".into(),
            args: "{\"command\":\"cargo test\"}".into(),
            ms: 12,
            ok: true,
            code: Some(0),
            output: "ok\n[exit 0]".into(),
            cwd: "/src".into(),
            env_removed: "OPENAI_API_KEY".into(),
            stdout: "ok".into(),
            stderr: String::new(),
            open: false,
        });
        v.files.push(FileRec {
            path: "src/main.rs".into(),
            op: "write".into(),
            diff: "- old\n+ new\n".into(),
        });
        v.perms.push(PermRec {
            tool: "run_command".into(),
            decision: "allow".into(),
            detail: String::new(),
        });
        v.hooks.push(HookRec {
            event: "PreToolUse".into(),
            command: "echo ok".into(),
        });
        v.events.push(EventRec {
            title: "turn".into(),
        });
        v.frames_us = vec![40, 80, 120, 90];
        v.last_allocs = 12;
        v.last_input_us = 30;
        v.cpu_pct = 0.4;
        v.rss_bytes = 9 * 1024 * 1024;
        v
    }

    #[test]
    fn cycle_walks_every_mode() {
        assert_eq!(cycle(Mode::Hidden), Mode::Strip);
        assert_eq!(cycle(Mode::Strip), Mode::Panel);
        assert_eq!(cycle(Mode::Panel), Mode::Full);
        assert_eq!(cycle(Mode::Full), Mode::Hidden);
    }

    #[test]
    fn hidden_layout_is_empty() {
        assert!(layout(&sample(), Mode::Hidden, 80, 24).is_empty());
    }

    #[test]
    fn strip_is_one_row_and_fits_80() {
        let lines = render_lines(&sample(), Mode::Strip, 80, 24);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("thinking"), "{}", lines[0]);
        assert!(
            lines[0].chars().count() <= 80,
            "{}",
            lines[0].chars().count()
        );
    }

    #[test]
    fn panel_80_lists_sections_and_fits() {
        let lines = render_lines(&sample(), Mode::Full, 80, 40);
        let joined = lines.join("\n");
        for name in ["Agent", "Model", "Tools", "Files", "Perf"] {
            assert!(joined.contains(name), "{name} missing:\n{joined}");
        }
        assert!(joined.contains("fix the build"));
        assert!(joined.contains("run_command"));
        assert!(lines.iter().all(|l| l.chars().count() <= 80));
        let bottom = render_lines(&sample(), Mode::Panel, 80, 30);
        assert_eq!(bottom.len(), 12);
        assert!(bottom.iter().all(|l| l.chars().count() <= 80));
    }

    #[test]
    fn panel_120_is_a_side_column() {
        let placed = layout(&sample(), Mode::Panel, 120, 30);
        assert!(!placed.is_empty());
        assert!(placed.iter().all(|p| p.col >= 120 - 46), "{placed:?}");
        assert!(placed.iter().all(|p| p.text.chars().count() <= 46));
    }

    #[test]
    fn fullscreen_uses_every_row() {
        let lines = render_lines(&sample(), Mode::Full, 100, 20);
        assert_eq!(lines.len(), 20);
        assert!(lines.iter().all(|l| l.chars().count() <= 100));
    }

    #[test]
    fn enter_expands_a_collapsed_section() {
        let mut view = sample();
        view.focus = 3;
        view.open[3] = false;
        apply_nav(&mut view, DebugKey::Enter);
        assert!(view.open[3]);
        let lines = render_lines(&view, Mode::Full, 100, 40);
        assert!(lines.iter().any(|l| l.contains("src/main.rs")));
    }

    #[test]
    fn enter_on_a_tool_shows_cwd_and_env() {
        let mut view = sample();
        view.focus = 2;
        view.tool_cursor = 0;
        apply_nav(&mut view, DebugKey::Enter);
        let lines = render_lines(&view, Mode::Full, 120, 40);
        let joined = lines.join("\n");
        assert!(joined.contains("cwd /src"), "{joined}");
        assert!(joined.contains("OPENAI_API_KEY"), "{joined}");
    }

    #[test]
    fn export_roundtrip_keeps_tools_files_and_prompt() {
        let view = sample();
        let text = export_jsonl(&view);
        let back = import_jsonl(&text).unwrap();
        assert_eq!(back.tools.len(), 1);
        assert_eq!(back.tools[0].name, "run_command");
        assert_eq!(back.tools[0].code, Some(0));
        assert_eq!(back.tools[0].cwd, "/src");
        assert_eq!(back.files[0].path, "src/main.rs");
        assert!(back.files[0].diff.contains("- old"));
        assert_eq!(back.prompt[0].text, "fix the build");
        assert_eq!(back.hooks[0].event, "PreToolUse");
        assert_eq!(back.turns, 2);
        assert_eq!(back.model, "gpt-test");
    }

    #[test]
    fn import_accepts_trace_rows() {
        let text = "{\"id\":1,\"created_ms\":1,\"kind\":\"tool_call\",\"title\":\"read a.rs\",\"detail\":{}}\n";
        let view = import_jsonl(text).unwrap();
        assert!(view.events.iter().any(|e| e.title.contains("read a.rs")));
    }

    #[test]
    fn unarmed_checkpoint_does_not_record() {
        let was = ARMED.swap(false, Ordering::Relaxed);
        let before = store().view.events.len();
        checkpoint("should not land");
        assert!(tool_start("read_file", &json!({}), Path::new(".")).is_none());
        assert_eq!(store().view.events.len(), before);
        ARMED.store(was, Ordering::Relaxed);
    }

    #[test]
    fn shell_split_finds_stderr_and_exit() {
        let (out, err, code) = split_shell("hello\n[stderr]\nnope\n[exit 2]");
        assert!(out.contains("hello"));
        assert!(err.contains("nope"));
        assert_eq!(code, Some(2));
    }
}
