// The orchestration that replaces LangGraph: plain Rust control flow for the
// three modes. PLAN decomposes + gates, BUILD is a ReAct tool loop, BRAINSTORM
// is streaming-free chat. Roles are a small data table, not an agent framework.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::hooks::{self, PreDecision};
use crate::provider::{self, complete, Msg, Provider, Reply, ToolResult};
use crate::report;
use crate::tools;
use crate::tui;

const MAX_ITERS: usize = 30;
const MAX_DEPTH: usize = 3;

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

pub struct Role {
    pub system: &'static str,
}

pub fn role(id: &str) -> Role {
    let system = match id {
        "researcher" => "You are a meticulous research engineer. Investigate the codebase with the read and list tools before drawing conclusions. Cite file paths. Do not modify files unless explicitly asked.",
        _ => "You are an autonomous senior software engineer. Use the tools to inspect and modify the project directly. Prefer small, verifiable edits. Read before you write. When the task is complete, call the finish tool with a one-paragraph summary.",
    };
    Role { system }
}

fn confirm(label: &str) -> Option<String> {
    // Never block waiting for input when no human can answer (JSON/automation or
    // a non-terminal stdin) — deny by default instead of hanging.
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

// Returns Some(reason) when a call is blocked, None when allowed. Layers:
// sensitive paths and catastrophic commands always require an explicit yes
// (in auto/headless an un-answerable prompt safely resolves to denial); then
// the chosen permission mode governs mutations and out-of-cwd reads.
fn gate(perm: Permission, name: &str, input: &serde_json::Value, cwd: &Path) -> Option<String> {
    let path = tools::touched_path(name, input, cwd);

    if let Some(p) = &path {
        if tools::is_sensitive(p) {
            return confirm(&format!("access sensitive path {}", p.display()));
        }
    }
    if name == "run_command" {
        if let Some(c) = input["command"].as_str() {
            if tools::catastrophic(c) {
                return confirm(&format!("run dangerous command `{c}`"));
            }
        }
    }

    match perm {
        Permission::Auto => None,
        Permission::ReadOnly => {
            if tools::is_mutating(name) {
                return Some("read-only mode: mutation skipped".into());
            }
            if let Some(p) = &path {
                if tools::escapes_cwd(p, cwd) {
                    return Some(format!("read-only: refusing to read outside the working directory ({})", p.display()));
                }
            }
            None
        }
        Permission::Ask => {
            if tools::is_mutating(name) {
                return confirm(&tools::preview(name, input));
            }
            if let Some(p) = &path {
                if tools::escapes_cwd(p, cwd) {
                    return confirm(&format!("read outside working directory: {}", p.display()));
                }
            }
            None
        }
    }
}

// One ReAct turn-and-tools loop. Shared by BUILD and the execution half of PLAN.
// The Stop hook always fires once the turn ends, however it ended.
pub fn run_build(p: &Provider, perm: Permission, role_id: &str, task: &str, cwd: &Path) -> Result<(), String> {
    let r = build_inner(p, perm, role_id, task, cwd, 0).map(|_| ());
    hooks::notify("Stop", cwd);
    r
}

// Returns the final assistant text / finish summary, so a parent can read a
// spawned subagent's result. `depth` bounds recursion via spawn_subagent.
fn build_inner(p: &Provider, perm: Permission, role_id: &str, task: &str, cwd: &Path, depth: usize) -> Result<String, String> {
    let task = match hooks::user_prompt_submit(task, cwd) {
        Err(reason) => {
            report::error(&format!("blocked by hook: {reason}"));
            return Ok(String::new());
        }
        Ok(ctx) if !ctx.is_empty() => format!("{task}\n\n[hook context]\n{ctx}"),
        Ok(_) => task.to_string(),
    };
    let defs = tools::defs(depth < MAX_DEPTH);
    let mut msgs: Vec<Msg> = vec![Msg::System(role(role_id).system.into()), Msg::User(task)];

    for _ in 0..MAX_ITERS {
        if tui::interrupted() {
            report::notice("interrupted");
            return Ok(String::new());
        }
        // JSON mode buffers (one clean event); human mode streams tokens live,
        // with a spinner covering the gap until the first token so a slow model
        // never looks frozen.
        let reply: Reply = if report::is_json() {
            let r = complete(p, &msgs, &defs)?;
            report::assistant(&r.text);
            r
        } else {
            let mut spin = Some(tui::spinner_start("thinking…"));
            let mut streamed = false;
            let res = provider::stream(p, &msgs, &defs, &mut |c| {
                if let Some(s) = spin.take() {
                    tui::spinner_stop(s);
                }
                report::assistant_delta(c);
                streamed = true;
            });
            if let Some(s) = spin.take() {
                tui::spinner_stop(s); // no text streamed (tool-only turn) — clear it
            }
            let r = res?;
            if streamed {
                report::assistant_end();
            }
            r
        };
        if reply.calls.is_empty() {
            if reply.text.trim().is_empty() {
                report::notice("model returned no output");
            }
            return Ok(reply.text); // model answered in prose
        }

        let mut results = Vec::new();
        let mut summary: Option<String> = None;
        for call in &reply.calls {
            // Reject calls whose JSON arguments didn't parse, with a message the
            // model can act on (common failure mode for small local models).
            if let Some(raw) = call.input.get(tools::INVALID_ARGS).and_then(|v| v.as_str()) {
                let msg = format!("tool arguments were not valid JSON: {}", raw.chars().take(200).collect::<String>());
                report::tool_denied(&msg);
                results.push(ToolResult { id: call.id.clone(), content: msg, is_error: true });
                continue;
            }
            // Show the proposed action (with an inline diff for edits) first, so
            // the approval prompt that follows has visible context.
            report::tool_call(&call.name, &tools::preview(&call.name, &call.input), &call.input);

            // PreToolUse hook can allow (skip the gate), deny, or defer to it.
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

            if call.name == "spawn_subagent" {
                let out = spawn_subagent(p, perm, &call.input, cwd, depth);
                report::tool_result(&call.name, &out, false);
                results.push(ToolResult { id: call.id.clone(), content: out, is_error: false });
                continue;
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
        if let Some(s) = summary {
            return Ok(s);
        }
    }
    Err(format!("reached the {MAX_ITERS}-step limit without finishing"))
}

static SUB_SEQ: AtomicUsize = AtomicUsize::new(0);

// Delegate a self-contained sub-task to a fresh agent (own context window).
// `isolate` runs it in a new git worktree so parallel edits never collide.
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
    let result = build_inner(p, perm, role, task, &run_cwd, depth + 1)
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

// PLAN: ask for a numbered breakdown, let the user approve/edit, then execute.
pub fn run_plan(p: &Provider, perm: Permission, task: &str, cwd: &Path) -> Result<(), String> {
    let sys = "You are a planning engineer. Break the user's task into a concise numbered list of concrete steps (no prose, one step per line, max 8 steps). Output ONLY the list.";
    let msgs = vec![Msg::System(sys.into()), Msg::User(task.into())];
    let reply = tui::with_spinner("planning…", || complete(p, &msgs, &[]))?;

    let mut steps: Vec<String> = reply.text.lines()
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

// BRAINSTORM: free-form chat, no tools, history retained across turns.
pub fn run_brainstorm(p: &Provider, first: &str) -> Result<(), String> {
    let mut msgs: Vec<Msg> = vec![Msg::System(
        "You are a sharp, concise thought partner. Offer ideas, tradeoffs, and concrete suggestions. No fluff.".into(),
    )];
    let mut question = first.to_string();
    loop {
        msgs.push(Msg::User(question.clone()));
        tui::line("");
        let mut streamed = false;
        let reply = provider::stream(p, &msgs, &[], &mut |c| {
            report::assistant_delta(c);
            streamed = true;
        })?;
        if streamed {
            report::assistant_end();
        }
        tui::line("");
        msgs.push(Msg::Assistant { text: reply.text, calls: vec![] });

        match tui::ask(&format!("{} ", tui::blue("you ›"))) {
            None => return Ok(()),
            Some(f) => {
                let t = f.trim();
                if t.is_empty() || t == "exit" || t == "done" {
                    return Ok(());
                }
                question = t.to_string();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── permission ──────────────────────────────────────────────────────────
    #[test]
    fn permission_parsing() {
        assert!(matches!(permission("auto"), Permission::Auto));
        assert!(matches!(permission("readonly"), Permission::ReadOnly));
        assert!(matches!(permission("ask"), Permission::Ask));
        assert!(matches!(permission("anything-else"), Permission::Ask));
        assert!(matches!(permission(""), Permission::Ask));
    }

    // ── role ────────────────────────────────────────────────────────────────
    #[test]
    fn role_selection() {
        assert!(role("researcher").system.contains("research"));
        assert!(role("engineer").system.contains("software engineer"));
        // Unknown roles fall back to the engineer prompt.
        assert!(role("ceo").system.contains("software engineer"));
    }

    // ── gate ────────────────────────────────────────────────────────────────
    // These run with a non-terminal stdin, so any path that would prompt resolves
    // to a denial (Some(reason)) rather than blocking.
    #[test]
    fn gate_auto_allows_ordinary_mutation() {
        let cwd = Path::new("/proj");
        let r = gate(Permission::Auto, "write_file", &json!({"path": "a.txt", "content": "x"}), cwd);
        assert!(r.is_none());
    }

    #[test]
    fn gate_auto_still_confirms_sensitive_path() {
        let cwd = Path::new("/proj");
        // Sensitive even under auto → would prompt → denied in non-terminal tests.
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
    fn gate_readonly_blocks_out_of_cwd_read() {
        let cwd = Path::new("/proj/work");
        let r = gate(Permission::ReadOnly, "read_file", &json!({"path": "/etc/passwd"}), cwd);
        assert!(r.unwrap().contains("outside"));
    }

    #[test]
    fn gate_readonly_allows_in_cwd_read() {
        let cwd = Path::new("/proj/work");
        let r = gate(Permission::ReadOnly, "read_file", &json!({"path": "/proj/work/a.rs"}), cwd);
        assert!(r.is_none());
    }

    #[test]
    fn gate_ask_prompts_for_mutation() {
        let cwd = Path::new("/proj");
        // Would prompt → denied in non-terminal test env.
        let r = gate(Permission::Ask, "write_file", &json!({"path": "a", "content": "x"}), cwd);
        assert!(r.is_some());
    }

    #[test]
    fn gate_ask_allows_in_cwd_read() {
        let cwd = Path::new("/proj");
        let r = gate(Permission::Ask, "read_file", &json!({"path": "/proj/a.rs"}), cwd);
        assert!(r.is_none());
    }
}
