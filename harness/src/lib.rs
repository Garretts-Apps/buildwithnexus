//! A hilariously fast, agentic AI coding CLI — one self-contained binary,
//! written in Rust. Works with hosted APIs (Anthropic, OpenAI, OpenRouter,
//! Groq, Hugging Face), local models (Ollama, llama.cpp, LM Studio), and any
//! OpenAI-compatible `/v1` endpoint.
//!
//! This crate is the whole application: the binaries (`buildwithnexus` and
//! the `bwn` alias) are thin shims over [`run`]. It ships as a library so
//! integration suites can reach the internals directly — it is not a stable
//! API for building other tools on, and minor versions may rearrange it.
//!
//! # Install
//!
//! ```text
//! cargo install buildwithnexus --locked   # installs `buildwithnexus` + `bwn`
//! npm install -g buildwithnexus           # prebuilt, provenance-attested binary
//! ```
//!
//! Then run `bwn` in a repository and describe a task. The agent plans,
//! edits files, and runs commands — asking before each change (permission
//! gates), with checkpoints that can rewind any write (`/undo`), lifecycle
//! hooks, and hot-swappable models (`/model`, validated before it commits).
//!
//! # Map of the crate
//!
//! | Module | What lives there |
//! |---|---|
//! | [`agent`] | the ReAct loop: planning, tool calls, recovery, compaction |
//! | [`provider`] | wire protocols (Anthropic, OpenAI-compat, Ollama native), streaming, retries |
//! | [`tools`] | the tool surface: file IO, search, shell, web — with permission gating |
//! | [`mcp`] | Model Context Protocol client: stdio / HTTP servers, discovery, `mcp__*` dispatch |
//! | [`tui`] | the alternate-screen terminal UI: incremental wrap cache, diffs, autocomplete |
//! | [`checkpoint`] | pre-edit snapshots and turn-grouped undo |
//! | [`session`] | save/resume of conversations |
//! | [`workflow`] | background `/schedule` and `/loop` runs, persisted across restarts |
//! | [`config`] | provider presets, settings files, key store, bundled skills |
//! | [`usage`] | session token ledger, price table, `--max-budget-usd` guard |
//! | [`hooks`] | Claude-Code-style lifecycle hooks (deny-capable, never grant) |
//!
//! Performance is the project's primary design lever; every claim is
//! measured and reproducible — see `BENCHMARKS.md` in the repository.
//! Docs, guides, and the changelog live at
//! <https://buildwithnexus.dev>; source at
//! <https://github.com/Garretts-Apps/buildwithnexus>.

pub mod agent;
pub mod checkpoint;
pub mod config;
pub mod graphics;
pub mod highlight;
pub mod hooks;
pub mod knowledge;
pub mod local;
pub mod mcp;
pub mod media;
pub mod net;
pub mod onboarding;
pub mod provider;
pub mod report;
pub mod rules;
pub mod sandbox;
pub mod session;
pub mod sixel;
pub mod tools;
pub mod trace;
pub mod tui;
pub mod update;
pub mod usage;
pub mod verifier;
pub mod workflow;

use std::io::IsTerminal;
use std::path::PathBuf;

use agent::Permission;
use config::Settings;
use provider::Provider;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_ATTACHED_FILE_BYTES: u64 = 256 * 1024;

#[derive(Default, Clone, Debug)]
struct CliOptions {
    provider: Option<String>,
    model: Option<String>,
    permission_mode: Option<String>,
    sandbox: Option<String>,
    prompt: Option<String>,
    /// `--effort off|low|medium|high`, validated when the provider is built.
    effort: Option<String>,
    /// `--max-budget-usd <n>`: session spend ceiling, already parsed and > 0.
    max_budget_usd: Option<f64>,
    json: bool,
    // True once `--` was seen: everything after it is literal text, even
    // words that start with '-'.
    args_literal: bool,
    /// `--yes` / `-y`: auto-approve a plan and execute it (headless `plan`).
    yes: bool,
    /// `--legacy-exit-codes` (or BWN_LEGACY_EXIT_CODES=1): exit 0 when a
    /// headless run stops short without failing, as before 0.15.
    legacy_exit_codes: bool,
    /// `--trust-project <digest>` (or BWN_TRUST_PROJECT): trust exactly this
    /// project settings content for this run (`buildwithnexus trust --print`).
    trust_project: Option<String>,
}

fn parse_cli_options(args: Vec<String>) -> Result<(CliOptions, Vec<String>), String> {
    let mut opts = CliOptions::default();
    let mut rest = Vec::new();
    let mut budget_raw: Option<String> = None;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        if arg == "--" {
            opts.args_literal = true;
            rest.extend(it);
            break;
        }
        if arg == "--json" {
            opts.json = true;
            continue;
        }
        if arg == "--yes" || arg == "-y" {
            opts.yes = true;
            continue;
        }
        if arg == "--legacy-exit-codes" {
            opts.legacy_exit_codes = true;
            continue;
        }
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(k, v)| (k, Some(v)));
        let slot = match flag {
            "--provider" => &mut opts.provider,
            "--model" => &mut opts.model,
            "--permission-mode" | "--permission" => &mut opts.permission_mode,
            "--sandbox" => &mut opts.sandbox,
            "--prompt" => &mut opts.prompt,
            "--effort" => &mut opts.effort,
            "--max-budget-usd" => &mut budget_raw,
            "--trust-project" => &mut opts.trust_project,
            _ => {
                rest.push(arg);
                continue;
            }
        };
        let value = inline
            .map(str::to_string)
            .or_else(|| it.next().filter(|v| !v.starts_with('-')));
        *slot = Some(
            value
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| format!("{flag} requires a value; see `buildwithnexus --help`"))?,
        );
    }
    if let Some(raw) = budget_raw {
        let usd = raw
            .trim()
            .trim_start_matches('$')
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && *n > 0.0)
            .ok_or_else(|| {
                format!("--max-budget-usd expects a positive dollar amount (got '{raw}')")
            })?;
        opts.max_budget_usd = Some(usd);
    }
    Ok((opts, rest))
}

pub fn run() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (opts, args) = match parse_cli_options(args) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("buildwithnexus: {e}");
            std::process::exit(2);
        }
    };
    if opts.json {
        report::set(report::Mode::Json);
    }
    hooks::set_trust_digest(opts.trust_project.clone());
    if let Some(p) = opts
        .provider
        .as_deref()
        .filter(|p| config::preset(p).is_none())
    {
        eprintln!("buildwithnexus: {}", unknown_provider_msg(p));
        std::process::exit(2);
    }
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let rest = || args[1..].join(" ");

    match cmd {
        "" => interactive(opts.prompt.clone(), opts),
        "init" | "da-init" | "setup" => init_cli(&opts, &args[1..]),
        "login" => login_cli(&opts),
        "providers" => {
            for p in config::PRESETS {
                let tag = if p.local { "local" } else { "remote" };
                println!("  {:<12} {:<26} {}", p.id, p.label, tag);
            }
        }
        "run" | "build" | "headless" | "--headless" | "-p" | "--print" => {
            headless(&opts, |p, perm, cwd| {
                let (task, images) = headless_attachments(p, &rest(), &cwd);
                agent::run_build(p, perm, "engineer", &task, &cwd, images)
            })
        }
        "plan" => {
            // Approving a plan needs a terminal; without one (and without
            // --yes) fail fast instead of hanging on the selector.
            if !opts.yes && !std::io::stdin().is_terminal() {
                eprintln!(
                    "buildwithnexus: `plan` needs an interactive terminal to approve the plan — \
                     pass --yes (-y) to auto-approve and execute"
                );
                std::process::exit(2);
            }
            headless(&opts, |p, perm, cwd| {
                let (task, images) = headless_attachments(p, &rest(), &cwd);
                agent::run_plan(p, perm, &task, &cwd, opts.yes, images)
            })
        }
        "brainstorm" => headless(&opts, |p, perm, cwd| {
            let (task, images) = headless_attachments(p, &rest(), &cwd);
            agent::run_brainstorm(p, perm, &cwd, &task, images).map(|_| ())
        }),
        "sessions" => {
            let all = session::list();
            if all.is_empty() {
                // Empty output reads as "broken"; say why the list is empty.
                println!("no saved sessions yet — BUILD sessions are saved as they run.");
                return;
            }
            for s in &all {
                // Titles come from task text and cwd from the checkout's
                // folder name, so neither reaches the terminal raw.
                let title: String = tui::sanitize_terminal(&s.title).chars().take(48).collect();
                println!(
                    "  {}  {:<48}  {}",
                    s.id,
                    title,
                    tui::sanitize_terminal(&s.cwd)
                );
            }
            println!();
            println!(
                "{}",
                tui::dim("resume one:  buildwithnexus resume <id> <task>  ·  the latest:  buildwithnexus continue <task>")
            );
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
        "trust" => std::process::exit(hooks::trust_cli(&args[1..])),
        "mcp" => match mcp::manage(&args[1..], false) {
            Ok(lines) => {
                for l in lines {
                    println!("  {l}");
                }
            }
            Err(e) => {
                eprintln!("buildwithnexus mcp: {e}");
                std::process::exit(2);
            }
        },
        // A stray flag must not become an interactive prompt: `bwn --modle x`
        // silently launching the TUI hides the typo.
        other if !opts.args_literal && is_unknown_option(other) => {
            eprintln!("buildwithnexus: unknown option '{other}'; see --help");
            std::process::exit(2);
        }
        _ if !args.is_empty() => {
            interactive(opts.prompt.clone().or_else(|| Some(args.join(" "))), opts)
        }
        other => {
            eprintln!("unknown command: {other}\n");
            usage();
            std::process::exit(2);
        }
    }
}

// Options and flag-spelled subcommands the top-level match accepts. Anything
// else that looks like a flag (`-x`, `--foo`) is a typo, not a prompt; a lone
// `-` is left alone so it can still be a plain word.
fn is_unknown_option(arg: &str) -> bool {
    const KNOWN: &[&str] = &[
        "-v",
        "-V",
        "--version",
        "-h",
        "--help",
        "--headless",
        "-p",
        "--print",
        "-c",
        "--continue",
        "-r",
        "--resume",
    ];
    arg.len() > 1 && arg.starts_with('-') && !KNOWN.contains(&arg)
}

fn provider_or_onboard(opts: &CliOptions) -> Result<(Provider, Permission), String> {
    let load = config::load_settings_diag();
    warn_settings_issues(&load);
    let mut settings = match load.settings {
        Some(s) if !s.provider.is_empty() => s,
        // Settings files exist but none were usable: refuse to fall through
        // to onboarding, which would overwrite them. Broken config is a fix,
        // not a first run.
        None if load.any_present => return Err(broken_settings_msg()),
        // No provider yet (or only a project file that names none). Without
        // a terminal there is nobody to answer the setup questions, so take
        // the provider from --provider or from the first API key in the
        // environment, and fail with the fix otherwise.
        loaded if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) => {
            let picked =
                unattended_settings(opts.provider.as_deref(), |k| config::load_key(k).is_some())
                    .ok_or_else(no_setup_headless_msg)?;
            Settings {
                provider: picked.provider,
                ..loaded.unwrap_or_default()
            }
        }
        _ => onboarding::run().ok_or("setup not finished")?,
    };
    if let Some(p) = opts.provider.as_ref().filter(|p| **p != settings.provider) {
        // The saved address belongs to the saved provider: --provider runs
        // at the address last used with it, or at its preset default.
        settings.base_url = remembered_endpoint(p);
        settings.provider = p.clone();
    }
    set_active_preset(&settings.provider);
    let mut provider = build_provider(&settings)?;
    if let Some(model) = &opts.model {
        provider.model = model.clone();
    }
    if let Some(level) = &opts.effort {
        provider.effort = config::Effort::parse(level).ok_or_else(|| {
            format!("--effort must be one of off, low, medium, high (got '{level}')")
        })?;
    }
    // The CLI flag wins over the settings key; either arms the pre-request
    // guard in the agent loop.
    usage::set_budget(opts.max_budget_usd.or(settings.max_budget_usd));
    if let Some(why) = provider::budget_guard(&provider) {
        eprintln!("buildwithnexus: {why}");
        std::process::exit(2);
    }
    let perm = permission_from(opts.permission_mode.as_deref(), &settings.permission);
    // A bad --sandbox flag is a hard error; a bad settings value only warns
    // (and leaves the sandbox off) so a typo can't lock the user out.
    let sandbox_mode = opts.sandbox.as_deref().unwrap_or(&settings.sandbox);
    if let Err(e) = sandbox::configure(sandbox_mode, settings.sandbox_network) {
        if opts.sandbox.is_some() {
            return Err(e);
        }
        eprintln!("{}", tui::yellow(&format!("buildwithnexus: warning: {e}")));
    }
    workflow::set_launch(
        &settings.provider,
        &provider.model,
        &provider.base_url,
        agent::permission_name(perm),
    );
    Ok((provider, perm))
}

// `--permission-mode` wins over the setting. A misspelt flag is a usage
// error (exit 2) before anything is sent; a misspelt setting warns and
// falls back to ask, so a typo can neither lock the user out nor loosen
// the gate.
fn permission_from(flag: Option<&str>, setting: &str) -> Permission {
    match flag {
        Some(name) => agent::parse_permission(name).unwrap_or_else(|e| {
            eprintln!("{}", tui::red(&format!("buildwithnexus: {e}")));
            std::process::exit(2);
        }),
        None => agent::parse_permission(setting).unwrap_or_else(|e| {
            eprintln!(
                "{}",
                tui::yellow(&format!(
                    "buildwithnexus: warning: settings: {e} — using ask"
                ))
            );
            Permission::Ask
        }),
    }
}

/// Every ignored settings file gets one loud stderr line — a typo in a config
/// file must never be invisible.
fn warn_settings_issues(load: &config::SettingsLoad) {
    for i in &load.issues {
        eprintln!(
            "{}",
            tui::yellow(&format!(
                "buildwithnexus: warning: {}: {}",
                tui::sanitize_terminal(&i.source),
                tui::sanitize_terminal(&i.error)
            ))
        );
    }
}

/// Settings for a first run with no terminal: the `--provider` preset if
/// one was named, else the first remote preset whose key is set. Nothing is
/// written to disk; `buildwithnexus init` still owns the saved setup.
fn unattended_settings(provider: Option<&str>, has_key: impl Fn(&str) -> bool) -> Option<Settings> {
    // An unknown --provider is kept so build_provider reports it by name.
    let id = match provider {
        Some(id) => id.to_string(),
        None => config::PRESETS
            .iter()
            .find(|p| !p.local && !p.env_key.is_empty() && has_key(p.env_key))?
            .id
            .to_string(),
    };
    Some(Settings {
        provider: id,
        ..Default::default()
    })
}

fn no_setup_headless_msg() -> String {
    "no provider is set up, and there is no terminal to run setup in.\n  \
     Set an API key (ANTHROPIC_API_KEY, OPENAI_API_KEY, OPENROUTER_API_KEY, GROQ_API_KEY or HF_TOKEN),\n  \
     pass --provider (e.g. --provider ollama), or run `buildwithnexus init` once in a terminal."
        .to_string()
}

fn broken_settings_msg() -> String {
    "settings files exist but none could be used (see warnings above).\n  \
     Fix the file, or delete it and run `buildwithnexus init` to set up again.\n  \
     `buildwithnexus doctor` lists every settings file and its status."
        .to_string()
}

// One rotating line under the banner: half real tips, half jokes — the
// personality lives here, in the terminal, never in the way. Errors stay
// serious; this line is the only place bwn gets to be funny at startup.
const STARTUP_TIPS: &[&str] = &[
    "tip: Shift+Tab cycles PLAN → BUILD → BRAINSTORM. plan first, thank yourself later",
    "tip: Esc interrupts the agent mid-thought. it can take it",
    "tip: Ctrl+V pastes a screenshot straight into the prompt — the model sees what you see",
    "tip: @ completes file paths, @kb: searches the knowledge base",
    "tip: double-click a word, triple-click a line. copied, confirmed, footer says so",
    "tip: /checkpoint before you get brave",
    "tip: /model swaps models mid-session — it validates before it commits",
    "tip: ↑ filters history by what you've typed, and never eats your draft",
    "tip: /vim exists. you already knew, somehow",
    "tip: queue your next prompt while the agent works — it sends itself when the turn ends",
    "tip: local models via Ollama keep your code where it belongs: on your machine",
    "tip: Ctrl+G opens your $EDITOR when the prompt outgrows one line",
    "tip: file paths in edit headers are clickable. yes, even the .docx",
    "tip: bwn started faster than you read this sentence",
    "tip: the other terminals keep asking about bwn. tell them not to worry about it",
    "tip: /trace shows receipts — every tool call, hook, and skill load",
    "tip: 966µs → 4.5µs per streamed chunk. we timed it so you don't have to feel it",
    "tip: /schedule and /loop run work while you're at lunch. bwn doesn't take lunch",
];

fn startup_tip() -> &'static str {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0);
    STARTUP_TIPS[nanos % STARTUP_TIPS.len()]
}

fn is_loopback_url(u: &str) -> bool {
    let rest = u
        .strip_prefix("http://")
        .or_else(|| u.strip_prefix("https://"))
        .unwrap_or(u);
    let host = rest
        .split('/')
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    let host = host.strip_prefix('[').map_or_else(
        // Not bracketed: strip a :port if present.
        || host.split(':').next().unwrap_or("").to_string(),
        // Bracketed IPv6: take up to the closing bracket.
        |h| h.split(']').next().unwrap_or("").to_string(),
    );
    host == "localhost" || host == "::1" || host.starts_with("127.") || host == "0.0.0.0"
}

/// Names an unknown provider id, the closest real one, and the full list.
pub(crate) fn unknown_provider_msg(id: &str) -> String {
    if id.trim().is_empty() {
        return "no provider is set up yet; run `buildwithnexus init`, or pass --provider".into();
    }
    let ids: Vec<&str> = config::PRESETS.iter().map(|p| p.id).collect();
    let typed = id.trim().to_ascii_lowercase();
    let near = ids
        .iter()
        .map(|p| (tools::levenshtein(&typed, p), *p))
        .min()
        .filter(|(d, p)| *d <= 2.max(p.len() / 4) || p.starts_with(&typed));
    let shown = tui::sanitize_terminal(id);
    match near {
        Some((_, p)) => format!(
            "unknown provider {shown} — did you mean {p}? Providers: {}",
            ids.join(", ")
        ),
        None => format!("unknown provider {shown} — Providers: {}", ids.join(", ")),
    }
}

pub fn build_provider(s: &Settings) -> Result<Provider, String> {
    build_provider_with_key(s, None)
}

/// `build_provider` with a key that is not saved yet: it is used in place of
/// the stored one and passes the same checks, so a key can be proven with a
/// probe before it is written to disk.
pub(crate) fn build_provider_with_key(s: &Settings, key: Option<&str>) -> Result<Provider, String> {
    let preset = config::preset(&s.provider).ok_or_else(|| unknown_provider_msg(&s.provider))?;
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
    let api_key = if preset.id == "custom" {
        // Optional — most self-hosted OpenAI-compatible servers are keyless.
        key.map(str::to_string)
            .or_else(|| config::load_key(config::CUSTOM_KEY))
    } else if preset.env_key.is_empty() {
        None
    } else {
        key.map(str::to_string)
            .or_else(|| config::load_key(preset.env_key))
    };
    if !preset.env_key.is_empty() && api_key.is_none() {
        return Err(format!(
            "{} not set; run `buildwithnexus init`",
            preset.env_key
        ));
    }
    // The custom preset allows plain http for loopback servers, but a
    // configured key must never travel unencrypted to a remote host.
    if preset.id == "custom"
        && api_key.is_some()
        && !base_url.starts_with("https://")
        && !is_loopback_url(&base_url)
    {
        return Err(format!(
            "refusing to send {} to a non-HTTPS remote endpoint ({base_url}); use https:// or a loopback address",
            config::CUSTOM_KEY
        ));
    }
    // A notice, not stderr: /model rebuilds the provider inside the TUI.
    for w in usage::set_prices(&s.prices) {
        report::notice(&format!("  ⚠ {w}"));
    }
    let mut context_tokens = match preset.id {
        "anthropic" => 200_000,
        _ if preset.local => 8_192,
        _ => 128_000,
    };
    // An explicit settings override wins over presets and detection alike.
    if let Some(n) = s.context_tokens {
        context_tokens = n as usize;
    }
    // Backward compatibility: an explicit base_url pointing at the OpenAI-compat
    // surface (`…/v1`) keeps the OpenAI protocol — configs saved before the
    // native Ollama path existed (and users deliberately targeting a /v1
    // proxy) must not switch wire formats. Native is for root URLs only.
    let mut protocol = preset.protocol;
    if protocol == config::Protocol::OllamaNative
        && s.base_url
            .as_deref()
            .is_some_and(|u| u.trim_end_matches('/').ends_with("/v1"))
    {
        protocol = config::Protocol::OpenAi;
    }
    let mut provider = Provider {
        protocol,
        base_url,
        model,
        api_key,
        context_tokens,
        temperature: s.temperature,
        max_tokens: s.max_tokens,
        // An unrecognized level behaves like the default rather than
        // rejecting the whole settings file.
        effort: config::Effort::parse(&s.effort).unwrap_or_default(),
        ollama_ctx: std::sync::OnceLock::new(),
    };
    if provider.protocol == config::Protocol::OllamaNative {
        if let Some(n) = s.context_tokens {
            // Pre-seed the probe cache: the native path uses the override as
            // num_ctx without ever querying /api/show.
            let _ = provider.ollama_ctx.set(Some(n));
        } else if let Some(n) = provider::ollama_ctx(&provider) {
            // Detected window replaces the hardcoded 8k local default so
            // compaction thresholds match what the model can actually hold.
            provider.context_tokens = n as usize;
        }
    } else if s.context_tokens.is_none() && (preset.local || is_loopback_url(&provider.base_url)) {
        // llama.cpp, LM Studio and vLLM report the window they loaded the
        // model with; the 8k guess stands only when they say nothing.
        let served = provider::served_model(&provider.base_url, &provider.model);
        if let Some(n) = served.window {
            provider.context_tokens = n;
            provider::remember_window(&provider);
        }
        if let Some(v) = served.vision {
            provider::remember_vision(&provider, v);
        }
    }
    media::set_vision_override(s.vision);
    if s.context_tokens.is_some() {
        provider::remember_window(&provider);
    }
    Ok(provider)
}

fn headless(
    opts: &CliOptions,
    f: impl FnOnce(&Provider, Permission, PathBuf) -> Result<(), String>,
) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    hooks::trust_project(&cwd, false);
    let (provider, perm) = match provider_or_onboard(opts) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", tui::red(&e));
            std::process::exit(1);
        }
    };
    provider::prewarm(&provider);
    hooks::init(&cwd, false);
    hooks::set_permission_mode(agent::permission_name(perm));
    hooks::notify("SessionStart", &cwd);

    if !report::is_json() {
        // No hand-drawn box: long provider/model/cwd values would shatter
        // fixed-width borders. Plain aligned rows can't overflow.
        println!(
            "{} {}",
            tui::bold("buildwithnexus headless"),
            tui::dim(&format!("v{}", crate::VERSION))
        );
        println!(
            "{}",
            tui::dim(&format!(
                "  model  {} · {}",
                onboarding::provider_label(
                    &active_preset().unwrap_or_default(),
                    &provider.base_url
                ),
                tui::sanitize_terminal(&provider.model)
            ))
        );
        // The folder name comes from whoever made the checkout.
        let shown_cwd = cwd.display().to_string();
        println!(
            "{}",
            tui::dim(&format!("  cwd    {}", tui::sanitize_terminal(&shown_cwd)))
        );
        // Skill names and paths come from files in the checkout.
        for note in config::startup_context_notices(&cwd) {
            let note = tui::sanitize_terminal(&note);
            println!("{}", tui::dim(&format!("  {note}")));
        }
        println!();
        // Off the critical path: five `which` probes cost real startup latency,
        // and with interactive=false this only prints when something is missing.
        std::thread::spawn(|| check_and_offer_install_dependencies(false));
    }

    if let Some(n) = agent::ignored_approvals_notice_once(&cwd) {
        report::notice(&format!("  {n}"));
    }
    // MCP tools must be on the surface before the first request; discovery
    // is bounded by each server's timeout, and every outcome is a notice.
    mcp::ensure_ready();
    report_mcp_notices();

    // Nobody can answer an approval prompt here, so `ask` blocks every edit
    // and command. Say so before the run, not after it looks successful.
    let unattended = report::is_json() || !std::io::stdin().is_terminal();
    if unattended && perm == Permission::Ask {
        eprintln!(
            "{}",
            tui::yellow(
                "buildwithnexus: no terminal to approve changes, so edits and commands will be blocked.\n  \
                 Pass --permission-mode auto to allow them, or --permission-mode readonly to only read."
            )
        );
    } else if unattended && perm == Permission::AcceptEdits {
        eprintln!(
            "{}",
            tui::yellow(
                "buildwithnexus: accept-edits with no terminal: file edits run, commands and network access will be blocked."
            )
        );
    }

    let start_time = std::time::Instant::now();
    let mut r = f(&provider, perm, cwd.clone());
    let elapsed = start_time.elapsed();
    hooks::notify("SessionEnd", &cwd);
    let blocked = agent::blocked_without_terminal();
    if r.is_ok() && blocked > 0 {
        r = Err(format!(
            "{blocked} change{} blocked for lack of approval; nothing was applied for {}. \
             Re-run with --permission-mode auto to allow changes.",
            if blocked == 1 { " was" } else { "s were" },
            if blocked == 1 { "it" } else { "them" }
        ));
    }

    let outcome = match &r {
        Err(_) if blocked > 0 => agent::Outcome::ApprovalBlocked,
        Err(_) => agent::Outcome::Failed,
        Ok(()) => agent::stopped_short_outcome().unwrap_or(agent::Outcome::Success),
    };
    let legacy = opts.legacy_exit_codes
        || std::env::var("BWN_LEGACY_EXIT_CODES").is_ok_and(|v| !v.is_empty() && v != "0");
    let code = headless_exit_code(outcome, r.is_ok(), legacy);

    if !report::is_json() {
        println!();
        if outcome == agent::Outcome::Success {
            println!("{}", tui::green(&format!("✓ done in {elapsed:.2?}")));
        } else if r.is_ok() {
            println!(
                "{}",
                tui::yellow(&format!("⚠ {} after {elapsed:.2?}", outcome.label()))
            );
        } else {
            println!("{}", tui::red(&format!("✗ failed after {elapsed:.2?}")));
        }
    }
    report::result(outcome.as_str(), code);

    if let Err(e) = r {
        eprintln!("{}", tui::red(&tui::sanitize_terminal(&e)));
    }
    if code != 0 {
        std::process::exit(code);
    }
}

// A turn that ended without an error but short of success gets its own exit
// code, unless legacy exit codes ask for the pre-0.15 zero.
fn headless_exit_code(outcome: agent::Outcome, turn_ok: bool, legacy: bool) -> i32 {
    if legacy && turn_ok {
        0
    } else {
        outcome.exit_code()
    }
}

// Connected / failed / disconnected lines from background discovery.
fn report_mcp_notices() {
    for (msg, ok) in mcp::drain_notices() {
        if ok {
            report::info(&format!("  {msg}"));
        } else {
            report::notice(&format!("  {msg}"));
        }
    }
}

// Setup was left before a model answered: nothing was saved, so the next
// launch starts setup again.
fn setup_not_finished() -> ! {
    let why = if std::io::stdin().is_terminal() {
        ""
    } else {
        " (there is no terminal to answer its questions in)"
    };
    eprintln!(
        "{}",
        tui::yellow(&format!(
            "setup not finished{why} — nothing was saved. `buildwithnexus` (or `buildwithnexus init`) starts it again."
        ))
    );
    std::process::exit(1);
}

// `buildwithnexus init`: setup in a terminal; `init --agents-md` writes
// AGENTS.md from the repository as an ordinary headless run.
fn init_cli(opts: &CliOptions, args: &[String]) {
    if args.iter().any(|a| a == "--agents-md") {
        return headless(opts, |p, perm, cwd| {
            let task = agents_md_task(&cwd);
            agent::run_build(p, perm, "engineer", &task, &cwd, Vec::new())
        });
    }
    if onboarding::run().is_none() {
        setup_not_finished();
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if !has_instruction_file(&cwd) {
        tui::line(&tui::dim(
            "  No AGENTS.md here: /init in a session, or `buildwithnexus init --agents-md`, writes one from this repository.",
        ));
    }
}

// `buildwithnexus login`: a new key for the configured provider (or
// --provider), checked before it is saved.
fn login_cli(opts: &CliOptions) {
    let settings = config::load_settings().filter(|s| !s.provider.is_empty());
    let Some(mut settings) =
        settings.or_else(|| opts.provider.as_ref().map(|_| Settings::default()))
    else {
        eprintln!("{}", tui::red(&unknown_provider_msg("")));
        std::process::exit(1);
    };
    if let Some(p) = opts.provider.as_ref().filter(|p| **p != settings.provider) {
        settings.provider = p.clone();
        settings.model = String::new();
        settings.base_url = None;
    }
    if let Some(m) = &opts.model {
        settings.model = m.clone();
    }
    if onboarding::login(&settings).is_none() {
        std::process::exit(1);
    }
}

fn interactive(initial_prompt: Option<String>, opts: CliOptions) {
    // Always scaffold on interactive launch so existing users also get the
    // directory skeleton and starter Agents.md if they're missing.
    config::scaffold_home();
    let load = config::load_settings_diag();
    warn_settings_issues(&load);
    // Settings that name no provider (a team repo's hooks-only file, or
    // nothing at all) mean setup has not run yet; only files that exist and
    // cannot be read stop startup, so they are never set up over.
    if load.settings.is_none() && load.any_present {
        eprintln!("{}", tui::red(&broken_settings_msg()));
        std::process::exit(1);
    }
    if !load
        .settings
        .as_ref()
        .is_some_and(|s| !s.provider.is_empty())
        && onboarding::run().is_none()
    {
        setup_not_finished();
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let raw = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    // Before the provider is built: base_url, permission and sandbox may
    // come from the project only once the user trusts it.
    hooks::trust_project(&cwd, raw);
    let (provider, perm) = match provider_or_onboard(&opts) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", tui::red(&e));
            std::process::exit(1);
        }
    };
    provider::prewarm(&provider);

    hooks::init(&cwd, raw);
    hooks::set_permission_mode(agent::permission_name(perm));
    // Once per process: the session id is fixed here so SessionStart, every
    // turn's hooks, and the saved transcript all agree on it.
    hooks::notify("SessionStart", &cwd);
    tui::enter_alt(raw);
    let result = repl(provider, perm, &cwd, raw, initial_prompt);
    kill_local_server();
    mcp::shutdown();
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
    tui::configure_ui(&settings.images, &settings.notify);
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
            if restored == 1 { "" } else { "s" }
        )));
    }
    if let Some(notice) = update::startup_notice(&settings.auto_update) {
        tui::line(&tui::dim(&notice));
    }
    // Off the critical path: five `which` probes cost real startup latency,
    // and with interactive=false this only prints when something is missing.
    std::thread::spawn(|| check_and_offer_install_dependencies(false));
    update::spawn_check(&settings.auto_update);

    let mut transcript: Vec<provider::Msg> = Vec::new();
    // The REPL owns the id SessionStart already announced.
    let mut sid = session::current_or_new();
    session::set_current(&sid);
    trace::set_session(&sid);
    let mut mode = Mode::Brainstorm;
    let mut last_suggested_mode: Option<&'static str> = None;
    // /btw: extra context injected into the next task without interrupting.
    let mut btw_ctx: Option<String> = None;
    let mut pending_prompt = initial_prompt;
    // The workflow count the queue line last showed.
    let mut shown_active = 0usize;

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
                if active == 1 { "" } else { "s" }
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
            let vision = media::model_supports_vision(&provider);
            let (task, images) = extract_attachments(task.trim(), cwd, vision);
            if let Err(e) = agent::run_plan(&provider, perm, &task, cwd, false, images) {
                tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
            }
            tui::bell();
            continue;
        }
        if let Some(task) = t.strip_prefix("/build ") {
            tui::line("");
            let vision = media::model_supports_vision(&provider);
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
            let vision = media::model_supports_vision(&provider);
            let (task, images) = extract_attachments(task.trim(), cwd, vision);
            if let Err(e) = agent::run_brainstorm(&provider, perm, cwd, &task, images).map(|_| ()) {
                tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
            }
            tui::bell();
            continue;
        }

        match t {
            "/exit" | "/quit" | "exit" | "quit" => return Ok(()),
            "/clear" => {
                transcript.clear();
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
                usage::forget_last();
                sid = session::new_id();
                session::set_current(&sid);
                trace::set_session(&sid);
                tui::line(&tui::dim("  started a fresh session"));
                continue;
            }
            "/resume" => {
                handle_resume(&mut transcript, &mut sid);
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
                    tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
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
                    tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
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
                    let out = tools::run("run_command", &tool_input, cwd);
                    for l in tui::sanitize_terminal(&out.content).lines() {
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
                        tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
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
            } else {
                tui::line(&tui::red("  type /help for available commands"));
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

        // Mode routing: auto-switch out of BRAINSTORM when the task clearly
        // demands real work (chat mode can't fulfill "build X"); elsewhere only
        // hint, and stay quiet for greetings and ordinary questions.
        if should_answer_conversationally(t, &mode) {
            last_suggested_mode = None;
        } else if let Some(new_mode) = auto_switch_mode(t, &mode) {
            mode = new_mode;
            last_suggested_mode = None;
            tui::line(&tui::dim(&format!(
                "  auto-switched to {} for this task — /mode to switch back",
                mode_label(&mode)
            )));
            tui::show_mode_change(mode_label(&mode));
        } else {
            suggest_mode_if_mismatch(t, &mode, &mut last_suggested_mode);
        }

        // Extract @path tokens. Images become multimodal attachments; text files
        // are appended into the prompt with optional @file:start-end ranges.
        let vision = media::model_supports_vision(&provider);
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
            let plural = if n_images == 1 { "" } else { "s" };
            tui::line(&tui::dim(&format!("  ⎘ attached {n_images} image{plural}")));
        }

        tui::line("");
        let r = if conversational {
            agent::run_chat_turn(&provider, perm, cwd, t, std::mem::take(&mut image_data))
        } else {
            match &mode {
                Mode::Plan => match agent::run_plan(
                    &provider,
                    perm,
                    t,
                    cwd,
                    false,
                    std::mem::take(&mut image_data),
                ) {
                    Ok(()) => {
                        mode = Mode::Build;
                        tui::show_mode_change("BUILD");
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
                Mode::Brainstorm => match agent::run_brainstorm(
                    &provider,
                    perm,
                    cwd,
                    t,
                    std::mem::take(&mut image_data),
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
                    Ok(Some(agent::ModeHint::CycleMode)) => {
                        mode = mode.next();
                        tui::show_mode_change(mode_label(&mode));
                        Ok(())
                    }
                    Ok(Some(agent::ModeHint::Handoff(line))) => {
                        pending_prompt = Some(line);
                        Ok(())
                    }
                },
            }
        };
        if let Err(e) = r {
            tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
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

// Auto-switch when the task phrasing clearly demands a different mode.
// Conservative matrix: only ever escalates out of BRAINSTORM — a chat mode
// can't fulfill a build/plan request. A deliberate PLAN gate is never bypassed
// silently; build-shaped tasks there still get the tip below.
fn auto_switch_mode(task: &str, current: &Mode) -> Option<Mode> {
    let target = classify(task);
    match (&target, current) {
        // Never auto-switch directly from BRAINSTORM to BUILD — always step through PLAN first.
        (Mode::Build, Mode::Brainstorm) => Some(Mode::Plan),
        (Mode::Plan, Mode::Brainstorm) => Some(Mode::Plan),
        _ => None,
    }
}

// Suggest switching modes when the task phrasing strongly implies a different mode.
// Suppresses the tip if it was already shown for this mode combo in the current session.
fn suggest_mode_if_mismatch(task: &str, current: &Mode, last_suggested: &mut Option<&'static str>) {
    let suggested = classify(task);
    let mismatch = matches!((&suggested, current), (Mode::Build, Mode::Plan));
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

fn should_answer_conversationally(task: &str, current: &Mode) -> bool {
    // Simple greetings are always conversational, even in Brainstorm mode.
    if is_simple_conversation(task) {
        return true;
    }

    if matches!(current, Mode::Brainstorm) {
        return false;
    }

    matches!(classify(task), Mode::Brainstorm) && !looks_like_action_request(task)
}

fn is_simple_conversation(task: &str) -> bool {
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

fn looks_like_action_request(task: &str) -> bool {
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
            tui::sanitize_terminal(&s.title)
        ));
    }
    let pick = tui::ask(&tui::dim("  resume # (Enter to cancel): "))
        .as_deref()
        .map(str::trim)
        .and_then(|x| x.parse::<usize>().ok());
    if let Some(n) = pick {
        if n >= 1 && n <= sessions.len().min(15) {
            let s = sessions.swap_remove(n - 1);
            let title = s.title.clone();
            *transcript = s.msgs;
            *sid = s.id;
            tui::line(&tui::green(&format!(
                "  ✓ resumed: {}",
                tui::sanitize_terminal(&title)
            )));
            tui::line(&tui::dim("  ── restored history ──"));
            for msg in transcript.iter() {
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
    if let Err(e) = agent::run_build(provider, perm, "engineer", &full_task, cwd, Vec::new()) {
        tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
    }
}

fn handle_memory(
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

fn handle_skills(cwd: &std::path::Path) {
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

fn handle_mcp(arg: &str) {
    let args = shlex::split(arg.trim()).unwrap_or_default();
    // Server names, server_info and errors are server- or config-supplied.
    match mcp::manage(&args, true) {
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

fn has_instruction_file(cwd: &std::path::Path) -> bool {
    cwd.join("AGENTS.md").exists() || cwd.join("CLAUDE.md").exists()
}

/// What /init (and `init --agents-md`) asks the model to do: read this
/// repository's own build, test and CI files and write down only what they
/// say, or improve the AGENTS.md that is already there.
fn agents_md_task(cwd: &std::path::Path) -> String {
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
fn offer_agents_md(
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
        _ => false,
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
fn handle_init(
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
fn handle_login(provider: &mut Provider) {
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

fn find_custom_command(name: &str) -> Option<config::CustomCommand> {
    config::load_custom_commands()
        .into_iter()
        .find(|c| c.name == name)
}

fn handle_model(provider: &mut Provider) {
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
fn parse_model_endpoint(pick: &str) -> Option<(String, String)> {
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
fn endpoint_preset(url: &str) -> &'static str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let root = rest.trim_end_matches('/').split_once('/').is_none();
    if root && (is_ollama_address(url) || !provider::ollama_models(url).is_empty()) {
        "ollama"
    } else {
        "custom"
    }
}

fn is_ollama_address(url: &str) -> bool {
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
fn remembered_endpoint(preset: &str) -> Option<String> {
    config::load_user_settings().and_then(|u| u.endpoints.get(preset).cloned())
}

fn swap_cancelled() {
    tui::line(&tui::dim("  /model cancelled — nothing saved"));
}

/// Maps a typed model name to the provider that serves it. Anything
/// unrecognized stays on the current provider — the swap then validates it.
fn parse_model_pick(pick: &str, current_provider: &str) -> (String, String) {
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

fn find_active_local_base_url(preferred: &str) -> Option<String> {
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
fn find_active_local_base_url_in(candidates: &[&str]) -> Option<String> {
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
fn is_ollama(root: &str) -> bool {
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
fn gguf_unservable(model_name: &str, have_llama_server: bool) -> Option<&'static str> {
    let gguf = model_name.to_ascii_lowercase().ends_with(".gguf");
    (gguf && !have_llama_server).then_some(
        "llama-server is not installed — install llama.cpp, or load the file in LM Studio",
    )
}

fn find_llama_server_binary() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let pathext = std::env::var_os("PATHEXT");
    find_llama_server_in(&path, pathext.as_deref(), cfg!(windows))
}

/// `find_llama_server_binary` with its inputs passed in, so tests can run
/// the Windows rules anywhere: there the binary is `llama-server.exe`.
fn find_llama_server_in(
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

static LLAMA_SERVER_PROCESS: std::sync::Mutex<Option<std::process::Child>> =
    std::sync::Mutex::new(None);

pub fn kill_local_server() {
    if let Ok(mut lock) = LLAMA_SERVER_PROCESS.lock() {
        if let Some(mut child) = lock.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn ensure_local_gguf_server(preferred_url: &str, model_name: &str) -> Option<String> {
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
static ACTIVE_PRESET: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn set_active_preset(id: &str) {
    if let Ok(mut a) = ACTIVE_PRESET.lock() {
        *a = id.to_string();
    }
}

fn active_preset() -> Option<String> {
    ACTIVE_PRESET
        .lock()
        .ok()
        .map(|a| a.clone())
        .filter(|a| !a.is_empty())
}

/// Where a `/model` swap points: the base URL it probes and runs against,
/// and what it writes to the saved `base_url`.
#[derive(Debug, PartialEq)]
struct SwapTarget {
    base_url: String,
    /// None leaves the saved value alone; Some(None) clears it.
    save: Option<Option<String>>,
}

/// A saved `base_url` belongs to the provider it was set for: a swap within
/// that provider keeps it (a remote Ollama host, an LM Studio box on the
/// LAN), a swap to another provider goes back to the address last used with
/// it (`remembered`, from `endpoints`) or else to its preset default, and an
/// explicit URL always wins.
fn plan_swap(
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
fn remember_endpoints(
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

/// The swap's success line names the model actually saved, which differs
/// from the one asked for when a local server only answers as "local-model".
fn swap_success_line(requested: &str, active: &str, provider_label: &str) -> String {
    if requested == active || requested.is_empty() {
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
fn swap_model(
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
    let key_name = onboarding::key_name(preset);
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
        let new_address = custom_url.as_deref().is_some_and(|u| Some(u) != current);
        if new_address && config::load_key(config::CUSTOM_KEY).is_none() {
            let Some(key) = tui::ask_secret("  API key for this endpoint (Enter for none): ")
            else {
                return swap_cancelled();
            };
            new_key = Some(key.trim().to_string()).filter(|k| !k.is_empty());
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
        let Some(key) = tui::ask_secret(&format!("  {}: ", preset.env_key)) else {
            return swap_cancelled();
        };
        if key.trim().is_empty() {
            return swap_cancelled();
        }
        new_key = Some(key.trim().to_string());
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
    if let Some(k) = &new_key {
        config::save_key(key_name, k);
        tui::line(&tui::green(&format!("  ✓ {key_name} saved")));
    }
    if let Some(k) = p.api_key.as_deref() {
        config::record_key_check(key_name, k, true);
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
fn probe_swap(
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
            Ok(None) => return Some(p),
            Err(e) => e,
        };
        let fail = onboarding::Fail::from_error(&e);
        if let onboarding::Fail::KeyRejected(code) = fail {
            if new_key.is_some() {
                tui::line(&tui::red(&format!(
                    "  ✗ rejected (HTTP {code}) — not saved"
                )));
                keeping();
                return None;
            }
            if preset.id == "custom" && config::load_key(config::CUSTOM_KEY).is_none() {
                tui::line(&tui::yellow(&format!(
                    "  the endpoint wants an API key (HTTP {code}) — paste it, or Esc to cancel"
                )));
                let key = tui::ask_secret("  API key for this endpoint: ")
                    .map(|k| k.trim().to_string())
                    .filter(|k| !k.is_empty());
                if key.is_none() {
                    swap_cancelled();
                    return None;
                }
                *new_key = key;
                continue;
            }
        }
        tui::line(&tui::red(&format!(
            "  ✗ validation failed: {}",
            tui::sanitize_terminal(&e)
        )));
        let hint = match fail {
            onboarding::Fail::KeyRejected(_) => {
                let name = onboarding::key_name(preset);
                if let Some(k) = config::load_key(name) {
                    config::record_key_check(name, &k, false);
                }
                "the API key was rejected — /login to replace it".to_string()
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

fn handle_voice(arg: &str) -> Option<String> {
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
struct LocalServer {
    label: &'static str,
    /// The preset `/model <preset> <name>` switches to.
    preset: &'static str,
    base: String,
}

/// The servers /local probes: the configured one first (a LAN Ollama, LM
/// Studio on another port), then each local preset at its default address.
fn local_servers(settings: &Settings) -> Vec<LocalServer> {
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
    for p in config::PRESETS.iter().filter(|p| p.local) {
        add(p.id, p.base_url);
    }
    out
}

fn handle_local(provider: &mut Provider) {
    tui::line(&tui::accent("  local models"));
    let settings = config::load_settings().unwrap_or_default();
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

fn handle_rules(cwd: &std::path::Path) {
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
fn rules_listing(cwd: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut engine = crate::rules::RuleEngine::load_defaults();
    let rules_dir = cwd.join(".buildwithnexus").join("rules");
    let (loaded, failures) = load_workspace_rule_files(&rules_dir);
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
            "  [{sev_badge}] {} — {}",
            tui::bold(&tui::sanitize_terminal(&r.id)),
            tui::sanitize_terminal(&r.description)
        ));
    }
    out
}

// Every file in the workspace rules dir, plus one (file name, error) per
// file that failed to load — a broken rules file used to vanish silently.
fn load_workspace_rule_files(
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

fn handle_kb_index(cwd: &std::path::Path) {
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
    let mut dirs_to_visit = vec![cwd.to_path_buf()];
    while let Some(dir) = dirs_to_visit.pop() {
        if tui::interrupted() {
            break;
        }
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.')
                    || name == "target"
                    || name == "node_modules"
                    || name == "vendor"
                    || name == "dist"
                {
                    continue;
                }
                if path.is_dir() {
                    dirs_to_visit.push(path);
                } else if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                    if matches!(ext, "rs" | "js" | "ts" | "py" | "go" | "java" | "c" | "cpp") {
                        let rel_path = path
                            .strip_prefix(cwd)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .to_string();
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            for line in content.lines() {
                                let trimmed = line.trim();
                                let mut entity_type = None;
                                let mut sym_name = None;
                                if ext == "rs" {
                                    if trimmed.starts_with("fn ")
                                        || trimmed.starts_with("pub fn ")
                                        || trimmed.starts_with("async fn ")
                                        || trimmed.starts_with("pub async fn ")
                                    {
                                        entity_type = Some(crate::knowledge::EntityType::Function);
                                        if let Some(idx) = trimmed.find("fn ") {
                                            let rest = &trimmed[idx + 3..];
                                            if let Some(paren) = rest.find('(') {
                                                sym_name = Some(rest[..paren].trim().to_string());
                                            }
                                        }
                                    } else if trimmed.starts_with("struct ")
                                        || trimmed.starts_with("pub struct ")
                                    {
                                        entity_type = Some(crate::knowledge::EntityType::Class);
                                        if let Some(idx) = trimmed.find("struct ") {
                                            let rest = &trimmed[idx + 7..];
                                            let name_part =
                                                rest.split_whitespace().next().unwrap_or("");
                                            sym_name = Some(
                                                name_part
                                                    .trim_matches(|c| {
                                                        c == '{' || c == '(' || c == ';'
                                                    })
                                                    .to_string(),
                                            );
                                        }
                                    } else if trimmed.starts_with("enum ")
                                        || trimmed.starts_with("pub enum ")
                                    {
                                        entity_type = Some(crate::knowledge::EntityType::Class);
                                        if let Some(idx) = trimmed.find("enum ") {
                                            let rest = &trimmed[idx + 5..];
                                            let name_part =
                                                rest.split_whitespace().next().unwrap_or("");
                                            sym_name = Some(
                                                name_part
                                                    .trim_matches(|c| {
                                                        c == '{' || c == '(' || c == ';'
                                                    })
                                                    .to_string(),
                                            );
                                        }
                                    }
                                } else if ext == "py" {
                                    if let Some(rest) = trimmed.strip_prefix("def ") {
                                        entity_type = Some(crate::knowledge::EntityType::Function);
                                        if let Some(paren) = rest.find('(') {
                                            sym_name = Some(rest[..paren].trim().to_string());
                                        }
                                    } else if let Some(rest) = trimmed.strip_prefix("class ") {
                                        entity_type = Some(crate::knowledge::EntityType::Class);
                                        if let Some(paren) = rest.find(['(', ':']) {
                                            sym_name = Some(rest[..paren].trim().to_string());
                                        }
                                    }
                                } else if matches!(ext, "js" | "ts") {
                                    if trimmed.starts_with("function ")
                                        || trimmed.starts_with("export function ")
                                    {
                                        entity_type = Some(crate::knowledge::EntityType::Function);
                                        if let Some(idx) = trimmed.find("function ") {
                                            let rest = &trimmed[idx + 9..];
                                            if let Some(paren) = rest.find('(') {
                                                sym_name = Some(rest[..paren].trim().to_string());
                                            }
                                        }
                                    } else if trimmed.starts_with("class ")
                                        || trimmed.starts_with("export class ")
                                    {
                                        entity_type = Some(crate::knowledge::EntityType::Class);
                                        if let Some(idx) = trimmed.find("class ") {
                                            let rest = &trimmed[idx + 6..];
                                            let name_part =
                                                rest.split_whitespace().next().unwrap_or("");
                                            sym_name = Some(
                                                name_part.trim_matches(|c| c == '{').to_string(),
                                            );
                                        }
                                    }
                                }
                                if let (Some(et), Some(sn)) = (entity_type, sym_name) {
                                    if !sn.is_empty()
                                        && sn
                                            .chars()
                                            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
                                    {
                                        let id = format!("{sn}@{rel_path}");
                                        kb.add_entity(crate::knowledge::Entity {
                                            id,
                                            entity_type: et,
                                            name: sn,
                                            path: Some(rel_path.clone()),
                                            description: Some(format!(
                                                "Extracted symbol from {rel_path}"
                                            )),
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

/// Returns false when the permission gate or a PreToolUse hook refused the
/// project checks (which run the project's own build/test commands).
fn handle_verify_audit(perm: Permission, cwd: &std::path::Path) -> bool {
    let check_input = serde_json::json!({});
    if let Some(reason) = agent::hook_gate(perm, "check_work", &check_input, cwd) {
        tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&reason))));
        tui::bell();
        return false;
    }
    tui::line(&tui::accent(
        "  verifying workspace against rules and tests",
    ));

    let mut changed_files = Vec::new();
    if let Ok(o) = std::process::Command::new("git")
        .args(["status", "-s"])
        .current_dir(cwd)
        .output()
    {
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
        tui::line(&tui::dim(
            "  no modified git files — checking recent files in the workspace…",
        ));
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

    // Actually run the project's checks (build/test/lint) and feed the
    // verdict into the verifier's tests dimension instead of inferring it.
    let check = tools::run("check_work", &check_input, cwd);
    let tests_passed = if verify_check_had_nothing_to_run(&check.content) {
        None
    } else {
        Some(!check.is_error)
    };
    tui::line(&tui::render_md(&check.content));

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
        tests_passed,
    };

    let report = verifier.verify(&ctx);
    // The report is Markdown — render it instead of echoing raw ##/**/`.
    let report_str = crate::verifier::Verifier::format_report(&report);
    tui::line(&tui::render_md(&report_str));
    tui::line(&tui::dim(
        "  tip: @rules:<id> or /rules inspects specific constraints",
    ));
    true
}

// check_work found no project checks to run (or every checker was missing):
// no verdict, rather than a false "tests passed".
fn verify_check_had_nothing_to_run(report: &str) -> bool {
    report.contains("no build/test/lint commands detected") || report.contains("nothing ran")
}

fn handle_compact(provider: &Provider, transcript: &mut Vec<provider::Msg>) {
    if transcript.is_empty() {
        tui::line(&tui::dim("  nothing to compact (empty transcript)"));
        return;
    }
    let before = transcript.len();
    let taken = std::mem::take(transcript);
    *transcript = agent::compact_msgs(provider, taken);
    usage::forget_last();
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
    tui::line(&tui::accent("  background workflows"));
    tui::line(&tui::rule());
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
        if let Some(why) = &s.reason {
            tui::line(&tui::dim(&format!(
                "        {}",
                tui::sanitize_terminal(why)
            )));
        }
    }
    tui::line(&tui::rule());
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
            // The run's --json events, as the lines they stand for.
            let lines: Vec<String> = workflow::output(id)
                .iter()
                .filter_map(|l| workflow::readable(l))
                .collect();
            if lines.is_empty() {
                tui::line(&tui::dim(&format!(
                    "  no output captured for workflow #{id}"
                )));
            } else {
                tui::line(&tui::accent(&format!("  workflow #{id} output:")));
                // Child stderr is raw tool and model output.
                for l in lines.iter().take(100) {
                    tui::line(&format!("    {}", tui::dim(&tui::sanitize_terminal(l))));
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
                "accept edits" | "accept-edits" | "acceptedits" => return Some("accept-edits"),
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

// Writes only the changed keys into the user settings file. Saving the merged
// settings would copy a project's own settings into the user's global file.
fn save_user_settings(changes: &[(&str, Option<serde_json::Value>)]) {
    if let Err(e) = config::save_user_settings(changes) {
        tui::line(&tui::yellow(&format!("  ⚠ not saved: {e}")));
    }
}

// How far a permission change reaches: a switch typed in the conversation
// (or `/permissions <mode>`) lasts for this session; only an explicit
// "save as default" writes the user settings file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PermScope {
    Session,
    Default,
}

// Apply a permission name to the session, and save it as the default only
// when asked to.
fn apply_permission(perm: &mut Permission, ps: &str, scope: PermScope) {
    let new = match agent::parse_permission(ps) {
        Ok(p) => p,
        Err(e) => {
            tui::line(&tui::red(&format!("  {e}")));
            return;
        }
    };
    *perm = new;
    let name = agent::permission_name(new);
    hooks::set_permission_mode(name);
    tui::set_permission_mode(permission_label(perm));
    match scope {
        PermScope::Session => tui::line(&tui::green(&format!(
            "  ✓ permission: {name} for this session — /permissions to make it the default"
        ))),
        PermScope::Default => {
            save_user_settings(&[("permission", Some(name.into()))]);
            tui::line(&tui::green(&format!(
                "  ✓ permission: {name} — saved as the default for every project"
            )));
        }
    }
}

fn permission_label(perm: &Permission) -> &'static str {
    agent::permission_name(*perm)
}

// `/permissions <arg>`: a mode for this session, `default <mode>` to save
// it, or the saved-approval commands.
fn handle_permissions_arg(perm: &mut Permission, cwd: &std::path::Path, arg: &str) {
    // The numbers are the picker's rows, 1-3 as in 0.14.
    let mode = |a: &str| match a {
        "1" => Some("ask"),
        "2" => Some("auto"),
        "3" => Some("readonly"),
        "4" => Some("accept-edits"),
        other => agent::parse_permission(other)
            .ok()
            .map(agent::permission_name),
    };
    if let Some(rest) = arg.strip_prefix("default") {
        match mode(rest.trim()) {
            Some(m) => apply_permission(perm, m, PermScope::Default),
            None => tui::line(&tui::red(
                "  usage: /permissions default <ask|accept-edits|auto|readonly>",
            )),
        }
        return;
    }
    match arg {
        "reset" => handle_permissions_reset(cwd),
        "list" => print_saved_approvals(cwd),
        other if other.starts_with("remove ") => {
            handle_permissions_remove(cwd, &other["remove ".len()..])
        }
        other => match mode(other) {
            Some(m) => apply_permission(perm, m, PermScope::Session),
            None => tui::line(&tui::red(&format!(
                "  unknown permission '{other}' — try: ask, accept-edits, auto, readonly, default <mode>, list, remove <entry>, reset"
            ))),
        },
    }
}

// `/permissions reset`: forget every "always allow" answer given in this
// project (the per-project map in the user settings file).
fn handle_permissions_reset(cwd: &std::path::Path) {
    agent::clear_session_allowed(cwd);
    let n = config::reset_project_allowed(cwd);
    if n == 0 {
        tui::line(&tui::dim("  no \"always allow\" entries for this project"));
    } else {
        tui::line(&tui::green(&format!(
            "  ✓ cleared {n} \"always allow\" entr{} for {}",
            if n == 1 { "y" } else { "ies" },
            tui::sanitize_terminal(&cwd.display().to_string())
        )));
    }
}

// What `s` and `a` answers allow in this project, and how to take them back;
// then the allow, ask and deny rules in force, each with its file.
fn print_saved_approvals(cwd: &std::path::Path) {
    let rules = config::policy_rules(cwd);
    if !rules.is_empty() {
        tui::line("  rules (deny > ask > allow > mode):");
        for r in &rules {
            let what = if r.network {
                format!("network.{}", r.effect.as_str())
            } else {
                r.effect.as_str().to_string()
            };
            tui::line(&format!(
                "    {what:<13} {}  {}",
                tui::sanitize_terminal(&r.rule),
                tui::dim(&format!("— {}", r.source))
            ));
        }
    }
    let always = config::load_project_allowed(cwd);
    let session = agent::session_allowed(cwd);
    if always.is_empty() && session.is_empty() {
        tui::line(&tui::dim(
            "  no saved approvals for this project (answer s or a at an approval to add one)",
        ));
        return;
    }
    for (title, keys) in [
        ("always allowed in this project", &always),
        ("allowed for this session", &session),
    ] {
        if keys.is_empty() {
            continue;
        }
        tui::line(&format!("  {title}:"));
        for k in keys {
            tui::line(&format!("    {}", tui::sanitize_terminal(k)));
        }
    }
    tui::line(&tui::dim(
        "  /permissions remove <entry> forgets one · /permissions reset forgets them all",
    ));
}

// `/permissions remove <entry>`: forget one saved approval, both the
// project's "always" entry and this session's.
fn handle_permissions_remove(cwd: &std::path::Path, key: &str) {
    let key = key.trim().trim_matches('`');
    let always = config::remove_project_allowed(cwd, key);
    let session = agent::remove_session_allowed(cwd, key);
    let shown = tui::sanitize_terminal(key);
    if always || session {
        tui::line(&tui::green(&format!("  ✓ removed: {shown}")));
    } else {
        tui::line(&tui::yellow(&format!(
            "  no saved approval '{shown}' — /permissions list shows them"
        )));
    }
}

fn handle_permissions(perm: &mut Permission, cwd: &std::path::Path) {
    if let Some(n) = agent::ignored_approvals_notice(cwd) {
        report::notice(&format!("  {n}"));
    }
    print_saved_approvals(cwd);
    let current = permission_label(perm);
    // 0.14's rows keep their places; accept-edits is added last, so a row
    // picked from memory never lands on a looser mode.
    let modes = [
        (
            "ask",
            "Confirm before each file write or command (recommended)",
        ),
        ("auto", "Auto-approve all safe tool operations (yolo)"),
        ("readonly", "Never write files or run mutating commands"),
        (
            "accept-edits",
            "Apply file edits in this project without asking; commands and network still ask",
        ),
    ];
    let items: Vec<tui::SelectItem> = modes
        .iter()
        .map(|(label, detail)| tui::SelectItem {
            label: (*label).into(),
            detail: (*detail).into(),
        })
        .collect();
    let title = format!("Select Tool Permission Mode (Current: {current})");
    let Some(&(mode, _)) = tui::select_item(&title, &items).and_then(|i| modes.get(i)) else {
        return;
    };
    let scopes = [
        tui::SelectItem {
            label: "this session".into(),
            detail: "Back to the saved default next time".into(),
        },
        tui::SelectItem {
            label: "save as default".into(),
            detail: "Every new session, in every project".into(),
        },
    ];
    match tui::select_item(&format!("Use {mode} for"), &scopes) {
        Some(0) => apply_permission(perm, mode, PermScope::Session),
        Some(1) => apply_permission(perm, mode, PermScope::Default),
        _ => {}
    }
}

// `/sandbox`: bare or `status` reports; a mode switches the session and
// persists to settings.json, like /permissions.
fn handle_sandbox(arg: &str) {
    match arg {
        "" | "status" => {
            for l in sandbox::status_lines() {
                tui::line(&format!("  {l}"));
            }
        }
        other => match sandbox::Mode::parse(other) {
            Some(mode) => {
                sandbox::set_mode(mode);
                save_user_settings(&[("sandbox", Some(mode.as_str().into()))]);
                tui::line(&tui::green(&format!("  ✓ sandbox: {}", mode.as_str())));
                for l in sandbox::status_lines().into_iter().skip(1) {
                    tui::line(&tui::dim(&format!("  {l}")));
                }
            }
            None => tui::line(&tui::red(&format!(
                "  unknown sandbox mode '{other}' — try: off, auto, require, status"
            ))),
        },
    }
}

fn handle_mouse(arg: Option<&str>) {
    let cmd = arg.unwrap_or("").trim();
    match cmd {
        "on" | "enable" => {
            tui::set_mouse_capture(true);
            tui::line(&tui::green(
                "  ✓ mouse: on — wheel scroll and drag-to-copy are enabled",
            ));
        }
        "off" | "disable" => {
            tui::set_mouse_capture(false);
            tui::line(&tui::green(
                "  ✓ mouse: off — terminal-native selection restored; use PgUp/PgDn to scroll",
            ));
        }
        "" | "toggle" => {
            let new_state = !tui::mouse_capture_enabled();
            tui::set_mouse_capture(new_state);
            if new_state {
                tui::line(&tui::green(
                    "  ✓ mouse: on — wheel scroll and drag-to-copy are enabled",
                ));
            } else {
                tui::line(&tui::green(
                    "  ✓ mouse: off — terminal-native selection restored; use PgUp/PgDn to scroll",
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
                "  Default is on: wheel scrolls transcript and drag copies selected transcript text. /mouse off restores terminal-native selection.",
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
    // File names in the stat come from the checkout.
    for line in tui::sanitize_terminal(&out.content).lines() {
        tui::line(&tui::dim(&format!("  {line}")));
    }
}

/// Where the next request's tokens go, estimated at four characters a token
/// (images by their encoded size, as compaction counts them).
#[derive(Debug, Default, PartialEq)]
struct ContextBreakdown {
    system: usize,
    tools: usize,
    mcp_tools: usize,
    conversation: usize,
    images: usize,
}

impl ContextBreakdown {
    fn total(&self) -> usize {
        self.system + self.tools + self.mcp_tools + self.conversation + self.images
    }
}

fn context_breakdown(msgs: &[provider::Msg], tools: &[tools::ToolDef]) -> ContextBreakdown {
    let mut b = ContextBreakdown::default();
    for m in msgs {
        match m {
            provider::Msg::System(s) => b.system += s.len() / 4,
            provider::Msg::User(s) => b.conversation += s.len() / 4,
            provider::Msg::UserImages { text, images } => {
                b.conversation += text.len() / 4;
                b.images += images.iter().map(|(_, d)| d.len() / 3).sum::<usize>() / 4;
            }
            provider::Msg::Assistant { text, calls } => {
                b.conversation += (text.len()
                    + calls
                        .iter()
                        .map(|c| c.name.len() + c.input.to_string().len())
                        .sum::<usize>())
                    / 4;
            }
            provider::Msg::Tool(results) => {
                b.conversation += results.iter().map(|r| r.content.len()).sum::<usize>() / 4;
            }
        }
    }
    for t in tools {
        let size = (t.name.len() + t.description.len() + t.schema.to_string().len()) / 4;
        if mcp::is_mcp_tool(t.name) {
            b.mcp_tools += size;
        } else {
            b.tools += size;
        }
    }
    b
}

fn handle_context(transcript: &[provider::Msg], total: usize) {
    let tools = tools::defs_for_context(true, total);
    let b = context_breakdown(transcript, &tools);
    let estimate = b.total();
    // The server's own count for the last request beats the chars/4 guess,
    // but only while the transcript it measured is still the live one.
    let measured = if transcript.is_empty() {
        None
    } else {
        usage::last_context_tokens()
    };
    let used = measured.unwrap_or(estimate);
    tui::context_meter(used, total);
    let pct = (used * 100).checked_div(total).unwrap_or(0);
    tui::line(&format!(
        "  context: {} of {} tokens ({pct}%)",
        provider::short_tokens(used),
        provider::short_tokens(total)
    ));
    let rows = [
        ("system prompt", b.system),
        ("tools", b.tools),
        ("MCP tools", b.mcp_tools),
        ("conversation", b.conversation),
        ("images", b.images),
    ];
    for (name, n) in rows {
        tui::line(&tui::dim(&format!(
            "    {name:<14} {:>7}",
            provider::short_tokens(n)
        )));
    }
    tui::line(&tui::dim(&match measured {
        Some(_) => format!(
            "  total measured from the last request; rows estimated at 4 characters a token · {} messages",
            transcript.len()
        ),
        None => format!(
            "  estimated at 4 characters a token — no usage reported yet · {} messages",
            transcript.len()
        ),
    }));
    if pct >= 80 {
        tui::line(&tui::yellow(
            "  nearly full — /compact summarizes the conversation to make room",
        ));
    }
}

fn handle_cost(provider: &Provider) {
    tui::line(&tui::accent("  session usage"));
    for l in usage::render(&usage::snapshot(), &provider.model) {
        tui::line(&tui::dim(&l));
    }
}

// `/effort` shows the level; `/effort <level>` applies it to the live
// provider and persists it to settings.json.
fn handle_effort(provider: &mut Provider, arg: &str) {
    let arg = arg.trim();
    if arg.is_empty() {
        tui::line(&tui::dim(&format!(
            "  effort: {} — /effort off|low|medium|high",
            provider.effort
        )));
        return;
    }
    match config::Effort::parse(arg) {
        Some(level) => {
            provider.effort = level;
            save_user_settings(&[("reasoning_effort", Some(level.as_str().into()))]);
            tui::line(&tui::green(&format!(
                "  ✓ effort → {level} (saved to settings)"
            )));
        }
        None => tui::line(&tui::red(&format!(
            "  unknown effort '{arg}' — try: off, low, medium, high"
        ))),
    }
}

fn handle_checkpoints(cwd: &std::path::Path) {
    let items = checkpoint::list(cwd);
    if items.is_empty() {
        tui::line(&tui::dim("  no checkpoints for this directory"));
        return;
    }
    // Paths are wherever the model wrote; checkpoint files are on disk.
    for cp in items.iter().take(10) {
        tui::line(&format!(
            "  {}  {}  {}",
            tui::bold(&tui::sanitize_terminal(&cp.id)),
            tui::sanitize_terminal(&cp.action),
            tui::sanitize_terminal(&cp.path.display().to_string())
        ));
    }
}

fn handle_undo(cwd: &std::path::Path, arg: &str) {
    let arg = arg.trim();
    if arg.is_empty() {
        // Bare /undo reverts the last agent turn as a unit — the recovery for
        // a partial multi-file edit, where undoing one file would quietly
        // leave the rest changed.
        match checkpoint::undo_last_turn(cwd) {
            Ok(cps) => {
                tui::line(&tui::green(&format!(
                    "  ✓ undid the last agent turn — restored {} file{}:",
                    cps.len(),
                    if cps.len() == 1 { "" } else { "s" }
                )));
                for c in cps {
                    tui::line(&format!(
                        "    - {} ({})",
                        tui::sanitize_terminal(&c.path.display().to_string()),
                        tui::sanitize_terminal(&c.action)
                    ));
                }
            }
            Err(e) => tui::line(&tui::yellow(&format!("  {}", tui::sanitize_terminal(&e)))),
        }
    } else if arg == "git" {
        // The one command here that can destroy work bwn didn't do: it
        // discards ALL unstaged changes, including the user's hand edits.
        // Every file write asks first — the most destructive command must too.
        tui::line(&tui::yellow(
            "  /undo git runs `git checkout -- .` — it discards ALL unstaged changes,",
        ));
        tui::line(&tui::yellow(
            "  including edits you made by hand outside bwn.",
        ));
        let go = tui::ask("  Discard all unstaged changes? [y/N]: ").unwrap_or_default();
        if !matches!(go.trim(), "y" | "Y" | "yes" | "YES") {
            tui::line(&tui::dim("  cancelled — nothing discarded."));
            return;
        }
        match checkpoint::git_rollback(cwd) {
            Ok(msg) => tui::line(&tui::green(&format!(
                "  ✓ git reset: {}",
                tui::sanitize_terminal(&msg)
            ))),
            Err(e) => tui::line(&tui::red(&format!(
                "  git reset error: {}",
                tui::sanitize_terminal(&e)
            ))),
        }
    } else if arg == "all" || arg == "session" {
        let since = checkpoint::now_ms().saturating_sub(24 * 3600 * 1000);
        // Say exactly what's about to be rewound before doing it.
        let pending: Vec<checkpoint::Checkpoint> = checkpoint::list(cwd)
            .into_iter()
            .filter(|c| c.created_ms >= since)
            .collect();
        if pending.is_empty() {
            tui::line(&tui::dim("  no checkpoints in the last 24 hours."));
            return;
        }
        tui::line(&tui::yellow(&format!(
            "  /undo all rewinds every checkpoint from the last 24 hours — {} restore{}:",
            pending.len(),
            if pending.len() == 1 { "" } else { "s" }
        )));
        for c in pending.iter().take(8) {
            tui::line(&tui::dim(&format!(
                "    - {} ({})",
                tui::sanitize_terminal(&c.path.display().to_string()),
                tui::sanitize_terminal(&c.action)
            )));
        }
        if pending.len() > 8 {
            tui::line(&tui::dim(&format!("    … and {} more", pending.len() - 8)));
        }
        let go = tui::ask("  Rewind all of it? [y/N]: ").unwrap_or_default();
        if !matches!(go.trim(), "y" | "Y" | "yes" | "YES") {
            tui::line(&tui::dim("  cancelled — nothing restored."));
            return;
        }
        match checkpoint::undo_all_since(cwd, since) {
            Ok(cps) => {
                tui::line(&tui::green(&format!(
                    "  ✓ restored {} files across session:",
                    cps.len()
                )));
                for c in cps {
                    tui::line(&format!(
                        "    - {} ({})",
                        tui::sanitize_terminal(&c.path.display().to_string()),
                        tui::sanitize_terminal(&c.action)
                    ));
                }
            }
            Err(e) => tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e)))),
        }
    } else if arg == "latest" {
        match checkpoint::undo_latest(cwd) {
            Ok(cp) => tui::line(&tui::green(&format!(
                "  ✓ restored latest {}",
                tui::sanitize_terminal(&cp.path.display().to_string())
            ))),
            Err(e) => tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e)))),
        }
    } else {
        match checkpoint::undo_by_id(cwd, arg) {
            Ok(cp) => tui::line(&tui::green(&format!(
                "  ✓ restored checkpoint {} ({})",
                cp.id,
                tui::sanitize_terminal(&cp.path.display().to_string())
            ))),
            Err(e) => tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e)))),
        }
    }
}

fn handle_align(cwd: &std::path::Path) {
    tui::line(&tui::accent("  alignment interview"));
    tui::line(&tui::dim(
        "  a short alignment review before proceeding with complex changes",
    ));

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

    tui::line(&tui::green("  ✓ alignment recorded"));
    tui::line(&format!("    • Primary Risk: {}", tui::bold(risk_label)));
    tui::line(&format!("    • Reversibility: {}", tui::bold(rev_label)));
    tui::line(&format!(
        "    • Confidence Threshold: {}",
        tui::bold(conf_label)
    ));

    let mut kb = crate::knowledge::KnowledgeBase::new(&cwd.to_string_lossy());
    let id = format!("dec-{}", crate::checkpoint::now_ms());
    let entity = crate::knowledge::Entity {
        id: id.clone(),
        entity_type: crate::knowledge::EntityType::ArchitectureDecision,
        name: format!("Operational Alignment ({})", risk_label),
        path: None,
        description: Some(format!(
            "Risk: {}, Reversibility: {}, Confidence: {}",
            risk_label, rev_label, conf_label
        )),
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
    if let Err(e) = kb.save() {
        tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
        return;
    }
    tui::line(&tui::dim(
        "  Decision recorded into structured knowledge base (.buildwithnexus/knowledge/).",
    ));
}

fn handle_teamwork() {
    tui::line(&tui::accent("  teamwork — multi-agent swarm preview"));
    tui::line(&tui::dim(
        "  for complex projects, buildwithnexus orchestrates specialized subagent teams:",
    ));
    tui::line(&format!(
        "    • {} — Explores documentation, code graphs, and symbol trees",
        tui::bold("Researcher Subagent")
    ));
    tui::line(&format!(
        "    • {} — Analyzes logs, stack traces, and test regressions",
        tui::bold("Debugger Subagent")
    ));
    tui::line(&format!(
        "    • {} — Edits code files, runs migrations, and applies patches",
        tui::bold("Code Writer Subagent")
    ));
    tui::line(&format!(
        "    • {} — Checks engineering rules, static analysis, and confidence",
        tui::bold("Verifier Subagent")
    ));
    tui::line(&tui::dim("  Tip: Use `invoke_subagent` in your custom rules/workflows to dispatch tasks to this team."));
}

fn handle_agents() {
    match config::load_agents() {
        Some(agents) => {
            // Agents.md is Markdown — render it instead of dumping #/**/- raw.
            let shown: Vec<&str> = agents.lines().take(80).collect();
            tui::line(&tui::render_md(&shown.join("\n")));
            let total = agents.lines().count();
            if total > 80 {
                tui::line(&tui::dim(&format!("  …(+{} more lines)", total - 80)));
            }
        }
        None => tui::line(&tui::dim("  no Agents.md found")),
    }
}

// One line per configured MCP server, after a bounded connection attempt.
fn doctor_mcp_lines() -> Vec<String> {
    mcp::ensure_ready();
    let reports = mcp::report();
    if reports.is_empty() {
        return vec!["  ·  mcp          no servers configured".into()];
    }
    reports
        .into_iter()
        .map(|r| {
            let name = format!("mcp:{}", r.name);
            match r.status {
                mcp::Status::Connected => format!(
                    "  ✓ {name:<14} {} · {} tool{}",
                    r.transport,
                    r.tools.len(),
                    if r.tools.len() == 1 { "" } else { "s" }
                ),
                mcp::Status::Disabled => format!("  ·  {name:<13} disabled"),
                mcp::Status::Connecting => {
                    format!("  ✗ {name:<14} still connecting after the timeout")
                }
                mcp::Status::Failed(e) | mcp::Status::Invalid(e) => {
                    format!("  ✗ {name:<14} {}", e.chars().take(160).collect::<String>())
                }
            }
        })
        .map(|l| tui::sanitize_terminal(&l).into_owned())
        .collect()
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
    let (glyph, text) = sandbox::doctor_summary();
    tui::line(&format!("  {glyph} sandbox: {text}"));
    tui::line(&format!("  home: {}", config::home().display()));
    for line in doctor_mcp_lines() {
        tui::line(&line);
    }
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
    tui::line(&tui::dim(
        "  /plan <task>        Break down implementation into steps",
    ));
    tui::line(&tui::dim(
        "  /build <task>       Agentic execution of a task",
    ));
    tui::line(&tui::dim(
        "  /brainstorm <task>  Conversational thought partner",
    ));
    tui::line(&tui::dim("  /model <name>       Hot-swap the AI model"));
    tui::line(&tui::dim(
        "  /permissions        Change what the agent can do unprompted",
    ));
    tui::line(&tui::dim(
        "  /schedule <delay>   Run a task later (e.g. 5m cargo test)",
    ));
    tui::line(&tui::dim(
        "  /loop <interval>    Run a task repeatedly (e.g. 30m)",
    ));
    tui::line(&tui::dim(
        "  /trace <id>         View detailed receipts for a turn",
    ));
    tui::line("");
    tui::line(&tui::bold(&tui::accent("  commands")));
    // (command, args/aliases hint, description) grouped by section. Rendered
    // as an auto-aligned table so alignment can't drift as commands change.
    type Row = (&'static str, &'static str, &'static str);
    let sections: &[(&str, &[Row])] = &[
        (
            "modes",
            &[
                ("Shift+Tab", "", "cycle PLAN → BUILD → BRAINSTORM"),
                ("/mode", "[plan|build|brainstorm]", "show or switch mode"),
                (
                    "/permissions",
                    "[ask|auto|readonly|reset]",
                    "tool permission level (reset: forget always-allow)",
                ),
                (
                    "/sandbox",
                    "[off|auto|require|status]",
                    "OS sandbox for shell commands",
                ),
                (
                    "/model",
                    "[name | <url> <model>]",
                    "hot-swap the AI model mid-session",
                ),
                ("/effort", "[off|low|medium|high]", "reasoning depth"),
                ("/local", "", "probe local servers and list GGUF models"),
            ],
        ),
        (
            "context & git",
            &[
                ("/compact", "", "compress context to free token budget"),
                ("/context", "", "show context window usage"),
                ("/cost", "", "session tokens and estimated cost"),
                ("/diff", "", "show current git diff summary"),
                ("/review", "", "AI code review of staged git diff"),
                ("/commit", "", "AI-drafted conventional commit message"),
                ("/pr", "", "AI-drafted PR title + description"),
                ("/checkpoints", "", "list edit checkpoints"),
                (
                    "/undo",
                    "(/rewind) [latest|git|all|<id>]",
                    "bare: revert the last agent turn's edits",
                ),
            ],
        ),
        (
            "automation",
            &[
                ("/schedule", "<delay> <task>", "one-shot scheduled workflow"),
                ("/loop", "<interval> <task>", "repeating scheduled workflow"),
                ("/workflows", "(/tasks)", "list background workflows"),
                ("/btw", "<context>", "inject context into next agent turn"),
                ("/teamwork", "(/swarm)", "multi-agent swarm preview"),
                ("/grill-me", "(/align)", "operational alignment interview"),
            ],
        ),
        (
            "project",
            &[
                ("/memory", "", "view and edit session memory"),
                ("/skills", "", "list skills and custom commands"),
                ("/tools", "", "browse callable tools"),
                ("/rules", "", "inspect engineering rules and violations"),
                ("/kb", "(/index)", "query or index project knowledge base"),
                (
                    "/verify",
                    "(/audit)",
                    "verify codebase against rules and tests",
                ),
                ("/agents", "", "show loaded Agents.md context"),
                (
                    "/mcp",
                    "[name|add|remove|reload]",
                    "MCP servers and their tools",
                ),
                ("/trace", "", "inspect hooks, tools, skills, subagents"),
            ],
        ),
        (
            "session",
            &[
                ("/new", "", "start a fresh session"),
                ("/resume", "", "pick a saved session to resume"),
                ("/init", "", "run setup (keys, providers, local models)"),
                (
                    "/login",
                    "",
                    "replace the API key, checked before it is saved",
                ),
                ("/config", "", "configure hooks, memory, commands via AI"),
                ("/voice", "[<file>]", "audio transcription & voice input"),
                ("/vim", "", "toggle Vim modal editing"),
                ("/mouse", "[on|off]", "wheel scroll + drag-copy (/scroll)"),
                ("/doctor", "(/debug)", "diagnose setup"),
                ("/clear", "", "clear the screen"),
                ("/exit", "", "exit"),
            ],
        ),
    ];

    let cmd_w = sections
        .iter()
        .flat_map(|(_, rows)| rows.iter())
        .map(|(cmd, _, _)| cmd.chars().count())
        .max()
        .unwrap_or(0);

    tui::line("");
    for (title, rows) in sections {
        tui::line("");
        tui::line(&tui::dim(&format!("  {title}")));
        for (cmd, args, desc) in rows.iter() {
            let pad = " ".repeat(cmd_w.saturating_sub(cmd.chars().count()));
            let args_part = if args.is_empty() {
                String::new()
            } else {
                format!("  {}", tui::dim(args))
            };
            tui::line(&format!("    {}{pad}  {desc}{args_part}", tui::bold(cmd)));
        }
    }
    tui::line("");
    tui::line(&tui::dim("  input"));
    tui::line(&tui::dim(
        "    !<cmd> shell command · @<path> attach file/image/video · @diff @kb: @symbol:",
    ));
    tui::line(&tui::dim(
        "    ^V paste image/text · Tab complete · ↑↓ history · ^R search · ^G $EDITOR",
    ));
    tui::line(&tui::dim(
        "    ←→ ^A ^E move · ^W ^U ^K kill · ^Y yank · PgUp/PgDn scroll",
    ));
    tui::line("");
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

// Parse `@path` attachment tokens. Images become multimodal entries; videos
// are parsed with ffmpeg into sampled frames + a metadata block; readable
// text files are appended to the prompt. Unreadable tokens are left
// unchanged. `vision` gates image/video attachment to multimodal models.
// Headless runs take the same @path attachments as the TUI; a model that
// can't see images gets a stderr warning instead of a silent drop.
fn headless_attachments(
    p: &Provider,
    task: &str,
    cwd: &std::path::Path,
) -> (String, Vec<(String, String)>) {
    let vision = media::model_supports_vision(p);
    let (task, images) = extract_attachments(task, cwd, vision);
    if !images.is_empty() {
        let n = images.len();
        eprintln!("⎘ attached {n} image{}", if n == 1 { "" } else { "s" });
    }
    (task, images)
}

fn extract_attachments(
    task: &str,
    cwd: &std::path::Path,
    vision: bool,
) -> (String, Vec<(String, String)>) {
    use std::io::Read;
    let image_exts = ["png", "jpg", "jpeg", "gif", "webp"];
    let mut images: Vec<(String, String)> = Vec::new();
    let mut clean = String::new();
    let mut text_attachments = Vec::new();
    let words: Vec<String> = shlex::split(task)
        .unwrap_or_else(|| task.split_whitespace().map(|s| s.to_string()).collect());
    for word_str in &words {
        // Sentence punctuation after a path ("what is in @shot.png?") is not
        // part of the file name.
        let word = word_str.trim_end_matches(['?', '!', '.', ',', ';', ':']);
        let is_at = word.starts_with('@');
        let clean_word = word.trim_matches(|c| {
            c == '\'' || c == '"' || c == ',' || c == ';' || c == '(' || c == ')' || c == '`'
        });
        let ext = clean_word.rsplit('.').next().unwrap_or("").to_lowercase();
        let is_img = image_exts.contains(&ext.as_str());
        let is_video = media::VIDEO_EXTS.contains(&ext.as_str());
        if !is_at && !is_img && !is_video {
            if !clean.is_empty() {
                clean.push(' ');
            }
            clean.push_str(word_str);
            continue;
        }
        if let Some(raw_path) = if is_at {
            word.strip_prefix('@')
        } else {
            Some(clean_word)
        } {
            if raw_path == "diff" || raw_path == "git:diff" {
                if let Ok(o) = std::process::Command::new("git")
                    .args(["diff", "HEAD"])
                    .current_dir(cwd)
                    .output()
                {
                    let diff_text = String::from_utf8_lossy(&o.stdout);
                    if !diff_text.trim().is_empty() {
                        text_attachments.push(format!("[git diff HEAD]\n{}", diff_text));
                        if !clean.is_empty() {
                            clean.push(' ');
                        }
                        clean.push_str("[git diff HEAD]");
                        continue;
                    }
                }
            } else if raw_path == "status" || raw_path == "git:status" {
                if let Ok(o) = std::process::Command::new("git")
                    .args(["status", "-s"])
                    .current_dir(cwd)
                    .output()
                {
                    let stat_text = String::from_utf8_lossy(&o.stdout);
                    if !stat_text.trim().is_empty() {
                        text_attachments.push(format!("[git status]\n{}", stat_text));
                        if !clean.is_empty() {
                            clean.push(' ');
                        }
                        clean.push_str("[git status]");
                        continue;
                    }
                }
            } else if let Some(kb_query) = raw_path.strip_prefix("kb:") {
                let kb = crate::knowledge::KnowledgeBase::new(&cwd.to_string_lossy());
                let res = kb.search(kb_query);
                if !res.is_empty() {
                    let summary = res
                        .iter()
                        .map(|e| {
                            format!(
                                "Entity: {} ({:?})\nDescription: {}\nPath: {:?}",
                                e.name,
                                e.entity_type,
                                e.description.as_deref().unwrap_or(""),
                                e.path
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n---\n");
                    text_attachments.push(format!("[knowledge base: {}]\n{}", kb_query, summary));
                    if !clean.is_empty() {
                        clean.push(' ');
                    }
                    clean.push_str(&format!("[kb: {}]", kb_query));
                    continue;
                }
            } else if raw_path == "rules" || raw_path.starts_with("rule:") {
                let engine = crate::rules::RuleEngine::load_defaults();
                let rules_summary = engine
                    .rules
                    .iter()
                    .map(|r| {
                        format!(
                            "Rule [{}]: {} (Severity: {})",
                            r.id, r.description, r.severity
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                text_attachments.push(format!("[active engineering rules]\n{}", rules_summary));
                if !clean.is_empty() {
                    clean.push(' ');
                }
                clean.push_str("[active rules]");
                continue;
            } else if let Some(url) = raw_path
                .strip_prefix("url:")
                .or_else(|| raw_path.strip_prefix("web:"))
            {
                // Only real web URLs, and `--` so a value like `-K file` or
                // `file:///…` can't become curl options or a local read.
                if !is_web_url(url) {
                    tui::line(&tui::yellow(&format!(
                        "  ⚠ @{} not fetched — only http:// and https:// URLs are attached",
                        tui::sanitize_terminal(raw_path)
                    )));
                } else if let Ok(o) = std::process::Command::new("curl")
                    .args(["-sL", "--max-time", "5", "--", url])
                    .output()
                {
                    let web_text = String::from_utf8_lossy(&o.stdout);
                    if !web_text.trim().is_empty() {
                        let snippet: String = web_text.chars().take(8000).collect();
                        text_attachments.push(format!("[web: {}]\n{}", url, snippet));
                        if !clean.is_empty() {
                            clean.push(' ');
                        }
                        clean.push_str(&format!("[web: {}]", url));
                        continue;
                    }
                }
            } else if let Some(sym_query) = raw_path.strip_prefix("symbol:") {
                if let Ok(o) = std::process::Command::new("grep")
                    .args(["-rnI", "-e", sym_query, "--", "."])
                    .current_dir(cwd)
                    .output()
                {
                    let sym_text = String::from_utf8_lossy(&o.stdout);
                    if !sym_text.trim().is_empty() {
                        let snippet: String =
                            sym_text.lines().take(30).collect::<Vec<_>>().join("\n");
                        text_attachments
                            .push(format!("[symbol search: {}]\n{}", sym_query, snippet));
                        if !clean.is_empty() {
                            clean.push(' ');
                        }
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
            // Credentials must never ride along in a prompt unnoticed; the
            // token stays as typed and nothing is read.
            if p.exists() && tools::is_sensitive(&p) {
                tui::line(&tui::yellow(&format!(
                    "  ⚠ {} not attached — it is a sensitive file (keys, credentials, secrets)",
                    tui::sanitize_terminal(&p.display().to_string())
                )));
            } else if image_exts.contains(&ext.as_str()) && p.exists() {
                if !vision {
                    tui::line(&tui::yellow(
                        "  ⚠ current model is not multimodal — image not attached",
                    ));
                } else if let Ok(mut f) = std::fs::File::open(&p) {
                    let mut buf = Vec::new();
                    if f.read_to_end(&mut buf).is_ok() {
                        let media_type = match ext.as_str() {
                            "jpg" | "jpeg" => "image/jpeg",
                            "gif" => "image/gif",
                            "webp" => "image/webp",
                            _ => "image/png",
                        };
                        images.push((media_type.to_string(), media::b64_encode(&buf)));
                        // Show the attachment in the transcript (pixel-perfect
                        // or half-block, see tui::show_image_file); a pasted
                        // screenshot already previewed at paste time is not
                        // drawn twice.
                        tui::show_image_file(&p, true);
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
            } else if media::VIDEO_EXTS.contains(&ext.as_str()) && p.exists() {
                if !vision {
                    tui::line(&tui::yellow(
                        "  ⚠ current model is not multimodal — video not attached",
                    ));
                } else if !media::ffmpeg_available() {
                    tui::line(&tui::yellow(
                        "  ⚠ ffmpeg/ffprobe not found — install ffmpeg to attach videos",
                    ));
                } else {
                    tui::line(&tui::dim(&format!(
                        "  ⎘ parsing video {} with ffmpeg…",
                        p.file_name().unwrap_or_default().to_string_lossy()
                    )));
                    if let Some(v) = media::attach_video(&p) {
                        let n = v.frames.len();
                        images.extend(v.frames);
                        text_attachments.push(v.summary);
                        // First frame inline.
                        tui::show_image_file(&p, true);
                        if !clean.is_empty() {
                            clean.push(' ');
                        }
                        clean.push_str(&format!(
                            "[video: {} — {n} sampled frames attached in order]",
                            p.file_name().unwrap_or_default().to_string_lossy()
                        ));
                        continue;
                    }
                    tui::line(&tui::red(&format!(
                        "  ✗ could not decode video {}",
                        p.display()
                    )));
                }
            } else if let Some(att) = read_text_attachment(&p, range) {
                if let Some(total_kib) = att.truncated_from_kib {
                    tui::line(&tui::yellow(&format!(
                        "  ⚠ {} is {total_kib} KiB — only the first {} KiB attached",
                        p.display(),
                        MAX_ATTACHED_FILE_BYTES / 1024
                    )));
                }
                text_attachments.push(format!("[file: {}]\n{}", p.display(), att.text));
                if !clean.is_empty() {
                    clean.push(' ');
                }
                clean.push_str(&format!("[file: {}]", p.display()));
                continue;
            } else if p.is_file() {
                // Never drop an attachment silently: the token stays in the
                // prompt as typed, and the user is told why.
                tui::line(&tui::yellow(&format!(
                    "  ⚠ could not attach {} (not readable as UTF-8 text) — leaving `{word}` as typed",
                    p.display()
                )));
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

fn is_web_url(url: &str) -> bool {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
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

struct TextAttachment {
    text: String,
    // Total file size in KiB when only the first MAX_ATTACHED_FILE_BYTES
    // were attached; None when the whole file fit.
    truncated_from_kib: Option<u64>,
}

// Oversized files are attached up to the cap with a visible marker rather
// than dropped: a silently missing attachment sends the model a bare path.
fn read_text_attachment(
    path: &std::path::Path,
    range: Option<(usize, usize)>,
) -> Option<TextAttachment> {
    let meta = std::fs::metadata(path).ok()?;
    let (text, truncated_from_kib) = if meta.len() > MAX_ATTACHED_FILE_BYTES {
        use std::io::Read;
        let mut f = std::fs::File::open(path).ok()?;
        let mut buf = vec![0u8; MAX_ATTACHED_FILE_BYTES as usize];
        let mut filled = 0usize;
        while filled < buf.len() {
            match f.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(_) => return None,
            }
        }
        buf.truncate(filled);
        // Cut at a char boundary so a split multi-byte sequence doesn't
        // turn a valid UTF-8 file into garbage.
        let mut text = match String::from_utf8(buf) {
            Ok(t) => t,
            Err(e) => {
                let valid = e.utf8_error().valid_up_to();
                if valid == 0 {
                    return None;
                }
                let mut bytes = e.into_bytes();
                bytes.truncate(valid);
                String::from_utf8(bytes).ok()?
            }
        };
        let total_kib = meta.len().div_ceil(1024);
        text.push_str(&format!(
            "\n[truncated: file is {total_kib} KiB, first {} KiB attached]",
            MAX_ATTACHED_FILE_BYTES / 1024
        ));
        (text, Some(total_kib))
    } else {
        (std::fs::read_to_string(path).ok()?, None)
    };
    let Some((start, end)) = range else {
        return Some(TextAttachment {
            text,
            truncated_from_kib,
        });
    };
    Some(TextAttachment {
        text: text
            .lines()
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
        truncated_from_kib,
    })
}

// Suggest a mode from the task phrasing (used for the "tip" hint, not a gate).
pub fn classify(task: &str) -> Mode {
    let l = task.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| l.contains(*w));
    if has(&[
        "what should",
        "what if",
        "ideas for",
        "ideas on",
        "what to build",
        "what to create",
        "what do you think",
        "why would",
        "why is",
        "why does",
        "how about",
        "what are the options",
        "advice on",
        "suggest",
        "tradeoffs",
        "brainstorm",
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
        Mode::Brainstorm
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
         \x20 buildwithnexus init --agents-md  write AGENTS.md from this repository\n\
         \x20 buildwithnexus login           replace the provider's API key (checked first)\n\
         \x20 buildwithnexus providers       list built-in providers\n\
         \x20 buildwithnexus doctor          diagnose setup (keys, tools, connectivity)\n\
         \x20 buildwithnexus mcp [list|<name>|add|remove|reload]  manage MCP servers\n\
         \x20 buildwithnexus version | help\n\n\
         OPTIONS:\n\
         \x20 --provider <name>             override the configured provider\n\
         \x20 --model <name>                override the configured model\n\
         \x20 --permission-mode <mode>      ask, auto, or readonly\n\
         \x20 --sandbox <mode>              off, auto, or require (OS sandbox for shell commands)\n\
         \x20 --prompt <text>               initial interactive prompt\n\
         \x20 --effort <level>              reasoning depth: off, low, medium, high\n\
         \x20 --max-budget-usd <n>          stop before the next request once spend exceeds n\n\
         \x20 --json                        structured headless output\n\
         \x20 --yes, -y                     auto-approve the plan and execute (plan)\n\
         \x20 --legacy-exit-codes           exit 0 when a run stops short without failing\n\
         \x20 --                            stop parsing options (run -- <task>)\n\n\
         INTERACTIVE:\n\
         \x20 Shift+Tab              cycle mode (PLAN → BUILD → BRAINSTORM → PLAN)\n\
         \x20 /mode [plan|build|brainstorm]    show or switch mode\n\
         \x20 /model [name]                    hot-swap the AI model\n\
         \x20 /effort [off|low|medium|high]    show or set reasoning depth\n\
         \x20 /permissions [ask|auto|readonly|reset] show or switch tool permission level\n\
         \x20                                  (reset forgets this project's always-allow answers)\n\
         \x20 /sandbox [off|auto|require|status] OS sandbox for shell commands\n\
         \x20 /mouse|/scroll [on|off|status]   wheel scroll + drag-to-copy (on by default)\n\
         \x20   or say: \"switch to build mode\" / \"use readonly\"\n\
         \x20 /compact               compress context to free up token budget\n\
         \x20 /context               show current context usage\n\
         \x20 /cost                  session tokens and estimated cost\n\
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
         \x20 /mcp [name|add|remove|reload]  MCP servers and their tools\n\
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
    let load = config::load_settings_diag();
    for i in &load.issues {
        println!(
            "  ✗ settings       {}: {}",
            tui::sanitize_terminal(&i.source),
            tui::sanitize_terminal(&i.error)
        );
    }
    match load.settings.as_ref() {
        None if load.any_present => {
            println!("  ✗ settings       present but unusable — fix the file(s) above");
        }
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
            // Live connectivity through the exact path real requests take —
            // key presence says nothing about whether the provider answers.
            // Ollama is probed via its free /api/tags; everything else pays
            // one output token, which is what a diagnostic command is for.
            match build_provider(s) {
                Ok(p) => {
                    if config::preset(&s.provider).is_some_and(|pr| pr.id == "ollama") {
                        let models = provider::ollama_models(&p.base_url);
                        if models.is_empty() {
                            println!(
                                "  ✗ provider       can't reach Ollama at {} — is it running? (ollama serve)",
                                p.base_url
                            );
                        } else {
                            println!(
                                "  ✓ provider       Ollama at {} — {} model{} installed",
                                p.base_url,
                                models.len(),
                                if models.len() == 1 { "" } else { "s" }
                            );
                        }
                    } else {
                        match provider::validate(&p) {
                            Ok(_) => println!(
                                "  ✓ provider       {} answers as {} (one-token probe)",
                                s.provider, p.model
                            ),
                            // The error can carry the server's response body.
                            Err(e) => println!(
                                "  ✗ provider       {}: {}",
                                s.provider,
                                tui::sanitize_terminal(&e)
                                    .chars()
                                    .take(160)
                                    .collect::<String>()
                            ),
                        }
                    }
                }
                Err(e) => println!(
                    "  ✗ provider       {}",
                    tui::sanitize_terminal(&e.to_string())
                ),
            }
        }
    }

    // Sandbox: the probe runs the real backend once, so this reports whether
    // shell commands would actually be confined on this machine.
    if let Some(s) = &load.settings {
        if let Err(e) = sandbox::configure(&s.sandbox, s.sandbox_network) {
            println!("  ✗ sandbox        {e}");
        }
    }
    let (glyph, text) = sandbox::doctor_summary();
    println!("  {glyph} sandbox        {text}");

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

    // MCP servers: a real connect + handshake each, bounded by their timeouts.
    for line in doctor_mcp_lines() {
        println!("{line}");
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
        let found = crate::tools::find_on_path(bin).is_some();
        let glyph = if found { "✓" } else { "·" };
        println!("  {glyph} {bin:<12} {label}");
    }

    if crate::tools::is_wsl() {
        println!();
        println!("  ✓ WSL2 runtime     detected");
        let home = config::home();
        if crate::tools::is_wsl_windows_mount(&home) {
            println!(
                "  ⚠ WSL2 filesystem  NEXUS_HOME is on a Windows mount ({}).",
                home.display()
            );
            println!("                     Set NEXUS_HOME to a Linux path (~/.buildwithnexus) for 10x faster I/O.");
        } else {
            println!("  ✓ WSL2 filesystem  native Linux filesystem detected (optimal I/O speed)");
        }
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

/// Tools bwn can use from PATH: (binary, Homebrew package, apt package,
/// download page, what it is for, needed by bwn itself).
const DEPENDENCIES: &[(&str, &str, &str, &str, &str, bool)] = &[
    (
        "git",
        "git",
        "git",
        "https://git-scm.com/downloads",
        "/undo, /diff and checkpoints use it",
        true,
    ),
    (
        "rg",
        "ripgrep",
        "ripgrep",
        "https://github.com/BurntSushi/ripgrep#installation",
        "faster searches when the agent runs it; bwn's own search does not need it",
        false,
    ),
    (
        "node",
        "node",
        "nodejs",
        "https://nodejs.org",
        "MCP servers written for Node",
        false,
    ),
    (
        "npm",
        "node",
        "npm",
        "https://nodejs.org",
        "installing Node MCP servers",
        false,
    ),
    (
        "python3",
        "python",
        "python3",
        "https://www.python.org/downloads",
        "Python scripts and MCP servers",
        false,
    ),
];

/// Lists tools missing from PATH as plain advice. It never installs
/// anything: `interactive` (doctor) lists every missing tool with its
/// install command; otherwise (session start) only a missing tool bwn itself
/// needs gets a line. The name is kept for its callers.
pub fn check_and_offer_install_dependencies(interactive: bool) {
    let missing: Vec<_> = DEPENDENCIES
        .iter()
        .filter(|d| crate::tools::find_on_path(d.0).is_none())
        .collect();
    if missing.is_empty() {
        if interactive {
            tui::line(&tui::green(
                "  ✓ dependencies installed (git, rg, node, npm, python3)",
            ));
        }
        return;
    }
    let brew = crate::tools::find_on_path("brew").is_some();
    let apt = crate::tools::find_on_path("apt-get").is_some();
    for &&(bin, brew_pkg, apt_pkg, page, why, needed) in &missing {
        if !interactive && !needed {
            continue;
        }
        let how = install_hint(brew_pkg, apt_pkg, page, brew, apt, cfg!(windows));
        let line = format!(
            "  · {bin} not found — {}{why}; install it with: {how}",
            if needed { "" } else { "optional, " }
        );
        if needed {
            tui::line(&tui::yellow(&line));
        } else {
            tui::line(&tui::dim(&line));
        }
    }
}

// The command (or page) that installs a tool here. Printed for the person to
// run, never run by bwn. `windows` is a parameter so tests cover it anywhere.
fn install_hint(
    brew_pkg: &str,
    apt_pkg: &str,
    page: &str,
    brew: bool,
    apt: bool,
    windows: bool,
) -> String {
    if windows {
        page.to_string()
    } else if brew {
        format!("brew install {brew_pkg}")
    } else if apt {
        format!("sudo apt-get install {apt_pkg}")
    } else {
        page.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_tips_fit_one_line_and_stay_in_character() {
        assert!(STARTUP_TIPS.len() >= 12, "keep the rotation fresh");
        for t in STARTUP_TIPS {
            assert!(t.starts_with("tip: "), "uniform prefix: {t}");
            assert!(t.chars().count() <= 100, "must fit one line: {t}");
            assert!(!t.contains('!'), "exclamation marks are hype: {t}");
        }
        // The picker always returns a member, whatever the clock says.
        assert!(STARTUP_TIPS.contains(&startup_tip()));
    }

    #[test]
    fn model_pick_routes_to_serving_provider() {
        let p = |s: &str| parse_model_pick(s, "anthropic");
        assert_eq!(
            p("claude-sonnet-4-6"),
            ("anthropic".into(), "claude-sonnet-4-6".into())
        );
        assert_eq!(p("gpt-4o"), ("openai".into(), "gpt-4o".into()));
        assert_eq!(
            p("ollama/qwen2.5-coder"),
            ("ollama".into(), "qwen2.5-coder".into())
        );
        assert_eq!(
            p("local/phi-4.gguf"),
            ("llamacpp".into(), "phi-4.gguf".into())
        );
        // Gemini has no native preset — routed through OpenRouter's naming.
        assert_eq!(
            p("gemini-2.5-pro"),
            ("openrouter".into(), "google/gemini-2.5-pro".into())
        );
        // org/model naming is OpenRouter's scheme.
        assert_eq!(
            p("meta-llama/llama-3.3-70b"),
            ("openrouter".into(), "meta-llama/llama-3.3-70b".into())
        );
        // Explicit "<provider> <model>" wins over inference.
        assert_eq!(
            p("groq llama-3.3-70b-versatile"),
            ("groq".into(), "llama-3.3-70b-versatile".into())
        );
        // Unknown names stay on the current provider for the swap to validate.
        assert_eq!(
            p("mystery-model"),
            ("anthropic".into(), "mystery-model".into())
        );
        // Custom endpoints are addressable by preset name too.
        assert_eq!(
            p("custom vllm-model"),
            ("custom".into(), "vllm-model".into())
        );
    }

    #[test]
    fn custom_provider_keyless_http_and_key_guard() {
        let _g = config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let h = std::env::temp_dir().join("bwn-custom-provider-test");
        let _ = std::fs::remove_dir_all(&h);
        std::fs::create_dir_all(&h).unwrap();
        std::env::set_var("NEXUS_HOME", &h);
        std::env::remove_var(config::CUSTOM_KEY);

        // Keyless over plain http to loopback: fine (vLLM's default posture).
        let s = Settings {
            provider: "custom".into(),
            model: "my-vllm-model".into(),
            base_url: Some("http://localhost:8000/v1".into()),
            ..Default::default()
        };
        let p = build_provider(&s).expect("keyless custom endpoint must build");
        assert!(p.api_key.is_none());
        assert_eq!(p.base_url, "http://localhost:8000/v1");
        assert_eq!(p.model, "my-vllm-model");

        // With a key configured, plain http to a REMOTE host is refused…
        config::save_key(config::CUSTOM_KEY, "sk-custom");
        let mut remote = s.clone();
        remote.base_url = Some("http://gateway.example.com/v1".into());
        assert!(build_provider(&remote).is_err());
        // …but loopback and https are both fine.
        assert!(build_provider(&s).unwrap().api_key.is_some());
        let mut tls = s.clone();
        tls.base_url = Some("https://gateway.example.com/v1".into());
        assert!(build_provider(&tls).is_ok());

        std::env::remove_var("NEXUS_HOME");
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn loopback_url_detection() {
        assert!(is_loopback_url("http://localhost:8000/v1"));
        assert!(is_loopback_url("http://127.0.0.1:11434"));
        assert!(is_loopback_url("http://[::1]:8080/v1"));
        assert!(!is_loopback_url("http://gateway.example.com/v1"));
        assert!(!is_loopback_url("http://localhost.evil.com/v1"));
    }

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
        assert!(matches!(
            classify("i need some ideas on what to build"),
            Mode::Brainstorm
        ));
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
    fn classify_short_defaults_brainstorm() {
        assert!(matches!(classify("foo bar"), Mode::Brainstorm));
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
    fn auto_switch_escalates_out_of_brainstorm_only() {
        // Brainstorming mode escalates to Plan mode first before stepping to Build mode.
        assert!(matches!(
            auto_switch_mode("build me a snake game", &Mode::Brainstorm),
            Some(Mode::Plan)
        ));
        assert!(matches!(
            auto_switch_mode("plan the migration to sqlite", &Mode::Brainstorm),
            Some(Mode::Plan)
        ));
        // A deliberate PLAN gate is never silently bypassed.
        assert!(auto_switch_mode("fix the parser bug", &Mode::Plan).is_none());
        // Matching mode: nothing to do.
        assert!(auto_switch_mode("fix the parser bug", &Mode::Build).is_none());
    }

    #[test]
    fn conversational_turns_bypass_plan_and_build_dispatch() {
        assert!(should_answer_conversationally("hello", &Mode::Build));
        assert!(should_answer_conversationally(
            "what can you do?",
            &Mode::Plan
        ));
        assert!(should_answer_conversationally(
            "why is this slow?",
            &Mode::Build
        ));
    }

    #[test]
    fn action_turns_stay_in_active_dispatch() {
        assert!(!should_answer_conversationally(
            "build me a canvas game",
            &Mode::Build
        ));
        assert!(!should_answer_conversationally(
            "find a folder named nexus",
            &Mode::Build
        ));
        assert!(!should_answer_conversationally(
            "read my projects file and tell me what repos I have",
            &Mode::Plan
        ));
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
        ])
        .unwrap();
        assert_eq!(opts.model.as_deref(), Some("qwen3"));
        assert_eq!(opts.permission_mode.as_deref(), Some("acceptEdits"));
        assert_eq!(rest, vec!["fix", "tests"]);
    }

    #[test]
    fn parse_cli_options_extracts_sandbox_mode() {
        let (opts, rest) = parse_cli_options(
            ["--sandbox", "require", "run", "x"]
                .map(str::to_string)
                .to_vec(),
        )
        .unwrap();
        assert_eq!(opts.sandbox.as_deref(), Some("require"));
        assert_eq!(rest, ["run", "x"]);
        let (opts, _) = parse_cli_options(["--sandbox=auto"].map(str::to_string).to_vec()).unwrap();
        assert_eq!(opts.sandbox.as_deref(), Some("auto"));
        assert!(parse_cli_options(
            ["run", "--", "--sandbox", "auto"]
                .map(str::to_string)
                .to_vec()
        )
        .unwrap()
        .0
        .sandbox
        .is_none());
    }

    #[test]
    fn parse_cli_options_effort_and_budget() {
        let (opts, rest) = parse_cli_options(
            ["--effort", "high", "--max-budget-usd=1.50", "run", "x"]
                .map(str::to_string)
                .to_vec(),
        )
        .unwrap();
        assert_eq!(opts.effort.as_deref(), Some("high"));
        assert_eq!(opts.max_budget_usd, Some(1.5));
        assert_eq!(rest, ["run", "x"]);
        // Defaults: no level, no budget.
        let (opts, _) = parse_cli_options(vec![]).unwrap();
        assert!(opts.effort.is_none());
        assert!(opts.max_budget_usd.is_none());
    }

    #[test]
    fn legacy_exit_codes_flag_zeroes_only_incomplete_runs() {
        let (opts, rest) = parse_cli_options(
            ["--legacy-exit-codes", "run", "x"]
                .map(str::to_string)
                .to_vec(),
        )
        .unwrap();
        assert!(opts.legacy_exit_codes);
        assert_eq!(rest, ["run", "x"]);
        assert!(!parse_cli_options(vec![]).unwrap().0.legacy_exit_codes);
        use agent::Outcome;
        assert_eq!(headless_exit_code(Outcome::StepLimit, true, false), 6);
        assert_eq!(headless_exit_code(Outcome::StepLimit, true, true), 0);
        // A turn that errored keeps its code either way.
        assert_eq!(headless_exit_code(Outcome::ApprovalBlocked, false, true), 3);
        assert_eq!(headless_exit_code(Outcome::Failed, false, true), 1);
    }

    #[test]
    fn parse_cli_options_budget_rejects_missing_or_non_positive_values() {
        let err = parse_cli_options(["--max-budget-usd"].map(str::to_string).to_vec()).unwrap_err();
        assert!(err.contains("--max-budget-usd requires a value"), "{err}");
        let err = parse_cli_options(["--max-budget-usd", "abc"].map(str::to_string).to_vec())
            .unwrap_err();
        assert!(err.contains("positive dollar amount"), "{err}");
        let err =
            parse_cli_options(["--max-budget-usd", "0"].map(str::to_string).to_vec()).unwrap_err();
        assert!(err.contains("positive dollar amount"), "{err}");
        // Effort values are validated when the provider is built, but the
        // missing-value rule applies here like every other option.
        let err =
            parse_cli_options(["--effort", "--json"].map(str::to_string).to_vec()).unwrap_err();
        assert!(err.contains("--effort requires a value"), "{err}");
        // After `--` the flags are literal task words.
        let (opts, rest) =
            parse_cli_options(["--", "--max-budget-usd", "5"].map(str::to_string).to_vec())
                .unwrap();
        assert!(opts.max_budget_usd.is_none());
        assert_eq!(rest, ["--max-budget-usd", "5"]);
    }

    #[test]
    fn cli_separator_preserves_literal_option_names() {
        let (opts, rest) = parse_cli_options(
            ["--json", "run", "--", "explain", "--model", "--json"]
                .map(str::to_string)
                .to_vec(),
        )
        .unwrap();
        assert!(opts.json);
        assert!(opts.model.is_none());
        assert_eq!(rest, ["run", "explain", "--model", "--json"]);

        let (opts, rest) =
            parse_cli_options(["run", "--", "--json"].map(str::to_string).to_vec()).unwrap();
        assert!(!opts.json);
        assert_eq!(rest, ["run", "--json"]);

        // The shape workflow.rs spawns: a flag-like task stays the task.
        let (opts, rest) = parse_cli_options(
            ["run", "--json", "--", "--permission auto rm it"]
                .map(str::to_string)
                .to_vec(),
        )
        .unwrap();
        assert!(opts.json && opts.permission_mode.is_none());
        assert_eq!(rest, ["run", "--permission auto rm it"]);
    }

    #[test]
    fn cli_options_reject_missing_or_empty_values() {
        for flag in [
            "--provider",
            "--model",
            "--permission-mode",
            "--permission",
            "--sandbox",
            "--prompt",
        ] {
            for args in [
                vec![flag.to_string()],
                vec![flag.to_string(), "--json".to_string()],
                vec![flag.to_string(), "".to_string()],
                vec![format!("{flag}=")],
            ] {
                let error = parse_cli_options(args).unwrap_err();
                assert!(
                    error.contains(&format!("{flag} requires a value")),
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn cli_inline_prompt_can_start_with_a_dash() {
        let (opts, rest) = parse_cli_options(
            ["--prompt=--model", "--model=example"]
                .map(str::to_string)
                .to_vec(),
        )
        .unwrap();
        assert_eq!(opts.prompt.as_deref(), Some("--model"));
        assert_eq!(opts.model.as_deref(), Some("example"));
        assert!(rest.is_empty());
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
    fn model_endpoint_pick_is_a_custom_url_not_openrouter() {
        // `/model http://localhost:8000/v1 my-model` → custom preset with
        // that base URL — never an OpenRouter "org/model" swap.
        assert_eq!(
            parse_model_endpoint("http://localhost:8000/v1 my-model"),
            Some(("http://localhost:8000/v1".into(), "my-model".into()))
        );
        assert_eq!(
            parse_model_endpoint("HTTPS://api.example.com/v1   gpt-x"),
            Some(("HTTPS://api.example.com/v1".into(), "gpt-x".into()))
        );
        // URL only: swap_model asks for the model name.
        assert_eq!(
            parse_model_endpoint("http://localhost:8000/v1"),
            Some(("http://localhost:8000/v1".into(), String::new()))
        );
        // Not a URL: the regular picker handles it.
        assert_eq!(parse_model_endpoint("custom my-model"), None);
        assert_eq!(parse_model_endpoint("meta-llama/llama-3.3-70b"), None);
        assert_eq!(parse_model_endpoint(""), None);
        // A stray URL reaching parse_model_pick stays on the current
        // provider instead of being misread as an OpenRouter org/model.
        assert_eq!(
            parse_model_pick("http://localhost:8000/v1", "anthropic"),
            ("anthropic".into(), "http://localhost:8000/v1".into())
        );
        // `/model custom <model>` keeps working.
        assert_eq!(
            parse_model_pick("custom my-model", "anthropic"),
            ("custom".into(), "my-model".into())
        );
    }

    #[test]
    fn unknown_top_level_flags_are_rejected_not_prompts() {
        assert!(is_unknown_option("--modle"));
        assert!(is_unknown_option("-x"));
        assert!(is_unknown_option("--json=yes"));
        // Known flag spellings of subcommands and options pass through.
        for known in [
            "-v",
            "--version",
            "-h",
            "--help",
            "-p",
            "--print",
            "-c",
            "-r",
            "--resume",
        ] {
            assert!(!is_unknown_option(known), "{known}");
        }
        // Plain words still become the interactive prompt (`bwn fix the bug`).
        assert!(!is_unknown_option("fix"));
        assert!(!is_unknown_option(""));
        assert!(!is_unknown_option("-"));
        // Everything after `--` is literal text, even if it looks like a flag.
        let (opts, rest) =
            parse_cli_options(vec!["--".into(), "-weird".into(), "task".into()]).unwrap();
        assert!(opts.args_literal);
        assert_eq!(rest, vec!["-weird".to_string(), "task".to_string()]);
        let (opts, _) = parse_cli_options(vec!["fix".into(), "the".into(), "bug".into()]).unwrap();
        assert!(!opts.args_literal);
    }

    #[test]
    fn oversized_text_attachment_is_truncated_with_marker() {
        let dir = std::env::temp_dir().join(format!("bwn-attach-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.log");
        // 300 KiB of short lines, ending with a multi-byte char run so the
        // cut lands mid-sequence and must be repaired.
        let mut body = "0123456789abcde\n".repeat(300 * 1024 / 16);
        body.push_str("éééé");
        std::fs::write(&big, &body).unwrap();

        let att = read_text_attachment(&big, None).expect("big files attach truncated");
        assert_eq!(att.truncated_from_kib, Some(301));
        assert!(att
            .text
            .ends_with("\n[truncated: file is 301 KiB, first 256 KiB attached]"));
        let attached = att.text.split("\n[truncated").next().unwrap();
        assert!(attached.len() <= MAX_ATTACHED_FILE_BYTES as usize);
        assert!(attached.len() > MAX_ATTACHED_FILE_BYTES as usize - 32);
        assert!(attached.starts_with("0123456789abcde\n"));

        // Line ranges still apply on top of the truncated text.
        let ranged = read_text_attachment(&big, Some((2, 2))).unwrap();
        assert_eq!(ranged.text, "0123456789abcde");
        assert_eq!(ranged.truncated_from_kib, Some(301));

        // Small files are untouched.
        let small = dir.join("small.txt");
        std::fs::write(&small, "hi\n").unwrap();
        let att = read_text_attachment(&small, None).unwrap();
        assert_eq!(att.text, "hi\n");
        assert_eq!(att.truncated_from_kib, None);

        // The prompt carries the marker so the model knows the file is cut.
        let (text, _) = extract_attachments("look at @big.log", &dir, false);
        assert!(text.contains("[file: "));
        assert!(text.contains("[truncated: file is 301 KiB, first 256 KiB attached]"));
        assert!(!text.contains(" @big.log"), "token must not be pasted raw");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_rule_files_report_parse_failures() {
        let dir = std::env::temp_dir().join(format!("bwn-rules-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("good.json"),
            r#"{"rules":[{"id":"x","description":"d","severity":"low","message":"m"}]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("bad.yaml"), "rules:\n  - id: nope\n").unwrap();

        let (rules, failures) = load_workspace_rule_files(&dir);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "x");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, "bad.yaml");
        assert!(!failures[0].1.is_empty());
        assert!(
            !failures[0].1.contains("Failed to parse rules file"),
            "reason only: {}",
            failures[0].1
        );
        // No rules dir at all is not an error.
        let (rules, failures) = load_workspace_rule_files(&dir.join("missing"));
        assert!(rules.is_empty() && failures.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rules_listing_neutralizes_escapes_from_repo_rules() {
        let dir = std::env::temp_dir().join(format!("bwn-rules-esc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rules = dir.join(".buildwithnexus").join("rules");
        std::fs::create_dir_all(&rules).unwrap();
        // OSC 52 sets the clipboard; the payload is base64 for "rm -rf ~".
        let rule = serde_json::json!({"rules": [{
            "id": "clip\u{1b}[2J",
            "description": "harmless\u{1b}]52;c;cm0gLXJmIH4=\u{7}",
            "severity": "low",
            "message": "m"
        }]});
        std::fs::write(rules.join("evil.json"), rule.to_string()).unwrap();
        if cfg!(unix) {
            std::fs::write(rules.join("bad\u{1b}]0;x\u{7}.yaml"), "rules: [").unwrap();
        }

        let out = rules_listing(&dir).join("\n");
        assert!(!out.contains("\u{1b}]"), "live OSC in /rules: {out:?}");
        assert!(
            !out.contains("\u{1b}[2J") && !out.contains('\u{7}'),
            "{out:?}"
        );
        assert!(out.contains("harmless␛]52;c;cm0gLXJmIH4="), "{out:?}");
        assert!(out.contains("clip␛[2J"), "{out:?}");
        if cfg!(unix) {
            assert!(out.contains("bad␛]0;x.yaml"), "{out:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn text_attachment_context_is_extracted() {
        let dir = std::env::temp_dir().join(format!("bwn-attach-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "one\ntwo\nthree\n").unwrap();

        let (text, images) = extract_attachments("please read @src/lib.rs:2-3", &dir, true);
        assert!(images.is_empty());
        assert!(text.contains("[file:"));
        assert!(text.contains("two\nthree"));
        assert!(!text.contains("\none\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sensitive_attachment_is_not_read() {
        let dir = std::env::temp_dir().join(format!("bwn-attach-secret-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env.local"), "API_KEY=sk-live-secret\n").unwrap();
        std::fs::write(dir.join("id_rsa.png"), "not an image").unwrap();
        let (text, images) =
            extract_attachments("check @.env.local and id_rsa.png please", &dir, true);
        assert!(images.is_empty());
        assert!(!text.contains("sk-live-secret"), "{text}");
        assert!(!text.contains("[attached files]"), "{text}");
        assert!(text.contains("@.env.local"), "token stays as typed: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn url_attachment_requires_web_scheme() {
        assert!(is_web_url("https://example.com/x"));
        assert!(is_web_url("HTTP://example.com"));
        for bad in [
            "-K/etc/passwd",
            "file:///etc/passwd",
            "--config=x",
            "ftp://h",
            "",
        ] {
            assert!(!is_web_url(bad), "{bad}");
        }
        // Rejected values are never handed to curl, so nothing is attached.
        let (text, _) = extract_attachments("@url:-K/etc/passwd", &std::env::temp_dir(), false);
        assert!(!text.contains("[web:"), "{text}");
    }

    #[test]
    fn symbol_attachment_treats_query_as_pattern() {
        let dir = std::env::temp_dir().join(format!("bwn-attach-sym-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.sh"), "rm --files-from list\n").unwrap();
        // A leading dash is a pattern, not a grep option.
        let (text, _) = extract_attachments("@symbol:--files-from", &dir, false);
        assert!(text.contains("[symbol search: --files-from]"), "{text}");
        assert!(text.contains("rm --files-from list"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn image_path_before_sentence_punctuation_attaches() {
        let dir = std::env::temp_dir().join(format!("bwn-attach-punct-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 1x1 PNG.
        let png = [
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82u8,
        ];
        std::fs::write(dir.join("shot.png"), png).unwrap();
        for prompt in [
            "what is in @shot.png?",
            "describe shot.png.",
            "see @shot.png, then fix it",
        ] {
            let (_, images) = extract_attachments(prompt, &dir, true);
            assert_eq!(images.len(), 1, "{prompt}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unattended_settings_prefers_flag_then_first_env_key() {
        let none = |_: &str| false;
        assert!(unattended_settings(None, none).is_none());
        let openai_only = |k: &str| k == "OPENAI_API_KEY";
        assert_eq!(
            unattended_settings(None, openai_only).unwrap().provider,
            "openai"
        );
        let both = |k: &str| k == "OPENAI_API_KEY" || k == "ANTHROPIC_API_KEY";
        assert_eq!(
            unattended_settings(None, both).unwrap().provider,
            "anthropic"
        );
        assert_eq!(
            unattended_settings(Some("ollama"), none).unwrap().provider,
            "ollama"
        );
        assert_eq!(
            unattended_settings(Some("nope"), none).unwrap().provider,
            "nope"
        );
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
        std::fs::write(
            &file_path,
            "pub fn authenticate_user() {}\npub struct SessionData {}",
        )
        .unwrap();
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
        assert!(super::handle_verify_audit(
            crate::agent::Permission::Auto,
            &dir
        ));
        // Read-only sessions must not run the project's build/test commands.
        assert!(!super::handle_verify_audit(
            crate::agent::Permission::ReadOnly,
            &dir
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_check_and_offer_install_dependencies() {
        super::check_and_offer_install_dependencies(false);
    }

    #[test]
    fn missing_tools_get_an_install_command_that_bwn_never_runs() {
        let page = "https://github.com/BurntSushi/ripgrep#installation";
        assert_eq!(
            install_hint("ripgrep", "ripgrep", page, false, true, false),
            "sudo apt-get install ripgrep"
        );
        assert_eq!(
            install_hint("ripgrep", "ripgrep", page, true, true, false),
            "brew install ripgrep"
        );
        // No package manager known, and Windows (where winget may be absent):
        // the download page.
        assert_eq!(
            install_hint("ripgrep", "ripgrep", page, false, false, false),
            page
        );
        assert_eq!(
            install_hint("ripgrep", "ripgrep", page, true, true, true),
            page
        );
        // Only git is needed by bwn itself; the rest are optional.
        let needed: Vec<&str> = DEPENDENCIES.iter().filter(|d| d.5).map(|d| d.0).collect();
        assert_eq!(needed, ["git"]);
    }

    // A loopback HTTP server answering every request with `respond(method,
    // path, body)` → (status, JSON body). Runs until the test process exits.
    fn mock_http(respond: fn(&str, &str, &str) -> (u16, String)) -> u16 {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if reader.read_line(&mut first).is_err() {
                    continue;
                }
                let mut parts = first.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; len];
                let _ = reader.read_exact(&mut body);
                let (status, reply) = respond(&method, &path, &String::from_utf8_lossy(&body));
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        port
    }

    // Runs `swap_model` against settings saved under a scratch NEXUS_HOME and
    // returns the settings it left behind plus the live provider.
    fn swap_with_saved(
        saved: config::Settings,
        target: &str,
        model: &str,
    ) -> (config::Settings, Provider) {
        let _g = config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!(
            "bwn-swap-{}-{}",
            std::process::id(),
            saved.provider
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("NEXUS_HOME", &home);
        config::save_settings(&saved);
        let mut provider = build_provider(&saved).unwrap();
        swap_model(&mut provider, target, model, None);
        let after = config::load_settings().unwrap();
        std::env::remove_var("NEXUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
        (after, provider)
    }

    #[test]
    fn same_provider_swap_on_a_remote_ollama_keeps_its_base_url() {
        let port = mock_http(|_, path, _| match path {
            "/api/tags" => (200, r#"{"models":[{"name":"m1"},{"name":"m2"}]}"#.into()),
            _ => (200, "{}".into()),
        });
        let url = format!("http://127.0.0.1:{port}");
        let (after, provider) = swap_with_saved(
            config::Settings {
                provider: "ollama".into(),
                model: "m1".into(),
                base_url: Some(url.clone()),
                ..Default::default()
            },
            "ollama",
            "m2",
        );
        assert_eq!(after.model, "m2");
        assert_eq!(after.base_url.as_deref(), Some(url.as_str()));
        assert_eq!(provider.base_url, url);
    }

    // An OpenAI-compatible local server that only answers as "local-model".
    fn local_model_only(method: &str, path: &str, body: &str) -> (u16, String) {
        match (method, path) {
            ("GET", "/v1/models") => (200, r#"{"data":[{"id":"local-model"}]}"#.into()),
            ("POST", "/v1/chat/completions") if body.contains(r#""model":"local-model""#) => {
                (200, r#"{"choices":[{"message":{"content":"ok"}}]}"#.into())
            }
            ("POST", _) => (404, r#"{"error":"model not found"}"#.into()),
            _ => (200, "{}".into()),
        }
    }

    #[test]
    fn local_server_swaps_use_the_saved_base_url_and_report_the_fallback() {
        for preset in ["lmstudio", "llamacpp"] {
            let port = mock_http(local_model_only);
            let url = format!("http://127.0.0.1:{port}/v1");
            let (after, provider) = swap_with_saved(
                config::Settings {
                    provider: preset.into(),
                    model: "old".into(),
                    base_url: Some(url.clone()),
                    ..Default::default()
                },
                preset,
                "qwen-7b",
            );
            assert_eq!(after.base_url.as_deref(), Some(url.as_str()), "{preset}");
            assert_eq!(after.model, "local-model", "{preset}");
            assert_eq!(provider.model, "local-model", "{preset}");
            assert_eq!(provider.base_url, url, "{preset}");
        }
    }

    // (from, saved base_url, to, override URL, expected target)
    type SwapCase = (
        &'static str,
        Option<&'static str>,
        &'static str,
        Option<&'static str>,
        SwapTarget,
    );

    #[test]
    fn plan_swap_over_preset_transitions() {
        let preset = |id| config::preset(id).unwrap();
        let keep = |url: &str| SwapTarget {
            base_url: url.into(),
            save: None,
        };
        let reset = |id| SwapTarget {
            base_url: preset(id).base_url.into(),
            save: Some(None),
        };
        let remote_ollama = "http://gpu-box:11434";
        let lan_lmstudio = "http://10.0.0.5:1234/v1";
        let lan_llama = "http://10.0.0.6:8080/v1";
        let table: Vec<SwapCase> = vec![
            (
                "ollama",
                Some(remote_ollama),
                "ollama",
                None,
                keep(remote_ollama),
            ),
            (
                "ollama",
                None,
                "ollama",
                None,
                keep(preset("ollama").base_url),
            ),
            (
                "ollama",
                Some(remote_ollama),
                "anthropic",
                None,
                reset("anthropic"),
            ),
            ("anthropic", None, "ollama", None, reset("ollama")),
            (
                "lmstudio",
                Some(lan_lmstudio),
                "lmstudio",
                None,
                keep(lan_lmstudio),
            ),
            (
                "llamacpp",
                Some(lan_llama),
                "llamacpp",
                None,
                keep(lan_llama),
            ),
            (
                "llamacpp",
                Some(lan_llama),
                "lmstudio",
                None,
                reset("lmstudio"),
            ),
            (
                "lmstudio",
                Some(lan_lmstudio),
                "ollama",
                None,
                reset("ollama"),
            ),
            ("openrouter", None, "openai", None, reset("openai")),
            (
                "anthropic",
                None,
                "custom",
                Some("http://127.0.0.1:9000/v1"),
                SwapTarget {
                    base_url: "http://127.0.0.1:9000/v1".into(),
                    save: Some(Some("http://127.0.0.1:9000/v1".into())),
                },
            ),
            (
                "custom",
                Some("http://127.0.0.1:9000/v1"),
                "custom",
                None,
                keep("http://127.0.0.1:9000/v1"),
            ),
        ];
        for (from, saved, to, url, want) in table {
            assert_eq!(
                plan_swap(from, saved, None, preset(to), url),
                want,
                "{from} ({saved:?}) → {to} ({url:?})"
            );
        }
    }

    #[test]
    fn plan_swap_returns_to_the_address_last_used_with_a_provider() {
        let preset = |id| config::preset(id).unwrap();
        let lan = "http://192.168.50.10:11434";
        // LM Studio → Ollama: back to the LAN box, and it is saved again.
        assert_eq!(
            plan_swap("lmstudio", None, Some(lan), preset("ollama"), None),
            SwapTarget {
                base_url: lan.into(),
                save: Some(Some(lan.into())),
            }
        );
        // Within a provider the saved address still wins, and a typed URL
        // wins over both.
        assert_eq!(
            plan_swap(
                "ollama",
                Some("http://a:11434"),
                Some(lan),
                preset("ollama"),
                None
            )
            .base_url,
            "http://a:11434"
        );
        assert_eq!(
            plan_swap(
                "lmstudio",
                None,
                Some(lan),
                preset("ollama"),
                Some("http://b:11434")
            )
            .base_url,
            "http://b:11434"
        );

        // Leaving a provider remembers its address; arriving records the new one.
        let own = |provider: &str, url: Option<&str>| config::Settings {
            provider: provider.into(),
            base_url: url.map(String::from),
            ..Default::default()
        };
        let map = remember_endpoints(
            &own("ollama", Some(lan)),
            "ollama",
            "lmstudio",
            Some(Some("http://127.0.0.1:1234/v1")),
        );
        assert_eq!(map.get("ollama").map(String::as_str), Some(lan));
        assert_eq!(
            map.get("lmstudio").map(String::as_str),
            Some("http://127.0.0.1:1234/v1")
        );
        // A preset default (no saved URL) adds nothing.
        assert!(
            remember_endpoints(&own("anthropic", None), "anthropic", "openai", Some(None))
                .is_empty()
        );
        // Only the user's own addresses are remembered: a base_url a trusted
        // project layers in serves that project, never every project.
        let mine = own("custom", Some("http://my-gateway:8080/v1"));
        let map = remember_endpoints(&mine, "custom", "ollama", Some(Some(lan)));
        assert_eq!(map["custom"], "http://my-gateway:8080/v1");
        let map = remember_endpoints(&mine, "custom", "custom", None);
        assert_eq!(map["custom"], "http://my-gateway:8080/v1");
        let map = remember_endpoints(&own("anthropic", None), "custom", "ollama", Some(Some(lan)));
        assert_eq!(map.keys().collect::<Vec<_>>(), ["ollama"]);
    }

    #[test]
    fn ollama_addresses_pick_the_ollama_preset() {
        assert!(is_ollama_address("http://192.168.50.10:11434"));
        assert!(is_ollama_address("http://gpu-box:11434/"));
        assert!(is_ollama_address("http://localhost:11434/v1"));
        assert!(is_ollama_address("http://user:pw@host:11434"));
        assert!(!is_ollama_address("http://127.0.0.1:8081/v1"));
        assert!(!is_ollama_address("https://gateway.example/11434"));
        assert_eq!(endpoint_preset("http://192.168.50.10:11434"), "ollama");
        assert_eq!(endpoint_preset("http://192.168.50.10:11434/"), "ollama");
        // Ollama's OpenAI-compatible /v1 stays the custom preset, as before.
        assert_eq!(endpoint_preset("http://192.168.50.10:11434/v1"), "custom");
        // A path, or nothing answering /api/tags, means an OpenAI-compatible
        // endpoint.
        assert_eq!(endpoint_preset("http://127.0.0.1:9/v1"), "custom");
        assert_eq!(endpoint_preset("http://127.0.0.1:9"), "custom");
        // An Ollama on another port is recognised by its own API.
        let port = mock_http(|_, path, _| match path {
            "/api/tags" => (200, r#"{"models":[{"name":"tinycoder:3b"}]}"#.into()),
            _ => (404, "{}".into()),
        });
        assert_eq!(
            endpoint_preset(&format!("http://127.0.0.1:{port}")),
            "ollama"
        );
    }

    #[test]
    fn swap_back_to_ollama_goes_to_the_remembered_host() {
        let port = mock_http(|_, path, _| match path {
            "/api/tags" => (200, r#"{"models":[{"name":"tinycoder:3b"}]}"#.into()),
            _ => (200, "{}".into()),
        });
        let lan = format!("http://127.0.0.1:{port}");
        let (after, provider) = swap_with_saved(
            config::Settings {
                provider: "lmstudio".into(),
                model: "tinycoder-7b-instruct".into(),
                endpoints: [("ollama".to_string(), lan.clone())].into(),
                ..Default::default()
            },
            "ollama",
            "tinycoder:3b",
        );
        assert_eq!(after.provider, "ollama");
        assert_eq!(after.base_url.as_deref(), Some(lan.as_str()));
        assert_eq!(provider.base_url, lan);
        assert_eq!(after.endpoints.get("ollama"), Some(&lan));
    }

    #[test]
    fn swap_success_line_names_the_model_actually_saved() {
        let same = swap_success_line("qwen", "qwen", "LM Studio");
        assert!(same.contains("→ qwen on LM Studio"), "{same}");
        let fell_back = swap_success_line("qwen", "local-model", "LM Studio");
        assert!(
            fell_back.contains("→ local-model on LM Studio"),
            "{fell_back}"
        );
        assert!(fell_back.contains("'qwen'"), "{fell_back}");
    }

    #[test]
    fn llama_server_is_found_as_an_exe_on_windows() {
        let dir = std::env::temp_dir().join(format!("bwn-llama-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // PATHEXT spells .EXE in upper case; Windows disks ignore case, this
        // one may not.
        let exe = dir.join("llama-server.EXE");
        std::fs::write(&exe, "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths([&dir]).unwrap();
        assert_eq!(find_llama_server_in(&path, None, true), Some(exe.clone()));
        assert_eq!(
            find_llama_server_in(&path, Some(std::ffi::OsStr::new(".COM;.EXE")), true),
            Some(exe)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_typed_permission_switch_lasts_for_the_session_only() {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("bwn-perm-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("NEXUS_HOME", &home);
        let file = home.join("settings.json");
        std::fs::write(
            &file,
            r#"{"provider":"anthropic","model":"m","permission":"ask"}"#,
        )
        .unwrap();
        let saved = || -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap()
        };
        let cwd = home.clone();
        let mut perm = Permission::Ask;

        // "use auto" and `/permissions auto` change the session only.
        let phrase = super::detect_permission_switch("use auto").unwrap();
        super::apply_permission(&mut perm, phrase, super::PermScope::Session);
        assert_eq!(perm, Permission::Auto);
        assert_eq!(saved()["permission"], "ask");
        super::handle_permissions_arg(&mut perm, &cwd, "accept-edits");
        assert_eq!(perm, Permission::AcceptEdits);
        assert_eq!(saved()["permission"], "ask");
        // Saving the default is its own, explicit choice.
        super::handle_permissions_arg(&mut perm, &cwd, "default auto");
        assert_eq!(perm, Permission::Auto);
        assert_eq!(saved()["permission"], "auto");
        // A name that isn't a mode changes nothing.
        super::handle_permissions_arg(&mut perm, &cwd, "yolo2");
        assert_eq!(perm, Permission::Auto);
        // The number shortcuts keep their 0.14 meaning (3 never loosens
        // to auto); accept-edits is the new 4.
        for (arg, want) in [
            ("1", Permission::Ask),
            ("3", Permission::ReadOnly),
            ("2", Permission::Auto),
            ("4", Permission::AcceptEdits),
        ] {
            super::handle_permissions_arg(&mut perm, &cwd, arg);
            assert_eq!(perm, want, "/permissions {arg}");
        }

        std::env::remove_var("NEXUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn local_servers_start_with_the_configured_one() {
        let s = Settings {
            provider: "ollama".into(),
            base_url: Some("http://192.168.50.10:11434".into()),
            ..Default::default()
        };
        let bases: Vec<(&str, String)> = local_servers(&s)
            .into_iter()
            .map(|l| (l.preset, l.base))
            .collect();
        assert_eq!(
            bases,
            vec![
                ("ollama", "http://192.168.50.10:11434".to_string()),
                ("ollama", "http://localhost:11434".to_string()),
                ("llamacpp", "http://localhost:8080/v1".to_string()),
                ("lmstudio", "http://localhost:1234/v1".to_string()),
                ("custom", "http://localhost:8000/v1".to_string()),
            ]
        );
        // LM Studio moved to another port: listed once, at that port.
        let s = Settings {
            provider: "lmstudio".into(),
            base_url: Some("http://localhost:1235/v1/".into()),
            ..Default::default()
        };
        let servers = local_servers(&s);
        assert_eq!(servers[0].base, "http://localhost:1235/v1");
        assert_eq!(servers[0].label, "LM Studio");
        assert_eq!(servers.len(), 5);
        // A hosted provider adds nothing of its own.
        let s = Settings {
            provider: "openai".into(),
            base_url: Some("https://api.openai.com/v1".into()),
            ..Default::default()
        };
        assert_eq!(local_servers(&s).len(), 4);
    }

    // Answers GETs: /api/tags only when `ollama`, /v1/models always.
    fn model_server(ollama: bool) -> String {
        use std::io::{BufRead, BufReader, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in l.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut first = String::new();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let _ = reader.read_line(&mut first);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                let (code, body) = if first.contains("/api/tags") && ollama {
                    (200, r#"{"models":[{"name":"tinycoder:3b"}]}"#)
                } else if first.contains("/v1/models") {
                    (200, r#"{"data":[{"id":"x"}]}"#)
                } else {
                    (404, r#"{"error":"Unexpected endpoint"}"#)
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        base
    }

    #[test]
    fn a_gguf_swap_never_lands_on_ollama() {
        let ollama = format!("{}/v1", model_server(true));
        let llama = format!("{}/v1", model_server(false));
        assert_eq!(
            find_active_local_base_url_in(&[&ollama, &llama]),
            Some(llama.clone())
        );
        assert_eq!(find_active_local_base_url_in(&[&ollama]), None);

        assert_eq!(
            gguf_unservable("tinycoder-7b-q4_k_m.gguf", false),
            Some(
                "llama-server is not installed — install llama.cpp, or load the file in LM Studio"
            )
        );
        assert!(gguf_unservable("sub/Model.GGUF", false).is_some());
        assert_eq!(gguf_unservable("tinycoder-7b-q4_k_m.gguf", true), None);
        assert_eq!(gguf_unservable("tinycoder-7b-instruct", false), None);
    }

    #[test]
    fn context_breakdown_counts_each_part_of_the_next_request() {
        let msgs = vec![
            provider::Msg::System("s".repeat(400)),
            provider::Msg::User("u".repeat(80)),
            provider::Msg::UserImages {
                text: "t".repeat(40),
                images: vec![("image/png".into(), "A".repeat(1_200))],
            },
            provider::Msg::Assistant {
                text: "a".repeat(40),
                calls: vec![],
            },
            provider::Msg::Tool(vec![provider::ToolResult {
                id: "1".into(),
                content: "r".repeat(40),
                is_error: false,
            }]),
        ];
        let tools = vec![tools::ToolDef {
            name: "read_file",
            description: "Read a file.",
            schema: serde_json::json!({}),
        }];
        let b = context_breakdown(&msgs, &tools);
        assert_eq!(b.system, 100);
        assert_eq!(b.conversation, 20 + 10 + 10 + 10);
        assert_eq!(b.images, 100);
        assert_eq!(b.tools, (9 + 12 + 2) / 4);
        assert_eq!(b.mcp_tools, 0);
        assert_eq!(b.total(), 100 + 50 + 100 + 5);
    }
}
