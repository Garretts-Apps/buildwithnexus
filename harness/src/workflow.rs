// Background workflow system. Workflows run the CLI binary as a subprocess so
// they have their own context and don't write to the foreground TUI's stdout.
// The REPL calls tick() each turn to check due schedules and collect status.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

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

#[derive(Clone, Debug)]
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
    active: Option<ActiveProcess>,
}

fn manager() -> &'static Mutex<Manager> {
    static M: OnceLock<Mutex<Manager>> = OnceLock::new();
    M.get_or_init(|| {
        Mutex::new(Manager {
            workflows: Vec::new(),
            next_id: 1,
            active: None,
        })
    })
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
    id
}

/// Cancel a workflow by ID. Kills the subprocess if running.
pub fn cancel(id: usize) -> bool {
    let mut m = manager().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(a) = &mut m.active {
        if a.id == id {
            let _ = a.child.kill();
            let a = m.active.take().unwrap();
            drop(a.child);
        }
    }
    if let Some(wf) = m.workflows.iter_mut().find(|w| w.id == id) {
        if matches!(wf.status, WorkflowStatus::Pending | WorkflowStatus::Running) {
            wf.status = WorkflowStatus::Cancelled;
            wf.finished_ms = Some(now_ms());
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

/// Returns the buffered output for a workflow (for inspect panel).
pub fn output(id: usize) -> Vec<String> {
    let m = manager().lock().unwrap_or_else(|e| e.into_inner());
    m.workflows
        .iter()
        .find(|w| w.id == id)
        .map(|w| w.output.clone())
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

/// Called by the REPL each turn. Returns a notification message if a workflow
/// just completed, so the REPL can surface it above the next prompt.
pub fn tick() -> Option<String> {
    let mut m = manager().lock().unwrap_or_else(|e| e.into_inner());
    let now = now_ms();
    let mut notification = None;

    // Check if the currently active subprocess has finished.
    if let Some(a) = &mut m.active {
        let finished = a.child.try_wait().ok().and_then(|s| s);
        if let Some(exit) = finished {
            let id = a.id;
            let buf_lines = a
                .output_buf
                .lock()
                .ok()
                .map(|b| b.clone())
                .unwrap_or_default();
            let ok = exit.success();
            let a = m.active.take().unwrap();
            drop(a.child);

            if let Some(wf) = m.workflows.iter_mut().find(|w| w.id == id) {
                wf.output.extend(buf_lines);
                wf.finished_ms = Some(now);
                let label: String = wf.task.chars().take(40).collect();
                if ok {
                    wf.status = WorkflowStatus::Done;
                    notification = Some(format!("  ✓ workflow #{id} done: {label}"));
                    // For Loop kind: schedule next iteration.
                    if let WorkflowKind::Loop { interval_secs } = wf.kind.clone() {
                        wf.status = WorkflowStatus::Pending;
                        wf.next_fire_ms = Some(now + interval_secs * 1000);
                        wf.started_ms = None;
                        wf.finished_ms = None;
                    }
                } else {
                    wf.status = WorkflowStatus::Failed("non-zero exit".to_string());
                    notification = Some(format!("  ✗ workflow #{id} failed: {label}"));
                }
            }
        }
    }

    // If nothing is running, try to start the next due workflow.
    if m.active.is_none() {
        let due_idx = m.workflows.iter().position(|w| {
            if !matches!(w.status, WorkflowStatus::Pending) {
                return false;
            }
            match &w.kind {
                WorkflowKind::Scheduled { fire_at_ms } => *fire_at_ms <= now,
                WorkflowKind::Loop { .. } => w.next_fire_ms.map(|f| f <= now).unwrap_or(true),
                WorkflowKind::Once => true,
            }
        });

        if let Some(idx) = due_idx {
            if let Ok(bin) = std::env::current_exe() {
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
                    m.active = Some(ActiveProcess {
                        id,
                        child,
                        output_buf: buf.clone(),
                    });
                } else {
                    m.workflows[idx].status = WorkflowStatus::Failed("failed to spawn".to_string());
                }
            }
        }
    }

    notification
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
