//! BRAINSTORM mode: a conversation with read-only tools that never writes.

use super::*;

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
pub(super) fn brainstorm_reply(
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

pub(super) const BRAINSTORM_READONLY: &str =
    "BRAINSTORM is read-only: switch to BUILD (Shift+Tab or /build) to make changes";
pub(super) const PLAN_CHAT_READONLY: &str =
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
pub(super) fn chat_reply(
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

pub(super) fn chat_rounds(
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
