// Library root. The binary is a thin shim; everything lives here so integration
// suites can reach the internals directly.

pub mod agent;
pub mod config;
pub mod hooks;
pub mod local;
pub mod onboarding;
pub mod provider;
pub mod report;
pub mod session;
pub mod tools;
pub mod tui;

use std::io::IsTerminal;
use std::path::PathBuf;

use agent::Permission;
use config::Settings;
use provider::Msg;
use provider::Provider;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn run() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--json") {
        args.retain(|a| a != "--json");
        report::set(report::Mode::Json);
    }
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let rest = || args[1..].join(" ");

    match cmd {
        "" => interactive(),
        "init" | "da-init" | "setup" => {
            onboarding::run();
        }
        "providers" => {
            for p in config::PRESETS {
                let tag = if p.local { "local" } else { "remote" };
                println!("  {:<12} {:<26} {}", p.id, p.label, tag);
            }
        }
        "run" | "build" => headless(|p, perm, cwd| agent::run_build(p, perm, "engineer", &rest(), &cwd)),
        "plan" => headless(|p, perm, cwd| agent::run_plan(p, perm, &rest(), &cwd)),
        "brainstorm" => headless(|p, perm, cwd| {
            agent::run_brainstorm(p, perm, &cwd, &rest()).map(|_| ())
        }),
        "sessions" => {
            for s in session::list() {
                let title: String = s.title.chars().take(48).collect();
                println!("  {}  {:<48}  {}", s.id, title, s.cwd);
            }
        }
        "continue" => headless(|p, perm, cwd| match session::latest() {
            Some(s) => agent::run_build_resumed(p, perm, "engineer", &rest(), &cwd, s.msgs, &s.id),
            None => Err("no sessions to continue".into()),
        }),
        "resume" => {
            let id = args.get(1).cloned().unwrap_or_default();
            let task = if args.len() > 2 { args[2..].join(" ") } else { String::new() };
            headless(|p, perm, cwd| match session::load(&id) {
                Some(s) => agent::run_build_resumed(p, perm, "engineer", &task, &cwd, s.msgs, &s.id),
                None => Err(format!("no session '{id}'")),
            })
        }
        "-v" | "-V" | "--version" | "version" => println!("buildwithnexus {VERSION}"),
        "-h" | "--help" | "help" => usage(),
        other => {
            eprintln!("unknown command: {other}\n");
            usage();
            std::process::exit(2);
        }
    }
}

fn provider_or_onboard() -> Result<(Provider, Permission), String> {
    let settings = match config::load_settings() {
        Some(s) => s,
        None => onboarding::run().ok_or("setup cancelled")?,
    };
    Ok((build_provider(&settings)?, agent::permission(&settings.permission)))
}

pub fn build_provider(s: &Settings) -> Result<Provider, String> {
    let preset = config::preset(&s.provider).ok_or_else(|| format!("unknown provider '{}'; run `buildwithnexus init`", s.provider))?;
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
    let model = if s.model.is_empty() { preset.default_model.to_string() } else { s.model.clone() };
    let api_key = if preset.env_key.is_empty() { None } else { config::load_key(preset.env_key) };
    if !preset.env_key.is_empty() && api_key.is_none() {
        return Err(format!("{} not set; run `buildwithnexus init`", preset.env_key));
    }
    let context_tokens = match preset.id {
        "anthropic" => 200_000,
        _ if preset.local => 8_192,
        _ => 128_000,
    };
    Ok(Provider { protocol: preset.protocol, base_url, api_key, model, context_tokens })
}

fn headless(f: impl FnOnce(&Provider, Permission, PathBuf) -> Result<(), String>) {
    let (provider, perm) = match provider_or_onboard() {
        Ok(v) => v,
        Err(e) => { eprintln!("{}", tui::red(&e)); std::process::exit(1); }
    };
    provider::prewarm(&provider);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    hooks::init(&cwd, false);
    hooks::notify("SessionStart", &cwd);
    let r = f(&provider, perm, cwd.clone());
    hooks::notify("SessionEnd", &cwd);
    if let Err(e) = r {
        eprintln!("{}", tui::red(&e));
        std::process::exit(1);
    }
}

fn interactive() {
    // Always scaffold on interactive launch so existing users also get the
    // directory skeleton and starter Agents.md if they're missing.
    config::scaffold_home();
    let onboarded = config::load_settings().is_some();
    if !onboarded && onboarding::run().is_none() {
        return;
    }
    let (provider, perm) = match provider_or_onboard() {
        Ok(v) => v,
        Err(e) => { eprintln!("{}", tui::red(&e)); std::process::exit(1); }
    };
    provider::prewarm(&provider);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let raw = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    hooks::init(&cwd, raw);
    hooks::notify("SessionStart", &cwd);
    tui::enter_alt(raw);
    let result = repl(&provider, perm, &cwd, raw);
    tui::leave_alt();
    hooks::notify("SessionEnd", &cwd);
    if let Err(e) = result {
        eprintln!("{}", tui::red(&e));
    }
}

// ── REPL ──────────────────────────────────────────────────────────────────────
fn repl(provider: &Provider, mut perm: Permission, cwd: &std::path::Path, raw: bool) -> Result<(), String> {
    let settings = config::load_settings().unwrap_or_default();

    // Show the full-screen header banner.
    let mode_name = "BUILD"; // default starting mode
    tui::show_banner(
        &settings.provider,
        &provider.model,
        mode_name,
        &cwd.display().to_string(),
    );
    tui::line(&tui::dim("  describe a task · /help for all commands · !<cmd> for shell · Shift+Tab to change mode"));

    let mut transcript: Vec<provider::Msg> = Vec::new();
    let mut sid = session::new_id();
    let mut mode = Mode::Build;
    let mut last_suggested_mode: Option<&'static str> = None;

    loop {
        tui::line("");
        let prompt = format!("{} {} ", tui::mode_badge(mode_label(&mode)), tui::accent("›"));
        let task = match tui::ask_task(&prompt) {
            None => return Ok(()),
            Some(tui::InputEvent::CycleMode) => {
                mode = mode.next();
                last_suggested_mode = None;
                tui::show_mode_change(mode_label(&mode));
                continue;
            }
            Some(tui::InputEvent::Text(t)) => t,
        };
        let t = task.trim();
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
                "1" | "plan"       => { mode = Mode::Plan;       last_suggested_mode = None; tui::show_mode_change("PLAN"); }
                "2" | "build"      => { mode = Mode::Build;      last_suggested_mode = None; tui::show_mode_change("BUILD"); }
                "3" | "brainstorm" => { mode = Mode::Brainstorm; last_suggested_mode = None; tui::show_mode_change("BRAINSTORM"); }
                other => tui::line(&tui::red(&format!("  unknown mode '{other}' — try: plan, build, brainstorm"))),
            }
            continue;
        }

        // /permissions with an inline argument, e.g. `/permissions auto`.
        if let Some(perm_arg) = t.strip_prefix("/permissions ") {
            match perm_arg.trim() {
                "ask" | "1"      => apply_permission(&mut perm, "ask"),
                "auto" | "2"     => apply_permission(&mut perm, "auto"),
                "readonly" | "3" => apply_permission(&mut perm, "readonly"),
                other => tui::line(&tui::red(&format!("  unknown permission '{other}' — try: ask, auto, readonly"))),
            }
            continue;
        }

        match t {
            "/exit" | "/quit" | "exit" => return Ok(()),
            "/clear" => { tui::clear(); continue; }
            "/new" => {
                transcript.clear();
                sid = session::new_id();
                tui::line(&tui::dim("  started a fresh session"));
                continue;
            }
            "/resume" => {
                handle_resume(&mut transcript, &mut sid);
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
            "/mode" => {
                tui::line(&format!("  Current mode: {}", tui::mode_badge(mode_label(&mode))));
                tui::line(&tui::dim("  Tab-complete: /mode plan|build|brainstorm  ·  Shift+Tab to cycle"));
                tui::line("");
                tui::line(&format!("    {}  {}", tui::bold("1"), "plan"));
                tui::line(&format!("    {}  {}", tui::bold("2"), "build"));
                tui::line(&format!("    {}  {}", tui::bold("3"), "brainstorm"));
                tui::line("");
                let pick = tui::ask("  switch to [1/2/3 or name, Enter to keep]: ").unwrap_or_default();
                match pick.trim() {
                    "1" | "plan"       => { mode = Mode::Plan;       last_suggested_mode = None; tui::show_mode_change("PLAN"); }
                    "2" | "build"      => { mode = Mode::Build;      last_suggested_mode = None; tui::show_mode_change("BUILD"); }
                    "3" | "brainstorm" => { mode = Mode::Brainstorm; last_suggested_mode = None; tui::show_mode_change("BRAINSTORM"); }
                    _ => {}
                }
                continue;
            }
            "/permissions" => {
                handle_permissions(&mut perm);
                continue;
            }
            "/config" => {
                handle_config(provider, perm, cwd);
                continue;
            }
            "/memory" => {
                handle_memory(provider, perm, cwd, &mut transcript, &sid);
                continue;
            }
            "/skills" => {
                handle_skills();
                continue;
            }
            _ => {}
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
                    if let hooks::PreDecision::Deny(r) = hooks::pre_tool_use("run_command", &tool_input, cwd) {
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
                    let user_input = if cmd_args.is_empty() { t.to_string() } else { format!("{t} {cmd_args}") };
                    let task_with_context = format!("{user_input}\n\n[Skill: {cmd_name}]\n{}", custom.content);
                    tui::line("");
                    if let Err(e) = agent::run_build_session(provider, perm, "engineer", &task_with_context, cwd, &mut transcript, &sid) {
                        tui::line(&tui::red(&format!("  {e}")));
                    }
                }
                tui::bell();
                continue;
            }
            // UX-001: unknown slash command — show error instead of falling through to AI.
            if !cmd_name.is_empty() {
                tui::line(&tui::red(&format!("  unknown command /{cmd_name} — /help for all commands")));
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

        // Extract @image.png tokens → push a UserImages message so build_inner skips
        // its own Msg::User push and uses this multimodal turn instead.
        let (clean_task, image_data) = extract_images(t, cwd);
        let n_images = image_data.len();
        if n_images > 0 {
            transcript.push(Msg::UserImages { text: clean_task.clone(), images: image_data });
            tui::line(&tui::dim(&format!("  attached {n_images} image(s)")));
        }
        let t = clean_task.as_str();

        tui::line("");
        let r = match &mode {
            Mode::Plan => agent::run_plan(provider, perm, t, cwd),
            Mode::Build => agent::run_build_session(provider, perm, "engineer", t, cwd, &mut transcript, &sid),
            Mode::Brainstorm => {
                match agent::run_brainstorm(provider, perm, cwd, t) {
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
                }
            }
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
    let mismatch = match (&suggested, current) {
        (Mode::Build, Mode::Brainstorm) |
        (Mode::Plan, Mode::Brainstorm) |
        (Mode::Build, Mode::Plan) => true,
        _ => false,
    };
    if mismatch {
        let sug_label = mode_label(&suggested);
        if *last_suggested != Some(sug_label) {
            tui::line(&tui::dim(&format!("  tip: this looks like a {} task — Shift+Tab or /mode to switch", sug_label)));
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
        tui::line(&format!("  {}  {}", tui::bold(&(i + 1).to_string()), s.title));
    }
    let pick = tui::ask(&tui::dim("  resume # (Enter to cancel): "))
        .as_deref().map(str::trim).and_then(|x| x.parse::<usize>().ok());
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
    tui::line(&tui::dim("  Tell me what to configure (hooks, memory, custom commands, settings…)"));
    tui::line(&tui::dim("  Examples: 'add a hook to log every command run'"));
    tui::line(&tui::dim("            'remember I prefer TypeScript over JavaScript'"));
    tui::line(&tui::dim("            'create a /deploy slash command'"));
    tui::line("");

    let input = match tui::ask(&format!("  {} ", tui::accent("›"))) {
        None => return,
        Some(s) => s,
    };
    let t = input.trim();
    if t.is_empty() { return; }

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

fn handle_memory(provider: &Provider, perm: Permission, cwd: &std::path::Path, transcript: &mut Vec<provider::Msg>, sid: &str) {
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
    tui::line(&tui::dim("  [a] add entry  [c] clear  [e] edit via AI  [Enter] dismiss"));
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
            if let Err(e) = agent::run_build_session(provider, perm, "engineer", task, cwd, transcript, sid) {
                tui::line(&tui::red(&format!("  {e}")));
            }
        }
        _ => {}
    }
}

fn handle_skills() {
    tui::line(&tui::accent("  /skills — available skills"));
    let skills = config::load_skills();
    if skills.is_empty() {
        tui::line(&tui::dim("  No skills found."));
        tui::line(&tui::dim(&format!("  Add .md files to {}/skills/", config::home().display())));
    } else {
        for (name, content) in &skills {
            let preview: String = content.lines().next().unwrap_or("").chars().take(60).collect();
            tui::line(&format!("  {}  {}", tui::bold(&format!("/{name}")), tui::dim(&preview)));
        }
    }
    // Also show custom commands.
    let cmds = config::load_custom_commands();
    if !cmds.is_empty() {
        tui::line(&tui::dim("  Custom commands:"));
        for cmd in &cmds {
            tui::line(&format!("  {}  {}", tui::bold(&format!("/{}", cmd.name)), tui::dim(if cmd.script.is_some() { "[script]" } else { "[context]" })));
        }
    }
}

fn find_custom_command(name: &str) -> Option<config::CustomCommand> {
    config::load_custom_commands().into_iter().find(|c| c.name == name)
}

// Detect intent to switch agent mode from natural language input.
// Only catches unambiguous switch phrases — not ordinary task verbs like "plan this".
fn detect_mode_switch(t: &str) -> Option<Mode> {
    let l = t.trim().to_lowercase();
    let l = l.trim_end_matches(['!', '.', '?']).trim();

    let verb_prefixes: &[&str] = &[
        "switch to ", "switch mode to ", "change to ", "change mode to ",
        "go to ", "set mode to ", "set mode ",
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
        "switch to ", "change to ", "change permission to ", "set permission to ",
        "set permission ", "use ",
    ];
    for prefix in verb_prefixes {
        if let Some(rest) = l.strip_prefix(prefix) {
            let rest = rest.trim().trim_end_matches("mode").trim()
                           .trim_end_matches("permission").trim();
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
    tui::line(&tui::green(&format!("  ✓ permission: {ps}")));
}

fn handle_permissions(perm: &mut Permission) {
    let current = match perm {
        Permission::Ask      => "ask",
        Permission::Auto     => "auto",
        Permission::ReadOnly => "readonly",
    };
    tui::line(&tui::accent("  /permissions — tool permission mode"));
    tui::line(&format!("  Current: {}", tui::bold(current)));
    tui::line(&tui::dim("  Tab-complete: /permissions ask|auto|readonly"));
    tui::line("");
    tui::line(&format!("    {}  {} — confirm before each file write or command  {}",
        tui::bold("1"), tui::bold("ask"), tui::dim("(recommended)")));
    tui::line(&format!("    {}  {} — auto-approve all actions                   {}",
        tui::bold("2"), tui::bold("auto"), tui::dim("(yolo)")));
    tui::line(&format!("    {}  {} — never write files or run commands",
        tui::bold("3"), tui::bold("readonly")));
    tui::line("");
    let pick = tui::ask("  choice [1/2/3 or name, Enter to keep]: ").unwrap_or_default();
    match pick.trim() {
        "1" | "ask"      => apply_permission(perm, "ask"),
        "2" | "auto"     => apply_permission(perm, "auto"),
        "3" | "readonly" => apply_permission(perm, "readonly"),
        _ => {}
    }
}

fn print_help() {
    tui::line(&tui::accent("  buildwithnexus commands"));
    tui::line(&tui::dim("  ─────────────────────────────────────────────────────────────"));
    tui::line(&format!("  {}  cycle modes (PLAN → BUILD → BRAINSTORM)", tui::bold("Shift+Tab")));
    tui::line(&format!("  {}         show/switch mode  {}", tui::bold("/mode"), tui::dim("[plan|build|brainstorm]")));
    tui::line(&format!("  {}   show/switch tool permissions  {}", tui::bold("/permissions"), tui::dim("[ask|auto|readonly]")));
    tui::line(&tui::dim("               or just say: \"switch to build mode\" / \"use readonly\""));
    tui::line(&format!("  {}       configure hooks, memory, commands via AI", tui::bold("/config")));
    tui::line(&format!("  {}       view/edit session memory", tui::bold("/memory")));
    tui::line(&format!("  {}       list available skills and custom commands", tui::bold("/skills")));
    tui::line(&format!("  {}          start a fresh session", tui::bold("/new")));
    tui::line(&format!("  {}      pick a saved session to resume", tui::bold("/resume")));
    tui::line(&format!("  {}         run setup (keys, providers, local models)", tui::bold("/init")));
    tui::line(&format!("  {}        clear the screen", tui::bold("/clear")));
    tui::line(&format!("  {}         exit", tui::bold("/exit")));
    tui::line(&tui::dim("  ─────────────────────────────────────────────────────────────"));
    tui::line(&tui::dim("  !<cmd>   run a shell command directly"));
    tui::line(&tui::dim("  @<path>  Tab-complete a file path into your message"));
    tui::line(&tui::dim("  Tab      autocomplete /commands and their sub-args"));
    tui::line(&tui::dim("  ←→ ^A ^E  move · ^W ^U ^K kill · ^Y yank · ↑↓ history"));
    tui::line(&tui::dim("  ^G open in $EDITOR · ^R reverse search · \\ + Enter = newline"));
    tui::line(&tui::dim("  Tools active in all modes: read_file, run_command, grep, etc."));
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

// Parse `@path` image tokens from a task string. Returns the cleaned text and
// a list of `(media_type, base64_data)` pairs for any recognized image paths.
// Unreadable or non-image @-tokens are left in the text unchanged.
fn extract_images(task: &str, cwd: &std::path::Path) -> (String, Vec<(String, String)>) {
    use std::io::Read;
    let image_exts = ["png", "jpg", "jpeg", "gif", "webp"];
    let mut images: Vec<(String, String)> = Vec::new();
    let mut clean = String::new();
    for word in task.split_whitespace() {
        if let Some(raw_path) = word.strip_prefix('@') {
            let ext = raw_path.rsplit('.').next().unwrap_or("").to_lowercase();
            if image_exts.contains(&ext.as_str()) {
                let p = if raw_path.starts_with('/') || raw_path.starts_with('~') {
                    PathBuf::from(raw_path)
                } else {
                    cwd.join(raw_path)
                };
                if let Ok(mut f) = std::fs::File::open(&p) {
                    let mut buf = Vec::new();
                    if f.read_to_end(&mut buf).is_ok() {
                        let media_type = match ext.as_str() {
                            "jpg" | "jpeg" => "image/jpeg",
                            "gif"          => "image/gif",
                            "webp"         => "image/webp",
                            _              => "image/png",
                        };
                        images.push((media_type.to_string(), base64_encode(&buf)));
                        if !clean.is_empty() { clean.push(' '); }
                        clean.push_str(&format!("[image: {}]", p.file_name().unwrap_or_default().to_string_lossy()));
                        continue;
                    }
                }
            }
        }
        if !clean.is_empty() { clean.push(' '); }
        clean.push_str(word);
    }
    (clean, images)
}

fn base64_encode(data: &[u8]) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = if chunk.len() > 1 { chunk[1] as usize } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as usize } else { 0 };
        out.push(ALPHA[b0 >> 2] as char);
        out.push(ALPHA[((b0 & 3) << 4) | (b1 >> 4)] as char);
        out.push(if chunk.len() > 1 { ALPHA[((b1 & 0xf) << 2) | (b2 >> 6)] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHA[b2 & 0x3f] as char } else { '=' });
    }
    out
}

// Suggest a mode from the task phrasing (used for the "tip" hint, not a gate).
pub fn classify(task: &str) -> Mode {
    let l = task.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| l.contains(*w));
    if has(&["what should", "what if", "ideas for", "what do you think", "why would", "why is", "why does", "how about", "what are the options", "advice on", "suggest", "tradeoffs"]) {
        return Mode::Brainstorm;
    }
    if has(&["plan", "design", "architect", "break down", "roadmap", "scope"]) {
        return Mode::Plan;
    }
    if has(&["build", "create", "add", "fix", "implement", "write", "refactor", "run", "make"]) {
        return Mode::Build;
    }
    if task.split_whitespace().count() > 8 { Mode::Plan } else { Mode::Build }
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
         \x20 buildwithnexus version | help\n\n\
         INTERACTIVE:\n\
         \x20 Shift+Tab              cycle mode (PLAN → BUILD → BRAINSTORM → PLAN)\n\
         \x20 /mode [plan|build|brainstorm]    show or switch mode\n\
         \x20 /permissions [ask|auto|readonly] show or switch tool permission level\n\
         \x20   or say: \"switch to build mode\" / \"use readonly\"\n\
         \x20 /config                configure hooks, memory, commands via AI\n\
         \x20 /memory                view and edit session memory\n\
         \x20 /skills                list available skills and custom commands\n\
         \x20 /help /clear /new /resume /init /exit\n\
         \x20 !<cmd>                 run shell command directly\n\
         \x20 @<path>                Tab-complete a file path\n\
         \x20 Tab                    autocomplete /commands and sub-args\n"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_brainstorm_phrases() {
        assert!(matches!(classify("what should I name this?"), Mode::Brainstorm));
        assert!(matches!(classify("any ideas for the API?"), Mode::Brainstorm));
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
        assert!(matches!(detect_mode_switch("switch to plan mode"), Some(Mode::Plan)));
        assert!(matches!(detect_mode_switch("change to build"), Some(Mode::Build)));
        assert!(matches!(detect_mode_switch("go to brainstorm"), Some(Mode::Brainstorm)));
        assert!(matches!(detect_mode_switch("set mode to planning"), Some(Mode::Plan)));
        assert!(matches!(detect_mode_switch("use build mode"), Some(Mode::Build)));
        assert!(matches!(detect_mode_switch("use brainstorm mode"), Some(Mode::Brainstorm)));
    }

    #[test]
    fn detect_mode_switch_bare_short_form() {
        assert!(matches!(detect_mode_switch("plan mode"), Some(Mode::Plan)));
        assert!(matches!(detect_mode_switch("build mode"), Some(Mode::Build)));
        assert!(matches!(detect_mode_switch("brainstorm mode"), Some(Mode::Brainstorm)));
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
        assert_eq!(detect_permission_switch("switch to readonly"), Some("readonly"));
        assert_eq!(detect_permission_switch("change to auto"), Some("auto"));
        assert_eq!(detect_permission_switch("set permission to ask"), Some("ask"));
        assert_eq!(detect_permission_switch("use readonly mode"), Some("readonly"));
    }

    #[test]
    fn build_provider_rejects_http_for_keyed_preset() {
        let s = Settings { provider: "openai".into(), model: String::new(),
            permission: "ask".into(), base_url: Some("http://insecure.local/v1".into()) };
        match build_provider(&s) {
            Err(e) => assert!(e.contains("non-HTTPS")),
            Ok(_) => panic!("expected http base_url to be rejected"),
        }
    }

    #[test]
    fn build_provider_unknown_provider() {
        let s = Settings { provider: "does-not-exist".into(), model: String::new(),
            permission: "ask".into(), base_url: None };
        match build_provider(&s) {
            Err(e) => assert!(e.contains("unknown provider")),
            Ok(_) => panic!("expected unknown provider error"),
        }
    }

    #[test]
    fn build_provider_local_preset_allows_http() {
        let s = Settings { provider: "ollama".into(), model: String::new(),
            permission: "ask".into(), base_url: Some("http://localhost:11434/v1".into()) };
        match build_provider(&s) {
            Ok(p) => {
                assert!(p.api_key.is_none());
                assert_eq!(p.model, "llama3.2");
            }
            Err(e) => panic!("local http should build: {e}"),
        }
    }
}
