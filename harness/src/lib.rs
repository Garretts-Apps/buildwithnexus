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
pub mod session;
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
        "sessions" => {
            for s in session::list() {
                let title: String = s.title.chars().take(48).collect();
                println!("  {}  {:<48}  {}", s.id, title, s.cwd);
            }
        }
        // Continue the most recent session with a new task.
        "continue" => headless(|p, perm, cwd| match session::latest() {
            Some(s) => agent::run_build_resumed(p, perm, "engineer", &rest(), &cwd, s.msgs, &s.id),
            None => Err("no sessions to continue".into()),
        }),
        // Resume a specific session by id: `resume <id> <task…>`.
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
    // Context window drives auto-compaction. Local models are small (compact
    // early — exactly where it matters for SLM swarms); hosted models are large.
    let context_tokens = match preset.id {
        "anthropic" => 200_000,
        _ if preset.local => 8_192,
        _ => 128_000,
    };
    Ok(Provider { protocol: preset.protocol, base_url, api_key, model, context_tokens })
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
    // Banner once. We render inline on the normal screen (no alt buffer), so the
    // transcript stays in the terminal's scrollback — never cleared per task.
    tui::line(&tui::accent("  buildwithnexus"));
    tui::line(&tui::dim(&format!("  {} · {} · {} · {}",
        settings.provider, provider.model, settings.permission, cwd.display())));
    tui::line(&tui::dim("  describe a task · /help for commands · !<cmd> to run a shell command"));

    // One continuous, persisted session across BUILD tasks (resumable via
    // /resume). Compaction in the agent loop keeps it within the model window.
    let mut transcript: Vec<provider::Msg> = Vec::new();
    let mut sid = session::new_id();

    loop {
        tui::line("");
        let task = match tui::ask_task(&format!("{} ", tui::accent("›"))) {
            None => return Ok(()),
            Some(t) => t,
        };
        let t = task.trim();
        if t.is_empty() {
            continue;
        }

        // Shell mode: `!cmd` runs directly in the working dir, no model.
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
                let mut sessions = session::list();
                if sessions.is_empty() {
                    tui::line(&tui::dim("  no saved sessions yet"));
                    continue;
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
                        transcript = s.msgs;
                        sid = s.id;
                    }
                }
                continue;
            }
            "/help" => {
                tui::line(&tui::dim("  /help  /clear  /new  /resume  /init  /exit   ·   !<cmd> shell   ·   Tab completes / and @"));
                tui::line(&tui::dim("  edit: ←→ ^A ^E move · ^W ^U ^K kill · ^Y yank · ↑↓ history · ^G $EDITOR · \\+Enter newline"));
                continue;
            }
            "/init" => {
                tui::leave_alt();
                onboarding::run();
                tui::enter_alt(raw);
                continue;
            }
            _ => {}
        }

        let mode = choose_mode(t);
        tui::line("");
        let r = match mode {
            Mode::Plan => agent::run_plan(provider, perm, t, cwd),
            Mode::Build => agent::run_build_session(provider, perm, "engineer", t, cwd, &mut transcript, &sid),
            Mode::Brainstorm => agent::run_brainstorm(provider, t),
        };
        if let Err(e) = r {
            tui::line(&tui::red(&format!("  {e}")));
        }
        tui::bell(); // soft "turn finished" nudge for long tasks
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
         \x20 buildwithnexus continue <task> continue the most recent session\n\
         \x20 buildwithnexus resume <id> <t> resume a specific session\n\
         \x20 buildwithnexus sessions        list saved sessions\n\
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
