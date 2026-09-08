// Background workflow system. Workflows run the CLI binary as a subprocess so
// they have their own context and don't write to the foreground TUI's stdout.
// A scheduler thread (start_scheduler) calls tick() every second so due
// workflows start while the user is idle; the REPL also ticks at each prompt
// and drains the completion notices queued by the thread. Up to
// `max_concurrent_workflows` (default 2) run at once, and every run's
// stdout/stderr is written to ~/.buildwithnexus/workflows/<id>.log so
// `/workflows i<id>` can show it after a restart.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Debug, PartialEq)]
pub enum WorkflowStatus {
    Pending,
    Running,
    Done,
    Failed(String),
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WorkflowKind {
    Once,
    // Repeat with `interval_secs` between the end of one run and start of next.
    Loop { interval_secs: u64 },
    // One-shot, fires when wall clock >= fire_at_ms.
    Scheduled { fire_at_ms: u64 },
}

#[derive(Debug)]
pub struct Workflow {
    pub id: usize,
    pub task: String,
    pub kind: WorkflowKind,
    pub status: WorkflowStatus,
    pub output: Vec<String>,
    pub created_ms: u64,
    pub started_ms: Option<u64>,
    pub finished_ms: Option<u64>,
    pub iteration: u32,
    pub next_fire_ms: Option<u64>, // for Loop: when the next iteration fires
}

struct ActiveProcess {
    id: usize,
    child: Child,
    output_buf: Arc<Mutex<Vec<String>>>,
}

struct Manager {
    workflows: Vec<Workflow>,
    next_id: usize,
    active: Vec<ActiveProcess>,
}

fn manager() -> &'static Mutex<Manager> {
    static M: OnceLock<Mutex<Manager>> = OnceLock::new();
    M.get_or_init(|| {
        Mutex::new(Manager {
            workflows: Vec::new(),
            next_id: 1,
            active: Vec::new(),
        })
    })
}

// ── concurrency limit ─────────────────────────────────────────────────────────
pub const DEFAULT_MAX_CONCURRENT: usize = 2;
static MAX_CONCURRENT: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_CONCURRENT);

/// Sets how many workflows may run at once (`max_concurrent_workflows`);
/// zero is treated as one so the queue can never stall.
pub fn set_max_concurrent(n: usize) {
    MAX_CONCURRENT.store(n.max(1), Ordering::Relaxed);
}

pub fn max_concurrent() -> usize {
    MAX_CONCURRENT.load(Ordering::Relaxed)
}

// ── completion notices ────────────────────────────────────────────────────────
// The scheduler thread must not draw on the TUI while the user is typing at
// the prompt, so notices it produces are queued and shown at the next prompt.
fn notices() -> &'static Mutex<Vec<String>> {
    static N: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    N.get_or_init(|| Mutex::new(Vec::new()))
}

/// Notices queued by the scheduler thread since the last call.
pub fn take_notices() -> Vec<String> {
    let mut n = notices().lock().unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *n)
}

/// Starts the background scheduler: ticks once a second so due workflows
/// start (and finished ones are collected) while the REPL waits for input.
/// Idempotent — a second call is a no-op.
pub fn start_scheduler() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        thread::spawn(|| loop {
            thread::sleep(Duration::from_secs(1));
            let produced = tick();
            if !produced.is_empty() {
                if let Ok(mut n) = notices().lock() {
                    n.extend(produced);
                }
            }
        });
    });
}

// ── run logs ──────────────────────────────────────────────────────────────────
#[cfg(not(test))]
fn log_dir() -> PathBuf {
    crate::config::home().join("workflows")
}

#[cfg(test)]
fn log_dir() -> PathBuf {
    std::env::temp_dir().join(format!("bwn-workflow-logs-test-{}", std::process::id()))
}

fn log_path(id: usize) -> PathBuf {
    log_dir().join(format!("{id}.log"))
}

// Persist a finished run's captured stdout/stderr. Loop iterations append so
// the history of a repeating workflow survives; the header separates runs.
fn write_log(wf: &Workflow, lines: &[String]) {
    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let mut body = format!(
        "# workflow #{} iteration {} [{}] {}\n",
        wf.id,
        wf.iteration,
        status_label(&wf.status),
        wf.task
    );
    for l in lines {
        body.push_str(l);
        body.push('\n');
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(wf.id))
    {
        use std::io::Write;
        let _ = f.write_all(body.as_bytes());
    }
}

// Ids never collide with an earlier session's logs: start numbering past the
// highest `<id>.log` on disk.
fn highest_logged_id() -> usize {
    std::fs::read_dir(log_dir())
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    e.path()
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .and_then(|s| s.parse::<usize>().ok())
                })
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

// ── persistence ───────────────────────────────────────────────────────────────
// Pending /schedule and /loop workflows used to live only in process memory:
// quit, crash, or /resume and they vanished silently. They now persist to
// workflows.json on every mutation and are restored at interactive startup.
//
// Only the interactive REPL enables persistence (restore() flips the flag).
// Workflow runs are subprocesses of this same binary — if they also wrote the
// store file, every child would clobber the parent's queue.
static PERSIST: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Deserialize)]
struct PersistedWorkflow {
    task: String,
    kind: WorkflowKind,
    iteration: u32,
    next_fire_ms: Option<u64>,
}

#[cfg(not(test))]
fn store_path() -> PathBuf {
    crate::config::home().join("workflows.json")
}

#[cfg(test)]
fn store_path() -> PathBuf {
    std::env::temp_dir().join(format!("bwn-workflows-test-{}.json", std::process::id()))
}

// Atomic write (temp + rename), same discipline as session saves. An empty
// queue removes the file so stale state can't outlive its workflows.
fn save_pending_locked(m: &Manager) {
    if !PERSIST.load(Ordering::Relaxed) {
        return;
    }
    let pending: Vec<PersistedWorkflow> = m
        .workflows
        .iter()
        // Running counts as pending: the subprocess dies with this process,
        // so after a restart the run should fire again.
        .filter(|w| matches!(w.status, WorkflowStatus::Pending | WorkflowStatus::Running))
        .map(|w| PersistedWorkflow {
            task: w.task.clone(),
            kind: w.kind.clone(),
            iteration: w.iteration,
            next_fire_ms: w.next_fire_ms,
        })
        .collect();
    let path = store_path();
    if pending.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(body) = serde_json::to_string_pretty(&pending) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Loads workflows saved by a previous session and enables persistence for
/// this one. Called once from the interactive REPL; returns how many pending
/// workflows were restored so the UI can say so out loud.
pub fn restore() -> usize {
    if PERSIST.swap(true, Ordering::Relaxed) {
        return 0; // already restored in this process
    }
    let mut m = manager().lock().unwrap_or_else(|e| e.into_inner());
    m.next_id = m.next_id.max(highest_logged_id() + 1);
    let Ok(text) = std::fs::read_to_string(store_path()) else {
        return 0;
    };
    let Ok(list) = serde_json::from_str::<Vec<PersistedWorkflow>>(&text) else {
        return 0;
    };
    let mut restored = 0;
    for p in list {
        let id = m.next_id;
        m.next_id += 1;
        m.workflows.push(Workflow {
            id,
            task: p.task,
            kind: p.kind,
            status: WorkflowStatus::Pending,
            output: Vec::new(),
            created_ms: now_ms(),
            started_ms: None,
            finished_ms: None,
            iteration: p.iteration,
            next_fire_ms: p.next_fire_ms,
        });
        restored += 1;
    }
    save_pending_locked(&m);
    restored
}

// Human-readable label for a workflow kind.
pub fn kind_label(kind: &WorkflowKind) -> String {
    match kind {
        WorkflowKind::Once => "once".to_string(),
        WorkflowKind::Loop { interval_secs } => format!("loop {}s", interval_secs),
        WorkflowKind::Scheduled { fire_at_ms } => {
            let now = now_ms();
            if *fire_at_ms <= now {
                "scheduled (due)".to_string()
            } else {
                let secs = (*fire_at_ms - now) / 1000;
                format!("in {}s", secs)
            }
        }
    }
}

pub fn status_label(s: &WorkflowStatus) -> &'static str {
    match s {
        WorkflowStatus::Pending => "pending",
        WorkflowStatus::Running => "running",
        WorkflowStatus::Done => "done",
        WorkflowStatus::Failed(_) => "failed",
        WorkflowStatus::Cancelled => "cancelled",
    }
}

/// Add a workflow to the queue. Returns its ID.
pub fn enqueue(task: &str, kind: WorkflowKind) -> usize {
    let mut m = manager().lock().unwrap_or_else(|e| e.into_inner());
    let id = m.next_id;
    m.next_id += 1;
    let initial_status = match &kind {
        WorkflowKind::Scheduled { fire_at_ms } if *fire_at_ms > now_ms() => WorkflowStatus::Pending,
        WorkflowKind::Loop { .. } => WorkflowStatus::Pending,
        _ => WorkflowStatus::Pending,
    };
    m.workflows.push(Workflow {
        id,
        task: task.to_string(),
        kind,
        status: initial_status,
        output: Vec::new(),
        created_ms: now_ms(),
        started_ms: None,
        finished_ms: None,
        iteration: 0,
        next_fire_ms: None,
    });
    save_pending_locked(&m);
    id
}

/// Cancel a workflow by ID. Kills the subprocess if running.
pub fn cancel(id: usize) -> bool {
    let mut m = manager().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(pos) = m.active.iter().position(|a| a.id == id) {
        let mut a = m.active.remove(pos);
        let _ = a.child.kill();
        drop(a.child);
    }
    if let Some(wf) = m.workflows.iter_mut().find(|w| w.id == id) {
        if matches!(wf.status, WorkflowStatus::Pending | WorkflowStatus::Running) {
            wf.status = WorkflowStatus::Cancelled;
            wf.finished_ms = Some(now_ms());
            save_pending_locked(&m);
            return true;
        }
    }
    false
}

/// Returns a summary snapshot (task, kind, status, id) for the /workflows panel.
pub struct WorkflowSnapshot {
    pub id: usize,
    pub task: String,
    pub kind_str: String,
    pub status_str: String,
    pub iteration: u32,
    pub elapsed_secs: Option<u64>,
    pub output_lines: usize,
}

pub fn snapshots() -> Vec<WorkflowSnapshot> {
    let m = manager().lock().unwrap_or_else(|e| e.into_inner());
    let now = now_ms();
    m.workflows
        .iter()
        .map(|w| {
            let elapsed_secs = w.started_ms.map(|s| (now - s) / 1000);
            WorkflowSnapshot {
                id: w.id,
                task: w.task.chars().take(60).collect(),
                kind_str: kind_label(&w.kind),
                status_str: status_label(&w.status).to_string(),
                iteration: w.iteration,
                elapsed_secs,
                output_lines: w.output.len(),
            }
        })
        .collect()
}

/// Returns the buffered output for a workflow (for inspect panel), falling
/// back to its on-disk log so runs from a previous session stay inspectable.
pub fn output(id: usize) -> Vec<String> {
    let in_memory = {
        let m = manager().lock().unwrap_or_else(|e| e.into_inner());
        m.workflows
            .iter()
            .find(|w| w.id == id)
            .map(|w| w.output.clone())
            .unwrap_or_default()
    };
    if !in_memory.is_empty() {
        return in_memory;
    }
    std::fs::read_to_string(log_path(id))
        .map(|t| t.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Clear completed/cancelled workflows older than the last N kept.
pub fn prune(keep: usize) {
    let mut m = manager().lock().unwrap_or_else(|e| e.into_inner());
    let done: Vec<usize> = m
        .workflows
        .iter()
        .enumerate()
        .filter(|(_, w)| {
            matches!(
                w.status,
                WorkflowStatus::Done | WorkflowStatus::Failed(_) | WorkflowStatus::Cancelled
            )
        })
        .map(|(i, _)| i)
        .collect();
    if done.len() > keep {
        let to_remove = &done[..done.len() - keep];
        // Remove in reverse order so indices stay valid.
        for &i in to_remove.iter().rev() {
            m.workflows.remove(i);
        }
    }
}

// A pending workflow whose schedule has come due.
fn is_due(w: &Workflow, now: u64) -> bool {
    if !matches!(w.status, WorkflowStatus::Pending) {
        return false;
    }
    match &w.kind {
        WorkflowKind::Scheduled { fire_at_ms } => *fire_at_ms <= now,
        WorkflowKind::Loop { .. } => w.next_fire_ms.map(|f| f <= now).unwrap_or(true),
        WorkflowKind::Once => true,
    }
}

// Which workflows to start now: the due ones, oldest first, bounded by the
// free concurrency slots.
fn startable(workflows: &[Workflow], running: usize, limit: usize, now: u64) -> Vec<usize> {
    let slots = limit.saturating_sub(running);
    workflows
        .iter()
        .enumerate()
        .filter(|(_, w)| is_due(w, now))
        .map(|(i, _)| i)
        .take(slots)
        .collect()
}

// Records a finished subprocess on its workflow: output, status, the next
// loop iteration, and the on-disk log. Returns the notice to show.
fn complete(wf: &mut Workflow, lines: Vec<String>, ok: bool, now: u64) -> String {
    wf.output.extend(lines);
    wf.finished_ms = Some(now);
    let id = wf.id;
    let label: String = wf.task.chars().take(40).collect();
    wf.status = if ok {
        WorkflowStatus::Done
    } else {
        WorkflowStatus::Failed("non-zero exit".to_string())
    };
    // Log the run before a loop reschedule clears the buffer.
    let run_lines = std::mem::take(&mut wf.output);
    write_log(wf, &run_lines);
    wf.output = run_lines;
    if ok {
        // For Loop kind: schedule next iteration.
        if let WorkflowKind::Loop { interval_secs } = wf.kind.clone() {
            wf.status = WorkflowStatus::Pending;
            wf.next_fire_ms = Some(now + interval_secs * 1000);
            wf.started_ms = None;
            wf.finished_ms = None;
            wf.output.clear();
        }
        format!("  ✓ workflow #{id} done: {label}")
    } else {
        format!("  ✗ workflow #{id} failed: {label}")
    }
}

/// Called by the scheduler thread every second and by the REPL at each
/// prompt. Collects finished subprocesses and starts every due workflow that
/// fits under the concurrency limit. Returns one notice per completion.
pub fn tick() -> Vec<String> {
    let mut m = manager().lock().unwrap_or_else(|e| e.into_inner());
    let now = now_ms();
    let mut notifications = Vec::new();

    // Collect every active subprocess that has finished.
    let mut i = 0;
    while i < m.active.len() {
        let finished = m.active[i].child.try_wait().ok().and_then(|s| s);
        let Some(exit) = finished else {
            i += 1;
            continue;
        };
        let a = m.active.remove(i);
        let buf_lines = a
            .output_buf
            .lock()
            .ok()
            .map(|b| b.clone())
            .unwrap_or_default();
        let ok = exit.success();
        drop(a.child);
        if let Some(wf) = m.workflows.iter_mut().find(|w| w.id == a.id) {
            notifications.push(complete(wf, buf_lines, ok, now));
        }
    }

    // Start due workflows while there is a free slot.
    let mut started_or_failed = false;
    let to_start = startable(&m.workflows, m.active.len(), max_concurrent(), now);
    for idx in to_start {
        let Ok(bin) = std::env::current_exe() else {
            break;
        };
        let task = m.workflows[idx].task.clone();
        let id = m.workflows[idx].id;
        let buf: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let buf2 = buf.clone();
        let child_res = Command::new(&bin)
            .args(["run", "--json", &task])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        if let Ok(mut child) = child_res {
            // Drain stdout line-by-line in a reader thread.
            if let Some(stdout) = child.stdout.take() {
                let b = buf2.clone();
                thread::spawn(move || {
                    let reader = BufReader::new(stdout);
                    for l in reader.lines().map_while(Result::ok) {
                        if let Ok(mut bv) = b.lock() {
                            bv.push(l);
                        }
                    }
                });
            }
            // Drain stderr too so the child doesn't block.
            if let Some(stderr) = child.stderr.take() {
                let b = buf.clone();
                thread::spawn(move || {
                    let reader = BufReader::new(stderr);
                    for l in reader.lines().map_while(Result::ok) {
                        if let Ok(mut bv) = b.lock() {
                            bv.push(format!("[stderr] {l}"));
                        }
                    }
                });
            }
            m.workflows[idx].status = WorkflowStatus::Running;
            m.workflows[idx].started_ms = Some(now);
            m.workflows[idx].iteration += 1;
            m.active.push(ActiveProcess {
                id,
                child,
                output_buf: buf.clone(),
            });
        } else {
            m.workflows[idx].status = WorkflowStatus::Failed("failed to spawn".to_string());
        }
        started_or_failed = true;
    }

    // Completions, loop reschedules, starts (iteration bump), and spawn
    // failures all change persisted state — keep the store in sync.
    if !notifications.is_empty() || started_or_failed {
        save_pending_locked(&m);
    }

    notifications
}

/// How many workflows are currently pending or running.
pub fn active_count() -> usize {
    let m = manager().lock().unwrap_or_else(|e| e.into_inner());
    m.workflows
        .iter()
        .filter(|w| matches!(w.status, WorkflowStatus::Pending | WorkflowStatus::Running))
        .count()
}

/// Parse a delay string like "5s", "2m", "1h" → milliseconds from now.
/// Returns None if unparseable.
pub fn parse_delay(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('s') {
        n.trim().parse::<u64>().ok().map(|v| now_ms() + v * 1000)
    } else if let Some(n) = s.strip_suffix('m') {
        n.trim()
            .parse::<u64>()
            .ok()
            .map(|v| now_ms() + v * 60 * 1000)
    } else if let Some(n) = s.strip_suffix('h') {
        n.trim()
            .parse::<u64>()
            .ok()
            .map(|v| now_ms() + v * 3600 * 1000)
    } else {
        // bare number interpreted as seconds
        s.parse::<u64>().ok().map(|v| now_ms() + v * 1000)
    }
}

/// Parse an interval string like "30s", "5m" → seconds.
pub fn parse_interval_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('s') {
        n.trim().parse::<u64>().ok()
    } else if let Some(n) = s.strip_suffix('m') {
        n.trim().parse::<u64>().ok().map(|v| v * 60)
    } else if let Some(n) = s.strip_suffix('h') {
        n.trim().parse::<u64>().ok().map(|v| v * 3600)
    } else {
        s.parse::<u64>().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One combined test: the manager, the PERSIST flag, and the store file
    // are process-global, so splitting these steps into parallel tests would
    // race. Order inside is the lifecycle: save on enqueue → save on cancel →
    // restore from a prior session's file.
    #[test]
    fn pending_workflows_persist_and_restore_across_sessions() {
        let path = store_path();
        let _ = std::fs::remove_file(&path);

        // Simulate the previous session's leftovers: one future /schedule and
        // one /loop, written in the on-disk format.
        let prior = vec![
            PersistedWorkflow {
                task: "nightly summary".into(),
                kind: WorkflowKind::Scheduled {
                    fire_at_ms: now_ms() + 3_600_000,
                },
                iteration: 0,
                next_fire_ms: None,
            },
            PersistedWorkflow {
                task: "poll ci".into(),
                kind: WorkflowKind::Loop { interval_secs: 300 },
                iteration: 7,
                next_fire_ms: Some(now_ms() + 250_000),
            },
        ];
        std::fs::write(&path, serde_json::to_string(&prior).unwrap()).unwrap();

        // Interactive startup restores both and enables persistence.
        let restored = restore();
        assert_eq!(restored, 2);
        let snaps = snapshots();
        assert!(snaps.iter().any(|s| s.task == "nightly summary"));
        assert!(snaps
            .iter()
            .any(|s| s.task == "poll ci" && s.iteration == 7));
        assert_eq!(active_count(), 2);

        // A second restore in the same process is a no-op, not a duplicate.
        assert_eq!(restore(), 0);
        assert_eq!(active_count(), 2);

        // New enqueues are saved immediately…
        let id = enqueue(
            "new one",
            WorkflowKind::Scheduled {
                fire_at_ms: now_ms() + 3_600_000,
            },
        );
        let on_disk: Vec<PersistedWorkflow> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.len(), 3);

        // …and cancels drop them from the store.
        assert!(cancel(id));
        let on_disk: Vec<PersistedWorkflow> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.len(), 2);
        assert!(on_disk.iter().all(|p| p.task != "new one"));

        // Cancelling everything removes the file entirely.
        for s in snapshots() {
            let _ = cancel(s.id);
        }
        assert!(!path.exists(), "empty queue must not leave a stale store");
    }

    fn wf(
        id: usize,
        kind: WorkflowKind,
        status: WorkflowStatus,
        next_fire_ms: Option<u64>,
    ) -> Workflow {
        Workflow {
            id,
            task: format!("task {id}"),
            kind,
            status,
            output: Vec::new(),
            created_ms: 0,
            started_ms: None,
            finished_ms: None,
            iteration: 0,
            next_fire_ms,
        }
    }

    #[test]
    fn is_due_respects_kind_status_and_clock() {
        let now = 10_000;
        assert!(is_due(
            &wf(1, WorkflowKind::Once, WorkflowStatus::Pending, None),
            now
        ));
        assert!(!is_due(
            &wf(1, WorkflowKind::Once, WorkflowStatus::Running, None),
            now
        ));
        assert!(!is_due(
            &wf(1, WorkflowKind::Once, WorkflowStatus::Done, None),
            now
        ));
        let sched = |at| {
            wf(
                2,
                WorkflowKind::Scheduled { fire_at_ms: at },
                WorkflowStatus::Pending,
                None,
            )
        };
        assert!(is_due(&sched(now), now));
        assert!(is_due(&sched(now - 1), now));
        assert!(!is_due(&sched(now + 1), now));
        let lp = |next| {
            wf(
                3,
                WorkflowKind::Loop { interval_secs: 5 },
                WorkflowStatus::Pending,
                next,
            )
        };
        assert!(is_due(&lp(None), now), "first loop iteration fires at once");
        assert!(is_due(&lp(Some(now)), now));
        assert!(!is_due(&lp(Some(now + 500)), now));
    }

    #[test]
    fn startable_honours_concurrency_limit_oldest_first() {
        let now = 10_000;
        let list = vec![
            wf(1, WorkflowKind::Once, WorkflowStatus::Done, None),
            wf(2, WorkflowKind::Once, WorkflowStatus::Pending, None),
            wf(
                3,
                WorkflowKind::Scheduled {
                    fire_at_ms: now + 9_000,
                },
                WorkflowStatus::Pending,
                None,
            ),
            wf(4, WorkflowKind::Once, WorkflowStatus::Pending, None),
            wf(5, WorkflowKind::Once, WorkflowStatus::Pending, None),
        ];
        // Default limit of 2, nothing running: the two oldest due ones.
        assert_eq!(startable(&list, 0, DEFAULT_MAX_CONCURRENT, now), vec![1, 3]);
        // One slot taken: only one more starts.
        assert_eq!(startable(&list, 1, 2, now), vec![1]);
        // Saturated: nothing starts, even though work is due.
        assert!(startable(&list, 2, 2, now).is_empty());
        assert!(startable(&list, 5, 2, now).is_empty(), "never underflows");
        // A larger limit drains the whole due set in order.
        assert_eq!(startable(&list, 0, 8, now), vec![1, 3, 4]);
    }

    #[test]
    fn set_max_concurrent_never_stalls_the_queue() {
        set_max_concurrent(0);
        assert_eq!(max_concurrent(), 1);
        set_max_concurrent(3);
        assert_eq!(max_concurrent(), 3);
        set_max_concurrent(DEFAULT_MAX_CONCURRENT);
    }

    #[test]
    fn complete_writes_log_reschedules_loops_and_reads_back() {
        let _ = std::fs::remove_dir_all(log_dir());
        let now = 50_000;

        // A one-shot failure: status, notice, and the log on disk.
        let mut once = wf(901, WorkflowKind::Once, WorkflowStatus::Running, None);
        once.iteration = 1;
        let note = complete(
            &mut once,
            vec!["hello".into(), "[stderr] boom".into()],
            false,
            now,
        );
        assert!(note.contains('✗') && note.contains("#901"));
        assert!(matches!(once.status, WorkflowStatus::Failed(_)));
        let log = std::fs::read_to_string(log_path(901)).unwrap();
        assert!(log.starts_with("# workflow #901 iteration 1 [failed] task 901\n"));
        assert!(log.contains("hello\n[stderr] boom\n"));

        // A loop success reschedules and appends its next run to the same log.
        let mut lp = wf(
            902,
            WorkflowKind::Loop { interval_secs: 30 },
            WorkflowStatus::Running,
            None,
        );
        lp.iteration = 1;
        let note = complete(&mut lp, vec!["run one".into()], true, now);
        assert!(note.contains('✓'));
        assert!(matches!(lp.status, WorkflowStatus::Pending));
        assert_eq!(lp.next_fire_ms, Some(now + 30_000));
        assert!(
            lp.output.is_empty(),
            "loop buffer resets between iterations"
        );
        lp.iteration = 2;
        complete(&mut lp, vec!["run two".into()], true, now + 40_000);
        let log = std::fs::read_to_string(log_path(902)).unwrap();
        assert!(log.contains("iteration 1 [done]") && log.contains("run one"));
        assert!(log.contains("iteration 2 [done]") && log.contains("run two"));

        // output() falls back to the log when nothing is in memory (restart).
        let lines = output(901);
        assert!(lines.iter().any(|l| l == "hello"));
        assert!(lines.iter().any(|l| l == "[stderr] boom"));
        assert!(output(999_999).is_empty());

        // Fresh ids start past the highest logged id.
        assert_eq!(highest_logged_id(), 902);
        let _ = std::fs::remove_dir_all(log_dir());
        assert_eq!(highest_logged_id(), 0);
    }

    #[test]
    fn take_notices_drains_the_queue() {
        notices()
            .lock()
            .unwrap()
            .push("  ✓ workflow #7 done: x".into());
        let got = take_notices();
        assert_eq!(got.len(), 1);
        assert!(take_notices().is_empty());
    }

    #[test]
    fn parse_delay_seconds() {
        let now = now_ms();
        let result = parse_delay("30s").unwrap();
        assert!(result >= now + 29_000 && result <= now + 31_000);
    }

    #[test]
    fn parse_delay_minutes() {
        let now = now_ms();
        let result = parse_delay("2m").unwrap();
        assert!(result >= now + 119_000 && result <= now + 121_000);
    }

    #[test]
    fn parse_delay_hours() {
        let now = now_ms();
        let result = parse_delay("1h").unwrap();
        assert!(result >= now + 3_599_000 && result <= now + 3_601_000);
    }

    #[test]
    fn parse_delay_bare_number_is_seconds() {
        let now = now_ms();
        let result = parse_delay("60").unwrap();
        assert!(result >= now + 59_000 && result <= now + 61_000);
    }

    #[test]
    fn parse_delay_invalid_returns_none() {
        assert!(parse_delay("abc").is_none());
        assert!(parse_delay("").is_none());
    }

    #[test]
    fn parse_interval_secs_works() {
        assert_eq!(parse_interval_secs("30s"), Some(30));
        assert_eq!(parse_interval_secs("5m"), Some(300));
        assert_eq!(parse_interval_secs("1h"), Some(3600));
        assert_eq!(parse_interval_secs("45"), Some(45));
        assert_eq!(parse_interval_secs("bad"), None);
    }

    #[test]
    fn kind_label_once() {
        assert_eq!(kind_label(&WorkflowKind::Once), "once");
    }

    #[test]
    fn kind_label_loop() {
        assert_eq!(
            kind_label(&WorkflowKind::Loop { interval_secs: 60 }),
            "loop 60s"
        );
    }
}
