// `bwn acp`: an Agent Client Protocol (v1) server on stdio, so editors that
// speak ACP (Zed, JetBrains, Neovim plugins) drive the same agent loop as
// the terminal. JSON-RPC 2.0, one message per line; stdout carries only
// protocol messages, so anything else the process prints goes to stderr.
//
// The loop itself is unchanged: its report events become session/update
// notifications, the approval prompt becomes session/request_permission,
// and the file tools read and write through the editor when it offers fs.
// Permissions, trust and hooks apply exactly as in the terminal.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Condvar, Mutex, MutexGuard, OnceLock};

use serde_json::{json, Value};

use crate::agent::{self, Permission, RemoteAnswer, RemoteAsk};
use crate::provider::{Msg, Provider};
use crate::{config, hooks, mcp, media, report, session, tools, tui, CliOptions, Mode};

const PROTOCOL_VERSION: u64 = 1;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

// Tool results longer than this are cut in the update; the model still gets
// the whole result.
const MAX_SHOWN_RESULT: usize = 4_000;

type RpcError = (i64, String);

// A turn the editor started with session/prompt.
struct Job {
    request_id: Value,
    session_id: String,
    prompt: Vec<Value>,
}

struct SessionState {
    cwd: PathBuf,
    mode: Mode,
    transcript: Vec<Msg>,
    mcp_servers: Vec<Value>,
    running: bool,
}

// The turn being run, for the event sink, the approver and session/cancel.
struct Turn {
    session_id: String,
    cwd: PathBuf,
    cancelled: bool,
    events: EventState,
}

// What translating one turn's events needs to remember.
#[derive(Default)]
struct EventState {
    // Tool calls between their announcement and their result, innermost
    // last: a helper's (spawn_subagent) calls run inside its own.
    open: Vec<OpenCall>,
    // The last update was message text, so a new block starts on its own
    // paragraph.
    after_text: bool,
}

struct Waiting {
    // A session/request_permission, which session/cancel closes itself.
    question: bool,
    answer: mpsc::Sender<Result<Value, Value>>,
}

struct OpenCall {
    id: String,
    diffs: Vec<Value>,
}

struct Shared {
    out: Mutex<Box<dyn Write + Send>>,
    next_request: AtomicU64,
    // Our requests to the editor that wait for an answer.
    waiting: Mutex<HashMap<u64, Waiting>>,
    closed: AtomicBool,
    client_reads: AtomicBool,
    client_writes: AtomicBool,
    // The folder this process serves: hooks, trust and project settings are
    // per process, as in the terminal.
    cwd: Mutex<Option<PathBuf>>,
    sessions: Mutex<HashMap<String, SessionState>>,
    turn: Mutex<Option<Turn>>,
    queue: Mutex<VecDeque<Job>>,
    queued: Condvar,
}

static SHARED: OnceLock<Shared> = OnceLock::new();
static CALL_SEQ: AtomicU64 = AtomicU64::new(1);

fn shared() -> &'static Shared {
    SHARED.get().expect("acp server state")
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ── wire ────────────────────────────────────────────────────────────────────

fn send(msg: Value) {
    let Some(s) = SHARED.get() else { return };
    let mut out = lock(&s.out);
    // serde_json escapes newlines, so each message is exactly one line.
    let _ = writeln!(out, "{msg}");
    let _ = out.flush();
}

fn respond(id: Value, result: Value) {
    send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

fn respond_error(id: Value, (code, message): RpcError) {
    send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
}

fn notify(method: &str, params: Value) {
    send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
}

fn session_update(session_id: &str, update: Value) {
    notify(
        "session/update",
        json!({"sessionId": session_id, "update": update}),
    );
}

// A request to the editor, answered on the reader thread. Err carries the
// editor's error message, or says the connection is gone.
fn request(method: &str, params: Value) -> Result<Value, String> {
    let s = shared();
    let id = s.next_request.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    lock(&s.waiting).insert(
        id,
        Waiting {
            question: method == "session/request_permission",
            answer: tx,
        },
    );
    if s.closed.load(Ordering::Relaxed) {
        lock(&s.waiting).remove(&id);
        return Err("the editor closed the connection".into());
    }
    send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
    match rx.recv() {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(e["message"]
            .as_str()
            .unwrap_or("the editor refused the request")
            .to_string()),
        Err(_) => Err("the editor closed the connection".into()),
    }
}

// ── stdout ──────────────────────────────────────────────────────────────────

// The protocol keeps the real stdout; fd 1 (or the Windows standard output
// handle) now points at stderr, so a stray print can never corrupt a message.
// The kept copy is not inherited: a command, hook or MCP server that could
// write to it could send the editor requests of its own.
#[cfg(unix)]
fn take_stdout() -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    // SAFETY: fcntl and dup2 on this process's own standard descriptors; the
    // duplicate is owned by the returned File and by nothing else.
    unsafe {
        let fd = libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::dup2(2, 1) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        Ok(std::fs::File::from_raw_fd(fd))
    }
}

#[cfg(windows)]
fn take_stdout() -> std::io::Result<std::fs::File> {
    use std::os::windows::io::FromRawHandle;
    type Handle = *mut std::ffi::c_void;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const HANDLE_FLAG_INHERIT: u32 = 1;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> Handle;
        fn SetStdHandle(which: u32, handle: Handle) -> i32;
        fn SetHandleInformation(handle: Handle, mask: u32, flags: u32) -> i32;
    }
    // SAFETY: Rust's stdout looks the handle up on every write, so after
    // SetStdHandle prints reach stderr; the original handle is owned by the
    // returned File and by nothing else.
    unsafe {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        if out.is_null() || out as isize == -1 {
            return Err(std::io::Error::last_os_error());
        }
        if SetHandleInformation(out, HANDLE_FLAG_INHERIT, 0) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if SetStdHandle(STD_OUTPUT_HANDLE, GetStdHandle(STD_ERROR_HANDLE)) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(std::fs::File::from_raw_handle(out))
    }
}

// ── entry point ─────────────────────────────────────────────────────────────

/// `bwn acp`: serves one editor until it closes stdin. Returns the exit code.
pub(crate) fn serve(opts: &CliOptions) -> i32 {
    // A misspelt --permission-mode is a usage error before anything starts.
    if let Some(Err(e)) = opts.permission_mode.as_deref().map(agent::parse_permission) {
        eprintln!("buildwithnexus: {e}");
        return 2;
    }
    let out = match take_stdout() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("buildwithnexus acp: cannot take stdout for the protocol: {e}");
            return 1;
        }
    };
    let _ = SHARED.set(Shared {
        out: Mutex::new(Box::new(std::io::BufWriter::new(out))),
        next_request: AtomicU64::new(1),
        waiting: Mutex::new(HashMap::new()),
        closed: AtomicBool::new(false),
        client_reads: AtomicBool::new(false),
        client_writes: AtomicBool::new(false),
        cwd: Mutex::new(None),
        sessions: Mutex::new(HashMap::new()),
        turn: Mutex::new(None),
        queue: Mutex::new(VecDeque::new()),
        queued: Condvar::new(),
    });
    tui::set_protocol_stdin();
    report::set(report::Mode::Json);
    report::set_sink(Box::new(on_event));
    agent::set_remote_approver(Box::new(on_question));
    tools::set_editor_files(Box::new(EditorFiles));

    let worker_opts = opts.clone();
    let worker = std::thread::spawn(move || run_turns(&worker_opts));

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if !line.trim().is_empty() {
            handle_line(&line);
        }
    }

    // End of input: nobody is left to answer. Stop the turn, fail what
    // waits on the editor, and let the worker finish.
    let s = shared();
    s.closed.store(true, Ordering::Relaxed);
    lock(&s.waiting).clear();
    if let Some(t) = lock(&s.turn).as_mut() {
        t.cancelled = true;
        tui::trigger_interrupt(tui::InterruptKind::Escape);
    }
    lock(&s.queue).clear();
    s.queued.notify_all();
    let _ = worker.join();
    0
}

// ── incoming messages ───────────────────────────────────────────────────────

fn handle_line(line: &str) {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            respond_error(Value::Null, (PARSE_ERROR, format!("parse error: {e}")));
            return;
        }
    };
    let Some(obj) = msg.as_object() else {
        respond_error(
            Value::Null,
            (
                INVALID_REQUEST,
                "expected one JSON-RPC message object".into(),
            ),
        );
        return;
    };
    let id = obj.get("id").cloned();
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    match (obj.get("method"), id) {
        (None, Some(id)) if obj.contains_key("result") || obj.contains_key("error") => {
            answer_arrived(&id, obj.get("result").cloned(), obj.get("error").cloned());
        }
        (None, id) => respond_error(
            id.unwrap_or(Value::Null),
            (INVALID_REQUEST, "a request needs a method".into()),
        ),
        (Some(Value::String(method)), None) => on_notification(method, &params),
        (Some(Value::String(method)), Some(id)) => on_request(id, method, params),
        (Some(_), id) => respond_error(
            id.unwrap_or(Value::Null),
            (INVALID_REQUEST, "method must be a string".into()),
        ),
    }
}

fn answer_arrived(id: &Value, result: Option<Value>, error: Option<Value>) {
    let Some(n) = id.as_u64() else { return };
    if let Some(w) = lock(&shared().waiting).remove(&n) {
        let _ = w.answer.send(match error {
            Some(e) => Err(e),
            None => Ok(result.unwrap_or(Value::Null)),
        });
    }
}

fn on_notification(method: &str, params: &Value) {
    if method == "session/cancel" {
        if let Some(sid) = params["sessionId"].as_str() {
            cancel(sid);
        }
    }
    // Other notifications ($/cancel_request, extensions) need no answer.
}

fn on_request(id: Value, method: &str, params: Value) {
    let result = match method {
        "initialize" => initialize(&params),
        "authenticate" => Err((
            INVALID_PARAMS,
            "bwn has no editor sign-in: set up a provider by running `bwn` in a terminal once, \
             or put the provider's API key in the environment the editor starts bwn with"
                .into(),
        )),
        "session/new" => new_session(&params),
        "session/load" => load_session(&params),
        "session/set_mode" => set_mode(&params),
        "session/prompt" => {
            // Answered by the worker when the turn ends.
            if let Err(e) = queue_prompt(id.clone(), &params) {
                respond_error(id, e);
            }
            return;
        }
        other => Err((METHOD_NOT_FOUND, format!("method not found: {other}"))),
    };
    match result {
        Ok(v) => respond(id, v),
        Err(e) => respond_error(id, e),
    }
}

fn initialize(params: &Value) -> Result<Value, RpcError> {
    if !params["protocolVersion"].is_u64() {
        return Err((
            INVALID_PARAMS,
            "initialize needs a protocolVersion number".into(),
        ));
    }
    let fs = &params["clientCapabilities"]["fs"];
    let s = shared();
    s.client_reads
        .store(fs["readTextFile"] == true, Ordering::Relaxed);
    s.client_writes
        .store(fs["writeTextFile"] == true, Ordering::Relaxed);
    Ok(json!({
        "protocolVersion": PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": true,
            "promptCapabilities": {"image": true, "audio": false, "embeddedContext": true},
            "mcpCapabilities": {"http": true, "sse": false},
        },
        "agentInfo": {"name": "buildwithnexus", "title": "buildwithnexus", "version": crate::VERSION},
        "authMethods": [],
    }))
}

fn modes(current: Mode) -> Value {
    json!({
        "currentModeId": mode_id(current),
        "availableModes": [
            {"id": "build", "name": "Build",
             "description": "Edits files and runs commands, asking first as your permission mode says"},
            {"id": "plan", "name": "Plan",
             "description": "Reads the code and proposes a plan; nothing changes until you approve it"},
            {"id": "brainstorm", "name": "Brainstorm",
             "description": "A read-only discussion of the code"},
        ],
    })
}

fn mode_id(m: Mode) -> &'static str {
    match m {
        Mode::Build => "build",
        Mode::Plan => "plan",
        Mode::Brainstorm => "brainstorm",
    }
}

fn parse_mode(id: &str) -> Option<Mode> {
    match id {
        "build" => Some(Mode::Build),
        "plan" => Some(Mode::Plan),
        "brainstorm" => Some(Mode::Brainstorm),
        _ => None,
    }
}

// The session's folder: absolute and present.
fn session_cwd(params: &Value) -> Result<PathBuf, RpcError> {
    let raw = params["cwd"]
        .as_str()
        .ok_or((INVALID_PARAMS, "cwd is required".to_string()))?;
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err((
            INVALID_PARAMS,
            format!("cwd must be an absolute path (got {raw})"),
        ));
    }
    if !path.is_dir() {
        return Err((INVALID_PARAMS, format!("cwd is not a folder: {raw}")));
    }
    Ok(path)
}

// The first session's folder becomes the process's: hooks, trust and
// project settings belong to one folder, as in the terminal.
fn bind_cwd(path: PathBuf) -> Result<PathBuf, RpcError> {
    let mut bound = lock(&shared().cwd);
    match bound.as_ref() {
        Some(b) if !same_dir(b, &path) => Err((
            INVALID_PARAMS,
            format!(
                "this bwn acp process serves {}; start another for {}",
                b.display(),
                path.display()
            ),
        )),
        Some(b) => Ok(b.clone()),
        None => {
            std::env::set_current_dir(&path).map_err(|e| {
                (
                    INVALID_PARAMS,
                    format!("cannot open {}: {e}", path.display()),
                )
            })?;
            *bound = Some(path.clone());
            Ok(path)
        }
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
}

fn new_session(params: &Value) -> Result<Value, RpcError> {
    let cwd = bind_cwd(session_cwd(params)?)?;
    let id = session::new_id();
    lock(&shared().sessions).insert(
        id.clone(),
        SessionState {
            cwd,
            mode: Mode::Build,
            transcript: Vec::new(),
            mcp_servers: mcp_list(params),
            running: false,
        },
    );
    Ok(json!({"sessionId": id, "modes": modes(Mode::Build)}))
}

fn mcp_list(params: &Value) -> Vec<Value> {
    params["mcpServers"].as_array().cloned().unwrap_or_default()
}

// session/load: the saved bwn session with this id, replayed to the editor
// before the response, as the protocol asks.
fn load_session(params: &Value) -> Result<Value, RpcError> {
    let sid = params["sessionId"]
        .as_str()
        .ok_or((INVALID_PARAMS, "sessionId is required".to_string()))?;
    let cwd = session_cwd(params)?;
    let saved =
        session::load(sid).ok_or_else(|| (INVALID_PARAMS, format!("no saved session {sid}")))?;
    if !saved.is_in(&cwd) {
        return Err((
            INVALID_PARAMS,
            format!(
                "session {sid} was started in {}, not {}",
                saved.cwd,
                cwd.display()
            ),
        ));
    }
    if lock(&shared().sessions).get(sid).is_some_and(|s| s.running) {
        return Err((INVALID_PARAMS, format!("session {sid} is running a prompt")));
    }
    let cwd = bind_cwd(cwd)?;
    for update in replay_updates(&saved.msgs) {
        session_update(sid, update);
    }
    lock(&shared().sessions).insert(
        sid.to_string(),
        SessionState {
            cwd,
            mode: Mode::Build,
            transcript: saved.msgs,
            mcp_servers: mcp_list(params),
            running: false,
        },
    );
    Ok(json!({"modes": modes(Mode::Build)}))
}

fn set_mode(params: &Value) -> Result<Value, RpcError> {
    let sid = params["sessionId"].as_str().unwrap_or("");
    let mode_id = params["modeId"].as_str().unwrap_or("");
    let mode = parse_mode(mode_id).ok_or_else(|| {
        (
            INVALID_PARAMS,
            format!("unknown mode {mode_id}: build, plan or brainstorm"),
        )
    })?;
    let mut sessions = lock(&shared().sessions);
    let s = sessions
        .get_mut(sid)
        .ok_or_else(|| (INVALID_PARAMS, format!("unknown session {sid}")))?;
    s.mode = mode;
    Ok(json!({}))
}

fn queue_prompt(request_id: Value, params: &Value) -> Result<(), RpcError> {
    let sid = params["sessionId"].as_str().unwrap_or("");
    let Some(prompt) = params["prompt"].as_array() else {
        return Err((
            INVALID_PARAMS,
            "prompt must be an array of content blocks".into(),
        ));
    };
    let input = prompt_input(prompt);
    if input.text.trim().is_empty() && input.images.is_empty() {
        return Err((INVALID_PARAMS, "the prompt is empty".into()));
    }
    let s = shared();
    {
        let mut sessions = lock(&s.sessions);
        let state = sessions
            .get_mut(sid)
            .ok_or_else(|| (INVALID_PARAMS, format!("unknown session {sid}")))?;
        if state.running {
            return Err((
                INVALID_PARAMS,
                format!("session {sid} is already running a prompt"),
            ));
        }
        state.running = true;
    }
    lock(&s.queue).push_back(Job {
        request_id,
        session_id: sid.to_string(),
        prompt: prompt.clone(),
    });
    s.queued.notify_all();
    Ok(())
}

// session/cancel: a queued prompt ends at once; the running turn stops at
// its next check, and its prompt answers `cancelled`.
fn cancel(sid: &str) {
    let s = shared();
    let dropped: Vec<Job> = {
        let mut q = lock(&s.queue);
        let (gone, kept): (Vec<Job>, Vec<Job>) = q.drain(..).partition(|j| j.session_id == sid);
        *q = kept.into();
        gone
    };
    for job in dropped {
        mark_idle(&job.session_id);
        respond(job.request_id, json!({"stopReason": "cancelled"}));
    }
    if let Some(t) = lock(&s.turn).as_mut().filter(|t| t.session_id == sid) {
        t.cancelled = true;
        tui::trigger_interrupt(tui::InterruptKind::Escape);
        // The protocol has the editor answer an open question `cancelled`;
        // one that does not would leave the turn waiting for good. Its
        // answer, if it comes, finds nobody waiting.
        lock(&s.waiting).retain(|_, w| !w.question);
    }
}

fn mark_idle(sid: &str) {
    if let Some(state) = lock(&shared().sessions).get_mut(sid) {
        state.running = false;
    }
}

// ── turns ───────────────────────────────────────────────────────────────────

struct Runtime {
    provider: Provider,
    perm: Permission,
    cwd: PathBuf,
}

// The worker: one turn at a time, in the order the prompts came.
fn run_turns(opts: &CliOptions) {
    let s = shared();
    let mut runtime: Option<Runtime> = None;
    loop {
        let job = {
            let mut q = lock(&s.queue);
            loop {
                if let Some(job) = q.pop_front() {
                    // Claimed with the queue locked, so session/cancel finds
                    // a prompt either queued or running, never in between.
                    *lock(&s.turn) = Some(Turn {
                        session_id: job.session_id.clone(),
                        cwd: PathBuf::new(),
                        cancelled: false,
                        events: EventState::default(),
                    });
                    break Some(job);
                }
                if s.closed.load(Ordering::Relaxed) {
                    break None;
                }
                q = s.queued.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some(job) = job else { break };
        run_turn(opts, &mut runtime, job);
    }
    if let Some(rt) = &runtime {
        hooks::notify("SessionEnd", &rt.cwd);
    }
    mcp::shutdown();
}

fn run_turn(opts: &CliOptions, runtime: &mut Option<Runtime>, job: Job) {
    let s = shared();
    let Some((cwd, mode, mut transcript, servers)) = ({
        let mut sessions = lock(&s.sessions);
        sessions.get_mut(&job.session_id).map(|st| {
            (
                st.cwd.clone(),
                st.mode,
                std::mem::take(&mut st.transcript),
                st.mcp_servers.clone(),
            )
        })
    }) else {
        lock(&s.turn).take();
        respond_error(
            job.request_id,
            (
                INVALID_PARAMS,
                format!("unknown session {}", job.session_id),
            ),
        );
        return;
    };
    tui::consume_interrupt();
    agent::reset_turn_outcome();
    report::clear_denials();
    let cancelled_early = match lock(&s.turn).as_mut() {
        Some(t) => {
            t.cwd = cwd.clone();
            t.cancelled
        }
        None => false,
    };

    let outcome = (|| -> Result<(), String> {
        if cancelled_early {
            return Ok(());
        }
        if runtime.is_none() {
            *runtime = Some(start_runtime(opts, &cwd, &servers)?);
        }
        let rt = runtime.as_ref().expect("runtime");
        let input = prompt_input(&job.prompt);
        let mut images = input.images;
        if !images.is_empty() && !media::model_supports_vision(&rt.provider) {
            images.clear();
            report::notice(&media::vision_refusal(&rt.provider));
        }
        for skipped in &input.skipped {
            report::notice(skipped);
        }
        // Only images the model cannot see: the notice above is the answer.
        if input.text.trim().is_empty() && images.is_empty() {
            return Ok(());
        }
        run_mode(
            rt,
            &job.session_id,
            mode,
            &input.text,
            images,
            &mut transcript,
        )
    })();

    let cancelled = lock(&s.turn).take().is_some_and(|t| t.cancelled);
    tui::consume_interrupt();
    if let Some(st) = lock(&s.sessions).get_mut(&job.session_id) {
        st.transcript = transcript;
        st.running = false;
    }
    match outcome {
        _ if cancelled => respond(job.request_id, json!({"stopReason": "cancelled"})),
        Ok(()) => {
            let stop = match agent::stopped_short_outcome() {
                Some(agent::Outcome::StepLimit) => "max_turn_requests",
                _ => "end_turn",
            };
            respond(job.request_id, json!({"stopReason": stop}));
        }
        Err(e) => respond_error(job.request_id, (INTERNAL_ERROR, e)),
    }
}

// Runs the turn in the session's mode, as the terminal's composer does.
fn run_mode(
    rt: &Runtime,
    sid: &str,
    mode: Mode,
    task: &str,
    images: Vec<(String, String)>,
    transcript: &mut Vec<Msg>,
) -> Result<(), String> {
    let (p, perm, cwd) = (&rt.provider, rt.perm, rt.cwd.as_path());
    if crate::should_answer_conversationally(task, &mode) {
        let within = match mode {
            Mode::Build => agent::ChatIn::Build,
            Mode::Plan => agent::ChatIn::Plan,
            Mode::Brainstorm => agent::ChatIn::Brainstorm,
        };
        return agent::run_chat_turn(p, perm, within, cwd, task, images, transcript, sid);
    }
    match mode {
        Mode::Build => agent::run_build_session_with_images(
            p, perm, "engineer", task, cwd, transcript, sid, images,
        ),
        Mode::Plan => {
            agent::plan_turn(p, perm, task, cwd, false, images, transcript, sid).map(|_| ())
        }
        // A suggestion to switch modes is the editor's to make.
        Mode::Brainstorm => {
            agent::brainstorm_turn(p, cwd, task, images, transcript, sid).map(|_| ())
        }
    }
}

// What the terminal sets up once at startup, done at the first prompt: the
// trust questions need a turn to be asked in.
fn start_runtime(opts: &CliOptions, cwd: &Path, servers: &[Value]) -> Result<Runtime, String> {
    hooks::trust_project_remote(cwd, &mut ask_trust);
    let (provider, perm) = crate::provider_or_onboard(opts)?;
    crate::provider::prewarm(&provider);
    hooks::init(cwd, false);
    hooks::set_permission_mode(agent::permission_name(perm));
    hooks::notify("SessionStart", cwd);
    if let Some(n) = agent::ignored_approvals_notice_once(cwd) {
        eprintln!("buildwithnexus: {}", n.trim());
    }
    let mut all = config::load_settings()
        .map(|s| s.mcp_servers)
        .unwrap_or_default();
    for (name, server) in editor_mcp_servers(servers, &mut |n| eprintln!("buildwithnexus: {n}")) {
        all.insert(name, server);
    }
    mcp::start_with(&all);
    for n in mcp::ensure_ready_headless() {
        eprintln!("buildwithnexus: {}", n.trim());
    }
    for (msg, _) in mcp::drain_notices() {
        eprintln!("buildwithnexus: {}", msg.trim());
    }
    Ok(Runtime {
        provider,
        perm,
        cwd: cwd.to_path_buf(),
    })
}

// The editor's MCP servers in the shape of the `mcp_servers` setting. SSE
// is not offered in initialize; one sent anyway is skipped with a note.
fn editor_mcp_servers(servers: &[Value], note: &mut dyn FnMut(&str)) -> BTreeMap<String, Value> {
    let pairs = |v: &Value| -> serde_json::Map<String, Value> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|e| Some((e["name"].as_str()?.to_string(), e["value"].clone())))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut out = BTreeMap::new();
    for s in servers {
        let Some(name) = s["name"].as_str().filter(|n| !n.is_empty()) else {
            continue;
        };
        let entry = match s["type"].as_str() {
            Some("http") => {
                json!({"type": "http", "url": s["url"], "headers": pairs(&s["headers"])})
            }
            Some("sse") => {
                note(&format!(
                    "mcp: {name} uses SSE, which bwn does not support — skipped"
                ));
                continue;
            }
            _ => json!({"command": s["command"], "args": s["args"], "env": pairs(&s["env"])}),
        };
        out.insert(name.to_string(), entry);
    }
    out
}

// ── prompt content ──────────────────────────────────────────────────────────

#[derive(Debug, Default, PartialEq)]
struct PromptInput {
    text: String,
    images: Vec<(String, String)>,
    // Blocks that could not be used, said to the person.
    skipped: Vec<String>,
}

fn file_path_of(uri: &str) -> String {
    uri.strip_prefix("file://").unwrap_or(uri).to_string()
}

// The task text and images of a session/prompt. Embedded files become
// context blocks; linked files are named for the model to read.
fn prompt_input(blocks: &[Value]) -> PromptInput {
    fn push(text: &mut String, part: &str) {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(part);
    }
    let mut input = PromptInput::default();
    for b in blocks {
        match b["type"].as_str().unwrap_or("") {
            "text" => {
                let t = b["text"].as_str().unwrap_or("");
                if !t.is_empty() {
                    push(&mut input.text, t);
                }
            }
            "image" => match (b["mimeType"].as_str(), b["data"].as_str()) {
                (Some(m), Some(d)) => input.images.push((m.to_string(), d.to_string())),
                _ => input
                    .skipped
                    .push("an image without data was skipped".into()),
            },
            "resource_link" => {
                let uri = b["uri"].as_str().unwrap_or("");
                push(
                    &mut input.text,
                    &format!("Referenced file: {}", file_path_of(uri)),
                );
            }
            "resource" => {
                let r = &b["resource"];
                let uri = r["uri"].as_str().unwrap_or("");
                let mime = r["mimeType"].as_str().unwrap_or("");
                if let Some(t) = r["text"].as_str() {
                    push(
                        &mut input.text,
                        &format!("<context uri=\"{uri}\">\n{t}\n</context>"),
                    );
                } else if let (true, Some(d)) = (mime.starts_with("image/"), r["blob"].as_str()) {
                    input.images.push((mime.to_string(), d.to_string()));
                } else {
                    input
                        .skipped
                        .push(format!("{uri} is binary and was not sent to the model"));
                }
            }
            "audio" => input
                .skipped
                .push("audio is not supported and was not sent to the model".into()),
            other => input
                .skipped
                .push(format!("a {other} block is not supported and was skipped")),
        }
    }
    input
}

// ── events → session updates ────────────────────────────────────────────────

// Calls that steer the loop rather than act; the editor sees their effect
// (the summary, the plan), not the call.
fn is_control_tool(name: &str) -> bool {
    matches!(name, "finish" | "exit_plan" | "ExitPlanMode")
}

fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" | "read_file" | "read_many_files" | "list" | "list_dir" | "list_tree"
        | "file_info" | "read_server_log" | "list_servers" | "todo_read" | "todoread"
        | "list_skills" | "load_skill" | "skill" | "list_python_tools" | "kb_query" => "read",
        "write" | "write_file" | "edit" | "edit_file" | "multi_edit" | "patch" | "apply_patch"
        | "create_docx" | "create_dir" | "str_replace_editor" => "edit",
        "remove_path" => "delete",
        "move_path" => "move",
        "glob" | "find_paths" | "find_files" | "grep" | "grep_files" | "web_search"
        | "websearch" => "search",
        "bash" | "run_command" | "python_tool" | "start_server" | "stop_server" | "check_work" => {
            "execute"
        }
        "fetch_url" | "webfetch" | "headless_browser" | "wait_for_url" | "open_browser" => "fetch",
        "todo_write" | "todowrite" => "think",
        "exit_plan" | "ExitPlanMode" => "switch_mode",
        _ => "other",
    }
}

fn text_content(text: &str) -> Value {
    json!({"type": "content", "content": {"type": "text", "text": text}})
}

fn clip(text: &str) -> String {
    if text.len() <= MAX_SHOWN_RESULT {
        return text.to_string();
    }
    let mut end = MAX_SHOWN_RESULT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… ({} more bytes)", &text[..end], text.len() - end)
}

fn next_call_id() -> String {
    format!("call-{}", CALL_SEQ.fetch_add(1, Ordering::Relaxed))
}

fn plan_status(s: &str) -> &'static str {
    match s {
        "completed" | "done" => "completed",
        "in_progress" | "in-progress" | "active" => "in_progress",
        _ => "pending",
    }
}

// A message chunk, on a new paragraph when it does not continue the reply.
fn message(state: &mut EventState, text: &str, continues: bool) -> Value {
    let text = if state.after_text && !continues {
        format!("\n\n{text}")
    } else {
        text.to_string()
    };
    json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}})
}

// The session updates one bwn event becomes. `cwd` resolves tool paths.
fn updates_for(ev: &Value, state: &mut EventState, cwd: &Path) -> Vec<Value> {
    let kind = ev["type"].as_str().unwrap_or("");
    let text = |k: &str| ev[k].as_str().unwrap_or("").to_string();
    let mut out = Vec::new();
    let mut is_text = false;
    match kind {
        "assistant_delta" => {
            out.push(message(state, &text("text"), true));
            is_text = true;
        }
        "assistant" | "finish" | "error" => {
            let t = text(if kind == "finish" {
                "summary"
            } else if kind == "assistant" {
                "text"
            } else {
                "message"
            });
            if !t.trim().is_empty() {
                out.push(message(state, t.trim_end(), false));
                is_text = true;
            }
        }
        "notice" | "subagent_result" => {
            let t = text("message");
            if !t.trim().is_empty() {
                out.push(message(state, t.trim(), false));
                is_text = true;
            }
        }
        "thinking_delta" => out.push(json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": text("text")},
        })),
        "tool_call" => {
            let name = text("name");
            if !is_control_tool(&name) {
                let id = next_call_id();
                let input = &ev["input"];
                // Some previews are only the tool's name: add what it reads.
                let title = match (ev["title"].as_str(), input["path"].as_str()) {
                    (Some(t), Some(path)) if t == name => format!("{name} {path}"),
                    (Some(t), _) => t.to_string(),
                    (None, _) => name.clone(),
                };
                let locations: Vec<Value> = tools::touched_paths(&name, input, cwd)
                    .into_iter()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(|p| json!({"path": p.display().to_string()}))
                    .collect();
                out.push(json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": id,
                    "title": title,
                    "kind": tool_kind(&name),
                    "status": "pending",
                    "rawInput": input,
                    "locations": locations,
                }));
                state.open.push(OpenCall {
                    id,
                    diffs: Vec::new(),
                });
            }
        }
        "diff" => {
            if let Some(open) = state.open.last_mut() {
                let old = text("old_text");
                open.diffs.push(json!({
                    "type": "diff",
                    "path": text("path"),
                    "oldText": if old.is_empty() { Value::Null } else { Value::String(old) },
                    "newText": text("new_text"),
                }));
                out.push(json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": open.id,
                    "status": "in_progress",
                    "content": open.diffs,
                }));
            }
        }
        "tool_result" => {
            if !is_control_tool(&text("name")) {
                if let Some(open) = state.open.pop() {
                    let failed = ev["is_error"] == true;
                    let mut content = open.diffs;
                    // An applied diff says what happened; the "edited x"
                    // line under it would only repeat it.
                    if content.is_empty() || failed {
                        content.push(text_content(&clip(&text("content"))));
                    }
                    out.push(json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": open.id,
                        "status": if failed { "failed" } else { "completed" },
                        "content": content,
                    }));
                }
            }
        }
        "tool_denied" => {
            let reason = text("reason");
            match state.open.pop() {
                Some(open) => out.push(json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": open.id,
                    "status": "failed",
                    "content": [text_content(&reason)],
                })),
                // Refused before it was announced (arguments that did not
                // parse): one failed call says so.
                None => out.push(json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": next_call_id(),
                    "title": "tool call refused",
                    "kind": "other",
                    "status": "failed",
                    "content": [text_content(&reason)],
                })),
            }
        }
        "plan" => {
            let entries: Vec<Value> = ev["steps"]
                .as_array()
                .map(|steps| {
                    steps
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|s| json!({"content": s, "priority": "medium", "status": "pending"}))
                        .collect()
                })
                .unwrap_or_default();
            out.push(json!({"sessionUpdate": "plan", "entries": entries}));
        }
        "todos" => {
            let entries: Vec<Value> = ev["items"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|i| {
                            json!({
                                "content": i["task"],
                                "priority": "medium",
                                "status": plan_status(i["status"].as_str().unwrap_or("")),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.push(json!({"sessionUpdate": "plan", "entries": entries}));
        }
        // info is chrome; verify, finding and result have no ACP form.
        _ => {}
    }
    if !out.is_empty() {
        state.after_text = is_text;
    }
    out
}

// The report sink: events of the running turn become its session updates;
// outside a turn they are logged on stderr.
fn on_event(ev: Value) {
    let mut turn = lock(&shared().turn);
    let Some(t) = turn.as_mut() else {
        if let Some(m) = ev["message"].as_str() {
            eprintln!("buildwithnexus: {}", m.trim());
        }
        return;
    };
    if ev["type"] == "info" {
        if let Some(m) = ev["message"].as_str() {
            eprintln!("buildwithnexus: {}", m.trim());
        }
    }
    let cwd = t.cwd.clone();
    for u in updates_for(&ev, &mut t.events, &cwd) {
        session_update(&t.session_id, u);
    }
}

// Replays a saved conversation: what was said, and each tool call with its
// result. Call ids are prefixed with the message index, so the same model
// id in two turns stays two calls.
fn replay_updates(msgs: &[Msg]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut ids: HashMap<String, String> = HashMap::new();
    let chunk = |kind: &str, content: Value| json!({"sessionUpdate": kind, "content": content});
    for (i, m) in msgs.iter().enumerate() {
        match m {
            Msg::System(_) => {}
            Msg::User(t) => out.push(chunk(
                "user_message_chunk",
                json!({"type": "text", "text": t}),
            )),
            Msg::UserImages { text, images } => {
                out.push(chunk(
                    "user_message_chunk",
                    json!({"type": "text", "text": text}),
                ));
                for (mime, data) in images {
                    out.push(chunk(
                        "user_message_chunk",
                        json!({"type": "image", "mimeType": mime, "data": data}),
                    ));
                }
            }
            Msg::Assistant { text, calls } => {
                if !text.trim().is_empty() {
                    out.push(chunk(
                        "agent_message_chunk",
                        json!({"type": "text", "text": text}),
                    ));
                }
                ids.clear();
                for c in calls {
                    if c.name == "finish" {
                        if let Some(s) = c.input["summary"].as_str() {
                            out.push(chunk(
                                "agent_message_chunk",
                                json!({"type": "text", "text": s}),
                            ));
                        }
                        continue;
                    }
                    if is_control_tool(&c.name) {
                        continue;
                    }
                    let id = format!("{i}-{}", c.id);
                    ids.insert(c.id.clone(), id.clone());
                    out.push(json!({
                        "sessionUpdate": "tool_call",
                        "toolCallId": id,
                        "title": tools::preview(&c.name, &c.input),
                        "kind": tool_kind(&c.name),
                        "status": "pending",
                        "rawInput": c.input,
                    }));
                }
            }
            Msg::Tool(results) => {
                for r in results {
                    let Some(id) = ids.get(&r.id) else { continue };
                    out.push(json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": id,
                        "status": if r.is_error { "failed" } else { "completed" },
                        "content": [text_content(&clip(&r.content))],
                    }));
                }
            }
        }
    }
    out
}

// ── questions to the editor ─────────────────────────────────────────────────

fn option(id: &str, name: &str, kind: &str) -> Value {
    json!({"optionId": id, "name": name, "kind": kind})
}

// The id of the selected option, or None when the question was cancelled or
// the editor could not answer.
fn selected_option(answer: &Result<Value, String>) -> Option<String> {
    let outcome = &answer.as_ref().ok()?["outcome"];
    (outcome["outcome"] == "selected")
        .then(|| outcome["optionId"].as_str().map(String::from))
        .flatten()
}

// What `ask_editor` got: the chosen option (None: the turn was cancelled,
// or the editor is gone), and the call it made to carry a question that is
// not about a tool call, for `close_question`.
struct Asked {
    option: Option<String>,
    made: Option<(String, String)>,
}

// Asks the person through session/request_permission. A `call` without a
// kind is the gate asking about the call just announced; one with a kind
// (a plan, a trust question) is announced as a call of its own.
fn ask_editor(call: Value, options: Vec<Value>) -> Asked {
    let none = Asked {
        option: None,
        made: None,
    };
    let (sid, call_id, announce) = {
        let mut turn = lock(&shared().turn);
        let Some(t) = turn.as_mut().filter(|t| !t.cancelled) else {
            return none;
        };
        match (t.events.open.last(), call["kind"].is_null()) {
            (Some(open), true) => (t.session_id.clone(), open.id.clone(), false),
            (None, true) => {
                // Asked before anything was announced (a harness recovery
                // step): this call carries the question and the result.
                let id = next_call_id();
                t.events.open.push(OpenCall {
                    id: id.clone(),
                    diffs: Vec::new(),
                });
                (t.session_id.clone(), id, true)
            }
            (_, false) => (t.session_id.clone(), next_call_id(), true),
        }
    };
    let own_call = !call["kind"].is_null();
    let mut tool_call = call;
    tool_call["toolCallId"] = json!(call_id);
    if announce {
        let mut first = tool_call.clone();
        first["sessionUpdate"] = json!("tool_call");
        first["status"] = json!("pending");
        if first["kind"].is_null() {
            first["kind"] = json!("other");
        }
        session_update(&sid, first);
    }
    let answer = request(
        "session/request_permission",
        json!({"sessionId": sid, "toolCall": tool_call, "options": options}),
    );
    let cancelled = lock(&shared().turn).as_ref().is_none_or(|t| t.cancelled);
    if let (Err(e), false) = (&answer, cancelled) {
        eprintln!("buildwithnexus: session/request_permission: {e}");
    }
    Asked {
        option: if cancelled {
            None
        } else {
            selected_option(&answer)
        },
        made: own_call.then_some((sid, call_id)),
    }
}

// Closes a call made only to carry a question.
fn close_question(made: Option<(String, String)>, accepted: bool) {
    if let Some((sid, id)) = made {
        session_update(
            &sid,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": id,
                   "status": if accepted { "completed" } else { "failed" }}),
        );
    }
}

// The remote approver: the gate's prompt and the plan approval.
fn on_question(q: RemoteAsk) -> RemoteAnswer {
    match q {
        RemoteAsk::Tool { label, key } => {
            let mut options = vec![option("allow_once", "Allow once", "allow_once")];
            if !key.is_empty() {
                options.push(option(
                    "allow_always",
                    &format!("Always allow `{key}` in this project"),
                    "allow_always",
                ));
            }
            options.push(option("reject_once", "Reject", "reject_once"));
            match ask_editor(json!({"title": label}), options)
                .option
                .as_deref()
            {
                Some("allow_once") => RemoteAnswer::Once,
                Some("allow_always") => RemoteAnswer::Always,
                Some(_) => RemoteAnswer::Reject,
                None => RemoteAnswer::Cancelled,
            }
        }
        RemoteAsk::Plan { steps } => {
            let plan: String = steps
                .iter()
                .enumerate()
                .map(|(i, s)| format!("{}. {s}\n", i + 1))
                .collect();
            let asked = ask_editor(
                json!({"title": "Build this plan?", "kind": "switch_mode",
                       "content": [text_content(&plan)]}),
                vec![
                    option(
                        "build",
                        "Yes, switch to Build and implement it",
                        "allow_once",
                    ),
                    option("keep_planning", "No, keep planning", "reject_once"),
                ],
            );
            let approved = asked.option.as_deref() == Some("build");
            close_question(asked.made, approved);
            match asked.option {
                Some(_) if approved => {
                    switch_turn_mode(Mode::Build);
                    RemoteAnswer::Once
                }
                Some(_) => RemoteAnswer::Reject,
                None => RemoteAnswer::Cancelled,
            }
        }
    }
}

// The agent changed the running session's mode (an approved plan builds).
fn switch_turn_mode(mode: Mode) {
    let Some(sid) = lock(&shared().turn).as_ref().map(|t| t.session_id.clone()) else {
        return;
    };
    if let Some(s) = lock(&shared().sessions).get_mut(&sid) {
        s.mode = mode;
    }
    session_update(
        &sid,
        json!({"sessionUpdate": "current_mode_update", "currentModeId": mode_id(mode)}),
    );
}

// A trust question from hooks::trust_project_remote. A yes is remembered in
// trusted.json, as the terminal's is.
fn ask_trust(question: &str, details: &[String]) -> bool {
    let asked = ask_editor(
        json!({"title": question, "kind": "other", "content": [text_content(&details.join("\n"))]}),
        vec![
            option("trust", "Trust", "allow_always"),
            option("ignore", "Don't trust", "reject_once"),
        ],
    );
    let yes = asked.option.as_deref() == Some("trust");
    close_question(asked.made, yes);
    yes
}

// ── files through the editor ────────────────────────────────────────────────

struct EditorFiles;

fn turn_session() -> Option<String> {
    lock(&shared().turn).as_ref().map(|t| t.session_id.clone())
}

impl tools::EditorFiles for EditorFiles {
    fn read(&self, path: &Path) -> Option<Result<String, String>> {
        if !shared().client_reads.load(Ordering::Relaxed) {
            return None;
        }
        let sid = turn_session()?;
        let r = request(
            "fs/read_text_file",
            json!({"sessionId": sid, "path": path.display().to_string()}),
        );
        Some(r.and_then(|v| {
            v["content"]
                .as_str()
                .map(String::from)
                .ok_or_else(|| "the editor sent no content".to_string())
        }))
    }

    fn write(&self, path: &Path, contents: &str) -> Option<Result<(), String>> {
        if !shared().client_writes.load(Ordering::Relaxed) {
            return None;
        }
        let sid = turn_session()?;
        let r = request(
            "fs/write_text_file",
            json!({"sessionId": sid, "path": path.display().to_string(), "content": contents}),
        );
        Some(r.map(|_| ()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ToolCall, ToolResult};

    fn ev(v: Value) -> Value {
        v
    }

    #[test]
    fn a_tool_call_its_diff_and_result_become_one_call_with_the_diff() {
        let mut st = EventState::default();
        let cwd = Path::new("/w");
        let call = updates_for(
            &ev(
                json!({"type": "tool_call", "name": "edit_file", "title": "edit a.txt",
                       "input": {"path": "a.txt", "old": "x", "new": "y"}}),
            ),
            &mut st,
            cwd,
        );
        assert_eq!(call.len(), 1);
        assert_eq!(call[0]["sessionUpdate"], "tool_call");
        assert_eq!(call[0]["kind"], "edit");
        assert_eq!(call[0]["title"], "edit a.txt");
        assert_eq!(call[0]["locations"][0]["path"], "/w/a.txt");
        let id = call[0]["toolCallId"].clone();
        let diff = updates_for(
            &ev(json!({"type": "diff", "path": "/w/a.txt", "old_text": "x\n", "new_text": "y\n"})),
            &mut st,
            cwd,
        );
        assert_eq!(diff[0]["toolCallId"], id);
        assert_eq!(diff[0]["content"][0]["oldText"], "x\n");
        let done = updates_for(
            &ev(
                json!({"type": "tool_result", "name": "edit_file", "content": "edited", "is_error": false}),
            ),
            &mut st,
            cwd,
        );
        assert_eq!(done[0]["status"], "completed");
        assert_eq!(done[0]["content"].as_array().unwrap().len(), 1, "{done:?}");
        assert_eq!(done[0]["content"][0]["type"], "diff");
        assert!(st.open.is_empty());
    }

    #[test]
    fn a_new_file_diff_has_no_old_text_and_a_failure_keeps_its_message() {
        let mut st = EventState::default();
        let cwd = Path::new("/w");
        updates_for(
            &json!({"type": "tool_call", "name": "write_file", "input": {"path": "n.txt", "content": "z"}}),
            &mut st,
            cwd,
        );
        let d = updates_for(
            &json!({"type": "diff", "path": "/w/n.txt", "old_text": "", "new_text": "z"}),
            &mut st,
            cwd,
        );
        assert!(d[0]["content"][0]["oldText"].is_null());
        let r = updates_for(
            &json!({"type": "tool_result", "name": "write_file", "content": "disk full", "is_error": true}),
            &mut st,
            cwd,
        );
        assert_eq!(r[0]["status"], "failed");
        assert_eq!(r[0]["content"][1]["content"]["text"], "disk full");
    }

    #[test]
    fn a_helpers_calls_nest_inside_its_own_call_and_each_one_ends() {
        let mut st = EventState::default();
        let cwd = Path::new("/w");
        let outer = updates_for(
            &json!({"type": "tool_call", "name": "spawn_subagent", "title": "subagent: read a",
                    "input": {"task": "read a"}}),
            &mut st,
            cwd,
        );
        let inner = updates_for(
            &json!({"type": "tool_call", "name": "read_file", "input": {"path": "a"}}),
            &mut st,
            cwd,
        );
        let inner_done = updates_for(
            &json!({"type": "tool_result", "name": "read_file", "content": "x", "is_error": false}),
            &mut st,
            cwd,
        );
        assert_eq!(inner_done[0]["toolCallId"], inner[0]["toolCallId"]);
        let outer_done = updates_for(
            &json!({"type": "tool_result", "name": "spawn_subagent", "content": "it says x",
                    "is_error": false}),
            &mut st,
            cwd,
        );
        assert_eq!(outer_done.len(), 1, "the helper's call never ended");
        assert_eq!(outer_done[0]["toolCallId"], outer[0]["toolCallId"]);
        assert_eq!(outer_done[0]["status"], "completed");
        assert!(st.open.is_empty());
    }

    #[test]
    fn denials_fail_the_open_call_or_stand_alone() {
        let mut st = EventState::default();
        let cwd = Path::new("/w");
        let c = updates_for(
            &json!({"type": "tool_call", "name": "run_command", "input": {"command": "ls"}}),
            &mut st,
            cwd,
        );
        assert_eq!(c[0]["kind"], "execute");
        let d = updates_for(
            &json!({"type": "tool_denied", "reason": "denied by user"}),
            &mut st,
            cwd,
        );
        assert_eq!(d[0]["toolCallId"], c[0]["toolCallId"]);
        assert_eq!(d[0]["status"], "failed");
        let alone = updates_for(
            &json!({"type": "tool_denied", "reason": "bad args"}),
            &mut st,
            cwd,
        );
        assert_eq!(alone[0]["sessionUpdate"], "tool_call");
        assert_eq!(alone[0]["status"], "failed");
    }

    #[test]
    fn control_calls_are_not_shown_but_their_summary_and_plans_are() {
        let mut st = EventState::default();
        let cwd = Path::new("/w");
        assert!(updates_for(
            &json!({"type": "tool_call", "name": "finish", "input": {}}),
            &mut st,
            cwd
        )
        .is_empty());
        assert!(updates_for(
            &json!({"type": "tool_result", "name": "finish", "content": "x", "is_error": false}),
            &mut st,
            cwd
        )
        .is_empty());
        let f = updates_for(
            &json!({"type": "finish", "summary": "all done"}),
            &mut st,
            cwd,
        );
        assert_eq!(f[0]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(f[0]["content"]["text"], "all done");
        let p = updates_for(&json!({"type": "plan", "steps": ["a", "b"]}), &mut st, cwd);
        assert_eq!(p[0]["entries"][1]["content"], "b");
        assert_eq!(p[0]["entries"][1]["status"], "pending");
        let t = updates_for(
            &json!({"type": "todos", "items": [{"task": "x", "status": "completed"}, {"task": "y", "status": "in_progress"}]}),
            &mut st,
            cwd,
        );
        assert_eq!(t[0]["entries"][0]["status"], "completed");
        assert_eq!(t[0]["entries"][1]["status"], "in_progress");
        assert!(
            updates_for(&json!({"type": "info", "message": "chrome"}), &mut st, cwd).is_empty()
        );
    }

    #[test]
    fn streamed_text_continues_and_other_text_starts_a_paragraph() {
        let mut st = EventState::default();
        let cwd = Path::new("/w");
        let a = updates_for(
            &json!({"type": "assistant_delta", "text": "Hel"}),
            &mut st,
            cwd,
        );
        let b = updates_for(
            &json!({"type": "assistant_delta", "text": "lo"}),
            &mut st,
            cwd,
        );
        assert_eq!(a[0]["content"]["text"], "Hel");
        assert_eq!(b[0]["content"]["text"], "lo");
        let n = updates_for(
            &json!({"type": "notice", "message": "  ⚠ budget reached"}),
            &mut st,
            cwd,
        );
        assert_eq!(n[0]["content"]["text"], "\n\n⚠ budget reached");
        let t = updates_for(
            &json!({"type": "thinking_delta", "text": "hmm"}),
            &mut st,
            cwd,
        );
        assert_eq!(t[0]["sessionUpdate"], "agent_thought_chunk");
        let c = updates_for(
            &json!({"type": "assistant_delta", "text": "Next"}),
            &mut st,
            cwd,
        );
        assert_eq!(c[0]["content"]["text"], "Next");
    }

    #[test]
    fn prompt_blocks_become_task_text_images_and_notes() {
        let p = prompt_input(&[
            json!({"type": "text", "text": "fix the bug"}),
            json!({"type": "resource", "resource": {"uri": "file:///w/a.py", "text": "print(1)"}}),
            json!({"type": "resource_link", "uri": "file:///w/b.py", "name": "b.py"}),
            json!({"type": "image", "mimeType": "image/png", "data": "iVBOR"}),
            json!({"type": "resource", "resource": {"uri": "file:///w/c.png", "mimeType": "image/png", "blob": "AAAA"}}),
            json!({"type": "resource", "resource": {"uri": "file:///w/d.bin", "blob": "AAAA"}}),
            json!({"type": "audio", "mimeType": "audio/wav", "data": "UklG"}),
        ]);
        assert_eq!(
            p.text,
            "fix the bug\n\n<context uri=\"file:///w/a.py\">\nprint(1)\n</context>\n\nReferenced file: /w/b.py"
        );
        assert_eq!(
            p.images,
            [
                ("image/png".to_string(), "iVBOR".to_string()),
                ("image/png".to_string(), "AAAA".to_string())
            ]
        );
        assert_eq!(p.skipped.len(), 2, "{:?}", p.skipped);
        assert!(p.skipped[0].contains("d.bin"));
        assert!(p.skipped[1].contains("audio"));
    }

    #[test]
    fn a_saved_conversation_replays_messages_calls_and_results() {
        let msgs = vec![
            Msg::System("sys".into()),
            Msg::User("read a.txt".into()),
            Msg::Assistant {
                text: "Reading.".into(),
                calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "read_file".into(),
                    input: json!({"path": "a.txt"}),
                }],
            },
            Msg::Tool(vec![ToolResult {
                id: "c1".into(),
                content: "hello".into(),
                is_error: false,
                images: Vec::new(),
            }]),
            Msg::Assistant {
                text: String::new(),
                calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "finish".into(),
                    input: json!({"summary": "It says hello."}),
                }],
            },
        ];
        let u = replay_updates(&msgs);
        let kinds: Vec<&str> = u
            .iter()
            .map(|x| x["sessionUpdate"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "user_message_chunk",
                "agent_message_chunk",
                "tool_call",
                "tool_call_update",
                "agent_message_chunk"
            ]
        );
        assert_eq!(u[2]["toolCallId"], "2-c1");
        assert_eq!(u[3]["toolCallId"], "2-c1");
        assert_eq!(u[3]["content"][0]["content"]["text"], "hello");
        assert_eq!(u[4]["content"]["text"], "It says hello.");
    }

    #[test]
    fn editor_mcp_servers_take_the_settings_shape() {
        let mut notes = Vec::new();
        let m = editor_mcp_servers(
            &[
                json!({"name": "fs", "command": "/bin/fs", "args": ["--stdio"], "env": [{"name": "K", "value": "v"}]}),
                json!({"type": "http", "name": "api", "url": "https://x/mcp", "headers": [{"name": "Authorization", "value": "Bearer t"}]}),
                json!({"type": "sse", "name": "old", "url": "https://x/sse", "headers": []}),
            ],
            &mut |n| notes.push(n.to_string()),
        );
        assert_eq!(
            m["fs"],
            json!({"command": "/bin/fs", "args": ["--stdio"], "env": {"K": "v"}})
        );
        assert_eq!(m["api"]["headers"]["Authorization"], "Bearer t");
        assert!(mcp::parse_server("fs", &m["fs"]).is_ok());
        assert!(mcp::parse_server("api", &m["api"]).is_ok());
        assert!(!m.contains_key("old"));
        assert!(notes[0].contains("SSE"), "{notes:?}");
    }

    #[test]
    fn only_a_selected_option_is_an_answer() {
        let sel = Ok(json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}}));
        assert_eq!(selected_option(&sel).as_deref(), Some("allow_once"));
        let cancelled = Ok(json!({"outcome": {"outcome": "cancelled"}}));
        assert_eq!(selected_option(&cancelled), None);
        assert_eq!(selected_option(&Err("gone".into())), None);
    }
}
