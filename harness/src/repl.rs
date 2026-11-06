//! The interactive session: the REPL loop, sessions, models, local servers and the knowledge-base index.

use super::*;

// ── REPL ──────────────────────────────────────────────────────────────────────
pub(super) fn repl(
    mut provider: Provider,
    mut perm: Permission,
    cwd: &std::path::Path,
    raw: bool,
    initial_prompt: Option<String>,
) -> Result<(), String> {
    let settings = config::load_settings().unwrap_or_default();
    tui::configure_ui(&settings.images, &settings.notify);
    let idle = settings.idle_notify_secs.unwrap_or(60);
    if idle > 0 {
        let dir = cwd.to_path_buf();
        tui::on_idle(std::time::Duration::from_secs(idle), move || {
            hooks::notification("idle_prompt", "waiting for your input", &dir)
        });
    }
    tui::set_permission_mode(permission_label(&perm));
    tui::set_model_label(&provider.model);

    // Show the full-screen header banner.
    let mode_name = "BRAINSTORM"; // default starting mode
    tui::show_banner(
        &onboarding::provider_label(
            &active_preset().unwrap_or(settings.provider.clone()),
            &provider.base_url,
        ),
        &provider.model,
        mode_name,
        &cwd.display().to_string(),
    );
    tui::line(&tui::dim(
        "  describe a task · /help for all commands · !<cmd> for shell · Shift+Tab to change mode",
    ));
    tui::line(&tui::dim(&format!("  {}", startup_tip())));
    if let Some(problem) = provider::startup_problem(&provider) {
        report::notice(&format!("  ⚠ {problem}"));
    }
    // Skill names and paths come from files in the checkout.
    for note in config::startup_context_notices(cwd) {
        let note = tui::sanitize_terminal(&note);
        tui::line(&tui::dim(&format!("  {note}")));
    }
    if let Some(n) = config::untrusted_extensions_notice(cwd) {
        report::notice(&format!("  {}", tui::sanitize_terminal(&n)));
    }
    for n in workdirs::notices(cwd) {
        report::notice(&format!("  {n}"));
    }
    review_repo_instructions(cwd);
    if let Some(n) = config::custom_key_move_notice() {
        report::notice(&format!("  {n}"));
    }
    if let Some(n) = agent::ignored_approvals_notice_once(cwd) {
        report::notice(&format!("  {n}"));
    }
    for issue in hooks::take_startup_issues() {
        tui::line(&tui::yellow(&format!("  [hook] ⚠ {issue}")));
    }
    let restored = workflow::restore();
    workflow::set_max_concurrent(settings.max_concurrent_workflows);
    // Background scheduler: due workflows start while the user is idle at the
    // prompt; their completion notices are shown at the next prompt.
    workflow::start_scheduler();
    // Once per set of restored workflows, not at every launch they wait.
    if restored > 0
        && !config::notice_seen("restored-workflows", &workflow::pending_tasks().join("\n"))
    {
        tui::line(&tui::green(&format!(
            "  ⟳ restored {restored} scheduled workflow{} from the previous session — /workflows to manage",
            crate::report::plural(restored)
        )));
    }
    if let Some(notice) = update::startup_notice(&settings.auto_update) {
        tui::line(&tui::dim(&notice));
    }
    // Off the critical path: five `which` probes cost real startup latency,
    // and with interactive=false this only prints when something is missing.
    std::thread::spawn(check_and_offer_install_dependencies);
    update::spawn_check(&settings.auto_update);

    // The REPL owns the id SessionStart already announced: a fresh one, or
    // the session `bwn continue` / `bwn resume <id>` asked to open.
    let (mut transcript, mut sid) = match session::take_resume_on_start() {
        Some(s) => {
            show_resumed(&s, cwd);
            (s.msgs, s.id)
        }
        None => (Vec::new(), session::current_or_new()),
    };
    session::set_current(&sid);
    trace::set_session(&sid);
    let mut mode = Mode::Brainstorm;
    let mut last_suggested_mode: Option<&'static str> = None;
    // /btw: extra context injected into the next task without interrupting.
    let mut btw_ctx: Option<String> = None;
    let mut pending_prompt = initial_prompt;
    // The workflow count the queue line last showed.
    let mut shown_active = 0usize;
    // Where each prompt of this run started, for /rewind.
    let mut rewind_points: Vec<RewindPoint> = Vec::new();

    loop {
        // Tick background workflows and surface any completion notifications
        // (both those queued by the scheduler thread and this tick's own).
        // Color by outcome — a "✗ workflow failed" line must not render green.
        // Background runs take this session's permission and model.
        workflow::update_live(
            agent::permission_name(perm),
            &provider.model,
            &provider.base_url,
        );
        let mut notes = workflow::take_notices();
        notes.extend(workflow::tick());
        for note in notes {
            if note.contains('✗') {
                tui::line(&tui::red(&note));
            } else if note.contains('⚠') {
                tui::line(&tui::yellow(&note));
            } else {
                tui::line(&tui::green(&note));
            }
        }
        // Prune old done/cancelled workflows, keep last 20.
        workflow::prune(20);

        // MCP servers connect in the background after the first prompt; their
        // one-line outcomes surface here so they never interleave with a turn.
        for (msg, ok) in mcp::drain_notices() {
            // Notices carry server names and error text from the server.
            let msg = tui::sanitize_terminal(&msg);
            if ok {
                tui::line(&tui::dim(&format!("  {msg}")));
            } else {
                tui::line(&tui::yellow(&format!("  {msg}")));
            }
        }

        // Workflow activity badge, when the number pending/running changes
        // (not again after every command).
        let active = workflow::active_count();
        if active > 0 && active != shown_active {
            tui::line(&tui::dim(&format!(
                "  ⟳ {} workflow{} in queue — /workflows to manage",
                active,
                crate::report::plural(active)
            )));
        }
        shown_active = active;

        let mut task = if let Some(prompted) = pending_prompt.take() {
            tui::line("");
            tui::line(&format!(
                "{} {} {}",
                tui::mode_badge(mode_label(&mode)),
                tui::accent("›"),
                prompted
            ));
            prompted
        } else {
            tui::line("");
            let prompt = format!(
                "{} {} ",
                tui::mode_badge(mode_label(&mode)),
                tui::accent("›")
            );
            match tui::ask_task(&prompt) {
                None if confirm_quit() => return Ok(()),
                None => continue,
                Some(tui::InputEvent::CycleMode) => {
                    mode = mode.next();
                    last_suggested_mode = None;
                    tui::show_mode_change(mode_label(&mode));
                    continue;
                }
                Some(tui::InputEvent::Rewind) => {
                    handle_rewind(
                        &mut transcript,
                        &mut rewind_points,
                        &sid,
                        cwd,
                        &provider.model,
                    );
                    continue;
                }
                Some(tui::InputEvent::Text(t)) => t,
            }
        };
        let mut t = task.trim();
        if t.is_empty() {
            continue;
        }
        // Someone typing "help" wants the command list, not a model's guess.
        if t.eq_ignore_ascii_case("help") || t == "?" {
            t = "/help";
        }

        // Shell passthrough: `!cmd` runs in the shell directly.
        if let Some(cmd) = t.strip_prefix('!') {
            let cmd = cmd.trim();
            if !cmd.is_empty() {
                let tool_input = serde_json::json!({ "command": cmd });
                if let hooks::PreDecision::Deny(r) =
                    hooks::pre_tool_use("run_command", &tool_input, cwd)
                {
                    tui::line(&tui::red(&format!(
                        "  blocked by hook: {}",
                        tui::sanitize_terminal(&r)
                    )));
                    tui::bell();
                    continue;
                }
                if let Some(reason) = agent::gate(perm, "run_command", &tool_input, cwd) {
                    tui::line(&tui::red(&format!("  {reason}")));
                    tui::bell();
                    continue;
                }
                if sandbox::would_confine() {
                    tui::line(&tui::dim("  [sandboxed]"));
                }
                let out = tools::run("run_command", &tool_input, cwd);
                // Command output is arbitrary bytes; keep its escapes inert.
                for l in tui::sanitize_terminal(&out.content).lines() {
                    tui::line(&tui::dim(&format!("  {l}")));
                }
            }
            continue;
        }

        // /mode with an inline argument, e.g. `/mode build`, `/mode 1`.
        if let Some(mode_arg) = t.strip_prefix("/mode ") {
            let arg = mode_arg.trim();
            if arg.is_empty() {
                let items = vec![
                    tui::SelectItem {
                        label: "Plan".into(),
                        detail: "Break down implementation into concrete steps before building"
                            .into(),
                    },
                    tui::SelectItem {
                        label: "Build".into(),
                        detail: "Agentic execution — edit files, run commands, solve tasks".into(),
                    },
                    tui::SelectItem {
                        label: "Brainstorm".into(),
                        detail: "Conversational thought partner with full codebase read access"
                            .into(),
                    },
                ];
                let title = format!("Select Execution Mode (Current: {})", mode_label(&mode));
                if let Some(idx) = tui::select_item(&title, &items) {
                    match idx {
                        0 => {
                            mode = Mode::Plan;
                            last_suggested_mode = None;
                            tui::show_mode_change("PLAN");
                        }
                        1 => {
                            mode = Mode::Build;
                            last_suggested_mode = None;
                            tui::show_mode_change("BUILD");
                        }
                        2 => {
                            mode = Mode::Brainstorm;
                            last_suggested_mode = None;
                            tui::show_mode_change("BRAINSTORM");
                        }
                        _ => {}
                    }
                }
            } else {
                match arg {
                    "1" | "plan" => {
                        mode = Mode::Plan;
                        last_suggested_mode = None;
                        tui::show_mode_change("PLAN");
                    }
                    "2" | "build" => {
                        mode = Mode::Build;
                        last_suggested_mode = None;
                        tui::show_mode_change("BUILD");
                    }
                    "3" | "brainstorm" => {
                        mode = Mode::Brainstorm;
                        last_suggested_mode = None;
                        tui::show_mode_change("BRAINSTORM");
                    }
                    other => tui::line(&tui::red(&format!(
                        "  unknown mode '{other}' — try: plan, build, brainstorm"
                    ))),
                }
            }
            continue;
        }

        // /mcp with arguments: `/mcp <name>`, `/mcp add …`, `/mcp remove …`, `/mcp reload`.
        if let Some(mcp_arg) = t.strip_prefix("/mcp ") {
            handle_mcp(mcp_arg);
            continue;
        }

        // /effort with an inline level, e.g. `/effort high` (bare /effort is
        // in the match below).
        if let Some(level) = t.strip_prefix("/effort ") {
            handle_effort(&mut provider, level);
            continue;
        }

        // /permissions with an inline argument, e.g. `/permissions auto`.
        if let Some(perm_arg) = t.strip_prefix("/permissions ") {
            let arg = perm_arg.trim();
            if arg.is_empty() {
                handle_permissions(&mut perm, cwd);
            } else {
                handle_permissions_arg(&mut perm, cwd, arg);
            }
            continue;
        }

        // /add-dir <path>: another folder to work in for this session.
        if let Some(arg) = t.strip_prefix("/add-dir ") {
            handle_add_dir(arg, cwd);
            continue;
        }

        // /sandbox off|auto|require|status — OS-level shell sandbox.
        if let Some(arg) = t.strip_prefix("/sandbox ") {
            handle_sandbox(arg.trim());
            continue;
        }

        // /mouse or /scroll on|off — wheel transcript scrolling is on by
        // default; off restores terminal-native text selection.
        if let Some(mouse_arg) = t.strip_prefix("/mouse ") {
            handle_mouse(Some(mouse_arg.trim()));
            continue;
        }
        if let Some(mouse_arg) = t.strip_prefix("/scroll ") {
            handle_mouse(Some(mouse_arg.trim()));
            continue;
        }

        // /model with an inline argument — hot-swap the model mid-session.
        if let Some(model_arg) = t.strip_prefix("/model ") {
            let new_model = model_arg.trim();
            if let Some((url, m)) = parse_model_endpoint(new_model) {
                // `/model http://host:port/v1 <model>`: that endpoint,
                // persisted the same way the picker's custom entry does it.
                swap_model(&mut provider, endpoint_preset(&url), &m, Some(url));
            } else if let Some(word) = unknown_provider_word(new_model) {
                tui::line(&tui::red(&format!(
                    "  unknown provider '{}' — try: {}, or /model <name> for this endpoint",
                    tui::sanitize_terminal(word),
                    config::PRESETS
                        .iter()
                        .map(|p| p.id)
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            } else if !new_model.is_empty() {
                let settings = config::load_settings().unwrap_or_default();
                let (prov, m) = parse_model_pick(new_model, &settings.provider);
                swap_model(&mut provider, &prov, &m, None);
            } else {
                handle_model(&mut provider);
            }
            continue;
        }

        // /schedule <delay> <task>  e.g. `/schedule 5m git pull && cargo test`
        if let Some(rest) = slash_args(t, "/schedule") {
            let mut parts = rest.splitn(2, char::is_whitespace);
            let delay_str = parts.next().unwrap_or("").trim();
            let task = parts.next().unwrap_or("").trim();
            if task.is_empty() {
                tui::line(&tui::red(
                    "  usage: /schedule <delay> <task>  e.g. /schedule 5m cargo test",
                ));
            } else if let Some(fire_at) = workflow::parse_delay(delay_str) {
                let id = workflow::enqueue(
                    task,
                    workflow::WorkflowKind::Scheduled {
                        fire_at_ms: fire_at,
                    },
                );
                tui::line(&tui::green(&format!(
                    "  ✓ scheduled workflow #{id}: {task}"
                )));
            } else {
                tui::line(&tui::red(&format!(
                    "  invalid delay '{delay_str}' — try: 30s, 5m, 1h"
                )));
            }
            continue;
        }

        // /loop <interval> <task>  e.g. `/loop 10m cargo test`
        if let Some(rest) = slash_args(t, "/loop") {
            let mut parts = rest.splitn(2, char::is_whitespace);
            let interval_str = parts.next().unwrap_or("").trim();
            let task = parts.next().unwrap_or("").trim();
            if task.is_empty() {
                tui::line(&tui::red(
                    "  usage: /loop <interval> <task>  e.g. /loop 30m cargo test",
                ));
            } else if let Some(secs) = workflow::parse_interval_secs(interval_str) {
                let id = workflow::enqueue(
                    task,
                    workflow::WorkflowKind::Loop {
                        interval_secs: secs,
                    },
                );
                tui::line(&tui::green(&format!(
                    "  ✓ loop workflow #{id} every {secs}s: {task}"
                )));
            } else {
                tui::line(&tui::red(&format!(
                    "  invalid interval '{interval_str}' — try: 30s, 5m, 1h"
                )));
            }
            continue;
        }

        // /btw <context> — added to the next message sent, without a turn
        // of its own. /ask is the side question that answers now.
        if let Some(ctx) = t.strip_prefix("/btw ") {
            let ctx = ctx.trim();
            if ctx.is_empty() {
                tui::line(&tui::red(
                    "  usage: /btw <note>  — adds the note to your next message (e.g. /btw also update the tests); /ask <question> asks aside now",
                ));
            } else {
                btw_ctx = Some(ctx.to_string());
                tui::line(&tui::dim(&format!(
                    "  ⚑ noted for your next message: {} — /ask <question> asks aside now",
                    tui::sanitize_terminal(ctx)
                )));
            }
            continue;
        }

        // Bare /plan, /build and /brainstorm switch the mode, as the command
        // list says; with a task they run it in that mode (below).
        if let Some(next) = bare_mode_command(t) {
            mode = next;
            last_suggested_mode = None;
            tui::show_mode_change(mode_label(&mode));
            continue;
        }
        if let Some(arg) = slash_args(t, "/theme") {
            handle_theme(arg);
            continue;
        }
        // `/plan <task>` and `/brainstorm <task>` are turns of the one
        // conversation, like `/build <task>`: kept, saved and counted.
        if let Some(task) = t.strip_prefix("/plan ") {
            tui::line("");
            let vision = Vision::of(&provider);
            let (task, images) = extract_attachments(task.trim(), cwd, vision);
            match agent::plan_turn(
                &provider,
                perm,
                &task,
                cwd,
                false,
                images,
                &mut transcript,
                &sid,
            ) {
                // "Execute Plan" switches to BUILD, as its label says; the
                // other answers leave the mode alone.
                Ok(agent::PlanEnd::Executed) if !matches!(mode, Mode::Build) => {
                    mode = Mode::Build;
                    tui::show_mode_change(mode_label(&mode));
                }
                Ok(_) => {}
                Err(e) => tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e)))),
            }
            tui::bell();
            continue;
        }
        if let Some(task) = t.strip_prefix("/build ") {
            tui::line("");
            let vision = Vision::of(&provider);
            let (task, images) = extract_attachments(task.trim(), cwd, vision);
            if let Err(e) = agent::run_build_session_with_images(
                &provider,
                perm,
                "engineer",
                &task,
                cwd,
                &mut transcript,
                &sid,
                images,
            ) {
                tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
            }
            tui::bell();
            continue;
        }
        if let Some(task) = t.strip_prefix("/brainstorm ") {
            tui::line("");
            let vision = Vision::of(&provider);
            let (task, images) = extract_attachments(task.trim(), cwd, vision);
            match agent::brainstorm_turn(&provider, cwd, &task, images, &mut transcript, &sid) {
                // The person answered y to the model's suggestion to switch.
                Ok(Some(hint)) => {
                    mode = match hint {
                        agent::ModeHint::Build => Mode::Build,
                        agent::ModeHint::Plan => Mode::Plan,
                    };
                    tui::show_mode_change(mode_label(&mode));
                }
                Ok(None) => {}
                Err(e) => tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e)))),
            }
            tui::bell();
            continue;
        }

        match t {
            "/exit" | "/quit" | "exit" | "quit" if confirm_quit() => return Ok(()),
            "/exit" | "/quit" | "exit" | "quit" => continue,
            "/clear" => {
                transcript.clear();
                rewind_points.clear();
                usage::forget_last();
                sid = session::new_id();
                session::set_current(&sid);
                trace::set_session(&sid);
                tui::clear();
                tui::line(&tui::dim("  ✓ context cleared — fresh session"));
                continue;
            }
            "/new" => {
                transcript.clear();
                rewind_points.clear();
                usage::forget_last();
                sid = session::new_id();
                session::set_current(&sid);
                trace::set_session(&sid);
                tui::line(&tui::dim("  started a fresh session"));
                continue;
            }
            "/resume" => {
                rewind_points.clear();
                handle_resume(&mut transcript, &mut sid, cwd);
                usage::forget_last();
                session::set_current(&sid);
                trace::set_session(&sid);
                continue;
            }
            "/trace" => {
                trace::render_list(30);
                continue;
            }
            "/help" => {
                print_help();
                continue;
            }
            "/init" => {
                handle_init(&mut provider, perm, cwd, raw, &mut transcript, &sid);
                continue;
            }
            "/login" => {
                handle_login(&mut provider);
                continue;
            }
            "/model" => {
                handle_model(&mut provider);
                continue;
            }
            "/compact" => {
                handle_compact(&provider, &mut transcript);
                continue;
            }
            _ if t == "/review" || t.starts_with("/review ") => {
                handle_review(
                    &provider,
                    t["/review".len()..].trim(),
                    cwd,
                    &mut transcript,
                    &sid,
                );
                continue;
            }
            "/commit" => {
                handle_commit(&provider, cwd);
                continue;
            }
            "/pr" => {
                tui::line(&tui::accent("  /pr — AI pull request"));
                tui::line(&tui::dim(
                    "  Generates a PR title and description from your branch diff.",
                ));
                let task = "Generate a pull request title and description for the current branch. Run `git log main..HEAD --oneline` and `git diff main...HEAD` (or use origin/main if main isn't local) to understand the changes. Then use `gh pr create` (if gh is available) or just print the title and description so the user can paste it.";
                tui::line("");
                if let Err(e) = agent::run_build_session(
                    &provider,
                    perm,
                    "engineer",
                    task,
                    cwd,
                    &mut transcript,
                    &sid,
                ) {
                    tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
                }
                tui::bell();
                continue;
            }
            "/workflows" | "/tasks" => {
                handle_workflows();
                continue;
            }
            "/doctor" | "/debug" => {
                handle_doctor_tui(&provider);
                continue;
            }
            "/diff" => {
                handle_diff(cwd, "");
                continue;
            }
            "/context" => {
                handle_context(&transcript, &provider);
                continue;
            }
            "/cost" => {
                handle_cost(&provider);
                continue;
            }
            "/effort" => {
                handle_effort(&mut provider, "");
                continue;
            }
            "/agents" => {
                handle_agents();
                continue;
            }
            "/checkpoints" => {
                handle_checkpoints(cwd);
                continue;
            }
            "/undo" => {
                handle_undo(cwd, "");
                continue;
            }
            "/rewind" => {
                handle_rewind(
                    &mut transcript,
                    &mut rewind_points,
                    &sid,
                    cwd,
                    &provider.model,
                );
                continue;
            }
            "/grill-me" | "/align" | "/interview" => {
                handle_align(cwd);
                continue;
            }
            "/teamwork" | "/teamwork-preview" | "/swarm" => {
                handle_teamwork(&provider);
                continue;
            }
            "/mode" => {
                let items = vec![
                    tui::SelectItem {
                        label: "Plan".into(),
                        detail: "Break down implementation into concrete steps before building"
                            .into(),
                    },
                    tui::SelectItem {
                        label: "Build".into(),
                        detail: "Agentic execution — edit files, run commands, solve tasks".into(),
                    },
                    tui::SelectItem {
                        label: "Brainstorm".into(),
                        detail: "Conversational thought partner with full codebase read access"
                            .into(),
                    },
                ];
                let title = format!("Select Execution Mode (Current: {})", mode_label(&mode));
                if let Some(idx) = tui::select_item(&title, &items) {
                    match idx {
                        0 => {
                            mode = Mode::Plan;
                            last_suggested_mode = None;
                            tui::show_mode_change("PLAN");
                        }
                        1 => {
                            mode = Mode::Build;
                            last_suggested_mode = None;
                            tui::show_mode_change("BUILD");
                        }
                        2 => {
                            mode = Mode::Brainstorm;
                            last_suggested_mode = None;
                            tui::show_mode_change("BRAINSTORM");
                        }
                        _ => {}
                    }
                }
                continue;
            }
            "/permissions" => {
                handle_permissions(&mut perm, cwd);
                continue;
            }
            "/sandbox" => {
                handle_sandbox("status");
                continue;
            }
            "/add-dir" => {
                handle_add_dir("", cwd);
                continue;
            }
            "/mouse" => {
                handle_mouse(None);
                continue;
            }
            "/scroll" => {
                handle_mouse(None);
                continue;
            }
            "/config" => {
                handle_config(&provider, perm, cwd);
                continue;
            }
            "/memory" => {
                handle_memory(&provider, perm, cwd, &mut transcript, &sid);
                continue;
            }
            "/skills" => {
                handle_skills(cwd);
                continue;
            }
            "/tools" => {
                handle_tools();
                continue;
            }
            "/mcp" => {
                handle_mcp("");
                continue;
            }
            "/vim" => {
                let current = tui::toggle_vim_mode();
                tui::line(&format!(
                    "  Vim modal editing mode is now {}",
                    if current {
                        tui::green("ENABLED [Normal/Insert]")
                    } else {
                        tui::yellow("DISABLED [Standard Emacs/Readline]")
                    }
                ));
                continue;
            }
            "/local" => {
                handle_local(&mut provider);
                continue;
            }
            "/rules" => {
                handle_rules(cwd);
                continue;
            }
            "/kb" | "/index" => {
                handle_kb_index(cwd);
                continue;
            }
            "/verify" | "/audit" => {
                handle_verify_audit(perm, cwd);
                continue;
            }
            _ => {}
        }

        if let Some(arg) = t.strip_prefix("/voice") {
            if let Some(voice_text) = handle_voice(arg) {
                if !voice_text.trim().is_empty() {
                    // The transcript is read from a `.txt` next to the audio
                    // file, which a checkout can supply.
                    tui::line(&format!(
                        "  {} {}",
                        tui::green("✓ Voice input transcribed:"),
                        tui::bold(&tui::sanitize_terminal(&voice_text))
                    ));
                    task = voice_text;
                    t = task.trim();
                } else {
                    continue;
                }
            } else {
                continue;
            }
        }

        if let Some(arg) = t.strip_prefix("/diff ") {
            handle_diff(cwd, arg);
            continue;
        }

        if t == "/export" || t.starts_with("/export ") {
            handle_export(
                &transcript,
                &sid,
                cwd,
                &provider.model,
                &t["/export".len()..],
            );
            continue;
        }
        if t == "/copy" {
            handle_copy(&transcript);
            continue;
        }
        if t == "/ask" || t.starts_with("/ask ") {
            handle_ask(&provider, cwd, &transcript, &t["/ask".len()..]);
            continue;
        }

        if t == "/rename" || t.starts_with("/rename ") {
            handle_rename(&sid, &t["/rename".len()..]);
            continue;
        }

        if let Some(arg) = t
            .strip_prefix("/undo ")
            .or_else(|| t.strip_prefix("/rewind "))
        {
            handle_undo(cwd, arg);
            continue;
        }

        if let Some(id) = t.strip_prefix("/trace ") {
            match id.trim().parse::<u64>() {
                Ok(id) => trace::render_detail(id),
                Err(_) => tui::line(&tui::red("  usage: /trace <id>")),
            }
            continue;
        }

        // Check for custom user-defined slash commands.
        if t.starts_with('/') {
            let mut words = t.trim_start_matches('/').splitn(2, char::is_whitespace);
            let cmd_name = words.next().unwrap_or("");
            let cmd_args = words.next().unwrap_or("").trim();
            if let Some(custom) = find_custom_command(cmd_name) {
                if let Some(script) = &custom.script {
                    match run_script_command(script, cmd_args, perm, cwd) {
                        Ok(out) => {
                            for l in tui::sanitize_terminal(&out).lines() {
                                tui::line(&format!("  {l}"));
                            }
                        }
                        Err(e) => {
                            tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))))
                        }
                    }
                } else {
                    // A command's body (or a skill as context) with the
                    // arguments filled in once, run in BUILD mode.
                    let task_with_context = config::command_prompt(&custom, cmd_args);
                    tui::line("");
                    if let Err(e) = agent::run_build_session(
                        &provider,
                        perm,
                        "engineer",
                        &task_with_context,
                        cwd,
                        &mut transcript,
                        &sid,
                    ) {
                        tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
                    }
                }
                tui::bell();
                continue;
            }
            // UX-001: unknown slash command — show error instead of falling
            // through to AI. A message that starts with an absolute path (a
            // dropped screenshot) is a message, and goes on to the agent.
            if !starts_with_path(t) {
                if let Some(usage) = command_usage(cmd_name) {
                    // A listed command that needs an argument, typed bare.
                    tui::line(&tui::yellow(&format!("  usage: {usage}")));
                } else if let Some(msg) = unloaded_repo_command(t, cwd) {
                    tui::line(&tui::yellow(&format!("  {msg}")));
                } else if config::is_untrusted_repo_command(cwd, cmd_name) {
                    tui::line(&tui::yellow(&format!(
                        "  /{} comes from this repo and is off until you trust this folder: start bwn here again and answer y",
                        tui::sanitize_terminal(cmd_name)
                    )));
                } else if !cmd_name.is_empty() {
                    tui::line(&tui::red(&format!(
                        "  unknown command /{cmd_name} — /help for all commands"
                    )));
                } else {
                    tui::line(&tui::red("  type /help for available commands"));
                }
                continue;
            }
        }

        // Natural-language mode/permission switch: "switch to build mode", "use readonly", etc.
        if let Some(new_mode) = detect_mode_switch(t) {
            mode = new_mode;
            last_suggested_mode = None;
            tui::show_mode_change(mode_label(&mode));
            continue;
        }
        if let Some(new_perm) = detect_permission_switch(t) {
            apply_permission(&mut perm, new_perm, PermScope::Session);
            continue;
        }

        // Mode routing: the mode changes only when the person changes it
        // (Shift+Tab, /mode, "switch to build mode"). A task typed in another
        // mode gets a one-time hint and is answered where it was typed; a
        // long paste of notes must never move the session on its own.
        if should_answer_conversationally(t, &mode) {
            last_suggested_mode = None;
        } else {
            suggest_mode_if_mismatch(t, &mode, &mut last_suggested_mode);
        }

        // Extract @path tokens. Images become multimodal attachments; text files
        // are appended into the prompt with optional @file:start-end ranges.
        let typed = t.to_string();
        let vision = Vision::of(&provider);
        let (clean_task, mut image_data) = extract_attachments(t, cwd, vision);

        // Merge any /btw context queued since the last turn.
        let effective_task = if let Some(ctx) = btw_ctx.take() {
            format!("{}\n\n[btw: {}]", clean_task, ctx)
        } else {
            clean_task.clone()
        };
        let t = effective_task.as_str();

        // Lazy MCP connect: kicked off by the first real prompt, never at
        // startup, so servers only spawn once the session is actually used.
        mcp::start_background();

        // Every mode sends attached images, on its own user message after the
        // system prompt.
        let conversational = should_answer_conversationally(t, &mode);
        let n_images = image_data.len();
        if n_images > 0 {
            let plural = crate::report::plural(n_images);
            tui::line(&tui::dim(&format!("  ⎘ attached {n_images} image{plural}")));
        }

        tui::line("");
        rewind_points.push(RewindPoint {
            index: transcript.len(),
            started_ms: checkpoint::now_ms(),
            prompt: t.to_string(),
            typed,
        });
        // Every mode reads and extends the one conversation, saved as the
        // session after each turn.
        let r = if conversational {
            let within = match mode {
                Mode::Build => agent::ChatIn::Build,
                Mode::Plan => agent::ChatIn::Plan,
                Mode::Brainstorm => agent::ChatIn::Brainstorm,
            };
            agent::run_chat_turn(
                &provider,
                perm,
                within,
                cwd,
                t,
                std::mem::take(&mut image_data),
                &mut transcript,
                &sid,
            )
        } else {
            match &mode {
                Mode::Plan => match agent::plan_turn(
                    &provider,
                    perm,
                    t,
                    cwd,
                    false,
                    std::mem::take(&mut image_data),
                    &mut transcript,
                    &sid,
                ) {
                    Ok(end) => {
                        let next = mode_after_plan(end);
                        if !matches!(next, Mode::Plan) {
                            mode = next;
                            tui::show_mode_change(mode_label(&mode));
                        }
                        Ok(())
                    }
                    Err(e) => Err(e),
                },
                Mode::Build => agent::run_build_session_with_images(
                    &provider,
                    perm,
                    "engineer",
                    t,
                    cwd,
                    &mut transcript,
                    &sid,
                    std::mem::take(&mut image_data),
                ),
                // The person answered y to the model's suggestion to switch.
                Mode::Brainstorm => match agent::brainstorm_turn(
                    &provider,
                    cwd,
                    t,
                    std::mem::take(&mut image_data),
                    &mut transcript,
                    &sid,
                ) {
                    Err(e) => Err(e),
                    Ok(None) => Ok(()),
                    Ok(Some(agent::ModeHint::Build)) => {
                        mode = Mode::Build;
                        tui::show_mode_change("BUILD");
                        Ok(())
                    }
                    Ok(Some(agent::ModeHint::Plan)) => {
                        mode = Mode::Plan;
                        tui::show_mode_change("PLAN");
                        Ok(())
                    }
                },
            }
        };
        if let Err(e) = r {
            tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
        }
        follow_reported_window(&mut provider, &transcript);
        tui::bell();
    }
}

// An Ollama started after bwn reports its window on the first request that
// reaches it, and the requests use it from then on; the footer total and
// /context follow instead of keeping the startup guess.
pub(super) fn follow_reported_window(provider: &mut Provider, transcript: &[provider::Msg]) {
    if let Some(Some(n)) = provider.ollama_ctx.get() {
        let n = *n as usize;
        if n != provider.context_tokens {
            provider.context_tokens = n;
            tui::context_meter(context_in_use(transcript, n), n);
        }
    }
}

pub(super) fn mode_label(mode: &Mode) -> &'static str {
    match mode {
        Mode::Plan => "PLAN",
        Mode::Build => "BUILD",
        Mode::Brainstorm => "BRAINSTORM",
    }
}

// The mode after a PLAN turn: BUILD only when the person chose to execute
// the plan; Cancel, Esc and a plain answer stay in PLAN.
pub(super) fn mode_after_plan(end: agent::PlanEnd) -> Mode {
    match end {
        agent::PlanEnd::Executed => Mode::Build,
        agent::PlanEnd::Cancelled | agent::PlanEnd::Answered => Mode::Plan,
    }
}

// A hint when the task phrasing suggests another mode. Only a hint: the
// mode itself changes only when the person changes it.
pub(super) fn mode_hint(task: &str, current: &Mode) -> Option<(&'static str, String)> {
    match (classify(task), current) {
        (Mode::Build | Mode::Plan, Mode::Brainstorm) => Some((
            "PLAN",
            "  tip: this looks like a task — Shift+Tab for PLAN".to_string(),
        )),
        (Mode::Build, Mode::Plan) => Some((
            "BUILD",
            "  tip: this looks like a BUILD task — Shift+Tab or /mode to switch".to_string(),
        )),
        _ => None,
    }
}

// Shows the hint once per mode combination until the mode or the kind of
// task changes.
pub(super) fn suggest_mode_if_mismatch(
    task: &str,
    current: &Mode,
    last_suggested: &mut Option<&'static str>,
) {
    match mode_hint(task, current) {
        Some((target, hint)) => {
            if *last_suggested != Some(target) {
                tui::line(&tui::dim(&hint));
                *last_suggested = Some(target);
            }
        }
        None => *last_suggested = None,
    }
}

pub(super) fn should_answer_conversationally(task: &str, current: &Mode) -> bool {
    // Simple greetings are always conversational, even in Brainstorm mode.
    if is_simple_conversation(task) {
        return true;
    }

    if matches!(current, Mode::Brainstorm) {
        return false;
    }

    matches!(classify(task), Mode::Brainstorm) && !looks_like_action_request(task)
}

pub(super) fn is_simple_conversation(task: &str) -> bool {
    let normalized = task
        .trim()
        .trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace())
        .to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "hi" | "hello"
            | "hey"
            | "yo"
            | "sup"
            | "thanks"
            | "thank you"
            | "ok"
            | "okay"
            | "cool"
            | "nice"
            | "what can you do"
            | "what can you do?"
            | "who are you"
            | "who are you?"
            | "help"
    )
}

pub(super) fn looks_like_action_request(task: &str) -> bool {
    let l = task.to_ascii_lowercase();
    let action_words = [
        "build",
        "create",
        "add",
        "fix",
        "implement",
        "write",
        "refactor",
        "run",
        "make",
        "edit",
        "change",
        "update",
        "delete",
        "remove",
        "start",
        "launch",
        "open",
        "find",
        "search",
        "read",
        "inspect",
        "list",
        "app",
        "script",
        "program",
        "function",
        "input",
        "return",
        "code",
        "logic",
    ];
    action_words.iter().any(|word| {
        l == *word
            || l.starts_with(&format!("{word} "))
            || l.contains(&format!(" {word} "))
            || l.contains(&format!(" {word} me "))
    })
}

// Where a session ran, as a list shows it: "this folder", or the last two
// parts of its path.
// Where a session ran, short: "this folder", or its last two folder names.
pub(super) fn session_folder(folder: &str, here: bool) -> String {
    if here {
        return "this folder".to_string();
    }
    let parts: Vec<&str> = folder
        .split(['/', '\\'])
        .filter(|p| !p.is_empty())
        .collect();
    match parts.len() {
        0 => folder.to_string(),
        1 => parts[0].to_string(),
        n => format!("…/{}/{}", parts[n - 2], parts[n - 1]),
    }
}

// One picker row for a saved session: its title, then age, message count
// and folder.
pub(super) fn session_item(s: &session::SessionInfo, cwd: &std::path::Path) -> tui::SelectItem {
    // Titles come from task text and cwd from the checkout's folder name.
    let label: String = tui::sanitize_terminal(s.label()).chars().take(56).collect();
    tui::SelectItem {
        label,
        detail: format!(
            "{} · {} msgs · {}",
            session::ago(s.updated_ms),
            s.msg_count,
            tui::sanitize_terminal(&session_folder(&s.cwd, s.is_in(cwd)))
        ),
    }
}

// /resume: the shared picker over sessions from this folder first; arrows
// move, text filters, Enter opens the highlighted session, Esc closes.
pub(super) fn handle_resume(
    transcript: &mut Vec<provider::Msg>,
    sid: &mut String,
    cwd: &std::path::Path,
) {
    let all = session::list_here_first(cwd);
    if all.is_empty() {
        tui::line(&tui::dim("  no saved sessions yet"));
        return;
    }
    let items: Vec<tui::SelectItem> = all.iter().map(|s| session_item(s, cwd)).collect();
    let Some(i) = tui::select_item("Resume a session (this folder first)", &items) else {
        return;
    };
    let Some(picked) = session::load(&all[i].id) else {
        tui::line(&tui::yellow("  that session file can no longer be read"));
        return;
    };
    show_resumed(&picked, cwd);
    *transcript = picked.msgs;
    *sid = picked.id;
}

// The confirmation and history replay for a session opened by /resume,
// `bwn continue` or `bwn resume <id>`.
pub(super) fn show_resumed(s: &session::Session, cwd: &std::path::Path) {
    tui::line(&tui::green(&format!(
        "  ✓ resumed: {}",
        tui::sanitize_terminal(s.label())
    )));
    if !s.is_in(cwd) {
        tui::line(&tui::yellow(&format!(
            "  this session ran in {} — its files are not in this folder",
            tui::sanitize_terminal(&s.cwd)
        )));
    }
    tui::line(&tui::dim("  ── restored history ──"));
    for msg in &s.msgs {
        match msg {
            // Saved sessions are files on disk: replay them through
            // the same sanitizer as live model and tool output.
            provider::Msg::User(text) | provider::Msg::UserImages { text, .. } => {
                tui::line(&format!(
                    "{} {}",
                    tui::accent("›"),
                    tui::sanitize_terminal(text)
                ));
            }
            provider::Msg::Assistant { text, .. } if !text.trim().is_empty() => {
                tui::line(&tui::render_md(text));
            }
            _ => {}
        }
    }
    tui::line(&tui::dim("  ────────────────────"));
}

// /export [path]: the conversation as Markdown. The session is saved first
// so the file matches what /resume would open.
pub(super) fn handle_export(
    transcript: &[provider::Msg],
    sid: &str,
    cwd: &std::path::Path,
    model: &str,
    arg: &str,
) {
    if transcript.is_empty() {
        tui::line(&tui::dim("  nothing to export yet"));
        return;
    }
    session::save(sid, cwd, model, transcript);
    let Some(s) = session::load(sid) else {
        tui::line(&tui::red("  could not read the saved session back"));
        return;
    };
    let arg = arg.trim();
    let path = (!arg.is_empty()).then(|| cwd.join(arg));
    match session::export(&s, path.as_deref()) {
        Ok(p) => tui::line(&tui::green(&format!(
            "  ✓ exported to {}",
            tui::sanitize_terminal(&p.display().to_string())
        ))),
        Err(e) => tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e)))),
    }
}

// The last answer with any text, for /copy.
pub(super) fn last_answer(transcript: &[provider::Msg]) -> Option<&str> {
    transcript.iter().rev().find_map(|m| match m {
        provider::Msg::Assistant { text, .. } if !text.trim().is_empty() => Some(text.trim()),
        _ => None,
    })
}

// The OSC 52 sequence that puts `text` on the terminal's clipboard.
pub(super) fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", media::b64_encode(text.as_bytes()))
}

// /copy: the last answer to the clipboard through the terminal (OSC 52),
// which also works over SSH; terminals that do not support it ignore it.
pub(super) fn handle_copy(transcript: &[provider::Msg]) {
    let Some(answer) = last_answer(transcript) else {
        tui::line(&tui::dim("  no answer to copy yet"));
        return;
    };
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(osc52(answer).as_bytes());
    let _ = out.flush();
    let n = answer.chars().count();
    tui::flash_footer(&format!("copied the last answer ({n} chars)"));
    tui::line(&tui::dim(&format!(
        "  ⧉ copied the last answer ({n} chars) — if nothing pasted, your terminal does not allow clipboard writes (OSC 52)"
    )));
}

// /ask <question>: a side question answered with the conversation as
// context; it is not added to the conversation or the session.
pub(super) fn handle_ask(
    provider: &Provider,
    cwd: &std::path::Path,
    transcript: &[provider::Msg],
    q: &str,
) {
    let q = q.trim();
    if q.is_empty() {
        tui::line(&tui::red(
            "  usage: /ask <question>  — answered aside; it is not added to the conversation",
        ));
        return;
    }
    tui::line("");
    if let Err(e) = agent::ask_aside(provider, cwd, q, transcript) {
        tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
    }
    tui::line(&tui::dim("  (asked aside — not added to the conversation)"));
}

pub(super) fn handle_rename(sid: &str, name: &str) {
    if name.trim().is_empty() {
        tui::line(&tui::red("  usage: /rename <name>"));
        return;
    }
    match session::rename(sid, name) {
        Ok(()) => tui::line(&tui::green(&format!(
            "  ✓ session named '{}'",
            tui::sanitize_terminal(name.trim())
        ))),
        Err(e) => tui::line(&tui::yellow(&format!("  {e}"))),
    }
}

// `bwn sessions [rm <id>]`.
pub(super) fn sessions_command(args: &[String]) {
    match args.first().map(String::as_str) {
        None => {}
        Some("rm" | "remove" | "delete") => {
            let Some(id) = args.get(1) else {
                eprintln!("usage: buildwithnexus sessions rm <id>");
                std::process::exit(2);
            };
            match session::remove(id) {
                Ok(s) => println!(
                    "deleted session {id}: {}",
                    tui::sanitize_terminal(s.label())
                ),
                Err(e) => {
                    eprintln!("buildwithnexus: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("export") => {
            let Some(id) = args.get(1) else {
                eprintln!("usage: buildwithnexus sessions export <id> [file.md]");
                std::process::exit(2);
            };
            let Some(s) = session::load(id) else {
                eprintln!(
                    "no session '{}' — bwn sessions lists them",
                    tui::sanitize_terminal(id)
                );
                std::process::exit(1);
            };
            match session::export(&s, args.get(2).map(std::path::Path::new)) {
                Ok(p) => println!("{}", p.display()),
                Err(e) => {
                    eprintln!("buildwithnexus: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some(other) => {
            eprintln!(
                "buildwithnexus sessions: unknown subcommand '{other}' — try: rm <id>, export <id>"
            );
            std::process::exit(2);
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let all = session::list_here_first(&cwd);
    if report::is_json() {
        return print_sessions_json(&all);
    }
    if all.is_empty() {
        // Empty output reads as "broken"; say why the list is empty.
        println!("no saved sessions yet — every conversation is saved as it runs.");
        return;
    }
    for s in &all {
        // Titles come from task text and cwd from the checkout's
        // folder name, so neither reaches the terminal raw.
        let title: String = tui::sanitize_terminal(s.label()).chars().take(48).collect();
        println!(
            "  {}  {:<9} {:>4} msgs  {:<48}  {}",
            s.id,
            session::ago(s.updated_ms),
            s.msg_count,
            title,
            tui::sanitize_terminal(&s.cwd)
        );
    }
    println!();
    println!(
        "{}",
        tui::dim("open one:  buildwithnexus resume <id>  ·  this folder's latest:  buildwithnexus continue  ·  add a task to run it headless  ·  sessions export <id> | rm <id>")
    );
}

// Opens `s` in the terminal UI, or runs `task` on it headless.
pub(super) fn open_session(opts: CliOptions, s: session::Session, task: String, verb: &str) {
    if !task.trim().is_empty() {
        let (msgs, id) = (s.msgs, s.id);
        headless(&opts, move |p, perm, cwd| {
            agent::run_build_resumed(p, perm, "engineer", &task, &cwd, msgs, &id)
        });
        return;
    }
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        eprintln!(
            "buildwithnexus: `{verb}` with no task opens the session in the terminal UI, and there is no terminal here.\n  \
             Add a task to run it headless: buildwithnexus {verb} <task>"
        );
        std::process::exit(2);
    }
    session::resume_on_start(s);
    interactive(opts.prompt.clone(), opts);
}

// `bwn continue [task]`: this folder's latest session, or the latest
// anywhere with a note saying which.
pub(super) fn continue_command(opts: CliOptions, task: String) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let s = match session::latest_for(&cwd) {
        Some(s) => s,
        None => match session::latest() {
            Some(s) => {
                eprintln!(
                    "{}",
                    tui::yellow(&format!(
                        "no session in this folder — continuing '{}' from {}",
                        tui::sanitize_terminal(s.label()),
                        tui::sanitize_terminal(&session_folder(&s.cwd, s.is_in(&cwd)))
                    ))
                );
                s
            }
            None => {
                eprintln!(
                    "buildwithnexus: no saved sessions to continue — bwn sessions lists them"
                );
                std::process::exit(1);
            }
        },
    };
    open_session(opts, s, task, "continue");
}

// `bwn resume <id> [task]`; with no id, the /resume picker.
pub(super) fn resume_command(opts: CliOptions, args: &[String]) {
    let Some(id) = args.first() else {
        if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
            eprintln!(
                "buildwithnexus: `resume` with no id opens the session picker in the terminal UI, and there is no terminal here.\n  \
                 Pass an id and a task to run one headless (bwn sessions lists them): buildwithnexus resume <id> <task>"
            );
            std::process::exit(2);
        }
        let prompt = opts.prompt.clone().or_else(|| Some("/resume".to_string()));
        interactive(prompt, opts);
        return;
    };
    let Some(s) = session::load(id) else {
        eprintln!(
            "no session '{}' — bwn sessions lists them",
            tui::sanitize_terminal(id)
        );
        std::process::exit(1);
    };
    open_session(opts, s, args[1..].join(" "), "resume");
}

pub(super) fn handle_config(provider: &Provider, perm: Permission, cwd: &std::path::Path) {
    tui::line(&tui::accent("  /config — AI-assisted configuration"));
    tui::line(&tui::dim(
        "  Tell me what to configure (hooks, memory, custom commands, settings…)",
    ));
    tui::line(&tui::dim(
        "  Examples: 'add a hook to log every command run'",
    ));
    tui::line(&tui::dim(
        "            'remember I prefer TypeScript over JavaScript'",
    ));
    tui::line(&tui::dim("            'create a /deploy slash command'"));
    tui::line("");

    let input = match tui::ask(&format!("  {} ", tui::accent("›"))) {
        None => return,
        Some(s) => s,
    };
    let t = input.trim();
    if t.is_empty() {
        return;
    }

    // Show current config context to the model.
    let home_dir = config::home();
    let settings_json = std::fs::read_to_string(home_dir.join("settings.json")).unwrap_or_default();
    let memory_md = config::load_memory().unwrap_or_default();

    let context = format!(
        "The user wants to configure buildwithnexus. Their current settings.json:\n```json\n{settings_json}\n```\n\
        Their current memory.md:\n```markdown\n{memory_md}\n```\n\
        Home directory: {home}\n\
        User request: {t}",
        home = home_dir.display()
    );

    let full_task = format!(
        "Help configure buildwithnexus based on this request. You can:\n\
        - Write to ~/.buildwithnexus/settings.json to add/edit hooks\n\
        - Write to ~/.buildwithnexus/memory.md to add memory\n\
        - Create files in ~/.buildwithnexus/commands/ for custom slash commands\n\
        - Create files in ~/.buildwithnexus/skills/ for skills\n\
        - Create files in ~/.buildwithnexus/hooks/<Event>/ for auto-discovered hook scripts\n\n\
        {context}"
    );

    tui::line("");
    if let Err(e) = agent::run_build(provider, perm, "engineer", &full_task, cwd, Vec::new()) {
        tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
    }
}

pub(super) fn handle_memory(
    provider: &Provider,
    perm: Permission,
    cwd: &std::path::Path,
    transcript: &mut Vec<provider::Msg>,
    sid: &str,
) {
    tui::line(&tui::accent("  session memory"));
    match config::load_memory() {
        None => tui::line(&tui::dim("  no memory saved yet")),
        Some(mem) => {
            // memory.md is Markdown — render headings/bullets/emphasis.
            tui::line(&tui::render_md(&mem));
        }
    }
    tui::line("");
    tui::line(&tui::dim(
        "  [a] add entry  [c] clear  [e] edit via AI  [Enter] dismiss",
    ));
    let pick = tui::ask(&tui::dim("  action › ")).unwrap_or_default();
    match pick.trim() {
        "a" => {
            if let Some(entry) = tui::ask("  note to save: ") {
                if !entry.trim().is_empty() {
                    config::append_memory(entry.trim());
                    tui::line(&tui::green("  ✓ saved"));
                }
            }
        }
        "c" => {
            config::save_memory("");
            tui::line(&tui::yellow("  memory cleared"));
        }
        "e" => {
            let task = "Review and clean up the memory.md file at ~/.buildwithnexus/memory.md. \
                Remove duplicates, organize by topic, and keep it concise.";
            if let Err(e) =
                agent::run_build_session(provider, perm, "engineer", task, cwd, transcript, sid)
            {
                tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
            }
        }
        _ => {}
    }
}

pub(super) fn handle_skills(cwd: &std::path::Path) {
    let skills = config::discover_skills(cwd);
    if skills.is_empty() {
        tui::line(&tui::dim("  No skills found."));
        tui::line(&tui::dim(&format!(
            "  Add <name>.md files or <name>/SKILL.md folders to {}/skills/",
            config::home().display()
        )));
        return;
    }
    // First detail line is what the list shows: source + description.
    let mut items: Vec<(String, String)> = skills
        .iter()
        .map(|s| {
            (
                format!("/{}", s.name),
                format!(
                    "[{}] {}\n\n{}",
                    s.source.label(),
                    s.description_or_default(),
                    s.loaded_text()
                ),
            )
        })
        .collect();
    for cmd in config::load_custom_commands()
        .into_iter()
        .filter(|c| c.script.is_some())
    {
        items.push((
            format!("/{}", cmd.name),
            "[script command] runs through the run_command permission gate and hooks.".to_string(),
        ));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    tui::browse_items("skills", &items);
}

pub(super) fn handle_tools() {
    let mut items: Vec<(String, String)> = tools::defs(true)
        .into_iter()
        .map(|d| {
            let schema =
                serde_json::to_string_pretty(&d.schema).unwrap_or_else(|_| d.schema.to_string());
            (
                d.name.to_string(),
                format!("{}\n\nSchema:\n{schema}", d.description),
            )
        })
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    tui::browse_items("tools", &items);
}

pub(super) fn handle_mcp(arg: &str) {
    let args = shlex::split(arg.trim()).unwrap_or_default();
    // Server names, server_info and errors are server- or config-supplied.
    let mut say = |l: &str| tui::line(&format!("  {l}"));
    match mcp::manage(&args, true, &mut say) {
        Ok(lines) => {
            for l in lines {
                tui::line(&format!("  {}", tui::sanitize_terminal(&l)));
            }
        }
        Err(e) => {
            tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
            tui::bell();
        }
    }
}

pub(super) fn has_instruction_file(cwd: &std::path::Path) -> bool {
    cwd.join("AGENTS.md").exists() || cwd.join("CLAUDE.md").exists()
}

/// What /init (and `init --agents-md`) asks the model to do: read this
/// repository's own build, test and CI files and write down only what they
/// say, or improve the AGENTS.md that is already there.
pub(super) fn agents_md_task(cwd: &std::path::Path) -> String {
    let goal = if cwd.join("AGENTS.md").exists() {
        "Improve the AGENTS.md at the repository root: keep everything in it that is still true, \
         correct what the repository contradicts, and add what is missing. Change it with \
         edit_file (or write_file with the whole improved text)."
    } else {
        "Write AGENTS.md at the repository root with write_file."
    };
    format!(
        "{goal} AGENTS.md is the file coding agents read before working in this repository.\n\n\
         First look at what the repository really uses: list the root, then read the README and \
         the build and test files that exist (Makefile, package.json scripts, Cargo.toml, \
         pyproject.toml, setup.cfg, go.mod, build.gradle, pom.xml, CMakeLists.txt, justfile, \
         Taskfile.yml, and CI workflows such as .github/workflows/*.yml).\n\n\
         Then write these sections, using only commands and facts you found in those files:\n\
         - Build & test: the exact commands to build, run the tests, run one test, lint and format.\n\
         - Layout: the main directories and what lives in each.\n\
         - Conventions: style, naming and commit rules the code or config shows.\n\
         - Do not: generated or vendored files that must not be edited by hand, and commands \
           that must not be run.\n\n\
         Keep it under 60 lines. Leave out anything you could not confirm instead of guessing. \
         Do not run commands, and do not change any file other than AGENTS.md."
    )
}

/// `/init` step: offer to write AGENTS.md from this repository, or improve
/// the one there. The write always goes through the approval gate (even
/// under auto), so the proposed file is shown as a diff before it lands.
pub(super) fn offer_agents_md(
    provider: &Provider,
    perm: Permission,
    cwd: &std::path::Path,
    transcript: &mut Vec<provider::Msg>,
    sid: &str,
) {
    let exists = cwd.join("AGENTS.md").exists();
    if !exists && cwd.join("CLAUDE.md").exists() {
        return;
    }
    tui::line("");
    let question = if exists {
        "  improve AGENTS.md from this repository? [y/N] "
    } else {
        tui::line(&tui::dim(
            "  AGENTS.md tells the agent your build and test commands, conventions and do-nots.",
        ));
        "  generate AGENTS.md from this repository? [Y/n] "
    };
    let Some(answer) = tui::ask(question) else {
        return;
    };
    let yes = match answer.trim().to_lowercase().as_str() {
        "y" | "yes" => true,
        "" => !exists,
        "n" | "no" => false,
        // A message typed while the question was up is not an answer.
        _ => {
            tui::prefill_composer(&answer);
            tui::line(&tui::dim(
                "  skipped — that was not y or n; your text is back in the input box",
            ));
            return;
        }
    };
    if !yes {
        tui::line(&tui::dim("  skipped"));
        return;
    }
    if matches!(perm, Permission::ReadOnly) {
        tui::line(&tui::yellow(
            "  permission is read-only, so nothing can be written — /permissions ask, then /init again",
        ));
        return;
    }
    tui::line("");
    if let Err(e) = agent::run_build_session(
        provider,
        Permission::Ask,
        "engineer",
        &agents_md_task(cwd),
        cwd,
        transcript,
        sid,
    ) {
        tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
    }
    tui::bell();
}

// `/init`: setup again; the session then runs on what setup saved, and is
// left as it was when setup is cancelled.
pub(super) fn handle_init(
    provider: &mut Provider,
    perm: Permission,
    cwd: &std::path::Path,
    raw: bool,
    transcript: &mut Vec<provider::Msg>,
    sid: &str,
) {
    tui::leave_alt();
    let saved = onboarding::run();
    tui::enter_alt(raw);
    let Some(saved) = saved else {
        tui::line(&tui::dim("  /init cancelled — settings unchanged"));
        return;
    };
    // Project settings may still layer over what was saved; run on the merge.
    let s = config::load_settings()
        .filter(|s| !s.provider.is_empty())
        .unwrap_or(saved);
    match build_provider(&s) {
        Ok(mut p) => {
            p.effort = provider.effort;
            usage::forget_last();
            *provider = p;
            provider::prewarm(provider);
            set_active_preset(&s.provider);
            tui::set_model_label(&provider.model);
            tui::line(&tui::green(&format!(
                "  now using {} · {}",
                s.provider,
                tui::sanitize_terminal(&provider.base_url)
            )));
        }
        Err(e) => tui::line(&tui::red(&format!(
            "  ✗ {} — keeping {}",
            tui::sanitize_terminal(&e),
            provider.model
        ))),
    }
    offer_agents_md(provider, perm, cwd, transcript, sid);
}

// `/login`: a new key for the provider the session is using.
pub(super) fn handle_login(provider: &mut Provider) {
    let mut s = config::load_settings().unwrap_or_default();
    s.provider = active_preset().unwrap_or(s.provider);
    s.model = provider.model.clone();
    s.base_url = Some(provider.base_url.clone())
        .filter(|u| config::preset(&s.provider).is_none_or(|p| p.base_url != u));
    if let Some(mut p) = onboarding::login(&s) {
        p.effort = provider.effort;
        usage::forget_last();
        *provider = p;
    }
}

pub(super) fn find_custom_command(name: &str) -> Option<config::CustomCommand> {
    config::load_custom_commands()
        .into_iter()
        .find(|c| c.name == name)
}

// `/name …` naming one of the checkout's commands or skills that is off
// because the folder is not trusted.
pub(super) fn untrusted_repo_command(text: &str, cwd: &std::path::Path) -> Option<String> {
    let name = text.trim().strip_prefix('/')?.split_whitespace().next()?;
    (find_custom_command(name).is_none() && config::is_untrusted_repo_command(cwd, name))
        .then(|| name.to_string())
}

// `/name` naming a command or skill file of the checkout that does not
// load: what to tell the person instead of sending the line on.
pub(super) fn unloaded_repo_command(text: &str, cwd: &std::path::Path) -> Option<String> {
    let name = text.trim().strip_prefix('/')?.split_whitespace().next()?;
    if find_custom_command(name).is_some() {
        return None;
    }
    let path = config::unloaded_repo_command(cwd, name)?;
    Some(format!(
        "/{} ({}) is {}",
        tui::sanitize_terminal(name),
        tui::sanitize_terminal(&path),
        config::NOT_LOADED
    ))
}

/// A `/word` task that names no command or skill, as the session says it:
/// a path (`/etc/hosts is wrong`) is a task, anything else is a mistyped
/// command and no request is made for it.
pub(super) fn unknown_slash_command(text: &str) -> Option<String> {
    let t = text.trim();
    let name = t.strip_prefix('/')?.split_whitespace().next()?;
    if starts_with_path(t) || find_custom_command(name).is_some() {
        return None;
    }
    let commands = config::load_custom_commands();
    let known: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
    let name = tui::sanitize_terminal(name);
    Some(match hooks::did_you_mean(&name, &known) {
        Some(near) => format!("unknown command /{name} (did you mean /{near}?)"),
        None => format!(
            "unknown command /{name} — a headless run takes your own commands and skills, not the session's built-in ones"
        ),
    })
}

/// `/name args` naming a command or skill: that command and its arguments.
/// A task that merely starts with a path (`/usr/bin/foo fails`) is not one.
pub(super) fn find_slash_command(text: &str) -> Option<(config::CustomCommand, String)> {
    let rest = text.trim().strip_prefix('/')?;
    let mut words = rest.splitn(2, char::is_whitespace);
    let name = words.next().filter(|n| !n.is_empty() && !n.contains('/'))?;
    let args = words.next().unwrap_or("").trim().to_string();
    find_custom_command(name).map(|c| (c, args))
}

// A script command runs like any run_command: PreToolUse hooks, then the
// permission gate. Its output on success, the refusal or output otherwise.
pub(super) fn run_script_command(
    script: &std::path::Path,
    args: &str,
    perm: Permission,
    cwd: &std::path::Path,
) -> Result<String, String> {
    // Shell-quote the script path to guard against spaces (UX-007).
    let escaped = script.to_string_lossy().replace('\'', "'\"'\"'");
    let shell_cmd = if args.is_empty() {
        format!("'{escaped}'")
    } else {
        format!("'{escaped}' {args}")
    };
    let tool_input = serde_json::json!({"command": shell_cmd});
    let out = tools::with_script_command(script, || {
        match agent::hook_gate(perm, "run_command", &tool_input, cwd) {
            Some(reason) => Err(reason),
            None => Ok(tools::run("run_command", &tool_input, cwd)),
        }
    })?;
    if out.is_error {
        Err(out.content)
    } else {
        Ok(out.content)
    }
}

pub(super) fn handle_model(provider: &mut Provider) {
    let settings = config::load_settings().unwrap_or_default();
    let mut options: Vec<(String, String, String)> = Vec::new();

    let ollama_base = if settings.provider == "ollama" {
        settings.base_url.clone()
    } else {
        remembered_endpoint("ollama")
    }
    .unwrap_or_else(|| "http://localhost:11434".into());
    let ollama_installed = provider::ollama_models(&ollama_base);
    for m in ollama_installed.iter().take(onboarding::MODEL_LIST_MAX) {
        options.push(("ollama".into(), m.clone(), "installed Ollama model".into()));
    }
    // The rest stay reachable by name: this row (no model) asks for one.
    let more = ollama_installed
        .len()
        .checked_sub(onboarding::MODEL_LIST_MAX)
        .filter(|n| *n > 0)
        .map(onboarding::more_line);
    if more.is_some() {
        options.push((
            "ollama".into(),
            String::new(),
            "installed Ollama models".into(),
        ));
    }

    let no_server = find_llama_server_binary().is_none();
    for name in crate::local::scan_gguf() {
        let detail = if no_server {
            "GGUF on disk — local server (needs llama-server)".into()
        } else {
            "GGUF on disk — local server".into()
        };
        options.push(("llamacpp".into(), name, detail));
    }

    for p in config::PRESETS.iter().filter(|p| !p.local) {
        let status = if config::load_key(p.env_key).is_none() {
            format!("needs {}", p.env_key)
        } else if config::key_rejected(p.env_key) {
            "key rejected — /login".into()
        } else {
            "ready".into()
        };
        options.push((
            p.id.to_string(),
            p.default_model.to_string(),
            format!("{} ({})", p.label, status),
        ));
        for m in p.more_models {
            options.push((
                p.id.to_string(),
                m.to_string(),
                format!("{} ({})", p.label, status),
            ));
        }
    }

    options.push((
        "custom".into(),
        String::new(),
        "custom OpenAI-compatible endpoint".into(),
    ));

    let select_items: Vec<tui::SelectItem> = options
        .iter()
        .map(|(prov, model, desc)| {
            let label = match (prov.as_str(), model.is_empty()) {
                ("custom", true) => format!("[{prov}] custom endpoint"),
                (_, true) => format!("{prov} / {}", more.as_deref().unwrap_or_default()),
                _ => format!("{prov} / {model}"),
            };
            tui::SelectItem {
                label,
                detail: desc.clone(),
            }
        })
        .collect();

    let title = format!(
        "Select AI Model (Current: {} on {})",
        provider.model,
        active_preset().unwrap_or(settings.provider)
    );

    if let Some(idx) = tui::select_item(&title, &select_items) {
        let (target_provider, model, _) = options[idx].clone();
        if target_provider == "custom" && model.is_empty() {
            let Some(pick) =
                tui::ask("  Enter endpoint URL & model (e.g. http://localhost:8080/v1 model): ")
            else {
                return swap_cancelled();
            };
            let pick = pick.trim();
            if !pick.is_empty() {
                let (url, m) = pick
                    .split_once(char::is_whitespace)
                    .map(|(u, m)| (u.trim().to_string(), m.trim().to_string()))
                    .unwrap_or((pick.to_string(), String::new()));
                swap_model(provider, endpoint_preset(&url), &m, Some(url));
            }
        } else if model.is_empty() {
            let Some(m) = tui::ask("  model name: ") else {
                return swap_cancelled();
            };
            if !m.trim().is_empty() {
                swap_model(provider, &target_provider, m.trim(), None);
            }
        } else {
            swap_model(provider, &target_provider, &model, None);
        }
    }
}

/// `/model <http(s)://url> [model]` → (base_url, model). A URL first token
/// always names an endpoint; it must never fall into the org/model →
/// OpenRouter inference below. `endpoint_preset` says which preset serves it.
pub(super) fn parse_model_endpoint(pick: &str) -> Option<(String, String)> {
    let pick = pick.trim();
    let (first, rest) = pick.split_once(char::is_whitespace).unwrap_or((pick, ""));
    let lower = first.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return None;
    }
    Some((first.to_string(), rest.trim().to_string()))
}

/// An Ollama host root is the Ollama preset, so its own API (and its model
/// list) is used; any other URL, `…/v1` included, is the custom
/// OpenAI-compatible preset. Ollama's port says so; a host root on another
/// port is asked.
pub(super) fn endpoint_preset(url: &str) -> &'static str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let root = rest.trim_end_matches('/').split_once('/').is_none();
    if root && (is_ollama_address(url) || !provider::ollama_models(url).is_empty()) {
        "ollama"
    } else {
        "custom"
    }
}

pub(super) fn is_ollama_address(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split('/').next().unwrap_or(rest);
    authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .ends_with(":11434")
}

/// The address last used with `preset`, from the user's own settings only:
/// a cloned repository never chooses where a swap sends the key.
pub(super) fn remembered_endpoint(preset: &str) -> Option<String> {
    config::load_user_settings().and_then(|u| u.endpoints.get(preset).cloned())
}

pub(super) fn swap_cancelled() {
    tui::line(&tui::dim("  /model cancelled — nothing saved"));
}

/// Maps a typed model name to the provider that serves it. Anything
/// unrecognized stays on the current provider — the swap then validates it.
pub(super) fn parse_model_pick(pick: &str, current_provider: &str) -> (String, String) {
    // Explicit "<provider> <model>" always wins.
    if let Some((p, m)) = pick.split_once(char::is_whitespace) {
        if config::preset(p.trim()).is_some() {
            return (p.trim().to_string(), m.trim().to_string());
        }
    }
    if let Some(rest) = pick.strip_prefix("ollama/") {
        return ("ollama".into(), rest.to_string());
    }
    if let Some(rest) = pick.strip_prefix("local/") {
        return ("llamacpp".into(), rest.to_string());
    }
    let lower = pick.to_ascii_lowercase();
    if lower.starts_with("claude") {
        return ("anthropic".into(), pick.to_string());
    }
    if lower.starts_with("gpt") || lower.starts_with("chatgpt") {
        return ("openai".into(), pick.to_string());
    }
    // Gemini has no native preset — OpenRouter serves it.
    if lower.starts_with("gemini") {
        return ("openrouter".into(), format!("google/{lower}"));
    }
    // org/model naming is OpenRouter's scheme — but a URL is not a model
    // name (see parse_model_endpoint).
    if pick.contains('/') && !pick.contains("://") {
        return ("openrouter".into(), pick.to_string());
    }
    (current_provider.to_string(), pick.to_string())
}

/// The first word of a `/model <word> <model>` pick whose first word names
/// no provider. A model name has no spaces, so two words are a provider and
/// a model, and swapping to the pair as one name only ends in a 404.
pub(super) fn unknown_provider_word(pick: &str) -> Option<&str> {
    let (first, rest) = pick.split_once(char::is_whitespace)?;
    (!rest.trim().is_empty() && config::preset(first).is_none()).then_some(first)
}

pub(super) fn find_active_local_base_url(preferred: &str) -> Option<String> {
    find_active_local_base_url_in(&[
        preferred,
        "http://localhost:8080/v1",
        "http://localhost:1234/v1",
        "http://localhost:8000/v1",
    ])
}

/// The first of `candidates` where a llama.cpp, LM Studio or vLLM server
/// answers. Ollama also answers /v1/models, but cannot load a GGUF file
/// or an LM Studio model by name, so it is never the answer here.
pub(super) fn find_active_local_base_url_in(candidates: &[&str]) -> Option<String> {
    for url in candidates {
        let root = url.trim_end_matches('/').trim_end_matches("/v1");
        let get = |path: &str, ms: u64| {
            crate::net::shared()
                .get(&format!("{root}{path}"))
                .timeout(std::time::Duration::from_millis(ms))
                .call()
                .ok()
        };
        let json = |res: &ureq::Response| {
            res.status() < 500
                && res
                    .header("content-type")
                    .is_some_and(|ct| ct.contains("application/json"))
        };
        let answers =
            get("/v1/models", 400).is_some_and(|r| json(&r)) || get("/health", 300).is_some();
        if answers && !is_ollama(root) {
            return Some(url.to_string());
        }
    }
    None
}

// Ollama's own listing; LM Studio and llama.cpp answer it with a 404.
pub(super) fn is_ollama(root: &str) -> bool {
    crate::net::shared()
        .get(&format!("{root}/api/tags"))
        .timeout(std::time::Duration::from_millis(400))
        .call()
        .ok()
        .and_then(|r| r.into_json::<serde_json::Value>().ok())
        .is_some_and(|v| v["models"].is_array())
}

/// Why a GGUF file cannot be served, when nothing is running that could:
/// llama-server is needed to load it (or LM Studio, which loads it itself).
pub(super) fn gguf_unservable(model_name: &str, have_llama_server: bool) -> Option<&'static str> {
    let gguf = model_name.to_ascii_lowercase().ends_with(".gguf");
    (gguf && !have_llama_server).then_some(
        "llama-server is not installed — install llama.cpp, or load the file in LM Studio",
    )
}

pub(super) fn find_llama_server_binary() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let pathext = std::env::var_os("PATHEXT");
    find_llama_server_in(&path, pathext.as_deref(), cfg!(windows))
}

/// `find_llama_server_binary` with its inputs passed in, so tests can run
/// the Windows rules anywhere: there the binary is `llama-server.exe`.
pub(super) fn find_llama_server_in(
    path: &std::ffi::OsStr,
    pathext: Option<&std::ffi::OsStr>,
    windows: bool,
) -> Option<std::path::PathBuf> {
    if !windows {
        for known in [
            "/opt/homebrew/bin/llama-server",
            "/usr/local/bin/llama-server",
            "/usr/bin/llama-server",
        ] {
            let p = std::path::PathBuf::from(known);
            if p.exists() {
                return Some(p);
            }
        }
    }
    tools::find_in_path("llama-server", path, pathext, windows)
}

pub(super) static LLAMA_SERVER_PROCESS: std::sync::Mutex<Option<std::process::Child>> =
    std::sync::Mutex::new(None);

pub fn kill_local_server() {
    if let Ok(mut lock) = LLAMA_SERVER_PROCESS.lock() {
        if let Some(mut child) = lock.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(super) fn ensure_local_gguf_server(preferred_url: &str, model_name: &str) -> Option<String> {
    if let Some(active_url) = find_active_local_base_url(preferred_url) {
        if active_url.contains("8080") {
            let probe = format!("{}/models", active_url.trim_end_matches('/'));
            if let Ok(res) = crate::net::shared().get(&probe).call() {
                if let Ok(json) = res.into_string() {
                    if !json.contains(model_name) {
                        tui::line(&tui::yellow(&format!("  ⚠ local server at {active_url} is loaded with a different model. Switch it manually if needed.")));
                    }
                }
            }
        }
        return Some(active_url);
    }
    if let Some(why) = gguf_unservable(model_name, find_llama_server_binary().is_some()) {
        tui::line(&tui::yellow(&format!("  ✗ {why}")));
        return None;
    }
    let server_bin = find_llama_server_binary()?;
    let gguf_path = crate::local::find_gguf_path(model_name)?;

    tui::line(&tui::accent(&format!(
        "  ⟳ starting local llama-server for {model_name} on port 8080…"
    )));

    let spawn_res = std::process::Command::new(server_bin)
        .arg("-m")
        .arg(&gguf_path)
        .arg("--port")
        .arg("8080")
        .arg("-c")
        .arg("8192")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    match spawn_res {
        Ok(child) => {
            if let Ok(mut lock) = LLAMA_SERVER_PROCESS.lock() {
                *lock = Some(child);
            }
        }
        Err(_) => return None,
    }

    use std::io::Write;
    for _ in 0..60 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        print!(".");
        let _ = std::io::stdout().flush();
        if crate::net::shared()
            .get("http://localhost:8080/v1/models")
            .timeout(std::time::Duration::from_millis(300))
            .call()
            .is_ok()
        {
            println!();
            tui::line(&tui::green(
                "  ✓ local llama-server is active at http://localhost:8080/v1",
            ));
            return Some("http://localhost:8080/v1".to_string());
        }
    }
    println!();
    tui::line(&tui::red(
        "  ✗ local llama-server failed to start within 30 seconds",
    ));
    None
}

// The preset the live provider was built from: the settings and flags at
// startup, then each /model, /init and /login. Banners and "now using"
// lines read it, so they name what is really answering.
pub(super) static ACTIVE_PRESET: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

pub(super) fn set_active_preset(id: &str) {
    if let Ok(mut a) = ACTIVE_PRESET.lock() {
        *a = id.to_string();
    }
}

pub(super) fn active_preset() -> Option<String> {
    ACTIVE_PRESET
        .lock()
        .ok()
        .map(|a| a.clone())
        .filter(|a| !a.is_empty())
}

/// Where a `/model` swap points: the base URL it probes and runs against,
/// and what it writes to the saved `base_url`.
#[derive(Debug, PartialEq)]
pub(super) struct SwapTarget {
    pub(super) base_url: String,
    /// None leaves the saved value alone; Some(None) clears it.
    pub(super) save: Option<Option<String>>,
}

/// A saved `base_url` belongs to the provider it was set for: a swap within
/// that provider keeps it (a remote Ollama host, an LM Studio box on the
/// LAN), a swap to another provider goes back to the address last used with
/// it (`remembered`, from `endpoints`) or else to its preset default, and an
/// explicit URL always wins.
pub(super) fn plan_swap(
    current_provider: &str,
    saved_base_url: Option<&str>,
    remembered: Option<&str>,
    target: &config::Preset,
    override_url: Option<&str>,
) -> SwapTarget {
    if let Some(url) = override_url {
        return SwapTarget {
            base_url: url.to_string(),
            save: Some(Some(url.to_string())),
        };
    }
    match (saved_base_url, remembered) {
        (Some(url), _) if current_provider == target.id => SwapTarget {
            base_url: url.to_string(),
            save: None,
        },
        _ if current_provider == target.id => SwapTarget {
            base_url: target.base_url.to_string(),
            save: None,
        },
        (_, Some(url)) => SwapTarget {
            base_url: url.to_string(),
            save: Some(Some(url.to_string())),
        },
        _ => SwapTarget {
            base_url: target.base_url.to_string(),
            save: Some(None),
        },
    }
}

/// The `endpoints` map after a swap from `from` to `to`, kept for the next
/// swap back: the address the user's own settings give `from`, and for `to`
/// the one this swap saves (`saved`), or else the user's own. A base_url a
/// trusted project layers in serves that project only, so it is never
/// remembered for the others.
pub(super) fn remember_endpoints(
    user: &Settings,
    from: &str,
    to: &str,
    saved: Option<Option<&str>>,
) -> std::collections::BTreeMap<String, String> {
    let own = |id: &str| {
        (user.provider == id)
            .then_some(user.base_url.as_deref())
            .flatten()
    };
    let mut endpoints = user.endpoints.clone();
    if let Some(u) = own(from).filter(|_| from != to) {
        endpoints.insert(from.to_string(), u.to_string());
    }
    if let Some(u) = saved.unwrap_or_else(|| own(to)) {
        endpoints.insert(to.to_string(), u.to_string());
    }
    endpoints
}

// The last swap's probe was answered by a server that does not list the
// model: its success line must not say "validated".
pub(super) static SWAP_UNCONFIRMED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The swap's success line names the model actually saved, which differs
/// from the one asked for when a local server only answers as "local-model".
pub(super) fn swap_success_line(requested: &str, active: &str, provider_label: &str) -> String {
    if SWAP_UNCONFIRMED.swap(false, std::sync::atomic::Ordering::Relaxed) {
        // The server answered, but does not list the name: it may be
        // running another model under it (unlisted_note said so).
        format!(
            "  ✓ active model hot-swapped → {active} on {provider_label} (answered; the server does not list it)"
        )
    } else if requested == active || requested.is_empty() {
        format!("  ✓ active model hot-swapped → {active} on {provider_label} (validated)")
    } else {
        format!(
            "  ✓ active model hot-swapped → {active} on {provider_label} (validated; the server \
             does not serve '{requested}' by that name, so it runs as '{active}')"
        )
    }
}

/// Applies a model swap only after the target provider is actually usable:
/// walks the user through a missing API key (or a custom endpoint's URL),
/// checks that a local server is reachable and has the model, and keeps the
/// current model on any failure. A key typed here is saved only once the
/// probe accepts it; Esc or Ctrl+C at any question cancels with nothing saved.
pub(super) fn swap_model(
    provider: &mut Provider,
    target_provider: &str,
    model: &str,
    base_url_override: Option<String>,
) {
    let Some(preset) = config::preset(target_provider) else {
        tui::line(&tui::red(&format!(
            "  ✗ {}",
            unknown_provider_msg(target_provider)
        )));
        return;
    };
    let mut s = config::load_settings().unwrap_or_default();
    let (from, from_url) = (s.provider.clone(), s.base_url.clone());
    let remembered = remembered_endpoint(preset.id);
    let mut model = model.to_string();
    let mut custom_url = base_url_override;
    // For the custom endpoint, Some("") is "no key", not the saved one.
    let mut new_key: Option<String> = None;

    // Custom OpenAI-compatible endpoint: the address and key are asked only
    // for an endpoint not used before; `/model <name>` on a configured one
    // just switches the model.
    if preset.id == "custom" {
        let current = (from == "custom").then_some(from_url.as_deref()).flatten();
        let known = current.or(remembered.as_deref());
        if custom_url.is_none() && known.is_none() {
            let default_url = preset.base_url;
            let Some(url) = tui::ask(&format!(
                "  Endpoint base URL (OpenAI-compatible, usually ends in /v1) [{default_url}]: "
            )) else {
                return swap_cancelled();
            };
            let url = url.trim();
            custom_url = Some(if url.is_empty() {
                default_url.to_string()
            } else {
                url.to_string()
            });
        }
        // Keys are kept per endpoint: a new address gets its own key (or
        // none), never the key of the one before.
        let new_address = custom_url.as_deref().filter(|u| Some(*u) != current);
        if let Some(url) = new_address.filter(|u| config::saved_custom_key(u).is_none()) {
            new_key = Some(match onboarding::ask_custom_key(url) {
                onboarding::KeyAnswer::Key(k) => k,
                onboarding::KeyAnswer::Empty => String::new(),
                onboarding::KeyAnswer::Cancel => return swap_cancelled(),
            });
        }
        if model.is_empty() {
            let Some(m) = tui::ask("  Model name (as the server expects it): ") else {
                return swap_cancelled();
            };
            model = m.trim().to_string();
            if model.is_empty() {
                return swap_cancelled();
            }
        }
    }

    // Missing API key: take it right here instead of failing on the next
    // request with a raw HTTP error. It is saved once the probe accepts it.
    if !preset.env_key.is_empty() && config::load_key(preset.env_key).is_none() {
        tui::line(&tui::yellow(&format!(
            "  {} isn't configured yet — {} is not set.",
            preset.label, preset.env_key
        )));
        tui::line(&tui::dim(
            "  Paste an API key to set it up now (it is checked before it is saved), or Esc to cancel.",
        ));
        match onboarding::read_key(&format!("  {}: ", preset.env_key)) {
            onboarding::KeyAnswer::Key(k) => new_key = Some(k),
            _ => return swap_cancelled(),
        }
    }

    // Ollama: confirm the server is up and actually has the model before
    // committing — the alternative is an opaque failure mid-conversation.
    let mut target = plan_swap(
        &from,
        from_url.as_deref(),
        remembered.as_deref(),
        preset,
        custom_url.as_deref(),
    );
    if preset.id == "ollama" {
        let base = target.base_url.clone();
        let shown_base = tui::sanitize_terminal(&base);
        let installed = provider::ollama_models(&base);
        if installed.is_empty() {
            tui::line(&tui::yellow(&format!(
                "  ✗ can't reach Ollama at {shown_base} (or it has no models)."
            )));
            tui::line(&tui::dim("    1. install: https://ollama.com"));
            tui::line(&tui::dim("    2. start it:  ollama serve"));
            tui::line(&tui::dim(&format!(
                "    3. pull the model:  ollama pull {model}"
            )));
            tui::line(&tui::dim(
                "    then run /model again — keeping the current model.",
            ));
            return;
        }
        let have = installed
            .iter()
            .any(|m| *m == model || m.split(':').next() == Some(model.as_str()));
        if !have {
            tui::line(&tui::yellow(&format!(
                "  ✗ Ollama at {shown_base} is running but '{model}' isn't installed."
            )));
            let shown: Vec<&str> = installed.iter().take(8).map(String::as_str).collect();
            // Model names come from whatever answers on the Ollama port.
            let shown = shown.join(", ");
            tui::line(&tui::dim(&format!(
                "    installed: {}",
                tui::sanitize_terminal(&shown)
            )));
            tui::line(&tui::dim(&format!(
                "    pull it with  ollama pull {model}  — keeping the current model."
            )));
            return;
        }
    }

    if preset.id == "llamacpp" || preset.id == "lmstudio" {
        if let Some(active_url) = ensure_local_gguf_server(&target.base_url, &model) {
            // Another port answered: that server is the one to remember.
            if active_url != target.base_url {
                target = SwapTarget {
                    save: Some(Some(active_url.clone())),
                    base_url: active_url,
                };
            }
        } else {
            // ensure_local_gguf_server has already said why a GGUF file
            // cannot be served; the generic list below would contradict it.
            if gguf_unservable(&model, find_llama_server_binary().is_some()).is_some() {
                tui::line(&tui::dim("    keeping the current model."));
                return;
            }
            tui::line(&tui::yellow(&format!(
                "  ✗ no local server running on ports 8080/1234/11434/8000 for '{model}'."
            )));
            if find_llama_server_binary().is_none() {
                tui::line(&tui::dim(
                    "    llama-server not found. Install it with: brew install llama.cpp",
                ));
            }
            tui::line(&tui::dim(
                "    1. start llama.cpp server:  llama-server -m ~/.models/<model> --port 8080",
            ));
            tui::line(&tui::dim(
                "    2. or start LM Studio local server on port 1234",
            ));
            tui::line(&tui::dim(
                "    then re-run /model — keeping the current model.",
            ));
            return;
        }
    }

    let base_url_change = target.save.clone();
    if let Some(u) = &base_url_change {
        s.base_url = u.clone();
    }
    s.provider = preset.id.to_string();
    s.model = model.to_string();
    let Some(mut p) = probe_swap(&mut s, preset, &mut new_key, &provider.model) else {
        return;
    };
    let key_name = onboarding::key_slot(preset, &p.base_url);
    if let Some(k) = new_key.as_deref().filter(|k| !k.is_empty()) {
        if preset.id == "custom" {
            config::save_custom_key(&p.base_url, k);
            tui::line(&tui::green(&format!(
                "  ✓ key saved for {}",
                tui::sanitize_terminal(&config::endpoint_origin(&p.base_url))
            )));
        } else {
            config::save_key(&key_name, k);
            tui::line(&tui::green(&format!("  ✓ {key_name} saved")));
        }
    }
    if let Some(k) = p.api_key.as_deref() {
        config::record_key_check(&key_name, k, true);
    }
    // The probe's one-token usage mustn't pose as the live prompt
    // size, and a `--effort` given on the command line outlives the swap.
    usage::forget_last();
    p.effort = provider.effort;
    *provider = p;
    let user = config::load_user_settings().unwrap_or_default();
    let endpoints = remember_endpoints(
        &user,
        &from,
        preset.id,
        base_url_change.as_ref().map(|u| u.as_deref()),
    );
    let mut changes = vec![
        ("provider", Some(s.provider.as_str().into())),
        ("model", Some(s.model.as_str().into())),
    ];
    if let Some(u) = base_url_change {
        changes.push(("base_url", u.map(Into::into)));
    }
    if endpoints != user.endpoints {
        changes.push(("endpoints", serde_json::to_value(&endpoints).ok()));
    }
    save_user_settings(&changes);
    provider::prewarm(provider);
    set_active_preset(preset.id);
    tui::set_model_label(&s.model);
    tui::line(&tui::green(&swap_success_line(
        &model,
        &s.model,
        &onboarding::provider_label(preset.id, &provider.base_url),
    )));
}

// The proof half of a swap: builds the provider (a key typed during the swap
// stands in for the saved one) and has it answer a one-token probe, which
// catches bad keys, unknown model names and unreachable servers now instead
// of on the next prompt. Ollama was already checked live by the caller, and
// a probe there could cold-load a large model. A custom endpoint with no
// saved key that answers 401 is asked for one here. None once it has said
// why the current model stays.
pub(super) fn probe_swap(
    s: &mut Settings,
    preset: &config::Preset,
    new_key: &mut Option<String>,
    current_model: &str,
) -> Option<Provider> {
    let keeping = || {
        tui::line(&tui::dim(&format!(
            "    keeping the current model ({current_model})."
        )))
    };
    // `/model nonsense-provider some-model` is one name with a space in it.
    if s.model.split_whitespace().nth(1).is_some() {
        tui::line(&tui::red(&format!(
            "  ✗ '{}' is not a provider or a model name — /model <name>, or /model <provider> <name> with one of {}",
            tui::sanitize_terminal(&s.model),
            config::PRESETS.iter().map(|p| p.id).collect::<Vec<_>>().join(", ")
        )));
        keeping();
        return None;
    }
    loop {
        let mut p = match build_provider_with_key(s, new_key.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                tui::line(&tui::red(&format!(
                    "  ✗ swap failed: {} — keeping the current model; nothing saved.",
                    tui::sanitize_terminal(&e)
                )));
                return None;
            }
        };
        if preset.id == "ollama" {
            return Some(p);
        }
        tui::line(&tui::dim(&format!(
            "  validating {} — one-token probe…",
            s.model
        )));
        let e = match provider::validate(&p) {
            // The server answered only as its fallback name: run and save
            // that, not the name it rejected.
            Ok(Some(new_model)) => {
                s.model = new_model.clone();
                p.model = new_model;
                return Some(p);
            }
            Ok(None) => {
                let note = provider::unlisted_note(&p.model, &provider::served_names(&p));
                if let Some(note) = &note {
                    tui::line(&tui::yellow(&format!("  ⚠ {note}")));
                }
                SWAP_UNCONFIRMED.store(note.is_some(), std::sync::atomic::Ordering::Relaxed);
                return Some(p);
            }
            Err(e) => e,
        };
        let fail = onboarding::Fail::from_error(&e);
        if let onboarding::Fail::KeyRejected(code) = fail {
            // A key typed here and refused is asked for again, as setup and
            // /login do: the next paste must not land in the message box.
            let typed = new_key.as_deref().is_some_and(|k| !k.is_empty());
            let wants = preset.id == "custom" && p.api_key.is_none();
            if typed || wants {
                if let Some(why) = onboarding::plain_http_key_refusal(&p.base_url)
                    .filter(|_| preset.id == "custom")
                {
                    tui::line(&tui::yellow(&format!(
                        "  the endpoint wants an API key (HTTP {code}) — {why}"
                    )));
                    keeping();
                    return None;
                }
                tui::line(&if typed {
                    tui::red(&format!(
                        "  ✗ rejected (HTTP {code}) — not saved. Paste it again, or Esc to cancel"
                    ))
                } else {
                    tui::yellow(&format!(
                        "  the endpoint wants an API key (HTTP {code}) — paste it, or Esc to cancel"
                    ))
                });
                let prompt = if preset.id == "custom" {
                    "  API key for this endpoint: ".to_string()
                } else {
                    format!("  {}: ", preset.env_key)
                };
                match onboarding::read_key(&prompt) {
                    onboarding::KeyAnswer::Key(k) => *new_key = Some(k),
                    _ => {
                        swap_cancelled();
                        return None;
                    }
                }
                continue;
            }
        }
        // One line for a model the server does not know: it names what the
        // server serves, not the raw status and a second hint.
        if fail == onboarding::Fail::ModelMissing {
            let names = provider::served_names(&p);
            tui::line(&tui::red(&format!(
                "  ✗ the server does not know model {}{}",
                tui::sanitize_terminal(&s.model),
                if names.is_empty() {
                    " — check the name".to_string()
                } else {
                    format!(" — it serves: {}", provider::served_summary(&names))
                }
            )));
            keeping();
            return None;
        }
        tui::line(&tui::red(&format!(
            "  ✗ validation failed: {}",
            tui::sanitize_terminal(&e)
        )));
        let hint = match fail {
            onboarding::Fail::KeyRejected(_) => {
                if let Some(k) = &p.api_key {
                    config::record_key_check(&onboarding::key_slot(preset, &p.base_url), k, false);
                }
                match provider::key_source(&p) {
                    // The variable overrides the stored key: /login would not help.
                    Some(provider::KeySource::Env(name)) => format!(
                        "{name} (from your environment) was rejected — check or unset it; /login only changes the stored key, which this variable overrides"
                    ),
                    _ => "the API key was rejected — /login to replace it".to_string(),
                }
            }
            onboarding::Fail::ModelMissing => format!(
                "'{}' doesn't look like a model this provider serves — check the name",
                s.model
            ),
            onboarding::Fail::Unreachable => format!(
                "nothing is answering at {} — start the server, then /model again",
                tui::sanitize_terminal(&p.base_url)
            ),
            onboarding::Fail::Other(_) => "fix the issue above, then /model again".to_string(),
        };
        tui::line(&tui::dim(&format!("    {hint}")));
        keeping();
        return None;
    }
}

pub(super) fn handle_voice(arg: &str) -> Option<String> {
    tui::line(&tui::accent("  voice input"));
    tui::line(&tui::dim(
        "  supported backends: whisper-cpp, whisper-cli, openai-whisper, local models",
    ));
    let audio_path = if arg.trim().is_empty() {
        tui::line(&tui::dim(
            "  Tip: You can drop an audio file (.wav/.mp3/.m4a) directly or pass `/voice <path>`",
        ));
        tui::ask("  path to audio file (or press Enter to check local microphone/whisper): ")
            .unwrap_or_default()
    } else {
        arg.trim().to_string()
    };
    if audio_path.trim().is_empty() {
        let has_whisper = std::process::Command::new("whisper-cpp")
            .arg("--help")
            .output()
            .is_ok()
            || std::process::Command::new("whisper-cli")
                .arg("--help")
                .output()
                .is_ok()
            || std::process::Command::new("whisper")
                .arg("--help")
                .output()
                .is_ok();
        if has_whisper {
            tui::line(&tui::green(
                "  Local whisper binary detected! Ready for voice-to-text transcription.",
            ));
            tui::line(&tui::dim(
                "  To transcribe and run a prompt, use `/voice <path_to_audio_file>`",
            ));
        } else {
            tui::line(&tui::yellow("  No local whisper binary found in PATH."));
            tui::line(&tui::dim("  To enable offline zero-latency voice input, install `whisper-cpp` or `openai-whisper`."));
        }
        None
    } else {
        let path = audio_path.trim();
        if std::path::Path::new(path).exists() {
            tui::line(&format!("  Transcribing audio from {}...", tui::bold(path)));
            let bins = ["whisper-cpp", "whisper-cli", "whisper"];
            for bin in bins {
                if let Ok(_o) = std::process::Command::new(bin)
                    .args(["-f", path, "-otxt"])
                    .output()
                {
                    tui::line(&tui::green(&format!("  ✓ transcribed via {bin}")));
                    let txt_path = format!("{path}.txt");
                    if let Ok(txt) = std::fs::read_to_string(&txt_path) {
                        let _ = std::fs::remove_file(&txt_path);
                        return Some(txt.trim().to_string());
                    }
                    if let Ok(txt) = std::fs::read_to_string(
                        path.replace(".wav", ".txt").replace(".mp3", ".txt"),
                    ) {
                        return Some(txt.trim().to_string());
                    }
                }
            }
            tui::line(&tui::yellow("  Could not transcribe: please ensure `whisper-cpp`, `whisper-cli`, or `whisper` is installed and the audio format is supported."));
            None
        } else {
            tui::line(&tui::red(&format!("  File not found: {path}")));
            None
        }
    }
}

/// A model server /local asks about.
#[derive(Debug, PartialEq)]
pub(super) struct LocalServer {
    pub(super) label: &'static str,
    /// The preset `/model <preset> <name>` switches to.
    pub(super) preset: &'static str,
    pub(super) base: String,
}

/// The servers /local probes: the configured one first (a LAN Ollama, LM
/// Studio on another port), then the addresses /model remembers for local
/// presets, then each local preset at its default address.
pub(super) fn local_servers(settings: &Settings) -> Vec<LocalServer> {
    let label = |id: &str| match id {
        "ollama" => "Ollama",
        "lmstudio" => "LM Studio",
        "llamacpp" => "llama.cpp",
        _ => "OpenAI-compatible server",
    };
    let mut out: Vec<LocalServer> = Vec::new();
    let mut add = |preset: &'static str, base: &str| {
        let root = base.trim_end_matches('/').trim_end_matches("/v1");
        if !out.iter().any(|s| s.base.trim_end_matches("/v1") == root) {
            out.push(LocalServer {
                label: label(preset),
                preset,
                base: base.trim_end_matches('/').to_string(),
            });
        }
    };
    if let (Some(p), Some(base)) = (config::preset(&settings.provider), &settings.base_url) {
        if p.local || (p.id == "custom" && is_loopback_url(base)) {
            add(p.id, base);
        }
    }
    for (id, base) in &settings.endpoints {
        if let Some(p) = config::preset(id) {
            if p.local || (p.id == "custom" && is_loopback_url(base)) {
                add(p.id, base);
            }
        }
    }
    for p in config::PRESETS.iter().filter(|p| p.local) {
        add(p.id, p.base_url);
    }
    out
}

pub(super) fn handle_local(provider: &mut Provider) {
    tui::line(&tui::accent("  local models"));
    let mut settings = config::load_settings().unwrap_or_default();
    // Remembered addresses come from the user's own files only, like a swap.
    settings.endpoints = config::load_user_settings()
        .map(|u| u.endpoints)
        .unwrap_or_default();
    let servers = local_servers(&settings);
    // Every server at once: a dead address costs its timeout, not the sum.
    let found: Vec<Option<Vec<String>>> = std::thread::scope(|scope| {
        let probes: Vec<_> = servers
            .iter()
            .map(|s| {
                scope.spawn(move || match s.preset {
                    "ollama" => provider::ollama_models_checked(&s.base),
                    _ => provider::openai_models_checked(&s.base),
                })
            })
            .collect();
        probes
            .into_iter()
            .map(|h| h.join().ok().flatten())
            .collect()
    });
    let mut suggestions = Vec::new();
    for (s, models) in servers.iter().zip(found) {
        // Names come from whatever answers on that port.
        let shown_base = tui::sanitize_terminal(&s.base).into_owned();
        match models {
            None => tui::line(&tui::dim(&format!(
                "  · {} at {shown_base} — not running",
                s.label
            ))),
            Some(m) if m.is_empty() => tui::line(&format!(
                "  • {} at {shown_base} — {}",
                s.label,
                tui::yellow(if s.preset == "ollama" {
                    "running, no models yet (ollama pull <name>)"
                } else {
                    "running, no model loaded"
                })
            )),
            Some(m) => {
                let names: Vec<String> = m
                    .iter()
                    .map(|n| {
                        let n = tui::sanitize_terminal(n).into_owned();
                        if *n == provider.model {
                            format!("{n} (current)")
                        } else {
                            n
                        }
                    })
                    .collect();
                tui::line(&format!(
                    "  • {} at {shown_base} — {}",
                    tui::green(s.label),
                    names.join(", ")
                ));
                if let Some(first) = m.iter().find(|n| **n != provider.model) {
                    suggestions.push(format!(
                        "/model {} {}",
                        s.preset,
                        tui::sanitize_terminal(first)
                    ));
                }
            }
        }
    }
    let ggufs = crate::local::scan_gguf();
    if !ggufs.is_empty() {
        let llama_server = find_llama_server_binary().is_some();
        tui::line("  GGUF files on disk:");
        for m in &ggufs {
            tui::line(&format!("    - {}", tui::bold(&tui::sanitize_terminal(m))));
        }
        match gguf_unservable(&ggufs[0], llama_server) {
            Some(why) => tui::line(&tui::dim(&format!("    {why}"))),
            None => suggestions.push(format!("/model {}", tui::sanitize_terminal(&ggufs[0]))),
        }
    }
    match suggestions.first() {
        Some(_) => tui::line(&tui::dim(&format!(
            "  switch with: {}",
            suggestions.join("  ·  ")
        ))),
        None => tui::line(&tui::dim(
            "  nothing to switch to yet — start Ollama, LM Studio or llama-server, or put a .gguf file in ~/.buildwithnexus/models",
        )),
    }
}

pub(super) fn handle_rules(cwd: &std::path::Path) {
    tui::line(&tui::accent(
        "  /rules — active engineering constraints & business logic rules",
    ));
    for l in rules_listing(cwd) {
        tui::line(&l);
    }
    tui::line(&tui::dim(
        "  Tip: Add custom JSON rules to `.buildwithnexus/rules/` or use `@rules:<id>` in prompt",
    ));
}

// Rule ids, descriptions and rules-file names come from the checkout: a
// description carrying OSC 52 used to write to the user's clipboard.
pub(super) fn rules_listing(cwd: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let user_dir = config::home().join("rules");
    let (mut engine, user_failures) = crate::rules::RuleEngine::load_with_overrides(&user_dir);
    let rules_dir = cwd.join(".buildwithnexus").join("rules");
    let (loaded, mut failures) = load_workspace_rule_files(&rules_dir);
    failures.extend(user_failures);
    for r in loaded {
        engine.add_rule(r);
    }
    for (name, err) in failures {
        out.push(tui::yellow(&format!(
            "  ⚠ skipped rules file {}: {}",
            tui::sanitize_terminal(&name),
            tui::sanitize_terminal(&err)
        )));
    }
    out.push(format!(
        "  {} active rules loaded for workspace:",
        tui::bold(&engine.rules.len().to_string())
    ));
    for r in &engine.rules {
        let sev_badge = match r.severity {
            crate::rules::Severity::Critical => tui::red("CRITICAL"),
            crate::rules::Severity::High => tui::red("HIGH"),
            crate::rules::Severity::Medium => tui::yellow("MEDIUM"),
            crate::rules::Severity::Low | crate::rules::Severity::Info => tui::dim("INFO/LOW"),
        };
        out.push(format!(
            "  [{sev_badge}] {} — {}{}",
            tui::bold(&tui::sanitize_terminal(&r.id)),
            tui::sanitize_terminal(&r.description),
            if r.enabled {
                String::new()
            } else {
                tui::dim(" (off)")
            }
        ));
    }
    out
}

// Every file in the workspace rules dir, plus one (file name, error) per
// file that failed to load — a broken rules file used to vanish silently.
pub(super) fn load_workspace_rule_files(
    rules_dir: &std::path::Path,
) -> (Vec<crate::rules::Rule>, Vec<(String, String)>) {
    let mut rules = Vec::new();
    let mut failures = Vec::new();
    let Ok(rd) = std::fs::read_dir(rules_dir) else {
        return (rules, failures);
    };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let path_str = path.to_string_lossy();
        match crate::rules::RuleEngine::load_from_file(&path_str) {
            Ok(loaded) => rules.extend(loaded.rules),
            Err(e) => {
                // load_from_file already prefixes the path; keep the message
                // to the reason so the line stays short.
                let reason = [
                    format!("Failed to parse rules file {path_str}: "),
                    format!("Failed to read rules file {path_str}: "),
                ]
                .iter()
                .find_map(|prefix| e.strip_prefix(prefix.as_str()))
                .unwrap_or(&e)
                .to_string();
                failures.push((name, reason));
            }
        }
    }
    (rules, failures)
}

pub(super) fn handle_kb_index(cwd: &std::path::Path) {
    tui::line(&tui::accent(
        "  /kb (/index) — project structured knowledge base & symbol indexing",
    ));
    let mut kb = crate::knowledge::KnowledgeBase::new(&cwd.to_string_lossy());
    tui::line(&format!(
        "  Current knowledge base contains {} entities.",
        tui::bold(&kb.entities.len().to_string())
    ));

    tui::line(&tui::dim(
        "  scanning workspace for source files and symbols…",
    ));
    let mut count = 0;
    tui::set_agent_running(true);
    for rel_path in tools::project_files(cwd) {
        if tui::interrupted() {
            break;
        }
        let ext = rel_path.rsplit_once('.').map_or("", |(_, e)| e);
        if !matches!(ext, "rs" | "js" | "ts" | "py" | "go" | "java" | "c" | "cpp") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(cwd.join(&rel_path)) else {
            continue;
        };
        for (entity_type, name) in content.lines().filter_map(|l| extract_symbol(ext, l)) {
            kb.add_entity(crate::knowledge::Entity {
                id: format!("{name}@{rel_path}"),
                entity_type,
                name,
                path: Some(rel_path.clone()),
                description: Some(format!("Extracted symbol from {rel_path}")),
                metadata: serde_json::json!({"auto_indexed": true}),
                last_updated: crate::knowledge::chrono_now_iso(),
            });
            count += 1;
        }
    }
    tui::set_agent_running(false);
    if let Err(e) = kb.save() {
        tui::line(&tui::red(&format!(
            "  Failed to save knowledge base: {}",
            tui::sanitize_terminal(&e)
        )));
    } else {
        tui::line(&tui::green(&format!("  ✓ indexed {count} symbols")));
        tui::line(&tui::dim(
            "  tip: @kb:<name> or @symbol:<name> injects symbol definitions into prompts",
        ));
    }
}

/// The function or type a source line declares, by the line's own keyword:
/// `fn`/`struct`/`enum` in Rust, `def`/`class` in Python, `function`/`class`
/// in JavaScript and TypeScript. A line-based scan, not a parser; names that
/// are not plain identifiers are dropped.
pub(super) fn extract_symbol(
    ext: &str,
    line: &str,
) -> Option<(crate::knowledge::EntityType, String)> {
    use crate::knowledge::EntityType::{Class, Function};
    let t = line.trim();
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| t.starts_with(p));
    // The text after `kw`, up to the first of `stops`.
    let name_before = |kw: &str, stops: &[char]| {
        let rest = &t[t.find(kw)? + kw.len()..];
        Some(rest[..rest.find(stops)?].trim().to_string())
    };
    // The first word after `kw`, without the punctuation that can follow it.
    let word_after = |kw: &str, trim: &[char]| {
        let rest = &t[t.find(kw)? + kw.len()..];
        Some(
            rest.split_whitespace()
                .next()
                .unwrap_or("")
                .trim_matches(trim)
                .to_string(),
        )
    };
    let (kind, name) = match ext {
        "rs" if starts(&["fn ", "pub fn ", "async fn ", "pub async fn "]) => {
            (Function, name_before("fn ", &['('])?)
        }
        "rs" if starts(&["struct ", "pub struct "]) => {
            (Class, word_after("struct ", &['{', '(', ';'])?)
        }
        "rs" if starts(&["enum ", "pub enum "]) => (Class, word_after("enum ", &['{', '(', ';'])?),
        "py" if t.starts_with("def ") => (Function, name_before("def ", &['('])?),
        "py" if t.starts_with("class ") => (Class, name_before("class ", &['(', ':'])?),
        "js" | "ts" if starts(&["function ", "export function "]) => {
            (Function, name_before("function ", &['('])?)
        }
        "js" | "ts" if starts(&["class ", "export class "]) => {
            (Class, word_after("class ", &['{'])?)
        }
        _ => return None,
    };
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$');
    plain.then_some((kind, name))
}
