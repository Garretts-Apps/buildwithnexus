mod agent;
mod config;
mod onboarding;
mod provider;
mod tools;
mod tui;

use std::path::PathBuf;

use agent::Permission;
use config::Settings;
use provider::Provider;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
        "brainstorm" => headless(|p, _perm, _cwd| agent::run_brainstorm(p, &rest())),
        "-v" | "-V" | "--version" | "version" => println!("buildwithnexus {VERSION}"),
        "-h" | "--help" | "help" => usage(),
        other => {
            eprintln!("unknown command: {other}\n");
            usage();
            std::process::exit(2);
        }
    }
}

// Resolve a runnable provider from saved settings, running onboarding if needed.
fn provider_or_onboard() -> Result<(Provider, Permission), String> {
    let settings = match config::load_settings() {
        Some(s) => s,
        None => onboarding::run().ok_or("setup cancelled")?,
    };
    Ok((build_provider(&settings)?, agent::permission(&settings.permission)))
}

fn build_provider(s: &Settings) -> Result<Provider, String> {
    let preset = config::preset(&s.provider).ok_or_else(|| format!("unknown provider '{}'; run `buildwithnexus init`", s.provider))?;
    let base_url = s.base_url.clone().unwrap_or_else(|| preset.base_url.to_string());
    let model = if s.model.is_empty() { preset.default_model.to_string() } else { s.model.clone() };
    let api_key = if preset.env_key.is_empty() { None } else { config::load_key(preset.env_key) };
    if !preset.env_key.is_empty() && api_key.is_none() {
        return Err(format!("{} not set; run `buildwithnexus init`", preset.env_key));
    }
    Ok(Provider { protocol: preset.protocol, base_url, api_key, model })
}

// Headless one-shot commands: no alt screen, pipe-friendly.
fn headless(f: impl FnOnce(&Provider, Permission, PathBuf) -> Result<(), String>) {
    let (provider, perm) = match provider_or_onboard() {
        Ok(v) => v,
        Err(e) => { eprintln!("{}", tui::red(&e)); std::process::exit(1); }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Err(e) = f(&provider, perm, cwd) {
        eprintln!("{}", tui::red(&e));
        std::process::exit(1);
    }
}

// Full-screen interactive session.
fn interactive() {
    let onboarded = config::load_settings().is_some();
    if !onboarded && onboarding::run().is_none() {
        return;
    }
    let (provider, perm) = match provider_or_onboard() {
        Ok(v) => v,
        Err(e) => { eprintln!("{}", tui::red(&e)); std::process::exit(1); }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    tui::enter_alt();
    let result = repl(&provider, perm, &cwd);
    tui::leave_alt();
    if let Err(e) = result {
        eprintln!("{}", tui::red(&e));
    }
}

fn repl(provider: &Provider, perm: Permission, cwd: &std::path::Path) -> Result<(), String> {
    let settings = config::load_settings().unwrap_or_default();
    loop {
        tui::clear();
        tui::line(&tui::accent("  buildwithnexus"));
        tui::line(&tui::dim(&format!("  {} · {} · {} · {}",
            settings.provider, provider.model, settings.permission, cwd.display())));
        tui::line(&tui::dim("  describe a task, or /exit"));
        tui::line("");

        let task = match tui::ask(&format!("{} ", tui::accent("›"))) {
            None => return Ok(()),
            Some(t) => t,
        };
        let t = task.trim();
        if t.is_empty() {
            continue;
        }
        if t == "/exit" || t == "/quit" || t == "exit" {
            return Ok(());
        }
        if t == "/init" {
            tui::leave_alt();
            onboarding::run();
            tui::enter_alt();
            continue;
        }

        let mode = choose_mode(t);
        tui::line("");
        let r = match mode {
            Mode::Plan => agent::run_plan(provider, perm, t, cwd),
            Mode::Build => agent::run_build(provider, perm, "engineer", t, cwd),
            Mode::Brainstorm => agent::run_brainstorm(provider, t),
        };
        if let Err(e) = r {
            tui::line(&tui::red(&format!("  {e}")));
        }
        tui::line("");
        let _ = tui::ask(&tui::dim("  [Enter] for a new task "));
    }
}

enum Mode {
    Plan,
    Build,
    Brainstorm,
}

// Suggest a mode from the phrasing, then let the user confirm or override.
fn choose_mode(task: &str) -> Mode {
    let suggested = classify(task);
    let name = match suggested { Mode::Plan => "PLAN", Mode::Build => "BUILD", Mode::Brainstorm => "BRAINSTORM" };
    tui::line(&format!("  {} {}  ·  {} plan  {} build  {} brainstorm",
        tui::dim("suggested:"), tui::bold(name), tui::bold("1"), tui::bold("2"), tui::bold("3")));
    match tui::ask(&format!("  {} ", tui::dim("mode [Enter to accept]:"))).as_deref().map(str::trim) {
        Some("1") => Mode::Plan,
        Some("2") => Mode::Build,
        Some("3") => Mode::Brainstorm,
        _ => suggested,
    }
}

fn classify(task: &str) -> Mode {
    let l = task.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| l.contains(*w));
    if has(&["what", "should", "idea", "think", "why", "how about", "options", "advice", "suggest"]) {
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
         \x20 buildwithnexus                 full-screen interactive session\n\
         \x20 buildwithnexus run <task>      execute a task (agentic, headless)\n\
         \x20 buildwithnexus plan <task>     decompose, approve, then execute\n\
         \x20 buildwithnexus brainstorm <q>  free-form chat, no tools\n\
         \x20 buildwithnexus init            (re)configure provider / model / key\n\
         \x20 buildwithnexus providers       list built-in providers\n\
         \x20 buildwithnexus version | help\n"
    );
}
