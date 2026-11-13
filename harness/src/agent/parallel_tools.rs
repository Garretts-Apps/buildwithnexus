//! Read-only tool calls from one reply, run side by side.
//!
//! The build loop still checks, approves and reports every call in the
//! order the model gave, on the turn's own thread. Only the execution of a
//! run of consecutive read-only calls is batched: its searches and fetches
//! are spread over scoped threads while its file reads, which cost less
//! than a thread, run on this one. Outcomes are recorded back in call order,
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
    tools::is_parallel_safe(name, input) && !hooks::has_post_tool_use(name, input)
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
    let (outs, ran) = run_counted(calls, max_parallel_tools(), |c| {
        tools::run(&c.name, &c.input, cwd)
    });
    // Only a batch that ran calls side by side is one worth a trace line.
    if ran.side_by_side > 0 {
        trace::record(
            "tool_batch",
            format!(
                "{} read-only calls, {} side by side",
                calls.len(),
                ran.side_by_side
            ),
            serde_json::json!({
                "calls": calls.len(),
                "side_by_side": ran.side_by_side,
                "width": ran.width,
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

/// What a batch did beyond running calls in line: how many calls shared
/// the threads, and how many threads (this one included) ran them. Both
/// are zero when every call ran on this thread, one after another.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct SideBySide {
    pub(super) side_by_side: usize,
    pub(super) width: usize,
}

/// The scheduler, with the work passed in so tests can run it without
/// tools. Calls that cost less than a thread (`tools::is_worth_a_thread`)
/// run on this thread, in order. The rest (fetches, searches of the tree)
/// are shared between at most `width` threads, this one among them, when
/// there are two or more; a lone one runs in line too. Calls to the same
/// network host never overlap, so a batch of fetches is never a burst
/// against one server.
pub(super) fn run_with<F>(calls: &[PendingCall], width: usize, exec: F) -> Vec<tools::Outcome>
where
    F: Fn(&PendingCall) -> tools::Outcome + Sync,
{
    run_counted(calls, width, exec).0
}

pub(super) fn run_counted<F>(
    calls: &[PendingCall],
    width: usize,
    exec: F,
) -> (Vec<tools::Outcome>, SideBySide)
where
    F: Fn(&PendingCall) -> tools::Outcome + Sync,
{
    let queue: Vec<usize> = if calls.len() > 1 && width > 1 {
        (0..calls.len())
            .filter(|&i| tools::is_worth_a_thread(&calls[i].name, &calls[i].input))
            .collect()
    } else {
        Vec::new()
    };
    let threads = width.min(queue.len());
    if threads <= 1 {
        return (calls.iter().map(&exec).collect(), SideBySide::default());
    }
    let next = AtomicUsize::new(0);
    let hosts = HostLocks::default();
    // The next queued call, run: None once the queue is empty.
    let take = || -> Option<(usize, tools::Outcome)> {
        let i = *queue.get(next.fetch_add(1, Ordering::Relaxed))?;
        let call = &calls[i];
        let host = tools::network_host(&call.name, &call.input);
        let _held = host.as_deref().map(|h| hosts.hold(h));
        Some((i, exec(call)))
    };
    let mut outs: Vec<Option<tools::Outcome>> = (0..calls.len()).map(|_| None).collect();
    std::thread::scope(|s| {
        let (tx, rx) = std::sync::mpsc::channel::<(usize, tools::Outcome)>();
        for _ in 1..threads {
            let (tx, take) = (tx.clone(), &take);
            s.spawn(move || {
                while let Some(done) = take() {
                    if tx.send(done).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        // This thread runs the cheap calls, then takes queued ones too.
        let mut queued = queue.iter().copied().peekable();
        for (i, call) in calls.iter().enumerate() {
            if queued.next_if_eq(&i).is_none() {
                outs[i] = Some(exec(call));
            }
        }
        while let Some((i, out)) = take() {
            outs[i] = Some(out);
        }
        for (i, out) in rx {
            outs[i] = Some(out);
        }
    });
    // Every call was run by exactly one thread; a worker that panicked has
    // already ended the process (panic = "abort"), or, in tests, the scope
    // above.
    let outs = outs
        .into_iter()
        .map(|o| o.expect("every pending call has an outcome"))
        .collect();
    let ran = SideBySide {
        side_by_side: queue.len(),
        width: threads,
    };
    (outs, ran)
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
