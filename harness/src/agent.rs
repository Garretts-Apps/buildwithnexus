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

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 character —
/// byte-offset slicing (`&s[..max]`) panics on emoji/CJK at the boundary.
fn truncate_at_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

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
    let mut tail_start = msgs.len().saturating_sub(KEEP_RECENT).max(sys_end);
    // Never sever a tool_use/tool_result pair: a tail that starts with
    // Msg::Tool has orphaned results (API 400). Advance until the tail begins
    // at a safe boundary — a user turn or an assistant turn (an assistant turn
    // with calls keeps its Msg::Tool inside the tail).
    while tail_start < msgs.len()
        && !matches!(
            msgs[tail_start],
            Msg::User(_) | Msg::UserImages { .. } | Msg::Assistant { .. }
        )
    {
        tail_start += 1;
    }
    (sys_end, tail_start)
}

// The first real user message is the task; a prior compaction pins it after an
// "[Original task]" marker, so re-compactions keep the verbatim text.
const ORIGINAL_TASK_MARKER: &str = "[Original task]\n";

fn original_task_text(msgs: &[Msg]) -> Option<String> {
    for m in msgs {
        if let Msg::User(t) | Msg::UserImages { text: t, .. } = m {
            if let Some(idx) = t.find(ORIGINAL_TASK_MARKER) {
                return Some(t[idx + ORIGINAL_TASK_MARKER.len()..].to_string());
            }
            return Some(t.clone());
        }
    }
    None
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
    if tail_start <= sys_end {
        return msgs;
    }
    let task = original_task_text(&msgs);
    let mut it = msgs.into_iter();
    let system: Vec<Msg> = it.by_ref().take(sys_end).collect();
    // Clip bloated tool results only in the middle being summarized — the
    // recent tail keeps its full content so the model retains recent context.
    let middle: Vec<Msg> = truncate_tool_results(it.by_ref().take(tail_start - sys_end).collect());
    let tail: Vec<Msg> = it.collect();
    let summary = summarize(&middle);
    let mut v = system;
    let mut body =
        format!("[Summary of earlier conversation, compacted to save context]\n{summary}");
    // Pin the original task verbatim so it survives any number of compactions.
    if let Some(task) = task {
        body.push_str("\n\n");
        body.push_str(ORIGINAL_TASK_MARKER);
        body.push_str(&task);
    }
    v.push(Msg::User(body));
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
                            r.content = format!(
                                "{truncated}\n…(truncated, {} chars total)",
                                r.content.len()
                            );
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
    report::info("  ⟳ compacting context…");
    let taken = std::mem::take(msgs);
    *msgs = compact_with(taken, |middle| model_summary(p, middle));
}

#[derive(Default)]
struct ToolLoopGuard {
    seen: HashMap<String, ToolLoopRecord>,
}

struct ToolLoopRecord {
    content: String,
    count: usize,
    nudged: bool,
}

// What the guard wants the caller to do about a detected loop.
enum LoopSignal {
    /// First trigger: steer the model with a corrective user turn and continue.
    Nudge(String),
    /// The loop repeated even after a nudge: stop the run and say so honestly.
    Stop(String),
}

impl ToolLoopGuard {
    fn note(
        &mut self,
        name: &str,
        input: &serde_json::Value,
        content: &str,
        is_error: bool,
    ) -> Option<LoopSignal> {
        let key = format!(
            "{name}:{}",
            serde_json::to_string(input).unwrap_or_default()
        );
        // Identical successful, non-empty results are legitimate (e.g. re-reading
        // a file after edits elsewhere) — only repeated errors and repeated
        // no-op/empty results count as loop evidence.
        if !is_error && !content.trim().is_empty() {
            self.seen.remove(&key);
            return None;
        }
        let rec = self.seen.entry(key).or_insert_with(|| ToolLoopRecord {
            content: String::new(),
            count: 0,
            nudged: false,
        });
        if rec.content == content {
            rec.count += 1;
        } else {
            rec.content = content.to_string();
            rec.count = 1;
            rec.nudged = false;
        }
        if rec.count < MAX_IDENTICAL_TOOL_RESULTS {
            return None;
        }
        if !rec.nudged {
            rec.nudged = true;
            let preview = tools::preview(name, input);
            return Some(LoopSignal::Nudge(format!(
                "You already ran `{preview}` {} times and got the same result — \
                 take a different action instead of repeating the same call.",
                rec.count
            )));
        }
        Some(LoopSignal::Stop(repeated_tool_summary(
            name, input, content, is_error,
        )))
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
    nudge: &mut Option<String>,
    stop: &mut Option<String>,
) {
    if stop.is_some() {
        return;
    }
    match guard.note(name, input, content, is_error) {
        Some(LoopSignal::Nudge(msg)) => {
            report::notice("  ⟳ repeated tool result — nudging the model to change course");
            *nudge = Some(msg);
        }
        Some(LoopSignal::Stop(msg)) => {
            report::assistant(&msg);
            *stop = Some(msg);
        }
        None => {}
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
        "  ⟳ recovery: using current workspace for placeholder/root path in {name}"
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
    let path_broken = path.trim().is_empty()
        || matches!(path.trim(), "~" | "." | "/" | "./")
        || is_placeholder_path(path);
    // NEVER override non-empty plausible model content — only repair when the
    // model wrote nothing usable (empty or an obvious placeholder).
    if is_placeholder_content(content) {
        return Some(requested);
    }
    if path_broken {
        // The model's content is fine; only the destination path is broken.
        let mut repaired = input.clone();
        let key = if input.get("path").is_none() && input.get("filePath").is_some() {
            "filePath"
        } else {
            "path"
        };
        repaired[key] = serde_json::Value::String(requested_path.to_string());
        return Some(repaired);
    }
    None
}

/// True when write_file content is empty or an obvious stand-in the model left
/// for "fill this in later" — real content, however different from the task
/// text, must never be overwritten.
fn is_placeholder_content(content: &str) -> bool {
    let t = content.trim();
    if t.is_empty() {
        return true;
    }
    matches!(t, "..." | "…" | "TODO" | "todo" | "TBD" | "tbd")
        || matches!(t, "<content>" | "{content}" | "(content)" | "<contents>")
        || t.eq_ignore_ascii_case("your content here")
        || t.eq_ignore_ascii_case("placeholder")
        || t.eq_ignore_ascii_case("file content goes here")
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
    // Quiet chrome: this fires before every model call (up to ~30 per turn),
    // so keep it one short dim line instead of re-announcing model and tool
    // count in bright colors each step.
    tui::line(&tui::dim(&format!("  · {label}")));
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

// Normalized stop reason from the provider: "max_tokens" means the output was
// cut off mid-generation, so both the text and any tool call may be incomplete.
fn reply_truncated_at_token_limit(reply: &Reply) -> bool {
    reply.stop_reason.as_deref() == Some("max_tokens")
}

static TEXT_TOOL_SEQ: AtomicUsize = AtomicUsize::new(0);

fn normalize_text_tool_calls(mut reply: Reply, defs: &[tools::ToolDef], user_text: &str) -> Reply {
    if !reply.calls.is_empty() {
        return reply;
    }
    let Some(calls) = parse_text_tool_calls(&reply.text, defs) else {
        return reply;
    };
    if is_casual_turn(user_text)
        && calls
            .iter()
            .any(|call| tools::is_mutating_call(&call.name, &call.input))
    {
        if !report::is_json() {
            report::info("  ⟳ recovery: ignored unrelated tool JSON for casual input");
        }
        trace::record_visible(
            "tool_input_repaired",
            "ignored casual-turn tool JSON",
            serde_json::json!({"calls": calls.iter().map(|c| &c.name).collect::<Vec<_>>()}),
        );
        reply.text = "Hi. I am here and ready. Tell me what you want to build, inspect, or change."
            .to_string();
        reply.calls.clear();
        if !report::is_json() {
            report::assistant(&reply.text);
        }
        return reply;
    }
    if !report::is_json() {
        report::notice(&format!(
            "  ⟳ recovery: parsed {} tool call{} from model JSON",
            calls.len(),
            if calls.len() == 1 { "" } else { "s" }
        ));
    }
    trace::record_visible(
        "tool_input_repaired",
        "parsed text JSON tool call",
        serde_json::json!({"calls": calls.iter().map(|c| &c.name).collect::<Vec<_>>()}),
    );
    reply.text.clear();
    reply.calls = calls;
    reply
}

fn is_casual_turn(text: &str) -> bool {
    let normalized = text
        .trim()
        .trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace())
        .to_ascii_lowercase();
    // Only pure greetings: acknowledgements like "ok"/"thanks" are often valid
    // "yes, proceed" turns and must not have their tool calls discarded.
    matches!(normalized.as_str(), "hi" | "hello" | "hey")
}

fn parse_text_tool_calls(text: &str, defs: &[tools::ToolDef]) -> Option<Vec<provider::ToolCall>> {
    let names = defs.iter().map(|d| d.name).collect::<HashSet<_>>();
    if let Some(candidate) = extract_json_tool_candidate(text) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) {
            let mut calls = Vec::new();
            collect_text_tool_calls(&value, &names, &mut calls);
            if !calls.is_empty() {
                return Some(calls);
            }
        }
    }
    // Fall back to Gemma's `tool_code` Python-call format.
    parse_tool_code_call(text, &names)
}

// Positional argument names for the tools small models most often call, so a
// Gemma call like `write_file("/p", """…""")` maps arg 0→path, arg 1→content.
fn positional_params(name: &str) -> &'static [&'static str] {
    match name {
        "write_file" | "write" => &["path", "content"],
        "read_file" | "read" => &["path"],
        "edit_file" | "edit" => &["path", "old", "new"],
        "run_command" | "bash" => &["command"],
        "Artifact" | "publish_artifact" => &["contents"],
        "list_dir" | "list" => &["path"],
        "find_files" | "glob" => &["pattern"],
        "grep_files" | "grep" => &["pattern"],
        "finish" => &["summary"],
        "python_tool" => &["code"],
        "create_docx" => &["path", "title", "body"],
        _ => &[],
    }
}

// Gemma emits tool calls as a ```tool_code fenced Python function call, e.g.
//   write_file("/p/index.html", """<!doctype html>…""")
//   Artifact(contents="""…""")
// sometimes wrapped in `print(...)`. Native tool_calls / JSON never match, so
// the call is otherwise treated as prose. Parse the first known-tool call.
fn parse_tool_code_call(
    text: &str,
    allowed: &HashSet<&'static str>,
) -> Option<Vec<provider::ToolCall>> {
    let scope = tool_code_scope(text);
    let (name, after) = find_tool_call(scope, allowed)?;
    let inside = balanced_parens(after)?;
    let args = parse_python_args(inside);
    let input = tool_code_args_to_input(name, args);
    Some(vec![text_tool_call(name, input)])
}

// The region to scan: inside a ```tool_code fence when present, else the whole
// text (Gemma sometimes omits the fence).
fn tool_code_scope(text: &str) -> &str {
    if let Some(pos) = text.find("```tool_code") {
        let after = &text[pos + "```tool_code".len()..];
        let end = after.find("```").unwrap_or(after.len());
        &after[..end]
    } else {
        text
    }
}

// Find the earliest occurrence of a known tool name immediately followed by
// `(` (skipping any `print(` wrapper, which isn't a known tool). Returns the
// name and the slice just after the opening paren.
fn find_tool_call<'a>(
    scope: &'a str,
    allowed: &HashSet<&'static str>,
) -> Option<(&'static str, &'a str)> {
    let bytes = scope.as_bytes();
    let mut best: Option<(usize, &'static str, usize)> = None;
    for &name in allowed.iter() {
        let mut from = 0;
        while let Some(rel) = scope[from..].find(name) {
            let at = from + rel;
            // Must be a whole identifier (not a substring of a longer name).
            let before_ok = at == 0 || !is_ident_byte(bytes[at - 1]);
            let after_idx = at + name.len();
            let mut j = after_idx;
            while j < bytes.len() && bytes[j] == b' ' {
                j += 1;
            }
            if before_ok && j < bytes.len() && bytes[j] == b'(' {
                if best.map(|(p, _, _)| at < p).unwrap_or(true) {
                    best = Some((at, name, j + 1));
                }
                break;
            }
            from = at + name.len();
        }
    }
    best.map(|(_, name, paren)| (name, &scope[paren..]))
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

// Given the slice just after an opening `(`, return the argument text up to the
// matching `)`, tracking string literals (triple/single/double) and nesting.
fn balanced_parens(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let mut depth = 1i32;
    let mut i = 0;
    while i < bytes.len() {
        // Skip string literals whole so commas/parens inside don't count.
        if let Some(consumed) = string_literal_len(&s[i..]) {
            i += consumed;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[..i]);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// If `s` starts with a Python string literal, return its byte length (including
// delimiters). Handles triple `"""`/`'''` (raw) and single `"`/`'` (with `\`
// escapes). None if `s` doesn't start with a quote.
fn string_literal_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    for q in ["\"\"\"", "'''"] {
        if let Some(rest) = s.strip_prefix(q) {
            let close = rest.find(q)?;
            return Some(q.len() + close + q.len());
        }
    }
    let quote = *b.first()?;
    if quote == b'"' || quote == b'\'' {
        let mut i = 1;
        while i < b.len() {
            if b[i] == b'\\' {
                i += 2;
                continue;
            }
            if b[i] == quote {
                return Some(i + 1);
            }
            i += 1;
        }
    }
    None
}

// Split a Python argument list at top-level commas, then classify each as
// keyword (`name=value`) or positional, with the value parsed to a JSON value.
fn parse_python_args(inside: &str) -> Vec<(Option<String>, serde_json::Value)> {
    let bytes = inside.as_bytes();
    let mut parts: Vec<&str> = Vec::new();
    let (mut start, mut i) = (0usize, 0usize);
    while i < bytes.len() {
        if let Some(consumed) = string_literal_len(&inside[i..]) {
            i += consumed;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => {
                if let Some(inner) = balanced_parens(&inside[i + 1..]) {
                    i += 1 + inner.len();
                }
            }
            b',' => {
                parts.push(&inside[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < inside.len() {
        parts.push(&inside[start..]);
    }
    parts
        .into_iter()
        .filter_map(|p| {
            let p = p.trim();
            if p.is_empty() {
                return None;
            }
            // Keyword only if the `=` precedes any string literal.
            if let Some(eq) = kw_eq_pos(p) {
                let key = p[..eq].trim();
                if !key.is_empty() && key.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    return Some((Some(key.to_string()), parse_py_value(p[eq + 1..].trim())));
                }
            }
            Some((None, parse_py_value(p)))
        })
        .collect()
}

// Position of a top-level `=` (keyword separator) before any string literal,
// or None. `==`/`!=`/`>=`/`<=` are not keyword separators.
fn kw_eq_pos(p: &str) -> Option<usize> {
    let b = p.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if string_literal_len(&p[i..]).is_some() {
            return None; // hit a string before any '='
        }
        if b[i] == b'=' {
            let prev = if i > 0 { b[i - 1] } else { b' ' };
            let next = b.get(i + 1).copied().unwrap_or(b' ');
            if prev != b'!' && prev != b'<' && prev != b'>' && next != b'=' {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

// Parse a Python literal value into JSON: triple/single/double-quoted strings,
// booleans, and numbers; anything else becomes a raw string.
fn parse_py_value(expr: &str) -> serde_json::Value {
    let e = expr.trim();
    for q in ["\"\"\"", "'''"] {
        if let Some(rest) = e.strip_prefix(q) {
            if let Some(end) = rest.rfind(q) {
                return serde_json::Value::String(rest[..end].to_string());
            }
        }
    }
    if (e.starts_with('"') && e.ends_with('"') && e.len() >= 2)
        || (e.starts_with('\'') && e.ends_with('\'') && e.len() >= 2)
    {
        let inner = &e[1..e.len() - 1];
        // Unescape the common sequences for single-quoted strings.
        let unescaped = inner
            .replace("\\n", "\n")
            .replace("\\t", "\t")
            .replace("\\\"", "\"")
            .replace("\\'", "'")
            .replace("\\\\", "\\");
        return serde_json::Value::String(unescaped);
    }
    match e {
        "True" | "true" => return serde_json::Value::Bool(true),
        "False" | "false" => return serde_json::Value::Bool(false),
        _ => {}
    }
    if let Ok(n) = e.parse::<i64>() {
        return serde_json::Value::from(n);
    }
    if let Ok(f) = e.parse::<f64>() {
        return serde_json::Value::from(f);
    }
    serde_json::Value::String(e.to_string())
}

// Build the tool input object: keyword args by name, positional args mapped
// through the tool's positional signature.
fn tool_code_args_to_input(
    name: &str,
    args: Vec<(Option<String>, serde_json::Value)>,
) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    let params = positional_params(name);
    let mut pos = 0;
    for (key, val) in args {
        match key {
            Some(k) => {
                map.insert(k, val);
            }
            None => {
                if let Some(pname) = params.get(pos) {
                    map.insert((*pname).to_string(), val);
                }
                pos += 1;
            }
        }
    }
    serde_json::Value::Object(map)
}

fn extract_json_tool_candidate(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    // Whole-text JSON (the strict, original case).
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Some(trimmed);
    }
    // A fenced ```json block that is the entire message.
    if let Some(rest) = trimmed.strip_prefix("```") {
        if let Some(fence_end) = rest.find('\n') {
            let lang = rest[..fence_end].trim().to_ascii_lowercase();
            if lang.is_empty() || lang == "json" {
                let body = &rest[fence_end + 1..];
                if let Some(close) = body.rfind("```") {
                    if body[close + 3..].trim().is_empty() {
                        return Some(body[..close].trim());
                    }
                }
            }
        }
    }
    // XML-tagged tool calls. Small local coders (notably Qwen2.5-Coder on
    // llama.cpp/Ollama) emit calls as `<tools>{…}</tools>` or
    // `<tool_call>{…}</tool_call>` text in `content` instead of native
    // tool_calls. Pull the first balanced JSON object out of the tagged region.
    for tag in ["<tools>", "<tool_call>", "<function_call>", "<function>"] {
        if let Some(pos) = text.find(tag) {
            if let Some(json) = balanced_json_object(&text[pos + tag.len()..]) {
                return Some(json);
            }
        }
    }
    // A bare object embedded in prose (a leading sentence, then the JSON).
    balanced_json_object(text)
}

// Return the first balanced `{…}` slice, tracking string state so braces inside
// JSON string values (e.g. CSS `{ }` inside an HTML `content` field) don't end
// the object early. None if there's no `{` or it never closes (truncated output).
fn balanced_json_object(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        let c = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn collect_text_tool_calls(
    value: &serde_json::Value,
    allowed: &HashSet<&'static str>,
    out: &mut Vec<provider::ToolCall>,
) {
    if let Some(items) = value.as_array() {
        for item in items {
            collect_text_tool_calls(item, allowed, out);
        }
        return;
    }
    let Some(obj) = value.as_object() else {
        return;
    };
    if let Some(items) = obj.get("tool_calls").and_then(|v| v.as_array()) {
        for item in items {
            collect_text_tool_calls(item, allowed, out);
        }
        return;
    }
    if let Some(items) = obj.get("calls").and_then(|v| v.as_array()) {
        for item in items {
            collect_text_tool_calls(item, allowed, out);
        }
        return;
    }
    if let Some(function) = obj.get("function").and_then(|v| v.as_object()) {
        let Some(name) = function.get("name").and_then(|v| v.as_str()) else {
            return;
        };
        if !allowed.contains(name) {
            return;
        }
        let input = function
            .get("arguments")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        out.push(text_tool_call(name, input));
        return;
    }
    let Some(name) = obj
        .get("name")
        .or_else(|| obj.get("tool_name"))
        .and_then(|v| v.as_str())
    else {
        return;
    };
    if !allowed.contains(name) {
        return;
    }
    let input = obj
        .get("arguments")
        .or_else(|| obj.get("input"))
        .cloned()
        .unwrap_or_else(|| {
            let mut map = serde_json::Map::new();
            for (k, v) in obj {
                if k != "name" && k != "tool_name" && k != "type" && k != "id" {
                    map.insert(k.clone(), v.clone());
                }
            }
            serde_json::Value::Object(map)
        });
    out.push(text_tool_call(name, input));
}

fn text_tool_call(name: &str, input: serde_json::Value) -> provider::ToolCall {
    let id = TEXT_TOOL_SEQ.fetch_add(1, Ordering::Relaxed);
    provider::ToolCall {
        id: format!("text-tool-{id}"),
        name: name.to_string(),
        input,
    }
}

// The single-line composer prompt for reading a question answer. Kept on one
// line (no `\n`) so the alt-screen composer positions the cursor correctly and
// echoes typed input; the question text itself is printed separately above.
fn answer_input_prompt(default: &str) -> String {
    if default.is_empty() {
        "  Answer: ".to_string()
    } else {
        format!("  Answer {}: ", tui::dim(&format!("[{default}]")))
    }
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
    // Render the question on its own transcript line, then read the answer with
    // a SINGLE-LINE composer prompt. A multi-line prompt string mis-positions
    // the alt-screen composer cursor (prompt_width counts across the newline),
    // which hides what the user types.
    tui::line(&format!(
        "  {} {}",
        tui::yellow("?"),
        tui::bold(&full_prompt)
    ));
    let ans = tui::ask(&answer_input_prompt(default)).unwrap_or_default();
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
When asked to build or create something, call tools to create the files on disk (write_file, edit_file, run_command) instead of pasting instructions or code blocks in chat — you are the builder. \
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
            format!("{}…", truncate_at_char_boundary(&mem, 300))
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
            format!("{}…", truncate_at_char_boundary(&agents, 500))
        } else {
            agents
        };
        parts.push(format!("[Agent knowledge — Agents.md]\n{agents_text}"));
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
    // Action on its own line; the key legend stays short so the prompt never
    // wraps mid-legend on a normal-width terminal.
    tui::line(&format!("  {} {}", tui::yellow("➤"), tui::bold(label)));
    tui::line(&tui::dim(
        "    y yes · n no · s allow this session · a always allow · d <reason> deny",
    ));
    let q = format!("  {} ", tui::yellow("allow?"));
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
    let r = build_inner(p, perm, role_id, task, cwd, 0, transcript, Some(sid)).map(|_| ());
    hooks::notify("SessionEnd", cwd);
    hooks::notify("Stop", cwd);
    crate::session::save(sid, cwd, &p.model, transcript);
    r
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
        // Role identity and the current-mode contract come FIRST; the
        // environment/tool-manifest/skills/memory sections follow.
        let mut sys = String::from(role(role_id).system);
        if let Some(guidance) = artifact_guidance(&task_for_recovery) {
            sys.push_str("\n\n");
            sys.push_str(&guidance);
        }
        sys.push_str("\n\n");
        sys.push_str(&context_prefix(cwd, p.context_tokens));
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
    let mut empty_reply_retried = false;
    let mut token_limit_continuations = 0usize;
    let mut forced_compact_retry = false;
    let mut verifier_fix_rounds = 0usize;
    // Act-don't-explain tracking: whether any call reached dispatch, whether a
    // mutating tool actually executed, and how many nudges were spent.
    let mut act_nudges = 0usize;
    let mut any_tool_ran = false;
    let mut mutating_tool_ran = false;
    // Executed calls and touched files, collected for the verifier pass.
    let mut tool_records: Vec<crate::verifier::ToolCallRecord> = Vec::new();
    let mut changed_files: Vec<String> = Vec::new();

    for step in 1..=MAX_ITERS {
        if tui::interrupted() {
            report::notice("  ⚠ interrupted");
            return Ok(String::new());
        }
        maybe_compact(p, msgs);
        if step > 1 && !report::is_json() {
            tui::line(&tui::dim(&format!("  ↻ step {step}")));
        }
        let reply = match request_reply(p, msgs.as_slice(), &defs, "thinking") {
            Ok(r) => r,
            Err(e) => {
                // A context-overflow rejection is recoverable: force-compact the
                // transcript and retry once instead of dying.
                let lower = e.to_ascii_lowercase();
                if !forced_compact_retry
                    && (lower.contains("prompt is too long") || lower.contains("context length"))
                {
                    forced_compact_retry = true;
                    report::notice("  ⟳ context overflow — force-compacting and retrying…");
                    let taken = std::mem::take(msgs);
                    *msgs = compact_with(taken, |middle| model_summary(p, middle));
                    continue;
                }
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
            && token_limit_continuations < MAX_TOKEN_LIMIT_CONTINUATIONS
        {
            token_limit_continuations += 1;
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
        if reply.calls.is_empty() {
            // One empty response is retried before ending the run; a second
            // empty response ends it.
            if reply.text.trim().is_empty() {
                // No assistant turn is recorded here: an empty assistant
                // message would serialize to an empty content array (API 400).
                if !empty_reply_retried {
                    empty_reply_retried = true;
                    report::notice("  ⚠ model returned no output — asking it to continue");
                    msgs.push(Msg::User(
                        "You returned no output. Continue the task: call a tool or state your final answer."
                            .to_string(),
                    ));
                    continue;
                }
                report::notice("  ⚠ model returned no output");
                return Ok(reply.text);
            }
            // Imperative task answered with how-to prose instead of tool
            // calls: tell the model to act, bounded per task. Never fires
            // once a mutating tool has run (that prose is a real summary).
            if should_nudge_to_act(
                &task_for_recovery,
                &reply.text,
                any_tool_ran,
                mutating_tool_ran,
                act_nudges,
            ) {
                act_nudges += 1;
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
                continue;
            }
            if let Some(input) = auto_create_file_input(&task_for_recovery) {
                // Record this harness recovery in the transcript so saved and
                // resumed sessions aren't missing turns.
                msgs.push(Msg::Assistant {
                    text: reply.text.clone(),
                    calls: vec![],
                });
                if let Some(reason) = gate(perm, "write_file", &input, cwd) {
                    report::tool_denied(&reason);
                    msgs.push(Msg::User(format!(
                        "[harness] write_file recovery blocked: {reason}"
                    )));
                    return Err(reason);
                }
                report::tool_call("write_file", &tools::preview("write_file", &input), &input);
                trace_tool_call("write_file", &input, "build", depth);
                let out = tools::run("write_file", &input, cwd);
                hooks::post_tool_use("write_file", &input, &out.content, out.is_error, cwd);
                report::tool_result("write_file", &out.content, out.is_error);
                trace_tool_result("write_file", &out.content, out.is_error, "build", depth);
                if out.is_error {
                    msgs.push(Msg::User(format!(
                        "[harness] write_file recovery failed: {}",
                        out.content
                    )));
                    return Err(out.content);
                }
                msgs.push(Msg::User(format!(
                    "[harness] completed the explicit file creation with write_file: {}",
                    out.content
                )));
                return Ok(format!(
                    "{}\n\nCompleted the explicit file creation task after the model returned prose without tool calls.",
                    out.content
                ));
            }
            if let Some(input) = auto_static_artifact_input(&task_for_recovery, &reply.text) {
                msgs.push(Msg::Assistant {
                    text: reply.text.clone(),
                    calls: vec![],
                });
                report::tool_call("Artifact", &tools::preview("Artifact", &input), &input);
                trace_tool_call("Artifact", &input, "build", depth);
                let out = tools::run("Artifact", &input, cwd);
                report::tool_result("Artifact", &out.content, out.is_error);
                trace_tool_result("Artifact", &out.content, out.is_error, "build", depth);
                if out.is_error {
                    // Tell the model exactly why the artifact was rejected and
                    // let it fix and re-publish; fail honestly once bounded
                    // recovery attempts run out.
                    if artifact_error_count < 2 {
                        artifact_error_count += 1;
                        msgs.push(Msg::User(format!(
                            "The HTML artifact you wrote in plain text was rejected: {}. \
                             Fix that problem and re-publish: call Artifact/publish_artifact with one \
                             complete self-contained HTML file. Do not answer with markdown code.",
                            out.content
                        )));
                        continue;
                    }
                    return Err(format!(
                        "artifact publishing failed after {artifact_error_count} recovery attempts; last rejection: {}",
                        out.content
                    ));
                }
                return Ok(out.content);
            }
            if static_artifact_requested(&task_for_recovery) {
                msgs.push(Msg::Assistant {
                    text: reply.text.clone(),
                    calls: vec![],
                });
                if static_artifact_recovery_count < 2 {
                    static_artifact_recovery_count += 1;
                    msgs.push(Msg::User(static_artifact_recovery_prompt(
                        &task_for_recovery,
                    )));
                    continue;
                }
                // No fabricated fallback artifact: report the failure honestly.
                return Err(format!(
                    "the model did not produce a runnable web artifact after \
                     {static_artifact_recovery_count} recovery prompts; its last response was:\n{}",
                    reply.text
                ));
            }
            msgs.push(Msg::Assistant {
                text: reply.text.clone(),
                calls: vec![],
            });
            return Ok(reply.text);
        }

        let mut results = Vec::new();
        let mut summary: Option<String> = None;
        let mut loop_nudge: Option<String> = None;
        let mut loop_stop: Option<String> = None;
        let mut artifact_recovery: Option<String> = None;
        for call in &reply.calls {
            if let Some(raw) = call.input.get(tools::INVALID_ARGS).and_then(|v| v.as_str()) {
                let msg = invalid_args_feedback(&call.name, raw, &defs);
                report::tool_denied(&msg);
                note_loop_result(
                    &mut loop_guard,
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
            any_tool_ran = true;
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
                    &mut loop_nudge,
                    &mut loop_stop,
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
                    &mut loop_nudge,
                    &mut loop_stop,
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
                        &mut loop_nudge,
                        &mut loop_stop,
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
                // A subagent can mutate the workspace — count it as action.
                mutating_tool_ran = true;
                let (out, is_error) = spawn_subagent(p, perm, &call_input, cwd, depth);
                report::tool_result(&call.name, &out, is_error);
                trace_tool_result(&call.name, &out, is_error, "build", depth);
                results.push(ToolResult {
                    id: call.id.clone(),
                    content: out,
                    is_error,
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

            if tools::is_mutating_call(&call.name, &call_input) {
                mutating_tool_ran = true;
            }
            let out = tools::run(&call.name, &call_input, cwd);
            hooks::post_tool_use(&call.name, &call_input, &out.content, out.is_error, cwd);
            if out.is_error {
                hooks::notify("OnError", cwd);
            }
            report::tool_result(&call.name, &out.content, out.is_error);
            trace_tool_result(&call.name, &out.content, out.is_error, "build", depth);
            // Feed the verifier real data: every executed call, and the files
            // touched by successful write/edit tools.
            tool_records.push(crate::verifier::ToolCallRecord {
                tool_name: call.name.clone(),
                args_summary: tools::preview(&call.name, &call_input),
                result_preview: out.content.chars().take(200).collect(),
                timestamp: String::new(),
            });
            if !out.is_error {
                if let Some(pb) = tools::edit_tracking_path(&call.name, &call_input, cwd) {
                    let path = pb.display().to_string();
                    if !changed_files.contains(&path) {
                        changed_files.push(path);
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
                artifact_error_count += 1;
                if artifact_error_count <= 2 {
                    artifact_recovery = Some(format!(
                        "Your artifact was rejected: {}. Fix that exact problem and call \
                         Artifact/publish_artifact again with one complete self-contained HTML file \
                         (all CSS/JS inline). Do not answer with markdown code.",
                        out.content
                    ));
                } else {
                    loop_stop = Some(format!(
                        "artifact publishing failed after {artifact_error_count} attempts; \
                         last rejection: {}",
                        out.content
                    ));
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
                    &mut loop_nudge,
                    &mut loop_stop,
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
        // Persist the transcript after every tool round so a crash or kill
        // doesn't lose the session (save is cheap and swallows I/O errors).
        if let Some(sid) = sid {
            crate::session::save(sid, cwd, &p.model, msgs);
        }
        if !report::is_json() {
            tui::context_meter(estimate_tokens(msgs), p.context_tokens);
            tui::poll_typeahead();
        }
        if let Some(s) = summary {
            if depth == 0 && !report::is_json() {
                let verifier = crate::verifier::Verifier::new(&cwd.to_string_lossy());
                let ctx = crate::verifier::VerificationContext {
                    task_description: task.to_string(),
                    changed_files: changed_files.clone(),
                    tool_calls: tool_records.clone(),
                    ..Default::default()
                };
                let rep = verifier.verify(&ctx);
                match rep.status {
                    crate::verifier::VerificationStatus::Blocked
                    | crate::verifier::VerificationStatus::Failed
                        if verifier_fix_rounds < MAX_VERIFIER_FIX_ROUNDS =>
                    {
                        // Give the model a chance to address the violations
                        // before finishing.
                        verifier_fix_rounds += 1;
                        let violations =
                            crate::rules::RuleEngine::format_violations(&rep.rule_violations);
                        report::notice(&format!(
                            "  ⚠ verification {} — asking the model to address violations",
                            rep.status.label()
                        ));
                        tui::line(&tui::render_md(&violations));
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
                        report::notice(&format!(
                            "  ⚠ verification {}",
                            rep.status.label()
                        ));
                        return Ok(format!(
                            "{s}\n\n[verification note] The verifier still reports `{}` after \
                             {verifier_fix_rounds} fix attempts:\n{violations}",
                            rep.status
                        ));
                    }
                    crate::verifier::VerificationStatus::PassedWithWarnings => {
                        report::notice(&format!(
                            "  ⚠ verification {}",
                            rep.status.label()
                        ));
                        if !rep.rule_violations.is_empty() {
                            tui::line(&tui::render_md(
                                &crate::rules::RuleEngine::format_violations(
                                    &rep.rule_violations,
                                ),
                            ));
                        }
                    }
                    crate::verifier::VerificationStatus::Passed => {}
                }
            }
            return Ok(s);
        }
        if let Some(stop_msg) = loop_stop {
            // A stopped loop (or exhausted artifact recovery) is not success —
            // surface it as a failure with the honest summary.
            return Err(stop_msg);
        }
        if let Some(recovery) = artifact_recovery {
            msgs.push(Msg::User(recovery));
        } else if let Some(nudge) = loop_nudge {
            msgs.push(Msg::User(nudge));
        }
    }
    Err(format!(
        "reached the {MAX_ITERS}-step limit without finishing"
    ))
}

// Feedback for a tool call whose arguments failed to parse: name the tool,
// show the parse error and the schema's required params, and demand a re-send.
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

static SUB_SEQ: AtomicUsize = AtomicUsize::new(0);

// Returns the subagent's result and whether it failed — failures must reach
// the parent model as is_error so it can react instead of trusting bad output.
fn spawn_subagent(
    p: &Provider,
    perm: Permission,
    input: &serde_json::Value,
    cwd: &Path,
    depth: usize,
) -> (String, bool) {
    if depth + 1 >= MAX_DEPTH {
        return ("subagent depth limit reached".into(), true);
    }
    let task = input["task"]
        .as_str()
        .or_else(|| input["description"].as_str())
        .unwrap_or("")
        .trim();
    if task.is_empty() {
        return ("spawn_subagent requires a task".into(), true);
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

    report::info(&format!("  ↳ subagent: {}", trace::preview(task, 80)));
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
    let (result, is_error) =
        match build_inner(p, perm, role, task, &run_cwd, depth + 1, &mut child, None) {
            Ok(r) => (r, false),
            Err(e) => (format!("subagent error: {e}"), true),
        };
    trace::record_visible(
        "subagent_done",
        format!("{role}: {}", trace::preview(task, 80)),
        serde_json::json!({
            "task": task,
            "role": role,
            "isolate": isolate,
            "cwd": run_cwd.to_string_lossy(),
            "result": result,
            "is_error": is_error,
            "depth": depth + 1,
        }),
    );
    (format!("{note}{result}"), is_error)
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

// A build/create verb must co-occur with the artifact noun so that "fix the
// collision bug in my game" or "explain how this website works" don't trigger
// artifact-recovery flows.
fn task_has_build_verb(task: &str) -> bool {
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

fn static_artifact_requested(task: &str) -> bool {
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
fn task_is_imperative(task: &str) -> bool {
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

// Fix 14 decision: nudge the model to act instead of explaining. Fires only
// for imperative tasks, only before any mutating tool has run this session,
// and only while the per-task cap has not been reached. A prose reply with no
// tool activity at all, or one that reads as how-to instructions, is treated
// as explaining rather than doing.
fn should_nudge_to_act(
    task: &str,
    reply_text: &str,
    any_tool_ran: bool,
    mutating_tool_ran: bool,
    nudges_so_far: usize,
) -> bool {
    if nudges_so_far >= MAX_ACT_NUDGES || mutating_tool_ran || !task_is_imperative(task) {
        return false;
    }
    !any_tool_ran || reads_as_instructions(reply_text)
}

// True when a reply reads as instructions/explanation for the USER to follow
// ("here's how", "you should…") rather than a summary of completed work.
// Kept conservative: callers must also check that no mutating tool ran.
fn reads_as_instructions(text: &str) -> bool {
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

// ── PLAN mode ─────────────────────────────────────────────────────────────────
// The planning phase now has tools available so the model can inspect the
// codebase while breaking down the task. Execution still runs through BUILD.
pub fn run_plan(p: &Provider, perm: Permission, task: &str, cwd: &Path) -> Result<(), String> {
    // Role identity + mode contract come first; environment sections follow.
    let prefix = context_prefix(cwd, p.context_tokens);
    let sys = format!(
        "You are a planning engineer with full access to the codebase. \
        Use read_file/list_dir/list_tree/find_paths/grep_files/fetch_url and read-only bash/run_command calls to inspect the project as needed. \
        Do not write files, edit files, apply patches, spawn subagents, or run mutating shell commands while planning. \
        When ready, call exit_plan or ExitPlanMode with a concise numbered implementation plan. \
        The plan must be concrete, actionable, and at most 8 steps. \
        Do not include code fences, shell snippets, intro text, or outro text.\n\n{prefix}"
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
        let reply = request_reply(p, &msgs, &defs, "planning")?;
        let reply = normalize_text_tool_calls(reply, &defs, task);

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
        let mut loop_nudge: Option<String> = None;
        for call in &reply.calls {
            if let Some(raw) = call.input.get(tools::INVALID_ARGS).and_then(|v| v.as_str()) {
                let msg = invalid_args_feedback(&call.name, raw, &defs);
                report::tool_denied(&msg);
                note_loop_result(
                    &mut loop_guard,
                    &call.name,
                    &call.input,
                    &msg,
                    true,
                    &mut loop_nudge,
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
                    &mut loop_nudge,
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
                    &mut loop_nudge,
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
                &mut loop_nudge,
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
        if let Some(nudge) = loop_nudge {
            msgs.push(Msg::User(nudge));
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
    let full = approved_plan_build_task(task, &plan);
    run_build(p, perm, "engineer", &full, cwd)
}

fn approved_plan_build_task(task: &str, plan: &str) -> String {
    format!(
        "{task}\n\nFollow this approved plan:\n{plan}\n\n\
        BUILD EXECUTION DIRECTIVE:\n\
        - The user already approved this plan. Do not re-plan, ask to proceed, or answer with a plan.\n\
        - Execute the approved steps now using tools.\n\
        - If the task requires files/artifacts, create or edit them on disk instead of pasting code in chat.\n\
        - If the task requires a server or browser check, use start_server, wait_for_url, read_server_log, and open_browser where appropriate.\n\
        - Run focused verification when possible.\n\
        - Finish with the finish tool once the approved plan has been executed."
    )
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
    // Role identity + mode contract come first; environment sections follow.
    let prefix = context_prefix(cwd, p.context_tokens);
    let sys = format!("You are a sharp, concise thought partner with full access to the codebase and the internet. \
        Use tools freely to look things up, read files, grep for patterns, or run commands — \
        whatever helps the conversation. \
        When you think the user is ready to stop discussing and start building or planning, \
        end your response with the exact token [SUGGEST:BUILD] or [SUGGEST:PLAN] on its own line. \
        Otherwise just respond naturally. No fluff.\n\n{prefix}");

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
            let reply = request_reply(p, &msgs, &defs, "thinking")?;
            let reply = normalize_text_tool_calls(reply, &defs, &question);

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
            let mut loop_nudge: Option<String> = None;
            for call in &reply.calls {
                if let Some(raw) = call.input.get(tools::INVALID_ARGS).and_then(|v| v.as_str()) {
                    let msg = invalid_args_feedback(&call.name, raw, &defs);
                    report::tool_denied(&msg);
                    note_loop_result(
                        &mut loop_guard,
                        &call.name,
                        &call.input,
                        &msg,
                        true,
                        &mut loop_nudge,
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
                        &mut loop_nudge,
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
                        &mut loop_nudge,
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
                    &mut loop_nudge,
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
            if let Some(nudge) = loop_nudge {
                msgs.push(Msg::User(nudge));
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

pub fn run_chat_turn(
    p: &Provider,
    perm: Permission,
    cwd: &Path,
    question: &str,
) -> Result<(), String> {
    // Role identity + mode contract come first; environment sections follow.
    let prefix = context_prefix(cwd, p.context_tokens);
    let sys = format!(
        "You are buildwithnexus in a coding terminal. Answer the user's current message naturally and concisely. \
        If the user asks a normal conversational question or greeting, answer in plain text and do not call tools. \
        If answering well requires inspecting the workspace or environment, use tools, then summarize the result. \
        Do not emit JSON unless a tool call is actually required by the tool protocol.\n\n{prefix}"
    );

    let defs = tools::defs_for_context(false, p.context_tokens);
    let mut msgs: Vec<Msg> = vec![Msg::System(sys), Msg::User(question.to_string())];
    let mut loop_guard = ToolLoopGuard::default();

    for tool_round in 1..=MAX_CHAT_TOOL_ROUNDS {
        maybe_compact(p, &mut msgs);
        let reply = request_reply(p, &msgs, &defs, "thinking")?;
        let reply = normalize_text_tool_calls(reply, &defs, question);

        if reply.calls.is_empty() {
            if !reply.text.trim().is_empty() && !report::is_json() {
                tui::context_meter(estimate_tokens(&msgs), p.context_tokens);
            }
            return Ok(());
        }

        let mut results = Vec::new();
        let mut loop_summary: Option<String> = None;
        let mut loop_nudge: Option<String> = None;
        for call in &reply.calls {
            if let Some(raw) = call.input.get(tools::INVALID_ARGS).and_then(|v| v.as_str()) {
                let msg = invalid_args_feedback(&call.name, raw, &defs);
                report::tool_denied(&msg);
                note_loop_result(
                    &mut loop_guard,
                    &call.name,
                    &call.input,
                    &msg,
                    true,
                    &mut loop_nudge,
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
                "chat",
                tool_round,
                question,
            );
            report::tool_call(
                &call.name,
                &tools::preview(&call.name, &call_input),
                &call_input,
            );
            trace_tool_call(&call.name, &call_input, "chat", tool_round);

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
                    serde_json::json!({"tool": call.name, "reason": reason, "input": &call_input, "phase": "chat", "depth": tool_round}),
                );
                note_loop_result(
                    &mut loop_guard,
                    &call.name,
                    &call_input,
                    &reason,
                    true,
                    &mut loop_nudge,
                    &mut loop_summary,
                );
                results.push(ToolResult {
                    id: call.id.clone(),
                    content: reason,
                    is_error: true,
                });
                continue;
            }

            let out = tools::run(&call.name, &call_input, cwd);
            hooks::post_tool_use(&call.name, &call_input, &out.content, out.is_error, cwd);
            report::tool_result(&call.name, &out.content, out.is_error);
            trace_tool_result(&call.name, &out.content, out.is_error, "chat", tool_round);
            note_loop_result(
                &mut loop_guard,
                &call.name,
                &call_input,
                &out.content,
                out.is_error,
                &mut loop_nudge,
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
            report::assistant(&loop_msg);
            return Ok(());
        }
        if let Some(nudge) = loop_nudge {
            msgs.push(Msg::User(nudge));
        }
    }

    report::assistant(&format!(
        "I stopped after {MAX_CHAT_TOOL_ROUNDS} tool rounds without a final response."
    ));
    Ok(())
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
    fn answer_input_prompt_is_single_line() {
        // Must never contain a newline — a multi-line prompt breaks the
        // alt-screen composer's cursor positioning and hides typed input.
        let p = answer_input_prompt("");
        assert!(!p.contains('\n'));
        assert!(p.contains("Answer"));
        let d = answer_input_prompt("yes");
        assert!(!d.contains('\n'));
        assert!(d.contains("yes"));
    }

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
    fn text_json_tool_call_is_parsed_from_fenced_block() {
        let defs = tools::defs_for_context(true, 128_000);
        let reply = Reply {
            text: "```json\n{\"name\":\"start_server\",\"arguments\":{\"command\":\"npm start\",\"cwd\":\"/tmp/app\",\"port\":3000}}\n```".to_string(),
            ..Default::default()
        };
        let normalized = normalize_text_tool_calls(reply, &defs, "build the app");
        assert_eq!(normalized.text, "");
        assert_eq!(normalized.calls.len(), 1);
        assert_eq!(normalized.calls[0].name, "start_server");
        assert_eq!(normalized.calls[0].input["command"], "npm start");
    }

    #[test]
    fn qwen_tools_tagged_call_with_css_braces_is_parsed() {
        // The real failure from an end-to-end run against qwen2.5-coder-1.5b on
        // llama.cpp: the model emits its call as `<tools>{…}</tools>` text with
        // an HTML `content` field whose CSS contains `{ }`. The balanced-object
        // scanner must not stop at the first CSS brace.
        let defs = tools::defs_for_context(true, 128_000);
        let reply = Reply {
            text: "<tools>{\"name\":\"write_file\",\"arguments\":{\"path\":\"index.html\",\"content\":\"<style>body { color: #ff6faa; } .hero { padding: 2rem; }</style>\"}}</tools>".to_string(),
            ..Default::default()
        };
        let normalized = normalize_text_tool_calls(reply, &defs, "build a donut shop site");
        assert_eq!(normalized.calls.len(), 1, "text: unparsed");
        assert_eq!(normalized.calls[0].name, "write_file");
        assert_eq!(normalized.calls[0].input["path"], "index.html");
        assert!(normalized.calls[0].input["content"]
            .as_str()
            .unwrap()
            .contains("#ff6faa"));
    }

    #[test]
    fn tool_call_tagged_and_embedded_json_variants_parse() {
        let defs = tools::defs_for_context(true, 128_000);
        // Hermes/Qwen `<tool_call>` wrapper.
        let a = normalize_text_tool_calls(
            Reply {
                text: "<tool_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.txt\"}}</tool_call>"
                    .to_string(),
                ..Default::default()
            },
            &defs,
            "read it",
        );
        assert_eq!(a.calls.len(), 1);
        assert_eq!(a.calls[0].name, "read_file");
        // A leading sentence, then a bare JSON object.
        let b = normalize_text_tool_calls(
            Reply {
                text: "Sure, I'll do that now. {\"name\":\"read_file\",\"arguments\":{\"path\":\"b.txt\"}}"
                    .to_string(),
                ..Default::default()
            },
            &defs,
            "read it",
        );
        assert_eq!(b.calls.len(), 1);
        assert_eq!(b.calls[0].input["path"], "b.txt");
    }

    #[test]
    fn gemma_tool_code_write_file_positional_triple_quoted() {
        // The exact shape captured from gemma-2-2b-it: a ```tool_code fence with
        // a Python call, positional args, and triple-quoted HTML containing
        // commas, parens, and braces.
        let defs = tools::defs_for_context(true, 8192);
        let text = "```tool_code\nwrite_file(\"/p/index.html\", \"\"\"<!doctype html>\\n<canvas id=\"c\"></canvas>\\n<script>const a={x:1}; f(a,b);</script>\"\"\")\n```";
        let r = normalize_text_tool_calls(
            Reply {
                text: text.to_string(),
                ..Default::default()
            },
            &defs,
            "build a canvas game",
        );
        assert_eq!(r.calls.len(), 1, "text: {}", r.text);
        assert_eq!(r.calls[0].name, "write_file");
        assert_eq!(r.calls[0].input["path"], "/p/index.html");
        let content = r.calls[0].input["content"].as_str().unwrap();
        assert!(content.contains("<canvas"), "{content}");
        assert!(content.contains("f(a,b)"), "{content}");
    }

    #[test]
    fn gemma_tool_code_keyword_and_print_wrapper() {
        let defs = tools::defs_for_context(true, 8192);
        // Keyword arg + a print(...) wrapper Gemma sometimes adds.
        let text =
            "```tool_code\nprint(Artifact(title=\"Game\", contents=\"\"\"<html>ok</html>\"\"\"))\n```";
        let r = normalize_text_tool_calls(
            Reply {
                text: text.to_string(),
                ..Default::default()
            },
            &defs,
            "build it",
        );
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].name, "Artifact");
        assert_eq!(r.calls[0].input["title"], "Game");
        assert_eq!(r.calls[0].input["contents"], "<html>ok</html>");
    }

    #[test]
    fn python_value_and_arg_parsing_helpers() {
        assert_eq!(parse_py_value("\"\"\"hi, there\"\"\""), json!("hi, there"));
        assert_eq!(parse_py_value("'x'"), json!("x"));
        assert_eq!(parse_py_value("42"), json!(42));
        assert_eq!(parse_py_value("true"), json!(true));
        // A single top-level comma inside a triple-quoted string is not a split.
        let args = parse_python_args("\"a\", \"\"\"b, c\"\"\"");
        assert_eq!(args.len(), 2);
        assert_eq!(args[0].1, json!("a"));
        assert_eq!(args[1].1, json!("b, c"));
        // kw_eq only fires before a string literal, not on `==` in content.
        assert!(kw_eq_pos("contents=\"x\"").is_some());
        assert!(kw_eq_pos("\"a == b\"").is_none());
    }

    #[test]
    fn balanced_json_object_respects_strings_and_truncation() {
        assert_eq!(
            balanced_json_object("x {\"a\":\"}{\"} y"),
            Some("{\"a\":\"}{\"}")
        );
        // Unterminated object (truncated at the token limit) yields nothing.
        assert_eq!(balanced_json_object("{\"a\": {\"b\": 1"), None);
        assert_eq!(balanced_json_object("no braces here"), None);
    }

    #[test]
    fn casual_text_json_mutating_tool_call_is_ignored() {
        let defs = tools::defs_for_context(true, 128_000);
        let reply = Reply {
            text: "```json\n{\"name\":\"start_server\",\"arguments\":{\"command\":\"npm start\",\"port\":3000}}\n```".to_string(),
            ..Default::default()
        };
        let normalized = normalize_text_tool_calls(reply, &defs, "hello");
        assert!(normalized.calls.is_empty());
        assert!(normalized.text.contains("ready"));
        assert!(!normalized.text.contains("start_server"));
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
    fn approved_plan_handoff_is_execution_directive() {
        let full = approved_plan_build_task(
            "build me a canvas game",
            "1. Create a single HTML artifact.\n2. Verify it opens.",
        );
        assert!(full.contains("Follow this approved plan:"));
        assert!(full.contains("BUILD EXECUTION DIRECTIVE"));
        assert!(full.contains("Do not re-plan"));
        assert!(full.contains("Execute the approved steps now using tools"));
        assert!(full.contains("finish tool"));
    }

    #[test]
    fn recovery_task_text_strips_approved_plan_directive() {
        let full = approved_plan_build_task(
            "create scratch/exitplan-smoke.txt containing exit plan smoke",
            "1. Write the file.",
        );
        assert_eq!(
            recovery_task_text(&full),
            "create scratch/exitplan-smoke.txt containing exit plan smoke"
        );
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
    fn write_file_placeholder_path_is_repaired_but_content_is_kept() {
        // A broken destination path is repaired from the task, but the model's
        // non-empty content is preserved — never replaced with the task text.
        let repaired = repair_placeholder_tool_input(
            "write_file",
            &json!({"path": "~", "content": "exit plan smoke\n- Create scratch/exitplan-smoke.txt"}),
            Path::new("/tmp/example-project"),
            "create scratch/exitplan-smoke.txt containing exit plan smoke",
        )
        .unwrap();
        assert_eq!(repaired["path"], "scratch/exitplan-smoke.txt");
        assert_eq!(
            repaired["content"],
            "exit plan smoke\n- Create scratch/exitplan-smoke.txt"
        );
    }

    #[test]
    fn write_file_nonempty_content_is_never_overridden() {
        // Path matches the task but content differs: real model output must
        // pass through untouched (no repair at all).
        assert!(repair_placeholder_tool_input(
            "write",
            &json!({"path": "scratch/exitplan-smoke.txt", "content": "exit plan smoke\n\n1. Create scratch/exitplan-smoke.txt"}),
            Path::new("/tmp/example-project"),
            "create scratch/exitplan-smoke.txt containing exit plan smoke",
        )
        .is_none());
    }

    #[test]
    fn write_file_empty_or_placeholder_content_is_repaired() {
        let task = "create scratch/exitplan-smoke.txt containing exit plan smoke";
        let cwd = Path::new("/tmp/example-project");
        let empty = repair_placeholder_tool_input(
            "write_file",
            &json!({"path": "scratch/exitplan-smoke.txt", "content": ""}),
            cwd,
            task,
        )
        .unwrap();
        assert_eq!(empty["content"], "exit plan smoke");
        let todo = repair_placeholder_tool_input(
            "write_file",
            &json!({"path": "scratch/exitplan-smoke.txt", "content": "TODO"}),
            cwd,
            task,
        )
        .unwrap();
        assert_eq!(todo["content"], "exit plan smoke");
        assert_eq!(todo["path"], "scratch/exitplan-smoke.txt");
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
        // Only read-only binaries remain in the default allowed_commands list.
        for cmd in &["ls -la", "cat foo.txt", "grep -r pattern ."] {
            let r = gate(
                Permission::Ask,
                "run_command",
                &json!({"command": cmd}),
                cwd,
            );
            assert!(r.is_none(), "expected {cmd} to be auto-allowed in Ask mode");
        }
        // Mutating binaries now require confirmation (denied in a non-terminal).
        for cmd in &["npm install", "git commit -m 'x'"] {
            let r = gate(
                Permission::Ask,
                "run_command",
                &json!({"command": cmd}),
                cwd,
            );
            assert!(
                r.is_some(),
                "expected {cmd} to require confirmation in Ask mode"
            );
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

    #[test]
    fn truncate_at_char_boundary_never_splits_multibyte() {
        let s = "ab🚀cd"; // the emoji occupies bytes 2..6
        assert_eq!(truncate_at_char_boundary(s, 3), "ab");
        assert_eq!(truncate_at_char_boundary(s, 6), "ab🚀");
        assert_eq!(truncate_at_char_boundary(s, 100), s);
        let cjk = "日本語テキスト"; // 3 bytes per char
        let t = truncate_at_char_boundary(cjk, 7);
        assert_eq!(t, "日本");
        assert!(cjk.starts_with(t));
    }

    #[test]
    fn compaction_split_never_starts_tail_on_tool_result() {
        // Layout puts the naive tail boundary on a Msg::Tool; the split must
        // advance past it so no tool_result is orphaned from its tool_use.
        let mut msgs = vec![Msg::System("s".into()), Msg::User("task".into())];
        for i in 0..3 {
            msgs.push(Msg::Assistant {
                text: format!("a{i}"),
                calls: vec![],
            });
            msgs.push(Msg::Tool(vec![crate::provider::ToolResult {
                id: format!("{i}"),
                content: "r".into(),
                is_error: false,
            }]));
        }
        msgs.push(Msg::User("follow-up".into()));
        // len = 9 → naive tail_start = 3, which is a Msg::Tool.
        assert!(matches!(msgs[3], Msg::Tool(_)));
        let (sys_end, tail_start) = compaction_split(&msgs);
        assert_eq!(sys_end, 1);
        assert_eq!(tail_start, 4);
        assert!(matches!(msgs[tail_start], Msg::Assistant { .. }));
    }

    #[test]
    fn compaction_pins_original_task_verbatim() {
        let task = "build the 🚀 thing";
        let mut msgs = vec![Msg::System("sys".into()), Msg::User(task.into())];
        for i in 0..10 {
            msgs.push(Msg::User(format!("u{i}")));
        }
        let out = compact_with(msgs, |_| "SUMMARY".into());
        let Msg::User(summary) = &out[1] else {
            panic!("expected summary user message");
        };
        assert!(summary.contains(&format!("[Original task]\n{task}")));
        // A second compaction re-extracts the same verbatim task text.
        let mut again = out;
        for i in 0..10 {
            again.push(Msg::User(format!("v{i}")));
        }
        let out2 = compact_with(again, |_| "SUMMARY2".into());
        let Msg::User(summary2) = &out2[1] else {
            panic!("expected summary user message");
        };
        assert!(summary2.contains(&format!("[Original task]\n{task}")));
    }

    #[test]
    fn compaction_keeps_recent_tail_tool_results_intact() {
        let long = "x".repeat(2000);
        let mut msgs = vec![Msg::System("sys".into()), Msg::User("task".into())];
        for i in 0..8 {
            msgs.push(Msg::User(format!("u{i}")));
        }
        msgs.push(Msg::Assistant {
            text: "a".into(),
            calls: vec![],
        });
        msgs.push(Msg::Tool(vec![crate::provider::ToolResult {
            id: "1".into(),
            content: long.clone(),
            is_error: false,
        }]));
        let out = compact_with(msgs, |_| "S".into());
        let Some(Msg::Tool(results)) = out.last() else {
            panic!("expected tool results in the tail");
        };
        assert_eq!(results[0].content, long, "tail results must not be clipped");
    }

    #[test]
    fn loop_guard_ignores_repeated_successful_results() {
        let mut guard = ToolLoopGuard::default();
        let input = json!({"path": "src/main.rs"});
        for _ in 0..10 {
            assert!(guard
                .note("read_file", &input, "fn main() {}", false)
                .is_none());
        }
    }

    #[test]
    fn loop_guard_nudges_then_stops_on_repeated_errors() {
        let mut guard = ToolLoopGuard::default();
        let input = json!({"path": "missing.txt"});
        assert!(guard
            .note("read_file", &input, "no such file", true)
            .is_none());
        assert!(guard
            .note("read_file", &input, "no such file", true)
            .is_none());
        let third = guard.note("read_file", &input, "no such file", true);
        assert!(matches!(third, Some(LoopSignal::Nudge(_))));
        // The same error repeating after the nudge stops the run.
        let fourth = guard.note("read_file", &input, "no such file", true);
        assert!(matches!(fourth, Some(LoopSignal::Stop(_))));
    }

    #[test]
    fn loop_guard_resets_when_result_changes() {
        let mut guard = ToolLoopGuard::default();
        let input = json!({"path": "f"});
        assert!(guard.note("read_file", &input, "err A", true).is_none());
        assert!(guard.note("read_file", &input, "err A", true).is_none());
        // A different result resets the counter — no trigger on the next call.
        assert!(guard.note("read_file", &input, "err B", true).is_none());
        assert!(guard.note("read_file", &input, "err B", true).is_none());
    }

    #[test]
    fn max_tokens_stop_reason_is_detected() {
        let truncated = Reply {
            stop_reason: Some("max_tokens".into()),
            ..Default::default()
        };
        assert!(reply_truncated_at_token_limit(&truncated));
        let done = Reply {
            stop_reason: Some("end_turn".into()),
            ..Default::default()
        };
        assert!(!reply_truncated_at_token_limit(&done));
        assert!(!reply_truncated_at_token_limit(&Reply::default()));
    }

    #[test]
    fn artifact_classifiers_require_build_verb() {
        assert!(canvas_game_requested("build me a canvas game"));
        assert!(canvas_game_requested("make a browser game about space"));
        assert!(!canvas_game_requested("fix the collision bug in my game"));
        assert!(static_artifact_requested(
            "create a landing page for my startup"
        ));
        assert!(!static_artifact_requested("explain how this website works"));
        assert!(!static_artifact_requested("why is the landing page slow?"));
    }

    #[test]
    fn casual_turn_detection_only_matches_pure_greetings() {
        assert!(is_casual_turn("hi"));
        assert!(is_casual_turn("Hello!"));
        assert!(is_casual_turn("hey"));
        // Acknowledgements can be valid "yes, proceed" turns.
        assert!(!is_casual_turn("ok"));
        assert!(!is_casual_turn("okay"));
        assert!(!is_casual_turn("thanks"));
        assert!(!is_casual_turn("yes, proceed"));
    }

    #[test]
    fn placeholder_content_detection_is_conservative() {
        assert!(is_placeholder_content(""));
        assert!(is_placeholder_content("  \n"));
        assert!(is_placeholder_content("TODO"));
        assert!(is_placeholder_content("<content>"));
        assert!(!is_placeholder_content("hello world"));
        assert!(!is_placeholder_content("TODO: fix the parser later"));
    }

    #[test]
    fn invalid_args_feedback_names_tool_error_and_required_params() {
        let defs = tools::defs_for_context(true, 128_000);
        let msg = invalid_args_feedback("write_file", "{not json", &defs);
        assert!(msg.contains("write_file"));
        assert!(msg.contains("Required params:"));
        assert!(msg.ends_with("Re-send the complete corrected call."));
    }

    #[test]
    fn instructional_prose_classifier_is_conservative() {
        assert!(reads_as_instructions("First, you should create the file."));
        assert!(reads_as_instructions(
            "Here's how to build it. Steps:\n1. Create index.html\n2. Add a canvas"
        ));
        assert!(reads_as_instructions("To do this, you can run npm init."));
        // Completed-work summaries must not read as instructions.
        assert!(!reads_as_instructions(
            "I created index.html with the game and published the artifact."
        ));
        assert!(!reads_as_instructions(
            "Done — the collision bug is fixed and verified with the test suite."
        ));
    }

    #[test]
    fn act_nudge_fires_on_instructional_prose_for_imperative_task() {
        let prose = "Here's how you can build it:\nSteps:\n1. Create index.html\n2. Add a canvas";
        // Imperative task + how-to prose + no prior mutation → nudge.
        assert!(should_nudge_to_act(
            "build me a todo app",
            prose,
            false,
            false,
            0
        ));
        assert!(should_nudge_to_act(
            "fix the collision bug in my game",
            prose,
            false,
            false,
            0
        ));
        // The same prose after a mutating tool ran is a real summary → no nudge.
        assert!(!should_nudge_to_act(
            "build me a todo app",
            prose,
            true,
            true,
            0
        ));
        // Question-style tasks want an answer, not action → no nudge.
        assert!(!should_nudge_to_act(
            "how do I build a todo app?",
            prose,
            false,
            false,
            0
        ));
        assert!(!should_nudge_to_act(
            "what does this repo do",
            prose,
            false,
            false,
            0
        ));
        // Read-only exploration followed by a plain answer (no markers) → no nudge.
        assert!(!should_nudge_to_act(
            "build me a todo app",
            "The repo already contains a complete todo app in src/.",
            true,
            false,
            0
        ));
        // The per-task cap is respected.
        assert!(!should_nudge_to_act(
            "build me a todo app",
            prose,
            false,
            false,
            MAX_ACT_NUDGES
        ));
    }
}
