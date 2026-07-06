// Library root. The binary is a thin shim; everything lives here so integration
// suites can reach the internals directly.

pub mod agent;
pub mod checkpoint;
pub mod config;
pub mod hooks;
pub mod knowledge;
pub mod local;
pub mod onboarding;
pub mod provider;
pub mod report;
pub mod rules;
pub mod session;
pub mod tools;
pub mod trace;
pub mod tui;
pub mod verifier;
pub mod workflow;

use std::io::IsTerminal;
use std::path::PathBuf;

use agent::Permission;
use config::Settings;
use provider::Msg;
use provider::Provider;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_ATTACHED_FILE_BYTES: u64 = 48 * 1024;

#[derive(Default, Clone)]
struct CliOptions {
    model: Option<String>,
    permission_mode: Option<String>,
    prompt: Option<String>,
}

fn parse_cli_options(args: Vec<String>) -> (CliOptions, Vec<String>) {
    let mut opts = CliOptions::default();
    let mut rest = Vec::new();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        if let Some(v) = arg.strip_prefix("--model=") {
            opts.model = Some(v.to_string());
        } else if arg == "--model" {
            opts.model = it.next();
        } else if let Some(v) = arg.strip_prefix("--permission-mode=") {
            opts.permission_mode = Some(v.to_string());
        } else if let Some(v) = arg.strip_prefix("--permission=") {
            opts.permission_mode = Some(v.to_string());
        } else if arg == "--permission-mode" || arg == "--permission" {
            opts.permission_mode = it.next();
        } else if let Some(v) = arg.strip_prefix("--prompt=") {
            opts.prompt = Some(v.to_string());
        } else if arg == "--prompt" {
            opts.prompt = it.next();
        } else {
            rest.push(arg);
        }
    }
    (opts, rest)
}

pub fn run() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--json") {
        args.retain(|a| a != "--json");
        report::set(report::Mode::Json);
    }
    let (opts, args) = parse_cli_options(args);
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let rest = || args[1..].join(" ");

    match cmd {
        "" => interactive(opts.prompt.clone(), opts),
        "init" | "da-init" | "setup" => {
            onboarding::run();
        }
        "providers" => {
            for p in config::PRESETS {
                let tag = if p.local { "local" } else { "remote" };
                println!("  {:<12} {:<26} {}", p.id, p.label, tag);
            }
        }
        "run" | "build" | "headless" | "--headless" | "-p" | "--print" => headless(&opts, |p, perm, cwd| {
            agent::run_build(p, perm, "engineer", &rest(), &cwd)
        }),
        "plan" => headless(&opts, |p, perm, cwd| {
            agent::run_plan(p, perm, &rest(), &cwd)
        }),
        "brainstorm" => headless(&opts, |p, perm, cwd| {
            agent::run_brainstorm(p, perm, &cwd, &rest()).map(|_| ())
        }),
        "sessions" => {
            for s in session::list() {
                let title: String = s.title.chars().take(48).collect();
                println!("  {}  {:<48}  {}", s.id, title, s.cwd);
            }
        }
        "continue" | "-c" | "--continue" => {
            headless(&opts, |p, perm, cwd| match session::latest() {
                Some(s) => {
                    agent::run_build_resumed(p, perm, "engineer", &rest(), &cwd, s.msgs, &s.id)
                }
                None => Err("no sessions to continue".into()),
            })
        }
        "resume" | "-r" | "--resume" => {
            let id = args.get(1).cloned().unwrap_or_default();
            let task = if args.len() > 2 {
                args[2..].join(" ")
            } else {
                String::new()
            };
            headless(&opts, |p, perm, cwd| match session::load(&id) {
                Some(s) => {
                    agent::run_build_resumed(p, perm, "engineer", &task, &cwd, s.msgs, &s.id)
                }
                None => Err(format!("no session '{id}'")),
            })
        }
        "-v" | "-V" | "--version" | "version" => println!("buildwithnexus {VERSION}"),
        "-h" | "--help" | "help" => usage(),
        "doctor" => run_doctor(),
        _ if !args.is_empty() => interactive(opts.prompt.clone().or_else(|| Some(args.join(" "))), opts),
        other => {
            eprintln!("unknown command: {other}\n");
            usage();
            std::process::exit(2);
        }
    }
}

fn provider_or_onboard(opts: &CliOptions) -> Result<(Provider, Permission), String> {
    let settings = match config::load_settings() {
        Some(s) => s,
        None => onboarding::run().ok_or("setup cancelled")?,
    };
    let mut provider = build_provider(&settings)?;
    if let Some(model) = &opts.model {
        provider.model = model.clone();
    }
    let perm_name = opts
        .permission_mode
        .as_deref()
        .unwrap_or(&settings.permission);
    Ok((provider, agent::permission(perm_name)))
}

pub fn build_provider(s: &Settings) -> Result<Provider, String> {
    let preset = config::preset(&s.provider).ok_or_else(|| {
        format!(
            "unknown provider '{}'; run `buildwithnexus init`",
            s.provider
        )
    })?;
    let base_url = match &s.base_url {
        Some(u) if !preset.env_key.is_empty() && !u.starts_with("https://") => {
            return Err(format!(
                "refusing to send the {} API key to a non-HTTPS endpoint ({u}); use https:// or a local provider",
                preset.env_key
            ));
        }
        Some(u) => u.clone(),
        None => preset.base_url.to_string(),
    };
    let model = if s.model.is_empty() {
        preset.default_model.to_string()
    } else {
        s.model.clone()
    };
    let api_key = if preset.env_key.is_empty() {
        None
    } else {
        config::load_key(preset.env_key)
    };
    if !preset.env_key.is_empty() && api_key.is_none() {
        return Err(format!(
            "{} not set; run `buildwithnexus init`",
            preset.env_key
        ));
    }
    let context_tokens = match preset.id {
        "anthropic" => 200_000,
        _ if preset.local => 8_192,
        _ => 128_000,
    };
    Ok(Provider {
        protocol: preset.protocol,
        base_url,
        model,
        api_key,
        context_tokens,
    })
}

fn headless(
    opts: &CliOptions,
    f: impl FnOnce(&Provider, Permission, PathBuf) -> Result<(), String>,
) {
    let (provider, perm) = match provider_or_onboard(opts) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", tui::red(&e));
            std::process::exit(1);
        }
    };
    provider::prewarm(&provider);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    hooks::init(&cwd, false);
    hooks::notify("SessionStart", &cwd);

    if !report::is_json() {
        println!("{}", tui::bold(&format!("╭── buildwithnexus headless [v{}] ──────────────────────────────────╮", crate::VERSION)));
        println!("│ Provider: {:<24} Model: {:<30} │", provider.protocol, provider.model);
        println!("│ Working Dir: {:<58} │", cwd.display());
        println!("│ Mode: High-Quality Non-Interactive Operational Execution               │");
        println!("╰───────────────────────────────────────────────────────────────────────╯");
        println!();
        check_and_offer_install_dependencies(false);
    }

    let start_time = std::time::Instant::now();
    let r = f(&provider, perm, cwd.clone());
    let elapsed = start_time.elapsed();
    hooks::notify("SessionEnd", &cwd);

    if !report::is_json() {
        println!();
        if r.is_ok() {
            println!("{}", tui::green(&format!("✓ Headless execution completed successfully in {:.2?}.", elapsed)));
        } else {
            println!("{}", tui::red(&format!("✗ Headless execution failed after {:.2?}.", elapsed)));
        }
    }

    if let Err(e) = r {
        eprintln!("{}", tui::red(&e));
        std::process::exit(1);
    }
}

fn interactive(initial_prompt: Option<String>, opts: CliOptions) {
    // Always scaffold on interactive launch so existing users also get the
    // directory skeleton and starter Agents.md if they're missing.
    config::scaffold_home();
    let onboarded = config::load_settings().is_some();
    if !onboarded && onboarding::run().is_none() {
        return;
    }
    let (provider, perm) = match provider_or_onboard(&opts) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", tui::red(&e));
            std::process::exit(1);
        }
    };
    provider::prewarm(&provider);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let raw = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    hooks::init(&cwd, raw);
    hooks::notify("SessionStart", &cwd);
    tui::enter_alt(raw);
    let result = repl(provider, perm, &cwd, raw, initial_prompt);
    tui::leave_alt();
    hooks::notify("SessionEnd", &cwd);
    if let Err(e) = result {
        eprintln!("{}", tui::red(&e));
    }
}

// ── REPL ──────────────────────────────────────────────────────────────────────
fn repl(
    mut provider: Provider,
    mut perm: Permission,
    cwd: &std::path::Path,
    raw: bool,
    initial_prompt: Option<String>,
) -> Result<(), String> {
    let settings = config::load_settings().unwrap_or_default();
    tui::set_permission_mode(permission_label(&perm));

    // Show the full-screen header banner.
    let mode_name = "BUILD"; // default starting mode
    tui::show_banner(
        &settings.provider,
        &provider.model,
        mode_name,
        &cwd.display().to_string(),
    );
    tui::line(&tui::dim(
        "  describe a task · /help for all commands · !<cmd> for shell · Shift+Tab to change mode",
    ));
    check_and_offer_install_dependencies(false);

    let mut transcript: Vec<provider::Msg> = Vec::new();
    let mut sid = session::new_id();
    trace::set_session(&sid);
    let mut mode = Mode::Build;
    let mut last_suggested_mode: Option<&'static str> = None;
    // /btw: extra context injected into the next task without interrupting.
    let mut btw_ctx: Option<String> = None;
    let mut pending_prompt = initial_prompt;

    loop {
        // Tick background workflows and surface any completion notifications.
        if let Some(note) = workflow::tick() {
            tui::line(&tui::green(&note));
        }
        // Prune old done/cancelled workflows, keep last 20.
        workflow::prune(20);

        // Show workflow activity badge if any are pending/running.
        let active = workflow::active_count();
        if active > 0 {
            tui::line(&tui::dim(&format!(
                "  ⟳ {} workflow{} in queue — /workflows to manage",
                active,
                if active == 1 { "" } else { "s" }
            )));
        }

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
                None => return Ok(()),
                Some(tui::InputEvent::CycleMode) => {
                    mode = mode.next();
                    last_suggested_mode = None;
                    tui::show_mode_change(mode_label(&mode));
                    continue;
                }
                Some(tui::InputEvent::Text(t)) => t,
            }
        };
        let mut t = task.trim();
        if t.is_empty() {
            continue;
        }

        // Shell passthrough: `!cmd` runs in the shell directly.
        if let Some(cmd) = t.strip_prefix('!') {
            let cmd = cmd.trim();
            if !cmd.is_empty() {
                let out = tools::run("run_command", &serde_json::json!({ "command": cmd }), cwd);
                for l in out.content.lines() {
                    tui::line(&tui::dim(&format!("  {l}")));
                }
            }
            continue;
        }

        // /mode with an inline argument, e.g. `/mode build`, `/mode 1`.
        if let Some(mode_arg) = t.strip_prefix("/mode ") {
            match mode_arg.trim() {
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
            continue;
        }

        // /permissions with an inline argument, e.g. `/permissions auto`.
        if let Some(perm_arg) = t.strip_prefix("/permissions ") {
            match perm_arg.trim() {
                "ask" | "1" => apply_permission(&mut perm, "ask"),
                "auto" | "2" => apply_permission(&mut perm, "auto"),
                "readonly" | "3" => apply_permission(&mut perm, "readonly"),
                other => tui::line(&tui::red(&format!(
                    "  unknown permission '{other}' — try: ask, auto, readonly"
                ))),
            }
            continue;
        }

        // /mouse on|off — opt into mouse-wheel transcript scrolling. Off allows normal text selection.
        if let Some(mouse_arg) = t.strip_prefix("/mouse ") {
            handle_mouse(Some(mouse_arg.trim()));
            continue;
        }

        // /model with an inline argument — hot-swap the model mid-session.
        if let Some(model_arg) = t.strip_prefix("/model ") {
            let new_model = model_arg.trim();
            if !new_model.is_empty() {
                provider.model = new_model.to_string();
                if let Some(mut s) = config::load_settings() {
                    s.model = new_model.to_string();
                    config::save_settings(&s);
                }
                tui::line(&tui::green(&format!("  ✓ model → {new_model}")));
            }
            continue;
        }

        // /schedule <delay> <task>  e.g. `/schedule 5m git pull && cargo test`
        if let Some(rest) = t.strip_prefix("/schedule ") {
            let rest = rest.trim();
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
        if let Some(rest) = t.strip_prefix("/loop ") {
            let rest = rest.trim();
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

        // /btw <context> — inject context into the next agent turn without stopping current work.
        if let Some(ctx) = t.strip_prefix("/btw ") {
            let ctx = ctx.trim();
            if ctx.is_empty() {
                tui::line(&tui::red(
                    "  usage: /btw <context>  e.g. /btw also update the tests",
                ));
            } else {
                btw_ctx = Some(ctx.to_string());
                tui::line(&tui::dim(&format!(
                    "  ⚑ context queued for next turn: {ctx}"
                )));
            }
            continue;
        }

        if let Some(task) = t.strip_prefix("/plan ") {
            tui::line("");
            if let Err(e) = agent::run_plan(&provider, perm, task.trim(), cwd) {
                tui::line(&tui::red(&format!("  {e}")));
            }
            tui::bell();
            continue;
        }
        if let Some(task) = t.strip_prefix("/build ") {
            tui::line("");
            if let Err(e) = agent::run_build_session(
                &provider,
                perm,
                "engineer",
                task.trim(),
                cwd,
                &mut transcript,
                &sid,
            ) {
                tui::line(&tui::red(&format!("  {e}")));
            }
            tui::bell();
            continue;
        }
        if let Some(task) = t.strip_prefix("/brainstorm ") {
            tui::line("");
            if let Err(e) = agent::run_brainstorm(&provider, perm, cwd, task.trim()).map(|_| ()) {
                tui::line(&tui::red(&format!("  {e}")));
            }
            tui::bell();
            continue;
        }

        match t {
            "/exit" | "/quit" | "exit" => return Ok(()),
            "/clear" => {
                tui::clear();
                continue;
            }
            "/new" => {
                transcript.clear();
                sid = session::new_id();
                trace::set_session(&sid);
                tui::line(&tui::dim("  started a fresh session"));
                continue;
            }
            "/resume" => {
                handle_resume(&mut transcript, &mut sid);
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
                tui::leave_alt();
                onboarding::run();
                tui::enter_alt(raw);
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
            "/review" => {
                tui::line(&tui::accent("  /review — AI code review"));
                tui::line(&tui::dim("  Reviews staged changes (or the last diff). Press Enter to review, or type a focus area."));
                let focus = tui::ask("  focus (optional): ").unwrap_or_default();
                let task = if focus.trim().is_empty() {
                    "Review the current git diff (git diff HEAD and git diff --staged). Summarize what changed, identify bugs, style issues, and potential improvements. Be concise.".to_string()
                } else {
                    format!("Review the current git diff focusing on: {}. Run `git diff HEAD` and `git diff --staged` to see the changes.", focus.trim())
                };
                tui::line("");
                if let Err(e) = agent::run_build_session(
                    &provider,
                    perm,
                    "researcher",
                    &task,
                    cwd,
                    &mut transcript,
                    &sid,
                ) {
                    tui::line(&tui::red(&format!("  {e}")));
                }
                tui::bell();
                continue;
            }
            "/commit" => {
                let task = "Generate a conventional git commit message for the staged changes. Run `git diff --staged` to see what's staged. Then run `git commit -m \"<message>\"` with the generated message. If nothing is staged, remind the user to `git add` files first.";
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
                    tui::line(&tui::red(&format!("  {e}")));
                }
                tui::bell();
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
                    tui::line(&tui::red(&format!("  {e}")));
                }
                tui::bell();
                continue;
            }
            "/workflows" | "/tasks" => {
                handle_workflows();
                continue;
            }
            "/doctor" | "/debug" => {
                handle_doctor_tui();
                continue;
            }
            "/diff" => {
                handle_diff(cwd);
                continue;
            }
            "/context" => {
                handle_context(&transcript, provider.context_tokens);
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
            "/undo" | "/rewind" => {
                handle_undo(cwd, "");
                continue;
            }
            "/grill-me" | "/align" | "/interview" => {
                handle_align(cwd);
                continue;
            }
            "/teamwork" | "/teamwork-preview" | "/swarm" => {
                handle_teamwork();
                continue;
            }
            "/mode" => {
                tui::line(&format!(
                    "  Current mode: {}",
                    tui::mode_badge(mode_label(&mode))
                ));
                tui::line(&tui::dim(
                    "  Tab-complete: /mode plan|build|brainstorm  ·  Shift+Tab to cycle",
                ));
                tui::line("");
                tui::line(&format!("    {}  {}", tui::bold("1"), "plan"));
                tui::line(&format!("    {}  {}", tui::bold("2"), "build"));
                tui::line(&format!("    {}  {}", tui::bold("3"), "brainstorm"));
                tui::line("");
                let pick =
                    tui::ask("  switch to [1/2/3 or name, Enter to keep]: ").unwrap_or_default();
                match pick.trim() {
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
                    _ => {}
                }
                continue;
            }
            "/permissions" => {
                handle_permissions(&mut perm);
                continue;
            }
            "/mouse" => {
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
                handle_skills();
                continue;
            }
            "/tools" => {
                handle_tools();
                continue;
            }
            "/mcp" => {
                handle_mcp();
                continue;
            }
            "/vim" => {
                let current = tui::toggle_vim_mode();
                tui::line(&format!("  Vim modal editing mode is now {}", if current { tui::green("ENABLED [Normal/Insert]") } else { tui::yellow("DISABLED [Standard Emacs/Readline]") }));
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
                handle_verify_audit(cwd);
                continue;
            }
            _ => {}
        }

        if let Some(arg) = t.strip_prefix("/voice") {
            if let Some(voice_text) = handle_voice(arg) {
                if !voice_text.trim().is_empty() {
                    tui::line(&format!("  {} {}", tui::green("✓ Voice input transcribed:"), tui::bold(&voice_text)));
                    task = voice_text;
                    t = task.trim();
                } else {
                    continue;
                }
            } else {
                continue;
            }
        }

        if let Some(arg) = t.strip_prefix("/undo ").or_else(|| t.strip_prefix("/rewind ")) {
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
                if let Some(script) = custom.script {
                    // Shell-quote the script path to guard against spaces (UX-007).
                    let escaped = script.to_string_lossy().replace('\'', "'\"'\"'");
                    let shell_cmd = if cmd_args.is_empty() {
                        format!("'{escaped}'")
                    } else {
                        format!("'{escaped}' {cmd_args}")
                    };
                    let tool_input = serde_json::json!({"command": shell_cmd});
                    // UX-002: script-based custom commands must pass through the
                    // permission gate and PreToolUse hooks just like any run_command.
                    if let hooks::PreDecision::Deny(r) =
                        hooks::pre_tool_use("run_command", &tool_input, cwd)
                    {
                        tui::line(&tui::red(&format!("  blocked by hook: {r}")));
                        tui::bell();
                        continue;
                    }
                    if let Some(reason) = agent::gate(perm, "run_command", &tool_input, cwd) {
                        tui::line(&tui::red(&format!("  {reason}")));
                        tui::bell();
                        continue;
                    }
                    let out = tools::run("run_command", &tool_input, cwd);
                    for l in out.content.lines() {
                        tui::line(&format!("  {l}"));
                    }
                } else {
                    // Inject the skill content as context and run in BUILD mode.
                    let user_input = if cmd_args.is_empty() {
                        t.to_string()
                    } else {
                        format!("{t} {cmd_args}")
                    };
                    let task_with_context =
                        format!("{user_input}\n\n[Skill: {cmd_name}]\n{}", custom.content);
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
                        tui::line(&tui::red(&format!("  {e}")));
                    }
                }
                tui::bell();
                continue;
            }
            // UX-001: unknown slash command — show error instead of falling through to AI.
            if !cmd_name.is_empty() {
                tui::line(&tui::red(&format!(
                    "  unknown command /{cmd_name} — /help for all commands"
                )));
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
            apply_permission(&mut perm, new_perm);
            continue;
        }

        // Mode suggestion: hint once per unique suggestion (don't repeat if user stays in mode).
        suggest_mode_if_mismatch(t, &mode, &mut last_suggested_mode);

        // Extract @path tokens. Images become multimodal attachments; text files
        // are appended into the prompt with optional @file:start-end ranges.
        // its own Msg::User push and uses this multimodal turn instead.
        let (clean_task, image_data) = extract_attachments(t, cwd);
        let n_images = image_data.len();
        if n_images > 0 {
            transcript.push(Msg::UserImages {
                text: clean_task.clone(),
                images: image_data,
            });
            tui::line(&tui::dim(&format!("  attached {n_images} image(s)")));
        }

        // Merge any /btw context queued since the last turn.
        let effective_task = if let Some(ctx) = btw_ctx.take() {
            format!("{}\n\n[btw: {}]", clean_task, ctx)
        } else {
            clean_task.clone()
        };
        let t = effective_task.as_str();

        tui::line("");
        let r = match &mode {
            Mode::Plan => agent::run_plan(&provider, perm, t, cwd),
            Mode::Build => {
                agent::run_build_session(&provider, perm, "engineer", t, cwd, &mut transcript, &sid)
            }
            Mode::Brainstorm => match agent::run_brainstorm(&provider, perm, cwd, t) {
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
                Ok(Some(agent::ModeHint::CycleMode)) => {
                    mode = mode.next();
                    tui::show_mode_change(mode_label(&mode));
                    Ok(())
                }
            },
        };
        if let Err(e) = r {
            tui::line(&tui::red(&format!("  {e}")));
        }
        tui::bell();
    }
}

fn mode_label(mode: &Mode) -> &'static str {
    match mode {
        Mode::Plan => "PLAN",
        Mode::Build => "BUILD",
        Mode::Brainstorm => "BRAINSTORM",
    }
}

// Suggest switching modes when the task phrasing strongly implies a different mode.
// Suppresses the tip if it was already shown for this mode combo in the current session.
fn suggest_mode_if_mismatch(task: &str, current: &Mode, last_suggested: &mut Option<&'static str>) {
    let suggested = classify(task);
    let mismatch = matches!(
        (&suggested, current),
        (Mode::Build, Mode::Brainstorm)
            | (Mode::Plan, Mode::Brainstorm)
            | (Mode::Build, Mode::Plan)
    );
    if mismatch {
        let sug_label = mode_label(&suggested);
        if *last_suggested != Some(sug_label) {
            tui::line(&tui::dim(&format!(
                "  tip: this looks like a {} task — Shift+Tab or /mode to switch",
                sug_label
            )));
            *last_suggested = Some(sug_label);
        }
    } else {
        *last_suggested = None;
    }
}

fn handle_resume(transcript: &mut Vec<provider::Msg>, sid: &mut String) {
    let mut sessions = session::list();
    if sessions.is_empty() {
        tui::line(&tui::dim("  no saved sessions yet"));
        return;
    }
    tui::line(&tui::dim("  recent sessions:"));
    for (i, s) in sessions.iter().take(15).enumerate() {
        tui::line(&format!(
            "  {}  {}",
            tui::bold(&(i + 1).to_string()),
            s.title
        ));
    }
    let pick = tui::ask(&tui::dim("  resume # (Enter to cancel): "))
        .as_deref()
        .map(str::trim)
        .and_then(|x| x.parse::<usize>().ok());
    if let Some(n) = pick {
        if n >= 1 && n <= sessions.len().min(15) {
            let s = sessions.swap_remove(n - 1);
            tui::line(&tui::green(&format!("  ✓ resumed: {}", s.title)));
            *transcript = s.msgs;
            *sid = s.id;
        }
    }
}

fn handle_config(provider: &Provider, perm: Permission, cwd: &std::path::Path) {
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
    if let Err(e) = agent::run_build(provider, perm, "engineer", &full_task, cwd) {
        tui::line(&tui::red(&format!("  {e}")));
    }
}

fn handle_memory(
    provider: &Provider,
    perm: Permission,
    cwd: &std::path::Path,
    transcript: &mut Vec<provider::Msg>,
    sid: &str,
) {
    tui::line(&tui::accent("  /memory — session memory"));
    match config::load_memory() {
        None => tui::line(&tui::dim("  (no memory saved yet)")),
        Some(mem) => {
            tui::line(&tui::dim("  Current memory:"));
            for l in mem.lines() {
                tui::line(&format!("    {l}"));
            }
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
                tui::line(&tui::red(&format!("  {e}")));
            }
        }
        _ => {}
    }
}

fn handle_skills() {
    let skills = config::load_skills();
    if skills.is_empty() {
        tui::line(&tui::dim("  No skills found."));
        tui::line(&tui::dim(&format!(
            "  Add .md files to {}/skills/",
            config::home().display()
        )));
        return;
    }
    let mut items: Vec<(String, String)> = skills
        .into_iter()
        .map(|(name, content)| (format!("/{name}"), content))
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

fn handle_tools() {
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

fn handle_mcp() {
    let mut items = Vec::new();
    if let Some(s) = config::load_settings() {
        for (name, val) in &s.mcp_servers {
            let desc = serde_json::to_string_pretty(val).unwrap_or_else(|_| val.to_string());
            items.push((name.clone(), format!("MCP Server Configuration:\n{desc}")));
        }
    }
    if items.is_empty() {
        tui::line(&tui::dim("  No MCP servers configured in settings.json (mcp_servers)."));
        tui::line(&tui::dim("  Add servers to settings.json to enable enterprise tool dispatch via `mcp_call`."));
        return;
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    tui::browse_items("mcp servers", &items);
}

fn find_custom_command(name: &str) -> Option<config::CustomCommand> {
    config::load_custom_commands()
        .into_iter()
        .find(|c| c.name == name)
}

fn handle_model(provider: &mut Provider) {
    tui::line(&tui::accent("  /model — model hot-swap"));
    tui::line(&format!("  Current: {}", tui::bold(&provider.model)));
    tui::line(&tui::dim(
        "  Tip: /model <name>  e.g. /model claude-opus-4-5",
    ));
    tui::line("");
    let pick = tui::ask("  new model (Enter to keep): ").unwrap_or_default();
    let pick = pick.trim();
    if !pick.is_empty() {
        provider.model = pick.to_string();
        if let Some(mut s) = config::load_settings() {
            s.model = pick.to_string();
            config::save_settings(&s);
        }
        tui::line(&tui::green(&format!("  ✓ model → {pick}")));
    }
}

fn handle_voice(arg: &str) -> Option<String> {
    tui::line(&tui::accent("  /voice — audio transcription & voice input"));
    tui::line("  Supported backends: whisper-cpp, whisper-cli, openai-whisper, local models");
    let audio_path = if arg.trim().is_empty() {
        tui::line(&tui::dim("  Tip: You can drop an audio file (.wav/.mp3/.m4a) directly or pass `/voice <path>`"));
        tui::ask("  path to audio file (or press Enter to check local microphone/whisper): ").unwrap_or_default()
    } else {
        arg.trim().to_string()
    };
    if audio_path.trim().is_empty() {
        let has_whisper = std::process::Command::new("whisper-cpp").arg("--help").output().is_ok()
            || std::process::Command::new("whisper-cli").arg("--help").output().is_ok()
            || std::process::Command::new("whisper").arg("--help").output().is_ok();
        if has_whisper {
            tui::line(&tui::green("  Local whisper binary detected! Ready for voice-to-text transcription."));
            tui::line(&tui::dim("  To transcribe and run a prompt, use `/voice <path_to_audio_file>`"));
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
                if let Ok(_o) = std::process::Command::new(bin).args(["-f", path, "-otxt"]).output() {
                    tui::line(&tui::green(&format!("  Transcription complete via {bin}!")));
                    let txt_path = format!("{path}.txt");
                    if let Ok(txt) = std::fs::read_to_string(&txt_path) {
                        let _ = std::fs::remove_file(&txt_path);
                        return Some(txt.trim().to_string());
                    }
                    if let Ok(txt) = std::fs::read_to_string(path.replace(".wav", ".txt").replace(".mp3", ".txt")) {
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

fn handle_local(_provider: &mut Provider) {
    tui::line(&tui::accent("  /local — local model management & inference optimization"));
    tui::line("  Scanning local servers and model directories...");
    let mut servers = Vec::new();
    if let Ok(o) = std::process::Command::new("curl").args(["-s", "http://localhost:11434/api/tags"]).output() {
        if o.status.success() {
            servers.push("Ollama (port 11434 - ACTIVE)");
        }
    }
    if let Ok(o) = std::process::Command::new("curl").args(["-s", "http://localhost:8080/v1/models"]).output() {
        if o.status.success() {
            servers.push("llama.cpp / vLLM (port 8080 - ACTIVE)");
        }
    }
    if servers.is_empty() {
        tui::line(&tui::dim("  No running local model servers detected on port 11434 (Ollama) or 8080 (llama.cpp/vLLM)."));
    } else {
        for s in servers {
            tui::line(&format!("  • {}", tui::green(s)));
        }
    }
    let models_dir = std::env::var_os("HOME").map(PathBuf::from).map(|h| h.join(".buildwithnexus/models")).unwrap_or_else(|| PathBuf::from(".buildwithnexus/models"));
    if let Ok(rd) = std::fs::read_dir(&models_dir) {
        let ggufs: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".gguf")).collect();
        if !ggufs.is_empty() {
            tui::line(&format!("  Local GGUF models in {}:", models_dir.display()));
            for m in ggufs {
                tui::line(&format!("    - {}", tui::bold(&m)));
            }
        }
    }
    tui::line(&tui::dim("  Tip: Use `/model ollama/llama3` or `/model local/qwen2.5-coder` to switch inference to local models."));
}

fn handle_rules(cwd: &std::path::Path) {
    tui::line(&tui::accent("  /rules — active engineering constraints & business logic rules"));
    let mut engine = crate::rules::RuleEngine::load_defaults();
    let rules_dir = cwd.join(".buildwithnexus").join("rules");
    if let Ok(rd) = std::fs::read_dir(&rules_dir) {
        for e in rd.flatten() {
            if let Ok(loaded) = crate::rules::RuleEngine::load_from_file(&e.path().to_string_lossy()) {
                for r in loaded.rules {
                    engine.add_rule(r);
                }
            }
        }
    }
    tui::line(&format!("  {} active rules loaded for workspace:", tui::bold(&engine.rules.len().to_string())));
    for r in &engine.rules {
        let sev_badge = match r.severity {
            crate::rules::Severity::Critical => tui::red("CRITICAL"),
            crate::rules::Severity::High => tui::red("HIGH"),
            crate::rules::Severity::Medium => tui::yellow("MEDIUM"),
            crate::rules::Severity::Low | crate::rules::Severity::Info => tui::dim("INFO/LOW"),
        };
        tui::line(&format!("  [{sev_badge}] {} — {}", tui::bold(&r.id), r.description));
    }
    tui::line(&tui::dim("  Tip: Add custom JSON/YAML rules to `.buildwithnexus/rules/` or use `@rules:<id>` in prompt"));
}

fn handle_kb_index(cwd: &std::path::Path) {
    tui::line(&tui::accent("  /kb (/index) — project structured knowledge base & symbol indexing"));
    let mut kb = crate::knowledge::KnowledgeBase::new(&cwd.to_string_lossy());
    tui::line(&format!("  Current knowledge base contains {} entities.", tui::bold(&kb.entities.len().to_string())));
    
    tui::line("  Scanning workspace for source files and extracting symbols...");
    let mut count = 0;
    let mut dirs_to_visit = vec![cwd.to_path_buf()];
    while let Some(dir) = dirs_to_visit.pop() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || name == "target" || name == "node_modules" || name == "vendor" || name == "dist" {
                    continue;
                }
                if path.is_dir() {
                    dirs_to_visit.push(path);
                } else if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                    if matches!(ext, "rs" | "js" | "ts" | "py" | "go" | "java" | "c" | "cpp") {
                        let rel_path = path.strip_prefix(cwd).unwrap_or(&path).to_string_lossy().to_string();
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            for line in content.lines() {
                                let trimmed = line.trim();
                                let mut entity_type = None;
                                let mut sym_name = None;
                                if ext == "rs" {
                                    if trimmed.starts_with("fn ") || trimmed.starts_with("pub fn ") || trimmed.starts_with("async fn ") || trimmed.starts_with("pub async fn ") {
                                        entity_type = Some(crate::knowledge::EntityType::Function);
                                        if let Some(idx) = trimmed.find("fn ") {
                                            let rest = &trimmed[idx + 3..];
                                            if let Some(paren) = rest.find('(') {
                                                sym_name = Some(rest[..paren].trim().to_string());
                                            }
                                        }
                                    } else if trimmed.starts_with("struct ") || trimmed.starts_with("pub struct ") {
                                        entity_type = Some(crate::knowledge::EntityType::Class);
                                        if let Some(idx) = trimmed.find("struct ") {
                                            let rest = &trimmed[idx + 7..];
                                            let name_part = rest.split_whitespace().next().unwrap_or("");
                                            sym_name = Some(name_part.trim_matches(|c| c == '{' || c == '(' || c == ';').to_string());
                                        }
                                    } else if trimmed.starts_with("enum ") || trimmed.starts_with("pub enum ") {
                                        entity_type = Some(crate::knowledge::EntityType::Class);
                                        if let Some(idx) = trimmed.find("enum ") {
                                            let rest = &trimmed[idx + 5..];
                                            let name_part = rest.split_whitespace().next().unwrap_or("");
                                            sym_name = Some(name_part.trim_matches(|c| c == '{' || c == '(' || c == ';').to_string());
                                        }
                                    }
                                } else if ext == "py" {
                                    if trimmed.starts_with("def ") {
                                        entity_type = Some(crate::knowledge::EntityType::Function);
                                        if let Some(paren) = trimmed[4..].find('(') {
                                            sym_name = Some(trimmed[4..4 + paren].trim().to_string());
                                        }
                                    } else if trimmed.starts_with("class ") {
                                        entity_type = Some(crate::knowledge::EntityType::Class);
                                        if let Some(paren) = trimmed[6..].find(|c| c == '(' || c == ':') {
                                            sym_name = Some(trimmed[6..6 + paren].trim().to_string());
                                        }
                                    }
                                } else if matches!(ext, "js" | "ts") {
                                    if trimmed.starts_with("function ") || trimmed.starts_with("export function ") {
                                        entity_type = Some(crate::knowledge::EntityType::Function);
                                        if let Some(idx) = trimmed.find("function ") {
                                            let rest = &trimmed[idx + 9..];
                                            if let Some(paren) = rest.find('(') {
                                                sym_name = Some(rest[..paren].trim().to_string());
                                            }
                                        }
                                    } else if trimmed.starts_with("class ") || trimmed.starts_with("export class ") {
                                        entity_type = Some(crate::knowledge::EntityType::Class);
                                        if let Some(idx) = trimmed.find("class ") {
                                            let rest = &trimmed[idx + 6..];
                                            let name_part = rest.split_whitespace().next().unwrap_or("");
                                            sym_name = Some(name_part.trim_matches(|c| c == '{').to_string());
                                        }
                                    }
                                }
                                if let (Some(et), Some(sn)) = (entity_type, sym_name) {
                                    if !sn.is_empty() && sn.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$') {
                                        let id = format!("{sn}@{rel_path}");
                                        kb.add_entity(crate::knowledge::Entity {
                                            id,
                                            entity_type: et,
                                            name: sn,
                                            path: Some(rel_path.clone()),
                                            description: Some(format!("Extracted symbol from {rel_path}")),
                                            metadata: serde_json::json!({"auto_indexed": true}),
                                            relationships: vec![],
                                            last_updated: crate::knowledge::chrono_now_iso(),
                                        });
                                        count += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if let Err(e) = kb.save() {
        tui::line(&tui::red(&format!("  Failed to save knowledge base: {e}")));
    } else {
        tui::line(&tui::green(&format!("  ✓ Successfully scanned workspace and indexed {count} symbols into `.buildwithnexus/knowledge/entities.json`!")));
        tui::line(&tui::dim("  Tip: Use `@kb:<name>` or `@symbol:<name>` in your prompts to inject symbol definitions."));
    }
}

fn handle_verify_audit(cwd: &std::path::Path) {
    tui::line(&tui::accent("  /verify (/audit) — running operational judgment verifier on workspace"));
    
    let mut changed_files = Vec::new();
    if let Ok(o) = std::process::Command::new("git").args(["status", "-s"]).current_dir(cwd).output() {
        if o.status.success() {
            let out = String::from_utf8_lossy(&o.stdout);
            for line in out.lines() {
                if line.len() > 3 {
                    let path = line[3..].trim().to_string();
                    changed_files.push(path);
                }
            }
        }
    }
    if changed_files.is_empty() {
        tui::line(&tui::dim("  No modified git files detected. Running verifier against recent files in workspace..."));
        if let Ok(rd) = std::fs::read_dir(cwd) {
            for e in rd.flatten() {
                if let Some(s) = e.path().to_str() {
                    if !s.contains(".git") && !s.contains("target") && !s.contains("node_modules") {
                        changed_files.push(e.path().to_string_lossy().to_string());
                    }
                }
            }
        }
    }

    let verifier = crate::verifier::Verifier::new(&cwd.to_string_lossy());
    let ctx = crate::verifier::VerificationContext {
        task_description: "Interactive workspace verification and operational audit".to_string(),
        task_type: Some(crate::rules::TaskType::CodeReview),
        changed_files: changed_files.clone(),
        tool_calls: vec![],
        evidence_gathered: vec![],
        tests_added: vec![],
        dependencies_changed: vec![],
        git_diff: None,
    };

    let report = verifier.verify(&ctx);
    let report_str = crate::verifier::Verifier::format_report(&report);
    for line in report_str.lines() {
        if line.starts_with("=== ") {
            tui::line(&tui::bold(line));
        } else if line.contains("PASSED") {
            tui::line(&tui::green(line));
        } else if line.contains("WARNING") || line.contains("MEDIUM") {
            tui::line(&tui::yellow(line));
        } else if line.contains("FAILED") || line.contains("CRITICAL") || line.contains("HIGH") {
            tui::line(&tui::red(line));
        } else {
            tui::line(line);
        }
    }
    tui::line(&tui::dim("  Tip: Use `@rules:<id>` or `/rules` to inspect specific constraints."));
}

fn handle_compact(provider: &Provider, transcript: &mut Vec<provider::Msg>) {
    if transcript.is_empty() {
        tui::line(&tui::dim("  nothing to compact (empty transcript)"));
        return;
    }
    let before = transcript.len();
    let taken = std::mem::take(transcript);
    *transcript = agent::compact_msgs(provider, taken);
    let after = transcript.len();
    tui::line(&tui::green(&format!(
        "  ✓ compacted: {before} → {after} messages"
    )));
}

fn handle_workflows() {
    let snaps = workflow::snapshots();
    if snaps.is_empty() {
        tui::line(&tui::dim(
            "  no workflows yet — /schedule or /loop to create one",
        ));
        return;
    }
    tui::line(&tui::accent("  /workflows — background task manager"));
    tui::line(&tui::dim(
        "  ──────────────────────────────────────────────────────────────",
    ));
    for s in &snaps {
        let status_color = match s.status_str.as_str() {
            "running" => tui::blue(&s.status_str),
            "done" => tui::green(&s.status_str),
            "failed" => tui::red(&s.status_str),
            _ => tui::dim(&s.status_str),
        };
        let elapsed = s
            .elapsed_secs
            .map(|e| format!(" [{e}s]"))
            .unwrap_or_default();
        let iter_label = if s.iteration > 1 {
            format!(" ×{}", s.iteration)
        } else {
            String::new()
        };
        tui::line(&format!(
            "  #{:<3}  {}{}  [{}]  {}{}",
            s.id,
            status_color,
            elapsed,
            s.kind_str,
            tui::dim(&s.task),
            iter_label
        ));
    }
    tui::line(&tui::dim(
        "  ──────────────────────────────────────────────────────────────",
    ));
    tui::line(&tui::dim(
        "  c<id> cancel  ·  i<id> inspect output  ·  Enter dismiss",
    ));
    let action = tui::ask("  action: ").unwrap_or_default();
    let action = action.trim();
    if let Some(rest) = action.strip_prefix('c') {
        if let Ok(id) = rest.trim().parse::<usize>() {
            if workflow::cancel(id) {
                tui::line(&tui::yellow(&format!("  cancelled workflow #{id}")));
            } else {
                tui::line(&tui::dim(&format!(
                    "  workflow #{id} not found or already finished"
                )));
            }
        }
    } else if let Some(rest) = action.strip_prefix('i') {
        if let Ok(id) = rest.trim().parse::<usize>() {
            let lines = workflow::output(id);
            if lines.is_empty() {
                tui::line(&tui::dim(&format!(
                    "  no output captured for workflow #{id}"
                )));
            } else {
                tui::line(&tui::accent(&format!("  workflow #{id} output:")));
                for l in lines.iter().take(100) {
                    tui::line(&format!("    {}", tui::dim(l)));
                }
                if lines.len() > 100 {
                    tui::line(&tui::dim(&format!(
                        "  … ({} more lines)",
                        lines.len() - 100
                    )));
                }
            }
        }
    }
}

// Detect intent to switch agent mode from natural language input.
// Only catches unambiguous switch phrases — not ordinary task verbs like "plan this".
fn detect_mode_switch(t: &str) -> Option<Mode> {
    let l = t.trim().to_lowercase();
    let l = l.trim_end_matches(['!', '.', '?']).trim();

    let verb_prefixes: &[&str] = &[
        "switch to ",
        "switch mode to ",
        "change to ",
        "change mode to ",
        "go to ",
        "set mode to ",
        "set mode ",
    ];
    for prefix in verb_prefixes {
        if let Some(rest) = l.strip_prefix(prefix) {
            let rest = rest.trim().trim_end_matches("mode").trim();
            match rest {
                "plan" | "planning" => return Some(Mode::Plan),
                "build" | "building" | "code" => return Some(Mode::Build),
                "brainstorm" | "brainstorming" => return Some(Mode::Brainstorm),
                _ => {}
            }
        }
    }
    // "use X mode" — the word "mode" makes the intent unambiguous.
    if let Some(rest) = l.strip_prefix("use ") {
        if let Some(name) = rest.trim().strip_suffix(" mode") {
            match name.trim() {
                "plan" | "planning" => return Some(Mode::Plan),
                "build" | "building" | "code" => return Some(Mode::Build),
                "brainstorm" | "brainstorming" => return Some(Mode::Brainstorm),
                _ => {}
            }
        }
    }
    // Bare "X mode" when that's the entire input (2 words or fewer).
    if t.split_whitespace().count() <= 2 {
        let bare = l.trim_end_matches("mode").trim();
        match bare {
            "plan" | "planning" => return Some(Mode::Plan),
            "build" | "building" => return Some(Mode::Build),
            "brainstorm" | "brainstorming" => return Some(Mode::Brainstorm),
            _ => {}
        }
    }
    None
}

// Detect intent to switch permission mode from natural language input.
fn detect_permission_switch(t: &str) -> Option<&'static str> {
    let l = t.trim().to_lowercase();
    let l = l.trim_end_matches(['!', '.', '?']).trim();

    let verb_prefixes: &[&str] = &[
        "switch to ",
        "change to ",
        "change permission to ",
        "set permission to ",
        "set permission ",
        "use ",
    ];
    for prefix in verb_prefixes {
        if let Some(rest) = l.strip_prefix(prefix) {
            let rest = rest
                .trim()
                .trim_end_matches("mode")
                .trim()
                .trim_end_matches("permission")
                .trim();
            match rest {
                "ask" | "confirm" => return Some("ask"),
                "auto" | "yolo" | "approve all" => return Some("auto"),
                "readonly" | "read only" | "read-only" | "safe" => return Some("readonly"),
                _ => {}
            }
        }
    }
    // Bare "use readonly", "use ask" — short and unambiguous.
    if t.split_whitespace().count() <= 3 {
        match l.trim() {
            "readonly" | "read-only" | "read only" => return Some("readonly"),
            "auto permission" | "auto mode" => return Some("auto"),
            "ask permission" | "ask mode" => return Some("ask"),
            _ => {}
        }
    }
    None
}

// Apply a permission string, update the in-session value, and persist to settings.json.
fn apply_permission(perm: &mut Permission, ps: &str) {
    *perm = agent::permission(ps);
    if let Some(mut settings) = config::load_settings() {
        settings.permission = ps.to_string();
        config::save_settings(&settings);
    }
    tui::set_permission_mode(permission_label(perm));
    tui::line(&tui::green(&format!("  ✓ permission: {ps}")));
}

fn permission_label(perm: &Permission) -> &'static str {
    match perm {
        Permission::Ask => "ask",
        Permission::Auto => "auto",
        Permission::ReadOnly => "readonly",
    }
}

fn handle_permissions(perm: &mut Permission) {
    let current = permission_label(perm);
    tui::line(&tui::accent("  /permissions — tool permission mode"));
    tui::line(&format!("  Current: {}", tui::bold(current)));
    tui::line(&tui::dim("  Tab-complete: /permissions ask|auto|readonly"));
    tui::line("");
    tui::line(&format!(
        "    {}  {} — confirm before each file write or command  {}",
        tui::bold("1"),
        tui::bold("ask"),
        tui::dim("(recommended)")
    ));
    tui::line(&format!(
        "    {}  {} — auto-approve all actions                   {}",
        tui::bold("2"),
        tui::bold("auto"),
        tui::dim("(yolo)")
    ));
    tui::line(&format!(
        "    {}  {} — never write files or run commands",
        tui::bold("3"),
        tui::bold("readonly")
    ));
    tui::line("");
    let pick = tui::ask("  choice [1/2/3 or name, Enter to keep]: ").unwrap_or_default();
    match pick.trim() {
        "1" | "ask" => apply_permission(perm, "ask"),
        "2" | "auto" => apply_permission(perm, "auto"),
        "3" | "readonly" => apply_permission(perm, "readonly"),
        _ => {}
    }
}

fn handle_mouse(arg: Option<&str>) {
    let cmd = arg.unwrap_or("").trim();
    match cmd {
        "on" | "enable" => {
            tui::set_mouse_capture(true);
            tui::line(&tui::green(
                "  ✓ mouse: on — wheel scroll enabled; text selection may require Option/Alt",
            ));
        }
        "off" | "disable" => {
            tui::set_mouse_capture(false);
            tui::line(&tui::green(
                "  ✓ mouse: off — normal text selection enabled; use PgUp/PgDn to scroll",
            ));
        }
        "" | "toggle" => {
            let new_state = !tui::mouse_capture_enabled();
            tui::set_mouse_capture(new_state);
            if new_state {
                tui::line(&tui::green(
                    "  ✓ mouse: on — wheel scroll enabled; text selection may require Option/Alt",
                ));
            } else {
                tui::line(&tui::green(
                    "  ✓ mouse: off — normal text selection enabled; use PgUp/PgDn to scroll",
                ));
            }
        }
        "status" => {
            let state = if tui::mouse_capture_enabled() {
                "on"
            } else {
                "off"
            };
            tui::line(&tui::accent("  /mouse — mouse wheel scrolling"));
            tui::line(&format!("  Current: {}", tui::bold(state)));
            tui::line(&tui::dim(
                "  /mouse on enables wheel scrolling; /mouse off restores normal text selection",
            ));
        }
        other => tui::line(&tui::red(&format!(
            "  unknown mouse setting '{other}' — try: on, off, toggle, status"
        ))),
    }
}

fn handle_diff(cwd: &std::path::Path) {
    let out = tools::run(
        "run_command",
        &serde_json::json!({"command": "git diff --stat && git diff --shortstat"}),
        cwd,
    );
    for line in out.content.lines() {
        tui::line(&tui::dim(&format!("  {line}")));
    }
}

fn msg_token_estimate(msgs: &[provider::Msg]) -> usize {
    let chars: usize = msgs
        .iter()
        .map(|m| match m {
            provider::Msg::System(s) | provider::Msg::User(s) => s.len(),
            provider::Msg::UserImages { text, images } => text.len() + images.len() * 1024,
            provider::Msg::Assistant { text, calls } => {
                text.len()
                    + calls
                        .iter()
                        .map(|c| c.input.to_string().len())
                        .sum::<usize>()
            }
            provider::Msg::Tool(results) => results.iter().map(|r| r.content.len()).sum(),
        })
        .sum();
    chars / 4
}

fn handle_context(transcript: &[provider::Msg], total: usize) {
    let used = msg_token_estimate(transcript);
    tui::context_meter(used, total);
    tui::line(&tui::dim(&format!(
        "  {} messages in session",
        transcript.len()
    )));
}

fn handle_checkpoints(cwd: &std::path::Path) {
    let items = checkpoint::list(cwd);
    if items.is_empty() {
        tui::line(&tui::dim("  no checkpoints for this directory"));
        return;
    }
    for cp in items.iter().take(10) {
        tui::line(&format!(
            "  {}  {}  {}",
            tui::bold(&cp.id),
            cp.action,
            cp.path.display()
        ));
    }
}

fn handle_undo(cwd: &std::path::Path, arg: &str) {
    let arg = arg.trim();
    if arg == "git" {
        match checkpoint::git_rollback(cwd) {
            Ok(msg) => tui::line(&tui::green(&format!("  ✓ git reset: {msg}"))),
            Err(e) => tui::line(&tui::red(&format!("  git reset error: {e}"))),
        }
    } else if arg == "all" || arg == "session" {
        let since = checkpoint::now_ms().saturating_sub(24 * 3600 * 1000);
        match checkpoint::undo_all_since(cwd, since) {
            Ok(cps) => {
                tui::line(&tui::green(&format!("  ✓ restored {} files across session:", cps.len())));
                for c in cps {
                    tui::line(&format!("    - {} ({})", c.path.display(), c.action));
                }
            }
            Err(e) => tui::line(&tui::red(&format!("  {e}"))),
        }
    } else if !arg.is_empty() {
        match checkpoint::undo_by_id(cwd, arg) {
            Ok(cp) => tui::line(&tui::green(&format!("  ✓ restored checkpoint {} ({})", cp.id, cp.path.display()))),
            Err(e) => tui::line(&tui::red(&format!("  {e}"))),
        }
    } else {
        match checkpoint::undo_latest(cwd) {
            Ok(cp) => tui::line(&tui::green(&format!("  ✓ restored latest {}", cp.path.display()))),
            Err(e) => tui::line(&tui::red(&format!("  {e}"))),
        }
    }
}

fn handle_align(cwd: &std::path::Path) {
    tui::line(&tui::accent("  /grill-me — Interactive Operational Alignment & Decision Grill"));
    tui::line("  We will conduct an operational alignment review before proceeding with complex modifications.");
    
    let q1 = tui::ask("  1. What is the primary operational risk? [1: Regression | 2: Data Loss | 3: Performance | 4: Security]: ").unwrap_or_default();
    let risk_label = match q1.trim() {
        "2" => "Data Loss",
        "3" => "Performance Degradation",
        "4" => "Security Vulnerability",
        _ => "System Regression",
    };

    let q2 = tui::ask("  2. What is the reversibility of this change? [1: Easy (flag/config) | 2: Moderate (revert) | 3: Hard (db/contract) | 4: Irreversible]: ").unwrap_or_default();
    let rev_label = match q2.trim() {
        "1" => "Easy (Feature Flag / Config)",
        "3" => "Hard (Database Migration / API Contract)",
        "4" => "Irreversible",
        _ => "Moderate (Code Revert)",
    };

    let q3 = tui::ask("  3. What is the target confidence threshold? [1: High (>90%) | 2: Medium (>75%) | 3: Exploratory]: ").unwrap_or_default();
    let conf_label = match q3.trim() {
        "1" => "High (>90%)",
        "3" => "Exploratory / Prototype",
        _ => "Medium (>75%)",
    };

    tui::line(&tui::green("  ✓ Operational alignment recorded!"));
    tui::line(&format!("    • Primary Risk: {}", tui::bold(risk_label)));
    tui::line(&format!("    • Reversibility: {}", tui::bold(rev_label)));
    tui::line(&format!("    • Confidence Threshold: {}", tui::bold(conf_label)));

    let mut kb = crate::knowledge::KnowledgeBase::new(&cwd.to_string_lossy());
    let id = format!("dec-{}", crate::checkpoint::now_ms());
    let entity = crate::knowledge::Entity {
        id: id.clone(),
        entity_type: crate::knowledge::EntityType::ArchitectureDecision,
        name: format!("Operational Alignment ({})", risk_label),
        path: None,
        description: Some(format!("Risk: {}, Reversibility: {}, Confidence: {}", risk_label, rev_label, conf_label)),
        metadata: serde_json::json!({
            "risk": risk_label,
            "reversibility": rev_label,
            "confidence_target": conf_label,
            "timestamp": crate::checkpoint::now_ms()
        }),
        relationships: vec![],
        last_updated: "now".to_string(),
    };
    kb.add_entity(entity);
    let _ = kb.save();
    tui::line(&tui::dim("  Decision recorded into structured knowledge base (.buildwithnexus/knowledge/)."));
}

fn handle_teamwork() {
    tui::line(&tui::accent("  /teamwork-preview — Autonomous Multi-Agent Swarm Preview"));
    tui::line("  When executing complex projects, buildwithnexus orchestrates specialized subagent teams:");
    tui::line(&format!("    • {} — Explores documentation, code graphs, and symbol trees", tui::bold("Researcher Subagent")));
    tui::line(&format!("    • {} — Analyzes logs, stack traces, and test regressions", tui::bold("Debugger Subagent")));
    tui::line(&format!("    • {} — Edits code files, runs migrations, and applies patches", tui::bold("Code Writer Subagent")));
    tui::line(&format!("    • {} — Checks engineering rules, static analysis, and confidence", tui::bold("Verifier Subagent")));
    tui::line(&tui::dim("  Tip: Use `invoke_subagent` in your custom rules/workflows to dispatch tasks to this team."));
}

fn handle_agents() {
    match config::load_agents() {
        Some(agents) => {
            for line in agents.lines().take(80) {
                tui::line(&format!("  {line}"));
            }
        }
        None => tui::line(&tui::dim("  no Agents.md found")),
    }
}

fn handle_doctor_tui() {
    tui::line(&tui::accent(&format!("  buildwithnexus {VERSION} doctor")));
    match config::load_settings() {
        Some(s) => {
            tui::line(&format!("  provider: {}", s.provider));
            tui::line(&format!("  model: {}", s.model));
            tui::line(&format!("  permission: {}", s.permission));
        }
        None => tui::line(&tui::yellow("  settings: not configured")),
    }
    tui::line(&format!("  home: {}", config::home().display()));
    tui::line(&format!(
        "  rust: {}",
        std::process::Command::new("rustc")
            .arg("--version")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "not found".to_string())
    ));
}

fn print_help() {
    tui::line(&tui::accent("  buildwithnexus commands"));
    tui::line(&tui::dim(
        "  ─────────────────────────────────────────────────────────────",
    ));
    tui::line(&format!(
        "  {}  cycle modes (PLAN → BUILD → BRAINSTORM)",
        tui::bold("Shift+Tab")
    ));
    tui::line(&format!(
        "  {}         show/switch mode  {}",
        tui::bold("/mode"),
        tui::dim("[plan|build|brainstorm]")
    ));
    tui::line(&format!(
        "  {}   show/switch tool permissions  {}",
        tui::bold("/permissions"),
        tui::dim("[ask|auto|readonly]")
    ));
    tui::line(&format!(
        "  {}         mouse wheel scrolling  {}",
        tui::bold("/mouse"),
        tui::dim("[on|off]  on by default; off restores normal text selection")
    ));
    tui::line(&format!(
        "  {}        scroll transcript",
        tui::bold("PgUp/PgDn · Alt+↑/↓")
    ));
    tui::line(&tui::dim(
        "               or just say: \"switch to build mode\" / \"use readonly\"",
    ));
    tui::line(&format!(
        "  {}        hot-swap the AI model mid-session",
        tui::bold("/model")
    ));
    tui::line(&format!(
        "  {}      compact context  {}",
        tui::bold("/compact"),
        tui::dim("(free up token budget)")
    ));
    tui::line(&format!(
        "  {}          show current context usage",
        tui::bold("/context")
    ));
    tui::line(&format!(
        "  {}             show current git diff summary",
        tui::bold("/diff")
    ));
    tui::line(&format!(
        "  {}       AI code review of staged git diff",
        tui::bold("/review")
    ));
    tui::line(&format!(
        "  {}       AI-drafted conventional commit message",
        tui::bold("/commit")
    ));
    tui::line(&format!(
        "  {}           AI-drafted PR title + description",
        tui::bold("/pr")
    ));
    tui::line(&format!(
        "  {}    schedule a one-shot workflow  {}",
        tui::bold("/schedule"),
        tui::dim("<delay> <task>")
    ));
    tui::line(&format!(
        "  {}         start a repeating workflow  {}",
        tui::bold("/loop"),
        tui::dim("<interval> <task>")
    ));
    tui::line(&format!(
        "  {}   list and manage background workflows  {}",
        tui::bold("/workflows"),
        tui::dim("(/tasks)")
    ));
    tui::line(&format!(
        "  {}          inject context into next agent turn  {}",
        tui::bold("/btw"),
        tui::dim("<context>")
    ));
    tui::line(&format!(
        "  {}       configure hooks, memory, commands via AI",
        tui::bold("/config")
    ));
    tui::line(&format!(
        "  {}       view/edit session memory",
        tui::bold("/memory")
    ));
    tui::line(&format!(
        "  {}       list available skills and custom commands",
        tui::bold("/skills")
    ));
    tui::line(&format!(
        "  {}        browse callable tools",
        tui::bold("/tools")
    ));
    tui::line(&format!(
        "  {}          inspect configured enterprise MCP servers",
        tui::bold("/mcp")
    ));
    tui::line(&format!(
        "  {}        inspect hooks, tools, skills, and subagents",
        tui::bold("/trace")
    ));
    tui::line(&format!(
        "  {}       show loaded Agents.md context",
        tui::bold("/agents")
    ));
    tui::line(&format!(
        "  {}  list edit checkpoints",
        tui::bold("/checkpoints")
    ));
    tui::line(&format!(
        "  {}         restore checkpoint (<git|all|id>)",
        tui::bold("/undo")
    ));
    tui::line(&format!(
        "  {}     interactive operational alignment & decision grill",
        tui::bold("/grill-me")
    ));
    tui::line(&format!(
        "  {}     autonomous multi-agent swarm preview",
        tui::bold("/teamwork")
    ));
    tui::line(&format!("  {}       diagnose setup", tui::bold("/doctor")));
    tui::line(&format!(
        "  {}          start a fresh session",
        tui::bold("/new")
    ));
    tui::line(&format!(
        "  {}      pick a saved session to resume",
        tui::bold("/resume")
    ));
    tui::line(&format!(
        "  {}         run setup (keys, providers, local models)",
        tui::bold("/init")
    ));
    tui::line(&format!(
        "  {}        clear the screen",
        tui::bold("/clear")
    ));
    tui::line(&format!("  {}         exit", tui::bold("/exit")));
    tui::line(&tui::dim(
        "  ─────────────────────────────────────────────────────────────",
    ));
    tui::line(&tui::dim("  !<cmd>   run a shell command directly"));
    tui::line(&tui::dim(
        "  @<tool>  tab-complete operational tools (@diff, @status, @rules, @kb:, @symbol:, @url:)",
    ));
    tui::line(&format!(
        "  {}          toggle Vim modal editing mode (Normal/Insert)",
        tui::bold("/vim")
    ));
    tui::line(&format!(
        "  {}        audio transcription & offline voice input",
        tui::bold("/voice")
    ));
    tui::line(&format!(
        "  {}        local model server & GGUF management",
        tui::bold("/local")
    ));
    tui::line(&tui::dim(
        "  Tab      autocomplete /commands and their sub-args",
    ));
    tui::line(&tui::dim(
        "  ←→ ^A ^E  move · ^W ^U ^K kill · ^Y yank · ↑↓ history",
    ));
    tui::line(&tui::dim(
        "  ^G open in $EDITOR · ^R reverse search · \\ + Enter = newline",
    ));
    tui::line(&tui::dim(
        "  Tools active in all modes: read_file, run_command, grep, etc.",
    ));
}

// ── Mode ──────────────────────────────────────────────────────────────────────
pub enum Mode {
    Plan,
    Build,
    Brainstorm,
}

impl Mode {
    // Shift+Tab cycles PLAN → BUILD → BRAINSTORM → PLAN.
    pub fn next(&self) -> Mode {
        match self {
            Mode::Plan => Mode::Build,
            Mode::Build => Mode::Brainstorm,
            Mode::Brainstorm => Mode::Plan,
        }
    }
}

// Parse `@path` attachment tokens. Images become multimodal entries; readable
// text files are appended to the prompt. Unreadable tokens are left unchanged.
fn extract_attachments(task: &str, cwd: &std::path::Path) -> (String, Vec<(String, String)>) {
    use std::io::Read;
    let image_exts = ["png", "jpg", "jpeg", "gif", "webp"];
    let mut images: Vec<(String, String)> = Vec::new();
    let mut clean = String::new();
    let mut text_attachments = Vec::new();
    for word in task.split_whitespace() {
        if let Some(raw_path) = word.strip_prefix('@') {
            if raw_path == "diff" || raw_path == "git:diff" {
                if let Ok(o) = std::process::Command::new("git").args(["diff", "HEAD"]).current_dir(cwd).output() {
                    let diff_text = String::from_utf8_lossy(&o.stdout);
                    if !diff_text.trim().is_empty() {
                        text_attachments.push(format!("[git diff HEAD]\n{}", diff_text));
                        if !clean.is_empty() { clean.push(' '); }
                        clean.push_str("[git diff HEAD]");
                        continue;
                    }
                }
            } else if raw_path == "status" || raw_path == "git:status" {
                if let Ok(o) = std::process::Command::new("git").args(["status", "-s"]).current_dir(cwd).output() {
                    let stat_text = String::from_utf8_lossy(&o.stdout);
                    if !stat_text.trim().is_empty() {
                        text_attachments.push(format!("[git status]\n{}", stat_text));
                        if !clean.is_empty() { clean.push(' '); }
                        clean.push_str("[git status]");
                        continue;
                    }
                }
            } else if let Some(kb_query) = raw_path.strip_prefix("kb:") {
                let kb = crate::knowledge::KnowledgeBase::new(&cwd.to_string_lossy());
                let res = kb.search(kb_query);
                if !res.is_empty() {
                    let summary = res.iter().map(|e| format!("Entity: {} ({:?})\nDescription: {}\nPath: {:?}", e.name, e.entity_type, e.description.as_deref().unwrap_or(""), e.path)).collect::<Vec<_>>().join("\n---\n");
                    text_attachments.push(format!("[knowledge base: {}]\n{}", kb_query, summary));
                    if !clean.is_empty() { clean.push(' '); }
                    clean.push_str(&format!("[kb: {}]", kb_query));
                    continue;
                }
            } else if raw_path == "rules" || raw_path.starts_with("rule:") {
                let engine = crate::rules::RuleEngine::load_defaults();
                let rules_summary = engine.rules.iter().map(|r| format!("Rule [{}]: {} (Severity: {})", r.id, r.description, r.severity)).collect::<Vec<_>>().join("\n");
                text_attachments.push(format!("[active engineering rules]\n{}", rules_summary));
                if !clean.is_empty() { clean.push(' '); }
                clean.push_str("[active rules]");
                continue;
            } else if let Some(url) = raw_path.strip_prefix("url:").or_else(|| raw_path.strip_prefix("web:")) {
                if let Ok(o) = std::process::Command::new("curl").args(["-sL", "--max-time", "5", url]).output() {
                    let web_text = String::from_utf8_lossy(&o.stdout);
                    if !web_text.trim().is_empty() {
                        let snippet: String = web_text.chars().take(8000).collect();
                        text_attachments.push(format!("[web: {}]\n{}", url, snippet));
                        if !clean.is_empty() { clean.push(' '); }
                        clean.push_str(&format!("[web: {}]", url));
                        continue;
                    }
                }
            } else if let Some(sym_query) = raw_path.strip_prefix("symbol:") {
                if let Ok(o) = std::process::Command::new("grep").args(["-rnI", sym_query, "."]).current_dir(cwd).output() {
                    let sym_text = String::from_utf8_lossy(&o.stdout);
                    if !sym_text.trim().is_empty() {
                        let snippet: String = sym_text.lines().take(30).collect::<Vec<_>>().join("\n");
                        text_attachments.push(format!("[symbol search: {}]\n{}", sym_query, snippet));
                        if !clean.is_empty() { clean.push(' '); }
                        clean.push_str(&format!("[symbol: {}]", sym_query));
                        continue;
                    }
                }
            }
            let (raw_path, range) = split_attachment_range(raw_path);
            let ext = raw_path.rsplit('.').next().unwrap_or("").to_lowercase();
            let p = if let Some(rest) = raw_path.strip_prefix("~/") {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| cwd.to_path_buf())
                    .join(rest)
            } else if raw_path.starts_with('/') {
                PathBuf::from(raw_path)
            } else {
                cwd.join(raw_path)
            };
            if image_exts.contains(&ext.as_str()) {
                if let Ok(mut f) = std::fs::File::open(&p) {
                    let mut buf = Vec::new();
                    if f.read_to_end(&mut buf).is_ok() {
                        let media_type = match ext.as_str() {
                            "jpg" | "jpeg" => "image/jpeg",
                            "gif" => "image/gif",
                            "webp" => "image/webp",
                            _ => "image/png",
                        };
                        images.push((media_type.to_string(), base64_encode(&buf)));
                        if !clean.is_empty() {
                            clean.push(' ');
                        }
                        clean.push_str(&format!(
                            "[image: {}]",
                            p.file_name().unwrap_or_default().to_string_lossy()
                        ));
                        continue;
                    }
                }
            } else if let Some(text) = read_text_attachment(&p, range) {
                text_attachments.push(format!("[file: {}]\n{}", p.display(), text));
                if !clean.is_empty() {
                    clean.push(' ');
                }
                clean.push_str(&format!("[file: {}]", p.display()));
                continue;
            }
        }
        if !clean.is_empty() {
            clean.push(' ');
        }
        clean.push_str(word);
    }
    if !text_attachments.is_empty() {
        clean.push_str("\n\n[attached files]\n");
        clean.push_str(&text_attachments.join("\n\n"));
    }
    (clean, images)
}

fn split_attachment_range(raw: &str) -> (&str, Option<(usize, usize)>) {
    let Some((path, suffix)) = raw.rsplit_once(':') else {
        return (raw, None);
    };
    let parse_line = |s: &str| s.parse::<usize>().ok().filter(|n| *n > 0);
    if let Some((a, b)) = suffix.split_once('-') {
        if let (Some(start), Some(end)) = (parse_line(a), parse_line(b)) {
            return (path, Some((start, end.max(start))));
        }
    } else if let Some(line) = parse_line(suffix) {
        return (path, Some((line, line)));
    }
    (raw, None)
}

fn read_text_attachment(path: &std::path::Path, range: Option<(usize, usize)>) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_ATTACHED_FILE_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let Some((start, end)) = range else {
        return Some(text);
    };
    Some(
        text.lines()
            .enumerate()
            .filter_map(|(i, line)| {
                let line_no = i + 1;
                if line_no >= start && line_no <= end {
                    Some(line)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn base64_encode(data: &[u8]) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = if chunk.len() > 1 {
            chunk[1] as usize
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            chunk[2] as usize
        } else {
            0
        };
        out.push(ALPHA[b0 >> 2] as char);
        out.push(ALPHA[((b0 & 3) << 4) | (b1 >> 4)] as char);
        out.push(if chunk.len() > 1 {
            ALPHA[((b1 & 0xf) << 2) | (b2 >> 6)] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHA[b2 & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

// Suggest a mode from the task phrasing (used for the "tip" hint, not a gate).
pub fn classify(task: &str) -> Mode {
    let l = task.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| l.contains(*w));
    if has(&[
        "what should",
        "what if",
        "ideas for",
        "what do you think",
        "why would",
        "why is",
        "why does",
        "how about",
        "what are the options",
        "advice on",
        "suggest",
        "tradeoffs",
    ]) {
        return Mode::Brainstorm;
    }
    if has(&[
        "plan",
        "design",
        "architect",
        "break down",
        "roadmap",
        "scope",
    ]) {
        return Mode::Plan;
    }
    if has(&[
        "build",
        "create",
        "add",
        "fix",
        "implement",
        "write",
        "refactor",
        "run",
        "make",
    ]) {
        return Mode::Build;
    }
    if task.split_whitespace().count() > 8 {
        Mode::Plan
    } else {
        Mode::Build
    }
}

fn usage() {
    println!(
        "buildwithnexus {VERSION} — agentic AI CLI harness\n\n\
         USAGE:\n\
         \x20 buildwithnexus                 interactive session (all modes, full TUI)\n\
         \x20 buildwithnexus run <task>      execute a task (agentic BUILD loop)\n\
         \x20 buildwithnexus plan <task>     decompose, approve, then execute\n\
         \x20 buildwithnexus brainstorm <q>  chat with tools (grep, fetch, read, etc.)\n\
         \x20 buildwithnexus continue <task> continue the most recent session\n\
         \x20 buildwithnexus resume <id> <t> resume a specific session\n\
         \x20 buildwithnexus sessions        list saved sessions\n\
         \x20 buildwithnexus init            (re)configure provider / model / key\n\
         \x20 buildwithnexus providers       list built-in providers\n\
         \x20 buildwithnexus doctor          diagnose setup (keys, tools, connectivity)\n\
         \x20 buildwithnexus version | help\n\n\
         INTERACTIVE:\n\
         \x20 Shift+Tab              cycle mode (PLAN → BUILD → BRAINSTORM → PLAN)\n\
         \x20 /mode [plan|build|brainstorm]    show or switch mode\n\
         \x20 /model [name]                    hot-swap the AI model\n\
         \x20 /permissions [ask|auto|readonly] show or switch tool permission level\n\
         \x20   or say: \"switch to build mode\" / \"use readonly\"\n\
         \x20 /compact               compress context to free up token budget\n\
         \x20 /context               show current context usage\n\
         \x20 /diff                  show current git diff summary\n\
         \x20 /review                AI code review of staged git diff\n\
         \x20 /commit                AI-drafted conventional commit message\n\
         \x20 /pr                    AI-drafted pull request title + description\n\
         \x20 /schedule <delay> <t>  one-shot workflow  (e.g. /schedule 5m cargo test)\n\
         \x20 /loop <interval> <t>   repeating workflow (e.g. /loop 30m cargo test)\n\
         \x20 /workflows /tasks      list and manage background workflows\n\
         \x20 /btw <context>         inject context into next agent turn\n\
         \x20 /config                configure hooks, memory, commands via AI\n\
         \x20 /memory                view and edit session memory\n\
         \x20 /skills                browse available skills and custom commands\n\
         \x20 /tools                 browse callable tools\n\
         \x20 /trace                 inspect hooks, tools, skills, and subagents\n\
         \x20 /agents /checkpoints /undo /doctor\n\
         \x20 /help /clear /new /resume /init /exit\n\
         \x20 !<cmd>                 run shell command directly\n\
         \x20 @<path>                Tab-complete a file path\n\
         \x20 Tab                    autocomplete /commands and sub-args\n"
    );
}

fn run_doctor() {
    println!("buildwithnexus {VERSION} — doctor");
    println!();

    // Settings
    match config::load_settings() {
        None => println!("  ✗ settings       not found — run `buildwithnexus init`"),
        Some(s) => {
            println!(
                "  ✓ settings       provider={} model={} permission={}",
                s.provider,
                if s.model.is_empty() {
                    "(default)"
                } else {
                    &s.model
                },
                s.permission
            );
        }
    }

    // API key
    for preset in config::PRESETS
        .iter()
        .filter(|p| !p.env_key.is_empty() && !p.local)
    {
        match config::load_key(preset.env_key) {
            Some(_) => println!("  ✓ {}  set", preset.env_key),
            None => println!(
                "  ✗ {}  not set (needed for {})",
                preset.env_key, preset.label
            ),
        }
    }

    // Memory
    match config::load_memory() {
        None => println!("  ·  memory.md     (empty)"),
        Some(m) => println!("  ✓ memory.md      {} chars", m.len()),
    }

    // External tools
    let tools_to_check = [
        ("git", "version control"),
        ("cargo", "Rust build tool"),
        ("node", "Node.js runtime"),
        ("npm", "Node package manager"),
        ("python3", "Python runtime"),
        ("gh", "GitHub CLI (optional)"),
        ("docker", "Docker (optional)"),
        ("rg", "ripgrep (fast search, optional)"),
    ];
    for (bin, label) in &tools_to_check {
        let found = std::process::Command::new("which")
            .arg(bin)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        let glyph = if found { "✓" } else { "·" };
        println!("  {glyph} {bin:<12} {label}");
    }

    // Connectivity (quick HEAD to detect outbound network)
    println!();
    println!("  checking connectivity...");
    let reachable = std::process::Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "5",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "https://api.anthropic.com",
        ])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|code| code.trim() != "000")
        .unwrap_or(false);
    if reachable {
        println!("  ✓ api.anthropic.com reachable");
    } else {
        println!("  ✗ api.anthropic.com unreachable — check firewall / proxy");
    }

    println!();
    check_and_offer_install_dependencies(true);
    println!();
    println!("  Run `buildwithnexus init` to fix any missing configuration.");
}

pub fn check_and_offer_install_dependencies(interactive: bool) {
    let tools_to_check = [
        ("git", "git", "git", "Version control & workspace tracking"),
        ("rg", "ripgrep", "ripgrep", "High-speed semantic file searching"),
        ("node", "node", "nodejs", "Node.js runtime & MCP servers"),
        ("npm", "node", "npm", "Node package manager"),
        ("python3", "python", "python3", "Python runtime & data scripting"),
    ];

    let mut missing = Vec::new();
    for (bin, brew_pkg, apt_pkg, desc) in &tools_to_check {
        let found = std::process::Command::new("which")
            .arg(bin)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !found {
            missing.push((*bin, *brew_pkg, *apt_pkg, *desc));
        }
    }

    if missing.is_empty() {
        if interactive {
            tui::line(&tui::green("  ✓ All required OOTB dependencies (git, rg, node, npm, python3) are installed!"));
        }
        return;
    }

    tui::line(&tui::yellow(&format!("  ⚠ Missing {} OOTB development tool(s):", missing.len())));
    for (bin, _, _, desc) in &missing {
        tui::line(&format!("    • {} — {}", tui::bold(bin), desc));
    }

    if !interactive {
        tui::line(&tui::dim("  Tip: Run `buildwithnexus init` or `buildwithnexus doctor` to auto-install missing dependencies."));
        return;
    }

    let brew_available = std::process::Command::new("which").arg("brew").output().map(|o| o.status.success()).unwrap_or(false);
    let apt_available = std::process::Command::new("which").arg("apt-get").output().map(|o| o.status.success()).unwrap_or(false);

    for (bin, brew_pkg, apt_pkg, desc) in missing {
        let ask_msg = format!("  Would you like to install '{}' ({}) now? [Y/n]: ", bin, desc);
        let ans = match tui::ask(&ask_msg) {
            Some(a) => a.trim().to_lowercase(),
            None => break,
        };
        if ans == "n" || ans == "no" {
            tui::line(&tui::dim(&format!("    Skipped installing {bin}.")));
            continue;
        }

        if brew_available {
            tui::line(&tui::accent(&format!("    Running `brew install {brew_pkg}`...")));
            let res = std::process::Command::new("brew").args(["install", brew_pkg]).status();
            match res {
                Ok(s) if s.success() => tui::line(&tui::green(&format!("    ✓ Successfully installed {bin}!"))),
                _ => tui::line(&tui::red(&format!("    ✗ Failed to install {brew_pkg} via Homebrew."))),
            }
        } else if apt_available {
            tui::line(&tui::accent(&format!("    Running `sudo apt-get install -y {apt_pkg}`...")));
            let res = std::process::Command::new("sudo").args(["apt-get", "install", "-y", apt_pkg]).status();
            match res {
                Ok(s) if s.success() => tui::line(&tui::green(&format!("    ✓ Successfully installed {bin}!"))),
                _ => tui::line(&tui::red(&format!("    ✗ Failed to install {apt_pkg} via apt-get."))),
            }
        } else {
            tui::line(&tui::yellow(&format!("    Neither Homebrew nor apt-get found. Please install '{bin}' manually.")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_brainstorm_phrases() {
        assert!(matches!(
            classify("what should I name this?"),
            Mode::Brainstorm
        ));
        assert!(matches!(
            classify("any ideas for the API?"),
            Mode::Brainstorm
        ));
        assert!(matches!(classify("why is this slow"), Mode::Brainstorm));
    }

    #[test]
    fn classify_plan_phrases() {
        assert!(matches!(classify("design the auth system"), Mode::Plan));
        assert!(matches!(classify("architect a new module"), Mode::Plan));
        assert!(matches!(classify("break down the migration"), Mode::Plan));
    }

    #[test]
    fn classify_build_phrases() {
        assert!(matches!(classify("add a login button"), Mode::Build));
        assert!(matches!(classify("fix the off-by-one bug"), Mode::Build));
        assert!(matches!(classify("refactor the parser"), Mode::Build));
    }

    #[test]
    fn classify_short_defaults_build() {
        assert!(matches!(classify("foo bar"), Mode::Build));
    }

    #[test]
    fn classify_long_unmatched_defaults_plan() {
        assert!(matches!(
            classify("the quick brown fox jumps over the lazy sleeping dog today"),
            Mode::Plan
        ));
    }

    #[test]
    fn classify_is_case_insensitive() {
        assert!(matches!(classify("DESIGN the system"), Mode::Plan));
    }

    #[test]
    fn mode_cycles_correctly() {
        assert!(matches!(Mode::Plan.next(), Mode::Build));
        assert!(matches!(Mode::Build.next(), Mode::Brainstorm));
        assert!(matches!(Mode::Brainstorm.next(), Mode::Plan));
    }

    #[test]
    fn detect_mode_switch_verb_prefixes() {
        assert!(matches!(
            detect_mode_switch("switch to plan mode"),
            Some(Mode::Plan)
        ));
        assert!(matches!(
            detect_mode_switch("change to build"),
            Some(Mode::Build)
        ));
        assert!(matches!(
            detect_mode_switch("go to brainstorm"),
            Some(Mode::Brainstorm)
        ));
        assert!(matches!(
            detect_mode_switch("set mode to planning"),
            Some(Mode::Plan)
        ));
        assert!(matches!(
            detect_mode_switch("use build mode"),
            Some(Mode::Build)
        ));
        assert!(matches!(
            detect_mode_switch("use brainstorm mode"),
            Some(Mode::Brainstorm)
        ));
    }

    #[test]
    fn detect_mode_switch_bare_short_form() {
        assert!(matches!(detect_mode_switch("plan mode"), Some(Mode::Plan)));
        assert!(matches!(
            detect_mode_switch("build mode"),
            Some(Mode::Build)
        ));
        assert!(matches!(
            detect_mode_switch("brainstorm mode"),
            Some(Mode::Brainstorm)
        ));
        assert!(matches!(detect_mode_switch("planning"), Some(Mode::Plan)));
    }

    #[test]
    fn detect_mode_switch_no_false_positives() {
        assert!(detect_mode_switch("build me a todo app").is_none());
        assert!(detect_mode_switch("plan the migration carefully").is_none());
        assert!(detect_mode_switch("let's brainstorm some ideas").is_none());
        assert!(detect_mode_switch("use this library instead").is_none());
    }

    #[test]
    fn detect_permission_switch_verb_prefixes() {
        assert_eq!(
            detect_permission_switch("switch to readonly"),
            Some("readonly")
        );
        assert_eq!(detect_permission_switch("change to auto"), Some("auto"));
        assert_eq!(
            detect_permission_switch("set permission to ask"),
            Some("ask")
        );
        assert_eq!(
            detect_permission_switch("use readonly mode"),
            Some("readonly")
        );
    }

    #[test]
    fn parse_cli_options_extracts_model_and_permission() {
        let (opts, rest) = parse_cli_options(vec![
            "--model".into(),
            "qwen3".into(),
            "--permission-mode=acceptEdits".into(),
            "fix".into(),
            "tests".into(),
        ]);
        assert_eq!(opts.model.as_deref(), Some("qwen3"));
        assert_eq!(opts.permission_mode.as_deref(), Some("acceptEdits"));
        assert_eq!(rest, vec!["fix", "tests"]);
    }

    #[test]
    fn attachment_range_parsing() {
        assert_eq!(
            split_attachment_range("src/lib.rs:10-12"),
            ("src/lib.rs", Some((10, 12)))
        );
        assert_eq!(
            split_attachment_range("src/lib.rs:5"),
            ("src/lib.rs", Some((5, 5)))
        );
        assert_eq!(
            split_attachment_range("src/lib.rs:nope"),
            ("src/lib.rs:nope", None)
        );
    }

    #[test]
    fn text_attachment_context_is_extracted() {
        let dir = std::env::temp_dir().join(format!("bwn-attach-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "one\ntwo\nthree\n").unwrap();

        let (text, images) = extract_attachments("please read @src/lib.rs:2-3", &dir);
        assert!(images.is_empty());
        assert!(text.contains("[file:"));
        assert!(text.contains("two\nthree"));
        assert!(!text.contains("\none\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_provider_rejects_http_for_keyed_preset() {
        let s = Settings {
            provider: "openai".into(),
            model: String::new(),
            permission: "ask".into(),
            base_url: Some("http://insecure.local/v1".into()),
            allowed_commands: Vec::new(),
            ..Default::default()
        };
        match build_provider(&s) {
            Err(e) => assert!(e.contains("non-HTTPS")),
            Ok(_) => panic!("expected http base_url to be rejected"),
        }
    }

    #[test]
    fn build_provider_unknown_provider() {
        let s = Settings {
            provider: "does-not-exist".into(),
            model: String::new(),
            permission: "ask".into(),
            base_url: None,
            allowed_commands: Vec::new(),
            ..Default::default()
        };
        match build_provider(&s) {
            Err(e) => assert!(e.contains("unknown provider")),
            Ok(_) => panic!("expected unknown provider error"),
        }
    }

    #[test]
    fn build_provider_local_preset_allows_http() {
        let s = Settings {
            provider: "ollama".into(),
            model: String::new(),
            permission: "ask".into(),
            base_url: Some("http://localhost:11434/v1".into()),
            allowed_commands: Vec::new(),
            ..Default::default()
        };
        match build_provider(&s) {
            Ok(p) => {
                assert!(p.api_key.is_none());
                assert_eq!(p.model, "llama3.2");
            }
            Err(e) => panic!("local http should build: {e}"),
        }
    }

    #[test]
    fn test_handle_voice_missing_file_returns_none() {
        let res = super::handle_voice("/nonexistent/audio/path.wav");
        assert!(res.is_none());
    }

    #[test]
    fn test_handle_kb_index_and_rules() {
        let dir = std::env::temp_dir().join(format!("bwn-kb-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test_code.rs");
        std::fs::write(&file_path, "pub fn authenticate_user() {}\npub struct SessionData {}").unwrap();
        super::handle_kb_index(&dir);
        
        let kb = crate::knowledge::KnowledgeBase::new(&dir.to_string_lossy());
        assert!(!kb.entities.is_empty());
        assert!(kb.entities.values().any(|e| e.name == "authenticate_user"));
        assert!(kb.entities.values().any(|e| e.name == "SessionData"));

        super::handle_rules(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_handle_verify_audit() {
        let dir = std::env::temp_dir().join(format!("bwn-verify-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test_file.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();
        super::handle_verify_audit(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_check_and_offer_install_dependencies() {
        super::check_and_offer_install_dependencies(false);
    }
}
