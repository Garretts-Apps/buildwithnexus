//! PLAN mode: drafting a plan, revising it with the user, and carrying it out.

use super::*;

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

pub(super) fn numbered_plan(steps: &[String]) -> String {
    steps
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}. {}", i + 1, s))
        .collect::<Vec<_>>()
        .join("\n")
}

// The plan above the approval selector. The selector draws over the rows
// just above the composer, so blank rows keep the plan in view under it.
pub(super) fn show_plan(steps: &[String], selector_items: usize) {
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

pub(super) fn revision_request(feedback: &str) -> String {
    format!(
        "Revise the plan: {feedback}\n\
         Keep the steps that still apply and give the complete revised plan."
    )
}

// Continues the planning conversation with the plan as it stands (including
// any step edits) and the person's feedback, so the next draft builds on it.
pub(super) fn push_revision(msgs: &mut Vec<Msg>, current_plan: &str, feedback: &str) {
    msgs.push(Msg::Assistant {
        text: current_plan.to_string(),
        calls: vec![],
    });
    msgs.push(Msg::User(revision_request(feedback)));
}

// What a planning loop produced.
pub(super) enum Draft {
    Plan(String),
    /// A plain reply to a question or small talk, already shown.
    Answer(String),
    /// The budget stopped the turn.
    Stopped,
}

// Appends the PLAN question and its answer (the plan text) to the shared
// conversation, starting it with the PLAN prompt when it was empty.
pub(super) fn record_plan_exchange(
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
pub(super) fn draft_plan(
    p: &Provider,
    cwd: &Path,
    task: &str,
    msgs: &mut Vec<Msg>,
) -> Result<Draft, String> {
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

pub(super) fn approved_plan_build_task(task: &str, plan: &str) -> String {
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
