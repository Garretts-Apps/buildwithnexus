// Library root. The binary (`main.rs`) is a thin shim over `run()`; everything
// lives here so the integration and performance suites can reach the internals
// (the crate is otherwise a single binary). Module visibility is unchanged —
// each module exposes a small `#[doc(hidden)] pub mod bench` re-export used only
// by the criterion suite, never part of the real API.

pub mod agent;
pub mod config;
pub mod hooks;
pub mod onboarding;
pub mod provider;
pub mod report;
pub mod tools;
pub mod tui;

use std::io::IsTerminal;
use std::path::PathBuf;

use agent::Permission;
use config::Settings;
use provider::Provider;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn run() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // `--json` switches headless commands to structured, one-event-per-line output.
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

pub fn build_provider(s: &Settings) -> Result<Provider, String> {
    let preset = config::preset(&s.provider).ok_or_else(|| format!("unknown provider '{}'; run `buildwithnexus init`", s.provider))?;
    // Never ship an API key to a non-HTTPS endpoint. Keyed presets may only be
    // overridden to another https:// host; keyless (local) presets are exempt.
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
    Ok(Provider { protocol: preset.protocol, base_url, api_key, model })
}

// Headless one-shot commands: no alt screen, pipe-friendly.
fn headless(f: impl FnOnce(&Provider, Permission, PathBuf) -> Result<(), String>) {
    let (provider, perm) = match provider_or_onboard() {
        Ok(v) => v,
        Err(e) => { eprintln!("{}", tui::red(&e)); std::process::exit(1); }
    };
    provider::prewarm(&provider); // warm the TLS connection while we set up
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    hooks::init(&cwd, false); // headless: never run untrusted project hooks
    hooks::notify("SessionStart", &cwd);
    let r = f(&provider, perm, cwd.clone());
    hooks::notify("SessionEnd", &cwd);
    if let Err(e) = r {
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
    provider::prewarm(&provider); // warm the TLS connection while the user reads/types
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Raw mode only when we own a real terminal; piped/headless stays cooked.
    let raw = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    hooks::init(&cwd, raw); // may prompt to trust project hooks (cooked, pre-alt-screen)
    hooks::notify("SessionStart", &cwd);
    tui::enter_alt(raw);
    let result = repl(&provider, perm, &cwd, raw);
    tui::leave_alt();
    hooks::notify("SessionEnd", &cwd);
    if let Err(e) = result {
        eprintln!("{}", tui::red(&e));
    }
}

fn repl(provider: &Provider, perm: Permission, cwd: &std::path::Path, raw: bool) -> Result<(), String> {
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
            tui::enter_alt(raw);
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

pub enum Mode {
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

pub fn classify(task: &str) -> Mode {
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── classify ────────────────────────────────────────────────────────────
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

    // ── build_provider ──────────────────────────────────────────────────────
    // Provider deliberately has no Debug impl (it holds the API key), so match
    // on the Result rather than using unwrap/unwrap_err.
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
        // Ollama is keyless/local → http base_url is fine, and no key is required.
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
