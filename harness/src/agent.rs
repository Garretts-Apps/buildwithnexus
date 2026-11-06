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

// ── context compaction ────────────────────────────────────────────────────────
// The length of `v` serialized as compact JSON, without building the string:
// this runs on every step, over every tool input in the conversation.
fn json_len(v: &serde_json::Value) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0 += b.len();
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut n = Count(0);
    let _ = serde_json::to_writer(&mut n, v);
    n.0
}

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
                        .map(|c| c.name.len() + json_len(&c.input))
                        .sum::<usize>()
            }
            Msg::Tool(rs) => rs
                .iter()
                .map(|r| r.content.len() + r.images.iter().map(|(_, d)| d.len() / 3).sum::<usize>())
                .sum(),
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
                    if !r.images.is_empty() {
                        s.push_str(&format!("[+{} image(s)] ", r.images.len()));
                    }
                    s.push_str(&r.content.chars().take(800).collect::<String>());
                }
            }
        }
        s.push('\n');
    }
    s
}

// Compaction with no size limit on the carried images.
#[cfg(test)]
fn compact_with(msgs: Vec<Msg>, summarize: impl FnOnce(&[Msg]) -> String) -> Vec<Msg> {
    compact_within(msgs, usize::MAX, summarize)
}

// `compact_with` for a transcript that must come out under `budget`
// estimated tokens: carried images are dropped, oldest first, until it does.
fn compact_within(
    msgs: Vec<Msg>,
    budget: usize,
    summarize: impl FnOnce(&[Msg]) -> String,
) -> Vec<Msg> {
    // The last request's measured prompt size described the transcript
    // being replaced; the meter falls back to the estimate until the next one.
    crate::usage::forget_last();
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
    // It goes last: everything after the marker is read back as the task.
    let pinned = task.map_or(String::new(), |t| format!("\n\n{ORIGINAL_TASK_MARKER}{t}"));
    let rest = estimate_tokens(&v) + estimate_tokens(&tail) + (body.len() + pinned.len() + 160) / 4;
    let (images, dropped) = images_to_keep(&middle, budget.saturating_sub(rest));
    if dropped > 0 {
        let kept = match images.len() {
            0 => String::new(),
            n => format!("; the {n} most recent are attached"),
        };
        body.push_str(&format!(
            "\n\n[{dropped} earlier image(s) were dropped during compaction{kept}]"
        ));
    }
    body.push_str(&pinned);
    v.push(user_msg(body, images));
    v.extend(tail);
    v
}

// Images attached to the turns being summarized ride along on the summary
// turn: the model can still see what the user showed it and what its tools
// returned. Each image once, in first-seen order, capped to the most recent
// few that fit in `room` estimated tokens; the count of any dropped is
// returned.
const MAX_KEPT_IMAGES: usize = 4;

fn images_to_keep(middle: &[Msg], room: usize) -> (Vec<(String, String)>, usize) {
    let mut all: Vec<(String, String)> = Vec::new();
    for m in middle {
        let images: Vec<&(String, String)> = match m {
            Msg::UserImages { images, .. } => images.iter().collect(),
            Msg::Tool(rs) => rs.iter().flat_map(|r| &r.images).collect(),
            _ => Vec::new(),
        };
        for img in images {
            if !all.contains(img) {
                all.push(img.clone());
            }
        }
    }
    let mut used = 0usize;
    let mut kept = 0usize;
    for (_, data) in all.iter().rev() {
        // As estimate_tokens counts an image, rounded up.
        let cost = data.len() / 3 / 4 + 1;
        if kept == MAX_KEPT_IMAGES || used + cost > room {
            break;
        }
        used += cost;
        kept += 1;
    }
    let dropped = all.len() - kept;
    all.drain(..dropped);
    (all, dropped)
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
    let budget = compaction_budget(p);
    if estimate_tokens(msgs) <= budget {
        return;
    }
    let (sys_end, tail_start) = compaction_split(msgs);
    if tail_start <= sys_end {
        return;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    hooks::pre_compact("auto", "", &cwd);
    report::info("  ⟳ compacting context…");
    let taken = std::mem::take(msgs);
    *msgs = compact_within(taken, budget, |middle| model_summary(p, middle));
}

// The estimated size a compacted transcript must fit in: 80% of the model's
// context, or no limit when that is unknown.
fn compaction_budget(p: &Provider) -> usize {
    match p.context_tokens.saturating_mul(8) / 10 {
        0 => usize::MAX,
        n => n,
    }
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
        // Shown once, by the loop that ends on it.
        Some(LoopSignal::Stop(msg)) => *stop = Some(msg),
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
    step: usize,
    // Current line, buffered until its newline so markdown renders whole —
    // a half-streamed span like `**bo` can't be rendered, and re-rendering a
    // partial line already on screen would mean erase-and-repaint (flicker).
    line: String,
}

impl ThinkingStream {
    fn push(&mut self, chunk: &str) {
        if report::is_json() {
            return;
        }
        tui::poll_typeahead();
        for ch in chunk.chars() {
            match ch {
                '\r' => {}
                '\n' => self.flush_line(),
                _ => self.line.push(ch),
            }
        }
    }

    // Emit one completed thinking line as a single write: spinner + rendered
    // markdown + newline. Writing whole lines (never mid-line cursor moves)
    // keeps the stream from repainting rows the user is already reading.
    fn flush_line(&mut self) {
        let content = self.line.trim_end().to_string();
        self.line.clear();
        if content.is_empty() {
            return;
        }
        let spinners = tui::SPINNER_FRAMES;
        let s = spinners[self.step % spinners.len()];
        self.step += 1;
        tui::write_stream(&format!(
            "  {} {} {}\n",
            tui::cyan(s),
            tui::dim("thinking ›"),
            tui::render_md_dim_line(&content)
        ));
    }

    fn finish(&mut self) {
        self.flush_line();
        tui::render_queued_composer();
    }
}

// Meter input after a round-trip: the size the server measured for the last
// request when it reported usage, else the chars/4 estimate.
fn context_used(msgs: &[Msg]) -> usize {
    crate::usage::last_context_tokens().unwrap_or_else(|| estimate_tokens(msgs))
}

// `--max-budget-usd` guard, checked before each model request so a session
// stops between requests rather than mid-stream. Prints the notice (a
// `notice` event in --json mode) and reports whether the loop must stop.
fn budget_exhausted() -> bool {
    match crate::usage::budget_stop() {
        Some(msg) => {
            report::notice(&msg);
            stopped_short(Outcome::BudgetStop);
            true
        }
        None => false,
    }
}

fn request_reply(
    p: &Provider,
    msgs: &[Msg],
    defs: &[tools::ToolDef],
    label: &str,
) -> Result<Reply, String> {
    if report::has_sink() {
        // `bwn acp`: the reply streams to the editor as it arrives.
        return provider::stream(
            p,
            msgs,
            defs,
            &mut |c| report::assistant_delta(c),
            &mut |t| report::thinking_delta(t),
        );
    }
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
            if tui::interrupted() {
                return;
            }
            thinking.borrow_mut().finish();
            renderer.push(c);
            // Drains typed keys and repaints the composer once.
            tui::poll_typeahead();
            streamed = true;
        },
        &mut |t| {
            if tui::interrupted() {
                return;
            }
            thinking.borrow_mut().push(t);
        },
    );
    thinking.borrow_mut().finish();
    let r = match res {
        Ok(r) => r,
        Err(e) if e == "interrupted" || e.contains("interrupted") => {
            renderer.flush();
            if streamed {
                report::assistant_end();
            }
            return Err("interrupted".to_string());
        }
        Err(e) => return Err(e),
    };
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

// Tag forms local models use to emit tool calls as text (see
// extract_json_tool_candidate). Shared so the malformed-markup detector and
// the parser can never drift apart.
const TOOL_CALL_TAGS: [&str; 4] = ["<tools>", "<tool_call>", "<function_call>", "<function>"];

// A reply that produced no calls but still carries tool-call markup is a
// failed tool call, not a final answer — the tagged JSON was malformed or cut
// off before the parser could extract it. Detecting it lets the loop ask for
// a re-emit instead of presenting broken markup to the user as the answer.
fn malformed_tool_markup(reply: &Reply) -> bool {
    reply.calls.is_empty() && TOOL_CALL_TAGS.iter().any(|t| reply.text.contains(t))
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
    // The calls show as ordinary tool lines; the repair is in /trace.
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
    if let Some((candidate, placement)) = extract_json_tool_candidate(text) {
        // A call followed by a sentence ("… I will read the file first.")
        // is not JSON as a whole; its leading object is.
        let value = serde_json::from_str::<serde_json::Value>(candidate)
            .ok()
            .or_else(|| balanced_json_object(candidate).and_then(|o| serde_json::from_str(o).ok()));
        if let Some(value) = value {
            let mut calls = Vec::new();
            collect_text_tool_calls(&value, &names, &mut calls);
            // JSON quoted inside an answer ("you could send {…}", how
            // function calling works) is an example, never a call: only a
            // reply that opens with the JSON, is a fence of it, or opens with
            // call markup runs it.
            match placement {
                Placement::Reply => {}
                // A leading object followed by prose: a call when it names an
                // offered tool, else the example an explanation opens with.
                Placement::Leading => calls.retain(|c| names.contains(c.name.as_str())),
                Placement::Quoted => calls.clear(),
            }
            // A name that is not a tool is answered with the tools offered
            // (unoffered_call), so the turn goes on instead of ending on an
            // unusable reply.
            if !calls.is_empty() {
                return Some(calls);
            }
        }
    }
    // Fall back to Gemma's `tool_code` Python-call format.
    if let Some(calls) = parse_tool_code_call(text, &names) {
        return Some(calls);
    }
    parse_prose_tool_call(text, &names)
}

fn parse_prose_tool_call(
    text: &str,
    names: &HashSet<&'static str>,
) -> Option<Vec<provider::ToolCall>> {
    let lower = text.to_lowercase();
    if (names.contains("exit_plan") || names.contains("ExitPlanMode"))
        && (lower.contains("exit plan mode")
            || lower.contains("exit_plan")
            || lower.contains("exitplanmode"))
    {
        let name = if names.contains("exit_plan") {
            "exit_plan"
        } else {
            "ExitPlanMode"
        };
        return Some(vec![text_tool_call(name, serde_json::json!({}))]);
    }
    None
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
    let (name, inside) = match tool_code_fence(text) {
        Some(scope) => {
            let (name, after) = find_tool_call(scope, allowed)?;
            (name, balanced_parens(after)?)
        }
        None => whole_reply_call(text, allowed)?,
    };
    let args = parse_python_args(inside);
    let input = tool_code_args_to_input(name, args);
    Some(vec![text_tool_call(name, input)])
}

// The inside of a ```tool_code fence, when the reply has one.
fn tool_code_fence(text: &str) -> Option<&str> {
    // Only a fence placed as the reply's call (placed_as_call): one inside
    // an explanation quotes a call.
    let pos = text.find("```tool_code")?;
    let after = &text[pos + "```tool_code".len()..];
    let end = after.find("```").unwrap_or(after.len());
    let close = (pos + "```tool_code".len() + end + 3).min(text.len());
    placed_as_call(text, pos, close).then_some(&after[..end])
}

// Without the fence (Gemma sometimes omits it) the reply must be nothing but
// the call, optionally inside `print(...)`. A name and `(` in a sentence
// ("you could run_command(touch x) later"), or bwn's own refusal quoted back
// ("denied by rule run_command(rm *)"), is prose, not a call. Returns the
// name and the argument text.
fn whole_reply_call<'a>(
    text: &'a str,
    allowed: &HashSet<&'static str>,
) -> Option<(&'static str, &'a str)> {
    let t = text.trim();
    let (body, wrapped) = match t.strip_prefix("print(") {
        Some(rest) => (rest.trim_start(), true),
        None => (t, false),
    };
    let (name, after) = find_tool_call(body, allowed)?;
    // The call opens the reply: only its name comes before the `(`.
    if body[..body.len() - after.len() - 1].trim_end() != name {
        return None;
    }
    let inside = balanced_parens(after)?;
    let mut rest = after[inside.len() + 1..].trim();
    if wrapped {
        rest = rest.strip_prefix(')')?.trim();
    }
    rest.is_empty().then_some((name, inside))
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

// The JSON that may hold a tool call, and whether the reply is that call
// (it leads the reply, fills a fence that is the whole message, or sits in
// call markup) rather than JSON embedded in prose.
// Where text JSON sits: it is the reply (alone, a whole fence, or call
// markup placed as the reply's call), it opens a reply that goes on in prose
// or ends a one-line lead-in, or it is quoted inside an answer.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Placement {
    Reply,
    Leading,
    Quoted,
}

fn extract_json_tool_candidate(text: &str) -> Option<(&str, Placement)> {
    let trimmed = text.trim();
    // Whole-text JSON (the strict, original case). JSON followed by prose is
    // an answer that opens with an example; its leading object is looked at
    // below, like JSON anywhere else in an answer.
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && serde_json::from_str::<serde::de::IgnoredAny>(trimmed).is_ok()
    {
        return Some((trimmed, Placement::Reply));
    }
    // A fenced ```json block that is the entire message.
    if let Some(rest) = trimmed.strip_prefix("```") {
        if let Some(fence_end) = rest.find('\n') {
            let lang = rest[..fence_end].trim().to_ascii_lowercase();
            if lang.is_empty() || lang == "json" {
                let body = &rest[fence_end + 1..];
                if let Some(close) = body.rfind("```") {
                    if body[close + 3..].trim().is_empty() {
                        return Some((body[..close].trim(), Placement::Reply));
                    }
                }
            }
        }
    }
    // XML-tagged tool calls. Small local coders (notably Qwen2.5-Coder on
    // llama.cpp/Ollama) emit calls as `<tools>{…}</tools>` or
    // `<tool_call>{…}</tool_call>` text in `content` instead of native
    // tool_calls. Pull the first balanced JSON object out of the tagged region.
    for tag in TOOL_CALL_TAGS {
        if let Some(pos) = text.find(tag) {
            if let Some(json) = balanced_json_object(&text[pos + tag.len()..]) {
                let mut end = offset_in(text, json) + json.len();
                let close = format!("</{}", &tag[1..]);
                if let Some(c) = text[end..].find(&close) {
                    end += c + close.len();
                    end += text[end..].find('>').map_or(0, |g| g + 1);
                }
                let placed = if placed_as_call(text, pos, end) {
                    Placement::Reply
                } else {
                    Placement::Quoted
                };
                return Some((json, placed));
            }
        }
    }
    balanced_json_object(text).map(|json| {
        let start = offset_in(text, json);
        let placed = if placed_as_call(text, start, start + json.len()) {
            Placement::Leading
        } else {
            Placement::Quoted
        };
        (json, placed)
    })
}

// Byte offset of `part`, a slice of `whole`, within it.
fn offset_in(whole: &str, part: &str) -> usize {
    part.as_ptr() as usize - whole.as_ptr() as usize
}

// Whether a call at text[start..end] is the reply's own call rather than one
// it quotes. It opens the reply ("{…} I will read the file first."), or ends
// it after one short lead-in line ("Sure, reading it now. {…}"). A call with
// prose after it, or after a paragraph or an "for example"/"you could", is
// the model describing a call, and running it would act on an example.
fn placed_as_call(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].trim();
    if before.is_empty() {
        return true;
    }
    let lower = before.to_lowercase();
    text[end..].trim().is_empty()
        && before.len() <= 200
        && !before.contains('\n')
        && ![
            "example",
            "e.g.",
            "could",
            "would",
            "should",
            "like this",
            "for instance",
            "such as",
            "never",
            "don't",
            "do not",
            "won't",
            "will not",
        ]
        .iter()
        .any(|w| lower.contains(w))
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
    // Small models write the arguments as an object or a JSON string, under
    // `arguments`, Llama 3's `parameters`, or `input`.
    let args_of = |o: &serde_json::Map<String, serde_json::Value>| {
        o.get("arguments")
            .or_else(|| o.get("parameters"))
            .or_else(|| o.get("input"))
            .map(text_call_args)
    };
    if let Some(function) = obj.get("function").and_then(|v| v.as_object()) {
        let Some(name) = function.get("name").and_then(|v| v.as_str()) else {
            return;
        };
        if !allowed.contains(name) && !looks_like_tool_name(name) {
            return;
        }
        let input = args_of(function).unwrap_or_else(|| serde_json::json!({}));
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
    let input = match args_of(obj) {
        // An explicit arguments object marks a call even to a name that was
        // never offered; the tool runner answers it with the real list. A
        // tool's definition (a description, a JSON Schema) is not a call.
        Some(input) if allowed.contains(name) => input,
        Some(input) if looks_like_tool_name(name) && !is_tool_definition(obj, &input) => input,
        Some(_) => return,
        None if allowed.contains(name) => {
            let mut map = serde_json::Map::new();
            for (k, v) in obj {
                if k != "name" && k != "tool_name" && k != "type" && k != "id" {
                    map.insert(k.clone(), v.clone());
                }
            }
            serde_json::Value::Object(map)
        }
        None => return,
    };
    out.push(text_tool_call(name, input));
}

fn is_tool_definition(
    obj: &serde_json::Map<String, serde_json::Value>,
    args: &serde_json::Value,
) -> bool {
    obj.contains_key("description") || (args["type"] == "object" && args["properties"].is_object())
}

fn text_call_args(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => {
            serde_json::from_str(s).unwrap_or_else(|_| serde_json::json!({}))
        }
        serde_json::Value::Object(_) => v.clone(),
        _ => serde_json::json!({}),
    }
}

// A name a model would give a tool (`find_paths`, `search.files`), not a
// person's or product's name that happens to sit under a "name" key.
fn looks_like_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && (name.contains('_') || name.chars().all(|c| !c.is_ascii_uppercase()))
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
// The default is model-supplied, like the question.
fn answer_input_prompt(default: &str) -> String {
    if default.is_empty() {
        "  Answer: ".to_string()
    } else {
        let default = tui::sanitize_terminal(default);
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
    helper_prompt(
        || answer_with(question, &full_prompt, default, tui::ask),
        || {
            stop_turn();
            (STOPPED_BY_USER.into(), true)
        },
    )
}

// Shows the question and reads the answer with `ask` (None: cancelled).
fn answer_with(
    question: &str,
    full_prompt: &str,
    default: &str,
    ask: impl FnOnce(&str) -> Option<String>,
) -> (String, bool) {
    needs_you(
        "question",
        &format!("question: {}", trace::preview(question, 120)),
    );
    // Render the question on its own transcript line, then read the answer with
    // a SINGLE-LINE composer prompt. A multi-line prompt string mis-positions
    // the alt-screen composer cursor (prompt_width counts across the newline),
    // which hides what the user types. The question and options are
    // model-supplied, so their escapes are neutralized.
    tui::line(&format!(
        "  {} {}",
        tui::yellow("?"),
        tui::bold(&tui::sanitize_terminal(full_prompt))
    ));
    let Some(ans) = ask(&answer_input_prompt(default)) else {
        // Esc or Ctrl+C at the question stops the turn, as at an approval;
        // the model's default is never sent as the person's answer.
        stop_turn();
        return (STOPPED_BY_USER.into(), true);
    };
    (question_answer(ans, default), false)
}

/// What the model is told the person answered: Enter on an empty line takes
/// the default, typed text wins. (Esc or Ctrl+C never gets here: it stops
/// the turn, so the default is never sent as the person's choice.)
fn question_answer(ans: String, default: &str) -> String {
    if ans.trim().is_empty() && !default.is_empty() {
        default.to_string()
    } else {
        ans
    }
}

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
    // entry allows them without asking.
    if let Some(h) = host_ref {
        if matches!(perm, Permission::Auto) || allowed_by_rule {
            return None;
        }
        if is_pre_approved(None, &tool_key, cwd) || is_pre_approved(None, "fetch *", cwd) {
            return None;
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

    // Track which files have been read this session so we can enforce read-before-write.
    let mut read_paths: std::collections::HashSet<PathBuf> = Default::default();
    let mut loop_guard = ToolLoopGuard::default();
    let mut artifact_error_count = 0usize;
    let mut static_artifact_recovery_count = 0usize;
    let mut empty_reply_retried = false;
    let mut malformed_markup_retried = false;
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
    // check_work enforcement: whether the model ran it, the last verdict, and
    // whether the harness already ran it once on its behalf.
    let mut check_work_called = false;
    let mut check_work_passed: Option<bool> = None;
    let mut auto_check_rounds = 0usize;
    // The first call in the last tool round that never ran (unknown tool,
    // invalid or missing arguments): ending on text after one is not success.
    let mut last_unrun: Option<String> = None;

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
                if !forced_compact_retry
                    && (lower.contains("prompt is too long") || lower.contains("context length"))
                {
                    forced_compact_retry = true;
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
        // Tool markup that didn't parse into a call: the model tried to act
        // but emitted broken JSON inside the tags. Reprompting once turns a
        // garbage "final answer" into a corrected call; bounded so a model
        // that can't produce valid JSON still terminates.
        if malformed_tool_markup(&reply) && !malformed_markup_retried {
            malformed_markup_retried = true;
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
                if depth == 0 && (!any_tool_ran || last_unrun.is_some()) {
                    stopped_short(Outcome::Failed);
                }
                return Ok(reply.text);
            }
            // Imperative task answered with how-to prose instead of tool
            // calls: tell the model to act, bounded per task. Never fires
            // once a mutating tool has run (that prose is a real summary),
            // nor in a review, whose answer is prose by design (the diff in
            // its task reads as an order to change things).
            if !REVIEWING.load(Ordering::Relaxed)
                && should_nudge_to_act(
                    &task_for_recovery,
                    &reply.text,
                    any_tool_ran,
                    mutating_tool_ran,
                    act_nudges,
                )
            {
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
                if let Some(reason) = hook_gate(perm, "write_file", &input, cwd) {
                    report::tool_denied(&reason);
                    msgs.push(Msg::User(format!(
                        "[harness] write_file recovery blocked: {reason}"
                    )));
                    return Err(reason);
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
                if let Some(reason) = hook_gate(perm, "Artifact", &input, cwd) {
                    report::tool_denied(&reason);
                    msgs.push(Msg::User(format!(
                        "[harness] Artifact recovery blocked: {reason}"
                    )));
                    return Err(reason);
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
            if depth == 0 {
                if let Some(note) = unrun_call_note(last_unrun.as_deref(), &reply.text, &defs) {
                    report::notice(&note);
                    stopped_short(Outcome::Failed);
                }
                report::refused_this_turn();
            }
            return Ok(reply.text);
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
                    &mut loop_guard,
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
                    check_work_called = true;
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
                    images: Vec::new(),
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
                        "read('{}') required before editing or deleting existing file. Read the file first, then retry.",
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
                        images: Vec::new(),
                    });
                    continue;
                }
            }

            if matches!(call.name.as_str(), "task" | "spawn_subagent") {
                // A subagent can mutate the workspace — count it as action.
                mutating_tool_ran = true;
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
                mutating_tool_ran = true;
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
                check_work_called = true;
                if !check_work_found_no_project(&out.content) {
                    check_work_passed = Some(!out.is_error);
                }
            }
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
                if depth == 0 {
                    report::refused_this_turn();
                }
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
        last_unrun = round_unrun;
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
                changed_files.len(),
                check_work_called,
                auto_check_rounds,
            ) {
                auto_check_rounds += 1;
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
                tool_records.push(crate::verifier::ToolCallRecord {
                    tool_name: "check_work".into(),
                    args_summary: tools::preview("check_work", &input),
                    result_preview: preview.chars().take(200).collect(),
                    timestamp: String::new(),
                });
                if let Some(out) = out.filter(|o| !check_work_found_no_project(&o.content)) {
                    check_work_passed = Some(!out.is_error);
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
            let s = if auto_check_rounds > 0 && check_work_passed == Some(false) {
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
                    changed_files: changed_files.clone(),
                    tool_calls: tool_records.clone(),
                    tests_passed: check_work_passed,
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
                        if check_work_passed == Some(false) {
                            stopped_short(Outcome::CheckWorkFailed);
                        }
                        stopped_short(Outcome::VerificationFailed);
                        return Ok(format!(
                            "{s}\n\n[verification note] The verifier still reports `{}` after \
                             {verifier_fix_rounds} fix attempts:\n{violations}",
                            rep.status
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
                if check_work_passed == Some(false) {
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

// ── review ───────────────────────────────────────────────────────────────────

// Set while `run_review` runs, so every refusal says why edits are off.
static REVIEWING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
const REVIEW_READONLY: &str =
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
struct HelperCtx {
    agent: Option<config::AgentDef>,
    read_only: bool,
    // How a prompt from it names it: role and task.
    label: String,
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

fn note_helper_refusal(depth: usize, what: &str) {
    HELPER_REFUSED.with(|r| {
        let mut r = r.borrow_mut();
        if r.len() <= depth {
            r.resize(depth + 1, Vec::new());
        }
        r[depth].push(what.to_string());
    });
}

fn take_helper_refusals(depth: usize) -> Vec<String> {
    HELPER_REFUSED.with(|r| {
        r.borrow_mut()
            .get_mut(depth)
            .map(std::mem::take)
            .unwrap_or_default()
    })
}

// What the parent is told after a helper's own summary when some of its
// calls were refused: a summary can claim work that never happened.
fn helper_refusal_note(refused: &[String]) -> Option<String> {
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

fn set_helper_ctx(depth: usize, ctx: Option<HelperCtx>) {
    HELPER_CTX.with(|a| {
        let mut a = a.borrow_mut();
        if a.len() <= depth {
            a.resize(depth + 1, None);
        }
        a[depth] = ctx;
    });
}

fn helper_ctx(depth: usize) -> HelperCtx {
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
const DELEGATION_TOOLS: &[&str] = &["task", "spawn_subagent"];

// Whether a helper with the `tools` list `allowed` may call `name`.
fn agent_may_call(allowed: &[String], name: &str) -> bool {
    name == "finish" || (allowed.iter().any(|t| t == name) && !DELEGATION_TOOLS.contains(&name))
}

// The tool surface of an agent file with a `tools` list: those tools and
// finish, nothing else.
fn agent_defs_only(defs: Vec<tools::ToolDef>, agent: &config::AgentDef) -> Vec<tools::ToolDef> {
    match &agent.tools {
        Some(allowed) => defs
            .into_iter()
            .filter(|d| agent_may_call(allowed, d.name))
            .collect(),
        None => defs,
    }
}

// Refusal for a call outside an agent file's `tools` list.
fn agent_tool_refusal(agent: Option<&config::AgentDef>, name: &str) -> Option<String> {
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
fn read_only_helper_refusal(name: &str, input: &serde_json::Value, cwd: &Path) -> Option<String> {
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
fn agent_system(agent: &config::AgentDef) -> String {
    format!(
        "You are the `{}` helper: {}\n\n{}\n\nWork only on the task you are given. \
         When it is complete, call the finish tool with a one-paragraph summary.",
        agent.name, agent.description, agent.prompt
    )
}

static SUB_SEQ: AtomicUsize = AtomicUsize::new(0);

/// How many helpers from one reply run at once when not set (`max_parallel_helpers`).
pub const DEFAULT_MAX_PARALLEL_HELPERS: usize = 3;
static MAX_PARALLEL_HELPERS: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_PARALLEL_HELPERS);

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
struct HelperSpec {
    input: serde_json::Value,
    task: String,
    role: String,
    agent: Option<config::AgentDef>,
    read_only: bool,
    isolate: bool,
}

// A helper ready to start: where it runs, and the worktree it got.
struct Helper {
    spec: HelperSpec,
    run_cwd: PathBuf,
    note: String,
    worktree: Option<Worktree>,
}

impl Helper {
    // Read-only helpers and those in a worktree of their own touch nothing
    // another helper works on. A worktree does not cover folders added with
    // --add-dir, which every helper may write in. Under an editor (`bwn acp`)
    // helpers take turns: the editor nests each call inside the open one, so
    // two helpers' calls must not interleave.
    fn runs_beside_others(&self) -> bool {
        !report::has_sink()
            && (self.spec.read_only || (self.worktree.is_some() && crate::workdirs::count() == 0))
    }
}

// Checks a delegation call and resolves its role, without side effects. A
// role names a built-in prompt or an agent file (resolved from the
// session's folder, before any worktree). A helper is read-only when the
// call or its agent file says so, when its agent file lists only tools that
// change nothing, or when the session itself is read-only.
fn resolve_helper(
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
fn helper_reads_only(
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
fn place_helper(spec: HelperSpec, cwd: &Path) -> Helper {
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
fn run_helper(
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

fn spawn_subagent(
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
fn run_helpers(
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
fn queued_timeout_note((text, is_error): (String, bool), together: usize) -> (String, bool) {
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

const HELPER_NOT_STARTED: &str = "not started: the user stopped the turn";
const HELPER_STOPPED: &str = "stopped by the user before it finished";

// A helper call held back to start with the others from its reply: the
// results slot it fills and the tool name it was called by.
struct WaitingHelper {
    slot: usize,
    name: String,
    spec: HelperSpec,
}

// Starts the held helpers and puts each one's result in its slot, in call
// order. None start once the user has stopped the turn.
fn start_waiting_helpers(
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
struct HelperDone {
    index: usize,
    result: (String, bool),
    lines: Vec<String>,
    secs: f64,
    // Esc or Ctrl+C reached it.
    interrupted: bool,
    // The person cancelled a prompt it asked.
    stopped: bool,
}

// What helpers running side by side show outside their own block: an
// approval or a question one of them asks, and each finished block. Taken
// for each, so neither lands in the middle of the other.
static HELPER_SCREEN: Mutex<()> = Mutex::new(());

fn helper_screen() -> std::sync::MutexGuard<'static, ()> {
    HELPER_SCREEN.lock().unwrap_or_else(|e| e.into_inner())
}

// Runs `group` on worker threads, at most `max_parallel_helpers` at once.
// Each helper's output is held back and shown, labelled, when it finishes;
// Esc or Ctrl+C reaches every running helper (none of them clears it) and
// the queued ones never start. Tokens and cost add up in the session's
// usage ledger like any other request.
fn run_side_by_side(
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
fn show_helper_block(h: &Helper, index: usize, of: usize, done: &HelperDone) {
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
fn helper_prompt<T>(ask: impl FnOnce() -> T, stopped: impl FnOnce() -> T) -> T {
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

struct Worktree {
    path: PathBuf,
    branch: String,
    base: String,
}

// Making and closing helpers' worktrees, one at a time: they share the
// repository's refs and worktree list, and helpers in worktrees of their own
// run at the same time.
static WORKTREE_GIT: Mutex<()> = Mutex::new(());

// A worktree on a new branch from HEAD, or why there cannot be one.
fn make_worktree(cwd: &Path) -> Result<Worktree, String> {
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
struct WorktreeDone {
    note: String,
    screen: String,
    branch: Option<String>,
    commits: usize,
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
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
fn finish_worktree(cwd: &Path, wt: &Worktree) -> WorktreeDone {
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
            format!("Inspect workspace and requirements for: {task}"),
            format!("Create and edit target source files to build: {task}"),
            format!("Verify execution and run checks/tests for: {task}"),
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

// Workspace evidence: a path-ish token or a noun that names something living
// in the repo. An imperative verb alone is not enough to demand tool calls —
// "write a poem about pirates" is a chat deliverable, and prose IS the work.
fn task_references_workspace(task: &str) -> bool {
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
fn task_is_plannable(task: &str) -> bool {
    task_is_imperative(task) && task_references_workspace(task)
}

// Fix 14 decision: nudge the model to act instead of explaining. Fires only
// for imperative tasks that reference the workspace (a file, path, project,
// bug, …), only before any mutating tool has run this session, and only while
// the per-task cap has not been reached. A prose reply with no tool activity
// at all, or one that reads as how-to instructions, is treated as explaining
// rather than doing. Creative/chat asks ("write a poem about pirates") never
// nudge — the prose answer is the deliverable, not a failure to act.
fn should_nudge_to_act(
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
// `auto_approve` (headless `--yes`) skips the approval selector and executes
// the plan as soon as it is produced.
pub fn run_plan(
    p: &Provider,
    perm: Permission,
    task: &str,
    cwd: &Path,
    auto_approve: bool,
    images: Vec<(String, String)>,
) -> Result<(), String> {
    let mut transcript = Vec::new();
    plan_turn(
        p,
        perm,
        task,
        cwd,
        auto_approve,
        images,
        &mut transcript,
        &crate::session::claim_or_new(),
    )
    .map(|_| ())
}

/// How a PLAN turn ended. The REPL leaves PLAN only when the person chose to
/// execute the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanEnd {
    /// The approved plan ran in BUILD.
    Executed,
    /// Cancel or Esc at the plan selector.
    Cancelled,
    /// No plan was made: a plain answer, or the budget stopped the turn.
    Answered,
}

#[allow(clippy::too_many_arguments)]
pub fn plan_turn(
    p: &Provider,
    perm: Permission,
    task: &str,
    cwd: &Path,
    auto_approve: bool,
    images: Vec<(String, String)>,
    transcript: &mut Vec<Msg>,
    sid: &str,
) -> Result<PlanEnd, String> {
    crate::session::set_current(sid);
    let running = AgentRunningGuard::new();
    // Hooks see the phase's real gate, not the session permission.
    hooks::set_permission_mode("plan");
    // Planning reads the whole conversation under the PLAN prompt.
    let mut msgs = transcript.clone();
    use_system(&mut msgs, PLAN_HEAD, || {
        let prefix = context_prefix(cwd, p.context_tokens);
        let sys = format!(
        "You are bwn in PLAN mode: an expert software architect and technical lead working out an implementation plan with the user. \
        You have full read access to inspect the codebase as needed. Do not write files, edit files, apply patches, spawn subagents, or run mutating shell commands while planning. \
        When the user gives you a high-level or underspecified task (or when key choices like tech stack, exit conditions, or edge case handling are ambiguous): \
        1. Interrogate the user to resolve design decisions, edge cases, tech stack choices, and requirements. Use the `question` tool or ask clear targeted questions before finalizing the plan. \
        2. Once requirements and edge cases are clear, call exit_plan or ExitPlanMode with a concise 3-to-5 step implementation plan describing concrete human implementation steps (e.g. 1. Create main.py with stdin loop 2. Handle Ctrl+C and exit keywords 3. Verify with python main.py). \
        Do not list raw tool names (like read_file or write_file) as plan steps. \
        If the user makes small talk or greets you, reply naturally without forcing a plan.\n\n{prefix}"
    );
        sys
    });
    msgs.push(user_msg(task.into(), images.clone()));
    let drafted = draft_plan(p, cwd, task, &mut msgs)?;

    // The planning response is complete (a plan, or a natural reply). A plan
    // waits for the user's approval, so a Stop hook cannot send it on.
    if hooks::stop("Stop", cwd, serde_json::json!({"stop_hook_active": false})).is_some() {
        report::info("  (a Stop hook asked to continue; a plan waits for your approval instead)");
    }
    turn_done(cwd);
    let plan_text = match drafted {
        Draft::Plan(text) => text,
        Draft::Answer(text) => {
            record_plan_exchange(transcript, &msgs, task, images, &text);
            crate::session::save(sid, cwd, &p.model, transcript);
            return Ok(PlanEnd::Answered);
        }
        Draft::Stopped => return Ok(PlanEnd::Answered),
    };
    let mut plan_text = plan_text;
    let mut steps = parse_plan_steps(&plan_text);
    if !plan_steps_are_actionable(&steps) {
        return Err(
            "planning did not produce an actionable numbered or bulleted plan after recovery attempts"
                .into(),
        );
    }
    report::plan(&steps);
    // The conversation keeps the question and the plan; the planning tool
    // rounds stay out of it.
    record_plan_exchange(transcript, &msgs, task, images, &plan_text);
    crate::session::save(sid, cwd, &p.model, transcript);

    if auto_approve {
        report::info("  ✓ plan auto-approved (--yes) — executing");
    } else if let Some(ask) = REMOTE_APPROVER.get() {
        match ask(RemoteAsk::Plan { steps: &steps }) {
            RemoteAnswer::Once | RemoteAnswer::Always => {}
            RemoteAnswer::Reject | RemoteAnswer::Cancelled => return Ok(PlanEnd::Cancelled),
        }
    } else {
        let items = vec![
            tui::SelectItem {
                label: "Execute Plan".into(),
                detail: "Switch to BUILD mode and start implementing".into(),
            },
            tui::SelectItem {
                label: "Edit Step".into(),
                detail: "Modify one of the plan steps".into(),
            },
            tui::SelectItem {
                label: "Cancel".into(),
                detail: "Keep the plan in the conversation without executing".into(),
            },
            // Last, so the 0.14 positions (and ↓↓ Enter to cancel) still hold.
            tui::SelectItem {
                label: "Revise Plan".into(),
                detail: "Say what to change and get a revised plan".into(),
            },
        ];
        loop {
            show_plan(&steps, items.len());
            match tui::select_item("Approve Plan", &items) {
                Some(0) => break, // Execute Plan: explicitly selected
                Some(3) => {
                    let Some(feedback) = tui::ask("  what should change? ")
                        .map(|f| f.trim().to_string())
                        .filter(|f| !f.is_empty())
                    else {
                        continue;
                    };
                    let current = numbered_plan(&steps);
                    push_revision(&mut msgs, &current, &feedback);
                    match draft_plan(p, cwd, task, &mut msgs)? {
                        Draft::Plan(text)
                            if plan_steps_are_actionable(&parse_plan_steps(&text)) =>
                        {
                            plan_text = text;
                            steps = parse_plan_steps(&plan_text);
                            report::plan(&steps);
                        }
                        Draft::Stopped => return Ok(PlanEnd::Answered),
                        Draft::Plan(text) | Draft::Answer(text) => plan_text = text,
                    }
                    transcript.push(Msg::User(revision_request(&feedback)));
                    transcript.push(Msg::Assistant {
                        text: plan_text.clone(),
                        calls: vec![],
                    });
                    crate::session::save(sid, cwd, &p.model, transcript);
                }
                Some(1) => {
                    // The picker numbers the rows: a digit moves to that
                    // step, Enter opens it with its text to edit.
                    let step_items: Vec<tui::SelectItem> = steps
                        .iter()
                        .map(|s| tui::SelectItem {
                            label: s.clone(),
                            detail: "edit this step".into(),
                        })
                        .collect();
                    if let Some(idx) = tui::select_item("Select Step to Edit", &step_items) {
                        let prompt = format!("  edit step {}: ", idx + 1);
                        if let Some(new_text) = tui::ask_prefilled(&prompt, &steps[idx]) {
                            if !new_text.trim().is_empty() {
                                steps[idx] = new_text.trim().to_string();
                            }
                        }
                    }
                }
                _ => {
                    tui::line(&tui::yellow(
                        "  cancelled — still in PLAN; say what to change for a revised plan",
                    ));
                    return Ok(PlanEnd::Cancelled);
                }
            }
        }
    }

    let full = approved_plan_build_task(task, &numbered_plan(&steps));
    drop(running);
    run_build_session_with_images(p, perm, "engineer", &full, cwd, transcript, sid, Vec::new())
        .map(|_| PlanEnd::Executed)
}

fn numbered_plan(steps: &[String]) -> String {
    steps
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}. {}", i + 1, s))
        .collect::<Vec<_>>()
        .join("\n")
}

// The plan above the approval selector. The selector draws over the rows
// just above the composer, so blank rows keep the plan in view under it.
fn show_plan(steps: &[String], selector_items: usize) {
    tui::line("");
    tui::line(&tui::accent("  Plan"));
    // Steps are the model's plan text.
    for (i, s) in steps.iter().enumerate() {
        tui::line(&format!("  {}. {}", i + 1, tui::sanitize_terminal(s)));
    }
    tui::line("");
    if tui::is_raw() {
        for _ in 0..selector_items + 2 {
            tui::line("");
        }
    }
}

fn revision_request(feedback: &str) -> String {
    format!(
        "Revise the plan: {feedback}\n\
         Keep the steps that still apply and give the complete revised plan."
    )
}

// Continues the planning conversation with the plan as it stands (including
// any step edits) and the person's feedback, so the next draft builds on it.
fn push_revision(msgs: &mut Vec<Msg>, current_plan: &str, feedback: &str) {
    msgs.push(Msg::Assistant {
        text: current_plan.to_string(),
        calls: vec![],
    });
    msgs.push(Msg::User(revision_request(feedback)));
}

// What a planning loop produced.
enum Draft {
    Plan(String),
    /// A plain reply to a question or small talk, already shown.
    Answer(String),
    /// The budget stopped the turn.
    Stopped,
}

// Appends the PLAN question and its answer (the plan text) to the shared
// conversation, starting it with the PLAN prompt when it was empty.
fn record_plan_exchange(
    transcript: &mut Vec<Msg>,
    planning: &[Msg],
    task: &str,
    images: Vec<(String, String)>,
    answer: &str,
) {
    if transcript.is_empty() {
        if let Some(sys @ Msg::System(_)) = planning.first() {
            transcript.push(sys.clone());
        }
    }
    transcript.push(user_msg(task.to_string(), images));
    transcript.push(Msg::Assistant {
        text: answer.to_string(),
        calls: vec![],
    });
}

// The planning loop: read-only tool rounds until the model produces a plan
// (or a plain answer).
fn draft_plan(p: &Provider, cwd: &Path, task: &str, msgs: &mut Vec<Msg>) -> Result<Draft, String> {
    let defs = offered(p, tools::defs_readonly()); // planning inspects context but never writes
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
        if budget_exhausted() {
            return Ok(Draft::Stopped);
        }
        maybe_compact(p, msgs);
        let reply = request_reply(p, msgs, &defs, "planning")?;
        let reply = normalize_text_tool_calls(reply, &defs, task);

        if reply.calls.is_empty() {
            let candidate_steps = parse_plan_steps(&reply.text);
            // Non-plannable input — a greeting, a joke, "who are you", small talk.
            // The model answered naturally instead of planning, and that reply has
            // already streamed to the user. Accept it and end the turn. Forcing a
            // plan or a canned refusal here is exactly the personality failure we
            // want gone. (A real task with junk output still falls through to the
            // recovery path below, because task_is_plannable stays true for it.)
            if !plan_steps_are_actionable(&candidate_steps) && !task_is_plannable(task) {
                return Ok(Draft::Answer(reply.text));
            }
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
                    images: Vec::new(),
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
                        images: Vec::new(),
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
                    images: Vec::new(),
                });
                continue;
            }
            let reason = hook_gate(Permission::ReadOnly, &call.name, &call_input, cwd).map(|r| {
                phase_readonly_reason(
                    r,
                    "planning is read-only: changes run in BUILD after you approve the plan",
                )
            });
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
                    images: Vec::new(),
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
                        images: Vec::new(),
                    });
                    continue;
                }
            }
            let out = vision_checked(p, tools::run(&call.name, &call_input, cwd));
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
                images: out.images,
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
    Ok(Draft::Plan(plan_text))
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

// ── BRAINSTORM mode ───────────────────────────────────────────────────────────
// Brainstorm is conversational with read-only tool access: the model can grep,
// read files, fetch URLs, and run read-only commands when the conversation
// calls for it, but never writes — exactly like PLAN. It also has a
// mode-transition sensor: if it detects the user wants to build or plan, it
// suggests switching, and the person decides.

/// One headless BRAINSTORM turn, saved as its own session.
pub fn run_brainstorm(
    p: &Provider,
    // Unused: BRAINSTORM gates as read-only whatever the session permission.
    _perm: Permission,
    cwd: &Path,
    first: &str,
    images: Vec<(String, String)>,
) -> Result<Option<ModeHint>, String> {
    let mut transcript = Vec::new();
    brainstorm_turn(
        p,
        cwd,
        first,
        images,
        &mut transcript,
        &crate::session::claim_or_new(),
    )
}

/// One BRAINSTORM turn on the session's conversation. The REPL's composer is
/// the follow-up prompt; this returns after the reply, saved, with the mode
/// the person chose when the model suggested switching.
pub fn brainstorm_turn(
    p: &Provider,
    cwd: &Path,
    question: &str,
    images: Vec<(String, String)>,
    msgs: &mut Vec<Msg>,
    sid: &str,
) -> Result<Option<ModeHint>, String> {
    crate::session::set_current(sid);
    let running = AgentRunningGuard::new();
    hooks::set_permission_mode("readonly");
    // Role identity + mode contract come first; environment sections follow.
    use_system(msgs, BRAINSTORM_HEAD, || {
        let prefix = context_prefix(cwd, p.context_tokens);
        format!("You are a sharp, concise thought partner with read access to the codebase and the internet. \
        Use tools freely to look things up, read files, grep for patterns, or run read-only commands — \
        whatever helps the conversation. \
        This mode is read-only: do not write or edit files, apply patches, spawn subagents, or run mutating shell commands. \
        If the user wants changes made, say so and suggest switching modes. \
        When you think the user is ready to stop discussing and start building or planning, \
        end your response with the exact token [SUGGEST:BUILD] or [SUGGEST:PLAN] on its own line. \
        Otherwise just respond naturally. No fluff.\n\n{prefix}")
    });
    msgs.push(user_msg(question.to_string(), images));
    let r = brainstorm_reply(p, cwd, question, msgs);
    if r.is_err() {
        drop_refused_message(p, msgs);
    }
    // The reply is complete — the agent stopped responding for this turn.
    let r = after_stop_hooks(
        "Stop",
        cwd,
        |_| serde_json::json!({}),
        r,
        |reason| {
            msgs.push(Msg::User(reason.to_string()));
            let r = brainstorm_reply(p, cwd, reason, msgs);
            if r.is_err() {
                drop_refused_message(p, msgs);
            }
            r
        },
    );
    turn_done(cwd);
    crate::session::save(sid, cwd, &p.model, msgs);
    drop(running);
    let reply_text = match r {
        Ok(t) => t,
        Err(e) if interrupted_turn(&e) => return Ok(None),
        Err(e) => return Err(e),
    };

    // A suggestion to switch modes needs someone to answer it: never in
    // --json output or without a terminal.
    let hint = if reply_text.contains("[SUGGEST:BUILD]") {
        Some(ModeHint::Build)
    } else if reply_text.contains("[SUGGEST:PLAN]") {
        Some(ModeHint::Plan)
    } else {
        None
    };
    let Some(h) = hint else {
        return Ok(None);
    };
    if report::is_json() || !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    tui::line("");
    let suggestion = match h {
        ModeHint::Build => "switch to BUILD mode and implement this?",
        ModeHint::Plan => "switch to PLAN mode and break this down?",
    };
    tui::line(&tui::yellow(&format!("  ↪ AI suggests: {suggestion}")));
    tui::line(&tui::dim("  (y to switch, anything else to keep chatting)"));
    let ans = tui::ask("  ").unwrap_or_default();
    if matches!(ans.trim().to_lowercase().as_str(), "y" | "yes") {
        return Ok(Some(h));
    }
    Ok(None)
}

// Consumes tool calls until the model gives a text answer, which it returns
// (already pushed to `msgs`).
fn brainstorm_reply(
    p: &Provider,
    cwd: &Path,
    question: &str,
    msgs: &mut Vec<Msg>,
) -> Result<String, String> {
    let defs = offered(p, tools::defs_readonly()); // brainstorm inspects but never writes
    let mut loop_guard = ToolLoopGuard::default();
    maybe_compact(p, msgs);
    let mut tool_rounds = 0usize;
    loop {
        tool_rounds += 1;
        if tool_rounds > MAX_CHAT_TOOL_ROUNDS {
            let text = format!(
                "I stopped after {MAX_CHAT_TOOL_ROUNDS} tool rounds without a final response."
            );
            report::assistant(&text);
            msgs.push(Msg::Assistant {
                text: text.clone(),
                calls: vec![],
            });
            return Ok(text);
        }
        if budget_exhausted() {
            return Ok(String::new());
        }
        tui::line("");
        let reply = request_reply(p, msgs, &defs, "thinking")?;
        let reply = normalize_text_tool_calls(reply, &defs, question);

        if reply.calls.is_empty() {
            msgs.push(Msg::Assistant {
                text: reply.text.clone(),
                calls: vec![],
            });
            if !report::is_json() {
                tui::context_meter(context_used(msgs), p.context_tokens);
            }
            return Ok(reply.text);
        }

        // Execute tool calls inline.
        let mut results = Vec::new();
        let mut loop_summary: Option<String> = None;
        let mut loop_nudge: Option<String> = None;
        let mut finished: Option<String> = None;
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
                    images: Vec::new(),
                });
                continue;
            }
            if let Some((shown, told)) = text_only_refusal(p, &call.name).or_else(|| {
                unoffered_call(&call.name, &defs, true).map(|r| {
                    read_only_refusal(&call.name, &call.input, Some(BRAINSTORM_READONLY), r)
                })
            }) {
                report::tool_denied(&shown);
                note_loop_result(
                    &mut loop_guard,
                    &call.name,
                    &call.input,
                    &told,
                    true,
                    &mut loop_nudge,
                    &mut loop_summary,
                );
                results.push(ToolResult {
                    id: call.id.clone(),
                    content: told,
                    is_error: true,
                    images: Vec::new(),
                });
                continue;
            }
            let call_input =
                tool_input_for_execution(&call.name, &call.input, cwd, "brainstorm", 0, question);
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
                    images: Vec::new(),
                });
                continue;
            }
            // Read-only regardless of the session gate, like run_plan.
            let reason = hook_gate(Permission::ReadOnly, &call.name, &call_input, cwd)
                .map(|r| phase_readonly_reason(r, BRAINSTORM_READONLY));
            if let Some(reason) = reason {
                report::tool_denied(&reason);
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
                    images: Vec::new(),
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
                        images: Vec::new(),
                    });
                    continue;
                }
            }
            let mut out = vision_checked(p, tools::run(&call.name, &call_input, cwd));
            post_tool_hooks(&call.name, &call_input, &mut out, cwd);
            report::tool_result(&call.name, &out.content, out.is_error);
            trace_tool_result(&call.name, &out.content, out.is_error, "brainstorm", 0);
            if out.finished {
                finished.get_or_insert_with(|| out.content.clone());
            }
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
                images: out.images,
            });
        }
        msgs.push(Msg::Assistant {
            text: assistant_tool_turn_text(reply.text, &reply.calls),
            calls: reply.calls,
        });
        msgs.push(Msg::Tool(results));
        if !report::is_json() {
            tui::context_meter(context_used(msgs), p.context_tokens);
            tui::poll_typeahead();
        }
        // finish is offered here too: its summary is the answer.
        if let Some(summary) = finished.or(loop_summary) {
            report::assistant(&summary);
            msgs.push(Msg::Assistant {
                text: summary.clone(),
                calls: vec![],
            });
            return Ok(summary);
        }
        if let Some(nudge) = loop_nudge {
            msgs.push(Msg::User(nudge));
        }
    }
}

const BRAINSTORM_READONLY: &str =
    "BRAINSTORM is read-only: switch to BUILD (Shift+Tab or /build) to make changes";
const PLAN_CHAT_READONLY: &str =
    "PLAN is read-only: switch to BUILD (Shift+Tab or /build) to make changes";

/// The mode a conversational turn was typed in. BUILD answers under the
/// session permission; PLAN and BRAINSTORM are read-only whatever it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatIn {
    Build,
    Plan,
    Brainstorm,
}

/// A conversational turn ("what does this project do?"), answered on the
/// session's conversation and saved with it.
#[allow(clippy::too_many_arguments)]
pub fn run_chat_turn(
    p: &Provider,
    perm: Permission,
    within: ChatIn,
    cwd: &Path,
    question: &str,
    images: Vec<(String, String)>,
    msgs: &mut Vec<Msg>,
    sid: &str,
) -> Result<(), String> {
    crate::session::set_current(sid);
    let _running_guard = AgentRunningGuard::new();
    let (perm, readonly) = match within {
        ChatIn::Build => (perm, None),
        ChatIn::Plan => (Permission::ReadOnly, Some(PLAN_CHAT_READONLY)),
        ChatIn::Brainstorm => (Permission::ReadOnly, Some(BRAINSTORM_READONLY)),
    };
    hooks::set_permission_mode(match within {
        ChatIn::Plan => "plan",
        _ => permission_name(perm),
    });
    // A chat question keeps whatever prompt the conversation already has;
    // only a fresh conversation gets the chat prompt.
    if msgs.is_empty() {
        let prefix = context_prefix(cwd, p.context_tokens);
        let sys = format!(
        "You are buildwithnexus in a coding terminal. Answer the user's current message naturally and concisely. \
        If the user asks a normal conversational question or greeting, answer in plain text and do not call tools. \
        If answering well requires inspecting the workspace or environment, use tools, then summarize the result. \
        Do not emit JSON unless a tool call is actually required by the tool protocol.\n\n{prefix}"
    );
        msgs.push(Msg::System(sys));
    }
    msgs.push(user_msg(question.to_string(), images));
    let r = chat_reply(p, perm, readonly, cwd, question, msgs);
    if r.is_err() {
        drop_refused_message(p, msgs);
    }
    let r = after_stop_hooks(
        "Stop",
        cwd,
        |_| serde_json::json!({}),
        r.map(|()| String::new()),
        |reason| {
            msgs.push(Msg::User(reason.to_string()));
            let r = chat_reply(p, perm, readonly, cwd, reason, msgs);
            if r.is_err() {
                drop_refused_message(p, msgs);
            }
            r.map(|()| String::new())
        },
    )
    .map(|_| ());
    turn_done(cwd);
    crate::session::save(sid, cwd, &p.model, msgs);
    match r {
        Err(e) if interrupted_turn(&e) => Ok(()),
        r => r,
    }
}

/// `/ask`: answers a side question with the conversation as context,
/// read-only, and leaves no trace: nothing is added to the transcript or
/// saved to the session.
pub fn ask_aside(
    p: &Provider,
    cwd: &Path,
    question: &str,
    transcript: &[Msg],
) -> Result<(), String> {
    let _running_guard = AgentRunningGuard::new();
    let mut msgs = transcript.to_vec();
    if msgs.is_empty() {
        let prefix = context_prefix(cwd, p.context_tokens);
        msgs.push(Msg::System(format!(
            "{CHAT_HEAD}. Answer the user's question naturally and concisely.\n\n{prefix}"
        )));
    }
    msgs.push(Msg::User(format!(
        "{question}\n\n(A side question: answer it briefly; do not change any files.)"
    )));
    match chat_reply(p, Permission::ReadOnly, None, cwd, question, &mut msgs) {
        Err(e) if interrupted_turn(&e) => Ok(()),
        r => r,
    }
}

// A question can still lead to an edit: what it left each file as is
// recorded, so /undo asks before overwriting a later hand edit. `readonly`
// names the read-only mode in a refusal.
fn chat_reply(
    p: &Provider,
    perm: Permission,
    readonly: Option<&str>,
    cwd: &Path,
    question: &str,
    msgs: &mut Vec<Msg>,
) -> Result<(), String> {
    let started = checkpoint::now_ms();
    let r = chat_rounds(p, perm, readonly, cwd, question, msgs);
    let _ = checkpoint::seal_since(cwd, started);
    r
}

fn chat_rounds(
    p: &Provider,
    perm: Permission,
    readonly: Option<&str>,
    cwd: &Path,
    question: &str,
    msgs: &mut Vec<Msg>,
) -> Result<(), String> {
    let defs = offered(
        p,
        if matches!(perm, Permission::ReadOnly) {
            tools::defs_readonly()
        } else {
            tools::defs_for_context(false, p.context_tokens)
        },
    );
    let mut loop_guard = ToolLoopGuard::default();

    for tool_round in 1..=MAX_CHAT_TOOL_ROUNDS {
        if budget_exhausted() {
            return Ok(());
        }
        maybe_compact(p, msgs);
        let reply = request_reply(p, msgs, &defs, "thinking")?;
        let reply = normalize_text_tool_calls(reply, &defs, question);

        if reply.calls.is_empty() {
            msgs.push(Msg::Assistant {
                text: reply.text.clone(),
                calls: vec![],
            });
            if !reply.text.trim().is_empty() && !report::is_json() {
                tui::context_meter(context_used(msgs), p.context_tokens);
            }
            return Ok(());
        }

        let mut results = Vec::new();
        let mut loop_summary: Option<String> = None;
        let mut loop_nudge: Option<String> = None;
        let mut finished: Option<String> = None;
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
                    images: Vec::new(),
                });
                continue;
            }

            if let Some((shown, told)) = text_only_refusal(p, &call.name).or_else(|| {
                unoffered_call(&call.name, &defs, true)
                    .map(|r| read_only_refusal(&call.name, &call.input, readonly, r))
            }) {
                report::tool_denied(&shown);
                note_loop_result(
                    &mut loop_guard,
                    &call.name,
                    &call.input,
                    &told,
                    true,
                    &mut loop_nudge,
                    &mut loop_summary,
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

            let reason = hook_gate(perm, &call.name, &call_input, cwd).map(|r| match readonly {
                Some(msg) => phase_readonly_reason(r, msg),
                None => r,
            });
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
                    images: Vec::new(),
                });
                continue;
            }

            let mut out = vision_checked(p, tools::run(&call.name, &call_input, cwd));
            post_tool_hooks(&call.name, &call_input, &mut out, cwd);
            report::tool_result(&call.name, &out.content, out.is_error);
            trace_tool_result(&call.name, &out.content, out.is_error, "chat", tool_round);
            if out.finished {
                finished.get_or_insert_with(|| out.content.clone());
            }
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
                images: out.images,
            });
        }

        msgs.push(Msg::Assistant {
            text: assistant_tool_turn_text(reply.text, &reply.calls),
            calls: reply.calls,
        });
        msgs.push(Msg::Tool(results));
        if !report::is_json() {
            tui::context_meter(context_used(msgs), p.context_tokens);
            tui::poll_typeahead();
        }
        if let Some(summary) = finished.or(loop_summary) {
            report::assistant(&summary);
            msgs.push(Msg::Assistant {
                text: summary,
                calls: vec![],
            });
            return Ok(());
        }
        if let Some(nudge) = loop_nudge {
            msgs.push(Msg::User(nudge));
        }
    }

    let text =
        format!("I stopped after {MAX_CHAT_TOOL_ROUNDS} tool rounds without a final response.");
    report::assistant(&text);
    msgs.push(Msg::Assistant {
        text,
        calls: vec![],
    });
    Ok(())
}

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
