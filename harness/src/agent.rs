// The orchestration: plain Rust control flow for all three modes. PLAN
// decomposes + gates, BUILD is a ReAct tool loop, BRAINSTORM is conversational —
// but ALL modes have access to the full tool surface so the model can grep,
// fetch, read files, and run commands regardless of which mode the user is in.

use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::checkpoint;
use crate::config;
use crate::hooks::{self, PreDecision};
use crate::provider::{self, complete, Msg, Provider, Reply, ToolResult};
use crate::report;
use crate::tools;
use crate::trace;
use crate::tui;

// Step budget for a single BUILD turn. Generous, because complex agentic work
// legitimately needs many round-trips; and hitting it is never surfaced as a
// raw error — the turn lands gracefully with an honest summary (see the tail of
// run_agent). A wrap-up nudge fires one step before the budget runs out.
const MAX_ITERS: usize = 60;
const MAX_DEPTH: usize = 3;
const KEEP_RECENT: usize = 6;
const MAX_IDENTICAL_TOOL_RESULTS: usize = 3;
const MAX_PLAN_TOOL_ROUNDS: usize = 10;
const MAX_CHAT_TOOL_ROUNDS: usize = 12;
// How many times a turn truncated at the output-token limit is asked to continue.
const MAX_TOKEN_LIMIT_CONTINUATIONS: usize = 2;
// How many verifier Blocked/Failed verdicts the model is asked to address.
const MAX_VERIFIER_FIX_ROUNDS: usize = 2;
// How many act-don't-explain nudges an imperative BUILD task gets when the
// model answers with instructions instead of doing the work.
const MAX_ACT_NUDGES: usize = 2;

mod compact;
use compact::*;

// ── permissions ───────────────────────────────────────────────────────────────
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    Ask,
    /// File edits inside the project run without a prompt; commands,
    /// network and everything else ask as in `Ask`.
    AcceptEdits,
    Auto,
    ReadOnly,
}

// Wire name of the active gate — what hooks receive as `permission_mode`,
// and what the footer and settings show.
pub fn permission_name(perm: Permission) -> &'static str {
    match perm {
        Permission::Ask => "ask",
        Permission::AcceptEdits => "accept-edits",
        Permission::Auto => "auto",
        Permission::ReadOnly => "readonly",
    }
}

/// The permission named by `--permission-mode`, the `permission` setting or
/// /permissions. An unknown name is an error, never a silent `ask`.
pub fn parse_permission(s: &str) -> Result<Permission, String> {
    let normalized = s.trim().to_ascii_lowercase().replace(['_', '-', ' '], "");
    match normalized.as_str() {
        "ask" | "default" => Ok(Permission::Ask),
        "acceptedits" | "acceptedit" => Ok(Permission::AcceptEdits),
        "auto" | "bypasspermissions" => Ok(Permission::Auto),
        "readonly" | "read" | "plan" | "dontask" => Ok(Permission::ReadOnly),
        _ => Err(format!(
            "unknown permission mode {} — use ask, accept-edits, auto, readonly or plan",
            s.trim()
        )),
    }
}

// ── roles ─────────────────────────────────────────────────────────────────────
pub struct Role {
    pub system: &'static str,
}

pub fn role(id: &str) -> Role {
    let system = match id {
        "researcher" => "You are bwn in research mode: investigate before you conclude. Use the read and list tools to inspect the codebase, cite file paths, and don't modify files unless explicitly asked.",
        _ => "You are bwn in BUILD mode: a coding partner who ships the work, not a title. \
Use the tools to inspect and modify the project directly. \
Prefer small, verifiable edits. Read before you write. \
When writing or editing files, provide the complete, fully working code. NEVER use placeholders (e.g. `// ... rest of code`). \
BUILD mode means execute: for any concrete build/fix/create/change request, inspect the project and make the needed edits instead of replying with a capability statement. \
When asked to build or create something, call tools to create the files on disk (write_file, edit_file, run_command) instead of pasting instructions or code blocks in chat — you are the builder. \
For web applications, games, or interactive artifacts: NEVER create primitive placeholder dots or fake physics when actual mechanics/physics are requested. Use real, frame-rate independent physics (exponential dt-decay for friction), curated dark-mode color palettes, modern typography, glassmorphism, responsive canvas scaling, and complete touch/keyboard controls. \
For local web apps that require a dev server, use start_server, wait_for_url, inspect read_server_log if readiness fails, then open_browser when useful. \
If a path or file is missing, use discovery tools before asking the user. \
DO NOT ask the user for permission, themes, or choices unless absolutely necessary. If the user leaves something open-ended (e.g. 'pick a theme' or 'make it cool'), MAKE A REASONABLE DECISION and proceed immediately. \
Check your own work before you call it done: after a code change, run the project's tests/build/lint — \
use check_work (it auto-detects and runs them) or run_command — read what fails, and fix it before finishing. \
Don't claim something passes that you didn't run.\n\
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

// Web-artifact guidance is only relevant when the task actually asks to build
// one — injecting it into every prompt biased the model toward canvas games.
fn artifact_guidance(task: &str) -> Option<String> {
    (static_artifact_requested(task) || canvas_game_requested(task)).then(|| {
        "[Web artifact task]\n\
         Build an actual runnable artifact: one complete self-contained HTML file via \
         Artifact/publish_artifact with all CSS and JavaScript inline, unless the user \
         explicitly asks for a framework. Include controls, restart/error states, responsive \
         sizing, and touch/mobile support when useful; then open_browser if possible. \
         Write the code yourself from scratch — never search GitHub or clone external \
         repositories for the game/app. Never paste the HTML as plain markdown; call the tool."
            .to_string()
    })
}

// Shared identity + voice, injected ahead of every mode's contract so the
// personality is consistent whether the model is building, planning, or just
// chatting. bwn is a partner, not a job title — and a stiff canned refusal is a
// bug, not a personality. Voice rules mirror IDENTITY.md: dry and quick, wit in
// chat but never in error paths, lowercase bwn.
fn persona() -> String {
    "[Who you are]\n\
     You are buildwithnexus — bwn for short — an AI coding partner that lives in the terminal. \
     Not a form to fill out, not a job title, not an assistant waiting for orders: a partner. \
     Depending on the mode you build the work autonomously, brainstorm it, or work out a plan together — \
     three ways of pairing, not three different personalities.\n\
     Voice: plain-spoken, direct, a little dry. Quick and occasionally funny when it fits, never goofy, \
     never corporate, never padded with filler. Bias toward doing the work over describing it.\n\
     Handle people like a person would. If someone says hi, cracks a joke, or asks something off-task, \
     answer briefly and in character, then get back to it — don't force it into a task, a file, or a plan. \
     Canned refusals like \"I am a planning engineer and I cannot…\" or \"I am not able to tell jokes\" are a \
     bug: you can hold a normal conversation. Keep the wit in the room and out of error paths — when \
     something actually fails, be clear and useful, not cute, and punch at the problem, not the user."
        .to_string()
}

// Build the system prompt prefix from memory and skills/agents files.
// When context_tokens is small (≤16K), skip expensive sections to leave
// room for tool definitions + actual conversation.
fn context_prefix(cwd: &Path, context_tokens: usize) -> String {
    let compact = context_tokens <= 16_384;
    let mut parts: Vec<String> = Vec::new();

    // Identity leads every prompt — it is short and always worth the tokens.
    parts.push(persona());

    let home_str = crate::config::user_home()
        .map(|h| h.display().to_string())
        .unwrap_or_default();
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

    // Folders added with --add-dir, and what their instruction files say.
    if let Some(section) = crate::workdirs::prompt_section(cwd, compact) {
        parts.push(section);
    }

    // Project instructions (AGENTS.md / CLAUDE.md) come before memory and the
    // Agents.md roles: repository conventions frame everything that follows.
    let instructions = config::prompt_instructions(cwd);
    if let Some(text) = config::instructions_prompt(&instructions) {
        trace::record_visible(
            "instructions",
            format!("loaded {} instruction file(s)", instructions.len()),
            serde_json::json!({
                "files": instructions.iter().map(|f| f.label.clone()).collect::<Vec<_>>(),
                "bytes": text.len(),
            }),
        );
        let text = if compact && text.len() > 2_000 {
            format!("{}…", crate::config::cut_at_char_boundary(&text, 2_000))
        } else {
            text
        };
        parts.push(text);
    }

    if let Some(mem) = config::load_memory() {
        // Memory is user-important; always include but truncate for small ctx
        let mem_text = if compact && mem.len() > 300 {
            format!("{}…", crate::config::cut_at_char_boundary(&mem, 300))
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
            format!("{}…", crate::config::cut_at_char_boundary(&agents, 500))
        } else {
            agents
        };
        parts.push(format!("[Agent knowledge — Agents.md]\n{agents_text}"));
    }

    let (user_sys, project_sys) = config::load_system_prompts(cwd);
    if let Some(sys_prompt) = user_sys {
        parts.push(format!(
            "[Custom User System Prompt — system.md]\n{sys_prompt}"
        ));
    }
    if let Some(sys_prompt) = project_sys {
        parts.push(format!(
            "[Project System Prompt — .buildwithnexus/system.md]\n{sys_prompt}"
        ));
    }

    // Skip rules, knowledge, hooks, and skill descriptions for small contexts
    if !compact {
        // A separate verifier pass enforces the engineering rules — a 2-line
        // pointer is enough; injecting the full rule list bloated every prompt.
        parts.push(
            "[Operational rules]\nProject engineering rules are enforced by a separate \
             verifier pass after you finish.\nViolations are reported back to you to address."
                .to_string(),
        );

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
        let skill_descs = config::load_skill_descriptions(cwd);
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
    // Role guidance (no-placeholders, build-immediately, artifact rules) lives
    // in role()/artifact_guidance() — this section only lists what's built in.
    "[Built-in tools — always available, no install needed]\n\
Aliases are supported: bash/read/write/edit/patch/glob/grep/list/task/todowrite/todoread/webfetch/websearch/skill/question. \
Use built-ins before installing anything. Use list_tree/find_paths/grep_files before guessing paths; use find_paths kind=`dir` for folders. \
Use start_server/list_servers/wait_for_url/read_server_log/stop_server for long-running local dev servers, and open_browser for local URLs or generated HTML. \
For generated/edited code, HTML, or file contents, call write_file/edit_file/Artifact; never paste code as plain markdown."
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

// "Allow this session" answers, keyed by project so an approval given in one
// workspace never silences the gate in another opened later in the session.
static SESSION_ALLOWED_TOOLS: Mutex<Option<HashSet<(String, String)>>> = Mutex::new(None);

fn is_session_allowed_tool(cwd: &Path, key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    if let Ok(guard) = SESSION_ALLOWED_TOOLS.lock() {
        if let Some(set) = guard.as_ref() {
            return set.contains(&(config::project_key(cwd), key.to_string()));
        }
    }
    false
}

fn add_session_allowed_tool(cwd: &Path, key: &str) {
    if key.is_empty() {
        return;
    }
    if let Ok(mut guard) = SESSION_ALLOWED_TOOLS.lock() {
        let set = guard.get_or_insert_with(HashSet::new);
        set.insert((config::project_key(cwd), key.to_string()));
    }
}

/// This project's "allow this session" answers, sorted (`/permissions`).
pub fn session_allowed(cwd: &Path) -> Vec<String> {
    let project = config::project_key(cwd);
    let mut keys: Vec<String> = SESSION_ALLOWED_TOOLS
        .lock()
        .ok()
        .and_then(|g| {
            g.as_ref().map(|set| {
                set.iter()
                    .filter(|(p, _)| *p == project)
                    .map(|(_, k)| k.clone())
                    .collect()
            })
        })
        .unwrap_or_default();
    keys.sort();
    keys
}

/// Forget one "allow this session" answer. Returns whether it was there.
pub fn remove_session_allowed(cwd: &Path, key: &str) -> bool {
    let entry = (config::project_key(cwd), key.to_string());
    SESSION_ALLOWED_TOOLS
        .lock()
        .ok()
        .and_then(|mut g| g.as_mut().map(|set| set.remove(&entry)))
        .unwrap_or(false)
}

/// Forget this project's "allow this session" answers (`/permissions reset`).
pub fn clear_session_allowed(cwd: &Path) {
    let project = config::project_key(cwd);
    if let Ok(mut guard) = SESSION_ALLOWED_TOOLS.lock() {
        if let Some(set) = guard.as_mut() {
            set.retain(|(p, _)| *p != project);
        }
    }
}

/// How a headless run ended short of success, each with its own exit code.
/// Exit 0 is kept for real success; 1 (failed) and 2 (usage) predate this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failed,
    ApprovalBlocked,
    HookBlocked,
    BudgetStop,
    StepLimit,
    CheckWorkFailed,
    VerificationFailed,
}

impl Outcome {
    const ALL: [Outcome; 8] = [
        Outcome::Success,
        Outcome::Failed,
        Outcome::ApprovalBlocked,
        Outcome::HookBlocked,
        Outcome::BudgetStop,
        Outcome::StepLimit,
        Outcome::CheckWorkFailed,
        Outcome::VerificationFailed,
    ];

    pub fn exit_code(self) -> i32 {
        match self {
            Outcome::Success => 0,
            Outcome::Failed => 1,
            Outcome::ApprovalBlocked => 3,
            Outcome::HookBlocked => 4,
            Outcome::BudgetStop => 5,
            Outcome::StepLimit => 6,
            Outcome::CheckWorkFailed => 7,
            Outcome::VerificationFailed => 8,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Failed => "failed",
            Outcome::ApprovalBlocked => "approval_blocked",
            Outcome::HookBlocked => "hook_blocked",
            Outcome::BudgetStop => "budget_stop",
            Outcome::StepLimit => "step_limit",
            Outcome::CheckWorkFailed => "check_work_failed",
            Outcome::VerificationFailed => "verification_failed",
        }
    }

    /// Short human wording for the headless summary line.
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Success => "done",
            Outcome::Failed => "failed",
            Outcome::ApprovalBlocked => "changes blocked for lack of approval",
            Outcome::HookBlocked => "blocked by a hook",
            Outcome::BudgetStop => "stopped at the budget limit",
            Outcome::StepLimit => "ran out of steps",
            Outcome::CheckWorkFailed => "finished with failing checks",
            Outcome::VerificationFailed => "finished with verification failures",
        }
    }
}

// The first reason the top-level turn stopped short of success (0 = none
// yet). A turn that returns Ok with this set is not a success.
static STOPPED_SHORT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn stopped_short(o: Outcome) {
    let idx = Outcome::ALL.iter().position(|x| *x == o).unwrap_or(0);
    let _ = STOPPED_SHORT.compare_exchange(
        0,
        idx,
        std::sync::atomic::Ordering::Relaxed,
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// Why the run stopped short of success, if it did.
pub fn stopped_short_outcome() -> Option<Outcome> {
    match STOPPED_SHORT.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        i => Outcome::ALL.get(i).copied(),
    }
}

/// Confirmations refused because nobody was there to answer them, by what
/// each would have done. Headless runs fail instead of reporting "done"
/// when there are any.
static BLOCKED_WITHOUT_TERMINAL: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn blocked_without_terminal() -> Vec<String> {
    BLOCKED_WITHOUT_TERMINAL
        .lock()
        .map(|b| b.clone())
        .unwrap_or_default()
}

/// Of `blocked_without_terminal`, the ones --permission-mode auto would not
/// allow either: sensitive paths, dangerous commands, ask rules.
static BLOCKED_NEEDS_PERSON: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn blocked_needing_a_person() -> Vec<String> {
    BLOCKED_NEEDS_PERSON
        .lock()
        .map(|b| b.clone())
        .unwrap_or_default()
}

thread_local! {
    // Set around a confirmation that auto mode would have skipped, so a
    // headless run that blocks it can say auto would allow it.
    static AUTO_WOULD_ALLOW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// `confirm_call` for a call that --permission-mode auto lets through.
fn confirm_call_auto_allows(
    name: &str,
    input: &serde_json::Value,
    label: &str,
    tool_key: &str,
    cwd: &Path,
) -> Option<String> {
    AUTO_WOULD_ALLOW.with(|a| a.set(true));
    let r = confirm_call(name, input, label, tool_key, cwd);
    AUTO_WOULD_ALLOW.with(|a| a.set(false));
    r
}

thread_local! {
    // Set while the project's checks are gated: Some(true) once a refusal
    // for want of a terminal happened. Checks verify the work; they change
    // nothing, so that refusal is not one of the run's blocked changes.
    static GATING_CHECKS: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

// The gate for check_work. Returns the refusal, if any, and whether it was
// only that nobody could approve the checks.
fn gate_checks(perm: Permission, input: &serde_json::Value, cwd: &Path) -> Option<(String, bool)> {
    GATING_CHECKS.with(|g| g.set(Some(false)));
    let reason = hook_gate(perm, "check_work", input, cwd);
    let no_terminal = GATING_CHECKS.with(|g| g.replace(None)) == Some(true);
    reason.map(|r| (r, no_terminal))
}

const CHECKS_NOT_RUN: &str = "checks were not run (no terminal to approve them)";

thread_local! {
    // Set when the user cancels an approval prompt (Esc, Ctrl+C, end of
    // input): that call is refused, nothing else runs, and the build loop
    // ends the turn once the refused call is recorded. Cleared when the turn
    // ends. Per thread: a turn and every gate it calls run on one thread.
    static TURN_STOPPED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn set_turn_stopped(stopped: bool) {
    TURN_STOPPED.with(|t| t.set(stopped));
}

// The user cancelled a prompt mid-turn. The build loop ends the turn after
// recording the refusal; the interrupt also stops the plan and brainstorm
// loops before their next request (it is cleared when the next turn starts).
fn stop_turn() {
    set_turn_stopped(true);
    STOPPED_EARLY.with(|u| u.set(true));
    tui::trigger_interrupt(tui::InterruptKind::Escape);
}

const STOPPED_BY_USER: &str = "stopped by the user — ask what to do instead";

/// Whether the user stopped this turn at an approval prompt. The top-level
/// loop takes (clears) it; nested loops only look, so the stop reaches it.
pub(crate) fn turn_stopped(take: bool) -> bool {
    TURN_STOPPED.with(|t| if take { t.replace(false) } else { t.get() })
}

#[cfg(test)]
thread_local! {
    // What `needs_you` would have raised, in place of a real notification.
    static NOTIFIED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

// A desktop notification that the session is waiting on the user (an
// approval or a question), and the Notification hook of that `kind`.
// tui::notify only fires in the interactive UI and, with the default
// `notify: auto`, only while the terminal is unfocused; the hook always does.
fn needs_you(kind: &str, what: &str) {
    #[cfg(test)]
    {
        NOTIFIED.with(|n| n.borrow_mut().push(what.to_string()));
    }
    #[cfg(not(test))]
    tui::notify("buildwithnexus", what);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    hooks::notification(kind, what, &cwd);
}

// The gate is about to ask about this call: PermissionRequest hooks may
// answer first (allow, deny, or ask as usual), then the prompt.
fn confirm_call(
    name: &str,
    input: &serde_json::Value,
    label: &str,
    tool_key: &str,
    cwd: &Path,
) -> Option<String> {
    match hooks::permission_request(name, input, label, cwd) {
        PreDecision::Allow => None,
        PreDecision::Deny(r) => Some(r),
        PreDecision::Continue => confirm_tool(label, tool_key, cwd),
    }
}

/// A question for the person at the other end of `bwn acp`, asked in their
/// editor instead of the terminal.
pub enum RemoteAsk<'a> {
    /// The approval prompt: what the call does, and the key an "always"
    /// answer remembers (empty when there is nothing to remember).
    Tool { label: &'a str, key: &'a str },
    /// Whether to build an approved plan.
    Plan { steps: &'a [String] },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteAnswer {
    Once,
    /// The terminal's `a`: allowed from now on in this project.
    Always,
    Reject,
    /// The turn was cancelled while the question was open.
    Cancelled,
}

pub type RemoteApprover = Box<dyn Fn(RemoteAsk) -> RemoteAnswer + Send + Sync>;

static REMOTE_APPROVER: OnceLock<RemoteApprover> = OnceLock::new();

/// Sends every approval prompt to `ask` (an editor over ACP). Set once.
pub fn set_remote_approver(ask: RemoteApprover) {
    let _ = REMOTE_APPROVER.set(ask);
}

// A remote answer at the gate means what the terminal's y, a, n and Esc do.
fn remote_decision(answer: RemoteAnswer, tool_key: &str, cwd: &Path) -> Option<String> {
    match answer {
        RemoteAnswer::Once => None,
        RemoteAnswer::Always => {
            add_session_allowed_tool(cwd, tool_key);
            crate::config::add_project_allowed(cwd, tool_key);
            None
        }
        RemoteAnswer::Reject => Some("denied by user".into()),
        RemoteAnswer::Cancelled => {
            stop_turn();
            Some(STOPPED_BY_USER.into())
        }
    }
}

/// Forgets how the last turn ended, before the next one in the same process
/// (`bwn acp` serves many turns).
pub fn reset_turn_outcome() {
    STOPPED_SHORT.store(0, Ordering::Relaxed);
    if let Ok(mut b) = BLOCKED_WITHOUT_TERMINAL.lock() {
        b.clear();
    }
    if let Ok(mut b) = BLOCKED_NEEDS_PERSON.lock() {
        b.clear();
    }
}

fn confirm_tool(label: &str, tool_key: &str, cwd: &Path) -> Option<String> {
    if let Some(ask) = REMOTE_APPROVER.get() {
        let answer = ask(RemoteAsk::Tool {
            label,
            key: tool_key,
        });
        return remote_decision(answer, tool_key, cwd);
    }
    if report::is_json() || !std::io::stdin().is_terminal() {
        let checks = GATING_CHECKS.with(|g| g.get().is_some());
        if checks {
            GATING_CHECKS.with(|g| g.set(Some(true)));
        } else {
            if let Ok(mut b) = BLOCKED_WITHOUT_TERMINAL.lock() {
                b.push(label.to_string());
            }
            if !AUTO_WOULD_ALLOW.with(|a| a.get()) {
                if let Ok(mut b) = BLOCKED_NEEDS_PERSON.lock() {
                    b.push(label.to_string());
                }
            }
        }
        return Some(format!(
            "blocked (no interactive terminal to confirm: {label})"
        ));
    }
    helper_prompt(
        || confirm_with(label, tool_key, cwd, tui::ask),
        || {
            stop_turn();
            Some(STOPPED_BY_USER.into())
        },
    )
}

// The approval prompt itself; `ask` reads the answer (None: cancelled).
fn confirm_with(
    label: &str,
    tool_key: &str,
    cwd: &Path,
    ask: impl FnOnce(&str) -> Option<String>,
) -> Option<String> {
    needs_you(
        "permission_prompt",
        &format!("approval needed: {}", trace::preview(label, 120)),
    );
    // What a write or edit would change, right above the question.
    report::show_held_preview();
    // Action on its own line; the key legend stays short so the prompt never
    // wraps mid-legend on a normal-width terminal.
    // Right under the call's own header it would only repeat it.
    if !report::header_is_just_above(label) {
        tui::line(&format!(
            "  {} {}",
            tui::yellow("➤"),
            tui::bold(&tui::sanitize_terminal(label))
        ));
    }
    // Name what `s`/`a` would allow from now on: a binary (`cargo`), a
    // subcommand (`git status`), a host, or one exact command.
    let shown_key = tui::sanitize_terminal(tool_key);
    let scope = if tool_key.is_empty() {
        String::new()
    } else {
        format!(" `{shown_key}`")
    };
    tui::line(&tui::dim(&format!(
        "    y yes · n no · s allow{scope} this session · a always in this project · d <reason> deny"
    )));
    let q = format!("  {} ", tui::yellow("allow?"));
    let Some(ans) = ask(&q) else {
        // Esc or Ctrl+C: refuse this call and stop the turn, instead of
        // handing the refusal back to the model to try something else.
        stop_turn();
        return Some(STOPPED_BY_USER.into());
    };
    let trimmed = ans.trim();
    let lower = trimmed.to_lowercase();
    if matches!(lower.as_str(), "y" | "yes") {
        None
    } else if matches!(lower.as_str(), "s" | "session") {
        add_session_allowed_tool(cwd, tool_key);
        if !tool_key.is_empty() {
            tui::line(&tui::green(&format!(
                "  ✓ allowed for this session: {shown_key}"
            )));
        }
        None
    } else if matches!(lower.as_str(), "a" | "always") {
        add_session_allowed_tool(cwd, tool_key);
        // Scoped to this project (user settings `project_allowed`), so one
        // `a` on write_file here never silences the gate elsewhere.
        crate::config::add_project_allowed(cwd, tool_key);
        if !tool_key.is_empty() {
            tui::line(&tui::green(&format!(
                "  ✓ always allowed in this project: {shown_key} — /permissions to review or remove"
            )));
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
const READONLY_SKIP: &str = "read-only mode: mutation skipped";

/// PLAN and BRAINSTORM gate as read-only whatever the session permission is,
/// so "read-only mode" would name a setting the user never chose. Say which
/// mode is read-only and how to get out of it instead.
fn phase_readonly_reason(reason: String, phase_msg: &str) -> String {
    if reason == READONLY_SKIP {
        phase_msg.to_string()
    } else {
        reason
    }
}

pub(crate) fn gate(
    perm: Permission,
    name: &str,
    input: &serde_json::Value,
    cwd: &Path,
) -> Option<String> {
    let shell = tools::command_arg_for(name, input);
    // Approvals for shell commands are stored per binary, or per binary and
    // subcommand for multi-verb tools (`git status`, `npm test`).
    let host = tools::network_host(name, input);
    let tool_key = match (shell, &host) {
        (Some(c), _) => tools::approval_key(c),
        (None, Some(h)) => format!("fetch {h}"),
        (None, None) => name.to_string(),
    };
    // start_server and check_work run a shell command too: same
    // catastrophic and sensitive-path checks and rules, but approved as the
    // tool itself.
    let any_cmd = command_of(name, input);

    // Deny rules and network.deny refuse in every mode, before anything can
    // ask or allow.
    let rules = config::policy_rules(cwd);
    let host_ref = host.as_deref();
    if let Some(reason) = denied_by_rule(&rules, name, input, cwd, host_ref) {
        return Some(reason);
    }
    // screenshot_url runs the page in a real browser: this machine only,
    // unless an allow rule names the host. No prompt can widen that.
    if name == "screenshot_url" {
        if let Some(h) = host_ref.filter(|h| !crate::screenshot::is_loopback_host(h)) {
            if rule_for(
                &rules,
                config::RuleEffect::Allow,
                name,
                input,
                cwd,
                host_ref,
            )
            .is_none()
            {
                return Some(crate::screenshot::off_loopback_refusal(h));
            }
        }
    }

    // Read-only: refuse every mutation outright, before the sensitive-path and
    // catastrophic-command confirmations — those return "allowed" on `y`, which
    // used to let an approved `rm -rf /` through in read-only mode. Clearly
    // read-only shell commands (grep, find, git status…) still pass.
    if matches!(perm, Permission::ReadOnly) && tools::is_mutating_call(name, input) {
        let readonly_shell = shell
            .is_some_and(|c| tools::is_readonly_command(c) && tools::skips_prompt_safely(c, cwd));
        // A helper started in read-only mode is read-only too.
        let delegation = matches!(name, "task" | "spawn_subagent");
        if !readonly_shell && !delegation {
            return Some(READONLY_SKIP.into());
        }
    }
    // A write outside the working folders is refused by the tool itself
    // (tools::run), whatever the answer: asking first would offer an
    // approval that cannot work.
    if tools::out_of_cwd_mutation(name, input, cwd).is_some() {
        return None;
    }

    // A sensitive path is approved by itself: `s` and `a` cover that path,
    // not the tool or the command that reached it.
    let paths = tools::touched_paths(name, input, cwd);
    if let Some(p) = paths.iter().find(|p| tools::is_sensitive(p)) {
        let key = format!("access {}", p.display());
        if !is_pre_approved(None, &key, cwd) {
            return confirm_call(
                name,
                input,
                &format!("access sensitive path {}", p.display()),
                &key,
                cwd,
            );
        }
    }
    if let Some(p) = any_cmd.and_then(|c| tools::command_sensitive_path(c, cwd)) {
        let key = format!("access {}", p.display());
        if !is_pre_approved(None, &key, cwd) {
            return confirm_call(
                name,
                input,
                &format!("command touches sensitive path {}", p.display()),
                &key,
                cwd,
            );
        }
    }
    // In WSL2, writing to a Windows drive mount (/mnt/c/, /mnt/d/, etc.)
    // crosses the OS boundary — always confirm, even in Auto mode.
    if let Some(p) = paths.first() {
        if tools::is_mutating_call(name, input) && tools::is_wsl_windows_mount(p) {
            return confirm_call(
                name,
                input,
                &format!(
                    "write to Windows filesystem {} (WSL2 boundary)",
                    p.display()
                ),
                &tool_key,
                cwd,
            );
        }
    }
    if let Some(c) = any_cmd {
        if tools::catastrophic(c) {
            return confirm_call(
                name,
                input,
                &format!("run dangerous command `{c}`"),
                &tool_key,
                cwd,
            );
        }
        // In WSL2, commands that reference /mnt/<drive>/ target the Windows
        // filesystem — confirm before running, even in Auto mode.
        if tools::is_wsl() && tools::command_touches_wsl_mount(c) {
            return confirm_call(
                name,
                input,
                &format!("command targets Windows filesystem (WSL2): `{c}`"),
                &tool_key,
                cwd,
            );
        }
    }

    // Ask rules prompt in every mode, even when an allow rule, a saved
    // approval or auto would let the call through.
    if let Some(r) = rule_for(&rules, config::RuleEffect::Ask, name, input, cwd, host_ref) {
        return confirm_call(
            name,
            input,
            &format!(
                "{} — asks because of {}",
                tools::approval_label(name, input),
                rule_label(r)
            ),
            &tool_key,
            cwd,
        );
    }
    let allowed_by_rule = rule_for(
        &rules,
        config::RuleEffect::Allow,
        name,
        input,
        cwd,
        host_ref,
    )
    .is_some();

    // Network tools can send what the agent has read to any host and reach
    // services on the local network, so outside `auto` each new host is
    // approved once (read-only mode included). `fetch *` or a network.allow
    // entry allows them without asking. In `auto`, the tools that hand a
    // page's content back to the model (fetch_url, headless_browser) still
    // ask before reaching this machine or a private network: that is where
    // a router's admin page, a database console or the Docker API answer
    // without a login, and a prompt injection could have the agent read and
    // repeat them. Headless, with nobody to ask, such a fetch is refused
    // with how to allow it.
    if let Some(h) = host_ref {
        let auto = matches!(perm, Permission::Auto);
        let reads_back = matches!(name, "webfetch" | "fetch_url" | "headless_browser");
        let private = auto && reads_back && !allowed_by_rule && tools::private_network_host(h);
        if allowed_by_rule || (auto && !private) {
            return None;
        }
        if is_pre_approved(None, &tool_key, cwd) || is_pre_approved(None, "fetch *", cwd) {
            return None;
        }
        if private {
            return confirm_call(
                name,
                input,
                &format!(
                    "network access to {h}, on this machine or a private network (auto mode asks; add \"{h}\" to network.allow in settings to allow it) — {}",
                    tools::approval_label(name, input)
                ),
                &tool_key,
                cwd,
            );
        }
        return confirm_call_auto_allows(
            name,
            input,
            &format!(
                "network access to {h} — {}",
                tools::approval_label(name, input)
            ),
            &tool_key,
            cwd,
        );
    }
    if allowed_by_rule {
        return None;
    }

    match perm {
        Permission::Auto => None,
        // Mutations were refused above; reads anywhere and read-only shell
        // commands are allowed in readonly mode.
        Permission::ReadOnly => None,
        Permission::Ask | Permission::AcceptEdits => {
            if is_pre_approved(shell, &tool_key, cwd) {
                return None;
            }
            // Accept-edits: a file edit inside the project needs no prompt;
            // commands, deletions and everything else still ask. So does a
            // change inside `.git`: git runs what its config and hooks name
            // (fsmonitor, hooks) on its next call, bwn's own included.
            if perm == Permission::AcceptEdits
                && tools::is_file_edit(name, input)
                && paths
                    .iter()
                    .all(|p| !tools::escapes_roots(p, cwd) && !inside_git_dir(p, cwd))
            {
                return None;
            }
            if tools::is_mutating_call(name, input) {
                let mut label = tools::approval_label(name, input);
                if let Some(note) = checkpoint::undo_note(&paths) {
                    label = format!("{label} · {note}");
                }
                return confirm_call_auto_allows(name, input, &label, &tool_key, cwd);
            }
            // Out-of-cwd reads: just note it instead of hard-blocking.
            // The user asked for full filesystem access.
            None
        }
    }
}

// The shell command a call runs: run_command's and bash's, and the command
// start_server and check_work are given, which run through the same shell.
fn command_of<'a>(name: &str, input: &'a serde_json::Value) -> Option<&'a str> {
    tools::command_arg_for(name, input).or_else(|| {
        matches!(name, "start_server" | "check_work")
            .then(|| input["command"].as_str())
            .flatten()
            .filter(|c| !c.trim().is_empty())
    })
}

// Whether `p` lies inside a `.git` directory of the project or of a folder
// added with --add-dir (its own or a nested checkout's).
fn inside_git_dir(p: &Path, cwd: &Path) -> bool {
    let root = if tools::escapes_cwd(p, cwd) {
        crate::workdirs::containing(p)
    } else {
        None
    };
    tools::project_relative(p, root.as_deref().unwrap_or(cwd))
        .is_some_and(|rel| rel.split('/').any(|c| c.eq_ignore_ascii_case(".git")))
}

// ── allow / ask / deny rules ─────────────────────────────────────────────────
// `permissions` and `network` from settings, checked before the mode:
// deny > ask > allow > mode. See config::policy_rules for where they come from.

// `Tool(pattern)` → ("Tool", Some("pattern")); `Tool` → ("Tool", None).
fn split_rule(rule: &str) -> (&str, Option<&str>) {
    match rule.trim().split_once('(') {
        Some((tool, rest)) if rest.ends_with(')') => {
            (tool.trim(), Some(rest[..rest.len() - 1].trim()))
        }
        _ => (rule.trim(), None),
    }
}

// Does a rule cover this call? A network entry matches the host. For a
// permissions rule the tool part is a hook-style matcher (Claude Code names
// included) and the pattern is matched against the command for shell tools,
// the host for network tools (`domain:` optional), the query for web_search
// and the touched paths (project-relative, or absolute) for file tools. An
// allow rule must cover a whole plain command and every path; ask and deny
// rules match any part of a compound command and any path.
fn rule_covers(
    rule: &config::PolicyRule,
    name: &str,
    input: &serde_json::Value,
    cwd: &Path,
    host: Option<&str>,
) -> bool {
    if rule.network {
        return host.is_some_and(|h| host_matches(&rule.rule, h));
    }
    let Some(pattern) = rule_pattern_for(rule, name, input) else {
        return false;
    };
    let Some(pat) = pattern.filter(|p| !p.is_empty() && *p != "*") else {
        return true;
    };
    let allow = rule.effect == config::RuleEffect::Allow;
    if let Some(cmd) = command_of(name, input) {
        let pat = command_pattern(pat);
        // An allow rule covers the command exactly as written; deny and ask
        // rules see through wrappers, shells and git's own options.
        return if allow {
            tools::is_plain_command(cmd)
                && tools::skips_prompt_safely(cmd, cwd)
                && tools::rule_subjects(cmd)
                    .first()
                    .is_some_and(|c| hooks::glob_match(&pat, c))
        } else {
            tools::guard_subjects(cmd)
                .iter()
                .any(|c| hooks::glob_match(&pat, c))
        };
    }
    if matches!(name, "web_search" | "websearch") {
        return input["query"]
            .as_str()
            .is_some_and(|q| hooks::glob_match(pat, q.trim()));
    }
    if let Some(h) = host {
        return host_matches(pat.strip_prefix("domain:").unwrap_or(pat), h);
    }
    let paths = tools::touched_paths(name, input, cwd);
    let hit = |p: &PathBuf| path_matches(pat, p, cwd);
    !paths.is_empty()
        && if allow {
            paths.iter().all(hit)
        } else {
            paths.iter().any(hit)
        }
}

// The pattern part of a rule (None inside: a bare `Tool` rule) when the
// rule's tool covers this call. A rule for the shell tool (`run_command`,
// `bash`, Claude Code's `Bash`) covers every tool that runs a command, so
// check_work and start_server cannot step around `run_command(git push*)`.
fn rule_pattern_for<'a>(
    rule: &'a config::PolicyRule,
    name: &str,
    input: &serde_json::Value,
) -> Option<Option<&'a str>> {
    let (tool, pattern) = split_rule(&rule.rule);
    let names_shell = || {
        ["run_command", "bash"]
            .iter()
            .any(|t| hooks::tool_matches(tool, t))
    };
    (hooks::tool_matches(tool, name) || (command_of(name, input).is_some() && names_shell()))
        .then_some(pattern)
}

// Claude Code's prefix form `git push:*` means `git push*`.
fn command_pattern(pat: &str) -> String {
    match pat.strip_suffix(":*") {
        Some(p) => format!("{p}*"),
        None => pat.to_string(),
    }
}

// A deny rule for a shell command whose program a compound command names
// anywhere (`make && git $(echo push)` under `run_command(git push*)`):
// what that command runs cannot be read from its text.
fn compound_denied<'a>(
    rules: &'a [config::PolicyRule],
    name: &str,
    input: &serde_json::Value,
) -> Option<(&'a config::PolicyRule, String)> {
    let cmd = command_of(name, input)?;
    rules
        .iter()
        .filter(|r| r.effect == config::RuleEffect::Deny && !r.network)
        .find_map(|r| {
            let pat = rule_pattern_for(r, name, input)??;
            let pat = command_pattern(pat.trim());
            let program = pat.split_whitespace().next()?.to_string();
            tools::compound_mentions(cmd, &program).then_some((r, program))
        })
}

// Host patterns are case-insensitive and may leave out the port.
fn host_matches(pattern: &str, host: &str) -> bool {
    // Both sides spelled one way: `::ffff:127.0.0.1` in a rule or a URL is
    // 127.0.0.1.
    let pattern = crate::net::canonical_authority(pattern);
    let host = crate::net::canonical_authority(host);
    let bare = host
        .rsplit_once(':')
        .filter(|(_, port)| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()))
        .map_or(host.as_str(), |(h, _)| h);
    hooks::glob_match(&pattern, &host) || hooks::glob_match(&pattern, bare)
}

// A relative pattern names project paths (`migrations/**`, `*.lock`); an
// absolute or `~/` one names any path. `*` spans `/` too.
fn path_matches(pattern: &str, path: &Path, cwd: &Path) -> bool {
    let pattern = pattern.trim().trim_start_matches("./");
    if let Some(rest) = pattern.strip_prefix("~/") {
        let home = crate::config::user_home()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_default();
        return !home.is_empty()
            && hooks::glob_match(
                &format!("{}/{rest}", home.trim_end_matches(['/', '\\'])),
                &path.to_string_lossy(),
            );
    }
    if Path::new(pattern).is_absolute() {
        return hooks::glob_match(pattern, &path.to_string_lossy());
    }
    tools::project_relative(path, cwd).is_some_and(|rel| hooks::glob_match(pattern, &rel))
}

// The first rule with `effect` that covers the call (deny rules first by
// the caller's order).
fn rule_for<'a>(
    rules: &'a [config::PolicyRule],
    effect: config::RuleEffect,
    name: &str,
    input: &serde_json::Value,
    cwd: &Path,
    host: Option<&str>,
) -> Option<&'a config::PolicyRule> {
    rules
        .iter()
        .filter(|r| r.effect == effect)
        .find(|r| rule_covers(r, name, input, cwd, host))
}

/// Whether settings allow screenshot_url to open `host`: a `network.allow`
/// entry or an allow rule for the tool that covers it.
pub(crate) fn network_allows(host: &str, cwd: &Path) -> bool {
    let input = serde_json::json!({"url": format!("http://{host}/")});
    rule_for(
        &config::policy_rules(cwd),
        config::RuleEffect::Allow,
        "screenshot_url",
        &input,
        cwd,
        Some(host),
    )
    .is_some()
}

// The refusal for a call a deny rule or network.deny covers. A refusal by
// policy counts as a blocked change for the headless outcome.
fn denied_by_rule(
    rules: &[config::PolicyRule],
    name: &str,
    input: &serde_json::Value,
    cwd: &Path,
    host: Option<&str>,
) -> Option<String> {
    if let Some(r) = rule_for(rules, config::RuleEffect::Deny, name, input, cwd, host) {
        stopped_short(Outcome::ApprovalBlocked);
        return Some(format!("denied by {}", rule_label(r)));
    }
    let (r, program) = compound_denied(rules, name, input)?;
    stopped_short(Outcome::ApprovalBlocked);
    Some(format!(
        "denied by {}: `{program}` appears in a compound command, where what runs cannot be checked — run that command on its own",
        rule_label(r)
    ))
}

/// The refusal for a request bwn makes on its own behalf (an http hook) to
/// `host`, when network.deny names it.
pub(crate) fn network_denied(cwd: &Path, host: &str) -> Option<String> {
    config::policy_rules(cwd)
        .iter()
        .find(|r| r.network && r.effect == config::RuleEffect::Deny && host_matches(&r.rule, host))
        .map(|r| format!("denied by {}", rule_label(r)))
}

/// How a rule names itself in a refusal: `rule run_command(git push*)
/// (user settings)`, or `network.deny in user settings`.
pub fn rule_label(r: &config::PolicyRule) -> String {
    if r.network {
        format!("network.{} in {}", r.effect.as_str(), r.source)
    } else {
        format!("rule {} ({})", r.rule, r.source)
    }
}

// Ask mode skips the prompt for tools and commands in allowed_commands (the
// legacy global list), this project's "always allow" entries or this
// session's approvals. A shell command qualifies only as one plain command
// (no chaining, redirection, substitution, control characters or write/exec
// flags); a legacy single-word entry like `git` still covers every plain
// `git …` command.
fn is_pre_approved(shell: Option<&str>, tool_key: &str, cwd: &Path) -> bool {
    let mut keys = vec![tool_key.to_string()];
    if let Some(c) = shell {
        if !tools::is_plain_command(c) || !tools::skips_prompt_safely(c, cwd) {
            return false;
        }
        if let Some(bin) = tool_key.split_whitespace().next() {
            if bin != tool_key {
                keys.push(bin.to_string());
            }
        }
    }
    // Saved bare interpreter approvals (`python3`) are ignored, not rewritten.
    keys.retain(|k| !tools::is_bare_interpreter_approval(k));
    let allowed = config::load_allowed_commands();
    let project = config::load_project_allowed(cwd);
    keys.iter().any(|k| {
        allowed.iter().any(|a| a == k)
            || project.iter().any(|a| a == k)
            || is_session_allowed_tool(cwd, k)
    })
}

/// Saved approvals that `is_pre_approved` ignores: a shell or interpreter
/// approved by name (see `tools::is_bare_interpreter_approval`).
pub fn ignored_approvals(cwd: &Path) -> Vec<String> {
    let mut keys: Vec<String> = config::load_allowed_commands()
        .into_iter()
        .chain(config::load_project_allowed(cwd))
        .filter(|k| tools::is_bare_interpreter_approval(k))
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// `ignored_approvals_notice` at startup: shown in the first session that
/// finds these approvals, not at every launch (/permissions always shows it).
pub fn ignored_approvals_notice_once(cwd: &Path) -> Option<String> {
    ignored_approvals_notice(cwd).filter(|n| {
        !config::notice_seen(
            &format!("ignored-approvals {}", config::project_key(cwd)),
            n,
        )
    })
}

/// One line naming the ignored approvals and why, or None when there are none.
pub fn ignored_approvals_notice(cwd: &Path) -> Option<String> {
    let keys = ignored_approvals(cwd);
    if keys.is_empty() {
        return None;
    }
    Some(format!(
        "ignoring saved approval{} for {}: a shell or interpreter approved by name could run any code, so each of its commands is approved on its own",
        crate::report::plural(keys.len()),
        keys.join(", ")
    ))
}

// PostToolUse hooks for a call that ran; what they tell the model is added
// to its result.
fn post_tool_hooks(name: &str, input: &serde_json::Value, out: &mut tools::Outcome, cwd: &Path) {
    if let Some(said) = hooks::post_tool_use(name, input, &out.content, out.is_error, cwd) {
        out.content.push_str("\n\n");
        out.content.push_str(&said);
    }
}

/// Run the PreToolUse hook, then the gate. A hook may deny anything, but it
/// may only allow outside read-only mode: in PLAN, BRAINSTORM or a readonly
/// session the ReadOnly gate always runs after it.
pub(crate) fn hook_gate(
    perm: Permission,
    name: &str,
    input: &serde_json::Value,
    cwd: &Path,
) -> Option<String> {
    // The user stopped this turn at a prompt: run nothing else in it.
    if turn_stopped(false) {
        return Some("not run: the user stopped this turn".into());
    }
    // The legacy mcp_call reaches the same tools as `mcp__<server>__<tool>`:
    // hooks and rules naming that tool see it under that name.
    if name == "mcp_call" {
        let server = input["server"].as_str().unwrap_or("");
        let tool = input["tool"].as_str().unwrap_or("");
        if !server.is_empty() && !tool.is_empty() {
            let args = match &input["arguments"] {
                serde_json::Value::Null => serde_json::json!({}),
                a => a.clone(),
            };
            return hook_gate(perm, &crate::mcp::mangle(server, tool), &args, cwd);
        }
    }
    gate_after_hook(
        hooks::pre_tool_use(name, input, cwd),
        perm,
        name,
        input,
        cwd,
    )
}

fn gate_after_hook(
    decision: PreDecision,
    perm: Permission,
    name: &str,
    input: &serde_json::Value,
    cwd: &Path,
) -> Option<String> {
    match decision {
        PreDecision::Deny(r) => Some(r),
        // A hook's allow skips the prompts, never a deny rule.
        PreDecision::Allow if !matches!(perm, Permission::ReadOnly) => {
            let host = tools::network_host(name, input);
            denied_by_rule(
                &config::policy_rules(cwd),
                name,
                input,
                cwd,
                host.as_deref(),
            )
        }
        _ => gate(perm, name, input, cwd),
    }
}

// Public compact helper for the /compact REPL command.
pub fn compact_msgs(p: &Provider, msgs: Vec<Msg>) -> Vec<Msg> {
    compact_within(msgs, compaction_budget(p), |middle| {
        model_summary(p, middle)
    })
}

// ── BUILD mode ────────────────────────────────────────────────────────────────
pub fn run_build(
    p: &Provider,
    perm: Permission,
    role_id: &str,
    task: &str,
    cwd: &Path,
    images: Vec<(String, String)>,
) -> Result<(), String> {
    let mut transcript: Vec<Msg> = Vec::new();
    run_build_session_with_images(
        p,
        perm,
        role_id,
        task,
        cwd,
        &mut transcript,
        &crate::session::claim_or_new(),
        images,
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
    run_build_session_with_images(p, perm, role_id, task, cwd, transcript, sid, Vec::new())
}

/// A build turn whose user message carries `images` as (media_type, base64)
/// pairs alongside the task text.
#[allow(clippy::too_many_arguments)]
pub fn run_build_session_with_images(
    p: &Provider,
    perm: Permission,
    role_id: &str,
    task: &str,
    cwd: &Path,
    transcript: &mut Vec<Msg>,
    sid: &str,
    images: Vec<(String, String)>,
) -> Result<(), String> {
    // SessionStart/SessionEnd fire once per process (lib.rs); a build turn
    // only records which session and gate it runs under, then fires Stop.
    crate::session::set_current(sid);
    hooks::set_permission_mode(permission_name(perm));
    let pruned = checkpoint::begin_turn(cwd, task);
    if pruned > 0 {
        report::info(&format!(
            "  pruned {pruned} old checkpoints — this folder keeps the newest {}",
            checkpoint::KEEP_CHECKPOINTS
        ));
    }
    let r = build_turn(
        p,
        perm,
        role_id,
        task,
        cwd,
        0,
        transcript,
        Some(sid),
        images,
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
                perm,
                role_id,
                reason,
                cwd,
                0,
                transcript,
                Some(sid),
                Vec::new(),
            )
        },
    )
    .map(|_| ());
    // What the turn left each file as, so /undo can tell a later hand edit,
    // and which changes no checkpoint covers.
    checkpoint::end_turn(cwd);
    turn_done(cwd);
    crate::session::save(sid, cwd, &p.model, transcript);
    r
}

// A Stop or SubagentStop hook may send the agent on (it exits 2, or answers
// {"decision": "block", "reason": …}): its reason is the next user message.
// At most this many rounds in a row, so a hook that always objects cannot
// keep a turn going forever.
const MAX_STOP_HOOK_ROUNDS: usize = 3;

thread_local! {
    // Set just before a Stop hook's reason starts the next round of a turn
    // (taken by build_turn).
    static CONTINUING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // The last turn was stopped before it finished, by the user (Esc,
    // Ctrl+C, a cancelled prompt) or a UserPromptSubmit hook; no Stop hook
    // sends it on.
    static STOPPED_EARLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// Runs `event`'s hooks after the reply `r` and, while one asks to go on and
// the turn ended normally, `more` with its reason (the next round's reply).
// `fields` adds the event's payload fields for a reply.
fn after_stop_hooks(
    event: &str,
    cwd: &Path,
    fields: impl Fn(&Result<String, String>) -> serde_json::Value,
    mut r: Result<String, String>,
    mut more: impl FnMut(&str) -> Result<String, String>,
) -> Result<String, String> {
    for round in 0..=MAX_STOP_HOOK_ROUNDS {
        let mut payload = fields(&r);
        payload["stop_hook_active"] = serde_json::json!(round > 0);
        let Some(reason) = hooks::stop(event, cwd, payload) else {
            break;
        };
        if r.is_err() || STOPPED_EARLY.with(|u| u.get()) || crate::usage::budget_stop().is_some() {
            break;
        }
        if round == MAX_STOP_HOOK_ROUNDS {
            report::notice(&format!(
                "  ⚠ {event} hook still asks to continue after {MAX_STOP_HOOK_ROUNDS} rounds — stopping here"
            ));
            break;
        }
        report::notice(&format!(
            "  ⟳ {event} hook: {}",
            tui::sanitize_terminal(&trace::preview(&reason, 160))
        ));
        r = more(&reason);
    }
    r
}

// The Notification hook for a finished turn.
fn turn_done(cwd: &Path) {
    hooks::notification("done", "done — ready for your next prompt", cwd);
}

struct AgentRunningGuard;

impl AgentRunningGuard {
    fn new() -> Self {
        set_turn_stopped(false);
        STOPPED_EARLY.with(|u| u.set(false));
        tui::set_agent_running(true);
        Self
    }
}

impl Drop for AgentRunningGuard {
    fn drop(&mut self) {
        set_turn_stopped(false);
        tui::set_agent_running(false);
    }
}

// A fresh transcript gets the system prompt first, and one carried in from
// another mode gets the build prompt in its place; then the turn's user
// message, multimodal when images are attached, so a first-prompt image never
// displaces the system prompt.
fn open_turn(
    msgs: &mut Vec<Msg>,
    system: impl FnOnce() -> String,
    task: &str,
    images: Vec<(String, String)>,
) {
    match msgs.first() {
        None => msgs.push(Msg::System(system())),
        // A conversation carried in from BRAINSTORM, PLAN or a chat turn
        // gets the build prompt in place of theirs.
        Some(Msg::System(s))
            if [BRAINSTORM_HEAD, PLAN_HEAD, CHAT_HEAD]
                .iter()
                .any(|h| s.starts_with(h)) =>
        {
            msgs[0] = Msg::System(system());
        }
        Some(Msg::System(_)) => {}
        Some(_) => msgs.insert(0, Msg::System(system())),
    }
    msgs.push(user_msg(task.to_string(), images));
}

// A user message, multimodal when images are attached.
fn user_msg(text: String, images: Vec<(String, String)>) -> Msg {
    if images.is_empty() {
        Msg::User(text)
    } else {
        Msg::UserImages { text, images }
    }
}

// `sid` is Some for the top-level session (per-round transcript saves) and
// None for subagents, whose transcripts live inside the parent's results.
#[allow(clippy::too_many_arguments)]
fn build_inner(
    p: &Provider,
    perm: Permission,
    role_id: &str,
    task: &str,
    cwd: &Path,
    depth: usize,
    msgs: &mut Vec<Msg>,
    sid: Option<&str>,
) -> Result<String, String> {
    build_turn(p, perm, role_id, task, cwd, depth, msgs, sid, Vec::new())
}

/// A message `provider::request` refused before sending (bigger than the
/// server's window) stays out of the conversation, or every later message
/// would carry it to the server. A conversation left with only its system
/// prompt is emptied, so nothing is saved for it.
fn drop_refused_message(p: &Provider, msgs: &mut Vec<Msg>) {
    if provider::oversized_message(p, msgs).is_some() {
        msgs.pop();
        if msgs.iter().all(|m| matches!(m, Msg::System(_))) {
            msgs.clear();
        }
    }
}

/// How a BUILD step that got text and no tool calls goes on.
enum TextReply {
    /// The model was nudged; ask it again.
    Again,
    /// The turn ends with this.
    Done(Result<String, String>),
}

/// A BUILD step's reply had text and no tool calls: an empty reply is
/// retried once, how-to prose gets a nudge to act, an explicit file or
/// artifact task is finished on the model's behalf, and anything else ends
/// the turn with the text.
#[allow(clippy::too_many_arguments)]
fn on_text_reply(
    st: &mut TurnState,
    reply: Reply,
    task_for_recovery: &str,
    msgs: &mut Vec<Msg>,
    perm: Permission,
    cwd: &Path,
    depth: usize,
    defs: &[tools::ToolDef],
) -> TextReply {
    // One empty response is retried before ending the run; a second
    // empty response ends it.
    if reply.text.trim().is_empty() {
        // No assistant turn is recorded here: an empty assistant
        // message would serialize to an empty content array (API 400).
        if !st.empty_reply_retried {
            st.empty_reply_retried = true;
            report::notice("  ⚠ model returned no output — asking it to continue");
            msgs.push(Msg::User(
                    "You returned no output. Continue the task: call a tool or state your final answer."
                        .to_string(),
                ));
            return TextReply::Again;
        }
        report::notice("  ⚠ model returned no output");
        if depth == 0 && (!st.any_tool_ran || st.last_unrun.is_some()) {
            stopped_short(Outcome::Failed);
        }
        return TextReply::Done(Ok(reply.text));
    }
    // Imperative task answered with how-to prose instead of tool
    // calls: tell the model to act, bounded per task. Never fires
    // once a mutating tool has run (that prose is a real summary),
    // nor in a review, whose answer is prose by design (the diff in
    // its task reads as an order to change things).
    if !REVIEWING.load(Ordering::Relaxed)
        && should_nudge_to_act(
            task_for_recovery,
            &reply.text,
            st.any_tool_ran,
            st.mutating_tool_ran,
            st.act_nudges,
        )
    {
        st.act_nudges += 1;
        report::notice("  ⟳ model explained instead of acting — nudging it to use tools");
        msgs.push(Msg::Assistant {
            text: reply.text.clone(),
            calls: vec![],
        });
        msgs.push(Msg::User(
            "Do not explain how to do it — actually do it now. Use your tools \
                 (write_file/edit_file/run_command) to make the changes. Begin immediately \
                 with your first tool call."
                .to_string(),
        ));
        return TextReply::Again;
    }
    if let Some(input) = auto_create_file_input(task_for_recovery) {
        // Record this harness recovery in the transcript so saved and
        // resumed sessions aren't missing turns.
        msgs.push(Msg::Assistant {
            text: reply.text.clone(),
            calls: vec![],
        });
        if let Some(reason) = hook_gate(perm, "write_file", &input, cwd) {
            report::tool_denied(&reason);
            msgs.push(Msg::User(format!(
                "[harness] write_file recovery blocked: {reason}"
            )));
            return TextReply::Done(Err(reason));
        }
        report::tool_call("write_file", &tools::preview("write_file", &input), &input);
        trace_tool_call("write_file", &input, "build", depth);
        let mut out = tools::run("write_file", &input, cwd);
        post_tool_hooks("write_file", &input, &mut out, cwd);
        report::tool_result("write_file", &out.content, out.is_error);
        trace_tool_result("write_file", &out.content, out.is_error, "build", depth);
        if out.is_error {
            msgs.push(Msg::User(format!(
                "[harness] write_file recovery failed: {}",
                out.content
            )));
            return TextReply::Done(Err(out.content));
        }
        msgs.push(Msg::User(format!(
            "[harness] completed the explicit file creation with write_file: {}",
            out.content
        )));
        return TextReply::Done(Ok(format!(
                "{}\n\nCompleted the explicit file creation task after the model returned prose without tool calls.",
                out.content
            )));
    }
    if let Some(input) = auto_static_artifact_input(task_for_recovery, &reply.text) {
        msgs.push(Msg::Assistant {
            text: reply.text.clone(),
            calls: vec![],
        });
        if let Some(reason) = hook_gate(perm, "Artifact", &input, cwd) {
            report::tool_denied(&reason);
            msgs.push(Msg::User(format!(
                "[harness] Artifact recovery blocked: {reason}"
            )));
            return TextReply::Done(Err(reason));
        }
        report::tool_call("Artifact", &tools::preview("Artifact", &input), &input);
        trace_tool_call("Artifact", &input, "build", depth);
        let mut out = tools::run("Artifact", &input, cwd);
        post_tool_hooks("Artifact", &input, &mut out, cwd);
        report::tool_result("Artifact", &out.content, out.is_error);
        trace_tool_result("Artifact", &out.content, out.is_error, "build", depth);
        if out.is_error {
            // Tell the model exactly why the artifact was rejected and
            // let it fix and re-publish; fail honestly once bounded
            // recovery attempts run out.
            if st.artifact_error_count < 2 {
                st.artifact_error_count += 1;
                msgs.push(Msg::User(format!(
                    "The HTML artifact you wrote in plain text was rejected: {}. \
                         Fix that problem and re-publish: call Artifact/publish_artifact with one \
                         complete self-contained HTML file. Do not answer with markdown code.",
                    out.content
                )));
                return TextReply::Again;
            }
            return TextReply::Done(Err(format!(
                "artifact publishing failed after {} recovery attempts; last rejection: {}",
                st.artifact_error_count, out.content
            )));
        }
        return TextReply::Done(Ok(out.content));
    }
    if static_artifact_requested(task_for_recovery) {
        msgs.push(Msg::Assistant {
            text: reply.text.clone(),
            calls: vec![],
        });
        if st.static_artifact_recovery_count < 2 {
            st.static_artifact_recovery_count += 1;
            msgs.push(Msg::User(static_artifact_recovery_prompt(
                task_for_recovery,
            )));
            return TextReply::Again;
        }
        // No fabricated fallback artifact: report the failure honestly.
        return TextReply::Done(Err(format!(
            "the model did not produce a runnable web artifact after \
                 {} recovery prompts; its last response was:\n{}",
            st.static_artifact_recovery_count, reply.text
        )));
    }
    msgs.push(Msg::Assistant {
        text: reply.text.clone(),
        calls: vec![],
    });
    if depth == 0 {
        if let Some(note) = unrun_call_note(st.last_unrun.as_deref(), &reply.text, defs) {
            report::notice(&note);
            stopped_short(Outcome::Failed);
        }
        report::refused_this_turn();
    }
    TextReply::Done(Ok(reply.text))
}

/// What one BUILD turn tracks across its steps.
#[derive(Default)]
struct TurnState {
    /// Files read this turn, for read-before-write.
    read_paths: std::collections::HashSet<PathBuf>,
    loop_guard: ToolLoopGuard,
    artifact_error_count: usize,
    static_artifact_recovery_count: usize,
    empty_reply_retried: bool,
    malformed_markup_retried: bool,
    token_limit_continuations: usize,
    forced_compact_retry: bool,
    verifier_fix_rounds: usize,
    /// Act-don't-explain: nudges spent, whether any call reached dispatch,
    /// and whether a mutating tool actually ran.
    act_nudges: usize,
    any_tool_ran: bool,
    mutating_tool_ran: bool,
    /// Executed calls and touched files, for the verifier pass.
    tool_records: Vec<crate::verifier::ToolCallRecord>,
    changed_files: Vec<String>,
    /// check_work: whether the model ran it, its last verdict, and how often
    /// the harness ran it on the model's behalf.
    check_work_called: bool,
    check_work_passed: Option<bool>,
    auto_check_rounds: usize,
    /// The first call of the last tool round that never ran (unknown tool,
    /// invalid or missing arguments): ending on text after one is not success.
    last_unrun: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn build_turn(
    p: &Provider,
    perm: Permission,
    role_id: &str,
    task: &str,
    cwd: &Path,
    depth: usize,
    msgs: &mut Vec<Msg>,
    sid: Option<&str>,
    images: Vec<(String, String)>,
) -> Result<String, String> {
    let _running_guard = (depth == 0).then(AgentRunningGuard::new);
    // A Stop hook's reason goes on with the same turn: it is not a prompt
    // the user submitted, and /undo still covers the whole turn.
    let continuing = CONTINUING.with(|c| c.replace(false));
    // Top-level runs mark a turn boundary so bare /undo can revert exactly
    // this run's writes; subagent recursion must not shrink that window.
    if depth == 0 && !continuing {
        checkpoint::mark_turn_start();
        report::mark_turn_denials();
    }
    let task = match hooks::user_prompt_submit(task, cwd) {
        _ if continuing => task.to_string(),
        Err(reason) => {
            report::error(&format!("blocked by hook: {reason}"));
            STOPPED_EARLY.with(|u| u.set(true));
            if depth == 0 {
                stopped_short(Outcome::HookBlocked);
            }
            return Ok(String::new());
        }
        Ok(ctx) if !ctx.is_empty() => format!("{task}\n\n[hook context]\n{ctx}"),
        Ok(_) => task.to_string(),
    };
    let task_for_recovery = recovery_task_text(&task);
    let helper = helper_ctx(depth);
    let custom = helper.agent.clone();
    let defs = offered(
        p,
        if matches!(perm, Permission::ReadOnly) || helper.read_only {
            readonly_build_defs(perm, depth, p.context_tokens)
        } else {
            tools::defs_for_context(depth < MAX_DEPTH, p.context_tokens)
        },
    );
    let defs = match &custom {
        Some(a) => agent_defs_only(defs, a),
        None => defs,
    };
    let system = || {
        // Role identity and the current-mode contract come FIRST; the
        // environment/tool-manifest/skills/memory sections follow.
        let mut sys = match &custom {
            Some(a) => agent_system(a),
            None => String::from(role(role_id).system),
        };
        if let Some(guidance) = artifact_guidance(&task_for_recovery) {
            sys.push_str("\n\n");
            sys.push_str(&guidance);
        }
        sys.push_str("\n\n");
        sys.push_str(&context_prefix(cwd, p.context_tokens));
        sys
    };
    open_turn(msgs, system, &task, images);
    // A folder added (/add-dir) since the system prompt was written, or a
    // resumed conversation that names folders this session lacks: write it
    // again so the model knows where it may work.
    if let Some(Msg::System(s)) = msgs.first() {
        let stale = match crate::workdirs::prompt_marker() {
            Some(marker) => !s.contains(&marker),
            None => s.contains(crate::workdirs::PROMPT_HEAD),
        };
        if stale {
            msgs[0] = Msg::System(system());
        }
    }
    // Saved before the first request too, so a run killed mid-request
    // still leaves its task on disk to resume; not a message the request
    // will refuse as bigger than the window, which never joins it.
    if let Some(sid) = sid.filter(|_| provider::oversized_message(p, msgs).is_none()) {
        crate::session::save(sid, cwd, &p.model, msgs);
    }

    let mut st = TurnState::default();

    for step in 1..=MAX_ITERS {
        if tui::interrupted() {
            report::notice(take_interrupt(depth));
            return Ok(String::new());
        }
        if budget_exhausted() {
            return Ok(String::new());
        }
        maybe_compact(p, msgs);
        if step > 1 && !report::is_json() {
            tui::line(&tui::dim(&format!("  ↻ step {step}")));
        }
        // PrePrompt: fires before every model request in a build turn.
        hooks::notify("PrePrompt", cwd);
        let reply = match request_reply(p, msgs.as_slice(), &defs, "thinking") {
            Ok(r) => r,
            Err(e) => {
                let lower = e.to_ascii_lowercase();
                if lower.contains("interrupted") {
                    report::notice(take_interrupt(depth));
                    return Ok(String::new());
                }
                // A context-overflow rejection is recoverable: force-compact the
                // transcript and retry once instead of dying.
                if !st.forced_compact_retry
                    && (lower.contains("prompt is too long") || lower.contains("context length"))
                {
                    st.forced_compact_retry = true;
                    // Compaction keeps the system prompt and the newest
                    // message: with nothing in between to shrink, the same
                    // request would only be sent again.
                    if msgs.len() > 2 {
                        hooks::pre_compact("auto", "", cwd);
                        report::notice("  ⟳ context overflow — force-compacting and retrying…");
                        let taken = std::mem::take(msgs);
                        *msgs = compact_within(taken, compaction_budget(p), |middle| {
                            model_summary(p, middle)
                        });
                        continue;
                    }
                }
                drop_refused_message(p, msgs);
                hooks::notify("OnError", cwd);
                return Err(e);
            }
        };
        let reply = normalize_text_tool_calls(reply, &defs, &task_for_recovery);
        hooks::notify("PostResponse", cwd);
        // Output truncated at the token limit: the reply (and any tool call in
        // it) may be incomplete — ask the model to continue instead of acting
        // on or returning a cut-off result. Bounded to avoid loops.
        if reply_truncated_at_token_limit(&reply)
            && st.token_limit_continuations < MAX_TOKEN_LIMIT_CONTINUATIONS
        {
            st.token_limit_continuations += 1;
            report::notice(
                "  ⚠ output truncated at the token limit — asking the model to continue",
            );
            let follow_up = if reply.calls.is_empty() {
                "Your answer was cut off at the token limit — continue from where you stopped."
            } else {
                "Your response hit the output token limit mid tool call, so the call was \
                 discarded. Re-issue the complete tool call with smaller content — e.g. split \
                 large writes into multiple write_file/edit_file calls — and continue."
            };
            // Drop the possibly-truncated calls so no dangling tool_use block
            // reaches the API; keep the text (when present) for continuity —
            // an empty assistant turn would serialize to an empty content array.
            if !reply.text.trim().is_empty() {
                msgs.push(Msg::Assistant {
                    text: reply.text.clone(),
                    calls: vec![],
                });
            }
            msgs.push(Msg::User(follow_up.to_string()));
            continue;
        }
        // Tool markup that didn't parse into a call: the model tried to act
        // but emitted broken JSON inside the tags. Reprompting once turns a
        // garbage "final answer" into a corrected call; bounded so a model
        // that can't produce valid JSON still terminates.
        if malformed_tool_markup(&reply) && !st.malformed_markup_retried {
            st.malformed_markup_retried = true;
            report::notice("  ⚠ tool-call markup didn't parse — asking the model to re-emit it");
            trace::record_visible(
                "tool_input_repaired",
                "reprompted malformed tagged tool call",
                serde_json::json!({"text_preview": trace::preview(&reply.text, 120)}),
            );
            msgs.push(Msg::Assistant {
                text: reply.text.clone(),
                calls: vec![],
            });
            msgs.push(Msg::User(
                "Your tool call could not be parsed — the tagged JSON was malformed or \
                 incomplete. Re-emit it as exactly one JSON object of the form \
                 {\"name\": \"<tool>\", \"arguments\": { … }} inside a single <tool_call> tag, \
                 with nothing else in the message."
                    .to_string(),
            ));
            continue;
        }
        if reply.calls.is_empty() {
            match on_text_reply(
                &mut st,
                reply,
                &task_for_recovery,
                msgs,
                perm,
                cwd,
                depth,
                &defs,
            ) {
                TextReply::Again => continue,
                TextReply::Done(r) => return r,
            }
        }

        let mut results = Vec::new();
        let mut summary: Option<String> = None;
        let mut loop_nudge: Option<String> = None;
        let mut loop_stop: Option<String> = None;
        let mut artifact_recovery: Option<String> = None;
        let mut round_unrun: Option<String> = None;
        // Helpers from this reply that may run side by side (they only read,
        // or work in a worktree of their own), held until the next call that
        // is not one of them or the end of the reply, so every other call
        // still runs in the order the model gave. Only the top-level turn
        // starts helpers side by side.
        let side_by_side_ok = depth == 0 && MAX_PARALLEL_HELPERS.load(Ordering::Relaxed) > 1;
        let mut waiting: Vec<WaitingHelper> = Vec::new();
        for call in &reply.calls {
            let side_by_side = (side_by_side_ok
                && matches!(call.name.as_str(), "task" | "spawn_subagent")
                && call.input.get(tools::INVALID_ARGS).is_none())
            .then(|| resolve_helper(perm, &call.input, cwd, depth).ok())
            .flatten()
            .filter(|s| s.read_only || s.isolate);
            if side_by_side.is_none() && !waiting.is_empty() {
                start_waiting_helpers(p, perm, &mut waiting, &mut results, cwd, depth);
            }
            if let Some(raw) = call.input.get(tools::INVALID_ARGS).and_then(|v| v.as_str()) {
                let msg = invalid_args_feedback(&call.name, raw, &defs);
                round_unrun.get_or_insert_with(|| format!("{} (invalid arguments)", call.name));
                report::tool_denied(&msg);
                note_loop_result(
                    &mut st.loop_guard,
                    &call.name,
                    &call.input,
                    &msg,
                    true,
                    &mut loop_nudge,
                    &mut loop_stop,
                );
                results.push(ToolResult {
                    id: call.id.clone(),
                    content: msg,
                    is_error: true,
                    images: Vec::new(),
                });
                continue;
            }
            let refusal = text_only_refusal(p, &call.name).or_else(|| {
                let r = unoffered_call(&call.name, &defs, false);
                if r.is_some() {
                    round_unrun.get_or_insert_with(|| format!("{} (not a tool here)", call.name));
                }
                r
            });
            if let Some((shown, told)) = refusal {
                report::tool_denied(&shown);
                note_loop_result(
                    &mut st.loop_guard,
                    &call.name,
                    &call.input,
                    &told,
                    true,
                    &mut loop_nudge,
                    &mut loop_stop,
                );
                results.push(ToolResult {
                    id: call.id.clone(),
                    content: told,
                    is_error: true,
                    images: Vec::new(),
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
            st.any_tool_ran = true;
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
                    &mut st.loop_guard,
                    &call.name,
                    &call_input,
                    &answer,
                    is_error,
                    &mut loop_nudge,
                    &mut loop_stop,
                );
                results.push(ToolResult {
                    id: call.id.clone(),
                    content: answer,
                    is_error,
                    images: Vec::new(),
                });
                continue;
            }

            let reviewing = REVIEWING.load(Ordering::Relaxed);
            // Outside an agent file's tools is a slip of the helper's, like
            // an edit during a review: refused, but not a blocked change.
            let off_surface = agent_tool_refusal(custom.as_ref(), &call.name).or_else(|| {
                helper
                    .read_only
                    .then(|| read_only_helper_refusal(&call.name, &call_input, cwd))
                    .flatten()
            });
            // A helper's refused call is one too: the parent's run did not
            // do what the helper was asked, whatever its summary says.
            let counted = !reviewing;
            let mut checks_unapproved = false;
            let reason = off_surface.or_else(|| {
                if call.name != "check_work" {
                    return hook_gate(perm, &call.name, &call_input, cwd);
                }
                let (r, no_terminal) = gate_checks(perm, &call_input, cwd)?;
                checks_unapproved = no_terminal;
                Some(r)
            });
            let reason = reason.map(|r| {
                let r = if reviewing {
                    phase_readonly_reason(r, REVIEW_READONLY)
                } else {
                    r
                };
                match crate::mcp::untrusted_read_only_hint(&call.name) {
                    Some(hint) => format!("{r} — {hint}"),
                    None => r,
                }
            });
            if let Some(reason) = reason {
                report::tool_denied(&reason);
                trace::record_visible(
                    "tool_denied",
                    format!("{} denied", call.name),
                    serde_json::json!({"tool": call.name, "reason": reason, "input": &call_input, "phase": "build", "depth": depth}),
                );
                // A review asked for findings, not changes: a refused edit
                // there is the model's slip, not a failed run. Checks nobody
                // could approve leave the work as it is, unverified, and the
                // automatic round would only be refused again.
                let reason = if checks_unapproved {
                    st.check_work_called = true;
                    report::notice(&format!("  {CHECKS_NOT_RUN}"));
                    format!(
                        "{reason} — the project's {CHECKS_NOT_RUN}. Do not retry them; \
                         finish, and say in your summary that they were not run."
                    )
                } else {
                    if counted {
                        report::note_denial(
                            &call.name,
                            &tools::preview(&call.name, &call_input),
                            &reason,
                            tools::is_mutating_call(&call.name, &call_input),
                        );
                    }
                    if depth > 0 {
                        note_helper_refusal(depth, &tools::preview(&call.name, &call_input));
                    }
                    reason
                };
                note_loop_result(
                    &mut st.loop_guard,
                    &call.name,
                    &call_input,
                    &reason,
                    true,
                    &mut loop_nudge,
                    &mut loop_stop,
                );
                results.push(ToolResult {
                    id: call.id.clone(),
                    content: reason,
                    is_error: true,
                    images: Vec::new(),
                });
                continue;
            }

            // Record reads so we can enforce read-before-write below.
            if let Some(p) = tools::read_tracking_path(&call.name, &call_input, cwd) {
                st.read_paths.insert(p);
            }

            // Require the model to read a file before overwriting or patching it.
            if let Some(p) = tools::edit_tracking_path(&call.name, &call_input, cwd) {
                if p.exists() && !st.read_paths.contains(&p) {
                    let msg = format!(
                        "read('{}') required before editing or deleting existing file. Read the file first, then retry.",
                        p.display()
                    );
                    report::tool_denied(&msg);
                    note_loop_result(
                        &mut st.loop_guard,
                        &call.name,
                        &call_input,
                        &msg,
                        true,
                        &mut loop_nudge,
                        &mut loop_stop,
                    );
                    results.push(ToolResult {
                        id: call.id.clone(),
                        content: msg,
                        is_error: true,
                        images: Vec::new(),
                    });
                    continue;
                }
            }

            if matches!(call.name.as_str(), "task" | "spawn_subagent") {
                // A subagent can mutate the workspace — count it as action.
                st.mutating_tool_ran = true;
                if let Some(spec) = side_by_side {
                    waiting.push(WaitingHelper {
                        slot: results.len(),
                        name: call.name.clone(),
                        spec,
                    });
                    results.push(ToolResult {
                        id: call.id.clone(),
                        content: String::new(),
                        is_error: false,
                        images: Vec::new(),
                    });
                    continue;
                }
                let (out, is_error) = spawn_subagent(p, perm, &call_input, cwd, depth);
                report::tool_result(&call.name, &out, is_error);
                trace_tool_result(&call.name, &out, is_error, "build", depth);
                results.push(ToolResult {
                    id: call.id.clone(),
                    content: out,
                    is_error,
                    images: Vec::new(),
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
                        images: Vec::new(),
                    });
                    continue;
                }
            }

            if tools::is_mutating_call(&call.name, &call_input) {
                st.mutating_tool_ran = true;
            }
            let mut out = vision_checked(p, tools::run(&call.name, &call_input, cwd));
            post_tool_hooks(&call.name, &call_input, &mut out, cwd);
            if out.is_error {
                hooks::notify("OnError", cwd);
                if call_could_not_run(&call.name, &call_input, &defs) {
                    round_unrun.get_or_insert_with(|| {
                        let why = out.content.lines().next().unwrap_or("").trim();
                        format!("{} ({})", call.name, trace::preview(why, 100))
                    });
                }
            }
            report::tool_result(&call.name, &out.content, out.is_error);
            trace_tool_result(&call.name, &out.content, out.is_error, "build", depth);
            if call.name == "check_work" {
                st.check_work_called = true;
                if !check_work_found_no_project(&out.content) {
                    st.check_work_passed = Some(!out.is_error);
                }
            }
            // Feed the verifier real data: every executed call, and the files
            // touched by successful write/edit tools.
            st.tool_records.push(crate::verifier::ToolCallRecord {
                tool_name: call.name.clone(),
                args_summary: tools::preview(&call.name, &call_input),
                result_preview: out.content.chars().take(200).collect(),
                timestamp: String::new(),
            });
            if !out.is_error {
                if let Some(pb) = tools::edit_tracking_path(&call.name, &call_input, cwd) {
                    let path = pb.display().to_string();
                    if !st.changed_files.contains(&path) {
                        st.changed_files.push(path);
                    }
                }
            }
            if matches!(call.name.as_str(), "Artifact" | "publish_artifact")
                && out.is_error
                && (static_artifact_requested(&task_for_recovery)
                    || canvas_game_requested(&task_for_recovery))
            {
                // No fabricated fallback artifact: tell the model exactly why
                // publishing was rejected and ask it to fix and re-publish;
                // fail honestly after bounded recovery attempts.
                st.artifact_error_count += 1;
                if st.artifact_error_count <= 2 {
                    artifact_recovery = Some(format!(
                        "Your artifact was rejected: {}. Fix that exact problem and call \
                         Artifact/publish_artifact again with one complete self-contained HTML file \
                         (all CSS/JS inline). Do not answer with markdown code.",
                        out.content
                    ));
                } else {
                    loop_stop = Some(format!(
                        "artifact publishing failed after {} attempts; \
                         last rejection: {}",
                        st.artifact_error_count, out.content
                    ));
                }
            }
            if out.finished {
                report::finish(&out.content);
                if depth == 0 {
                    report::refused_this_turn();
                }
                summary = Some(out.content.clone());
            } else {
                note_loop_result(
                    &mut st.loop_guard,
                    &call.name,
                    &call_input,
                    &out.content,
                    out.is_error,
                    &mut loop_nudge,
                    &mut loop_stop,
                );
            }
            results.push(ToolResult {
                id: call.id.clone(),
                content: out.content,
                is_error: out.is_error,
                images: out.images,
            });
        }
        if !waiting.is_empty() {
            start_waiting_helpers(p, perm, &mut waiting, &mut results, cwd, depth);
        }

        msgs.push(Msg::Assistant {
            text: assistant_tool_turn_text(reply.text, &reply.calls),
            calls: reply.calls,
        });
        msgs.push(Msg::Tool(results));
        st.last_unrun = round_unrun;
        // Persist the transcript after every tool round so a crash or kill
        // doesn't lose the session (save is cheap and swallows I/O errors).
        if let Some(sid) = sid {
            crate::session::save(sid, cwd, &p.model, msgs);
        }
        // The refused call's own line already says the turn stopped.
        if turn_stopped(depth == 0) {
            return Ok(String::new());
        }
        if !report::is_json() {
            // The footer shows the conversation's context, not that of one
            // of several helpers running at once.
            if !tui::capturing() {
                tui::context_meter(context_used(msgs), p.context_tokens);
            }
            tui::poll_typeahead();
        }
        if let Some(s) = summary {
            // The model finished after editing files without checking its
            // work: run check_work once on its behalf and, if the project's
            // checks fail, hand the report back for one more round.
            if should_auto_check_work(
                perm,
                depth,
                st.changed_files.len(),
                st.check_work_called,
                st.auto_check_rounds,
            ) {
                st.auto_check_rounds += 1;
                let input = serde_json::json!({});
                report::notice(
                    "  ⟳ finish called without check_work — running the project's checks",
                );
                report::tool_call("check_work", &tools::preview("check_work", &input), &input);
                trace_tool_call("check_work", &input, "build", depth);
                // The project's scripts run like any model call: hook, then gate.
                // A denial skips the round and is noted in the tool record. A
                // refusal for want of a terminal is not a blocked change.
                let mut out = match gate_checks(perm, &input, cwd) {
                    Some((reason, no_terminal)) => {
                        report::tool_denied(&reason);
                        report::notice(&if no_terminal {
                            format!("  {CHECKS_NOT_RUN}")
                        } else {
                            format!("  checks were not run ({reason})")
                        });
                        trace_tool_result("check_work", &reason, true, "build", depth);
                        None
                    }
                    None => Some(tools::run("check_work", &input, cwd)),
                };
                if let Some(out) = &mut out {
                    post_tool_hooks("check_work", &input, out, cwd);
                    report::tool_result("check_work", &out.content, out.is_error);
                    trace_tool_result("check_work", &out.content, out.is_error, "build", depth);
                }
                let preview = out
                    .as_ref()
                    .map_or("skipped: denied by the permission gate", |o| {
                        o.content.as_str()
                    });
                st.tool_records.push(crate::verifier::ToolCallRecord {
                    tool_name: "check_work".into(),
                    args_summary: tools::preview("check_work", &input),
                    result_preview: preview.chars().take(200).collect(),
                    timestamp: String::new(),
                });
                if let Some(out) = out.filter(|o| !check_work_found_no_project(&o.content)) {
                    st.check_work_passed = Some(!out.is_error);
                    if out.is_error {
                        msgs.push(Msg::User(format!(
                            "[harness] You called finish without running check_work, so it ran \
                             automatically and the project's checks FAILED:\n{}\nFix the failures, \
                             run check_work again, then call finish.",
                            out.content
                        )));
                        continue;
                    }
                }
            }
            let s = if st.auto_check_rounds > 0 && st.check_work_passed == Some(false) {
                report::notice("  ⚠ check_work failed — finishing anyway; see the report above");
                format!(
                    "{s}\n\n[check_work note] The project's checks still fail after the \
                     automatic check_work round."
                )
            } else {
                s
            };
            if depth == 0 {
                let verifier = crate::verifier::Verifier::new(&cwd.to_string_lossy());
                let ctx = crate::verifier::VerificationContext {
                    task_description: task.to_string(),
                    changed_files: st.changed_files.clone(),
                    tool_calls: st.tool_records.clone(),
                    tests_passed: st.check_work_passed,
                    ..Default::default()
                };
                let rep = verifier.verify(&ctx);
                report::verify(
                    &rep.status.to_string(),
                    &crate::verifier::Verifier::format_report_json(&rep),
                );
                match rep.status {
                    crate::verifier::VerificationStatus::Blocked
                    | crate::verifier::VerificationStatus::Failed
                        if st.verifier_fix_rounds < MAX_VERIFIER_FIX_ROUNDS =>
                    {
                        // Give the model a chance to address the violations
                        // before finishing.
                        st.verifier_fix_rounds += 1;
                        let violations =
                            crate::rules::RuleEngine::format_violations(&rep.rule_violations);
                        report::notice(&format!(
                            "  ⚠ verification {} — asking the model to address violations",
                            rep.status.label()
                        ));
                        if !report::is_json() {
                            tui::line(&tui::render_md(&violations));
                        }
                        msgs.push(Msg::User(format!(
                            "Verification of your finished work returned `{}` with these rule \
                             violations:\n{violations}\nAddress them, then call finish again.",
                            rep.status
                        )));
                        continue;
                    }
                    crate::verifier::VerificationStatus::Blocked
                    | crate::verifier::VerificationStatus::Failed => {
                        // Fix rounds exhausted: finish anyway with an honest note.
                        let violations =
                            crate::rules::RuleEngine::format_violations(&rep.rule_violations);
                        report::notice(&format!("  ⚠ verification {}", rep.status.label()));
                        if st.check_work_passed == Some(false) {
                            stopped_short(Outcome::CheckWorkFailed);
                        }
                        stopped_short(Outcome::VerificationFailed);
                        return Ok(format!(
                            "{s}\n\n[verification note] The verifier still reports `{}` after \
                             {} fix attempts:\n{violations}",
                            rep.status, st.verifier_fix_rounds
                        ));
                    }
                    crate::verifier::VerificationStatus::PassedWithWarnings => {
                        report::notice(&format!("  ⚠ verification {}", rep.status.label()));
                        if !rep.rule_violations.is_empty() && !report::is_json() {
                            tui::line(&tui::render_md(
                                &crate::rules::RuleEngine::format_violations(&rep.rule_violations),
                            ));
                        }
                    }
                    crate::verifier::VerificationStatus::Passed => {}
                }
                // The last check_work verdict stands: finishing over failing
                // checks is not a success.
                if st.check_work_passed == Some(false) {
                    stopped_short(Outcome::CheckWorkFailed);
                }
            }
            return Ok(s);
        }
        if let Some(stop_msg) = loop_stop {
            // A stopped loop (or exhausted artifact recovery) is not success —
            // surface it as a failure with the honest summary. The terminal
            // shows the error itself; --json also gets it as the answer.
            if report::is_json() {
                report::assistant(&stop_msg);
            }
            return Err(stop_msg);
        }
        if let Some(recovery) = artifact_recovery {
            msgs.push(Msg::User(recovery));
        } else if let Some(nudge) = loop_nudge {
            msgs.push(Msg::User(nudge));
        }
        // One step before the budget runs out, tell the model to wrap up so the
        // final iteration lands a real result instead of being cut mid-thought.
        if step + 1 == MAX_ITERS {
            msgs.push(Msg::User(
                "You're almost out of step budget for this turn. Wrap up now: finish the smallest \
                 coherent piece you can, then call finish with a summary of what you did and the \
                 exact next step. Don't start anything large."
                    .into(),
            ));
        }
    }
    // Ran out of step budget without a finish call. This must never surface as a
    // raw \"hit the limit\" error — land the turn gracefully with an honest
    // summary of where things stand, returned as a normal reply.
    if depth == 0 {
        stopped_short(Outcome::StepLimit);
    }
    msgs.push(Msg::User(
        "That's the end of the step budget for this turn — stop calling tools now. In a few plain \
         sentences (no tool calls), tell me what you got done, what's still left, and the exact next \
         step to pick up. Be honest about anything unverified."
            .into(),
    ));
    if budget_exhausted() {
        return Ok(String::new());
    }
    hooks::notify("PrePrompt", cwd);
    match request_reply(p, msgs.as_slice(), &defs, "wrapping up") {
        Ok(r) if !r.text.trim().is_empty() => Ok(r.text),
        // Even if the wrap-up call fails or comes back empty, don't hand the user
        // a scary error for simply doing a lot of work — end the turn cleanly.
        _ => Ok(String::new()),
    }
}

// Whether a finish call should trigger an automatic check_work: top-level
// turns only, only when a file was actually written or edited, only when the
// model skipped check_work itself, never in read-only mode, and once per turn.
fn should_auto_check_work(
    perm: Permission,
    depth: usize,
    files_changed: usize,
    check_work_called: bool,
    auto_rounds: usize,
) -> bool {
    depth == 0
        && files_changed > 0
        && !check_work_called
        && !matches!(perm, Permission::ReadOnly)
        && auto_rounds == 0
}

// check_work had nothing to run (no recognised project, or every checker is
// missing) — nothing to enforce, and no verdict to record.
fn check_work_found_no_project(report: &str) -> bool {
    report.contains("no build/test/lint commands detected") || report.contains("nothing ran")
}

// Feedback for a tool call whose arguments failed to parse: name the tool,
// show the parse error and the schema's required params, and demand a re-send.
// A call to a tool this turn does not offer, refused before the gate so no
// approval is asked and no change previewed for it: what the screen shows
// and what the model is told, which names only the tools offered here.
// `strict` (the read-only and conversational loops) refuses every name not
// offered; BUILD still runs an alias or a catalogue tool a small model
// insists on (see tools::compact_tool) and refuses only unknown names.
fn unoffered_call(name: &str, defs: &[tools::ToolDef], strict: bool) -> Option<(String, String)> {
    // finish only ends the turn, so a model may always call it.
    let canonical = tools::canonical_name(name);
    if name == "finish"
        || defs
            .iter()
            .any(|d| d.name == name || tools::canonical_name(d.name) == canonical)
    {
        return None;
    }
    let known = crate::mcp::is_mcp_tool(name) || tools::defs(true).iter().any(|d| d.name == name);
    if known && !strict {
        return None;
    }
    let mut here: Vec<&str> = Vec::new();
    for d in defs {
        let c = tools::canonical_name(d.name);
        if !here.contains(&c) {
            here.push(c);
        }
    }
    let list = here.join(", ");
    if known {
        // Left out by the compact set for a small window, not by the mode:
        // say which setting brings it back.
        let compact: Vec<&str> = tools::defs_for_context(true, 1)
            .iter()
            .map(|d| d.name)
            .collect();
        let why = if !compact.contains(&name) && defs.iter().all(|d| compact.contains(&d.name)) {
            " (the compact tool set for a window under 32,768 tokens leaves it out; \
             set \"context_tokens\" in settings.json to the model's real window to offer it)"
        } else {
            ""
        };
        return Some((
            format!("{name} is not offered here{why}"),
            format!("{name} is not offered here{why}. Tools here: {list}"),
        ));
    }
    let lower = name.to_ascii_lowercase();
    let nearest = here
        .iter()
        .min_by_key(|n| tools::levenshtein(&lower, &n.to_ascii_lowercase()))
        .map(|n| format!(" Did you mean `{n}`?"))
        .unwrap_or_default();
    Some((
        format!("{name} is not a tool here"),
        format!("unknown tool: {name}.{nearest} Tools here: {list}"),
    ))
}

// screenshot_url for a model that does not take images: refused before
// anyone is asked or a browser starts. It can be offered before the server
// says so, which it may do only once the turn's first request is answered.
fn text_only_refusal(p: &Provider, name: &str) -> Option<(String, String)> {
    if name != "screenshot_url" || crate::media::model_supports_vision(p) {
        return None;
    }
    let why = crate::media::vision_refusal(p);
    let why = why.trim_end_matches(" — image not attached");
    Some((
        format!("screenshot_url not run: {why}"),
        format!("screenshot_url was not run: {why}, so a screenshot would not reach it."),
    ))
}

// In a read-only mode, a change it did not offer is refused with that
// mode's own reason, which says how to get out of it.
fn read_only_refusal(
    name: &str,
    input: &serde_json::Value,
    mode_reason: Option<&str>,
    refusal: (String, String),
) -> (String, String) {
    match mode_reason {
        Some(r) if tools::is_mutating_call(name, input) => (r.to_string(), r.to_string()),
        _ => refusal,
    }
}

// The tools of a read-only BUILD turn. In read-only mode it may also start
// helpers where BUILD offers them: each one is read-only too.
fn readonly_build_defs(perm: Permission, depth: usize, context: usize) -> Vec<tools::ToolDef> {
    let mut defs = tools::defs_readonly();
    if matches!(perm, Permission::ReadOnly) {
        defs.extend(
            tools::defs_for_context(depth + 1 < MAX_DEPTH, context)
                .into_iter()
                .filter(|d| d.name == "spawn_subagent"),
        );
    }
    defs
}

fn invalid_args_feedback(name: &str, raw: &str, defs: &[tools::ToolDef]) -> String {
    let parse_err = match serde_json::from_str::<serde_json::Value>(raw) {
        Err(e) => e.to_string(),
        Ok(_) => "arguments did not match the expected object shape".to_string(),
    };
    let required = defs
        .iter()
        .find(|d| d.name == name)
        .and_then(|d| d.schema.get("required"))
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(none)".to_string());
    format!(
        "Tool `{name}` arguments were not valid JSON ({parse_err}): {}. \
         Required params: {required}. Re-send the complete corrected call.",
        raw.chars().take(200).collect::<String>()
    )
}

// The tools a turn offers this model: a screenshot is no use to one that
// does not take images.
fn offered(p: &Provider, defs: Vec<tools::ToolDef>) -> Vec<tools::ToolDef> {
    if crate::media::model_supports_vision(p) {
        return defs;
    }
    defs.into_iter()
        .filter(|d| d.name != "screenshot_url")
        .collect()
}

// Images a tool returned reach the model only when it takes them; for one
// that does not, the result says why there are none.
fn vision_checked(p: &Provider, mut out: tools::Outcome) -> tools::Outcome {
    if !out.images.is_empty() && !crate::media::model_supports_vision(p) {
        out.images.clear();
        out.content
            .push_str(&format!("\n[{}]", crate::media::vision_refusal(p)));
    }
    out
}

// A call that never ran: a tool this turn does not offer, or one missing an
// argument its schema requires.
fn call_could_not_run(name: &str, input: &serde_json::Value, defs: &[tools::ToolDef]) -> bool {
    let Some(def) = defs.iter().find(|d| d.name == name) else {
        return true;
    };
    def.schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|k| k.as_str())
        .any(|k| {
            input
                .get(k)
                .is_none_or(|v| v.is_null() || v.as_str().is_some_and(|s| s.trim().is_empty()))
        })
}

// A reply that is nothing but a tool call in JSON (`{"name": …, "arguments":
// …}`, bare or alone in a ```json fence) to a tool this turn offers, which
// was not recognised, so nothing ran. JSON that opens an explanation, or
// names a function the run does not offer, is an answer showing a call.
fn unparsed_tool_call(text: &str, defs: &[tools::ToolDef]) -> Option<String> {
    let t = text.trim();
    let t = match t.strip_prefix("```") {
        Some(rest) => {
            let (lang, body) = rest.split_once('\n')?;
            let lang = lang.trim();
            if !(lang.is_empty() || lang.eq_ignore_ascii_case("json")) {
                return None;
            }
            body.trim_end().strip_suffix("```")?.trim()
        }
        None => t,
    };
    let v: serde_json::Value = serde_json::from_str(t).ok()?;
    // One call, a list of them (`[…]` or `{"tool_calls": […]}`), or one
    // under `function_call`: the first offered name stands for the reply.
    let calls: Vec<&serde_json::Value> = match (v.as_array(), v.get("tool_calls")) {
        (Some(list), _) => list.iter().collect(),
        (None, Some(list)) => list.as_array()?.iter().collect(),
        (None, None) => vec![v.get("function_call").unwrap_or(&v)],
    };
    let mut first = None;
    for c in &calls {
        let call = c.get("function").unwrap_or(c);
        let name = call.get("name")?.as_str()?;
        let has_args = ["arguments", "parameters", "args", "input"]
            .iter()
            .any(|k| call.get(k).is_some());
        if !(defs.iter().any(|d| d.name == name) && has_args) {
            return None;
        }
        first.get_or_insert(name);
    }
    first.map(str::to_string)
}

// Why a turn that ends on text is not a success: the last tool round held a
// call that never ran, or the text itself is a call nothing ran.
fn unrun_call_note(
    last_unrun: Option<&str>,
    text: &str,
    defs: &[tools::ToolDef],
) -> Option<String> {
    if let Some(call) = last_unrun {
        return Some(format!(
            "  ⚠ the model stopped after a call that could not run: {call}"
        ));
    }
    unparsed_tool_call(text, defs)
        .map(|name| format!("  ⚠ the model answered with a {name} call that bwn could not run"))
}

#[cfg(test)]
mod unrun_call_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn calls_that_never_ran_are_told_apart_from_failing_ones() {
        let defs = tools::defs_for_context(true, 128_000);
        assert!(call_could_not_run("open_document", &json!({}), &defs));
        assert!(call_could_not_run(
            "read_file",
            &json!({"path": " "}),
            &defs
        ));
        assert!(call_could_not_run(
            "read_file",
            &json!({"parameters": {}}),
            &defs
        ));
        // Ran and failed (a missing file) is the model's to handle.
        assert!(!call_could_not_run(
            "read_file",
            &json!({"path": "nope.txt"}),
            &defs
        ));
    }

    #[test]
    fn a_text_reply_that_is_a_call_is_recognised() {
        let defs = tools::defs_for_context(true, 128_000);
        let call = r#"{"name": "read_file", "arguments": {"path": "a.txt"}}"#;
        assert_eq!(
            unparsed_tool_call(call, &defs).as_deref(),
            Some("read_file")
        );
        let fenced = format!("```json\n{call}\n```");
        assert_eq!(
            unparsed_tool_call(&fenced, &defs).as_deref(),
            Some("read_file")
        );
        let wrapped = r#"{"type":"function","function":{"name":"grep_files","parameters":{}}}"#;
        assert_eq!(
            unparsed_tool_call(wrapped, &defs).as_deref(),
            Some("grep_files")
        );
        // Several calls, or one under another key, are still calls.
        for text in [
            format!("[{call}, {call}]"),
            format!("```json\n[{call}]\n```"),
            format!(r#"{{"tool_calls": [{call}]}}"#),
            r#"{"function_call": {"name": "read_file", "arguments": "{}"}}"#.to_string(),
            r#"{"name": "read_file", "args": {"path": "a.txt"}}"#.to_string(),
        ] {
            assert_eq!(
                unparsed_tool_call(&text, &defs).as_deref(),
                Some("read_file"),
                "{text}"
            );
        }
        assert_eq!(unparsed_tool_call("The file says hello.", &defs), None);
        assert!(
            unrun_call_note(Some("read_file (path argument is required)"), "ok", &defs)
                .unwrap()
                .contains("read_file")
        );
    }

    #[test]
    fn an_answer_that_shows_a_call_is_not_an_unparsed_call() {
        let defs = tools::defs_for_context(true, 128_000);
        for text in [
            // A name this run does not offer: an example, however bare.
            r#"{"name": "get_weather", "arguments": {"city": "Paris"}}"#,
            r#"{"name": "getWeather", "arguments": {"city": "Paris"}}"#,
            // An offered name with an explanation after it is an answer too.
            "{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.txt\"}}\n\nThat is a call.",
            r#"{"type":"function","function":{"name":"grep_files","parameters":{}}} then prose"#,
            r#"{"name": "Ada"} is the author"#,
            // A list holding one name this run does not offer, or none.
            r#"[{"name": "read_file", "arguments": {}}, {"name": "get_weather", "arguments": {}}]"#,
            "[]",
            r#"["read_file"]"#,
            r#"{"tool_calls": "read_file"}"#,
        ] {
            assert_eq!(unparsed_tool_call(text, &defs), None, "{text}");
            assert_eq!(unrun_call_note(None, text, &defs), None, "{text}");
        }
    }
}

mod review;
use review::*;
pub use review::{
    default_parallel_helpers, max_parallel_helpers, max_parallel_helpers_note, run_review,
    set_max_parallel_helpers, DEFAULT_MAX_PARALLEL_HELPERS,
};

mod plan;
pub use plan::{plan_turn, run_plan, PlanEnd};

// ── One conversation in every mode ─────────────────────────────────────────────
// BRAINSTORM, PLAN, BUILD and conversational turns all read and extend the
// same transcript, and every turn is saved to the session. Each mode keeps
// its own system prompt: a turn puts its mode's prompt at the head of the
// conversation (see open_turn and use_system) and leaves the messages after
// it as they are.
const BRAINSTORM_HEAD: &str = "You are a sharp, concise thought partner";
const PLAN_HEAD: &str = "You are bwn in PLAN mode";
const CHAT_HEAD: &str = "You are buildwithnexus in a coding terminal";

// This mode's system prompt at the head of a shared conversation; a prompt
// from another mode is replaced, the messages after it are kept.
fn use_system(msgs: &mut Vec<Msg>, head: &str, system: impl FnOnce() -> String) {
    match msgs.first() {
        Some(Msg::System(s)) if s.starts_with(head) => {}
        Some(Msg::System(_)) => msgs[0] = Msg::System(system()),
        _ => msgs.insert(0, Msg::System(system())),
    }
}

// An interrupted reply ends the turn quietly, like an interrupted build
// turn; the question stays in the conversation.
fn interrupted_turn(e: &str) -> bool {
    if !e.contains("interrupted") {
        return false;
    }
    report::notice(take_interrupt(0));
    true
}

// Takes the user's interrupt (Esc, Ctrl+C) and says so; a Stop hook will
// not send this turn on. The top-level turn clears it; a helper only looks,
// so the stop also reaches the helpers running beside it and the turn that
// started them.
fn take_interrupt(depth: usize) -> &'static str {
    STOPPED_EARLY.with(|u| u.set(true));
    let kind = if depth == 0 {
        tui::consume_interrupt()
    } else {
        tui::get_interrupt_kind()
    };
    match kind {
        tui::InterruptKind::CtrlC => "  ⚠ interrupted (queue cleared)",
        _ => "  ⚠ interrupted",
    }
}

mod brainstorm;
pub use brainstorm::{ask_aside, brainstorm_turn, run_brainstorm, run_chat_turn, ChatIn};

// ── /commit ──────────────────────────────────────────────────────────────────
// The model only drafts the message. The request carries no tools, so it
// cannot run `git commit` (or anything else); bwn shows the draft and commits
// after the person says so.
const MAX_COMMIT_DIFF_CHARS: usize = 40_000;

fn commit_draft_msgs(stat: &str, diff: &str) -> Vec<Msg> {
    let cut = crate::config::cut_at_char_boundary(diff, MAX_COMMIT_DIFF_CHARS);
    let more = if cut.len() < diff.len() {
        "\n[diff truncated]"
    } else {
        ""
    };
    vec![
        Msg::System(
            "You write git commit messages. Reply with the commit message only: a \
             conventional-commit subject line (`type: summary`, under 72 characters), then \
             optionally a blank line and a short body. No code fences, quotes or commentary."
                .into(),
        ),
        Msg::User(format!(
            "Write a commit message for these staged changes.\n\n\
             [git diff --staged --stat]\n{stat}\n\n[git diff --staged]\n{cut}{more}"
        )),
    ]
}

/// Drafts a commit message for the staged changes. Nothing is committed.
pub fn draft_commit_message(p: &Provider, stat: &str, diff: &str) -> Result<String, String> {
    let msgs = commit_draft_msgs(stat, diff);
    let reply = tui::with_spinner("drafting commit message", || complete(p, &msgs, &[]))?;
    let text = clean_commit_message(&reply.text);
    if text.is_empty() {
        return Err("the model returned an empty commit message".into());
    }
    Ok(text)
}

// Models wrap messages in fences or quotes despite being asked not to.
fn clean_commit_message(text: &str) -> String {
    let t = text.trim();
    let t = match t.strip_prefix("```") {
        Some(rest) => {
            let rest = rest.split_once('\n').map(|(_, body)| body).unwrap_or("");
            rest.trim_end().trim_end_matches("```")
        }
        None => t,
    };
    t.trim().trim_matches(['"', '`']).trim().to_string()
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeHint {
    Build,
    Plan,
}

#[cfg(test)]
mod tests;
