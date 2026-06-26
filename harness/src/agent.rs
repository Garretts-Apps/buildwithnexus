// The orchestration that replaces LangGraph: plain Rust control flow for the
// three modes. PLAN decomposes + gates, BUILD is a ReAct tool loop, BRAINSTORM
// is streaming-free chat. Roles are a small data table, not an agent framework.

use std::path::Path;

use crate::provider::{complete, Msg, Provider, Reply, ToolResult};
use crate::tools;
use crate::tui;

const MAX_ITERS: usize = 30;

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

// Returns Some(reason) when a call is blocked, None when allowed.
fn gate(perm: Permission, name: &str, input: &serde_json::Value) -> Option<String> {
    if !tools::is_mutating(name) {
        return None;
    }
    match perm {
        Permission::Auto => None,
        Permission::ReadOnly => Some("read-only mode: mutation skipped".into()),
        Permission::Ask => {
            let q = format!("  {} {}? {} ", tui::yellow("➤"), tui::bold(&tools::preview(name, input)), tui::dim("[y/N]"));
            let ans = tui::ask(&q).unwrap_or_default();
            if matches!(ans.trim().to_lowercase().as_str(), "y" | "yes") {
                None
            } else {
                Some("denied by user".into())
            }
        }
    }
}

// One ReAct turn-and-tools loop. Shared by BUILD and the execution half of PLAN.
pub fn run_build(p: &Provider, perm: Permission, role_id: &str, task: &str, cwd: &Path) -> Result<(), String> {
    let defs = tools::defs();
    let mut msgs: Vec<Msg> = vec![Msg::System(role(role_id).system.into()), Msg::User(task.into())];

    for _ in 0..MAX_ITERS {
        let reply: Reply = tui::with_spinner("thinking…", || complete(p, &msgs, &defs))?;

        if !reply.text.trim().is_empty() {
            tui::line(&reply.text);
        }
        if reply.calls.is_empty() {
            return Ok(()); // model answered in prose — nothing left to execute
        }

        let mut results = Vec::new();
        let mut finished = false;
        for call in &reply.calls {
            if let Some(reason) = gate(perm, &call.name, &call.input) {
                tui::line(&tui::dim(&format!("  ✗ {}", reason)));
                results.push(ToolResult { id: call.id.clone(), content: reason, is_error: true });
                continue;
            }
            tui::line(&tui::dim(&format!("  • {}", tools::preview(&call.name, &call.input))));
            let out = tools::run(&call.name, &call.input, cwd);
            if out.finished {
                tui::line("");
                tui::line(&tui::green(&format!("✨ {}", out.content)));
                finished = true;
            }
            results.push(ToolResult { id: call.id.clone(), content: out.content, is_error: out.is_error });
        }

        msgs.push(Msg::Assistant { text: reply.text, calls: reply.calls });
        msgs.push(Msg::Tool(results));
        if finished {
            return Ok(());
        }
    }
    tui::line(&tui::yellow(&format!("Reached the {MAX_ITERS}-step limit without finishing.")));
    Ok(())
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
        let reply = tui::with_spinner("thinking…", || complete(p, &msgs, &[]))?;
        tui::line("");
        tui::line(&reply.text);
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
