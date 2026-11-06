//! Code review: the reviewer roles, their findings, and the verdict a review reaches.

use super::*;

// ── review ───────────────────────────────────────────────────────────────────

// Set while `run_review` runs, so every refusal says why edits are off.
pub(super) static REVIEWING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
pub(super) const REVIEW_READONLY: &str =
    "review is read-only: it reports findings and changes nothing (use BUILD to apply a fix)";

/// A code review turn: read-only whatever the session permission is (in
/// auto too), with the researcher role. Returns the model's final answer,
/// which holds the findings.
pub fn run_review(
    p: &Provider,
    task: &str,
    cwd: &Path,
    transcript: &mut Vec<Msg>,
    sid: &str,
) -> Result<String, String> {
    crate::session::set_current(sid);
    REVIEWING.store(true, Ordering::Relaxed);
    let r = build_turn(
        p,
        Permission::ReadOnly,
        "researcher",
        task,
        cwd,
        0,
        transcript,
        Some(sid),
        Vec::new(),
    );
    let r = after_stop_hooks(
        "Stop",
        cwd,
        |_| serde_json::json!({}),
        r,
        |reason| {
            CONTINUING.with(|c| c.set(true));
            build_turn(
                p,
                Permission::ReadOnly,
                "researcher",
                reason,
                cwd,
                0,
                transcript,
                Some(sid),
                Vec::new(),
            )
        },
    );
    REVIEWING.store(false, Ordering::Relaxed);
    turn_done(cwd);
    crate::session::save(sid, cwd, &p.model, transcript);
    r
}

// What each running helper depth on this thread was started as: its agent
// file, if any, and whether it may only read. Per thread: helpers that run
// side by side each have a thread of their own, and a nested helper runs on
// its parent's.
#[derive(Clone, Default)]
pub(super) struct HelperCtx {
    pub(super) agent: Option<config::AgentDef>,
    pub(super) read_only: bool,
    // How a prompt from it names it: role and task.
    pub(super) label: String,
}

thread_local! {
    static HELPER_CTX: std::cell::RefCell<Vec<Option<HelperCtx>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

thread_local! {
    // The refused calls of each running helper depth on this thread, so its
    // parent hears that they were not done.
    static HELPER_REFUSED: std::cell::RefCell<Vec<Vec<String>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

pub(super) fn note_helper_refusal(depth: usize, what: &str) {
    HELPER_REFUSED.with(|r| {
        let mut r = r.borrow_mut();
        if r.len() <= depth {
            r.resize(depth + 1, Vec::new());
        }
        r[depth].push(what.to_string());
    });
}

pub(super) fn take_helper_refusals(depth: usize) -> Vec<String> {
    HELPER_REFUSED.with(|r| {
        r.borrow_mut()
            .get_mut(depth)
            .map(std::mem::take)
            .unwrap_or_default()
    })
}

// What the parent is told after a helper's own summary when some of its
// calls were refused: a summary can claim work that never happened.
pub(super) fn helper_refusal_note(refused: &[String]) -> Option<String> {
    let first = refused.first()?;
    let more = match refused.len() {
        1 => String::new(),
        n => format!(" and {} more", n - 1),
    };
    Some(format!(
        "\n[bwn: this helper's call was refused, so it was not done: {first}{more}. \
         Do not count on it.]"
    ))
}

pub(super) fn set_helper_ctx(depth: usize, ctx: Option<HelperCtx>) {
    HELPER_CTX.with(|a| {
        let mut a = a.borrow_mut();
        if a.len() <= depth {
            a.resize(depth + 1, None);
        }
        a[depth] = ctx;
    });
}

pub(super) fn helper_ctx(depth: usize) -> HelperCtx {
    if depth == 0 {
        return HelperCtx::default();
    }
    HELPER_CTX
        .with(|a| a.borrow().get(depth).cloned().flatten())
        .unwrap_or_default()
}

// Handing work to another helper. Never on a `tools` list's surface: that
// helper (an engineer, say) would have every tool, so the list would hold
// nothing back.
pub(super) const DELEGATION_TOOLS: &[&str] = &["task", "spawn_subagent"];

// Whether a helper with the `tools` list `allowed` may call `name`.
pub(super) fn agent_may_call(allowed: &[String], name: &str) -> bool {
    name == "finish" || (allowed.iter().any(|t| t == name) && !DELEGATION_TOOLS.contains(&name))
}

// The tool surface of an agent file with a `tools` list: those tools and
// finish, nothing else.
pub(super) fn agent_defs_only(
    defs: Vec<tools::ToolDef>,
    agent: &config::AgentDef,
) -> Vec<tools::ToolDef> {
    match &agent.tools {
        Some(allowed) => defs
            .into_iter()
            .filter(|d| agent_may_call(allowed, d.name))
            .collect(),
        None => defs,
    }
}

// Refusal for a call outside an agent file's `tools` list.
pub(super) fn agent_tool_refusal(agent: Option<&config::AgentDef>, name: &str) -> Option<String> {
    let agent = agent?;
    let allowed = agent.tools.as_ref()?;
    if agent_may_call(allowed, name) {
        return None;
    }
    Some(if DELEGATION_TOOLS.contains(&name) {
        format!(
            "the {} helper has a tools list, so it cannot hand work on to another helper — finish and say what is left",
            agent.name
        )
    } else {
        format!(
            "{name} is not one of the {} helper's tools ({}) — use those, or finish and say what is missing",
            agent.name,
            allowed.join(", ")
        )
    })
}

// A read-only helper's refusal of a call that would change something:
// whatever the session's permission, like read-only mode's own check.
// Clearly read-only shell commands (grep, git status…) still run.
pub(super) fn read_only_helper_refusal(
    name: &str,
    input: &serde_json::Value,
    cwd: &Path,
) -> Option<String> {
    if !tools::is_mutating_call(name, input) {
        return None;
    }
    let readonly_shell = tools::command_arg_for(name, input)
        .is_some_and(|c| tools::is_readonly_command(c) && tools::skips_prompt_safely(c, cwd));
    (!readonly_shell).then(|| {
        format!(
            "this helper is read-only, so {name} was not run — finish and say what should change"
        )
    })
}

// An agent file's system prompt: who it is, its own instructions, and how
// to end.
pub(super) fn agent_system(agent: &config::AgentDef) -> String {
    format!(
        "You are the `{}` helper: {}\n\n{}\n\nWork only on the task you are given. \
         When it is complete, call the finish tool with a one-paragraph summary.",
        agent.name, agent.description, agent.prompt
    )
}

pub(super) static SUB_SEQ: AtomicUsize = AtomicUsize::new(0);

/// How many helpers from one reply run at once when not set (`max_parallel_helpers`).
pub const DEFAULT_MAX_PARALLEL_HELPERS: usize = 3;
pub(super) static MAX_PARALLEL_HELPERS: AtomicUsize =
    AtomicUsize::new(DEFAULT_MAX_PARALLEL_HELPERS);

/// Sets `max_parallel_helpers`; zero counts as one (one at a time).
pub fn set_max_parallel_helpers(n: usize) {
    MAX_PARALLEL_HELPERS.store(n.max(1), Ordering::Relaxed);
}

/// `max_parallel_helpers` when settings leave it out. A local server often
/// answers one request at a time (OLLAMA_NUM_PARALLEL=1, one llama.cpp
/// slot), so helpers sent together queue there and time out waiting for
/// each other: on one, as many as it reports it answers at once, else one.
pub fn default_parallel_helpers(local: bool, slots: Option<usize>) -> usize {
    if local {
        slots.unwrap_or(1).clamp(1, DEFAULT_MAX_PARALLEL_HELPERS)
    } else {
        DEFAULT_MAX_PARALLEL_HELPERS
    }
}

/// How many helpers from one reply run at once.
pub fn max_parallel_helpers() -> usize {
    MAX_PARALLEL_HELPERS.load(Ordering::Relaxed)
}

/// What a `max_parallel_helpers` of 0 does, said at startup and by doctor:
/// 0 reads like "no limit", but it runs one helper at a time.
pub fn max_parallel_helpers_note(n: Option<usize>) -> Option<String> {
    (n == Some(0)).then(|| {
        format!(
            "max_parallel_helpers is 0, which runs helpers one at a time — there is no \
             unlimited setting; set a number such as {DEFAULT_MAX_PARALLEL_HELPERS}"
        )
    })
}

// A `task` / `spawn_subagent` call, checked and resolved: what the helper is
// asked, as which role or agent file, and whether it only reads or works in
// a worktree of its own (the two kinds that may run beside others).
pub(super) struct HelperSpec {
    pub(super) input: serde_json::Value,
    pub(super) task: String,
    pub(super) role: String,
    pub(super) agent: Option<config::AgentDef>,
    pub(super) read_only: bool,
    pub(super) isolate: bool,
}

// A helper ready to start: where it runs, and the worktree it got.
pub(super) struct Helper {
    pub(super) spec: HelperSpec,
    pub(super) run_cwd: PathBuf,
    pub(super) note: String,
    pub(super) worktree: Option<Worktree>,
}

impl Helper {
    // Read-only helpers and those in a worktree of their own touch nothing
    // another helper works on. A worktree does not cover folders added with
    // --add-dir, which every helper may write in. Under an editor (`bwn acp`)
    // helpers take turns: the editor nests each call inside the open one, so
    // two helpers' calls must not interleave.
    pub(super) fn runs_beside_others(&self) -> bool {
        !report::has_sink()
            && (self.spec.read_only || (self.worktree.is_some() && crate::workdirs::count() == 0))
    }
}

// Checks a delegation call and resolves its role, without side effects. A
// role names a built-in prompt or an agent file (resolved from the
// session's folder, before any worktree). A helper is read-only when the
// call or its agent file says so, when its agent file lists only tools that
// change nothing, or when the session itself is read-only.
pub(super) fn resolve_helper(
    perm: Permission,
    input: &serde_json::Value,
    cwd: &Path,
    depth: usize,
) -> Result<HelperSpec, String> {
    if depth + 1 >= MAX_DEPTH {
        return Err("subagent depth limit reached".into());
    }
    let task = input["task"]
        .as_str()
        .or_else(|| input["description"].as_str())
        .unwrap_or("")
        .trim();
    if task.is_empty() {
        return Err("spawn_subagent requires a task".into());
    }
    let role = input["role"].as_str().unwrap_or("engineer");
    let agent = if config::BUILTIN_ROLES.contains(&role) {
        None
    } else {
        let agents = config::load_agent_defs(cwd);
        match agents.iter().find(|a| a.name == role) {
            Some(a) => Some(a.clone()),
            None => {
                let mut known: Vec<String> = config::BUILTIN_ROLES
                    .iter()
                    .map(|r| r.to_string())
                    .collect();
                known.extend(agents.into_iter().map(|a| a.name));
                return Err(format!(
                    "unknown role '{role}' — use one of: {}",
                    known.join(", ")
                ));
            }
        }
    };
    Ok(HelperSpec {
        input: input.clone(),
        task: task.to_string(),
        role: role.to_string(),
        read_only: helper_reads_only(perm, input, agent.as_ref()),
        agent,
        isolate: input["isolate"].as_bool().unwrap_or(false),
    })
}

// Whether a helper may only read: the session is read-only, the call says
// `read_only: true`, or its agent file says so or lists only tools that
// change nothing.
pub(super) fn helper_reads_only(
    perm: Permission,
    input: &serde_json::Value,
    agent: Option<&config::AgentDef>,
) -> bool {
    // The editor tools write or only view by their `command`.
    let only_reads = |n: &String| {
        !tools::is_mutating(n)
            && !matches!(
                n.as_str(),
                "str_replace_editor" | "text_editor_20241022" | "text_editor_20250124"
            )
    };
    let lists_only_reads = agent.is_some_and(|a| {
        a.tools
            .as_ref()
            .is_some_and(|t| !t.is_empty() && t.iter().all(only_reads))
    });
    matches!(perm, Permission::ReadOnly)
        || input["read_only"].as_bool() == Some(true)
        || agent.is_some_and(|a| a.read_only)
        || lists_only_reads
}

// Gives a helper that asked for isolation its worktree; one that cannot have
// one runs in place, said before it writes anything.
pub(super) fn place_helper(spec: HelperSpec, cwd: &Path) -> Helper {
    // A helper that only reads needs no worktree, and making one is a write
    // (a branch, a folder, a checkout) a read-only session must not do.
    let (run_cwd, note, worktree) = if spec.isolate && !spec.read_only {
        match make_worktree(cwd) {
            Ok(wt) => {
                let n = format!("[isolated worktree: {}]\n", wt.path.display());
                (wt.path.clone(), n, Some(wt))
            }
            Err(why) => {
                report::notice(&format!("  {why} — the helper will write in your folder"));
                (
                    cwd.to_path_buf(),
                    format!("[worktree unavailable ({why}) — ran in place]\n"),
                    None,
                )
            }
        }
    } else {
        (cwd.to_path_buf(), String::new(), None)
    };
    Helper {
        spec,
        run_cwd,
        note,
        worktree,
    }
}

// Runs one helper on this thread. Returns its result and whether it failed
// — failures must reach the parent model as is_error so it can react
// instead of trusting bad output.
pub(super) fn run_helper(
    p: &Provider,
    perm: Permission,
    h: &Helper,
    cwd: &Path,
    depth: usize,
) -> (String, bool) {
    let HelperSpec {
        input,
        task,
        role,
        agent,
        read_only,
        isolate,
    } = &h.spec;
    report::info(&format!("  ↳ subagent: {}", trace::preview(task, 80)));
    trace::record_visible(
        "subagent_spawn",
        format!("{role}: {}", trace::preview(task, 80)),
        serde_json::json!({
            "task": task,
            "role": role,
            "isolate": isolate,
            "read_only": read_only,
            "cwd": h.run_cwd.to_string_lossy(),
            "parent_depth": depth,
        }),
    );
    let mut child: Vec<Msg> = Vec::new();
    take_helper_refusals(depth + 1);
    set_helper_ctx(
        depth + 1,
        Some(HelperCtx {
            agent: agent.clone(),
            read_only: *read_only,
            label: format!("{role} · {}", trace::preview(task, 60)),
        }),
    );
    let outcome = build_inner(p, perm, role, task, &h.run_cwd, depth + 1, &mut child, None);
    // SubagentStop carries the call and what the helper answered; a hook
    // may send the helper on, as Stop does for the turn.
    let outcome = after_stop_hooks(
        "SubagentStop",
        cwd,
        |r| {
            let (content, is_error) = match r {
                Ok(t) => (t.clone(), false),
                Err(e) => (format!("subagent error: {e}"), true),
            };
            serde_json::json!({
                "tool_name": "spawn_subagent",
                "tool_input": input,
                "tool_response": {"content": content, "is_error": is_error},
            })
        },
        outcome,
        |reason| {
            CONTINUING.with(|c| c.set(true));
            build_inner(
                p,
                perm,
                role,
                reason,
                &h.run_cwd,
                depth + 1,
                &mut child,
                None,
            )
        },
    );
    set_helper_ctx(depth + 1, None);
    let (mut result, is_error) = match outcome {
        // Esc or Ctrl+C ended it: say so, rather than an empty success.
        Ok(r) if r.is_empty() && tui::interrupted() => (HELPER_STOPPED.to_string(), true),
        Ok(r) => (r, false),
        Err(e) => (format!("subagent error: {e}"), true),
    };
    if let Some(note) = helper_refusal_note(&take_helper_refusals(depth + 1)) {
        result.push_str(&note);
    }
    let result = match &h.worktree {
        Some(wt) => {
            let done = finish_worktree(cwd, wt);
            report::subagent_result(task, done.branch.as_deref(), done.commits, &done.screen);
            format!("{result}\n{}", done.note)
        }
        None => result,
    };
    trace::record_visible(
        "subagent_done",
        format!("{role}: {}", trace::preview(task, 80)),
        serde_json::json!({
            "task": task,
            "role": role,
            "isolate": isolate,
            "read_only": read_only,
            "cwd": h.run_cwd.to_string_lossy(),
            "result": result,
            "is_error": is_error,
            "depth": depth + 1,
        }),
    );
    (format!("{}{result}", h.note), is_error)
}

pub(super) fn spawn_subagent(
    p: &Provider,
    perm: Permission,
    input: &serde_json::Value,
    cwd: &Path,
    depth: usize,
) -> (String, bool) {
    match resolve_helper(perm, input, cwd, depth) {
        Ok(spec) => run_helper(p, perm, &place_helper(spec, cwd), cwd, depth),
        Err(e) => (e, true),
    }
}

// Helpers started from one reply. Those that only read or have a worktree
// of their own run side by side, up to `max_parallel_helpers` at a time;
// one whose worktree could not be made writes in place, so it runs after
// them, on its own. Results come back in call order.
pub(super) fn run_helpers(
    p: &Provider,
    perm: Permission,
    specs: Vec<HelperSpec>,
    cwd: &Path,
    depth: usize,
) -> Vec<(String, bool)> {
    let total = specs.len();
    // Worktrees are made here, one after another: git takes its own locks.
    let placed: Vec<Helper> = specs.into_iter().map(|s| place_helper(s, cwd)).collect();
    let beside: Vec<usize> = (0..total)
        .filter(|&i| placed[i].runs_beside_others())
        .collect();
    let mut results: Vec<Option<(String, bool)>> = vec![None; total];
    if beside.len() > 1 {
        let group: Vec<&Helper> = beside.iter().map(|&i| &placed[i]).collect();
        for (k, r) in run_side_by_side(p, perm, &group, cwd, depth)
            .into_iter()
            .enumerate()
        {
            results[beside[k]] = Some(r);
        }
    }
    for i in 0..total {
        if results[i].is_some() {
            continue;
        }
        results[i] = Some(if turn_stopped(false) || tui::interrupted() {
            (HELPER_NOT_STARTED.into(), true)
        } else {
            run_helper(p, perm, &placed[i], cwd, depth)
        });
    }
    results
        .into_iter()
        .map(|r| r.unwrap_or_else(|| (HELPER_NOT_STARTED.into(), true)))
        .collect()
}

// A helper whose request timed out while others ran beside it may have
// waited behind them at a server that answers one request at a time.
pub(super) fn queued_timeout_note(
    (text, is_error): (String, bool),
    together: usize,
) -> (String, bool) {
    if !is_error || together < 2 || !text.contains("no answer from") {
        return (text, is_error);
    }
    (
        format!(
            "{text}\n[{together} helpers ran at the same time; a server that answers one \
             request at a time queues them, so this one may have waited for the others. \
             \"max_parallel_helpers\": 1 in settings runs them one after another.]"
        ),
        true,
    )
}

pub(super) const HELPER_NOT_STARTED: &str = "not started: the user stopped the turn";
pub(super) const HELPER_STOPPED: &str = "stopped by the user before it finished";

// A helper call held back to start with the others from its reply: the
// results slot it fills and the tool name it was called by.
pub(super) struct WaitingHelper {
    pub(super) slot: usize,
    pub(super) name: String,
    pub(super) spec: HelperSpec,
}

// Starts the held helpers and puts each one's result in its slot, in call
// order. None start once the user has stopped the turn.
pub(super) fn start_waiting_helpers(
    p: &Provider,
    perm: Permission,
    waiting: &mut Vec<WaitingHelper>,
    results: &mut [ToolResult],
    cwd: &Path,
    depth: usize,
) {
    let (calls, specs): (Vec<(usize, String)>, Vec<HelperSpec>) = std::mem::take(waiting)
        .into_iter()
        .map(|w| ((w.slot, w.name), w.spec))
        .unzip();
    let outs: Vec<(String, bool)> = if turn_stopped(false) || tui::interrupted() {
        specs
            .iter()
            .map(|_| (HELPER_NOT_STARTED.to_string(), true))
            .collect()
    } else {
        run_helpers(p, perm, specs, cwd, depth)
    };
    for ((slot, name), (out, is_error)) in calls.into_iter().zip(outs) {
        report::tool_result(&name, &out, is_error);
        trace_tool_result(&name, &out, is_error, "build", depth);
        if let Some(r) = results.get_mut(slot) {
            r.content = out;
            r.is_error = is_error;
        }
    }
}

// What a helper that ran beside others left: its result and its captured
// screen lines (or --json events), shown as one block when it finishes.
pub(super) struct HelperDone {
    pub(super) index: usize,
    pub(super) result: (String, bool),
    pub(super) lines: Vec<String>,
    pub(super) secs: f64,
    // Esc or Ctrl+C reached it.
    pub(super) interrupted: bool,
    // The person cancelled a prompt it asked.
    pub(super) stopped: bool,
}

// What helpers running side by side show outside their own block: an
// approval or a question one of them asks, and each finished block. Taken
// for each, so neither lands in the middle of the other.
pub(super) static HELPER_SCREEN: Mutex<()> = Mutex::new(());

pub(super) fn helper_screen() -> std::sync::MutexGuard<'static, ()> {
    HELPER_SCREEN.lock().unwrap_or_else(|e| e.into_inner())
}

// Runs `group` on worker threads, at most `max_parallel_helpers` at once.
// Each helper's output is held back and shown, labelled, when it finishes;
// Esc or Ctrl+C reaches every running helper (none of them clears it) and
// the queued ones never start. Tokens and cost add up in the session's
// usage ledger like any other request.
pub(super) fn run_side_by_side(
    p: &Provider,
    perm: Permission,
    group: &[&Helper],
    cwd: &Path,
    depth: usize,
) -> Vec<(String, bool)> {
    let n = group.len();
    let width = MAX_PARALLEL_HELPERS.load(Ordering::Relaxed).clamp(1, n);
    if !report::is_json() {
        let at_once = if width < n {
            format!("{n} helpers, {width} at a time")
        } else {
            format!("{n} helpers at once")
        };
        tui::line(&tui::accent(&format!(
            "  ❖ {at_once} — each shows its work when it finishes · Esc stops them all"
        )));
    }
    let next = AtomicUsize::new(0);
    let mut results: Vec<Option<(String, bool)>> = vec![None; n];
    std::thread::scope(|s| {
        let (tx, rx) = std::sync::mpsc::channel::<HelperDone>();
        for _ in 0..width {
            let tx = tx.clone();
            let next = &next;
            s.spawn(move || loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(h) = group.get(index) else {
                    break;
                };
                let started = std::time::Instant::now();
                let (result, lines) = if tui::interrupted() {
                    ((HELPER_NOT_STARTED.to_string(), true), Vec::new())
                } else {
                    tui::capture_start();
                    let r = queued_timeout_note(run_helper(p, perm, h, cwd, depth), width);
                    (r, tui::capture_take())
                };
                let done = HelperDone {
                    index,
                    result,
                    lines,
                    secs: started.elapsed().as_secs_f64(),
                    interrupted: tui::interrupted(),
                    // Taken: this thread may start another helper next.
                    stopped: turn_stopped(true),
                };
                if tx.send(done).is_err() {
                    break;
                }
            });
        }
        drop(tx);
        for done in rx {
            show_helper_block(group[done.index], done.index, n, &done);
            // A helper's cancelled prompt stops the whole turn.
            if done.stopped {
                set_turn_stopped(true);
            }
            results[done.index] = Some(done.result);
        }
    });
    results
        .into_iter()
        .map(|r| r.unwrap_or_else(|| (HELPER_NOT_STARTED.into(), true)))
        .collect()
}

// One finished helper's block: a header naming it, then what it showed
// while it ran. In --json mode its events, each with `helper` (its place
// among the helpers started together, from 1).
pub(super) fn show_helper_block(h: &Helper, index: usize, of: usize, done: &HelperDone) {
    let _screen = helper_screen();
    if report::is_json() {
        for l in &done.lines {
            match serde_json::from_str::<serde_json::Value>(l) {
                Ok(mut v) if v.is_object() => {
                    v["helper"] = (index + 1).into();
                    println!("{v}");
                }
                _ => println!("{l}"),
            }
        }
        return;
    }
    let (result, is_error) = &done.result;
    let how = if result == HELPER_NOT_STARTED {
        "not started"
    } else if done.interrupted {
        "stopped"
    } else if *is_error {
        "failed"
    } else {
        "done"
    };
    let head = format!(
        "  ❖ helper {} of {of} {how} · {} · {} · {:.1}s",
        index + 1,
        h.spec.role,
        trace::preview(&h.spec.task, 60),
        done.secs
    );
    let mut block = vec![if *is_error {
        tui::yellow(&tui::sanitize_terminal(&head))
    } else {
        tui::accent(&tui::sanitize_terminal(&head))
    }];
    let gutter = tui::dim("  │ ");
    block.extend(done.lines.iter().map(|l| format!("{gutter}{l}")));
    tui::line(&block.join("\n"));
}

// A helper asking the person something while others run: its block is not
// shown yet, so the prompt says which helper asks, and goes straight to the
// screen, one at a time. One that waited while the person stopped the turn
// at another helper's prompt asks nothing (`stopped`).
pub(super) fn helper_prompt<T>(ask: impl FnOnce() -> T, stopped: impl FnOnce() -> T) -> T {
    if !tui::capturing() {
        return ask();
    }
    let _screen = helper_screen();
    if tui::interrupted() {
        return stopped();
    }
    let who = HELPER_CTX
        .with(|a| a.borrow().iter().flatten().last().map(|h| h.label.clone()))
        .unwrap_or_default();
    tui::uncaptured(|| {
        tui::line(&tui::accent(&format!(
            "  ❖ helper ({}) asks:",
            tui::sanitize_terminal(&who)
        )));
        ask()
    })
}

pub(super) struct Worktree {
    pub(super) path: PathBuf,
    pub(super) branch: String,
    pub(super) base: String,
}

// Making and closing helpers' worktrees, one at a time: they share the
// repository's refs and worktree list, and helpers in worktrees of their own
// run at the same time.
pub(super) static WORKTREE_GIT: Mutex<()> = Mutex::new(());

// A worktree on a new branch from HEAD, or why there cannot be one.
pub(super) fn make_worktree(cwd: &Path) -> Result<Worktree, String> {
    let _git = WORKTREE_GIT.lock().unwrap_or_else(|e| e.into_inner());
    let id = SUB_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = cwd.join(format!(".bwn/worktrees/sub-{}-{id}", std::process::id()));
    let branch = format!("bwn-sub-{}-{id}", std::process::id());
    let base = git_out(cwd, &["rev-parse", "HEAD"])
        .ok_or("not a git repository with commits, so the helper cannot be isolated")?;
    // A checkout runs what the repository's config names (filters, an
    // fsmonitor) and its hooks; a repo that arrived with its .git must not
    // run them because a helper asked for isolation.
    if !tools::skips_prompt_safely("git status", cwd) {
        return Err(
            "this repository's git config can run programs (filters, fsmonitor), so no worktree was made"
                .into(),
        );
    }
    let no_hooks = format!("core.hooksPath={}", cwd.join(".bwn/no-hooks").display());
    let out = Command::new("git")
        .current_dir(cwd)
        .args(["-c", "core.fsmonitor=false", "-c", &no_hooks])
        .args(["worktree", "add", "-b", &branch])
        .arg(&path)
        .arg(&base)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("git could not start ({e})"))?;
    if out.status.success() {
        Ok(Worktree { path, branch, base })
    } else {
        let why = String::from_utf8_lossy(&out.stderr);
        Err(format!(
            "git could not make a worktree ({})",
            why.lines().last().unwrap_or("").trim()
        ))
    }
}

/// Does git know who the user is (config, GIT_AUTHOR_*/GIT_COMMITTER_*,
/// EMAIL)? Commits made for the user carry that identity when it does.
pub(crate) fn git_identity_known(dir: &Path) -> bool {
    git_out(dir, &["var", "GIT_AUTHOR_IDENT"]).is_some()
        && git_out(dir, &["var", "GIT_COMMITTER_IDENT"]).is_some()
}

// What became of an isolated helper's worktree: the note the model gets,
// the line the person sees, and the branch holding the work, if any.
pub(super) struct WorktreeDone {
    pub(super) note: String,
    pub(super) screen: String,
    pub(super) branch: Option<String>,
    pub(super) commits: usize,
}

pub(super) fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// Close an isolated subagent's worktree without losing its work, and say
// what became of it. Uncommitted edits are committed to the worktree's
// branch first, as the user when git knows them; if that fails the worktree
// stays where it is. A branch with no new commits is deleted, one with
// commits is kept and named.
pub(super) fn finish_worktree(cwd: &Path, wt: &Worktree) -> WorktreeDone {
    let _git = WORKTREE_GIT.lock().unwrap_or_else(|e| e.into_inner());
    let kept = |note: String, screen: String| WorktreeDone {
        note,
        screen,
        branch: Some(wt.branch.clone()),
        commits: 0,
    };
    let dirty = git_out(&wt.path, &["status", "--porcelain"]).map(|s| !s.is_empty());
    let mut saved = false;
    let mut as_bwn = false;
    if dirty != Some(false) {
        let add = git_out(&wt.path, &["add", "-A"]);
        // No hooks or signing, which could fail or prompt with no one to
        // answer; a local identity only when git has none for the user.
        let mut args = vec!["-c", "commit.gpgsign=false"];
        if !git_identity_known(&wt.path) {
            as_bwn = true;
            args.extend(["-c", "user.name=bwn", "-c", "user.email=bwn@localhost"]);
        }
        args.extend([
            "commit",
            "--no-verify",
            "-q",
            "-m",
            "bwn: uncommitted work from an isolated subagent",
        ]);
        if add.is_none() || git_out(&wt.path, &args).is_none() {
            return kept(
                format!(
                    "[isolated worktree kept: the subagent left uncommitted changes that could \
                     not be committed; they are in {} (branch {})]",
                    wt.path.display(),
                    wt.branch
                ),
                format!(
                    "the helper's uncommitted work is in {} (branch {})",
                    wt.path.display(),
                    wt.branch
                ),
            );
        }
        saved = true;
    }
    let range = format!("{}..{}", wt.base, wt.branch);
    let commits = git_out(cwd, &["rev-list", "--count", &range])
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(0);
    let removed = git_out(cwd, &["worktree", "remove", &wt.path.to_string_lossy()]).is_some();
    if !removed {
        return WorktreeDone {
            commits,
            ..kept(
                format!(
                    "[isolated worktree kept: {} could not be removed; its work is on branch {}]",
                    wt.path.display(),
                    wt.branch
                ),
                format!(
                    "the helper's work is on branch {} in {} — git merge {}",
                    wt.branch,
                    wt.path.display(),
                    wt.branch
                ),
            )
        };
    }
    if commits == 0 {
        let _ = git_out(cwd, &["branch", "-D", &wt.branch]);
        return WorktreeDone {
            note: "[isolated worktree removed: the subagent changed no files]".into(),
            screen: "the helper changed no files".into(),
            branch: None,
            commits: 0,
        };
    }
    let how = if saved {
        "including its uncommitted changes, committed for it"
    } else {
        "as the subagent committed it"
    };
    let author = if as_bwn {
        " (committed as bwn <bwn@localhost>: git has no identity for you)"
    } else {
        ""
    };
    WorktreeDone {
        note: format!(
            "[isolated worktree removed; its work is NOT in this checkout. {commits} commit(s) \
             on branch {} ({how}). Review with `git log -p {range}`, bring in with \
             `git merge {}`]",
            wt.branch, wt.branch
        ),
        screen: format!(
            "the helper's work is on branch {} — git merge {}{author}",
            wt.branch, wt.branch
        ),
        branch: Some(wt.branch.clone()),
        commits,
    }
}

pub(super) fn parse_plan_steps(plan_text: &str) -> Vec<String> {
    let mut listed = Vec::new();
    for line in plan_text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("```") {
            continue;
        }
        if let Some(step) = strip_plan_marker(trimmed) {
            let step = strip_markdown_emphasis(step);
            if !step.is_empty() {
                listed.push(step);
            }
        }
    }
    if !listed.is_empty() {
        return listed.into_iter().take(8).collect();
    }
    Vec::new()
}

pub(super) fn plan_steps_are_actionable(steps: &[String]) -> bool {
    !steps.is_empty() && steps.iter().all(|step| plan_step_is_actionable(step))
}

pub(super) fn is_exit_plan_tool(name: &str) -> bool {
    matches!(name, "exit_plan" | "ExitPlanMode")
}

pub(super) fn fallback_exit_plan_input(task: &str) -> serde_json::Value {
    let lower = task.to_ascii_lowercase();
    let read_only_request = lower.contains("do not modify")
        || lower.contains("don't modify")
        || lower.contains("readonly")
        || lower.contains("read-only")
        || (lower.contains("list") && !lower.contains("change") && !lower.contains("fix"));
    let steps = if let Some(input) = auto_create_file_input(task) {
        let path = input["path"].as_str().unwrap_or("the requested file");
        vec![
            format!("Create {path} with the requested content."),
            format!("Verify {path} exists and contains exactly the requested content."),
            "Summarize the completed file creation.".to_string(),
        ]
    } else if read_only_request {
        vec![
            "Inspect the current workspace with read-only tools.".to_string(),
            "List the requested files, folders, or findings.".to_string(),
            "Summarize the result without modifying files.".to_string(),
        ]
    } else {
        vec![
            format!("Inspect workspace and requirements for: {task}"),
            format!("Create and edit target source files to build: {task}"),
            format!("Verify execution and run checks/tests for: {task}"),
        ]
    };
    serde_json::json!({ "steps": steps })
}

pub(super) fn plan_step_is_actionable(step: &str) -> bool {
    let lower_step = step.to_ascii_lowercase();
    if lower_step.contains("exit plan")
        || lower_step.contains("exit_plan")
        || lower_step.contains("exitplanmode")
        || lower_step.contains("plan mode")
    {
        return false;
    }
    let trimmed = step.trim().trim_matches('`');
    if trimmed.is_empty() {
        return false;
    }
    for tool in [
        "read_file",
        "write_file",
        "grep_files",
        "list_dir",
        "find_paths",
        "run_command",
        "fetch_url",
        "apply_patch",
        "edit_file",
    ] {
        if lower_step.starts_with(tool) {
            return false;
        }
    }
    if (trimmed.starts_with('.') || trimmed.ends_with('/')) && !trimmed.contains(' ') {
        return false;
    }
    if (trimmed.ends_with(".md")
        || trimmed.ends_with(".rs")
        || trimmed.ends_with(".py")
        || trimmed.ends_with(".json")
        || trimmed.ends_with(".example"))
        && !trimmed.contains(' ')
    {
        return false;
    }
    true
}

pub(super) fn strip_plan_marker(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
        return Some(rest);
    }
    let mut saw_digit = false;
    for (idx, ch) in line.char_indices() {
        if ch.is_ascii_digit() {
            saw_digit = true;
            continue;
        }
        if saw_digit && matches!(ch, '.' | ')') {
            return Some(line[idx + ch.len_utf8()..].trim_start());
        }
        break;
    }
    None
}

pub(super) fn strip_markdown_emphasis(line: &str) -> String {
    let mut cleaned = line.trim().to_string();
    if let Some(rest) = cleaned.strip_prefix("**") {
        cleaned = rest.to_string();
        if let Some(idx) = cleaned.rfind("**") {
            cleaned.replace_range(idx..idx + 2, "");
        }
    }
    strip_step_label(cleaned.trim()).to_string()
}

pub(super) fn strip_step_label(line: &str) -> &str {
    let lower = line.to_ascii_lowercase();
    if !lower.starts_with("step ") {
        return line;
    }
    let rest = &line[5..];
    let mut saw_digit = false;
    for (idx, ch) in rest.char_indices() {
        if ch.is_ascii_digit() {
            saw_digit = true;
            continue;
        }
        if saw_digit && ch == ':' {
            return rest[idx + 1..].trim_start();
        }
        break;
    }
    line
}

pub(super) fn auto_static_artifact_input(task: &str, text: &str) -> Option<serde_json::Value> {
    if !static_artifact_requested(task) {
        return None;
    }
    let html = extract_html_from_text(text)?;
    let title = extract_html_title(&html).unwrap_or_else(|| "static-artifact".to_string());
    Some(serde_json::json!({
        "title": title,
        "contents": html,
        "type": "html",
    }))
}

pub(super) fn auto_create_file_input(task: &str) -> Option<serde_json::Value> {
    let task = recovery_task_text(task);
    let lower = task.to_ascii_lowercase();
    let create_pos = lower.find("create ")?;
    let after_create = &task[create_pos + "create ".len()..];
    let after_lower = &lower[create_pos + "create ".len()..];
    let containing_pos = after_lower.find(" containing ")?;
    let mut raw_path = after_create[..containing_pos]
        .trim()
        .trim_matches(['`', '\'', '"']);
    for article in ["a ", "an ", "the "] {
        if raw_path.to_ascii_lowercase().starts_with(article) {
            raw_path = raw_path[article.len()..].trim_start();
            break;
        }
    }
    let content = after_create[containing_pos + " containing ".len()..]
        .trim()
        .trim_matches(['`', '\'', '"']);
    if raw_path.is_empty()
        || content.is_empty()
        || raw_path.contains('\n')
        || raw_path.starts_with('-')
    {
        return None;
    }
    Some(serde_json::json!({
        "path": raw_path,
        "content": content,
    }))
}

pub(super) fn recovery_task_text(task: &str) -> String {
    task.split("\n\nFollow this approved plan:")
        .next()
        .unwrap_or(task)
        .split("\n\n[hook context]")
        .next()
        .unwrap_or(task)
        .trim()
        .to_string()
}

// A build/create verb must co-occur with the artifact noun so that "fix the
// collision bug in my game" or "explain how this website works" don't trigger
// artifact-recovery flows.
pub(super) fn task_has_build_verb(task: &str) -> bool {
    let lower = task.to_lowercase();
    lower.split(|c: char| !c.is_ascii_alphabetic()).any(|word| {
        matches!(
            word,
            "build"
                | "create"
                | "make"
                | "write"
                | "generate"
                | "develop"
                | "implement"
                | "code"
                | "publish"
        )
    })
}

pub(super) fn static_artifact_requested(task: &str) -> bool {
    if !task_has_build_verb(task) {
        return false;
    }
    let lower = task.to_lowercase();
    [
        "canvas game",
        "browser game",
        "static html",
        "static artifact",
        "single html",
        "standalone html",
        "landing page",
        "prototype",
        "demo",
        "website",
        "web app",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub(super) fn static_artifact_recovery_prompt(task: &str) -> String {
    format!(
        "This is a BUILD task, not a clarification request. Proceed with reasonable defaults for: {task}\n\n\
         If this is a canvas/browser game or static web artifact, build the runnable artifact now. \
         Call Artifact/publish_artifact with one complete self-contained HTML file. \
         Include all CSS and JavaScript inline. Do not ask the user for assets or a concept unless the task is impossible without them."
    )
}

pub(super) fn extract_html_from_text(text: &str) -> Option<String> {
    if let Some(fenced) = extract_fenced_html(text) {
        return Some(fenced);
    }
    let lower = text.to_lowercase();
    let start = lower
        .find("<!doctype html")
        .or_else(|| lower.find("<html"))?;
    let html = text[start..].trim();
    if html.contains("</html>") {
        Some(html.to_string())
    } else {
        None
    }
}

pub(super) fn extract_fenced_html(text: &str) -> Option<String> {
    let mut in_html = false;
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if in_html {
                return Some(out.join("\n").trim().to_string());
            }
            let tag = trimmed.trim_start_matches("```").trim().to_lowercase();
            if tag == "html" || tag.is_empty() {
                in_html = true;
                out.clear();
            }
            continue;
        }
        if in_html {
            out.push(line);
        }
    }
    None
}

pub(super) fn extract_html_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let start = lower.find("<title")?;
    let after_tag = &html[start..];
    let gt = after_tag.find('>')?;
    let rest = &after_tag[gt + 1..];
    let end = rest.to_lowercase().find("</title")?;
    let title = rest[..end].trim();
    (!title.is_empty()).then(|| title.to_string())
}

pub(super) fn canvas_game_requested(task: &str) -> bool {
    if !task_has_build_verb(task) {
        return false;
    }
    let lower = task.to_lowercase();
    lower.contains("canvas game") || lower.contains("browser game") || lower.contains("game")
}

// An imperative task tells the agent to do work, not answer a question —
// prose-only replies to these get the act-don't-explain nudge. Reuses the
// build-verb classifier plus change-verbs like add/fix; question-style tasks
// ("how do I build…?") want an answer and are excluded.
pub(super) fn task_is_imperative(task: &str) -> bool {
    let lower = task.trim().to_lowercase();
    if lower.ends_with('?')
        || [
            "how ", "what ", "why ", "when ", "where ", "who ", "which ", "should ", "could ",
            "can ", "is ", "are ", "does ", "do ", "explain ",
        ]
        .iter()
        .any(|q| lower.starts_with(q))
    {
        return false;
    }
    task_has_build_verb(task)
        || lower.split(|c: char| !c.is_ascii_alphabetic()).any(|word| {
            matches!(
                word,
                "add"
                    | "fix"
                    | "refactor"
                    | "update"
                    | "install"
                    | "convert"
                    | "remove"
                    | "rename"
                    | "delete"
                    | "change"
                    | "setup"
            )
        })
}

// Workspace evidence: a path-ish token or a noun that names something living
// in the repo. An imperative verb alone is not enough to demand tool calls —
// "write a poem about pirates" is a chat deliverable, and prose IS the work.
pub(super) fn task_references_workspace(task: &str) -> bool {
    let lower = task.to_lowercase();
    // Path-ish tokens: an explicit separator, or word.ext with a short
    // alphanumeric extension ("pirates.txt", "src/main.rs", "0.12.6").
    if lower.split_whitespace().any(|w| {
        let w = w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '/' && c != '.');
        if w.contains('/') {
            return true;
        }
        match w.rsplit_once('.') {
            Some((stem, ext)) => {
                !stem.is_empty()
                    && (1..=5).contains(&ext.len())
                    && ext.chars().all(|c| c.is_ascii_alphanumeric())
            }
            None => false,
        }
    }) {
        return true;
    }
    lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| {
        matches!(
            w,
            "file"
                | "files"
                | "folder"
                | "directory"
                | "repo"
                | "repository"
                | "project"
                | "script"
                | "module"
                | "function"
                | "class"
                | "struct"
                | "method"
                | "test"
                | "tests"
                | "bug"
                | "code"
                | "codebase"
                | "app"
                | "application"
                | "game"
                | "page"
                | "website"
                | "site"
                | "api"
                | "endpoint"
                | "server"
                | "config"
                | "configuration"
                | "dependency"
                | "dependencies"
                | "package"
                | "readme"
                | "changelog"
                | "docs"
                | "documentation"
                | "html"
                | "css"
                | "json"
                | "yaml"
                | "toml"
                | "component"
                | "library"
                | "crate"
                | "branch"
                | "commit"
                | "database"
                | "migration"
                | "schema"
        )
    })
}

// A message is "plannable" when it's a concrete build/change request aimed at
// the workspace — the only thing PLAN mode should decompose into steps. A
// greeting, a joke, an identity/meta question, or a creative aside ("write a
// poem") is not: those get a natural in-character reply, not a forced plan.
// Reuses the same imperative + workspace-evidence signals as the act-nudge.
pub(super) fn task_is_plannable(task: &str) -> bool {
    task_is_imperative(task) && task_references_workspace(task)
}

// Fix 14 decision: nudge the model to act instead of explaining. Fires only
// for imperative tasks that reference the workspace (a file, path, project,
// bug, …), only before any mutating tool has run this session, and only while
// the per-task cap has not been reached. A prose reply with no tool activity
// at all, or one that reads as how-to instructions, is treated as explaining
// rather than doing. Creative/chat asks ("write a poem about pirates") never
// nudge — the prose answer is the deliverable, not a failure to act.
pub(super) fn should_nudge_to_act(
    task: &str,
    reply_text: &str,
    any_tool_ran: bool,
    mutating_tool_ran: bool,
    nudges_so_far: usize,
) -> bool {
    if nudges_so_far >= MAX_ACT_NUDGES
        || mutating_tool_ran
        || !task_is_imperative(task)
        || !task_references_workspace(task)
    {
        return false;
    }
    !any_tool_ran || reads_as_instructions(reply_text)
}

// True when a reply reads as instructions/explanation for the USER to follow
// ("here's how", "you should…") rather than a summary of completed work.
// Kept conservative: callers must also check that no mutating tool ran.
pub(super) fn reads_as_instructions(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "you can",
        "you should",
        "you could",
        "you'll need to",
        "you will need to",
        "here's how",
        "here is how",
        "steps:",
        "step 1",
        "first,",
        "follow these steps",
        "to do this",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}
