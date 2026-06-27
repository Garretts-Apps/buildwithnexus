// The orchestration: plain Rust control flow for all three modes. PLAN
// decomposes + gates, BUILD is a ReAct tool loop, BRAINSTORM is conversational —
// but ALL modes have access to the full tool surface so the model can grep,
// fetch, read files, and run commands regardless of which mode the user is in.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config;
use crate::hooks::{self, PreDecision};
use crate::provider::{self, complete, Msg, Provider, Reply, ToolResult};
use crate::report;
use crate::tools;
use crate::tui;

const MAX_ITERS: usize = 30;
const MAX_DEPTH: usize = 3;
const KEEP_RECENT: usize = 6;

// ── context compaction ────────────────────────────────────────────────────────
fn estimate_tokens(msgs: &[Msg]) -> usize {
    let mut chars = 0usize;
    for m in msgs {
        chars += match m {
            Msg::System(s) | Msg::User(s) => s.len(),
            Msg::UserImages { text, images } => text.len() + images.iter().map(|(_, d)| d.len() / 3).sum::<usize>(),
            Msg::Assistant { text, calls } => {
                text.len() + calls.iter().map(|c| c.name.len() + c.input.to_string().len()).sum::<usize>()
            }
            Msg::Tool(rs) => rs.iter().map(|r| r.content.len()).sum(),
        };
    }
    chars / 4
}

fn compaction_split(msgs: &[Msg]) -> (usize, usize) {
    let sys_end = msgs.iter().take_while(|m| matches!(m, Msg::System(_))).count();
    let tail_start = msgs.len().saturating_sub(KEEP_RECENT).max(sys_end);
    (sys_end, tail_start)
}

fn structural_summary(middle: &[Msg]) -> String {
    let mut actions: Vec<String> = Vec::new();
    for m in middle {
        if let Msg::Assistant { calls, .. } = m {
            for c in calls {
                actions.push(tools::preview(&c.name, &c.input));
            }
        }
    }
    if actions.is_empty() {
        "(earlier discussion)".to_string()
    } else {
        format!("Earlier steps taken: {}", actions.join("; "))
    }
}

fn render_msgs(msgs: &[Msg]) -> String {
    let mut s = String::new();
    for m in msgs {
        match m {
            Msg::System(t) => { s.push_str("system: "); s.push_str(t); }
            Msg::User(t) => { s.push_str("user: "); s.push_str(t); }
            Msg::UserImages { text, images } => {
                s.push_str(&format!("user [+{} image(s)]: ", images.len()));
                s.push_str(text);
            }
            Msg::Assistant { text, calls } => {
                s.push_str("assistant: ");
                s.push_str(text);
                for c in calls {
                    s.push_str(&format!(" [tool {} {}]", c.name, c.input));
                }
            }
            Msg::Tool(rs) => {
                for r in rs {
                    s.push_str("result: ");
                    s.push_str(&r.content.chars().take(800).collect::<String>());
                }
            }
        }
        s.push('\n');
    }
    s
}

fn compact_with(msgs: Vec<Msg>, summarize: impl FnOnce(&[Msg]) -> String) -> Vec<Msg> {
    let (sys_end, tail_start) = compaction_split(&msgs);
    if tail_start <= sys_end {
        return msgs;
    }
    let mut it = msgs.into_iter();
    let system: Vec<Msg> = it.by_ref().take(sys_end).collect();
    let middle: Vec<Msg> = it.by_ref().take(tail_start - sys_end).collect();
    let tail: Vec<Msg> = it.collect();
    let summary = summarize(&middle);
    let mut v = system;
    v.push(Msg::User(format!("[Summary of earlier conversation, compacted to save context]\n{summary}")));
    v.extend(tail);
    v
}

fn model_summary(p: &Provider, middle: &[Msg]) -> String {
    let sys = "Summarize this AI coding-agent conversation into a terse brief that preserves the task, key findings, decisions, files changed, and the current state. Drop pleasantries; use compact bullet points.";
    let q = vec![Msg::System(sys.into()), Msg::User(render_msgs(middle))];
    match complete(p, &q, &[]) {
        Ok(r) if !r.text.trim().is_empty() => r.text,
        _ => structural_summary(middle),
    }
}

fn maybe_compact(p: &Provider, msgs: &mut Vec<Msg>) {
    let budget = p.context_tokens.saturating_mul(8) / 10;
    if budget == 0 || estimate_tokens(msgs) <= budget {
        return;
    }
    let (sys_end, tail_start) = compaction_split(msgs);
    if tail_start <= sys_end {
        return;
    }
    report::notice("  ⟳ compacting context…");
    let taken = std::mem::take(msgs);
    *msgs = compact_with(taken, |middle| model_summary(p, middle));
}

// ── permissions ───────────────────────────────────────────────────────────────
#[derive(Clone, Copy)]
pub enum Permission {
    Ask,
    Auto,
    ReadOnly,
}

pub fn permission(s: &str) -> Permission {
    match s {
        "auto" => Permission::Auto,
        "readonly" => Permission::ReadOnly,
        _ => Permission::Ask,
    }
}

// ── roles ─────────────────────────────────────────────────────────────────────
pub struct Role {
    pub system: &'static str,
}

pub fn role(id: &str) -> Role {
    let system = match id {
        "researcher" => "You are a meticulous research engineer. Investigate the codebase with the read and list tools before drawing conclusions. Cite file paths. Do not modify files unless explicitly asked.",
        _ => "You are an autonomous senior software engineer. \
Use the tools to inspect and modify the project directly. \
Prefer small, verifiable edits. Read before you write. \
When the task is complete, call the finish tool with a one-paragraph summary.\n\n\
IMPORTANT — tool discipline:\n\
• Before using run_command to install anything (npm install, pip install, cargo add, brew install, \
apt install, etc.), FIRST check whether an existing package or built-in tool can do the job. \
Look at package.json / Cargo.toml / requirements.txt / pyproject.toml to see what is already available. \
Use run_command with grep/find/jq/awk/sed/git/curl/which before reaching for new installs.\n\
• fetch_url is built-in — do NOT install curl/wget just to make an HTTP request.\n\
• read_file / list_dir / edit_file are built-in — do NOT install file-management packages for basic I/O.\n\
• If a system tool is already present (check with `which <tool>` or `command -v <tool>`) prefer it \
over installing an alternative.",
    };
    Role { system }
}

// Build the system prompt prefix from memory and skills/agents files.
fn context_prefix() -> String {
    let mut parts: Vec<String> = Vec::new();

    // Always include the tool manifest so the model knows what's built-in
    // and doesn't try to install external tools to do things we already handle.
    parts.push(tool_manifest());

    // Probe which common system tools are actually present so the model can
    // pick the right one without guessing or installing alternatives.
    let env_snap = env_snapshot();
    if !env_snap.is_empty() {
        parts.push(format!("[Environment — tools already installed]\n{env_snap}"));
    }

    if let Some(mem) = config::load_memory() {
        parts.push(format!("[Memory from previous sessions]\n{mem}"));
    }
    if let Some(agents) = config::load_agents() {
        parts.push(format!("[Agent knowledge — Agents.md]\n{agents}"));
    }
    let skills = config::load_skills();
    if !skills.is_empty() {
        let joined = skills.iter()
            .map(|(name, content)| format!("## Skill: {name}\n{content}"))
            .collect::<Vec<_>>().join("\n\n");
        parts.push(format!("[Available skills]\n{joined}"));
    }

    format!("{}\n\n", parts.join("\n\n"))
}

fn tool_manifest() -> String {
    "[Built-in tools — always available, no install needed]\n\
• read_file / list_dir   — read any file or directory on the filesystem\n\
• write_file / edit_file — create or surgically modify files\n\
• run_command            — run any shell command: grep, find, git, cargo, make, npm, python3, etc.\n\
• fetch_url              — HTTP GET (no curl/wget needed)\n\
• save_memory            — persist a note across sessions\n\
• spawn_subagent         — delegate a sub-task to a fresh agent with its own context\n\
• finish                 — mark the task complete with a summary"
    .to_string()
}

fn env_snapshot() -> String {
    // Probe a fixed list of common tools so the model knows what's available.
    // We run everything in parallel-ish with short timeouts via sequential calls —
    // the total overhead is ~10ms on a modern machine.
    let probes: &[(&str, &str)] = &[
        ("node",    "node --version"),
        ("npm",     "npm --version"),
        ("npx",     "npx --version"),
        ("python3", "python3 --version"),
        ("pip3",    "pip3 --version"),
        ("cargo",   "cargo --version"),
        ("rustc",   "rustc --version"),
        ("git",     "git --version"),
        ("docker",  "docker --version"),
        ("jq",      "jq --version"),
        ("rg",      "rg --version"),
        ("gh",      "gh --version"),
        ("bun",     "bun --version"),
        ("deno",    "deno --version"),
        ("go",      "go version"),
        ("ruby",    "ruby --version"),
        ("java",    "java --version"),
    ];

    use std::process::Command;
    let mut found: Vec<String> = Vec::new();
    for (label, cmd) in probes {
        let parts: Vec<&str> = cmd.splitn(2, ' ').collect();
        let bin = parts[0];
        let arg = parts.get(1).copied().unwrap_or("--version");
        let ok = Command::new(bin).arg(arg)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .map(|o| {
                let txt = String::from_utf8_lossy(&o.stdout).trim().to_string();
                let txt = if txt.is_empty() { String::from_utf8_lossy(&o.stderr).trim().to_string() } else { txt };
                txt.lines().next().unwrap_or("").to_string()
            })
            .ok()
            .filter(|v| !v.is_empty());
        if let Some(ver) = ok {
            found.push(format!("• {label}: {ver}"));
        }
    }
    found.join("\n")
}

fn confirm(label: &str) -> Option<String> {
    if report::is_json() || !std::io::stdin().is_terminal() {
        return Some(format!("blocked (no interactive terminal to confirm: {label})"));
    }
    let q = format!("  {} {}? {} ", tui::yellow("➤"), tui::bold(label), tui::dim("[y/N]"));
    let ans = tui::ask(&q).unwrap_or_default();
    if matches!(ans.trim().to_lowercase().as_str(), "y" | "yes") {
        None
    } else {
        Some("denied by user".into())
    }
}

// Permission gate — returns Some(reason) when blocked.
// Read operations outside CWD are allowed in all modes (no CWD confinement for
// reads); the user specifically asked for full filesystem access.
pub(crate) fn gate(perm: Permission, name: &str, input: &serde_json::Value, cwd: &Path) -> Option<String> {
    let path = tools::touched_path(name, input, cwd);

    if let Some(p) = &path {
        if tools::is_sensitive(p) {
            return confirm(&format!("access sensitive path {}", p.display()));
        }
        // In WSL2, writing to a Windows drive mount (/mnt/c/, /mnt/d/, etc.)
        // crosses the OS boundary — always confirm, even in Auto mode.
        if tools::is_mutating(name) && tools::is_wsl_windows_mount(p) {
            return confirm(&format!("write to Windows filesystem {} (WSL2 boundary)", p.display()));
        }
    }
    if name == "run_command" {
        if let Some(c) = input["command"].as_str() {
            if tools::catastrophic(c) {
                return confirm(&format!("run dangerous command `{c}`"));
            }
            // In WSL2, commands that reference /mnt/<drive>/ target the Windows
            // filesystem — confirm before running, even in Auto mode.
            if tools::is_wsl() && tools::command_touches_wsl_mount(c) {
                return confirm(&format!("command targets Windows filesystem (WSL2): `{c}`"));
            }
        }
    }

    match perm {
        Permission::Auto => None,
        Permission::ReadOnly => {
            if tools::is_mutating(name) {
                // run_command with clearly read-only shell tools (grep, find, etc.)
                // should pass through even in ReadOnly mode.
                if name == "run_command" {
                    if let Some(c) = input["command"].as_str() {
                        if tools::is_readonly_command(c) {
                            return None;
                        }
                    }
                }
                return Some("read-only mode: mutation skipped".into());
            }
            None // reads anywhere are allowed in readonly mode
        }
        Permission::Ask => {
            if tools::is_mutating(name) {
                return confirm(&tools::preview(name, input));
            }
            // Out-of-cwd reads: just note it instead of hard-blocking.
            // The user asked for full filesystem access.
            None
        }
    }
}

// ── BUILD mode ────────────────────────────────────────────────────────────────
pub fn run_build(p: &Provider, perm: Permission, role_id: &str, task: &str, cwd: &Path) -> Result<(), String> {
    let mut transcript: Vec<Msg> = Vec::new();
    run_build_session(p, perm, role_id, task, cwd, &mut transcript, &crate::session::new_id())
}

pub fn run_build_resumed(p: &Provider, perm: Permission, role_id: &str, task: &str, cwd: &Path, mut seed: Vec<Msg>, sid: &str) -> Result<(), String> {
    run_build_session(p, perm, role_id, task, cwd, &mut seed, sid)
}

pub fn run_build_session(p: &Provider, perm: Permission, role_id: &str, task: &str, cwd: &Path, transcript: &mut Vec<Msg>, sid: &str) -> Result<(), String> {
    let r = build_inner(p, perm, role_id, task, cwd, 0, transcript).map(|_| ());
    hooks::notify("Stop", cwd);
    crate::session::save(sid, cwd, &p.model, transcript);
    r
}

fn build_inner(p: &Provider, perm: Permission, role_id: &str, task: &str, cwd: &Path, depth: usize, msgs: &mut Vec<Msg>) -> Result<String, String> {
    let task = match hooks::user_prompt_submit(task, cwd) {
        Err(reason) => {
            report::error(&format!("blocked by hook: {reason}"));
            return Ok(String::new());
        }
        Ok(ctx) if !ctx.is_empty() => format!("{task}\n\n[hook context]\n{ctx}"),
        Ok(_) => task.to_string(),
    };
    let defs = tools::defs(depth < MAX_DEPTH);
    if msgs.is_empty() {
        let prefix = context_prefix();
        let sys = format!("{prefix}{}", role(role_id).system);
        msgs.push(Msg::System(sys));
    }
    // If the caller already pushed a UserImages message (multimodal input), use its
    // text as the task without pushing another User turn; otherwise push normally.
    let already_pushed = matches!(msgs.last(), Some(Msg::UserImages { .. }));
    if !already_pushed {
        msgs.push(Msg::User(task));
    }

    // Track which files have been read this session so we can enforce read-before-write.
    let mut read_paths: std::collections::HashSet<PathBuf> = Default::default();

    for step in 1..=MAX_ITERS {
        if tui::interrupted() {
            report::notice("interrupted");
            return Ok(String::new());
        }
        maybe_compact(p, msgs);
        let reply: Reply = if report::is_json() {
            let r = complete(p, msgs.as_slice(), &defs)?;
            report::assistant(&r.text);
            r
        } else {
            // Show a step counter so the user can see multi-step reasoning progress.
            if step > 1 {
                tui::line(&tui::dim(&format!("  ↻ step {step}")));
            }
            let mut spin = Some(tui::spinner_start("thinking…"));
            let mut streamed = false;
            let mut thinking_buf = String::new();
            let mut renderer = tui::StreamRenderer::new();
            let res = provider::stream(p, msgs.as_slice(), &defs, &mut |c| {
                if let Some(s) = spin.take() {
                    tui::spinner_stop(s);
                }
                renderer.push(c);
                streamed = true;
            }, &mut |t| {
                // Extended thinking: buffer and display as dim internal monologue.
                thinking_buf.push_str(t);
                // Flush complete sentences/lines so output feels live.
                while let Some(nl) = thinking_buf.find('\n') {
                    let line = thinking_buf[..nl].trim().to_string();
                    if !line.is_empty() {
                        tui::write_stream(&format!("\r  {} {}\r\n", tui::dim("◌"), tui::dim(&line)));
                    }
                    thinking_buf = thinking_buf[nl + 1..].to_string();
                }
            });
            if let Some(s) = spin.take() {
                tui::spinner_stop(s);
            }
            let r = res?;
            renderer.flush();
            if streamed {
                report::assistant_end();
            }
            r
        };
        if reply.calls.is_empty() {
            if reply.text.trim().is_empty() {
                report::notice("model returned no output");
            }
            return Ok(reply.text);
        }

        let mut results = Vec::new();
        let mut summary: Option<String> = None;
        for call in &reply.calls {
            if let Some(raw) = call.input.get(tools::INVALID_ARGS).and_then(|v| v.as_str()) {
                let msg = format!("tool arguments were not valid JSON: {}", raw.chars().take(200).collect::<String>());
                report::tool_denied(&msg);
                results.push(ToolResult { id: call.id.clone(), content: msg, is_error: true });
                continue;
            }
            report::tool_call(&call.name, &tools::preview(&call.name, &call.input), &call.input);

            let reason = match hooks::pre_tool_use(&call.name, &call.input, cwd) {
                PreDecision::Deny(r) => Some(r),
                PreDecision::Allow => None,
                PreDecision::Continue => gate(perm, &call.name, &call.input, cwd),
            };
            if let Some(reason) = reason {
                report::tool_denied(&reason);
                results.push(ToolResult { id: call.id.clone(), content: reason, is_error: true });
                continue;
            }

            // Record reads so we can enforce read-before-write below.
            if call.name == "read_file" {
                if let Some(raw) = call.input["path"].as_str() {
                    let p = if std::path::Path::new(raw).is_absolute() {
                        PathBuf::from(raw)
                    } else {
                        cwd.join(raw)
                    };
                    read_paths.insert(p);
                }
            }

            // Require the model to read a file before overwriting or patching it.
            if matches!(call.name.as_str(), "write_file" | "edit_file") {
                if let Some(raw) = call.input["path"].as_str() {
                    let p = if std::path::Path::new(raw).is_absolute() {
                        PathBuf::from(raw)
                    } else {
                        cwd.join(raw)
                    };
                    if p.exists() && !read_paths.contains(&p) {
                        let msg = format!(
                            "read_file('{}') required before editing. Read the file first, then retry.",
                            p.display()
                        );
                        report::tool_denied(&msg);
                        results.push(ToolResult { id: call.id.clone(), content: msg, is_error: true });
                        continue;
                    }
                }
            }

            if call.name == "spawn_subagent" {
                let out = spawn_subagent(p, perm, &call.input, cwd, depth);
                report::tool_result(&call.name, &out, false);
                results.push(ToolResult { id: call.id.clone(), content: out, is_error: false });
                continue;
            }
            // Memory-save tool: the model can call save_memory to persist facts.
            if call.name == "save_memory" {
                if let Some(note) = call.input["note"].as_str() {
                    config::append_memory(note);
                    let msg = "memory saved".to_string();
                    report::tool_result(&call.name, &msg, false);
                    results.push(ToolResult { id: call.id.clone(), content: msg, is_error: false });
                    continue;
                }
            }

            let out = tools::run(&call.name, &call.input, cwd);
            hooks::post_tool_use(&call.name, &call.input, &out.content, out.is_error, cwd);
            report::tool_result(&call.name, &out.content, out.is_error);
            if out.finished {
                report::finish(&out.content);
                summary = Some(out.content.clone());
            }
            results.push(ToolResult { id: call.id.clone(), content: out.content, is_error: out.is_error });
        }

        msgs.push(Msg::Assistant { text: reply.text, calls: reply.calls });
        msgs.push(Msg::Tool(results));
        if !report::is_json() {
            tui::context_meter(estimate_tokens(msgs), p.context_tokens);
        }
        if let Some(s) = summary {
            return Ok(s);
        }
    }
    Err(format!("reached the {MAX_ITERS}-step limit without finishing"))
}

static SUB_SEQ: AtomicUsize = AtomicUsize::new(0);

fn spawn_subagent(p: &Provider, perm: Permission, input: &serde_json::Value, cwd: &Path, depth: usize) -> String {
    if depth + 1 >= MAX_DEPTH {
        return "subagent depth limit reached".into();
    }
    let task = input["task"].as_str().unwrap_or("").trim();
    if task.is_empty() {
        return "spawn_subagent requires a task".into();
    }
    let role = input["role"].as_str().unwrap_or("engineer");
    let isolate = input["isolate"].as_bool().unwrap_or(false);

    let (run_cwd, note) = if isolate {
        match make_worktree(cwd) {
            Some(wt) => {
                let n = format!("[isolated worktree: {}]\n", wt.display());
                (wt, n)
            }
            None => (cwd.to_path_buf(), "[worktree unavailable — ran in place]\n".into()),
        }
    } else {
        (cwd.to_path_buf(), String::new())
    };

    report::notice(&format!("  ↳ subagent: {task}"));
    let mut child: Vec<Msg> = Vec::new();
    let result = build_inner(p, perm, role, task, &run_cwd, depth + 1, &mut child)
        .unwrap_or_else(|e| format!("subagent error: {e}"));
    format!("{note}{result}")
}

fn make_worktree(cwd: &Path) -> Option<PathBuf> {
    let id = SUB_SEQ.fetch_add(1, Ordering::Relaxed);
    let wt = cwd.join(format!(".bwn/worktrees/sub-{}-{id}", std::process::id()));
    let branch = format!("bwn-sub-{}-{id}", std::process::id());
    let out = Command::new("git")
        .current_dir(cwd)
        .args(["worktree", "add", "-b", &branch])
        .arg(&wt)
        .output()
        .ok()?;
    out.status.success().then_some(wt)
}

// ── PLAN mode ─────────────────────────────────────────────────────────────────
// The planning phase now has tools available so the model can inspect the
// codebase while breaking down the task. Execution still runs through BUILD.
pub fn run_plan(p: &Provider, perm: Permission, task: &str, cwd: &Path) -> Result<(), String> {
    let prefix = context_prefix();
    let sys = format!("{prefix}You are a planning engineer with full access to the codebase. \
        Use read_file and list_dir and run_command to inspect the project as needed. \
        Break the user's task into a concise numbered list of concrete steps \
        (one step per line, max 8 steps). Output the numbered list and nothing else.");

    let defs = tools::defs(false); // planning uses tools but not subagents
    let mut msgs = vec![Msg::System(sys), Msg::User(task.into())];

    // Let the model use tools while planning (e.g. read files to understand structure).
    let plan_text = loop {
        maybe_compact(p, &mut msgs);
        let reply = tui::with_spinner("planning…", || complete(p, &msgs, &defs))?;

        if reply.calls.is_empty() {
            break reply.text;
        }

        // Execute tool calls during the planning phase.
        let mut results = Vec::new();
        for call in &reply.calls {
            report::tool_call(&call.name, &tools::preview(&call.name, &call.input), &call.input);
            let reason = match hooks::pre_tool_use(&call.name, &call.input, cwd) {
                PreDecision::Deny(r) => Some(r),
                PreDecision::Allow => None,
                PreDecision::Continue => gate(perm, &call.name, &call.input, cwd),
            };
            if let Some(reason) = reason {
                results.push(ToolResult { id: call.id.clone(), content: reason, is_error: true });
                continue;
            }
            if call.name == "save_memory" {
                if let Some(note) = call.input["note"].as_str() {
                    config::append_memory(note);
                    let msg = "memory saved".to_string();
                    report::tool_result(&call.name, &msg, false);
                    results.push(ToolResult { id: call.id.clone(), content: msg, is_error: false });
                    continue;
                }
            }
            let out = tools::run(&call.name, &call.input, cwd);
            report::tool_result(&call.name, &out.content, out.is_error);
            results.push(ToolResult { id: call.id.clone(), content: out.content, is_error: out.is_error });
        }
        msgs.push(Msg::Assistant { text: reply.text, calls: reply.calls });
        msgs.push(Msg::Tool(results));
    };

    let mut steps: Vec<String> = plan_text.lines()
        .map(|l| l.trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '.' | ')' | '-' | ' ')).trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();

    loop {
        tui::line("");
        tui::line(&tui::accent("  Plan"));
        for (i, s) in steps.iter().enumerate() {
            tui::line(&format!("  {}. {}", i + 1, s));
        }
        tui::line("");
        let ans = tui::ask(&format!("  {} execute, {} edit <n>, {} cancel: ",
            tui::bold("[Enter]"), tui::bold("e"), tui::bold("c"))).unwrap_or_default();
        let a = ans.trim();
        if a.is_empty() || a == "y" {
            break;
        }
        if a == "c" {
            tui::line(&tui::yellow("  cancelled"));
            return Ok(());
        }
        if let Some(rest) = a.strip_prefix("e") {
            if let Ok(n) = rest.trim().parse::<usize>() {
                if n >= 1 && n <= steps.len() {
                    if let Some(new) = tui::ask("  new text: ") {
                        if !new.trim().is_empty() {
                            steps[n - 1] = new.trim().to_string();
                        }
                    }
                }
            }
        }
    }

    let plan = steps.iter().enumerate().map(|(i, s)| format!("{}. {}", i + 1, s)).collect::<Vec<_>>().join("\n");
    let full = format!("{task}\n\nFollow this approved plan:\n{plan}");
    run_build(p, perm, "engineer", &full, cwd)
}

// ── BRAINSTORM mode ───────────────────────────────────────────────────────────
// Brainstorm is conversational but has full tool access. The model can grep,
// read files, fetch URLs, and run commands when the conversation calls for it.
// It also has a mode-transition sensor: if it detects the user wants to build
// or plan, it suggests switching.
pub fn run_brainstorm(p: &Provider, perm: Permission, cwd: &Path, first: &str) -> Result<Option<ModeHint>, String> {
    let prefix = context_prefix();
    let sys = format!("{prefix}You are a sharp, concise thought partner with full access to the codebase and the internet. \
        Use tools freely to look things up, read files, grep for patterns, or run commands — \
        whatever helps the conversation. \
        When you think the user is ready to stop discussing and start building or planning, \
        end your response with the exact token [SUGGEST:BUILD] or [SUGGEST:PLAN] on its own line. \
        Otherwise just respond naturally. No fluff.");

    let defs = tools::defs(false);
    let mut msgs: Vec<Msg> = vec![Msg::System(sys)];
    let mut question = first.to_string();

    loop {
        msgs.push(Msg::User(question.clone()));
        maybe_compact(p, &mut msgs);

        // Keep consuming tool calls until the model gives a text response.
        let reply_text = loop {
            tui::line("");
            let mut spin = Some(tui::spinner_start("thinking…"));
            let mut streamed = false;
            let mut thinking_buf = String::new();
            let mut renderer = if !report::is_json() { Some(tui::StreamRenderer::new()) } else { None };
            let res = provider::stream(p, &msgs, &defs, &mut |c| {
                if let Some(s) = spin.take() { tui::spinner_stop(s); }
                if let Some(r) = &mut renderer { r.push(c); } else { report::assistant_delta(c); }
                streamed = true;
            }, &mut |t| {
                thinking_buf.push_str(t);
                while let Some(nl) = thinking_buf.find('\n') {
                    let tl = thinking_buf[..nl].trim().to_string();
                    if !tl.is_empty() {
                        tui::write_stream(&format!("\r  {} {}\r\n", tui::dim("◌"), tui::dim(&tl)));
                    }
                    thinking_buf = thinking_buf[nl + 1..].to_string();
                }
            });
            if let Some(s) = spin.take() { tui::spinner_stop(s); }
            let reply = res?;
            if let Some(r) = &mut renderer { r.flush(); }
            if streamed { report::assistant_end(); }

            if reply.calls.is_empty() {
                msgs.push(Msg::Assistant { text: reply.text.clone(), calls: vec![] });
                if !report::is_json() {
                    tui::context_meter(estimate_tokens(&msgs), p.context_tokens);
                }
                break reply.text;
            }

            // Execute tool calls inline.
            let mut results = Vec::new();
            for call in &reply.calls {
                report::tool_call(&call.name, &tools::preview(&call.name, &call.input), &call.input);
                let reason = match hooks::pre_tool_use(&call.name, &call.input, cwd) {
                    PreDecision::Deny(r) => Some(r),
                    PreDecision::Allow => None,
                    PreDecision::Continue => gate(perm, &call.name, &call.input, cwd),
                };
                if let Some(reason) = reason {
                    results.push(ToolResult { id: call.id.clone(), content: reason, is_error: true });
                    continue;
                }
                if call.name == "save_memory" {
                    if let Some(note) = call.input["note"].as_str() {
                        config::append_memory(note);
                        let msg = "memory saved".to_string();
                        report::tool_result(&call.name, &msg, false);
                        results.push(ToolResult { id: call.id.clone(), content: msg, is_error: false });
                        continue;
                    }
                }
                let out = tools::run(&call.name, &call.input, cwd);
                hooks::post_tool_use(&call.name, &call.input, &out.content, out.is_error, cwd);
                report::tool_result(&call.name, &out.content, out.is_error);
                results.push(ToolResult { id: call.id.clone(), content: out.content, is_error: out.is_error });
            }
            msgs.push(Msg::Assistant { text: reply.text, calls: reply.calls });
            msgs.push(Msg::Tool(results));
            if !report::is_json() {
                tui::context_meter(estimate_tokens(&msgs), p.context_tokens);
            }
        };

        // Check for mode-transition suggestion embedded in the reply.
        let hint = if reply_text.contains("[SUGGEST:BUILD]") {
            Some(ModeHint::Build)
        } else if reply_text.contains("[SUGGEST:PLAN]") {
            Some(ModeHint::Plan)
        } else {
            None
        };

        if let Some(ref h) = hint {
            tui::line("");
            let suggestion = match h {
                ModeHint::Build     => "switch to BUILD mode and implement this?",
                ModeHint::Plan      => "switch to PLAN mode and break this down?",
                ModeHint::CycleMode => "cycle to the next mode?",
            };
            tui::line(&tui::yellow(&format!("  ↪ AI suggests: {suggestion}")));
            tui::line(&tui::dim("  (y to switch, anything else to keep chatting)"));
            let ans = tui::ask("  ").unwrap_or_default();
            if matches!(ans.trim().to_lowercase().as_str(), "y" | "yes") {
                return Ok(Some(h.clone()));
            }
        }

        tui::line("");
        match tui::ask_task(&format!("{} ", tui::blue("you ›"))) {
            None => return Ok(None),
            Some(tui::InputEvent::CycleMode) => return Ok(Some(ModeHint::CycleMode)),
            Some(tui::InputEvent::Text(f)) => {
                let t = f.trim();
                if t.is_empty() || t == "exit" || t == "done" {
                    return Ok(None);
                }
                question = t.to_string();
            }
        }
    }
}

#[derive(Clone)]
pub enum ModeHint {
    Build,
    Plan,
    CycleMode,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn permission_parsing() {
        assert!(matches!(permission("auto"), Permission::Auto));
        assert!(matches!(permission("readonly"), Permission::ReadOnly));
        assert!(matches!(permission("ask"), Permission::Ask));
        assert!(matches!(permission("anything-else"), Permission::Ask));
        assert!(matches!(permission(""), Permission::Ask));
    }

    #[test]
    fn role_selection() {
        assert!(role("researcher").system.contains("research"));
        assert!(role("engineer").system.contains("software engineer"));
        assert!(role("ceo").system.contains("software engineer"));
    }

    #[test]
    fn gate_auto_allows_ordinary_mutation() {
        let cwd = Path::new("/proj");
        let r = gate(Permission::Auto, "write_file", &json!({"path": "a.txt", "content": "x"}), cwd);
        assert!(r.is_none());
    }

    #[test]
    fn gate_auto_still_confirms_sensitive_path() {
        let cwd = Path::new("/proj");
        let r = gate(Permission::Auto, "read_file", &json!({"path": "/proj/.env"}), cwd);
        assert!(r.is_some());
    }

    #[test]
    fn gate_auto_confirms_catastrophic_command() {
        let cwd = Path::new("/proj");
        let r = gate(Permission::Auto, "run_command", &json!({"command": "rm -rf /"}), cwd);
        assert!(r.is_some());
    }

    #[test]
    fn gate_auto_allows_safe_command() {
        let cwd = Path::new("/proj");
        let r = gate(Permission::Auto, "run_command", &json!({"command": "ls"}), cwd);
        assert!(r.is_none());
    }

    #[test]
    fn gate_readonly_blocks_mutation() {
        let cwd = Path::new("/proj");
        let r = gate(Permission::ReadOnly, "write_file", &json!({"path": "a", "content": "x"}), cwd);
        assert!(r.unwrap().contains("read-only"));
    }

    #[test]
    fn gate_readonly_allows_reads_everywhere() {
        // Reads outside CWD are now allowed — full filesystem access.
        let cwd = Path::new("/proj/work");
        let r = gate(Permission::ReadOnly, "read_file", &json!({"path": "/etc/passwd"}), cwd);
        assert!(r.is_none());
    }

    #[test]
    fn gate_ask_allows_out_of_cwd_read() {
        // Full filesystem read access — no longer blocked in Ask mode.
        let cwd = Path::new("/proj");
        let r = gate(Permission::Ask, "read_file", &json!({"path": "/home/user/docs/README.md"}), cwd);
        assert!(r.is_none());
    }

    #[test]
    fn gate_ask_prompts_for_mutation() {
        let cwd = Path::new("/proj");
        let r = gate(Permission::Ask, "write_file", &json!({"path": "a", "content": "x"}), cwd);
        assert!(r.is_some()); // non-terminal → denied
    }

    #[test]
    fn estimate_tokens_counts_text() {
        assert_eq!(estimate_tokens(&[Msg::User("a".repeat(40))]), 10);
    }

    #[test]
    fn compaction_split_keeps_system_and_recent_tail() {
        let mut msgs = vec![Msg::System("s".into())];
        for i in 0..10 {
            msgs.push(Msg::User(format!("m{i}")));
        }
        let (sys_end, tail_start) = compaction_split(&msgs);
        assert_eq!(sys_end, 1);
        assert_eq!(tail_start, msgs.len() - KEEP_RECENT);
    }

    #[test]
    fn compaction_split_noop_when_short() {
        let msgs = vec![Msg::System("s".into()), Msg::User("u".into())];
        let (sys_end, tail_start) = compaction_split(&msgs);
        assert!(tail_start <= sys_end);
    }

    #[test]
    fn compact_with_replaces_middle() {
        let mut msgs = vec![Msg::System("sys".into())];
        for i in 0..10 {
            msgs.push(Msg::User(format!("u{i}")));
        }
        let out = compact_with(msgs, |_| "SUMMARY".into());
        assert_eq!(out.len(), 1 + 1 + KEEP_RECENT);
        assert!(matches!(&out[0], Msg::System(s) if s == "sys"));
        assert!(matches!(&out[1], Msg::User(s) if s.contains("SUMMARY")));
        assert!(matches!(out.last(), Some(Msg::User(s)) if s == "u9"));
    }

    #[test]
    fn structural_summary_lists_tool_actions() {
        let msgs = vec![Msg::Assistant {
            text: String::new(),
            calls: vec![crate::provider::ToolCall {
                id: "1".into(),
                name: "run_command".into(),
                input: json!({"command": "ls"}),
            }],
        }];
        assert!(structural_summary(&msgs).contains("run: ls"));
    }
}
