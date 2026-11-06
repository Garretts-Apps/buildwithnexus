//! Context compaction: estimating a conversation's size and summarizing its middle when it outgrows the model's window.

use super::*;

// ── context compaction ────────────────────────────────────────────────────────
// The length of `v` serialized as compact JSON, without building the string:
// this runs on every step, over every tool input in the conversation.
pub(super) fn json_len(v: &serde_json::Value) -> usize {
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

pub(super) fn estimate_tokens(msgs: &[Msg]) -> usize {
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

pub(super) fn compaction_split(msgs: &[Msg]) -> (usize, usize) {
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
pub(super) const ORIGINAL_TASK_MARKER: &str = "[Original task]\n";

pub(super) fn original_task_text(msgs: &[Msg]) -> Option<String> {
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

pub(super) fn structural_summary(middle: &[Msg]) -> String {
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

pub(super) fn assistant_tool_turn_text(text: String, calls: &[provider::ToolCall]) -> String {
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

pub(super) fn render_msgs(msgs: &[Msg]) -> String {
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
pub(super) fn compact_with(msgs: Vec<Msg>, summarize: impl FnOnce(&[Msg]) -> String) -> Vec<Msg> {
    compact_within(msgs, usize::MAX, summarize)
}

// `compact_with` for a transcript that must come out under `budget`
// estimated tokens: carried images are dropped, oldest first, until it does.
pub(super) fn compact_within(
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
pub(super) const MAX_KEPT_IMAGES: usize = 4;

pub(super) fn images_to_keep(middle: &[Msg], room: usize) -> (Vec<(String, String)>, usize) {
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
pub(super) fn truncate_tool_results(msgs: Vec<Msg>) -> Vec<Msg> {
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

pub(super) fn model_summary(p: &Provider, middle: &[Msg]) -> String {
    let sys = "Summarize this AI coding-agent conversation into a terse brief that preserves the task, key findings, decisions, files changed, and the current state. Drop pleasantries; use compact bullet points.";
    let q = vec![Msg::System(sys.into()), Msg::User(render_msgs(middle))];
    match complete(p, &q, &[]) {
        Ok(r) if !r.text.trim().is_empty() => r.text,
        _ => structural_summary(middle),
    }
}

pub(super) fn maybe_compact(p: &Provider, msgs: &mut Vec<Msg>) {
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
pub(super) fn compaction_budget(p: &Provider) -> usize {
    match p.context_tokens.saturating_mul(8) / 10 {
        0 => usize::MAX,
        n => n,
    }
}

#[derive(Default)]
pub(super) struct ToolLoopGuard {
    pub(super) seen: HashMap<String, ToolLoopRecord>,
}

pub(super) struct ToolLoopRecord {
    pub(super) content: String,
    pub(super) count: usize,
    pub(super) nudged: bool,
}

// What the guard wants the caller to do about a detected loop.
pub(super) enum LoopSignal {
    /// First trigger: steer the model with a corrective user turn and continue.
    Nudge(String),
    /// The loop repeated even after a nudge: stop the run and say so honestly.
    Stop(String),
}

impl ToolLoopGuard {
    pub(super) fn note(
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

pub(super) fn repeated_tool_summary(
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

pub(super) fn note_loop_result(
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

pub(super) fn tool_input_for_execution(
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

pub(super) fn repair_placeholder_tool_input(
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

pub(super) fn repair_write_file_input(
    input: &serde_json::Value,
    task: &str,
) -> Option<serde_json::Value> {
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
pub(super) fn is_placeholder_content(content: &str) -> bool {
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

pub(super) fn should_repair_filesystem_root(path: &str, task: &str) -> bool {
    path.trim() == "/"
        && task_targets_current_workspace(task)
        && !task_explicitly_targets_root(task)
}

pub(super) fn task_targets_current_workspace(task: &str) -> bool {
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

pub(super) fn task_explicitly_targets_root(task: &str) -> bool {
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

pub(super) fn is_placeholder_path(path: &str) -> bool {
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
pub(super) struct ThinkingStream {
    pub(super) step: usize,
    // Current line, buffered until its newline so markdown renders whole —
    // a half-streamed span like `**bo` can't be rendered, and re-rendering a
    // partial line already on screen would mean erase-and-repaint (flicker).
    pub(super) line: String,
}

impl ThinkingStream {
    pub(super) fn push(&mut self, chunk: &str) {
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
    pub(super) fn flush_line(&mut self) {
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

    pub(super) fn finish(&mut self) {
        self.flush_line();
        tui::render_queued_composer();
    }
}

// Meter input after a round-trip: the size the server measured for the last
// request when it reported usage, else the chars/4 estimate.
pub(super) fn context_used(msgs: &[Msg]) -> usize {
    crate::usage::last_context_tokens().unwrap_or_else(|| estimate_tokens(msgs))
}

// `--max-budget-usd` guard, checked before each model request so a session
// stops between requests rather than mid-stream. Prints the notice (a
// `notice` event in --json mode) and reports whether the loop must stop.
pub(super) fn budget_exhausted() -> bool {
    match crate::usage::budget_stop() {
        Some(msg) => {
            report::notice(&msg);
            stopped_short(Outcome::BudgetStop);
            true
        }
        None => false,
    }
}

pub(super) fn request_reply(
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
pub(super) fn reply_truncated_at_token_limit(reply: &Reply) -> bool {
    reply.stop_reason.as_deref() == Some("max_tokens")
}

// Tag forms local models use to emit tool calls as text (see
// extract_json_tool_candidate). Shared so the malformed-markup detector and
// the parser can never drift apart.
pub(super) const TOOL_CALL_TAGS: [&str; 4] =
    ["<tools>", "<tool_call>", "<function_call>", "<function>"];

// A reply that produced no calls but still carries tool-call markup is a
// failed tool call, not a final answer — the tagged JSON was malformed or cut
// off before the parser could extract it. Detecting it lets the loop ask for
// a re-emit instead of presenting broken markup to the user as the answer.
pub(super) fn malformed_tool_markup(reply: &Reply) -> bool {
    reply.calls.is_empty() && TOOL_CALL_TAGS.iter().any(|t| reply.text.contains(t))
}

pub(super) static TEXT_TOOL_SEQ: AtomicUsize = AtomicUsize::new(0);

pub(super) fn normalize_text_tool_calls(
    mut reply: Reply,
    defs: &[tools::ToolDef],
    user_text: &str,
) -> Reply {
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

pub(super) fn is_casual_turn(text: &str) -> bool {
    let normalized = text
        .trim()
        .trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace())
        .to_ascii_lowercase();
    // Only pure greetings: acknowledgements like "ok"/"thanks" are often valid
    // "yes, proceed" turns and must not have their tool calls discarded.
    matches!(normalized.as_str(), "hi" | "hello" | "hey")
}

pub(super) fn parse_text_tool_calls(
    text: &str,
    defs: &[tools::ToolDef],
) -> Option<Vec<provider::ToolCall>> {
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

pub(super) fn parse_prose_tool_call(
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
pub(super) fn positional_params(name: &str) -> &'static [&'static str] {
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
pub(super) fn parse_tool_code_call(
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
pub(super) fn tool_code_fence(text: &str) -> Option<&str> {
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
pub(super) fn whole_reply_call<'a>(
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
pub(super) fn find_tool_call<'a>(
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

pub(super) fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

// Given the slice just after an opening `(`, return the argument text up to the
// matching `)`, tracking string literals (triple/single/double) and nesting.
pub(super) fn balanced_parens(s: &str) -> Option<&str> {
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
pub(super) fn string_literal_len(s: &str) -> Option<usize> {
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
pub(super) fn parse_python_args(inside: &str) -> Vec<(Option<String>, serde_json::Value)> {
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
pub(super) fn kw_eq_pos(p: &str) -> Option<usize> {
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
pub(super) fn parse_py_value(expr: &str) -> serde_json::Value {
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
pub(super) fn tool_code_args_to_input(
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
pub(super) enum Placement {
    Reply,
    Leading,
    Quoted,
}

pub(super) fn extract_json_tool_candidate(text: &str) -> Option<(&str, Placement)> {
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
pub(super) fn offset_in(whole: &str, part: &str) -> usize {
    part.as_ptr() as usize - whole.as_ptr() as usize
}

// Whether a call at text[start..end] is the reply's own call rather than one
// it quotes. It opens the reply ("{…} I will read the file first."), or ends
// it after one short lead-in line ("Sure, reading it now. {…}"). A call with
// prose after it, or after a paragraph or an "for example"/"you could", is
// the model describing a call, and running it would act on an example.
pub(super) fn placed_as_call(text: &str, start: usize, end: usize) -> bool {
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
pub(super) fn balanced_json_object(text: &str) -> Option<&str> {
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

pub(super) fn collect_text_tool_calls(
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

pub(super) fn is_tool_definition(
    obj: &serde_json::Map<String, serde_json::Value>,
    args: &serde_json::Value,
) -> bool {
    obj.contains_key("description") || (args["type"] == "object" && args["properties"].is_object())
}

pub(super) fn text_call_args(v: &serde_json::Value) -> serde_json::Value {
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
pub(super) fn looks_like_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && (name.contains('_') || name.chars().all(|c| !c.is_ascii_uppercase()))
}

pub(super) fn text_tool_call(name: &str, input: serde_json::Value) -> provider::ToolCall {
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
pub(super) fn answer_input_prompt(default: &str) -> String {
    if default.is_empty() {
        "  Answer: ".to_string()
    } else {
        let default = tui::sanitize_terminal(default);
        format!("  Answer {}: ", tui::dim(&format!("[{default}]")))
    }
}

pub(super) fn answer_question(input: &serde_json::Value) -> (String, bool) {
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
pub(super) fn answer_with(
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
pub(super) fn question_answer(ans: String, default: &str) -> String {
    if ans.trim().is_empty() && !default.is_empty() {
        default.to_string()
    } else {
        ans
    }
}
