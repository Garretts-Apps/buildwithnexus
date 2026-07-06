// The orchestration: plain Rust control flow for all three modes. PLAN
// decomposes + gates, BUILD is a ReAct tool loop, BRAINSTORM is conversational —
// but ALL modes have access to the full tool surface so the model can grep,
// fetch, read files, and run commands regardless of which mode the user is in.

use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::config;
use crate::hooks::{self, PreDecision};
use crate::provider::{self, complete, Msg, Provider, Reply, ToolResult};
use crate::report;
use crate::tools;
use crate::trace;
use crate::tui;

const MAX_ITERS: usize = 30;
const MAX_DEPTH: usize = 3;
const KEEP_RECENT: usize = 6;
const MAX_IDENTICAL_TOOL_RESULTS: usize = 2;
const MAX_PLAN_TOOL_ROUNDS: usize = 10;
const MAX_CHAT_TOOL_ROUNDS: usize = 12;

// ── context compaction ────────────────────────────────────────────────────────
fn estimate_tokens(msgs: &[Msg]) -> usize {
    let mut chars = 0usize;
    for m in msgs {
        chars += match m {
            Msg::System(s) | Msg::User(s) => s.len(),
            Msg::UserImages { text, images } => {
                text.len() + images.iter().map(|(_, d)| d.len() / 3).sum::<usize>()
            }
            Msg::Assistant { text, calls } => {
                text.len()
                    + calls
                        .iter()
                        .map(|c| c.name.len() + c.input.to_string().len())
                        .sum::<usize>()
            }
            Msg::Tool(rs) => rs.iter().map(|r| r.content.len()).sum(),
        };
    }
    chars / 4
}

fn compaction_split(msgs: &[Msg]) -> (usize, usize) {
    let sys_end = msgs
        .iter()
        .take_while(|m| matches!(m, Msg::System(_)))
        .count();
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

fn assistant_tool_turn_text(text: String, calls: &[provider::ToolCall]) -> String {
    if calls.is_empty() || text.len() <= 500 {
        return text;
    }
    let actions = calls
        .iter()
        .map(|c| tools::preview(&c.name, &c.input))
        .collect::<Vec<_>>()
        .join("; ");
    format!("Requested tool call(s): {actions}")
}

fn render_msgs(msgs: &[Msg]) -> String {
    let mut s = String::new();
    for m in msgs {
        match m {
            Msg::System(t) => {
                s.push_str("system: ");
                s.push_str(t);
            }
            Msg::User(t) => {
                s.push_str("user: ");
                s.push_str(t);
            }
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
    // Even if there's nothing to summarize, truncate bloated tool results
    let msgs = truncate_tool_results(msgs);
    if tail_start <= sys_end {
        return msgs;
    }
    let mut it = msgs.into_iter();
    let system: Vec<Msg> = it.by_ref().take(sys_end).collect();
    let middle: Vec<Msg> = it.by_ref().take(tail_start - sys_end).collect();
    let tail: Vec<Msg> = it.collect();
    let summary = summarize(&middle);
    let mut v = system;
    v.push(Msg::User(format!(
        "[Summary of earlier conversation, compacted to save context]\n{summary}"
    )));
    v.extend(tail);
    v
}

/// Truncate oversized tool result contents to keep context manageable.
/// Any individual tool result > 800 chars gets clipped.
fn truncate_tool_results(msgs: Vec<Msg>) -> Vec<Msg> {
    const MAX_RESULT_CHARS: usize = 800;
    msgs.into_iter()
        .map(|m| match m {
            Msg::Tool(results) => Msg::Tool(
                results
                    .into_iter()
                    .map(|mut r| {
                        if r.content.len() > MAX_RESULT_CHARS {
                            let truncated: String =
                                r.content.chars().take(MAX_RESULT_CHARS).collect();
                            r.content =
                                format!("{truncated}\n…(truncated, {} chars total)", r.content.len());
                        }
                        r
                    })
                    .collect(),
            ),
            Msg::Assistant { text, calls } => {
                // Also trim overly verbose assistant text in tool turns
                let trimmed = if text.len() > 1200 && !calls.is_empty() {
                    assistant_tool_turn_text(text, &calls)
                } else {
                    text
                };
                Msg::Assistant {
                    text: trimmed,
                    calls,
                }
            }
            other => other,
        })
        .collect()
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

#[derive(Default)]
struct ToolLoopGuard {
    seen: HashMap<String, ToolLoopRecord>,
}

struct ToolLoopRecord {
    content: String,
    is_error: bool,
    count: usize,
}

impl ToolLoopGuard {
    fn note(
        &mut self,
        name: &str,
        input: &serde_json::Value,
        content: &str,
        is_error: bool,
    ) -> Option<String> {
        let key = format!(
            "{name}:{}",
            serde_json::to_string(input).unwrap_or_default()
        );
        let rec = self.seen.entry(key).or_insert_with(|| ToolLoopRecord {
            content: String::new(),
            is_error,
            count: 0,
        });
        if rec.content == content && rec.is_error == is_error {
            rec.count += 1;
        } else {
            rec.content = content.to_string();
            rec.is_error = is_error;
            rec.count = 1;
        }
        if rec.count >= MAX_IDENTICAL_TOOL_RESULTS {
            Some(repeated_tool_summary(name, input, content, is_error))
        } else {
            None
        }
    }
}

fn repeated_tool_summary(
    name: &str,
    input: &serde_json::Value,
    content: &str,
    is_error: bool,
) -> String {
    let preview = tools::preview(name, input);
    let shown = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(20)
        .collect::<Vec<_>>()
        .join("\n");
    if is_error {
        format!(
            "I stopped a repeated tool loop. `{preview}` returned the same error more than once:\n{shown}"
        )
    } else if shown.is_empty() {
        format!("I stopped a repeated tool loop. `{preview}` returned no output more than once.")
    } else {
        format!(
            "I stopped a repeated tool loop. `{preview}` returned the same result more than once, so here is the result I found:\n{shown}"
        )
    }
}

fn note_loop_result(
    guard: &mut ToolLoopGuard,
    name: &str,
    input: &serde_json::Value,
    content: &str,
    is_error: bool,
    summary: &mut Option<String>,
) {
    if summary.is_some() {
        return;
    }
    if let Some(loop_msg) = guard.note(name, input, content, is_error) {
        report::assistant(&loop_msg);
        *summary = Some(loop_msg);
    }
}

fn tool_input_for_execution(
    name: &str,
    input: &serde_json::Value,
    cwd: &Path,
    phase: &str,
    depth: usize,
    task: &str,
) -> serde_json::Value {
    let Some(repaired) = repair_placeholder_tool_input(name, input, cwd, task) else {
        return input.clone();
    };
    report::notice(&format!(
        "  recovery: using current workspace for placeholder/root path in {name}"
    ));
    trace::record_visible(
        "tool_input_repaired",
        format!("{name} path repaired"),
        serde_json::json!({
            "tool": name,
            "original": input,
            "repaired": &repaired,
            "cwd": cwd.to_string_lossy(),
            "phase": phase,
            "depth": depth,
        }),
    );
    repaired
}

fn repair_placeholder_tool_input(
    name: &str,
    input: &serde_json::Value,
    cwd: &Path,
    task: &str,
) -> Option<serde_json::Value> {
    if matches!(name, "write" | "write_file") {
        if let Some(repaired) = repair_write_file_input(input, task) {
            return Some(repaired);
        }
    }

    let keys: &[&str] = match name {
        "list" | "list_dir" | "list_tree" | "file_info" => &["path", "filePath"],
        "glob" | "grep" => &["root", "path"],
        "find_paths" | "find_files" | "grep_files" => &["root"],
        _ => return None,
    };

    let mut repaired = input.clone();
    let mut changed = false;
    let cwd_str = cwd.to_string_lossy().to_string();
    for key in keys {
        if let Some(value) = repaired.get_mut(*key) {
            let Some(path) = value.as_str() else {
                continue;
            };
            if is_placeholder_path(path) || should_repair_filesystem_root(path, task) {
                *value = serde_json::Value::String(cwd_str.clone());
                changed = true;
            }
        }
    }
    changed.then_some(repaired)
}

fn repair_write_file_input(input: &serde_json::Value, task: &str) -> Option<serde_json::Value> {
    let requested = auto_create_file_input(task)?;
    let requested_path = requested["path"].as_str().unwrap_or("");
    let requested_content = requested["content"].as_str().unwrap_or("");
    let path = input
        .get("path")
        .or_else(|| input.get("filePath"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let content = input
        .get("content")
        .or_else(|| input.get("contents"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let should_repair = path.trim().is_empty()
        || matches!(path.trim(), "~" | "." | "/" | "./")
        || is_placeholder_path(path)
        || (path == requested_path && content != requested_content);
    should_repair.then_some(requested)
}

fn should_repair_filesystem_root(path: &str, task: &str) -> bool {
    path.trim() == "/"
        && task_targets_current_workspace(task)
        && !task_explicitly_targets_root(task)
}

fn task_targets_current_workspace(task: &str) -> bool {
    let lower = task.to_ascii_lowercase();
    [
        "this project",
        "current project",
        "the project",
        "this repo",
        "current repo",
        "repository",
        "workspace",
        "codebase",
        "top-level files",
        "top level files",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn task_explicitly_targets_root(task: &str) -> bool {
    let lower = task.to_ascii_lowercase();
    [
        "filesystem root",
        "file system root",
        "root filesystem",
        "root file system",
        "list /",
        "inspect /",
        "search /",
        "from /",
        "under /",
        "starting at /",
        "starting from /",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_placeholder_path(path: &str) -> bool {
    let lower = path.trim().to_ascii_lowercase();
    if lower.is_empty() || lower == "." || lower == "./" || lower == "~" {
        return false;
    }
    lower.contains("/path/to/")
        || lower.contains("path/to/your")
        || lower.contains("your/project")
        || lower.contains("your-project")
        || lower.contains("your_project")
        || lower.contains("project-directory")
        || lower.contains("project_directory")
        || matches!(
            lower.as_str(),
            "/path/to/project" | "path/to/project" | "<project-path>" | "{project-path}"
        )
}

#[derive(Default)]
struct ThinkingStream {
    active: bool,
    step: usize,
}

impl ThinkingStream {
    fn push(&mut self, chunk: &str) {
        if report::is_json() {
            return;
        }
        tui::poll_typeahead();
        let mut rest = chunk.replace('\r', "");
        if rest.is_empty() {
            return;
        }
        while let Some(nl) = rest.find('\n') {
            let part = rest[..nl].trim_end();
            if !part.is_empty() || self.active {
                self.start();
                tui::write_stream(&tui::dim(part));
                tui::write_stream("\n");
                self.active = false;
            }
            rest = rest[nl + 1..].to_string();
        }
        if !rest.is_empty() {
            self.start();
            tui::write_stream(&tui::dim(&rest));
        }
    }

    fn finish(&mut self) {
        if self.active {
            tui::write_stream("\n");
            self.active = false;
        }
        tui::render_queued_composer();
    }

    fn start(&mut self) {
        if !self.active {
            let spinners = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let s = spinners[self.step % spinners.len()];
            self.step += 1;
            tui::write_stream(&format!("  {} {} ", tui::cyan(s), tui::dim("thinking ›")));
            self.active = true;
        }
    }
}

fn request_reply(
    p: &Provider,
    msgs: &[Msg],
    defs: &[tools::ToolDef],
    label: &str,
) -> Result<Reply, String> {
    if report::is_json() {
        let r = complete(p, msgs, defs)?;
        report::assistant(&r.text);
        return Ok(r);
    }
    tui::line(&format!(
        "  {} {} · {} [{}]",
        tui::yellow("⚡"),
        tui::bold(label),
        tui::cyan(&format!("reasoning with {}", p.model)),
        tui::dim(&format!("{} tools available", defs.len()))
    ));
    let thinking = std::cell::RefCell::new(ThinkingStream::default());
    let mut streamed = false;
    let mut renderer = tui::StreamRenderer::new();
    let res = provider::stream(
        p,
        msgs,
        defs,
        &mut |c| {
            thinking.borrow_mut().finish();
            renderer.push(c);
            tui::poll_typeahead();
            tui::render_queued_composer();
            streamed = true;
        },
        &mut |t| {
            thinking.borrow_mut().push(t);
        },
    );
    thinking.borrow_mut().finish();
    let r = res?;
    renderer.flush();
    if streamed {
        report::assistant_end();
    }
    Ok(r)
}

fn answer_question(input: &serde_json::Value) -> (String, bool) {
    let question = if let Some(qs) = input["questions"].as_array() {
        let mut acc = Vec::new();
        for item in qs {
            if let Some(s) = item.as_str() {
                acc.push(s.to_string());
            } else if let Some(s) = item["question"].as_str() {
                acc.push(s.to_string());
            }
        }
        if acc.is_empty() {
            input["question"].as_str().unwrap_or("").to_string()
        } else {
            acc.join("\n")
        }
    } else {
        input["question"].as_str().unwrap_or("").to_string()
    };
    let question = question.trim();
    if question.is_empty() {
        return ("question is required".to_string(), true);
    }

    let mut options_str = String::new();
    if let Some(opts) = input["options"].as_array() {
        options_str.push_str("\nOptions:\n");
        for (i, opt) in opts.iter().enumerate() {
            if let Some(s) = opt.as_str() {
                options_str.push_str(&format!("  {}. {}\n", i + 1, s));
            } else {
                let label = opt["label"].as_str().unwrap_or("");
                let desc = opt["description"].as_str().unwrap_or("");
                if !desc.is_empty() {
                    options_str.push_str(&format!("  {}. {} - {}\n", i + 1, label, desc));
                } else {
                    options_str.push_str(&format!("  {}. {}\n", i + 1, label));
                }
            }
        }
    }

    let full_prompt = format!("{}{}", question, options_str);
    if report::is_json() || !std::io::stdin().is_terminal() {
        return (
            format!("blocked (no interactive terminal to answer question: {full_prompt})"),
            true,
        );
    }
    let default = input["default"].as_str().unwrap_or("").trim();
    let prompt = if default.is_empty() {
        format!(
            "  {} {}\n  Answer: ",
            tui::yellow("?"),
            tui::bold(&full_prompt)
        )
    } else {
        format!(
            "  {} {}\n  Answer {} ",
            tui::yellow("?"),
            tui::bold(&full_prompt),
            tui::dim(&format!("[{default}]"))
        )
    };
    let ans = tui::ask(&prompt).unwrap_or_default();
    let out = if ans.trim().is_empty() && !default.is_empty() {
        default.to_string()
    } else {
        ans
    };
    (out, false)
}

// ── permissions ───────────────────────────────────────────────────────────────
#[derive(Clone, Copy)]
pub enum Permission {
    Ask,
    Auto,
    ReadOnly,
}

pub fn permission(s: &str) -> Permission {
    let normalized = s.trim().to_ascii_lowercase().replace(['_', '-'], "");
    match normalized.as_str() {
        "auto" | "acceptedits" | "acceptedit" | "bypasspermissions" => Permission::Auto,
        "readonly" | "read" | "plan" | "dontask" => Permission::ReadOnly,
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
When writing or editing files, provide the complete, fully working code. NEVER use placeholders (e.g. `// ... rest of code`). \
BUILD mode means execute: for any concrete build/fix/create/change request, inspect the project and make the needed edits instead of replying with a capability statement. \
For simple browser games, canvas demos, landing pages, prototypes, and static visual apps, build an actual runnable artifact. Prefer a single self-contained HTML file via Artifact/publish_artifact unless the user explicitly asks for a framework. \
For local web apps that require a dev server, use start_server, wait_for_url, inspect read_server_log if readiness fails, then open_browser when useful. \
If a path or file is missing, use discovery tools before asking the user. \
DO NOT ask the user for permission, themes, or choices unless absolutely necessary. If the user leaves something open-ended (e.g. 'pick a theme' or 'make it cool'), MAKE A REASONABLE DECISION and proceed immediately. \
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
// When context_tokens is small (≤16K), skip expensive sections to leave
// room for tool definitions + actual conversation.
fn context_prefix(cwd: &Path, context_tokens: usize) -> String {
    let compact = context_tokens <= 16_384;
    let mut parts: Vec<String> = Vec::new();

    let home_str = std::env::var("HOME").unwrap_or_default();
    let mut path_info = format!(
        "[Environment Paths]\n\
         Workspace (CWD): {}\n\
         User Home (~): {}\n",
        cwd.display(),
        home_str
    );
    if cwd.to_string_lossy().contains("/tmp/") || cwd.to_string_lossy().contains("/private/tmp/") {
        path_info.push_str(
            "Note: The current workspace is running in a temporary directory/mount. \
             To find personal folders or files belonging to the user, you must search starting from the User Home (~).\n"
        );
    }
    parts.push(path_info);

    // Always include the tool manifest so the model knows what's built-in
    // and doesn't try to install external tools to do things we already handle.
    parts.push(tool_manifest());
    if !compact {
        parts.push(visible_reasoning_policy());
        parts.push(discovery_policy());
        parts.push(skill_manifest());
    }

    // Probe which common system tools are actually present so the model can
    // pick the right one without guessing or installing alternatives.
    // Skip for small contexts — the tool defs already dominate.
    if !compact {
        let env_snap = env_snapshot();
        if !env_snap.is_empty() {
            parts.push(format!(
                "[Environment — tools already installed]\n{env_snap}"
            ));
        }
    }

    if let Some(mem) = config::load_memory() {
        // Memory is user-important; always include but truncate for small ctx
        let mem_text = if compact && mem.len() > 300 {
            format!("{}…", &mem[..300])
        } else {
            mem
        };
        parts.push(format!("[Memory from previous sessions]\n{mem_text}"));
    }
    if let Some(agents) = config::load_agents() {
        trace::record_visible(
            "agents",
            "loaded Agents.md",
            serde_json::json!({"bytes": agents.len(), "preview": trace::preview(&agents, 600)}),
        );
        // For small contexts, truncate Agents.md to avoid blowing the budget
        let agents_text = if compact && agents.len() > 500 {
            format!("{}…", &agents[..500])
        } else {
            agents
        };
        parts.push(format!("[Agent knowledge — Agents.md]\n{agents_text}"));
    }

    // Skip rules, knowledge, hooks, and skill descriptions for small contexts
    if !compact {
        // Inject active rules for operational judgment
        let mut engine = crate::rules::RuleEngine::load_defaults();
        let rules_dir = config::home().join("rules");
        if let Ok(rd) = std::fs::read_dir(&rules_dir) {
            for e in rd.flatten() {
                if let Ok(loaded) =
                    crate::rules::RuleEngine::load_from_file(&e.path().to_string_lossy())
                {
                    for r in loaded.rules {
                        engine.add_rule(r);
                    }
                }
            }
        }
        let mut rules_summary = String::from("The following engineering constraints and business logic rules MUST be strictly enforced for operational judgment:\n");
        for r in &engine.rules {
            if r.enabled {
                rules_summary.push_str(&format!(
                    "• [{}] {} — {}\n",
                    r.severity, r.id, r.description
                ));
            }
        }
        parts.push(format!(
            "[Operational Judgment — Engineering Constraints & Rules]\n{}",
            rules_summary.trim()
        ));

        // Inject structured knowledge base if present
        let kb = crate::knowledge::KnowledgeBase::new(".");
        if !kb.entities.is_empty() {
            parts.push(format!(
                "[Structured Knowledge Base — Known Project Entities]\n{}",
                kb.generate_context_summary(&[])
            ));
        }

        let active_hooks = hooks::list_active();
        if !active_hooks.is_empty() {
            parts.push(format!("[Active Hooks]\n{}", active_hooks.join("\n")));
        }
        let skill_descs = config::load_skill_descriptions();
        if !skill_descs.is_empty() {
            let joined = skill_descs
                .iter()
                .map(|(name, desc)| format!("• /{name} — {desc}"))
                .collect::<Vec<_>>()
                .join("\n");
            parts.push(format!(
                "[Available skills — descriptions only]\n{joined}\n\n\
    To use a skill, call load_skill with the skill name to load its full instructions. \
    Only load skills that are directly relevant to the current task. \
    Do not load skills for simple greetings or casual replies."
            ));
        }
    }

    format!("{}\n\n", parts.join("\n\n"))
}

fn tool_manifest() -> String {
    "[Built-in tools — always available, no install needed]\n\
Aliases are supported: bash/read/write/edit/patch/glob/grep/list/task/todowrite/todoread/webfetch/websearch/skill/question. \
Use built-ins before installing anything. Use list_tree/find_paths/grep_files before guessing paths; use find_paths kind=`dir` for folders. \
Use start_server/list_servers/wait_for_url/read_server_log/stop_server for long-running local dev servers, and open_browser for local URLs or generated HTML.\n\n\
[CRITICAL TOOL DISCIPLINE]\n\
• For generated/edited code, HTML, or file contents, call write_file/edit_file/Artifact; never paste code as plain markdown.\n\
• For canvas games, browser games, standalone demos, landing pages, and small web apps: publish a complete runnable static HTML artifact with embedded CSS/JS unless a framework is explicitly requested. Include controls, restart/error states, responsive sizing, and touch/mobile support when useful; then open_browser if possible.\n\
• When tasked to build, create, or write code/applications/games, build them locally from scratch. NEVER search the web or attempt to fetch non-existent repositories or URLs from GitHub or the internet unless the user explicitly provides a URL or asks to download from external sources.\n\
• No placeholders. No asking for theme/layout/permission when a reasonable default works. Build immediately."
        .to_string()
}

fn visible_reasoning_policy() -> String {
    "[Visible reasoning]\n\
Emit short operational `<think>...</think>` notes before tool calls, recovery decisions, and final answers. Think briefly, then act."
        .to_string()
}

fn discovery_policy() -> String {
    "[Filesystem discovery policy]\n\
Do not invent placeholder paths. If a path is vague or a read/list fails, search with list_tree/find_paths/find_files/grep_files before asking. \
For folders use find_paths kind=`dir`. For user files/projects, search likely roots such as ~, ~/Documents, ~/Desktop, ~/Downloads, ~/Projects, ~/repos, then bounded parent directories. \
Only ask for a path after bounded discovery fails or the search would be too broad/sensitive."
        .to_string()
}

fn skill_manifest() -> String {
    let names = config::bundled_skills()
        .into_iter()
        .map(|(name, _)| format!("/{name}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "[Skill invocation policy]\n\
Skills are first-class operating instructions. Before substantial work, choose and follow the most relevant skill from the loaded skills. \
If the user names a skill or uses a skill slash command, treat that skill as active context. \
Bundled skills are callable by users as slash commands: {names}. \
Use /trace to inspect evidence of skill loading, tool calls, hooks, and subagents."
    )
}

fn env_snapshot() -> String {
    // Probe a fixed list of common tools so the model knows what's available.
    // We run everything in parallel-ish with short timeouts via sequential calls —
    // the total overhead is ~10ms on a modern machine.
    let probes: &[(&str, &str)] = &[
        ("node", "node --version"),
        ("npm", "npm --version"),
        ("npx", "npx --version"),
        ("python3", "python3 --version"),
        ("pip3", "pip3 --version"),
        ("cargo", "cargo --version"),
        ("rustc", "rustc --version"),
        ("git", "git --version"),
        ("docker", "docker --version"),
        ("jq", "jq --version"),
        ("rg", "rg --version"),
        ("gh", "gh --version"),
        ("bun", "bun --version"),
        ("deno", "deno --version"),
        ("go", "go version"),
        ("ruby", "ruby --version"),
        ("java", "java --version"),
    ];

    use std::process::Command;
    let mut found: Vec<String> = Vec::new();
    for (label, cmd) in probes {
        let parts: Vec<&str> = cmd.splitn(2, ' ').collect();
        let bin = parts[0];
        let arg = parts.get(1).copied().unwrap_or("--version");
        let ok = Command::new(bin)
            .arg(arg)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .map(|o| {
                let txt = String::from_utf8_lossy(&o.stdout).trim().to_string();
                let txt = if txt.is_empty() {
                    String::from_utf8_lossy(&o.stderr).trim().to_string()
                } else {
                    txt
                };
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

static SESSION_ALLOWED_TOOLS: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn is_session_allowed_tool(key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    if let Ok(guard) = SESSION_ALLOWED_TOOLS.lock() {
        if let Some(set) = guard.as_ref() {
            return set.contains(key);
        }
    }
    false
}

fn add_session_allowed_tool(key: &str) {
    if key.is_empty() {
        return;
    }
    if let Ok(mut guard) = SESSION_ALLOWED_TOOLS.lock() {
        let set = guard.get_or_insert_with(HashSet::new);
        set.insert(key.to_string());
    }
}

#[allow(dead_code)]
fn confirm(label: &str) -> Option<String> {
    confirm_tool(label, "")
}

fn confirm_tool(label: &str, tool_key: &str) -> Option<String> {
    if report::is_json() || !std::io::stdin().is_terminal() {
        return Some(format!(
            "blocked (no interactive terminal to confirm: {label})"
        ));
    }
    let q = format!(
        "  {} {}? {} ",
        tui::yellow("➤"),
        tui::bold(label),
        tui::dim("[y: yes | n: no | s: allow session | a: allow always | d <reason>: deny with feedback]")
    );
    let ans = tui::ask(&q).unwrap_or_default();
    let trimmed = ans.trim();
    let lower = trimmed.to_lowercase();
    if matches!(lower.as_str(), "y" | "yes") {
        None
    } else if matches!(lower.as_str(), "s" | "session") {
        add_session_allowed_tool(tool_key);
        None
    } else if matches!(lower.as_str(), "a" | "always") {
        add_session_allowed_tool(tool_key);
        if !tool_key.is_empty() {
            if let Some(mut s) = crate::config::load_settings() {
                if !s.allowed_commands.contains(&tool_key.to_string()) {
                    s.allowed_commands.push(tool_key.to_string());
                    crate::config::save_settings(&s);
                }
            }
        }
        None
    } else if lower.starts_with("d ")
        || lower.starts_with("deny ")
        || lower.starts_with("n ")
        || lower.starts_with("no ")
    {
        let idx = trimmed.find(' ').unwrap_or(0);
        let feedback = trimmed[idx..].trim();
        if feedback.is_empty() {
            Some("denied by user".into())
        } else {
            Some(format!("denied by user with feedback: {feedback}"))
        }
    } else {
        Some("denied by user".into())
    }
}

// Permission gate — returns Some(reason) when blocked.
// Read operations outside CWD are allowed in all modes (no CWD confinement for
// reads); the user specifically asked for full filesystem access.
pub(crate) fn gate(
    perm: Permission,
    name: &str,
    input: &serde_json::Value,
    cwd: &Path,
) -> Option<String> {
    let tool_key = if let Some(c) = tools::command_arg_for(name, input) {
        let first = c.split_whitespace().next().unwrap_or("");
        std::path::Path::new(first)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(first)
            .to_string()
    } else {
        name.to_string()
    };

    let path = tools::touched_path(name, input, cwd);

    if let Some(p) = &path {
        if tools::is_sensitive(p) {
            return confirm_tool(&format!("access sensitive path {}", p.display()), &tool_key);
        }
        // In WSL2, writing to a Windows drive mount (/mnt/c/, /mnt/d/, etc.)
        // crosses the OS boundary — always confirm, even in Auto mode.
        if tools::is_mutating_call(name, input) && tools::is_wsl_windows_mount(p) {
            return confirm_tool(
                &format!(
                    "write to Windows filesystem {} (WSL2 boundary)",
                    p.display()
                ),
                &tool_key,
            );
        }
    }
    if let Some(c) = tools::command_arg_for(name, input) {
        if tools::catastrophic(c) {
            return confirm_tool(&format!("run dangerous command `{c}`"), &tool_key);
        }
        // In WSL2, commands that reference /mnt/<drive>/ target the Windows
        // filesystem — confirm before running, even in Auto mode.
        if tools::is_wsl() && tools::command_touches_wsl_mount(c) {
            return confirm_tool(
                &format!("command targets Windows filesystem (WSL2): `{c}`"),
                &tool_key,
            );
        }
    }

    match perm {
        Permission::Auto => None,
        Permission::ReadOnly => {
            if tools::is_mutating_call(name, input) {
                // run_command with clearly read-only shell tools (grep, find, etc.)
                // should pass through even in ReadOnly mode.
                if let Some(c) = tools::command_arg_for(name, input) {
                    if tools::is_readonly_command(c) {
                        return None;
                    }
                }
                return Some("read-only mode: mutation skipped".into());
            }
            None // reads anywhere are allowed in readonly mode
        }
        Permission::Ask => {
            // run_command calls whose binary appears in allowed_commands skip the
            // confirmation prompt — git, cargo, npm, etc. should just work.
            if config::load_allowed_commands()
                .iter()
                .any(|a| a == &tool_key)
                || is_session_allowed_tool(&tool_key)
            {
                return None;
            }
            if tools::is_mutating_call(name, input) {
                return confirm_tool(&tools::preview(name, input), &tool_key);
            }
            // Out-of-cwd reads: just note it instead of hard-blocking.
            // The user asked for full filesystem access.
            None
        }
    }
}

// Public compact helper for the /compact REPL command.
pub fn compact_msgs(p: &Provider, msgs: Vec<Msg>) -> Vec<Msg> {
    compact_with(msgs, |middle| model_summary(p, middle))
}

// ── BUILD mode ────────────────────────────────────────────────────────────────
pub fn run_build(
    p: &Provider,
    perm: Permission,
    role_id: &str,
    task: &str,
    cwd: &Path,
) -> Result<(), String> {
    let mut transcript: Vec<Msg> = Vec::new();
    run_build_session(
        p,
        perm,
        role_id,
        task,
        cwd,
        &mut transcript,
        &crate::session::new_id(),
    )
}

pub fn run_build_resumed(
    p: &Provider,
    perm: Permission,
    role_id: &str,
    task: &str,
    cwd: &Path,
    mut seed: Vec<Msg>,
    sid: &str,
) -> Result<(), String> {
    run_build_session(p, perm, role_id, task, cwd, &mut seed, sid)
}

pub fn run_build_session(
    p: &Provider,
    perm: Permission,
    role_id: &str,
    task: &str,
    cwd: &Path,
    transcript: &mut Vec<Msg>,
    sid: &str,
) -> Result<(), String> {
    hooks::notify("SessionStart", cwd);
    let r = build_inner(p, perm, role_id, task, cwd, 0, transcript).map(|_| ());
    hooks::notify("SessionEnd", cwd);
    hooks::notify("Stop", cwd);
    crate::session::save(sid, cwd, &p.model, transcript);
    r
}

fn build_inner(
    p: &Provider,
    perm: Permission,
    role_id: &str,
    task: &str,
    cwd: &Path,
    depth: usize,
    msgs: &mut Vec<Msg>,
) -> Result<String, String> {
    let task = match hooks::user_prompt_submit(task, cwd) {
        Err(reason) => {
            report::error(&format!("blocked by hook: {reason}"));
            return Ok(String::new());
        }
        Ok(ctx) if !ctx.is_empty() => format!("{task}\n\n[hook context]\n{ctx}"),
        Ok(_) => task.to_string(),
    };
    let task_for_recovery = recovery_task_text(&task);
    let defs = if matches!(perm, Permission::ReadOnly) {
        tools::defs_readonly()
    } else {
        tools::defs_for_context(depth < MAX_DEPTH, p.context_tokens)
    };
    if msgs.is_empty() {
        let prefix = context_prefix(cwd, p.context_tokens);
        let sys = format!("{prefix}{}", role(role_id).system);
        msgs.push(Msg::System(sys));
    }
    // If the caller already pushed a UserImages message (multimodal input), use its
    // text as the task without pushing another User turn; otherwise push normally.
    let already_pushed = matches!(msgs.last(), Some(Msg::UserImages { .. }));
    if !already_pushed {
        msgs.push(Msg::User(task.clone()));
    }

    // Track which files have been read this session so we can enforce read-before-write.
    let mut read_paths: std::collections::HashSet<PathBuf> = Default::default();
    let mut loop_guard = ToolLoopGuard::default();
    let mut artifact_error_count = 0usize;
    let mut static_artifact_recovery_count = 0usize;

    for step in 1..=MAX_ITERS {
        if tui::interrupted() {
            report::notice("interrupted");
            return Ok(String::new());
        }
        maybe_compact(p, msgs);
        if step > 1 && !report::is_json() {
            tui::line(&tui::dim(&format!("  ↻ step {step}")));
        }
        let start_instant = std::time::Instant::now();
        let reply = match request_reply(p, msgs.as_slice(), &defs, "thinking") {
            Ok(r) => r,
            Err(e) => {
                hooks::notify("OnError", cwd);
                return Err(e);
            }
        };
        let elapsed = start_instant.elapsed().as_secs_f64();
        let gen_toks = (reply.text.len() / 4) + (reply.calls.len() * 40);
        if !report::is_json() {
            tui::inference_telemetry(gen_toks.max(10), elapsed);
        }
        hooks::notify("PostResponse", cwd);
        if reply.calls.is_empty() {
            if let Some(input) = auto_create_file_input(&task_for_recovery) {
                if let Some(reason) = gate(perm, "write_file", &input, cwd) {
                    report::tool_denied(&reason);
                    return Err(reason);
                }
                report::tool_call("write_file", &tools::preview("write_file", &input), &input);
                trace_tool_call("write_file", &input, "build", depth);
                let out = tools::run("write_file", &input, cwd);
                hooks::post_tool_use("write_file", &input, &out.content, out.is_error, cwd);
                report::tool_result("write_file", &out.content, out.is_error);
                trace_tool_result("write_file", &out.content, out.is_error, "build", depth);
                if out.is_error {
                    return Err(out.content);
                }
                return Ok(format!(
                    "{}\n\nCompleted the explicit file creation task after the model returned prose without tool calls.",
                    out.content
                ));
            }
            if let Some(input) = auto_static_artifact_input(&task_for_recovery, &reply.text) {
                report::tool_call("Artifact", &tools::preview("Artifact", &input), &input);
                trace_tool_call("Artifact", &input, "build", depth);
                let out = tools::run("Artifact", &input, cwd);
                report::tool_result("Artifact", &out.content, out.is_error);
                trace_tool_result("Artifact", &out.content, out.is_error, "build", depth);
                if out.is_error {
                    if canvas_game_requested(&task_for_recovery) {
                        let fallback = fallback_canvas_game_input(&task_for_recovery);
                        report::tool_call(
                            "Artifact",
                            &tools::preview("Artifact", &fallback),
                            &fallback,
                        );
                        trace_tool_call("Artifact", &fallback, "build", depth);
                        let fallback_out = tools::run("Artifact", &fallback, cwd);
                        report::tool_result(
                            "Artifact",
                            &fallback_out.content,
                            fallback_out.is_error,
                        );
                        trace_tool_result(
                            "Artifact",
                            &fallback_out.content,
                            fallback_out.is_error,
                            "build",
                            depth,
                        );
                        if !fallback_out.is_error {
                            return Ok(format!(
                                "{}\n\nThe model produced incomplete canvas HTML, so buildwithnexus published a complete self-contained fallback canvas game.",
                                fallback_out.content
                            ));
                        }
                        return Err(fallback_out.content);
                    }
                    msgs.push(Msg::Assistant {
                        text: reply.text.clone(),
                        calls: vec![],
                    });
                    msgs.push(Msg::User(format!(
                        "The HTML artifact you wrote in plain text was rejected: {}. \
                         Rewrite it as one complete self-contained HTML artifact and call Artifact/publish_artifact. \
                         Do not answer with markdown code.",
                        out.content
                    )));
                    continue;
                }
                msgs.push(Msg::Assistant {
                    text: reply.text.clone(),
                    calls: vec![],
                });
                return Ok(out.content);
            }
            if static_artifact_requested(&task_for_recovery) {
                msgs.push(Msg::Assistant {
                    text: reply.text.clone(),
                    calls: vec![],
                });
                if static_artifact_recovery_count == 0 {
                    static_artifact_recovery_count += 1;
                    msgs.push(Msg::User(static_artifact_recovery_prompt(
                        &task_for_recovery,
                    )));
                    continue;
                }
                if canvas_game_requested(&task_for_recovery) {
                    let fallback = fallback_canvas_game_input(&task_for_recovery);
                    report::tool_call(
                        "Artifact",
                        &tools::preview("Artifact", &fallback),
                        &fallback,
                    );
                    trace_tool_call("Artifact", &fallback, "build", depth);
                    let fallback_out = tools::run("Artifact", &fallback, cwd);
                    report::tool_result("Artifact", &fallback_out.content, fallback_out.is_error);
                    trace_tool_result(
                        "Artifact",
                        &fallback_out.content,
                        fallback_out.is_error,
                        "build",
                        depth,
                    );
                    if !fallback_out.is_error {
                        return Ok(format!(
                            "{}\n\nThe model did not produce a runnable canvas artifact after being asked to proceed with reasonable defaults, so buildwithnexus published a complete self-contained fallback canvas game.",
                            fallback_out.content
                        ));
                    }
                    return Err(fallback_out.content);
                }
                return Ok(reply.text);
            }
            if reply.text.trim().is_empty() {
                report::notice("model returned no output");
            }
            msgs.push(Msg::Assistant {
                text: reply.text.clone(),
                calls: vec![],
            });
            return Ok(reply.text);
        }

        let mut results = Vec::new();
        let mut summary: Option<String> = None;
        for call in &reply.calls {
            if let Some(raw) = call.input.get(tools::INVALID_ARGS).and_then(|v| v.as_str()) {
                let msg = format!(
                    "tool arguments were not valid JSON: {}",
                    raw.chars().take(200).collect::<String>()
                );
                report::tool_denied(&msg);
                note_loop_result(
                    &mut loop_guard,
                    &call.name,
                    &call.input,
                    &msg,
                    true,
                    &mut summary,
                );
                results.push(ToolResult {
                    id: call.id.clone(),
                    content: msg,
                    is_error: true,
                });
                continue;
            }
            let call_input = tool_input_for_execution(
                &call.name,
                &call.input,
                cwd,
                "build",
                depth,
                &task_for_recovery,
            );
            if matches!(call.name.as_str(), "bash" | "run_command") {
                if let Some(fallback) = auto_create_file_input(&task_for_recovery) {
                    report::notice(
                        "  recovery: using write_file for explicit file creation instead of shell",
                    );
                    if let Some(reason) = gate(perm, "write_file", &fallback, cwd) {
                        report::tool_denied(&reason);
                        results.push(ToolResult {
                            id: call.id.clone(),
                            content: reason,
                            is_error: true,
                        });
                        continue;
                    }
                    report::tool_call(
                        "write_file",
                        &tools::preview("write_file", &fallback),
                        &fallback,
                    );
                    trace_tool_call("write_file", &fallback, "build", depth);
                    let fallback_out = tools::run("write_file", &fallback, cwd);
                    hooks::post_tool_use(
                        "write_file",
                        &fallback,
                        &fallback_out.content,
                        fallback_out.is_error,
                        cwd,
                    );
                    report::tool_result("write_file", &fallback_out.content, fallback_out.is_error);
                    trace_tool_result(
                        "write_file",
                        &fallback_out.content,
                        fallback_out.is_error,
                        "build",
                        depth,
                    );
                    results.push(ToolResult {
                        id: call.id.clone(),
                        content: fallback_out.content.clone(),
                        is_error: fallback_out.is_error,
                    });
                    if !fallback_out.is_error {
                        summary = Some(format!(
                            "{}\n\nCompleted the explicit file creation task with write_file.",
                            fallback_out.content
                        ));
                    }
                    continue;
                }
            }
            report::tool_call(
                &call.name,
                &tools::preview(&call.name, &call_input),
                &call_input,
            );
            trace_tool_call(&call.name, &call_input, "build", depth);

            if call.name == "question" || call.name == "AskUserQuestion" {
                let (answer, is_error) = answer_question(&call_input);
                report::tool_result(&call.name, &answer, is_error);
                trace_tool_result(&call.name, &answer, is_error, "build", depth);
                note_loop_result(
                    &mut loop_guard,
                    &call.name,
                    &call_input,
                    &answer,
                    is_error,
                    &mut summary,
                );
                results.push(ToolResult {
                    id: call.id.clone(),
                    content: answer,
                    is_error,
                });
                continue;
            }

            let reason = match hooks::pre_tool_use(&call.name, &call_input, cwd) {
                PreDecision::Deny(r) => Some(r),
                PreDecision::Allow => None,
                PreDecision::Continue => gate(perm, &call.name, &call_input, cwd),
            };
            if let Some(reason) = reason {
                report::tool_denied(&reason);
                trace::record_visible(
                    "tool_denied",
                    format!("{} denied", call.name),
                    serde_json::json!({"tool": call.name, "reason": reason, "input": &call_input, "phase": "build", "depth": depth}),
                );
                note_loop_result(
                    &mut loop_guard,
                    &call.name,
                    &call_input,
                    &reason,
                    true,
                    &mut summary,
                );
                results.push(ToolResult {
                    id: call.id.clone(),
                    content: reason,
                    is_error: true,
                });
                continue;
            }

            // Record reads so we can enforce read-before-write below.
            if let Some(p) = tools::read_tracking_path(&call.name, &call_input, cwd) {
                read_paths.insert(p);
            }

            // Require the model to read a file before overwriting or patching it.
            if let Some(p) = tools::edit_tracking_path(&call.name, &call_input, cwd) {
                if p.exists() && !read_paths.contains(&p) {
                    let msg = format!(
                        "read('{}') required before editing. Read the file first, then retry.",
                        p.display()
                    );
                    report::tool_denied(&msg);
                    note_loop_result(
                        &mut loop_guard,
                        &call.name,
                        &call_input,
                        &msg,
                        true,
                        &mut summary,
                    );
                    results.push(ToolResult {
                        id: call.id.clone(),
                        content: msg,
                        is_error: true,
                    });
                    continue;
                }
            }

            if matches!(call.name.as_str(), "task" | "spawn_subagent") {
                let out = spawn_subagent(p, perm, &call_input, cwd, depth);
                report::tool_result(&call.name, &out, false);
                trace_tool_result(&call.name, &out, false, "build", depth);
                results.push(ToolResult {
                    id: call.id.clone(),
                    content: out,
                    is_error: false,
                });
                continue;
            }
            // Memory-save tool: the model can call save_memory to persist facts.
            if call.name == "save_memory" {
                if let Some(note) = call_input["note"].as_str() {
                    config::append_memory(note);
                    let msg = "memory saved".to_string();
                    report::tool_result(&call.name, &msg, false);
                    trace_tool_result(&call.name, &msg, false, "build", depth);
                    results.push(ToolResult {
                        id: call.id.clone(),
                        content: msg,
                        is_error: false,
                    });
                    continue;
                }
            }

            let out = tools::run(&call.name, &call_input, cwd);
            hooks::post_tool_use(&call.name, &call_input, &out.content, out.is_error, cwd);
            if out.is_error {
                hooks::notify("OnError", cwd);
            }
            report::tool_result(&call.name, &out.content, out.is_error);
            trace_tool_result(&call.name, &out.content, out.is_error, "build", depth);
            if matches!(call.name.as_str(), "bash" | "run_command")
                && out.is_error
                && out.content.contains("No such file or directory")
            {
                if let Some(fallback) = auto_create_file_input(&task_for_recovery) {
                    report::tool_call(
                        "write_file",
                        &tools::preview("write_file", &fallback),
                        &fallback,
                    );
                    trace_tool_call("write_file", &fallback, "build", depth);
                    let fallback_out = tools::run("write_file", &fallback, cwd);
                    report::tool_result("write_file", &fallback_out.content, fallback_out.is_error);
                    trace_tool_result(
                        "write_file",
                        &fallback_out.content,
                        fallback_out.is_error,
                        "build",
                        depth,
                    );
                    if !fallback_out.is_error {
                        summary = Some(format!(
                            "{}\n\nRecovered from a failed shell redirect by writing the requested file directly.",
                            fallback_out.content
                        ));
                    }
                }
            }
            if matches!(call.name.as_str(), "Artifact" | "publish_artifact")
                && out.is_error
                && canvas_game_requested(&task_for_recovery)
            {
                artifact_error_count += 1;
                if artifact_error_count >= 1 {
                    let fallback = fallback_canvas_game_input(&task_for_recovery);
                    report::tool_call(
                        "Artifact",
                        &tools::preview("Artifact", &fallback),
                        &fallback,
                    );
                    trace_tool_call("Artifact", &fallback, "build", depth);
                    let fallback_out = tools::run("Artifact", &fallback, cwd);
                    report::tool_result("Artifact", &fallback_out.content, fallback_out.is_error);
                    trace_tool_result(
                        "Artifact",
                        &fallback_out.content,
                        fallback_out.is_error,
                        "build",
                        depth,
                    );
                    if !fallback_out.is_error {
                        summary = Some(format!(
                            "{}\n\nThe model repeatedly produced incomplete canvas HTML, so buildwithnexus published a complete self-contained fallback canvas game.",
                            fallback_out.content
                        ));
                    }
                }
            }
            if out.finished {
                report::finish(&out.content);
                summary = Some(out.content.clone());
            } else {
                note_loop_result(
                    &mut loop_guard,
                    &call.name,
                    &call_input,
                    &out.content,
                    out.is_error,
                    &mut summary,
                );
            }
            results.push(ToolResult {
                id: call.id.clone(),
                content: out.content,
                is_error: out.is_error,
            });
        }

        msgs.push(Msg::Assistant {
            text: assistant_tool_turn_text(reply.text, &reply.calls),
            calls: reply.calls,
        });
        msgs.push(Msg::Tool(results));
        if !report::is_json() {
            tui::context_meter(estimate_tokens(msgs), p.context_tokens);
            tui::poll_typeahead();
        }
        if let Some(s) = summary {
            if depth == 0 && !report::is_json() {
                let verifier = crate::verifier::Verifier::new(&cwd.to_string_lossy());
                let ctx = crate::verifier::VerificationContext {
                    task_description: task.to_string(),
                    task_type: None,
                    changed_files: vec![],
                    tool_calls: vec![],
                    evidence_gathered: vec![],
                    tests_added: vec![],
                    dependencies_changed: vec![],
                    git_diff: None,
                };
                let rep = verifier.verify(&ctx);
                if matches!(
                    rep.status,
                    crate::verifier::VerificationStatus::PassedWithWarnings
                        | crate::verifier::VerificationStatus::Blocked
                        | crate::verifier::VerificationStatus::Failed
                ) {
                    report::notice(&format!("  [Verification: {}]", rep.status));
                    if !rep.rule_violations.is_empty() {
                        report::notice(&crate::rules::RuleEngine::format_violations(
                            &rep.rule_violations,
                        ));
                    }
                }
            }
            return Ok(s);
        }
    }
    Err(format!(
        "reached the {MAX_ITERS}-step limit without finishing"
    ))
}

static SUB_SEQ: AtomicUsize = AtomicUsize::new(0);

fn spawn_subagent(
    p: &Provider,
    perm: Permission,
    input: &serde_json::Value,
    cwd: &Path,
    depth: usize,
) -> String {
    if depth + 1 >= MAX_DEPTH {
        return "subagent depth limit reached".into();
    }
    let task = input["task"]
        .as_str()
        .or_else(|| input["description"].as_str())
        .unwrap_or("")
        .trim();
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
            None => (
                cwd.to_path_buf(),
                "[worktree unavailable — ran in place]\n".into(),
            ),
        }
    } else {
        (cwd.to_path_buf(), String::new())
    };

    report::notice(&format!("  ↳ subagent: {task}"));
    trace::record_visible(
        "subagent_spawn",
        format!("{role}: {}", trace::preview(task, 80)),
        serde_json::json!({
            "task": task,
            "role": role,
            "isolate": isolate,
            "cwd": run_cwd.to_string_lossy(),
            "parent_depth": depth,
        }),
    );
    let mut child: Vec<Msg> = Vec::new();
    let result = build_inner(p, perm, role, task, &run_cwd, depth + 1, &mut child)
        .unwrap_or_else(|e| format!("subagent error: {e}"));
    trace::record_visible(
        "subagent_done",
        format!("{role}: {}", trace::preview(task, 80)),
        serde_json::json!({
            "task": task,
            "role": role,
            "isolate": isolate,
            "cwd": run_cwd.to_string_lossy(),
            "result": result,
            "depth": depth + 1,
        }),
    );
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

fn parse_plan_steps(plan_text: &str) -> Vec<String> {
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

fn plan_steps_are_actionable(steps: &[String]) -> bool {
    !steps.is_empty() && steps.iter().all(|step| plan_step_is_actionable(step))
}

fn is_exit_plan_tool(name: &str) -> bool {
    matches!(name, "exit_plan" | "ExitPlanMode")
}

fn fallback_exit_plan_input(task: &str) -> serde_json::Value {
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
            format!("Implement the user's requested task: {task}."),
            "Inspect only the files or directories needed for that task.".to_string(),
            "Make the required edits or generated files with the appropriate tools.".to_string(),
            "Verify the result with focused checks or tests.".to_string(),
            "Summarize the completed work and any remaining risks.".to_string(),
        ]
    };
    serde_json::json!({ "steps": steps })
}

fn plan_step_is_actionable(step: &str) -> bool {
    let lower_step = step.to_ascii_lowercase();
    if lower_step.contains("exit plan")
        || lower_step.contains("exit_plan")
        || lower_step.contains("exitplanmode")
        || lower_step.contains("plan mode")
    {
        return false;
    }
    let first = step
        .trim()
        .trim_start_matches(['`', '\'', '"', '['])
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_ascii_alphabetic())
        .to_ascii_lowercase();
    matches!(
        first.as_str(),
        "add"
            | "analyze"
            | "audit"
            | "build"
            | "check"
            | "compare"
            | "configure"
            | "create"
            | "document"
            | "edit"
            | "find"
            | "fix"
            | "generate"
            | "identify"
            | "implement"
            | "inspect"
            | "install"
            | "list"
            | "load"
            | "open"
            | "publish"
            | "read"
            | "refactor"
            | "remove"
            | "review"
            | "run"
            | "search"
            | "start"
            | "stop"
            | "summarize"
            | "test"
            | "update"
            | "use"
            | "verify"
            | "write"
    )
}

fn strip_plan_marker(line: &str) -> Option<&str> {
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

fn strip_markdown_emphasis(line: &str) -> String {
    let mut cleaned = line.trim().to_string();
    if let Some(rest) = cleaned.strip_prefix("**") {
        cleaned = rest.to_string();
        if let Some(idx) = cleaned.rfind("**") {
            cleaned.replace_range(idx..idx + 2, "");
        }
    }
    strip_step_label(cleaned.trim()).to_string()
}

fn strip_step_label(line: &str) -> &str {
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

fn auto_static_artifact_input(task: &str, text: &str) -> Option<serde_json::Value> {
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

fn auto_create_file_input(task: &str) -> Option<serde_json::Value> {
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

fn recovery_task_text(task: &str) -> String {
    task.split("\n\nFollow this approved plan:")
        .next()
        .unwrap_or(task)
        .split("\n\n[hook context]")
        .next()
        .unwrap_or(task)
        .trim()
        .to_string()
}

fn static_artifact_requested(task: &str) -> bool {
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

fn static_artifact_recovery_prompt(task: &str) -> String {
    format!(
        "This is a BUILD task, not a clarification request. Proceed with reasonable defaults for: {task}\n\n\
         If this is a canvas/browser game or static web artifact, build the runnable artifact now. \
         Call Artifact/publish_artifact with one complete self-contained HTML file. \
         Include all CSS and JavaScript inline. Do not ask the user for assets or a concept unless the task is impossible without them."
    )
}

fn extract_html_from_text(text: &str) -> Option<String> {
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

fn extract_fenced_html(text: &str) -> Option<String> {
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

fn extract_html_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let start = lower.find("<title")?;
    let after_tag = &html[start..];
    let gt = after_tag.find('>')?;
    let rest = &after_tag[gt + 1..];
    let end = rest.to_lowercase().find("</title")?;
    let title = rest[..end].trim();
    (!title.is_empty()).then(|| title.to_string())
}

fn canvas_game_requested(task: &str) -> bool {
    let lower = task.to_lowercase();
    lower.contains("canvas game") || lower.contains("browser game") || lower.contains("game")
}

fn fallback_canvas_game_input(task: &str) -> serde_json::Value {
    let title = if task.to_lowercase().contains("orbit") {
        "Orbit Dodge"
    } else {
        "Canvas Dodge"
    };
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title>
<style>
html,body{{margin:0;height:100%;overflow:hidden;background:#111827;color:#e5e7eb;font-family:system-ui,-apple-system,Segoe UI,sans-serif}}
#wrap{{position:fixed;inset:0;display:grid;place-items:center;background:radial-gradient(circle at 50% 35%,#1f2937,#030712 70%)}}
canvas{{width:min(94vw,860px);height:min(72vh,560px);background:#050816;border:1px solid #334155;box-shadow:0 24px 80px #0008}}
#hud{{position:fixed;left:18px;right:18px;top:14px;display:flex;justify-content:space-between;gap:16px;font-weight:700;text-shadow:0 2px 8px #000}}
#help{{position:fixed;left:18px;right:18px;bottom:14px;text-align:center;color:#9ca3af;font-size:14px}}
#pad{{position:fixed;right:18px;bottom:56px;display:grid;grid-template-columns:48px 48px 48px;gap:8px}}
button{{background:#1f2937cc;color:#e5e7eb;border:1px solid #475569;border-radius:8px;min-height:44px;font:inherit}}
@media (pointer:fine){{#pad{{display:none}}}}
</style>
</head>
<body>
<div id="wrap"><canvas id="game" width="860" height="560"></canvas></div>
<div id="hud"><span id="score">Score 0</span><span id="state">Move to start</span></div>
<div id="help">WASD/Arrows move · Space/P pause · R restart · Dodge the orbiting sparks</div>
<div id="pad"><span></span><button data-k="ArrowUp">↑</button><span></span><button data-k="ArrowLeft">←</button><button data-k=" ">Ⅱ</button><button data-k="ArrowRight">→</button><span></span><button data-k="ArrowDown">↓</button><span></span></div>
<script>
const c=document.getElementById('game'),x=c.getContext('2d'),scoreEl=document.getElementById('score'),stateEl=document.getElementById('state');
const keys=new Set();let player,orbs,score,best=0,over=false,paused=false,last=0;
function reset(){{player={{x:c.width/2,y:c.height/2,r:11,vx:0,vy:0}};orbs=[];score=0;over=false;paused=false;last=0;stateEl.textContent='Survive';for(let i=0;i<7;i++)orbs.push({{a:i*.9,d:70+i*34,s:.7+i*.08,r:8+i%3}});}}
function key(k,on){{if(on)keys.add(k);else keys.delete(k);if(k==='r'||k==='R')reset();if(k===' '||k==='p'||k==='P')paused=!paused;}}
addEventListener('keydown',e=>{{key(e.key,true);if(['ArrowUp','ArrowDown','ArrowLeft','ArrowRight',' '].includes(e.key))e.preventDefault();}});
addEventListener('keyup',e=>key(e.key,false));
document.querySelectorAll('button').forEach(b=>{{b.onpointerdown=e=>{{b.setPointerCapture(e.pointerId);key(b.dataset.k,true)}};b.onpointerup=e=>key(b.dataset.k,false);}});
function step(t){{requestAnimationFrame(step);let dt=Math.min(.033,(t-last)/1000||0);last=t;if(paused){{draw();stateEl.textContent='Paused';return}}if(over){{draw();stateEl.textContent='Game over · R to restart';return}}
let ax=(keys.has('ArrowRight')||keys.has('d')||keys.has('D'))-(keys.has('ArrowLeft')||keys.has('a')||keys.has('A'));
let ay=(keys.has('ArrowDown')||keys.has('s')||keys.has('S'))-(keys.has('ArrowUp')||keys.has('w')||keys.has('W'));
player.vx=(player.vx+ax*900*dt)*.86;player.vy=(player.vy+ay*900*dt)*.86;player.x=Math.max(player.r,Math.min(c.width-player.r,player.x+player.vx*dt));player.y=Math.max(player.r,Math.min(c.height-player.r,player.y+player.vy*dt));
score+=dt*10;best=Math.max(best,score);scoreEl.textContent=`Score ${{score|0}} · Best ${{best|0}}`;
for(const o of orbs){{o.a+=o.s*dt;let ox=c.width/2+Math.cos(o.a)*o.d,oy=c.height/2+Math.sin(o.a*1.35)*o.d;if(Math.hypot(player.x-ox,player.y-oy)<player.r+o.r)over=true;}}
draw();}}
function draw(){{x.clearRect(0,0,c.width,c.height);x.fillStyle='#111827';x.fillRect(0,0,c.width,c.height);x.strokeStyle='#1f9cf0';x.globalAlpha=.18;for(let r=70;r<330;r+=34){{x.beginPath();x.ellipse(c.width/2,c.height/2,r,r*.72,0,0,7);x.stroke();}}x.globalAlpha=1;for(const o of orbs){{let ox=c.width/2+Math.cos(o.a)*o.d,oy=c.height/2+Math.sin(o.a*1.35)*o.d;x.fillStyle='#f97316';x.beginPath();x.arc(ox,oy,o.r,0,7);x.fill();}}x.fillStyle=over?'#ef4444':'#22c55e';x.beginPath();x.arc(player.x,player.y,player.r,0,7);x.fill();}}
reset();requestAnimationFrame(step);
</script>
</body>
</html>"#
    );
    serde_json::json!({
        "title": title,
        "contents": html,
        "type": "html",
    })
}

// ── PLAN mode ─────────────────────────────────────────────────────────────────
// The planning phase now has tools available so the model can inspect the
// codebase while breaking down the task. Execution still runs through BUILD.
pub fn run_plan(p: &Provider, perm: Permission, task: &str, cwd: &Path) -> Result<(), String> {
    let prefix = context_prefix(cwd, p.context_tokens);
    let sys = format!(
        "{prefix}You are a planning engineer with full access to the codebase. \
        Use read_file/list_dir/list_tree/find_paths/grep_files/fetch_url and read-only bash/run_command calls to inspect the project as needed. \
        Do not write files, edit files, apply patches, spawn subagents, or run mutating shell commands while planning. \
        When ready, call exit_plan or ExitPlanMode with a concise numbered implementation plan. \
        The plan must be concrete, actionable, and at most 8 steps. \
        Do not include code fences, shell snippets, intro text, or outro text."
    );

    let defs = tools::defs_readonly(); // planning inspects context but never writes
    let mut msgs = vec![Msg::System(sys), Msg::User(task.into())];
    let mut loop_guard = ToolLoopGuard::default();
    let mut tool_rounds = 0usize;
    let mut plan_format_recovery_count = 0usize;
    let mut plan_loop_recovery_count = 0usize;

    // Let the model use tools while planning (e.g. read files to understand structure).
    let plan_text = 'planning: loop {
        tool_rounds += 1;
        if tool_rounds > MAX_PLAN_TOOL_ROUNDS {
            return Err(format!(
                "planning stopped after {MAX_PLAN_TOOL_ROUNDS} tool rounds without producing a plan"
            ));
        }
        maybe_compact(p, &mut msgs);
        let start_instant = std::time::Instant::now();
        let reply = request_reply(p, &msgs, &defs, "planning")?;
        let elapsed = start_instant.elapsed().as_secs_f64();
        let gen_toks = (reply.text.len() / 4) + (reply.calls.len() * 40);
        if !report::is_json() {
            tui::inference_telemetry(gen_toks.max(10), elapsed);
        }

        if reply.calls.is_empty() {
            let candidate_steps = parse_plan_steps(&reply.text);
            if !plan_steps_are_actionable(&candidate_steps) && plan_format_recovery_count < 2 {
                plan_format_recovery_count += 1;
                msgs.push(Msg::Assistant {
                    text: reply.text.clone(),
                    calls: vec![],
                });
                msgs.push(Msg::User(format!(
                    "That was not a valid plan. Use the actual current workspace, not placeholder paths: {}\n\
                     If you need context, call read-only tools now. Otherwise output only a concise numbered list of concrete steps, max 8, with no prose.",
                    cwd.display()
                )));
                continue;
            }
            if !plan_steps_are_actionable(&candidate_steps) {
                let fallback = fallback_exit_plan_input(task);
                report::tool_call(
                    "exit_plan",
                    &tools::preview("exit_plan", &fallback),
                    &fallback,
                );
                trace_tool_call("exit_plan", &fallback, "plan", 0);
                let out = tools::run("exit_plan", &fallback, cwd);
                report::tool_result("exit_plan", &out.content, out.is_error);
                trace_tool_result("exit_plan", &out.content, out.is_error, "plan", 0);
                if !out.is_error && plan_steps_are_actionable(&parse_plan_steps(&out.content)) {
                    break out.content;
                }
                return Err(out.content);
            }
            break reply.text;
        }

        // Execute tool calls during the planning phase.
        let mut results = Vec::new();
        let mut loop_summary: Option<String> = None;
        for call in &reply.calls {
            if let Some(raw) = call.input.get(tools::INVALID_ARGS).and_then(|v| v.as_str()) {
                let msg = format!(
                    "tool arguments were not valid JSON: {}",
                    raw.chars().take(200).collect::<String>()
                );
                report::tool_denied(&msg);
                note_loop_result(
                    &mut loop_guard,
                    &call.name,
                    &call.input,
                    &msg,
                    true,
                    &mut loop_summary,
                );
                results.push(ToolResult {
                    id: call.id.clone(),
                    content: msg,
                    is_error: true,
                });
                continue;
            }
            let call_input =
                tool_input_for_execution(&call.name, &call.input, cwd, "plan", 0, task);
            report::tool_call(
                &call.name,
                &tools::preview(&call.name, &call_input),
                &call_input,
            );
            trace_tool_call(&call.name, &call_input, "plan", 0);
            if is_exit_plan_tool(&call.name) {
                let out = tools::run(&call.name, &call_input, cwd);
                let steps = parse_plan_steps(&out.content);
                let invalid_plan = out.is_error || !plan_steps_are_actionable(&steps);
                let content = if invalid_plan && !out.is_error {
                    format!(
                        "exit_plan rejected: provide an actionable numbered or bulleted plan for the user's task, not prose, a file list, or a description of exit_plan itself.\nCurrent user task: {task}\nCall exit_plan again with concrete task steps."
                    )
                } else {
                    out.content.clone()
                };
                report::tool_result(&call.name, &content, invalid_plan);
                trace_tool_result(&call.name, &content, invalid_plan, "plan", 0);
                if invalid_plan {
                    results.push(ToolResult {
                        id: call.id.clone(),
                        content,
                        is_error: true,
                    });
                    let fallback = fallback_exit_plan_input(task);
                    report::tool_call(
                        "exit_plan",
                        &tools::preview("exit_plan", &fallback),
                        &fallback,
                    );
                    trace_tool_call("exit_plan", &fallback, "plan", 0);
                    let fallback_out = tools::run("exit_plan", &fallback, cwd);
                    report::tool_result("exit_plan", &fallback_out.content, fallback_out.is_error);
                    trace_tool_result(
                        "exit_plan",
                        &fallback_out.content,
                        fallback_out.is_error,
                        "plan",
                        0,
                    );
                    if !fallback_out.is_error
                        && plan_steps_are_actionable(&parse_plan_steps(&fallback_out.content))
                    {
                        break 'planning fallback_out.content;
                    }
                    return Err(fallback_out.content);
                }
                break 'planning out.content;
            }
            if call.name == "question" || call.name == "AskUserQuestion" {
                let (answer, is_error) = answer_question(&call_input);
                report::tool_result(&call.name, &answer, is_error);
                trace_tool_result(&call.name, &answer, is_error, "plan", 0);
                note_loop_result(
                    &mut loop_guard,
                    &call.name,
                    &call_input,
                    &answer,
                    is_error,
                    &mut loop_summary,
                );
                results.push(ToolResult {
                    id: call.id.clone(),
                    content: answer,
                    is_error,
                });
                continue;
            }
            let reason = match hooks::pre_tool_use(&call.name, &call_input, cwd) {
                PreDecision::Deny(r) => Some(r),
                PreDecision::Allow => None,
                PreDecision::Continue => gate(Permission::ReadOnly, &call.name, &call_input, cwd),
            };
            if let Some(reason) = reason {
                trace::record_visible(
                    "tool_denied",
                    format!("{} denied", call.name),
                    serde_json::json!({"tool": call.name, "reason": reason, "input": &call_input, "phase": "plan", "depth": 0}),
                );
                note_loop_result(
                    &mut loop_guard,
                    &call.name,
                    &call_input,
                    &reason,
                    true,
                    &mut loop_summary,
                );
                results.push(ToolResult {
                    id: call.id.clone(),
                    content: reason,
                    is_error: true,
                });
                continue;
            }
            if call.name == "save_memory" {
                if let Some(note) = call_input["note"].as_str() {
                    config::append_memory(note);
                    let msg = "memory saved".to_string();
                    report::tool_result(&call.name, &msg, false);
                    trace_tool_result(&call.name, &msg, false, "plan", 0);
                    results.push(ToolResult {
                        id: call.id.clone(),
                        content: msg,
                        is_error: false,
                    });
                    continue;
                }
            }
            let out = tools::run(&call.name, &call_input, cwd);
            report::tool_result(&call.name, &out.content, out.is_error);
            trace_tool_result(&call.name, &out.content, out.is_error, "plan", 0);
            note_loop_result(
                &mut loop_guard,
                &call.name,
                &call_input,
                &out.content,
                out.is_error,
                &mut loop_summary,
            );
            results.push(ToolResult {
                id: call.id.clone(),
                content: out.content,
                is_error: out.is_error,
            });
        }
        msgs.push(Msg::Assistant {
            text: assistant_tool_turn_text(reply.text, &reply.calls),
            calls: reply.calls,
        });
        msgs.push(Msg::Tool(results));
        if let Some(loop_msg) = loop_summary {
            if plan_loop_recovery_count < 2 {
                plan_loop_recovery_count += 1;
                msgs.push(Msg::User(format!(
                    "You are repeating the same inspection instead of exiting plan mode.\n\
                     Use the gathered result below and call exit_plan/ExitPlanMode now with a concise actionable plan. \
                     Do not call the same tool again.\n\n{loop_msg}"
                )));
                continue;
            }
            return Err(loop_msg);
        }
    };

    let mut steps = parse_plan_steps(&plan_text);
    if !plan_steps_are_actionable(&steps) {
        return Err(
            "planning did not produce an actionable numbered or bulleted plan after recovery attempts"
                .into(),
        );
    }

    loop {
        tui::line("");
        tui::line(&tui::accent("  Plan"));
        for (i, s) in steps.iter().enumerate() {
            tui::line(&format!("  {}. {}", i + 1, s));
        }
        tui::line("");
        let ans = tui::ask(&format!(
            "  {} execute, {} edit <n>, {} cancel: ",
            tui::bold("[Enter]"),
            tui::bold("e"),
            tui::bold("c")
        ))
        .unwrap_or_default();
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

    let plan = steps
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}. {}", i + 1, s))
        .collect::<Vec<_>>()
        .join("\n");
    let full = format!("{task}\n\nFollow this approved plan:\n{plan}");
    run_build(p, perm, "engineer", &full, cwd)
}

// ── BRAINSTORM mode ───────────────────────────────────────────────────────────
// Brainstorm is conversational but has full tool access. The model can grep,
// read files, fetch URLs, and run commands when the conversation calls for it.
// It also has a mode-transition sensor: if it detects the user wants to build
// or plan, it suggests switching.
pub fn run_brainstorm(
    p: &Provider,
    perm: Permission,
    cwd: &Path,
    first: &str,
) -> Result<Option<ModeHint>, String> {
    let prefix = context_prefix(cwd, p.context_tokens);
    let sys = format!("{prefix}You are a sharp, concise thought partner with full access to the codebase and the internet. \
        Use tools freely to look things up, read files, grep for patterns, or run commands — \
        whatever helps the conversation. \
        When you think the user is ready to stop discussing and start building or planning, \
        end your response with the exact token [SUGGEST:BUILD] or [SUGGEST:PLAN] on its own line. \
        Otherwise just respond naturally. No fluff.");

    let defs = tools::defs_for_context(false, p.context_tokens);
    let mut msgs: Vec<Msg> = vec![Msg::System(sys)];
    let mut question = first.to_string();
    let mut loop_guard = ToolLoopGuard::default();

    loop {
        msgs.push(Msg::User(question.clone()));
        maybe_compact(p, &mut msgs);

        // Keep consuming tool calls until the model gives a text response.
        let mut tool_rounds = 0usize;
        let reply_text = loop {
            tool_rounds += 1;
            if tool_rounds > MAX_CHAT_TOOL_ROUNDS {
                break format!(
                    "I stopped after {MAX_CHAT_TOOL_ROUNDS} tool rounds without a final response."
                );
            }
            tui::line("");
            let start_instant = std::time::Instant::now();
            let reply = request_reply(p, &msgs, &defs, "thinking")?;
            let elapsed = start_instant.elapsed().as_secs_f64();
            let gen_toks = (reply.text.len() / 4) + (reply.calls.len() * 40);
            if !report::is_json() {
                tui::inference_telemetry(gen_toks.max(10), elapsed);
            }

            if reply.calls.is_empty() {
                msgs.push(Msg::Assistant {
                    text: reply.text.clone(),
                    calls: vec![],
                });
                if !report::is_json() {
                    tui::context_meter(estimate_tokens(&msgs), p.context_tokens);
                }
                break reply.text;
            }

            // Execute tool calls inline.
            let mut results = Vec::new();
            let mut loop_summary: Option<String> = None;
            for call in &reply.calls {
                if let Some(raw) = call.input.get(tools::INVALID_ARGS).and_then(|v| v.as_str()) {
                    let msg = format!(
                        "tool arguments were not valid JSON: {}",
                        raw.chars().take(200).collect::<String>()
                    );
                    report::tool_denied(&msg);
                    note_loop_result(
                        &mut loop_guard,
                        &call.name,
                        &call.input,
                        &msg,
                        true,
                        &mut loop_summary,
                    );
                    results.push(ToolResult {
                        id: call.id.clone(),
                        content: msg,
                        is_error: true,
                    });
                    continue;
                }
                let call_input = tool_input_for_execution(
                    &call.name,
                    &call.input,
                    cwd,
                    "brainstorm",
                    0,
                    &question,
                );
                report::tool_call(
                    &call.name,
                    &tools::preview(&call.name, &call_input),
                    &call_input,
                );
                trace_tool_call(&call.name, &call_input, "brainstorm", 0);
                if call.name == "question" || call.name == "AskUserQuestion" {
                    let (answer, is_error) = answer_question(&call_input);
                    report::tool_result(&call.name, &answer, is_error);
                    trace_tool_result(&call.name, &answer, is_error, "brainstorm", 0);
                    note_loop_result(
                        &mut loop_guard,
                        &call.name,
                        &call_input,
                        &answer,
                        is_error,
                        &mut loop_summary,
                    );
                    results.push(ToolResult {
                        id: call.id.clone(),
                        content: answer,
                        is_error,
                    });
                    continue;
                }
                let reason = match hooks::pre_tool_use(&call.name, &call_input, cwd) {
                    PreDecision::Deny(r) => Some(r),
                    PreDecision::Allow => None,
                    PreDecision::Continue => gate(perm, &call.name, &call_input, cwd),
                };
                if let Some(reason) = reason {
                    trace::record_visible(
                        "tool_denied",
                        format!("{} denied", call.name),
                        serde_json::json!({"tool": call.name, "reason": reason, "input": &call_input, "phase": "brainstorm", "depth": 0}),
                    );
                    note_loop_result(
                        &mut loop_guard,
                        &call.name,
                        &call_input,
                        &reason,
                        true,
                        &mut loop_summary,
                    );
                    results.push(ToolResult {
                        id: call.id.clone(),
                        content: reason,
                        is_error: true,
                    });
                    continue;
                }
                if call.name == "save_memory" {
                    if let Some(note) = call_input["note"].as_str() {
                        config::append_memory(note);
                        let msg = "memory saved".to_string();
                        report::tool_result(&call.name, &msg, false);
                        trace_tool_result(&call.name, &msg, false, "brainstorm", 0);
                        results.push(ToolResult {
                            id: call.id.clone(),
                            content: msg,
                            is_error: false,
                        });
                        continue;
                    }
                }
                let out = tools::run(&call.name, &call_input, cwd);
                hooks::post_tool_use(&call.name, &call_input, &out.content, out.is_error, cwd);
                report::tool_result(&call.name, &out.content, out.is_error);
                trace_tool_result(&call.name, &out.content, out.is_error, "brainstorm", 0);
                note_loop_result(
                    &mut loop_guard,
                    &call.name,
                    &call_input,
                    &out.content,
                    out.is_error,
                    &mut loop_summary,
                );
                results.push(ToolResult {
                    id: call.id.clone(),
                    content: out.content,
                    is_error: out.is_error,
                });
            }
            msgs.push(Msg::Assistant {
                text: assistant_tool_turn_text(reply.text, &reply.calls),
                calls: reply.calls,
            });
            msgs.push(Msg::Tool(results));
            if !report::is_json() {
                tui::context_meter(estimate_tokens(&msgs), p.context_tokens);
                tui::poll_typeahead();
            }
            if let Some(loop_msg) = loop_summary {
                break loop_msg;
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
                ModeHint::Build => "switch to BUILD mode and implement this?",
                ModeHint::Plan => "switch to PLAN mode and break this down?",
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

fn trace_tool_call(name: &str, input: &serde_json::Value, phase: &str, depth: usize) {
    trace::record_visible(
        "tool_call",
        tools::preview(name, input),
        serde_json::json!({"tool": name, "input": input, "phase": phase, "depth": depth}),
    );
}

fn trace_tool_result(name: &str, content: &str, is_error: bool, phase: &str, depth: usize) {
    trace::record_visible(
        "tool_result",
        format!("{name} {}", if is_error { "error" } else { "ok" }),
        serde_json::json!({"tool": name, "is_error": is_error, "content": content, "phase": phase, "depth": depth}),
    );
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
        assert!(matches!(permission("acceptEdits"), Permission::Auto));
        assert!(matches!(permission("bypass-permissions"), Permission::Auto));
        assert!(matches!(permission("readonly"), Permission::ReadOnly));
        assert!(matches!(permission("read-only"), Permission::ReadOnly));
        assert!(matches!(permission("plan"), Permission::ReadOnly));
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
    fn parse_plan_steps_keeps_only_list_items() {
        let steps = parse_plan_steps(
            "Intro prose.\n\
             1. **Inspect the content of `hello.txt`**.\n\
             ```bash\n\
             write_file hello.txt nope\n\
             ```\n\
             2. Step 2: Summarize the file.\n\
             Outro.",
        );
        assert_eq!(
            steps,
            vec![
                "Inspect the content of `hello.txt`.".to_string(),
                "Summarize the file.".to_string(),
            ]
        );
    }

    #[test]
    fn parse_plan_steps_rejects_prose_only_plan() {
        let steps = parse_plan_steps(
            "The project directory does not exist. I will attempt to list files if you provide a path.",
        );
        assert!(steps.is_empty());
    }

    #[test]
    fn plan_steps_must_be_actions_not_file_lists() {
        let files = vec![
            "`.env.example`".to_string(),
            "`README.md`".to_string(),
            "`harness/`".to_string(),
        ];
        assert!(!plan_steps_are_actionable(&files));
        let actions = vec![
            "Inspect the current workspace root.".to_string(),
            "List the top-level files.".to_string(),
            "Summarize the result.".to_string(),
        ];
        assert!(plan_steps_are_actionable(&actions));
        let meta = vec!["Write a clean exit plan with concrete steps.".to_string()];
        assert!(!plan_steps_are_actionable(&meta));
    }

    #[test]
    fn exit_plan_tool_aliases_are_recognized() {
        assert!(is_exit_plan_tool("exit_plan"));
        assert!(is_exit_plan_tool("ExitPlanMode"));
        assert!(!is_exit_plan_tool("finish"));
    }

    #[test]
    fn fallback_exit_plan_uses_readonly_steps_for_listing_tasks() {
        let input = fallback_exit_plan_input("inspect this project and list files; do not modify");
        let steps = input["steps"].as_array().unwrap();
        assert!(steps[0].as_str().unwrap().contains("read-only"));
        let plan = tools::run("exit_plan", &input, Path::new("/tmp"));
        assert!(!plan.is_error, "{}", plan.content);
        assert!(plan_steps_are_actionable(&parse_plan_steps(&plan.content)));
    }

    #[test]
    fn fallback_exit_plan_uses_task_specific_steps_for_file_creation() {
        let input = fallback_exit_plan_input(
            "create scratch/exitplan-smoke.txt containing exit plan smoke",
        );
        let plan = tools::run("exit_plan", &input, Path::new("/tmp"));
        assert!(!plan.is_error, "{}", plan.content);
        assert!(plan.content.contains("Create scratch/exitplan-smoke.txt"));
        assert!(!plan.content.contains("Implement the requested change"));
    }

    #[test]
    fn auto_create_file_input_parses_simple_create_request() {
        let input =
            auto_create_file_input("create scratch/exitplan-smoke.txt containing exit plan smoke")
                .unwrap();
        assert_eq!(input["path"], "scratch/exitplan-smoke.txt");
        assert_eq!(input["content"], "exit plan smoke");
        let with_article = auto_create_file_input(
            "Create a scratch/exitplan-smoke.txt containing exit plan smoke",
        )
        .unwrap();
        assert_eq!(with_article["path"], "scratch/exitplan-smoke.txt");
        let with_plan_context = auto_create_file_input(
            "create scratch/exitplan-smoke.txt containing exit plan smoke\n\nFollow this approved plan:\n1. Write the file.",
        )
        .unwrap();
        assert_eq!(with_plan_context["content"], "exit plan smoke");
        assert!(auto_create_file_input("create a thing").is_none());
    }

    #[test]
    fn write_file_placeholder_path_is_repaired_from_create_task() {
        let repaired = repair_placeholder_tool_input(
            "write_file",
            &json!({"path": "~", "content": "exit plan smoke\n- Create scratch/exitplan-smoke.txt"}),
            Path::new("/tmp/example-project"),
            "create scratch/exitplan-smoke.txt containing exit plan smoke",
        )
        .unwrap();
        assert_eq!(repaired["path"], "scratch/exitplan-smoke.txt");
        assert_eq!(repaired["content"], "exit plan smoke");
    }

    #[test]
    fn write_file_plan_contaminated_content_is_repaired_from_create_task() {
        let repaired = repair_placeholder_tool_input(
            "write",
            &json!({"path": "scratch/exitplan-smoke.txt", "content": "exit plan smoke\n\n1. Create scratch/exitplan-smoke.txt"}),
            Path::new("/tmp/example-project"),
            "create scratch/exitplan-smoke.txt containing exit plan smoke",
        )
        .unwrap();
        assert_eq!(repaired["path"], "scratch/exitplan-smoke.txt");
        assert_eq!(repaired["content"], "exit plan smoke");
    }

    #[test]
    fn placeholder_project_path_is_repaired_for_discovery_tools() {
        let cwd = Path::new("/tmp/example-project");
        let repaired = repair_placeholder_tool_input(
            "list",
            &json!({"path": "/path/to/your/project"}),
            cwd,
            "inspect this project",
        )
        .unwrap();
        assert_eq!(repaired["path"], "/tmp/example-project");
        assert!(repair_placeholder_tool_input(
            "read_file",
            &json!({"path": "your-project-file.txt"}),
            cwd,
            "inspect this project"
        )
        .is_none());
    }

    #[test]
    fn filesystem_root_is_repaired_for_current_workspace_discovery() {
        let cwd = Path::new("/tmp/example-project");
        let repaired = repair_placeholder_tool_input(
            "list",
            &json!({"path": "/"}),
            cwd,
            "inspect this project and list the top-level files",
        )
        .unwrap();
        assert_eq!(repaired["path"], "/tmp/example-project");
    }

    #[test]
    fn filesystem_root_is_not_repaired_when_user_asks_for_root() {
        let cwd = Path::new("/tmp/example-project");
        assert!(repair_placeholder_tool_input(
            "list",
            &json!({"path": "/"}),
            cwd,
            "inspect the filesystem root / and list its top-level files",
        )
        .is_none());
    }

    #[test]
    fn auto_static_artifact_extracts_html_response() {
        let input = auto_static_artifact_input(
            "build me a canvas game",
            "HTML:\n```html\n<!doctype html><html><head><title>Orbit</title></head><body><canvas></canvas><script>requestAnimationFrame(()=>{})</script></body></html>\n```",
        )
        .unwrap();
        assert_eq!(input["title"], "Orbit");
        assert!(input["contents"].as_str().unwrap().contains("<canvas>"));
    }

    #[test]
    fn auto_static_artifact_ignores_non_static_tasks() {
        assert!(auto_static_artifact_input(
            "explain this code",
            "<!doctype html><html><body></body></html>"
        )
        .is_none());
    }

    #[test]
    fn static_artifact_recovery_prompt_tells_model_to_build() {
        let prompt = static_artifact_recovery_prompt("build me a canvas game");
        assert!(prompt.contains("BUILD task"));
        assert!(prompt.contains("reasonable defaults"));
        assert!(prompt.contains("Artifact/publish_artifact"));
        assert!(prompt.contains("self-contained HTML"));
    }

    #[test]
    fn gate_auto_allows_ordinary_mutation() {
        let cwd = Path::new("/proj");
        let r = gate(
            Permission::Auto,
            "write_file",
            &json!({"path": "a.txt", "content": "x"}),
            cwd,
        );
        assert!(r.is_none());
    }

    #[test]
    fn gate_auto_still_confirms_sensitive_path() {
        let cwd = Path::new("/proj");
        let r = gate(
            Permission::Auto,
            "read_file",
            &json!({"path": "/proj/.env"}),
            cwd,
        );
        assert!(r.is_some());
    }

    #[test]
    fn gate_auto_confirms_catastrophic_command() {
        let cwd = Path::new("/proj");
        let r = gate(
            Permission::Auto,
            "run_command",
            &json!({"command": "rm -rf /"}),
            cwd,
        );
        assert!(r.is_some());
    }

    #[test]
    fn gate_auto_allows_safe_command() {
        let cwd = Path::new("/proj");
        let r = gate(
            Permission::Auto,
            "run_command",
            &json!({"command": "ls"}),
            cwd,
        );
        assert!(r.is_none());
    }

    #[test]
    fn gate_readonly_blocks_mutation() {
        let cwd = Path::new("/proj");
        let r = gate(
            Permission::ReadOnly,
            "write_file",
            &json!({"path": "a", "content": "x"}),
            cwd,
        );
        assert!(r.unwrap().contains("read-only"));
    }

    #[test]
    fn gate_readonly_allows_reads_everywhere() {
        // Reads outside CWD are now allowed — full filesystem access.
        let cwd = Path::new("/proj/work");
        let r = gate(
            Permission::ReadOnly,
            "read_file",
            &json!({"path": "/etc/passwd"}),
            cwd,
        );
        assert!(r.is_none());
    }

    #[test]
    fn gate_ask_allows_out_of_cwd_read() {
        // Full filesystem read access — no longer blocked in Ask mode.
        let cwd = Path::new("/proj");
        let r = gate(
            Permission::Ask,
            "read_file",
            &json!({"path": "/home/user/docs/README.md"}),
            cwd,
        );
        assert!(r.is_none());
    }

    #[test]
    fn gate_ask_prompts_for_mutation() {
        let cwd = Path::new("/proj");
        let r = gate(
            Permission::Ask,
            "write_file",
            &json!({"path": "a", "content": "x"}),
            cwd,
        );
        assert!(r.is_some()); // non-terminal → denied
    }

    #[test]
    fn gate_ask_allows_default_allowed_commands() {
        let cwd = Path::new("/proj");
        // git, cargo, npm are in the default allowed_commands list
        for cmd in &[
            "git status",
            "cargo test",
            "npm install",
            "git commit -m 'x'",
        ] {
            let r = gate(
                Permission::Ask,
                "run_command",
                &json!({"command": cmd}),
                cwd,
            );
            assert!(r.is_none(), "expected {cmd} to be auto-allowed in Ask mode");
        }
    }

    #[test]
    fn gate_ask_still_prompts_for_unknown_command() {
        let cwd = Path::new("/proj");
        let r = gate(
            Permission::Ask,
            "run_command",
            &json!({"command": "mycustombinary --flag"}),
            cwd,
        );
        assert!(r.is_some()); // not in allowed list → denied (non-terminal)
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

    #[test]
    fn test_session_allowlist() {
        assert!(!super::is_session_allowed_tool("custom_test_tool"));
        super::add_session_allowed_tool("custom_test_tool");
        assert!(super::is_session_allowed_tool("custom_test_tool"));
    }
}
