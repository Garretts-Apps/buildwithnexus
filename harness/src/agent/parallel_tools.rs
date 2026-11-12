//! Read-only tool calls from one reply, run side by side.
//!
//! The build loop still checks, approves and reports every call in the
//! order the model gave, on the turn's own thread. Only the execution of a
//! run of consecutive read-only calls (reads, searches, fetches) is spread
//! over scoped threads, and their outcomes are recorded back in call order,
//! so the conversation and the saved session are the same as when they ran
//! one after another. See `build_turn` for where calls are queued.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use serde_json::Value;

use crate::{hooks, tools, trace};

/// How many read-only calls from one reply run at once when settings leave
/// `max_parallel_tools` out.
pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 4;
static MAX_PARALLEL_TOOLS: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_PARALLEL_TOOLS);

/// Sets `max_parallel_tools`; 0 and 1 both run calls one after another.
pub fn set_max_parallel_tools(n: usize) {
    MAX_PARALLEL_TOOLS.store(n.max(1), Ordering::Relaxed);
}

/// How many read-only calls from one reply run at once.
pub fn max_parallel_tools() -> usize {
    MAX_PARALLEL_TOOLS.load(Ordering::Relaxed)
}

/// Whether this call may wait to run beside its read-only neighbours. A
/// PostToolUse hook that matches it keeps it in line, so a hook sees each
/// result before the next call runs, as it always has.
pub(super) fn may_run_beside_others(name: &str, input: &Value) -> bool {
    is_parallel_safe(name, input) && !hooks::has_post_tool_use(name, input)
}

// Read-only, non-interactive tools that print nothing while they run and
// touch no turn state. Everything else runs in line.
fn is_parallel_safe(name: &str, input: &Value) -> bool {
    input.get(tools::INVALID_ARGS).is_none()
        && matches!(
            name,
            "read"
                | "read_file"
                | "read_many_files"
                | "list"
                | "list_dir"
                | "list_tree"
                | "file_info"
                | "glob"
                | "find_paths"
                | "find_files"
                | "grep"
                | "grep_files"
                | "kb_query"
                | "webfetch"
                | "fetch_url"
                | "websearch"
                | "web_search"
        )
}

/// A call that passed its gate and waits for the rest of its run of
/// read-only calls. `slot` is its place in the round's results.
pub(super) struct PendingCall {
    pub(super) slot: usize,
    pub(super) id: String,
    pub(super) name: String,
    pub(super) input: Value,
}

/// Runs the calls and returns their outcomes in the order given.
pub(super) fn run_pending(calls: &[PendingCall], cwd: &Path) -> Vec<tools::Outcome> {
    let started = std::time::Instant::now();
    let width = max_parallel_tools();
    let outs = run_with(calls, width, |c| tools::run(&c.name, &c.input, cwd));
    if calls.len() > 1 {
        trace::record(
            "tool_batch",
            format!("{} read-only calls side by side", calls.len()),
            serde_json::json!({
                "calls": calls.len(),
                "width": width.min(calls.len()),
                "tools": calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                "ms": started.elapsed().as_millis() as u64,
            }),
        );
    }
    outs
}

/// Runs read-only calls the way the build loop runs a batch of them, at
/// most `width` at once, and returns their outcomes in order. For the
/// benchmarks; the build loop queues calls itself and runs nothing here
/// that did not pass its gate.
pub fn run_read_only_calls(
    calls: &[(String, Value)],
    cwd: &Path,
    width: usize,
) -> Vec<tools::Outcome> {
    let calls: Vec<PendingCall> = calls
        .iter()
        .enumerate()
        .map(|(slot, (name, input))| PendingCall {
            slot,
            id: String::new(),
            name: name.clone(),
            input: input.clone(),
        })
        .collect();
    run_with(&calls, width, |c| tools::run(&c.name, &c.input, cwd))
}

/// The scheduler, with the work passed in so tests can run it without
/// tools. One call, or a width of one, runs on this thread; more run on at
/// most `width` scoped threads, each taking the next call in order. Calls
/// to the same network host never overlap, so a batch of fetches is never
/// a burst against one server.
pub(super) fn run_with<F>(calls: &[PendingCall], width: usize, exec: F) -> Vec<tools::Outcome>
where
    F: Fn(&PendingCall) -> tools::Outcome + Sync,
{
    let n = calls.len();
    if n <= 1 || width <= 1 {
        return calls.iter().map(&exec).collect();
    }
    let next = AtomicUsize::new(0);
    let hosts = HostLocks::default();
    let mut outs: Vec<Option<tools::Outcome>> = (0..n).map(|_| None).collect();
    std::thread::scope(|s| {
        let (tx, rx) = std::sync::mpsc::channel::<(usize, tools::Outcome)>();
        for _ in 0..width.min(n) {
            let tx = tx.clone();
            let (next, hosts, exec) = (&next, &hosts, &exec);
            s.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(call) = calls.get(i) else {
                    break;
                };
                let host = tools::network_host(&call.name, &call.input);
                let _held = host.as_deref().map(|h| hosts.hold(h));
                let out = exec(call);
                drop(_held);
                if tx.send((i, out)).is_err() {
                    break;
                }
            });
        }
        drop(tx);
        for (i, out) in rx {
            outs[i] = Some(out);
        }
    });
    // Every index was taken by exactly one worker and sent once; a worker
    // that panicked has already ended the process (panic = "abort"), or,
    // in tests, the scope above.
    outs.into_iter()
        .map(|o| o.expect("every pending call has an outcome"))
        .collect()
}

// Network hosts with a call in flight. `hold` waits until the host is free.
#[derive(Default)]
struct HostLocks {
    busy: Mutex<HashSet<String>>,
    freed: Condvar,
}

struct HostHeld<'a> {
    locks: &'a HostLocks,
    host: String,
}

impl HostLocks {
    fn hold(&self, host: &str) -> HostHeld<'_> {
        let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        while busy.contains(host) {
            busy = self.freed.wait(busy).unwrap_or_else(|e| e.into_inner());
        }
        busy.insert(host.to_string());
        HostHeld {
            locks: self,
            host: host.to_string(),
        }
    }
}

impl Drop for HostHeld<'_> {
    fn drop(&mut self) {
        let mut busy = self.locks.busy.lock().unwrap_or_else(|e| e.into_inner());
        busy.remove(&self.host);
        self.locks.freed.notify_all();
    }
}
