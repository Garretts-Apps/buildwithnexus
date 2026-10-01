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
    /// `--base-url <url>`: the model endpoint, over the settings value.
    base_url: Option<String>,
    /// Words before `--` that look like options but are none of ours, in
    /// order. Commands with options of their own (`mcp add --url`) read
    /// them; every other command refuses them as a usage error.
    unknown_flags: Vec<String>,
    /// `--worktree <name>`: run the session in .bwn/worktrees/<name> on
    /// branch bwn/<name>.
    worktree: Option<String>,
}

/// Every option `parse_cli_options` knows, for "did you mean" hints.
const CLI_OPTIONS: &[&str] = &[
    "--provider",
    "--model",
    "--base-url",
    "--permission-mode",
    "--permission",
    "--sandbox",
    "--prompt",
    "--effort",
    "--max-budget-usd",
    "--json",
    "--yes",
    "--legacy-exit-codes",
    "--plain",
    "--trust-project",
    "--worktree",
    "--help",
    "--version",
];

// `-x` or `--word`; a lone `-` and negative numbers (`-1`) are plain words.
fn looks_like_option(arg: &str) -> bool {
    arg.len() > 1 && arg.starts_with('-') && !arg[1..].starts_with(|c: char| c.is_ascii_digit())
}

/// `unknown option --modle (did you mean --model?)`.
fn unknown_option_msg(flag: &str) -> String {
    unknown_option_among(flag, CLI_OPTIONS)
}

fn unknown_option_among(flag: &str, known: &[&str]) -> String {
    let name = flag.split('=').next().unwrap_or(flag);
    let near = known
        .iter()
        .map(|o| (tools::levenshtein(name, o), *o))
        .filter(|(d, _)| *d <= 2)
        .min();
    match near {
        Some((_, o)) => format!("unknown option {name} (did you mean {o}?); see --help"),
        None => format!("unknown option {name}; see --help"),
    }
}

fn parse_cli_options(args: Vec<String>) -> Result<(CliOptions, Vec<String>), String> {
    let mut opts = CliOptions::default();
    let mut rest = Vec::new();
    let mut budget_raw: Option<String> = None;
    let mut literal_from: Option<usize> = None;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        if arg == "--" {
            opts.args_literal = true;
            literal_from = Some(rest.len());
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
        // Line mode: no alternate screen or cursor addressing (TERM=dumb
        // does the same), for screen readers and plain consoles.
        if arg == "--plain" {
            tui::set_line_mode(true);
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
            "--base-url" => &mut opts.base_url,
            "--worktree" => &mut opts.worktree,
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
    // The first word may be a flag-spelled command (`-p`, `--version`).
    let options_end = literal_from.unwrap_or(rest.len());
    opts.unknown_flags = rest[..options_end]
        .iter()
        .enumerate()
        .filter(|(i, a)| looks_like_option(a) && !(*i == 0 && !is_unknown_option(a)))
        .map(|(_, a)| a.clone())
        .collect();
    if let Some(level) = &opts.effort {
        config::Effort::parse(level).ok_or_else(|| {
            format!("--effort must be one of off, low, medium, high (got '{level}')")
        })?;
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

#[cfg(test)]
mod cli_option_tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<(CliOptions, Vec<String>), String> {
        parse_cli_options(args.iter().map(|a| a.to_string()).collect())
    }

    #[test]
    fn options_nobody_knows_are_collected_before_the_separator_only() {
        let (opts, rest) = parse(&["run", "--modle", "big", "fix it"]).unwrap();
        assert_eq!(opts.unknown_flags, ["--modle"]);
        assert_eq!(rest, ["run", "--modle", "big", "fix it"]);
        let (opts, _) = parse(&["run", "--", "--modle", "big"]).unwrap();
        assert!(opts.unknown_flags.is_empty());
        // Flag-spelled commands first, negative numbers and a lone dash pass.
        let (opts, _) = parse(&["-p", "subtract", "-1", "-", "x"]).unwrap();
        assert!(opts.unknown_flags.is_empty());
        let (opts, _) = parse(&["run", "-p", "x"]).unwrap();
        assert_eq!(opts.unknown_flags, ["-p"]);
    }

    #[test]
    fn unknown_options_suggest_the_nearest_real_one() {
        assert_eq!(
            unknown_option_msg("--modle"),
            "unknown option --modle (did you mean --model?); see --help"
        );
        assert_eq!(
            unknown_option_msg("--base_url=http://x"),
            "unknown option --base_url (did you mean --base-url?); see --help"
        );
        assert_eq!(
            unknown_option_msg("--frobnicate"),
            "unknown option --frobnicate; see --help"
        );
    }

    #[test]
    fn base_url_is_an_option_and_effort_is_checked_while_parsing() {
        let (opts, rest) = parse(&["--base-url", "https://gw.example/v1", "run", "x"]).unwrap();
        assert_eq!(opts.base_url.as_deref(), Some("https://gw.example/v1"));
        assert_eq!(rest, ["run", "x"]);
        let err = parse(&["--effort", "hihg", "run", "x"]).unwrap_err();
        assert!(err.contains("off, low, medium, high"), "{err}");
    }
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
    // A mistyped option must not become part of a task (or an interactive
    // prompt): `run --modle big '<task>'` sends nothing and says so.
    // Commands with options of their own check them themselves.
    let own_options = matches!(cmd, "mcp" | "trust" | "update" | "review");
    // `init --agents-md` is init's one option; any other is still a mistake.
    let command_flags: &[&str] = match cmd {
        "init" | "da-init" | "setup" => &["--agents-md"],
        _ => &[],
    };
    let unknown = opts
        .unknown_flags
        .iter()
        .find(|f| !command_flags.contains(&f.as_str()));
    if let (false, Some(flag)) = (own_options, unknown) {
        if matches!(flag.as_str(), "-h" | "--help") {
            usage();
            return;
        }
        eprintln!("buildwithnexus: {}", unknown_option_msg(flag));
        std::process::exit(2);
    }
    if let Some(name) = &opts.worktree {
        if let Err((code, e)) = enter_session_worktree(name) {
            eprintln!("buildwithnexus: --worktree: {e}");
            std::process::exit(code);
        }
    }

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
            let input = HeadlessInput::require(&rest());
            headless(&opts, |p, perm, cwd| {
                if let Some((cmd, args)) = find_slash_command(&input.argv) {
                    if let Some(script) = &cmd.script {
                        let out = run_script_command(script, &args, perm, &cwd);
                        let text = out.as_ref().unwrap_or_else(|e| e);
                        report::tool_result("run_command", text, out.is_err());
                        return out.map(|_| ());
                    }
                }
                let (task, images) = input.task(p, &cwd);
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
            let input = HeadlessInput::require(&rest());
            headless(&opts, |p, perm, cwd| {
                let (task, images) = input.task(p, &cwd);
                agent::run_plan(p, perm, &task, &cwd, opts.yes, images)
            })
        }
        "brainstorm" => {
            let input = HeadlessInput::require(&rest());
            headless(&opts, |p, perm, cwd| {
                let (task, images) = input.task(p, &cwd);
                agent::run_brainstorm(p, perm, &cwd, &task, images).map(|_| ())
            })
        }
        "sessions" => sessions_command(&args[1..]),
        "continue" | "-c" | "--continue" => continue_command(opts.clone(), rest()),
        "resume" | "-r" | "--resume" => resume_command(opts.clone(), &args[1..]),
        "-v" | "-V" | "--version" | "version" => println!("buildwithnexus {VERSION}"),
        "-h" | "--help" | "help" => usage(),
        "doctor" => run_doctor(&opts),
        "trust" => std::process::exit(hooks::trust_cli(&args[1..])),
        "review" => {
            let req = match ReviewRequest::parse(&args[1..]) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("buildwithnexus review: {e}");
                    std::process::exit(2);
                }
            };
            headless(&opts, |p, _perm, cwd| headless_review(p, &req, &cwd))
        }
        "update" => std::process::exit(update::cli(&args[1..])),
        "mcp" => match mcp::manage(&args[1..], false) {
            Ok(lines) => {
                for l in lines {
                    println!("  {l}");
                }
            }
            Err(e) => {
                eprintln!("buildwithnexus mcp: {e}");
                // A refusal to overwrite is not a usage mistake.
                std::process::exit(if e.ends_with(mcp::EXISTS) { 1 } else { 2 });
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
    print_session_worktree_hint();
}

// `--json sessions`: one `session` event per saved session, newest first,
// and nothing else on stdout (an empty list prints nothing).
fn print_sessions_json(all: &[session::Session]) {
    for s in all {
        report::event(serde_json::json!({
            "type": "session",
            "id": s.id,
            "title": s.title,
            "cwd": s.cwd,
            "model": s.model,
            "created_ms": s.created_ms as u64,
            "updated_ms": s.updated_ms as u64,
            "messages": s.msgs.len(),
        }));
    }
}

// The worktree a `--worktree` session runs in: (path, branch, repo root).
static SESSION_WORKTREE: std::sync::OnceLock<(PathBuf, String, PathBuf)> =
    std::sync::OnceLock::new();

fn git_in(dir: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("git could not start ({e})"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err
            .lines()
            .last()
            .unwrap_or("git failed")
            .trim()
            .to_string())
    }
}

// A name git accepts in a branch and a folder: letters, digits, `.`, `_`, `-`.
fn worktree_name_ok(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['.', '-'])
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `--worktree <name>`: create (or reuse) <repo>/.bwn/worktrees/<name> on
/// branch bwn/<name> from HEAD and move the session into it. `.bwn/` goes
/// into the repository's info/exclude so the main checkout never shows it.
fn enter_session_worktree(name: &str) -> Result<(), (i32, String)> {
    if !worktree_name_ok(name) {
        return Err((
            2,
            format!("'{name}' is not a usable name — use letters, digits, '.', '_' or '-'"),
        ));
    }
    let cwd = std::env::current_dir().map_err(|e| (1, e.to_string()))?;
    let root = git_in(&cwd, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .map_err(|_| (1, "not inside a git repository".to_string()))?;
    git_in(&root, &["rev-parse", "--verify", "HEAD"])
        .map_err(|_| (1, "the repository has no commits yet".to_string()))?;
    let path = root.join(".bwn").join("worktrees").join(name);
    let branch = format!("bwn/{name}");
    if !path.join(".git").exists() {
        let has_branch = git_in(
            &root,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        )
        .is_ok();
        let path_s = path.to_string_lossy().into_owned();
        let args: Vec<&str> = if has_branch {
            vec!["worktree", "add", &path_s, &branch]
        } else {
            vec!["worktree", "add", "-b", &branch, &path_s, "HEAD"]
        };
        git_in(&root, &args).map_err(|e| (1, format!("git worktree add failed: {e}")))?;
    }
    if let Ok(common) = git_in(&root, &["rev-parse", "--git-common-dir"]) {
        let exclude = root.join(common).join("info").join("exclude");
        let text = std::fs::read_to_string(&exclude).unwrap_or_default();
        if !text.lines().any(|l| l.trim() == "/.bwn/") {
            let _ = std::fs::create_dir_all(exclude.parent().unwrap_or(&root));
            let sep = if text.is_empty() || text.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            let _ = std::fs::write(&exclude, format!("{text}{sep}/.bwn/\n"));
        }
    }
    std::env::set_current_dir(&path).map_err(|e| (1, e.to_string()))?;
    eprintln!(
        "{}",
        tui::dim(&format!(
            "buildwithnexus: working in .bwn/worktrees/{name} on branch {branch}"
        ))
    );
    let _ = SESSION_WORKTREE.set((path, branch, root));
    Ok(())
}

/// On the way out of a `--worktree` session: where the work is and how to
/// bring it in.
fn print_session_worktree_hint() {
    let Some((path, branch, root)) = SESSION_WORKTREE.get() else {
        return;
    };
    let shown = path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string();
    let dirty = git_in(path, &["status", "--porcelain"])
        .map(|s| s.lines().count())
        .unwrap_or(0);
    let pending = if dirty > 0 {
        format!(
            " ({dirty} uncommitted change{} there — commit them first)",
            if dirty == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };
    eprintln!(
        "{}",
        tui::yellow(&tui::sanitize_terminal(&format!(
            "buildwithnexus: this session's work is on branch {branch} in {shown}{pending} — git merge {branch}"
        )))
    );
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
    if let Some(u) = &opts.base_url {
        settings.base_url = Some(u.clone());
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
    "tip: /undo puts back the last turn's edits. go on, be brave",
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

/// Most piped input a headless run sends, as the task or as a `[stdin]`
/// block after it; the rest is read and dropped so the writer never sees a
/// broken pipe.
const MAX_STDIN_BYTES: usize = 1024 * 1024;
/// With a task on the command line, stdin is read only if something arrives
/// this soon: a pipe a parent process leaves open must not hang the run.
const STDIN_FIRST_BYTE_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// What a headless run was asked to do: the task from argv, and what was
/// piped on stdin.
struct HeadlessInput {
    argv: String,
    stdin: Option<String>,
    // Said once the run has started (truncation, an ignored silent pipe).
    notice: Option<String>,
}

#[derive(Debug, PartialEq)]
enum Piped {
    Text { text: String, cut: bool },
    // A task was given and nothing arrived in time; the pipe was not read.
    Silent,
    Nothing,
}

impl HeadlessInput {
    /// Reads stdin when it is not a terminal, and exits 2 when neither argv
    /// nor stdin holds a task, before any setup or request.
    fn require(argv: &str) -> Self {
        let have_task = !argv.trim().is_empty();
        let piped = if std::io::stdin().is_terminal() {
            Piped::Nothing
        } else {
            let wait = have_task.then_some(STDIN_FIRST_BYTE_WAIT);
            collect_piped(&spawn_stdin_reader(), wait, MAX_STDIN_BYTES)
        };
        let input = Self::from_parts(argv, piped);
        if !have_task && input.stdin.is_none() {
            eprintln!(
                "buildwithnexus: no task given — pass it as an argument \
                 (buildwithnexus run 'fix the typo') or on stdin (echo 'fix the typo' | buildwithnexus run)"
            );
            std::process::exit(2);
        }
        input
    }

    fn from_parts(argv: &str, piped: Piped) -> Self {
        let mib = MAX_STDIN_BYTES / (1024 * 1024);
        let (stdin, notice) = match piped {
            Piped::Text { text, .. } if text.trim().is_empty() => (None, None),
            Piped::Text { text, cut } => (
                Some(if cut {
                    format!("{text}\n[stdin cut at {mib} MiB]")
                } else {
                    text
                }),
                cut.then(|| {
                    format!("  stdin was longer than {mib} MiB — only the first {mib} MiB was sent")
                }),
            ),
            Piped::Silent => (
                None,
                Some(format!(
                    "  nothing arrived on stdin within {}s, so it was not read — \
                     for slow input, write it to a file and redirect it (< file)",
                    STDIN_FIRST_BYTE_WAIT.as_secs()
                )),
            ),
            Piped::Nothing => (None, None),
        };
        Self {
            argv: argv.to_string(),
            stdin,
            notice,
        }
    }

    /// The task for the model. Argv words go through @path attachments as
    /// typed tasks do; piped text is data (logs, diffs) and is sent as is.
    fn task(&self, p: &Provider, cwd: &std::path::Path) -> (String, Vec<(String, String)>) {
        if let Some(n) = &self.notice {
            report::notice(n);
        }
        let stdin = self.stdin.as_deref();
        if self.argv.trim().is_empty() {
            return (stdin.unwrap_or_default().to_string(), Vec::new());
        }
        // `/deploy staging` runs the deploy command or skill, as in a session.
        if let Some((cmd, args)) = find_slash_command(&self.argv) {
            let prompt = config::command_prompt(&cmd, &args);
            return match stdin {
                Some(s) => (format!("{prompt}\n\n[stdin]\n{s}"), Vec::new()),
                None => (prompt, Vec::new()),
            };
        }
        let (task, images) = headless_attachments(p, &self.argv, cwd);
        match stdin {
            Some(s) => (format!("{task}\n\n[stdin]\n{s}"), images),
            None => (task, images),
        }
    }
}

// Reads stdin on its own thread, a chunk per message; the channel closes at
// end of input. Once the receiver is gone the rest is read and dropped.
fn spawn_stdin_reader() -> std::sync::mpsc::Receiver<Vec<u8>> {
    use std::io::Read;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = tx.send(buf[..n].to_vec());
                }
            }
        }
    });
    rx
}

// Up to `cap` bytes from the reader. `first_wait` bounds the wait for the
// first chunk; without it the wait is unbounded, with a hint on stderr.
fn collect_piped(
    rx: &std::sync::mpsc::Receiver<Vec<u8>>,
    first_wait: Option<std::time::Duration>,
    cap: usize,
) -> Piped {
    use std::sync::mpsc::RecvTimeoutError;
    let first = match first_wait {
        Some(wait) => match rx.recv_timeout(wait) {
            Ok(chunk) => chunk,
            Err(RecvTimeoutError::Timeout) => return Piped::Silent,
            Err(RecvTimeoutError::Disconnected) => return Piped::Nothing,
        },
        None => match rx.recv_timeout(STDIN_FIRST_BYTE_WAIT) {
            Ok(chunk) => chunk,
            Err(RecvTimeoutError::Disconnected) => return Piped::Nothing,
            Err(RecvTimeoutError::Timeout) => {
                eprintln!("buildwithnexus: no task argument — waiting for the task on stdin…");
                match rx.recv() {
                    Ok(chunk) => chunk,
                    Err(_) => return Piped::Nothing,
                }
            }
        },
    };
    let mut buf = first;
    let mut cut = false;
    while buf.len() <= cap {
        match rx.recv() {
            Ok(chunk) => buf.extend_from_slice(&chunk),
            Err(_) => break,
        }
    }
    if buf.len() > cap {
        buf.truncate(cap);
        cut = true;
        // Drop a character split by the cut rather than send U+FFFD.
        while std::str::from_utf8(&buf).is_err_and(|e| e.error_len().is_none()) {
            buf.pop();
        }
    }
    Piped::Text {
        text: String::from_utf8_lossy(&buf).into_owned(),
        cut,
    }
}

#[cfg(test)]
mod headless_input_tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    #[test]
    fn piped_input_is_read_to_the_end_and_cut_at_the_cap() {
        let (tx, rx) = channel();
        tx.send(b"abc".to_vec()).unwrap();
        tx.send(b"def".to_vec()).unwrap();
        drop(tx);
        let got = collect_piped(&rx, Some(Duration::from_secs(1)), 100);
        assert_eq!(
            got,
            Piped::Text {
                text: "abcdef".into(),
                cut: false
            }
        );

        let (tx, rx) = channel();
        tx.send("é".repeat(10).into_bytes()).unwrap();
        // 7 bytes would split the fourth two-byte character: it is dropped.
        let Piped::Text { text, cut } = collect_piped(&rx, None, 7) else {
            panic!("no text");
        };
        assert!(cut);
        assert_eq!(text, "ééé");
    }

    #[test]
    fn a_silent_pipe_is_skipped_only_when_a_task_was_given() {
        let (tx, rx) = channel::<Vec<u8>>();
        assert_eq!(
            collect_piped(&rx, Some(Duration::from_millis(20)), 100),
            Piped::Silent
        );
        drop(tx);
        assert_eq!(collect_piped(&rx, None, 100), Piped::Nothing);
    }

    #[test]
    fn piped_text_follows_the_task_as_a_block_or_is_the_task() {
        let input = HeadlessInput::from_parts(
            "why?",
            Piped::Text {
                text: "log line".into(),
                cut: true,
            },
        );
        assert_eq!(
            input.stdin.as_deref(),
            Some("log line\n[stdin cut at 1 MiB]")
        );
        assert!(input.notice.unwrap().contains("1 MiB"));
        let blank = HeadlessInput::from_parts(
            "",
            Piped::Text {
                text: " \n".into(),
                cut: false,
            },
        );
        assert!(blank.stdin.is_none() && blank.notice.is_none());
        let silent = HeadlessInput::from_parts("why?", Piped::Silent);
        assert!(silent.stdin.is_none());
        assert!(silent.notice.unwrap().contains("not read"));
    }
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
    exit_on_interrupt();
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
    // MCP tools must be on the surface before the first request. A server
    // that is not ready within a few seconds (or its own timeout_secs) is
    // skipped for this run, with a notice, rather than stalling it.
    let skipped = mcp::ensure_ready_headless();
    report_mcp_notices();
    for n in skipped {
        report::notice(&format!("  {n}"));
    }

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
    // Refusals by a hook, a rule or read-only mode: the run did not do what
    // it was asked, though nothing failed.
    let denied = (blocked == 0)
        .then(|| report::denials_line(&report::denials()))
        .flatten();

    let outcome = match &r {
        Err(_) if blocked > 0 => agent::Outcome::ApprovalBlocked,
        Err(_) => agent::Outcome::Failed,
        Ok(()) if denied.is_some() => agent::Outcome::ApprovalBlocked,
        Ok(()) => agent::stopped_short_outcome().unwrap_or(agent::Outcome::Success),
    };
    let legacy = opts.legacy_exit_codes
        || std::env::var("BWN_LEGACY_EXIT_CODES").is_ok_and(|v| !v.is_empty() && v != "0");
    let mut code = headless_exit_code(outcome, r.is_ok(), legacy);
    let mut outcome_name = outcome.as_str();
    let blocking = REVIEW_BLOCKING.load(std::sync::atomic::Ordering::Relaxed);
    if outcome == agent::Outcome::Success && blocking > 0 {
        code = EXIT_REVIEW_BLOCKING;
        outcome_name = "review_blocking";
    }

    if !report::is_json() {
        println!();
        if code == EXIT_REVIEW_BLOCKING {
            println!(
                "{}",
                tui::yellow(&format!(
                    "⚠ review found {blocking} blocking issue{} after {elapsed:.2?}",
                    if blocking == 1 { "" } else { "s" }
                ))
            );
        } else if outcome == agent::Outcome::Success {
            println!("{}", tui::green(&format!("✓ done in {elapsed:.2?}")));
        } else if let (Ok(()), Some(line)) = (&r, &denied) {
            println!(
                "{}",
                tui::yellow(&format!(
                    "⚠ {} after {elapsed:.2?}",
                    tui::sanitize_terminal(line)
                ))
            );
        } else if r.is_ok() {
            println!(
                "{}",
                tui::yellow(&format!("⚠ {} after {elapsed:.2?}", outcome.label()))
            );
        } else {
            println!("{}", tui::red(&format!("✗ failed after {elapsed:.2?}")));
        }
    }
    report::result(outcome_name, code);

    if let Err(e) = r {
        eprintln!("{}", tui::red(&tui::sanitize_terminal(&e)));
    } else if let (true, Some(line)) = (report::is_json(), &denied) {
        eprintln!("{}", tui::yellow(&tui::sanitize_terminal(line)));
    }
    if code != 0 {
        print_session_worktree_hint();
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

// SIGINT or SIGTERM during a headless run: end with 128 + the signal, a
// final result event (outcome "interrupted") and a line naming the saved
// session. The handler only records the signal; a watcher thread does the
// rest outside signal context.
static INTERRUPT_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

fn exit_on_interrupt() {
    if !install_interrupt_handlers() {
        return;
    }
    std::thread::spawn(|| loop {
        let sig = INTERRUPT_SIGNAL.load(std::sync::atomic::Ordering::Relaxed);
        if sig != 0 {
            let code = 128 + sig;
            report::result("interrupted", code);
            let resume = crate::session::current()
                .map(|id| {
                    format!(
                        " — session {id} saved; resume with `buildwithnexus resume {id} <task>`"
                    )
                })
                .unwrap_or_default();
            eprintln!(
                "{}",
                tui::yellow(&format!("buildwithnexus: interrupted{resume}"))
            );
            print_session_worktree_hint();
            std::process::exit(code);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    });
}

#[cfg(unix)]
fn install_interrupt_handlers() -> bool {
    extern "C" fn on_signal(sig: libc::c_int) {
        INTERRUPT_SIGNAL.store(sig, std::sync::atomic::Ordering::Relaxed);
    }
    let h: extern "C" fn(libc::c_int) = on_signal;
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, h as usize);
        libc::signal(libc::SIGTERM, h as usize);
    }
    true
}

// Ctrl+C and Ctrl+Break in a console: handled (TRUE), so the watcher thread
// can report and exit with 130. Closing the window keeps the default.
#[cfg(windows)]
fn install_interrupt_handlers() -> bool {
    type HandlerRoutine = unsafe extern "system" fn(u32) -> i32;
    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<HandlerRoutine>, add: i32) -> i32;
    }
    unsafe extern "system" fn on_ctrl(kind: u32) -> i32 {
        // CTRL_C_EVENT = 0, CTRL_BREAK_EVENT = 1.
        if kind <= 1 {
            INTERRUPT_SIGNAL.store(2, std::sync::atomic::Ordering::Relaxed);
            1
        } else {
            0
        }
    }
    // SAFETY: registers a handler that only stores to an atomic.
    unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) != 0 }
}

#[cfg(not(any(unix, windows)))]
fn install_interrupt_handlers() -> bool {
    false
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
                None if confirm_quit() => return Ok(()),
                None => continue,
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
            let plural = if n_images == 1 { "" } else { "s" };
            tui::line(&tui::dim(&format!("  ⎘ attached {n_images} image{plural}")));
        }

        tui::line("");
        rewind_points.push(RewindPoint {
            index: transcript.len(),
            started_ms: checkpoint::now_ms(),
            prompt: t.to_string(),
        });
        // Every mode reads and extends the one conversation, saved as the
        // session after each turn.
        let r = if conversational {
            agent::run_chat_turn(
                &provider,
                perm,
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
fn follow_reported_window(provider: &mut Provider, transcript: &[provider::Msg]) {
    if let Some(Some(n)) = provider.ollama_ctx.get() {
        let n = *n as usize;
        if n != provider.context_tokens {
            provider.context_tokens = n;
            tui::context_meter(context_in_use(transcript, n), n);
        }
    }
}

fn mode_label(mode: &Mode) -> &'static str {
    match mode {
        Mode::Plan => "PLAN",
        Mode::Build => "BUILD",
        Mode::Brainstorm => "BRAINSTORM",
    }
}

// The mode after a PLAN turn: BUILD only when the person chose to execute
// the plan; Cancel, Esc and a plain answer stay in PLAN.
fn mode_after_plan(end: agent::PlanEnd) -> Mode {
    match end {
        agent::PlanEnd::Executed => Mode::Build,
        agent::PlanEnd::Cancelled | agent::PlanEnd::Answered => Mode::Plan,
    }
}

// A hint when the task phrasing suggests another mode. Only a hint: the
// mode itself changes only when the person changes it.
fn mode_hint(task: &str, current: &Mode) -> Option<(&'static str, String)> {
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
fn suggest_mode_if_mismatch(task: &str, current: &Mode, last_suggested: &mut Option<&'static str>) {
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

// Where a session ran, as a list shows it: "this folder", or the last two
// parts of its path.
fn session_folder(s: &session::Session, cwd: &std::path::Path) -> String {
    if s.is_in(cwd) {
        return "this folder".to_string();
    }
    let parts: Vec<&str> = s.cwd.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    match parts.len() {
        0 => s.cwd.clone(),
        1 => parts[0].to_string(),
        n => format!("…/{}/{}", parts[n - 2], parts[n - 1]),
    }
}

fn session_row(n: usize, s: &session::Session, cwd: &std::path::Path) -> String {
    // Titles come from task text and cwd from the checkout's folder name.
    let label: String = tui::sanitize_terminal(s.label()).chars().take(56).collect();
    format!(
        "  {:>3}  {:<9} {:>4} msgs  {}  {}",
        n,
        session::ago(s.updated_ms),
        s.msgs.len(),
        label,
        tui::dim(&tui::sanitize_terminal(&session_folder(s, cwd)))
    )
}

// A sessions list filtered by what was typed: every word must appear in the
// label or the folder.
fn filter_sessions<'a>(all: &'a [session::Session], filter: &str) -> Vec<&'a session::Session> {
    let words: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
    all.iter()
        .filter(|s| {
            let hay = format!("{} {}", s.label(), s.cwd).to_lowercase();
            words.iter().all(|w| hay.contains(w))
        })
        .collect()
}

// What a /resume answer means: a pick, a new filter, or cancel.
#[derive(Debug, PartialEq)]
enum ResumePick {
    Cancel,
    Pick(usize),
    Missing(usize),
    Filter(String),
}

fn resume_pick(answer: Option<&str>, shown: usize) -> ResumePick {
    let a = answer.map(str::trim).unwrap_or("");
    if a.is_empty() {
        return ResumePick::Cancel;
    }
    match a.parse::<usize>() {
        Ok(n) if n >= 1 && n <= shown => ResumePick::Pick(n - 1),
        Ok(n) => ResumePick::Missing(n),
        Err(_) => ResumePick::Filter(a.to_string()),
    }
}

const RESUME_ROWS: usize = 15;

// /resume: sessions from this folder first, with age, message count and
// folder; a number picks, text filters, Enter cancels.
fn handle_resume(transcript: &mut Vec<provider::Msg>, sid: &mut String, cwd: &std::path::Path) {
    let all = session::list_here_first(cwd);
    if all.is_empty() {
        tui::line(&tui::dim("  no saved sessions yet"));
        return;
    }
    let mut filter = String::new();
    loop {
        let shown = filter_sessions(&all, &filter);
        if shown.is_empty() {
            tui::line(&tui::yellow(&format!(
                "  no session matches '{}'",
                tui::sanitize_terminal(&filter)
            )));
        } else {
            let heading = if filter.is_empty() {
                "  sessions (this folder first):".to_string()
            } else {
                format!("  sessions matching '{}':", tui::sanitize_terminal(&filter))
            };
            tui::line(&tui::dim(&heading));
            for (i, s) in shown.iter().take(RESUME_ROWS).enumerate() {
                tui::line(&session_row(i + 1, s, cwd));
            }
            if shown.len() > RESUME_ROWS {
                tui::line(&tui::dim(&format!(
                    "  … {} more — type words to filter",
                    shown.len() - RESUME_ROWS
                )));
            }
        }
        let answer = tui::ask(&tui::dim("  resume # · text to filter · Enter to cancel: "));
        match resume_pick(answer.as_deref(), shown.len().min(RESUME_ROWS)) {
            ResumePick::Cancel => return,
            ResumePick::Missing(n) => {
                tui::line(&tui::yellow(&format!("  no session {n}")));
            }
            ResumePick::Filter(f) => filter = f,
            ResumePick::Pick(i) => {
                let Some(picked) = session::load(&shown[i].id) else {
                    tui::line(&tui::yellow("  that session file can no longer be read"));
                    return;
                };
                show_resumed(&picked, cwd);
                *transcript = picked.msgs;
                *sid = picked.id;
                return;
            }
        }
    }
}

// The confirmation and history replay for a session opened by /resume,
// `bwn continue` or `bwn resume <id>`.
fn show_resumed(s: &session::Session, cwd: &std::path::Path) {
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
fn handle_export(
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
fn last_answer(transcript: &[provider::Msg]) -> Option<&str> {
    transcript.iter().rev().find_map(|m| match m {
        provider::Msg::Assistant { text, .. } if !text.trim().is_empty() => Some(text.trim()),
        _ => None,
    })
}

// The OSC 52 sequence that puts `text` on the terminal's clipboard.
fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", media::b64_encode(text.as_bytes()))
}

// /copy: the last answer to the clipboard through the terminal (OSC 52),
// which also works over SSH; terminals that do not support it ignore it.
fn handle_copy(transcript: &[provider::Msg]) {
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
fn handle_ask(provider: &Provider, cwd: &std::path::Path, transcript: &[provider::Msg], q: &str) {
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

fn handle_rename(sid: &str, name: &str) {
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
fn sessions_command(args: &[String]) {
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
            s.msgs.len(),
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
fn open_session(opts: CliOptions, s: session::Session, task: String, verb: &str) {
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
fn continue_command(opts: CliOptions, task: String) {
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
                        tui::sanitize_terminal(&session_folder(&s, &cwd))
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
fn resume_command(opts: CliOptions, args: &[String]) {
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

/// `/name args` naming a command or skill: that command and its arguments.
/// A task that merely starts with a path (`/usr/bin/foo fails`) is not one.
fn find_slash_command(text: &str) -> Option<(config::CustomCommand, String)> {
    let rest = text.trim().strip_prefix('/')?;
    let mut words = rest.splitn(2, char::is_whitespace);
    let name = words.next().filter(|n| !n.is_empty() && !n.contains('/'))?;
    let args = words.next().unwrap_or("").trim().to_string();
    find_custom_command(name).map(|c| (c, args))
}

// A script command runs like any run_command: PreToolUse hooks, then the
// permission gate. Its output on success, the refusal or output otherwise.
fn run_script_command(
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
    if let Some(reason) = agent::hook_gate(perm, "run_command", &tool_input, cwd) {
        return Err(reason);
    }
    let out = tools::run("run_command", &tool_input, cwd);
    if out.is_error {
        Err(out.content)
    } else {
        Ok(out.content)
    }
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
/// Studio on another port), then the addresses /model remembers for local
/// presets, then each local preset at its default address.
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

fn handle_local(provider: &mut Provider) {
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

// ── review ───────────────────────────────────────────────────────────────────

/// `bwn review` exits with this when a finding is blocking.
const EXIT_REVIEW_BLOCKING: i32 = 9;
// Blocking findings of the headless review in this process.
static REVIEW_BLOCKING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
// The diff a review sends is cut here; the model can read files for more.
const MAX_REVIEW_DIFF_BYTES: usize = 200 * 1024;

/// What to review: `--base <ref>` (the branch since it forked, plus
/// uncommitted changes), `--staged`, or by default everything not yet
/// committed; any other words are the focus.
#[derive(Debug, Default, PartialEq)]
struct ReviewRequest {
    base: Option<String>,
    staged: bool,
    focus: String,
}

impl ReviewRequest {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut req = ReviewRequest::default();
        let mut focus = Vec::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let (flag, inline) = a
                .split_once('=')
                .map_or((a.as_str(), None), |(k, v)| (k, Some(v)));
            match flag {
                "--base" => {
                    let v = inline
                        .map(str::to_string)
                        .or_else(|| it.next().cloned())
                        .filter(|v| !v.trim().is_empty() && !v.starts_with('-'))
                        .ok_or("--base needs a git ref (e.g. --base origin/main)")?;
                    req.base = Some(v);
                }
                "--staged" => req.staged = true,
                f if looks_like_option(f) => {
                    return Err(unknown_option_among(f, &["--base", "--staged"]))
                }
                _ => focus.push(a.clone()),
            }
        }
        if req.staged && req.base.is_some() {
            return Err("give --base or --staged, not both".into());
        }
        req.focus = focus.join(" ");
        Ok(req)
    }

    // What the review covers, in words and as git diff arguments.
    fn diffs(&self) -> Vec<(String, Vec<String>)> {
        match (&self.base, self.staged) {
            (Some(b), _) => vec![
                (format!("changes since {b}"), vec![format!("{b}...HEAD")]),
                ("uncommitted changes".into(), vec!["HEAD".into()]),
            ],
            (None, true) => vec![("staged changes".into(), vec!["--staged".into()])],
            (None, false) => vec![("uncommitted changes".into(), vec!["HEAD".into()])],
        }
    }
}

// The diff text for `req`, each part headed; empty when nothing changed.
fn review_diff(req: &ReviewRequest, cwd: &std::path::Path) -> Result<String, String> {
    let mut out = String::new();
    for (what, args) in req.diffs() {
        let mut cmd = vec!["--no-pager", "diff", "--no-color", "--no-ext-diff"];
        cmd.extend(args.iter().map(String::as_str));
        let text = git_in(cwd, &cmd).map_err(|e| format!("git diff failed: {e}"))?;
        if !text.trim().is_empty() {
            out.push_str(&format!("### {what}\n```diff\n{text}\n```\n"));
        }
    }
    // A file git does not track yet is not yet committed either.
    if !req.staged {
        out.push_str(&untracked_diff(cwd));
    }
    if out.len() > MAX_REVIEW_DIFF_BYTES {
        let mut cut = MAX_REVIEW_DIFF_BYTES;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str("\n…(diff cut at 200 KiB — read the files for the rest)\n");
    }
    Ok(out)
}

// Files git neither tracks nor ignores, each as a new-file diff. Key and
// credential files are named but not sent, as the file tools hide them;
// links and binary files are named only.
fn untracked_diff(cwd: &std::path::Path) -> String {
    use std::io::Read;
    let Ok(list) = git_in(cwd, &["ls-files", "--others", "--exclude-standard", "-z"]) else {
        return String::new();
    };
    let mut body = String::new();
    let mut left_out = Vec::new();
    for rel in list.split('\0').filter(|p| !p.is_empty()) {
        if body.len() > MAX_REVIEW_DIFF_BYTES {
            break;
        }
        let path = cwd.join(rel);
        if tools::is_sensitive(&path) {
            left_out.push(rel);
            continue;
        }
        let regular = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file());
        let mut bytes = Vec::new();
        let read = regular
            && std::fs::File::open(&path)
                .and_then(|f| f.take(MAX_REVIEW_DIFF_BYTES as u64).read_to_end(&mut bytes))
                .is_ok();
        body.push_str(&format!("--- /dev/null\n+++ b/{rel}\n"));
        match std::str::from_utf8(&bytes) {
            Ok(text) if read && !bytes.contains(&0) => {
                for l in text.lines() {
                    body.push('+');
                    body.push_str(l);
                    body.push('\n');
                }
            }
            _ => body.push_str("(not text: a link, a folder or a binary file)\n"),
        }
    }
    let mut out = String::new();
    if !body.is_empty() {
        out.push_str(&format!(
            "### new files, not yet tracked\n```diff\n{body}```\n"
        ));
    }
    if !left_out.is_empty() {
        out.push_str(&format!(
            "(new files left out because they may hold keys or credentials: {})\n",
            left_out.join(", ")
        ));
    }
    out
}

fn review_task(req: &ReviewRequest, diff: &str) -> String {
    let focus = if req.focus.is_empty() {
        String::new()
    } else {
        format!("Focus on: {}.\n", req.focus)
    };
    format!(
        "Review this change as a careful senior reviewer. Read the files around it when \
         the diff alone is not enough. You cannot edit anything.\n{focus}\n\
         End with the findings, one per line, exactly in this form:\n\
         - [blocking] path/to/file.rs:42 — what is wrong and why it matters\n\
         Severities: blocking (a bug, security hole or data loss that must be fixed \
         before merging), major, minor, nit. Leave out the location when there is none. \
         If there is nothing to report, say: No findings.\n\n{diff}"
    )
}

#[derive(Debug, PartialEq)]
struct Finding {
    severity: String,
    path: Option<String>,
    line: Option<u64>,
    message: String,
}

// `- [blocking] src/a.rs:42 — message` lines of a review answer.
fn parse_findings(text: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let l = raw.trim().trim_start_matches(['-', '*', '•']).trim_start();
        let Some(rest) = l.strip_prefix('[') else {
            continue;
        };
        let Some((sev, body)) = rest.split_once(']') else {
            continue;
        };
        let severity = sev.trim().to_ascii_lowercase();
        if !matches!(severity.as_str(), "blocking" | "major" | "minor" | "nit") {
            continue;
        }
        let body = body.trim();
        let (loc, message) = match body.split_once(" — ").or_else(|| body.split_once(" - ")) {
            Some((loc, msg)) if !loc.contains(' ') => (Some(loc.trim_matches('`')), msg.trim()),
            _ => (None, body),
        };
        let (path, line) = match loc.and_then(|l| l.rsplit_once(':')) {
            Some((p, n)) if n.parse::<u64>().is_ok() => (Some(p.to_string()), n.parse().ok()),
            _ => (loc.map(str::to_string), None),
        };
        out.push(Finding {
            severity,
            path,
            line,
            message: message.to_string(),
        });
    }
    out
}

// `bwn review`: one read-only review turn over the diff; a `finding`
// event per issue, and exit 9 when one is blocking.
fn headless_review(p: &Provider, req: &ReviewRequest, cwd: &std::path::Path) -> Result<(), String> {
    let diff = review_diff(req, cwd)?;
    if diff.trim().is_empty() {
        report::notice("  nothing to review: no changes");
        return Ok(());
    }
    let mut transcript = Vec::new();
    let sid = session::claim_or_new();
    let text = agent::run_review(p, &review_task(req, &diff), cwd, &mut transcript, &sid)?;
    let findings = parse_findings(&text);
    for f in &findings {
        report::finding(&f.severity, f.path.as_deref(), f.line, &f.message);
    }
    let blocking = findings.iter().filter(|f| f.severity == "blocking").count();
    REVIEW_BLOCKING.store(blocking, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

// `/review [--base <ref>|--staged] [focus]`: the same read-only review in
// the session's conversation.
fn handle_review(
    provider: &Provider,
    args: &str,
    cwd: &std::path::Path,
    transcript: &mut Vec<provider::Msg>,
    sid: &str,
) {
    let words = shlex::split(args).unwrap_or_default();
    let req = match ReviewRequest::parse(&words) {
        Ok(r) => r,
        Err(e) => {
            tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
            tui::line(&tui::dim(
                "  usage: /review [--base <ref> | --staged] [focus]",
            ));
            return;
        }
    };
    let diff = match review_diff(&req, cwd) {
        Ok(d) if d.trim().is_empty() => {
            tui::line(&tui::dim("  nothing to review: no changes"));
            return;
        }
        Ok(d) => d,
        Err(e) => {
            tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
            return;
        }
    };
    let what: Vec<String> = req.diffs().into_iter().map(|(w, _)| w).collect();
    tui::line(&tui::accent(&format!(
        "  /review — {} (read-only)",
        what.join(" and ")
    )));
    tui::line("");
    if let Err(e) = agent::run_review(provider, &review_task(&req, &diff), cwd, transcript, sid) {
        tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
    }
    tui::bell();
}

#[cfg(test)]
mod review_tests {
    use super::*;

    #[test]
    fn review_targets_parse() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            ReviewRequest::parse(&a(&["--base", "origin/main", "auth", "paths"])).unwrap(),
            ReviewRequest {
                base: Some("origin/main".into()),
                staged: false,
                focus: "auth paths".into()
            }
        );
        assert!(ReviewRequest::parse(&a(&["--base=main"]))
            .unwrap()
            .base
            .is_some());
        assert!(ReviewRequest::parse(&a(&["--base"])).is_err());
        assert!(ReviewRequest::parse(&a(&["--staged", "--base", "x"])).is_err());
        assert!(ReviewRequest::parse(&a(&["--bsae", "x"]))
            .unwrap_err()
            .contains("did you mean --base"));
    }

    #[test]
    fn findings_are_read_from_the_answer() {
        let text = "Looks fine overall.\n\
                    - [blocking] src/auth.rs:42 — token compared with ==, timing leak\n\
                    * [nit] README.md — typo in the title\n\
                    - [minor] no tests for the new flag\n\
                    - [later] not a severity\n";
        let f = parse_findings(text);
        assert_eq!(f.len(), 3);
        assert_eq!(f[0].severity, "blocking");
        assert_eq!(f[0].path.as_deref(), Some("src/auth.rs"));
        assert_eq!(f[0].line, Some(42));
        assert_eq!(f[1].path.as_deref(), Some("README.md"));
        assert_eq!(f[1].line, None);
        assert_eq!(f[2].path, None);
        assert_eq!(f[2].message, "no tests for the new flag");
        assert!(parse_findings("No findings.").is_empty());
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

// Git for bwn's own commands (/commit, /diff), never for the model. A
// repository's config can name programs git runs (fsmonitor, an external
// diff, textconv, filters), so unless every repository-level key is known
// to be inert the person is asked first.
fn git_may_run(cwd: &std::path::Path, ask: &mut dyn FnMut(&str) -> Option<String>) -> bool {
    if tools::skips_prompt_safely("git status", cwd) {
        return true;
    }
    let a = ask(
        "  this repository's git config can run programs (hooks, filters, an external diff) — run git here anyway? [y/N]: ",
    );
    matches!(a.as_deref().map(str::trim), Some("y" | "Y" | "yes" | "YES"))
}

fn git_text(cwd: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let o = std::process::Command::new("git")
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !o.status.success() {
        return Err(String::from_utf8_lossy(&o.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

// `/commit`: the model drafts the message, bwn shows it, and nothing is
// committed until the person answers c.
fn handle_commit(provider: &Provider, cwd: &std::path::Path) {
    commit_flow(
        cwd,
        |stat, diff| agent::draft_commit_message(provider, stat, diff),
        &mut |q| tui::ask(q),
    );
}

// Returns the new commit's one-line summary when a commit was made.
fn commit_flow(
    cwd: &std::path::Path,
    draft: impl FnOnce(&str, &str) -> Result<String, String>,
    ask: &mut dyn FnMut(&str) -> Option<String>,
) -> Option<String> {
    if !git_may_run(cwd, ask) {
        tui::line(&tui::dim("  cancelled — nothing committed"));
        return None;
    }
    let stat = match git_text(cwd, &["diff", "--staged", "--stat", "--no-ext-diff"]) {
        Ok(s) => s,
        Err(e) => {
            tui::line(&tui::red(&format!(
                "  /commit needs a git repository: {}",
                tui::sanitize_terminal(&e)
            )));
            return None;
        }
    };
    if stat.trim().is_empty() {
        tui::line(&tui::yellow(
            "  nothing is staged — `git add <files>` first, then /commit",
        ));
        return None;
    }
    let diff =
        git_text(cwd, &["diff", "--staged", "--no-ext-diff", "--no-textconv"]).unwrap_or_default();
    let mut msg = match draft(&stat, &diff) {
        Ok(m) => m,
        Err(e) => {
            tui::line(&tui::red(&format!(
                "  could not draft a commit message: {}",
                tui::sanitize_terminal(&e)
            )));
            return None;
        }
    };
    loop {
        tui::line(&tui::accent("  proposed commit message:"));
        // The draft is model text.
        for l in tui::sanitize_terminal(&msg).lines() {
            tui::line(&format!("    {l}"));
        }
        let answer = ask("  [c]ommit · [e]dit · [n]o: ");
        match answer.as_deref().map(str::trim) {
            Some("c" | "C" | "commit") => break,
            Some("e" | "E" | "edit") => {
                if let Some(new) = ask("  new message (Enter keeps the proposed one): ") {
                    if !new.trim().is_empty() {
                        msg = new.trim().to_string();
                    }
                }
            }
            _ => {
                tui::line(&tui::dim("  not committed — the changes stay staged"));
                return None;
            }
        }
    }
    match git_commit(cwd, &msg) {
        Ok(summary) => {
            checkpoint::note_commit(cwd);
            tui::line(&tui::green(&format!(
                "  ✓ committed {}",
                tui::sanitize_terminal(&summary)
            )));
            Some(summary)
        }
        Err(e) => {
            tui::line(&tui::red(&format!(
                "  git commit failed: {}",
                tui::sanitize_terminal(&e)
            )));
            None
        }
    }
}

fn git_commit(cwd: &std::path::Path, msg: &str) -> Result<String, String> {
    use std::io::Write;
    let mut child = std::process::Command::new("git")
        .args(["-c", "core.fsmonitor=false", "commit", "-q", "-F", "-"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(msg.as_bytes());
    }
    let o = child
        .wait_with_output()
        .map_err(|e| format!("git commit: {e}"))?;
    if !o.status.success() {
        let mut out = String::from_utf8_lossy(&o.stderr).trim().to_string();
        if out.is_empty() {
            out = String::from_utf8_lossy(&o.stdout).trim().to_string();
        }
        return Err(out);
    }
    Ok(git_text(cwd, &["log", "-1", "--oneline", "--no-decorate"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default())
}

// One changed path in the working tree, as /diff lists it.
#[derive(Debug, PartialEq)]
struct ChangedFile {
    /// Relative to the repository root; a new folder ends with '/'.
    path: String,
    /// git's two status letters ("M ", " M", "??", "A ", " D", "R ").
    status: String,
    added: usize,
    removed: usize,
}

impl ChangedFile {
    fn kind(&self) -> &'static str {
        match self.status.as_str() {
            "??" if self.path.ends_with('/') => "new folder",
            "??" => "new",
            s if s.contains('D') => "deleted",
            s if s.contains('R') => "renamed",
            s if s.contains('A') => "added",
            _ => "modified",
        }
    }
}

// `git status --porcelain=v1 -z` entries as (status, path); a rename's
// source path is skipped.
fn parse_porcelain(z: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut parts = z.split('\0');
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        let (xy, path) = (&entry[..2], &entry[3..]);
        if xy.contains(['R', 'C']) {
            parts.next();
        }
        out.push((xy.to_string(), path.to_string()));
    }
    out
}

// `git diff --numstat -z` (added, removed) by path; binary files count 0.
fn parse_numstat(z: &str) -> std::collections::HashMap<String, (usize, usize)> {
    let mut out = std::collections::HashMap::new();
    for rec in z.split('\0') {
        let mut f = rec.splitn(3, '\t');
        let (Some(a), Some(r), Some(path)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        out.insert(
            path.trim_start_matches('\n').to_string(),
            (a.parse().unwrap_or(0), r.parse().unwrap_or(0)),
        );
    }
    out
}

// Lines in a new file, or in every file under a new folder (bounded).
fn new_lines(path: &std::path::Path) -> usize {
    let count = |p: &std::path::Path| {
        std::fs::read(p)
            .ok()
            .filter(|b| b.len() <= MAX_DIFF_BYTES)
            .map(|b| b.iter().filter(|c| **c == b'\n').count())
            .unwrap_or(0)
    };
    if !path.is_dir() {
        return count(path);
    }
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    let mut seen = 0;
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            seen += 1;
            if seen > 2000 {
                return total;
            }
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                total += count(&p);
            }
        }
    }
    total
}

const MAX_DIFF_BYTES: usize = 512 * 1024;

fn changed_files(cwd: &std::path::Path) -> Result<(PathBuf, Vec<ChangedFile>), String> {
    let top = PathBuf::from(git_text(cwd, &["rev-parse", "--show-toplevel"])?.trim());
    let status = git_text(
        cwd,
        &["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
    )?;
    // Against HEAD when there is one: staged and unstaged changes together.
    let numstat = git_text(
        cwd,
        &[
            "diff",
            "HEAD",
            "--numstat",
            "-z",
            "--no-ext-diff",
            "--no-textconv",
        ],
    )
    .or_else(|_| {
        git_text(
            cwd,
            &["diff", "--cached", "--numstat", "-z", "--no-ext-diff"],
        )
    })
    .unwrap_or_default();
    let counts = parse_numstat(&numstat);
    let files = parse_porcelain(&status)
        .into_iter()
        .map(|(status, path)| {
            let (added, removed) = if status == "??" {
                (new_lines(&top.join(&path)), 0)
            } else {
                counts.get(&path).copied().unwrap_or((0, 0))
            };
            ChangedFile {
                path,
                status,
                added,
                removed,
            }
        })
        .collect();
    Ok((top, files))
}

fn diff_summary(files: &[ChangedFile]) -> String {
    let added: usize = files.iter().map(|f| f.added).sum();
    let removed: usize = files.iter().map(|f| f.removed).sum();
    format!(
        "{} file{} changed, {added} insertion{}(+), {removed} deletion{}(-)",
        files.len(),
        if files.len() == 1 { "" } else { "s" },
        if added == 1 { "" } else { "s" },
        if removed == 1 { "" } else { "s" },
    )
}

// /diff: every changed and new file with its line counts and one summary;
// picking a file shows its diff. `/diff turn` shows what the last agent
// turn changed.
fn handle_diff(cwd: &std::path::Path, arg: &str) {
    let mut ask = |q: &str| tui::ask(q);
    if !git_may_run(cwd, &mut ask) {
        tui::line(&tui::dim("  cancelled"));
        return;
    }
    if arg.trim() == "turn" {
        diff_last_turn(cwd);
        return;
    }
    let (top, files) = match changed_files(cwd) {
        Ok(v) => v,
        Err(e) => {
            tui::line(&tui::red(&format!(
                "  /diff needs a git repository: {}",
                tui::sanitize_terminal(&e)
            )));
            return;
        }
    };
    if files.is_empty() {
        tui::line(&tui::dim("  no changes — the working tree matches HEAD"));
        return;
    }
    // File names come from the checkout.
    for f in &files {
        tui::line(&format!(
            "  {:<10} {}  {}",
            tui::dim(f.kind()),
            tui::sanitize_terminal(&f.path),
            tui::dim(&format!("+{} -{}", f.added, f.removed))
        ));
    }
    tui::line(&tui::dim(&format!("  {}", diff_summary(&files))));
    // The picker draws over the rows above the composer; keep the list in view.
    if tui::is_raw() {
        for _ in 0..files.len() + 2 {
            tui::line("");
        }
    }
    let items: Vec<tui::SelectItem> = files
        .iter()
        .map(|f| tui::SelectItem {
            label: f.path.clone(),
            detail: format!("{} +{} -{}", f.kind(), f.added, f.removed),
        })
        .collect();
    if let Some(i) = tui::select_item("Show the diff of", &items) {
        show_file_diff(&top, &files[i]);
    }
}

fn read_capped(p: &std::path::Path) -> Option<String> {
    std::fs::metadata(p)
        .ok()
        .filter(|m| m.len() as usize <= MAX_DIFF_BYTES)?;
    std::fs::read_to_string(p).ok()
}

fn show_file_diff(top: &std::path::Path, f: &ChangedFile) {
    let path = top.join(&f.path);
    if f.status == "??" && path.is_dir() {
        let mut stack = vec![path];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Some(new) = read_capped(&p) {
                    let shown = p.strip_prefix(top).unwrap_or(&p).display().to_string();
                    report::diff(&shown, "", &new);
                }
            }
        }
        return;
    }
    let old = if f.status == "??" || f.status.contains('A') {
        Some(String::new())
    } else {
        git_text(top, &["show", &format!("HEAD:{}", f.path)])
            .ok()
            .filter(|t| t.len() <= MAX_DIFF_BYTES)
    };
    let new = if f.status.contains('D') {
        Some(String::new())
    } else {
        read_capped(&path)
    };
    match (old, new) {
        (Some(old), Some(new)) => report::diff(&f.path, &old, &new),
        _ => tui::line(&tui::dim(&format!(
            "  {} is too large or not text to show here — git diff -- {}",
            tui::sanitize_terminal(&f.path),
            tui::sanitize_terminal(&f.path)
        ))),
    }
}

// What the last agent turn changed: each checkpointed file against its
// state before the turn, and the files shell commands changed.
fn diff_last_turn(cwd: &std::path::Path) {
    let Some(last) = checkpoint::last_turn(cwd) else {
        tui::line(&tui::dim("  no agent turn recorded in this folder"));
        return;
    };
    tui::line(&tui::dim(&format!(
        "  the last turn, '{}' ({}):",
        tui::sanitize_terminal(&last.turn.task),
        session::ago(last.turn.started_ms)
    )));
    // Oldest checkpoint per file holds its contents before the turn.
    let mut firsts: Vec<&checkpoint::Checkpoint> = Vec::new();
    for cp in last.checkpoints.iter().rev() {
        if !firsts.iter().any(|f| f.path == cp.path) {
            firsts.push(cp);
        }
    }
    for cp in &firsts {
        let shown = checkpoint::shown_path(&cp.path, cwd);
        let before = if cp.existed {
            Some(cp.content.clone())
        } else {
            Some(String::new())
        };
        match (
            before.filter(|_| cp.snapshotted),
            read_capped(&cp.path).or_else(|| (!cp.path.exists()).then(String::new)),
        ) {
            (Some(old), Some(new)) => report::diff(&shown, &old, &new),
            _ => tui::line(&tui::dim(&format!(
                "  {} — no snapshot to compare (too large or not text)",
                tui::sanitize_terminal(&shown)
            ))),
        }
    }
    if !last.turn.untracked.is_empty() {
        tui::line(&tui::dim("  changed by shell commands (against HEAD):"));
        for rel in &last.turn.untracked {
            tui::line(&tui::dim(&format!("    - {}", tui::sanitize_terminal(rel))));
        }
    }
    if firsts.is_empty() && last.turn.untracked.is_empty() {
        tui::line(&tui::dim("  it changed no files"));
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

// Tokens the next request would carry, as /context counts them.
fn context_in_use(transcript: &[provider::Msg], total: usize) -> usize {
    let measured = if transcript.is_empty() {
        None
    } else {
        usage::last_context_tokens()
    };
    measured.unwrap_or_else(|| {
        let tools = tools::defs_for_context(true, total);
        context_breakdown(transcript, &tools).total()
    })
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

const CHECKPOINT_ROWS: usize = 15;

// /checkpoints: newest first, grouped by the task that made them, with age
// and paths relative to this folder.
fn handle_checkpoints(cwd: &std::path::Path) {
    let pruned = checkpoint::prune_once(cwd);
    if pruned > 0 {
        tui::line(&tui::dim(&format!(
            "  pruned {pruned} old checkpoints — this folder keeps the newest {}",
            checkpoint::KEEP_CHECKPOINTS
        )));
    }
    let items = checkpoint::list(cwd);
    if items.is_empty() {
        tui::line(&tui::dim("  no checkpoints for this directory"));
        return;
    }
    tui::line(&tui::dim(
        "  checkpoints in this folder, newest first — /undo <id> restores one, /undo the last task",
    ));
    let mut group: Option<(Option<&str>, String)> = None;
    for cp in items.iter().take(CHECKPOINT_ROWS) {
        let task = cp.task.as_deref();
        let age = session::ago(cp.created_ms);
        if group.as_ref() != Some(&(task, age.clone())) {
            // Task text is the person's prompt; paths are wherever the model wrote.
            tui::line(&format!(
                "  {}  {}",
                tui::bold(&age),
                tui::sanitize_terminal(task.unwrap_or("(task not recorded)"))
            ));
            group = Some((task, age));
        }
        tui::line(&format!(
            "    {}  {:<11} {}",
            tui::dim(&tui::sanitize_terminal(&cp.id)),
            tui::sanitize_terminal(&cp.action),
            tui::sanitize_terminal(&checkpoint::shown_path(&cp.path, cwd))
        ));
    }
    if items.len() > CHECKPOINT_ROWS {
        tui::line(&tui::dim(&format!(
            "  … {} older — this folder keeps the newest {}",
            items.len() - CHECKPOINT_ROWS,
            checkpoint::KEEP_CHECKPOINTS
        )));
    }
}

// A prompt of this run that /rewind can go back to: where its messages
// start in the transcript, when its turn began (checkpoints after it are its
// changes and later ones), and the text sent.
struct RewindPoint {
    index: usize,
    started_ms: u128,
    prompt: String,
}

// Drops the point's prompt and everything after it from the conversation.
// The prompt is found at or after its recorded index (a turn may put the
// system prompt first); a hook may have appended context to it. False when
// it is no longer there (compacted away), leaving the transcript alone.
fn rewind_transcript(transcript: &mut Vec<provider::Msg>, point: &RewindPoint) -> bool {
    let start = point.index.min(transcript.len());
    let found = transcript[start..].iter().position(|m| match m {
        provider::Msg::User(t) | provider::Msg::UserImages { text: t, .. } => {
            t.starts_with(&point.prompt)
        }
        _ => false,
    });
    match found {
        Some(i) => {
            transcript.truncate(start + i);
            // Nothing but a system prompt left: an empty conversation.
            if transcript
                .iter()
                .all(|m| matches!(m, provider::Msg::System(_)))
            {
                transcript.clear();
            }
            true
        }
        None => false,
    }
}

// /rewind: pick an earlier prompt of this run and go back to just before it
// — the files its turn and later turns changed, the conversation from it on,
// or both.
fn handle_rewind(
    transcript: &mut Vec<provider::Msg>,
    points: &mut Vec<RewindPoint>,
    sid: &str,
    cwd: &std::path::Path,
    model: &str,
) {
    if points.is_empty() {
        tui::line(&tui::dim(
            "  nothing to rewind in this session yet — /undo restores files from earlier turns",
        ));
        return;
    }
    let items: Vec<tui::SelectItem> = points
        .iter()
        .rev()
        .map(|p| {
            let first: String = p
                .prompt
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(70)
                .collect();
            let files = checkpoint::list(cwd)
                .iter()
                .filter(|c| c.created_ms >= p.started_ms)
                .count();
            tui::SelectItem {
                label: first,
                detail: format!(
                    "{} · {files} file change{} since",
                    session::ago(p.started_ms),
                    if files == 1 { "" } else { "s" }
                ),
            }
        })
        .collect();
    let Some(pick) = tui::select_item("Rewind to before", &items) else {
        return;
    };
    let at = points.len() - 1 - pick;
    let what = [
        tui::SelectItem {
            label: "Code and conversation".into(),
            detail: "restore the files and drop this prompt and everything after".into(),
        },
        tui::SelectItem {
            label: "Conversation only".into(),
            detail: "drop this prompt and everything after; files stay".into(),
        },
        tui::SelectItem {
            label: "Code only".into(),
            detail: "restore the files changed since; the conversation stays".into(),
        },
    ];
    let Some(choice) = tui::select_item("Rewind what", &what) else {
        return;
    };
    let (code, conversation) = (choice != 1, choice != 2);
    if code {
        let mut overwrite = |p: &std::path::Path| confirm_overwrite(cwd, p);
        match checkpoint::undo_all_since(cwd, points[at].started_ms, &mut overwrite) {
            Ok(undone) => {
                tui::line(&tui::green(&format!(
                    "  ✓ restored {} file{}:",
                    undone.restored.len(),
                    if undone.restored.len() == 1 { "" } else { "s" }
                )));
                show_undone(cwd, &undone);
            }
            Err(e) if e.starts_with("no checkpoints") => {
                tui::line(&tui::dim("  no file changes since then"));
            }
            Err(e) => {
                tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
                return;
            }
        }
    }
    if conversation {
        let before = transcript.len();
        if rewind_transcript(transcript, &points[at]) {
            if transcript.is_empty() {
                let _ = session::remove(sid);
            } else {
                session::save(sid, cwd, model, transcript);
            }
            tui::line(&tui::green(&format!(
                "  ✓ conversation rewound — dropped {} message{}",
                before - transcript.len(),
                if before - transcript.len() == 1 {
                    ""
                } else {
                    "s"
                }
            )));
            let prompt = points[at].prompt.clone();
            points.truncate(at);
            tui::line(&tui::dim(
                "  your prompt was (type its first words and press ↑ to edit it):",
            ));
            for l in tui::sanitize_terminal(&prompt).lines().take(6) {
                tui::line(&format!("    {l}"));
            }
        } else {
            tui::line(&tui::yellow(
                "  that prompt is no longer in the conversation (it was compacted) — files only",
            ));
        }
    }
}

// The question /undo asks before it overwrites a file changed since the
// agent's edit. No (or Enter, Esc) keeps the file as it is.
fn confirm_overwrite(cwd: &std::path::Path, path: &std::path::Path) -> bool {
    let shown = path.strip_prefix(cwd).unwrap_or(path).display().to_string();
    let q = format!(
        "  {} changed after the agent edited it — overwrite your changes? [y/N]: ",
        tui::sanitize_terminal(&shown)
    );
    matches!(
        tui::ask(&q).as_deref().map(str::trim),
        Some("y" | "Y" | "yes" | "YES")
    )
}

fn show_undone(cwd: &std::path::Path, undone: &checkpoint::Undone) {
    for c in &undone.restored {
        tui::line(&format!(
            "    - {} ({})",
            tui::sanitize_terminal(&checkpoint::shown_path(&c.path, cwd)),
            tui::sanitize_terminal(&c.action)
        ));
    }
    for p in &undone.kept {
        let shown = p.strip_prefix(cwd).unwrap_or(p).display().to_string();
        tui::line(&tui::yellow(&format!(
            "    - kept your version of {}",
            tui::sanitize_terminal(&shown)
        )));
    }
}

// Bare /undo: reverts the last agent turn in this folder as a unit — the
// recovery for a partial multi-file edit, where undoing one file would
// quietly leave the rest changed — and says what it cannot undo: commits,
// files changed by shell commands, a turn from an earlier run (asked first).
fn undo_last_turn(cwd: &std::path::Path, overwrite: &mut dyn FnMut(&std::path::Path) -> bool) {
    let last = checkpoint::last_turn(cwd);
    let committed = checkpoint::committed_since_turn(cwd);
    for note in undo_preamble(last.as_ref(), committed) {
        tui::line(&tui::yellow(note));
    }
    let Some(last) = last else {
        return;
    };
    let shell_note = |lead: &str| {
        tui::line(&tui::yellow(&format!(
            "  {lead} changed files with shell commands, which checkpoints do not track — git diff shows them:"
        )));
        for p in last.turn.untracked.iter().take(8) {
            tui::line(&tui::dim(&format!("    - {}", tui::sanitize_terminal(p))));
        }
        if last.turn.untracked.len() > 8 {
            tui::line(&tui::dim(&format!(
                "    … and {} more",
                last.turn.untracked.len() - 8
            )));
        }
    };
    if last.checkpoints.is_empty() {
        if last.turn.untracked.is_empty() {
            tui::line(&tui::yellow(
                "  the last agent turn made no file changes — use /undo latest, /undo <id>, or /undo all",
            ));
        } else {
            shell_note("that turn");
        }
        return;
    }
    let files: std::collections::HashSet<&std::path::Path> =
        last.checkpoints.iter().map(|c| c.path.as_path()).collect();
    if !last.this_session {
        // A turn from an earlier run: say which before touching anything.
        let q = format!(
            "  undo the last task in this folder, '{}' ({}, {} file{})? [y/N]: ",
            tui::sanitize_terminal(&last.turn.task),
            session::ago(last.turn.started_ms),
            files.len(),
            if files.len() == 1 { "" } else { "s" }
        );
        let go = tui::ask(&q).unwrap_or_default();
        if !matches!(go.trim(), "y" | "Y" | "yes" | "YES") {
            tui::line(&tui::dim("  cancelled — nothing restored."));
            return;
        }
    }
    match checkpoint::undo_last_turn(cwd, overwrite) {
        Ok(undone) => {
            let n = undone.restored.len();
            tui::line(&tui::green(&format!(
                "  ✓ undid the last agent turn — restored {n} file{}:",
                if n == 1 { "" } else { "s" }
            )));
            show_undone(cwd, &undone);
            if !last.turn.untracked.is_empty() {
                shell_note("that turn also");
            }
        }
        Err(e) => tui::line(&tui::yellow(&format!("  {}", tui::sanitize_terminal(&e)))),
    }
}

// What bare /undo says before restoring anything: that a commit (by /commit,
// or HEAD moved since the turn) is not undone, and that there is no turn to
// undo — a /commit with no agent turn before it still gets the first.
fn undo_preamble(last: Option<&checkpoint::LastTurn>, committed: bool) -> Vec<&'static str> {
    let mut notes = Vec::new();
    if committed || last.is_some_and(|l| l.head_moved) {
        notes.push("  commits are not undone by /undo — git reset --soft HEAD~1 keeps the changes");
    }
    if last.is_none() {
        notes.push(
            "  no agent turn recorded in this folder — /checkpoints lists what can be restored",
        );
    }
    notes
}

fn handle_undo(cwd: &std::path::Path, arg: &str) {
    let arg = arg.trim();
    let mut overwrite = |p: &std::path::Path| confirm_overwrite(cwd, p);
    if arg.is_empty() {
        undo_last_turn(cwd, &mut overwrite);
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
        match checkpoint::undo_all_since(cwd, since, &mut overwrite) {
            Ok(undone) => {
                tui::line(&tui::green(&format!(
                    "  ✓ restored {} files across session:",
                    undone.restored.len()
                )));
                show_undone(cwd, &undone);
            }
            Err(e) => tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e)))),
        }
    } else if arg == "latest" {
        match checkpoint::undo_latest(cwd, &mut overwrite) {
            Ok(cp) => tui::line(&tui::green(&format!(
                "  ✓ restored latest {}",
                tui::sanitize_terminal(&cp.path.display().to_string())
            ))),
            Err(e) => tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e)))),
        }
    } else {
        match checkpoint::undo_by_id(cwd, arg, &mut overwrite) {
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

// What delegation really is: the model hands a subtask to a helper with the
// task tool; helpers are the two built-in roles and any agent files.
fn handle_teamwork() {
    tui::line(&tui::accent(
        "  teamwork — helpers the model can delegate to",
    ));
    tui::line(&tui::dim(
        "  In BUILD the model can hand a self-contained subtask to a helper with the task tool \
         (spawn_subagent). The helper gets a fresh context, works, and reports back.",
    ));
    tui::line(&format!(
        "    • {} — the default: edits files and runs commands under your permission",
        tui::bold("engineer")
    ));
    tui::line(&format!(
        "    • {} — reads and investigates, cites paths",
        tui::bold("researcher")
    ));
    tui::line(&format!(
        "    • {} — `isolate: true` runs the helper in a git worktree on its own branch",
        tui::bold("isolation")
    ));
    tui::line(&tui::dim(&format!(
        "  Your own helpers: <name>.md files (name, description, tools in frontmatter; instructions \
         as the body) in {}/agents or ~/.claude/agents, and in a trusted project's \
         .buildwithnexus/agents or .claude/agents. /agents lists them.",
        config::home().display()
    )));
}

// `/agents`: the helpers the model can delegate to, then Agents.md.
fn handle_agents() {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let defs = config::load_agent_defs(&cwd);
    if defs.is_empty() {
        tui::line(&tui::dim(&format!(
            "  no helper agents yet — add <name>.md to {}/agents or .buildwithnexus/agents (see /teamwork)",
            config::home().display()
        )));
    } else {
        tui::line(&tui::accent("  helper agents (task tool roles)"));
        for a in &defs {
            let tools = a
                .tools
                .as_ref()
                .map_or("all tools".to_string(), |t| t.join(", "));
            // Names, descriptions and paths come from files on disk.
            tui::line(&format!(
                "    • {} — {}",
                tui::bold(&tui::sanitize_terminal(&a.name)),
                tui::sanitize_terminal(&a.description)
            ));
            tui::line(&tui::dim(&format!(
                "      tools: {} · {}",
                tui::sanitize_terminal(&tools),
                tui::sanitize_terminal(&a.path.display().to_string())
            )));
        }
    }
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
// One MCP check per configured server: a real connect and handshake each,
// bounded by their timeouts.
fn mcp_checks() -> Vec<DoctorCheck> {
    mcp::ensure_ready();
    let reports = mcp::report();
    if reports.is_empty() {
        return vec![DoctorCheck::note("mcp", "no servers configured")];
    }
    reports
        .into_iter()
        .map(|r| {
            let name = format!("mcp:{}", r.name);
            match r.status {
                mcp::Status::Connected => DoctorCheck::pass(
                    name,
                    format!(
                        "{} · {} tool{}",
                        r.transport,
                        r.tools.len(),
                        if r.tools.len() == 1 { "" } else { "s" }
                    ),
                ),
                mcp::Status::Disabled => DoctorCheck::note(name, "disabled"),
                mcp::Status::Connecting => {
                    DoctorCheck::fail(name, "still connecting after the timeout")
                }
                mcp::Status::Failed(e) | mcp::Status::Invalid(e) => {
                    DoctorCheck::fail(name, e.chars().take(160).collect::<String>())
                }
            }
        })
        .collect()
}

// `/doctor` (and `/debug`): the same checks as `buildwithnexus doctor`,
// probing the model this session is using.
fn handle_doctor_tui(live: &Provider) {
    tui::line(&tui::accent(&format!("  buildwithnexus {VERSION} doctor")));
    let checks = doctor_checks(&CliOptions::default(), Some(live));
    for c in &checks {
        tui::line(&c.line());
    }
    if let Some(summary) = doctor_summary_line(&checks) {
        tui::line(&tui::yellow(&summary));
    }
}

/// How to call a listed command that needs an argument, for when it is
/// typed bare.
fn command_usage(cmd_name: &str) -> Option<&'static str> {
    match cmd_name {
        "btw" => Some("/btw <context>  e.g. /btw also update the tests"),
        _ => None,
    }
}

/// The mode a bare `/plan`, `/build` or `/brainstorm` switches to.
fn bare_mode_command(t: &str) -> Option<Mode> {
    match t {
        "/plan" => Some(Mode::Plan),
        "/build" => Some(Mode::Build),
        "/brainstorm" => Some(Mode::Brainstorm),
        _ => None,
    }
}

/// Whether input that starts with `/` is really a path: its first word has
/// another `/` in it (`/home/me/shot.png what is this`) or names something
/// on disk. Command names never contain a second slash.
fn starts_with_path(t: &str) -> bool {
    let first = t.split_whitespace().next().unwrap_or("");
    first.len() > 1
        && first.starts_with('/')
        && (first[1..].contains('/') || std::path::Path::new(first).exists())
}

/// `/cmd` alone or `/cmd <args>`: the trimmed arguments ("" when bare).
/// None for any other input, including `/cmdmore`.
fn slash_args<'a>(t: &'a str, cmd: &str) -> Option<&'a str> {
    match t.strip_prefix(cmd)? {
        "" => Some(""),
        rest if rest.starts_with(char::is_whitespace) => Some(rest.trim()),
        _ => None,
    }
}

/// `/theme [dark|light|ansi|auto]`: switch the colour theme and save it as
/// the `theme` setting. Bare, it opens a picker.
fn handle_theme(arg: &str) {
    let choice = if arg.is_empty() {
        let names = ["dark", "light", "ansi", "auto"];
        let details = [
            "for dark terminal backgrounds",
            "for light terminal backgrounds",
            "the terminal's own 16 colours",
            "follow the terminal's background colour",
        ];
        let items: Vec<tui::SelectItem> = names
            .iter()
            .zip(details)
            .map(|(n, d)| tui::SelectItem {
                label: n.to_string(),
                detail: d.to_string(),
            })
            .collect();
        let title = format!("Theme (now: {})", tui::theme_name());
        match tui::select_item(&title, &items) {
            Some(i) => names[i].to_string(),
            None => return,
        }
    } else {
        arg.to_ascii_lowercase()
    };
    match tui::set_theme(&choice) {
        Ok(now) => {
            let saved = config::save_user_settings(&[("theme", Some(choice.clone().into()))]);
            let shown = if choice == "auto" {
                format!("auto ({now})")
            } else {
                now.to_string()
            };
            match saved {
                Ok(()) => tui::line(&tui::green(&format!(
                    "  ✓ theme: {shown} — saved; new output uses it"
                ))),
                Err(e) => tui::line(&tui::yellow(&format!(
                    "  theme: {shown} for this session — not saved: {e}"
                ))),
            }
        }
        Err(e) => tui::line(&tui::red(&format!("  {e}"))),
    }
}

/// Before the session closes (Ctrl+D, a second Ctrl+C, /exit): background
/// workflows run only while bwn is open, so name the ones that would wait
/// and ask. True means quit. Input that has gone away quits without asking.
fn confirm_quit() -> bool {
    let waiting = waiting_workflows(&workflow::snapshots());
    if waiting.is_empty() || tui::input_closed() {
        return true;
    }
    let answer = tui::ask(&quit_question(&waiting)).map(|a| a.trim().to_lowercase());
    matches!(answer.as_deref(), Some("y" | "yes"))
}

// The workflows that would not run once bwn closes.
fn waiting_workflows(snaps: &[workflow::WorkflowSnapshot]) -> Vec<usize> {
    snaps
        .iter()
        .filter(|w| matches!(w.status_str.as_str(), "pending" | "running"))
        .map(|w| w.id)
        .collect()
}

fn quit_question(ids: &[usize]) -> String {
    let list: Vec<String> = ids.iter().map(|id| format!("#{id}")).collect();
    let noun = if ids.len() == 1 {
        "workflow"
    } else {
        "workflows"
    };
    format!(
        "  {noun} {} will not run while bwn is closed — quit anyway? [y/N] ",
        list.join(", ")
    )
}

fn print_help() {
    tui::line(&tui::bold(&tui::accent(
        "  buildwithnexus — commands and keys",
    )));
    tui::line(&tui::dim(
        "  type a task or a question; /command runs a command; Shift+Tab changes mode",
    ));
    // (command, args/aliases hint, description) grouped by section. Rendered
    // as an auto-aligned table so alignment can't drift as commands change.
    type Row = (&'static str, &'static str, &'static str);
    let sections: &[(&str, &[Row])] = &[
        (
            "modes",
            &[
                ("Shift+Tab", "", "cycle PLAN → BUILD → BRAINSTORM"),
                ("/plan", "[task]", "switch to PLAN, or plan this task"),
                ("/build", "[task]", "switch to BUILD, or do this task"),
                (
                    "/brainstorm",
                    "[task]",
                    "switch to BRAINSTORM, or talk this through",
                ),
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
                (
                    "/schedule",
                    "<delay> <task>",
                    "run a task later (e.g. 5m cargo test)",
                ),
                (
                    "/loop",
                    "<interval> <task>",
                    "run a task repeatedly (e.g. 30m)",
                ),
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
                (
                    "/trace",
                    "[<id>]",
                    "receipts: tool calls, hooks, skills, subagents",
                ),
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
                ("/theme", "[dark|light|ansi|auto]", "colour theme"),
                ("/mouse", "[on|off]", "wheel scroll + drag-copy (/scroll)"),
                ("/doctor", "(/debug)", "diagnose setup"),
                ("/clear", "", "clear the screen"),
                ("/exit", "(/quit)", "exit"),
            ],
        ),
        (
            "keys",
            &[
                ("Enter", "", "send · end a line with \\ to add another"),
                (
                    "Esc",
                    "",
                    "stop the agent mid-turn · cancel a question or picker",
                ),
                (
                    "Ctrl+C",
                    "",
                    "stop the agent · clear the draft · twice on an empty line: quit",
                ),
                ("Ctrl+D", "", "quit (asks first if workflows are waiting)"),
                ("Ctrl+Q / Ctrl+X", "", "edit / drop the next queued message"),
                ("↑↓  Ctrl+R", "", "history · search history"),
                ("Tab", "", "complete commands and @paths"),
            ],
        ),
        (
            "answering an approval (allow?)",
            &[
                ("y", "", "yes, this once"),
                ("n", "", "no"),
                ("s", "", "allow it for the rest of this session"),
                ("a", "", "always allow it in this project"),
                ("d <reason>", "", "deny and tell the agent why"),
                ("Esc", "", "deny and stop the turn"),
            ],
        ),
    ];

    let cmd_w = sections
        .iter()
        .flat_map(|(_, rows)| rows.iter())
        .map(|(cmd, _, _)| cmd.chars().count())
        .max()
        .unwrap_or(0);

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
        "    ^V paste image/text · ^G $EDITOR · ←→ ^A ^E move · ^W ^U ^K kill · ^Y yank",
    ));
    tui::line(&tui::dim(
        "    PgUp/PgDn scroll · --plain (or TERM=dumb) for line mode without screen control",
    ));
    tui::line("");
}

#[cfg(test)]
mod terminal_ui_tests {
    use super::*;

    #[test]
    fn quitting_names_the_workflows_that_would_wait() {
        assert_eq!(
            quit_question(&[1]),
            "  workflow #1 will not run while bwn is closed — quit anyway? [y/N] "
        );
        assert!(quit_question(&[2, 5]).contains("workflows #2, #5 will not run"));
        // Built from snapshots, not the live queue: other tests schedule
        // workflows in this process while this one runs.
        let snap = |id: usize, status: &str| workflow::WorkflowSnapshot {
            id,
            task: "cargo test".into(),
            kind_str: "once".into(),
            status_str: status.into(),
            iteration: 0,
            elapsed_secs: None,
            output_lines: 0,
            reason: None,
        };
        assert!(waiting_workflows(&[]).is_empty());
        assert_eq!(
            waiting_workflows(&[
                snap(1, "done"),
                snap(2, "pending"),
                snap(3, "running"),
                snap(4, "cancelled"),
            ]),
            [2, 3]
        );
    }

    // The REPL's source, from `fn repl(` to the end of that function.
    fn repl_source() -> &'static str {
        let src = include_str!("lib.rs");
        let start = src.find("\nfn repl(").expect("fn repl");
        let body = &src[start..];
        &body[..body[1..].find("\n}\n").expect("end of repl") + 3]
    }

    // A listed command typed bare does something: the REPL matches it
    // alone (an arm, slash_args), it switches the mode, or it needs an
    // argument and its usage is printed.
    fn handled(cmd: &str) -> bool {
        repl_source().contains(&format!("\"{cmd}\""))
            || bare_mode_command(cmd).is_some()
            || command_usage(&cmd[1..]).is_some()
    }

    #[test]
    fn every_listed_command_and_tip_has_a_handler() {
        for cmd in tui::builtin_slash_commands() {
            assert!(
                handled(cmd),
                "{cmd} is in the command list but no REPL arm handles it"
            );
        }
        for tip in STARTUP_TIPS {
            for word in tip.split_whitespace().filter(|w| w.starts_with('/')) {
                let cmd = word.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
                assert!(
                    handled(cmd) && tui::builtin_slash_commands().contains(&cmd),
                    "tip names {cmd}, which is not a command: {tip}"
                );
            }
        }
        // Bare mode commands switch the mode; with a task they run it.
        assert!(matches!(bare_mode_command("/plan"), Some(Mode::Plan)));
        assert!(matches!(bare_mode_command("/build"), Some(Mode::Build)));
        assert!(matches!(
            bare_mode_command("/brainstorm"),
            Some(Mode::Brainstorm)
        ));
        assert!(bare_mode_command("/plan add tests").is_none());
        assert_eq!(slash_args("/loop", "/loop"), Some(""));
        assert_eq!(
            slash_args("/loop  5m cargo test ", "/loop"),
            Some("5m cargo test")
        );
        assert_eq!(slash_args("/loops", "/loop"), None);
    }

    #[test]
    fn a_message_starting_with_an_absolute_path_is_not_a_command() {
        assert!(starts_with_path(
            "/home/me/shot.png what is in this screenshot"
        ));
        assert!(starts_with_path("/home/me/my\\ shot.png what is this"));
        assert!(starts_with_path("/tmp"));
        assert!(!starts_with_path("/help"));
        assert!(!starts_with_path("/frobnicate now"));
        assert!(!starts_with_path("/"));
    }
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
    let (task, images) = extract_attachments(task, cwd, Vision::of(p));
    if !images.is_empty() {
        let n = images.len();
        eprintln!("⎘ attached {n} image{}", if n == 1 { "" } else { "s" });
    }
    (task, images)
}

/// Whether an image or a video may be attached, and if not, the notice that
/// says who decided (the `vision` setting, the server, or the model name).
enum Vision {
    Yes,
    No(String),
}

impl Vision {
    fn of(p: &Provider) -> Self {
        if media::model_supports_vision(p) {
            Vision::Yes
        } else {
            Vision::No(media::vision_refusal(p))
        }
    }
}

impl From<bool> for Vision {
    fn from(yes: bool) -> Self {
        if yes {
            Vision::Yes
        } else {
            Vision::No("this model does not accept images — image not attached".into())
        }
    }
}

fn extract_attachments(
    task: &str,
    cwd: &std::path::Path,
    vision: impl Into<Vision>,
) -> (String, Vec<(String, String)>) {
    let vision = vision.into();
    let mut images: Vec<(String, String)> = Vec::new();
    let mut text_attachments = Vec::new();
    // Attachment words are replaced where they stand; every other byte of
    // the prompt (quotes, line breaks, tabs, runs of spaces) reaches the
    // model exactly as typed.
    let mut clean = String::with_capacity(task.len());
    let mut at = 0;
    for w in prompt_words(task) {
        // Sentence punctuation after a path ("what is in @shot.png?") is not
        // part of the file name; it stays in the prompt after the marker.
        let word = w.value.trim_end_matches(['?', '!', '.', ',', ';', ':']);
        let after = &w.value[word.len()..];
        let Some(marker) = attach_word(word, cwd, &vision, &mut images, &mut text_attachments)
        else {
            continue;
        };
        // Brackets or quotes around a bare path ("(shot.png)") stay too.
        let (before, behind) = if word.starts_with('@') {
            ("", "")
        } else {
            let core = word.trim_start_matches(ATTACHMENT_WRAP);
            let lead = &word[..word.len() - core.len()];
            let trail = &core[core.trim_end_matches(ATTACHMENT_WRAP).len()..];
            (lead, trail)
        };
        clean.push_str(&task[at..w.start]);
        clean.push_str(before);
        clean.push_str(&marker);
        clean.push_str(behind);
        clean.push_str(after);
        at = w.end;
    }
    clean.push_str(&task[at..]);
    if !text_attachments.is_empty() {
        clean.push_str("\n\n[attached files]\n");
        clean.push_str(&text_attachments.join("\n\n"));
    }
    (clean, images)
}

const ATTACHMENT_WRAP: &[char] = &['\'', '"', ',', ';', '(', ')', '`'];

// One whitespace-separated word of a prompt: its byte range as typed and
// its value with surrounding quotes and `\ ` escapes removed, so a quoted or
// escaped path with spaces ('/tmp/My Shot.png', @"my notes.md",
// My\ Shot.png) is one word. A quote with no closing match is plain text.
struct PromptWord {
    start: usize,
    end: usize,
    value: String,
}

fn prompt_words(text: &str) -> Vec<PromptWord> {
    let mut words = Vec::new();
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        let start = i;
        let mut value = String::new();
        let mut j = i;
        if c == '@' {
            value.push('@');
            j += 1;
        }
        if let Some((close, inner)) = quoted(text, j) {
            value.push_str(&inner);
            j = close + 1;
        }
        while let Some(ch) = text[j..].chars().next() {
            if ch.is_whitespace() {
                break;
            }
            if ch == '\\' && text[j + 1..].starts_with(' ') {
                value.push(' ');
                j += 2;
                continue;
            }
            value.push(ch);
            j += ch.len_utf8();
        }
        words.push(PromptWord {
            start,
            end: j,
            value,
        });
        i = j;
    }
    words
}

// A quoted span starting at `j`: the index of its closing quote and its
// value. Inside double quotes `\\` and `\"` are escapes, as in the tokens
// tui::attachment_token writes; single quotes take everything literally.
fn quoted(text: &str, j: usize) -> Option<(usize, String)> {
    let q = text[j..]
        .chars()
        .next()
        .filter(|q| *q == '"' || *q == '\'')?;
    let mut value = String::new();
    let mut k = j + 1;
    while let Some(ch) = text[k..].chars().next() {
        if ch == q {
            return Some((k, value));
        }
        if q == '"' && ch == '\\' {
            if let Some(next) = text[k + 1..]
                .chars()
                .next()
                .filter(|n| matches!(n, '\\' | '"'))
            {
                value.push(next);
                k += 2;
                continue;
            }
        }
        value.push(ch);
        k += ch.len_utf8();
    }
    None
}

// Attaches what one prompt word names, if anything, and returns the marker
// that replaces it in the prompt. Unreadable or unknown words return None
// and stay as typed.
fn attach_word(
    word: &str,
    cwd: &std::path::Path,
    vision: &Vision,
    images: &mut Vec<(String, String)>,
    text_attachments: &mut Vec<String>,
) -> Option<String> {
    use std::io::Read;
    let image_exts = ["png", "jpg", "jpeg", "gif", "webp"];
    let is_at = word.starts_with('@');
    let clean_word = word.trim_matches(ATTACHMENT_WRAP);
    let ext = clean_word.rsplit('.').next().unwrap_or("").to_lowercase();
    let is_img = image_exts.contains(&ext.as_str());
    let is_video = media::VIDEO_EXTS.contains(&ext.as_str());
    if !is_at && !is_img && !is_video {
        return None;
    }
    let raw_path = if is_at {
        word.strip_prefix('@')?
    } else {
        clean_word
    };
    if raw_path == "diff" || raw_path == "git:diff" {
        if let Ok(o) = std::process::Command::new("git")
            .args(["diff", "HEAD"])
            .current_dir(cwd)
            .output()
        {
            let diff_text = String::from_utf8_lossy(&o.stdout);
            if !diff_text.trim().is_empty() {
                text_attachments.push(format!("[git diff HEAD]\n{}", diff_text));
                return Some("[git diff HEAD]".to_string());
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
                return Some("[git status]".to_string());
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
            return Some(format!("[kb: {}]", kb_query));
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
        return Some("[active rules]".to_string());
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
                return Some(format!("[web: {}]", url));
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
                let snippet: String = sym_text.lines().take(30).collect::<Vec<_>>().join("\n");
                text_attachments.push(format!("[symbol search: {}]\n{}", sym_query, snippet));
                return Some(format!("[symbol: {}]", sym_query));
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
        if let Vision::No(why) = vision {
            tui::line(&tui::yellow(&format!("  ⚠ {why}")));
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
                return Some(format!(
                    "[image: {}]",
                    p.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
        }
    } else if media::VIDEO_EXTS.contains(&ext.as_str()) && p.exists() {
        if let Vision::No(why) = vision {
            let why = why.replace("image not attached", "video not attached");
            tui::line(&tui::yellow(&format!("  ⚠ {why}")));
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
                return Some(format!(
                    "[video: {} — {n} sampled frames attached in order]",
                    p.file_name().unwrap_or_default().to_string_lossy()
                ));
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
        return Some(format!("[file: {}]", p.display()));
    } else if p.is_file() {
        // Never drop an attachment silently: the token stays in the
        // prompt as typed, and the user is told why.
        tui::line(&tui::yellow(&format!(
            "  ⚠ could not attach {} (not readable as UTF-8 text) — leaving `{word}` as typed",
            p.display()
        )));
    }
    None
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
         \x20 ... | buildwithnexus run [task] piped text is the task, or context after it (1 MiB)\n\
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
         \x20 buildwithnexus update [--check] install the latest release (--check: exit 10 if behind)\n\
         \x20 buildwithnexus review [--base <ref>|--staged] [focus]  read-only review (exit 9: blocking)\n\
         \x20 buildwithnexus mcp [list|<name>|add|remove|reload]  manage MCP servers\n\
         \x20 buildwithnexus version | help\n\n\
         OPTIONS:\n\
         \x20 --provider <name>             override the configured provider\n\
         \x20 --model <name>                override the configured model\n\
         \x20 --base-url <url>              model endpoint, e.g. a gateway (--provider custom\n\
         \x20                               reads CUSTOM_API_KEY from the environment)\n\
         \x20 --permission-mode <mode>      ask, auto, or readonly\n\
         \x20 --sandbox <mode>              off, auto, or require (OS sandbox for shell commands)\n\
         \x20 --worktree <name>             work in .bwn/worktrees/<name> on branch bwn/<name>\n\
         \x20 --prompt <text>               initial interactive prompt\n\
         \x20 --effort <level>              reasoning depth: off, low, medium, high\n\
         \x20 --max-budget-usd <n>          stop before the next request once spend exceeds n\n\
         \x20 --json                        structured headless output\n\
         \x20 --yes, -y                     auto-approve the plan and execute (plan)\n\
         \x20 --legacy-exit-codes           exit 0 when a run stops short without failing\n\
         \x20 --                            stop parsing options (run -- <task>)\n\n\
         EXIT CODES (headless; --json ends with a result event naming the outcome):\n\
         \x20 0 success   1 failed   2 usage error   3 changes blocked or denied\n\
         \x20 4 hook blocked the task   5 budget limit   6 step limit\n\
         \x20 7 checks fail   8 verification failed   9 review found blocking issues\n\
         \x20 130/143 interrupted (SIGINT/SIGTERM)\n\n\
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
         \x20 /review [--base <ref>|--staged] [focus]  read-only review of your changes\n\
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

#[derive(Clone, Copy, Debug, PartialEq)]
enum CheckState {
    Pass,
    Warn,
    Fail,
    Note,
}

/// One line of `doctor`: what was checked, how it went, and the detail
/// (with the fix when it failed).
#[derive(Debug)]
struct DoctorCheck {
    name: String,
    state: CheckState,
    detail: String,
}

impl DoctorCheck {
    fn new(name: impl Into<String>, state: CheckState, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            state,
            detail: detail.into(),
        }
    }
    fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(name, CheckState::Pass, detail)
    }
    fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(name, CheckState::Fail, detail)
    }
    fn note(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(name, CheckState::Note, detail)
    }
    // sandbox::doctor_summary and hook lines speak in glyphs.
    fn from_glyph(name: impl Into<String>, glyph: &str, detail: impl Into<String>) -> Self {
        let state = match glyph {
            "✓" => CheckState::Pass,
            "✗" => CheckState::Fail,
            "⚠" => CheckState::Warn,
            _ => CheckState::Note,
        };
        Self::new(name, state, detail)
    }

    fn status(&self) -> &'static str {
        match self.state {
            CheckState::Pass => "ok",
            CheckState::Warn => "warn",
            CheckState::Fail => "fail",
            CheckState::Note => "info",
        }
    }

    // Names and details carry paths, server names and server errors.
    fn line(&self) -> String {
        let glyph = match self.state {
            CheckState::Pass => "✓",
            CheckState::Warn => "⚠",
            CheckState::Fail => "✗",
            CheckState::Note => "·",
        };
        format!(
            "  {glyph} {:<14} {}",
            tui::sanitize_terminal(&self.name),
            tui::sanitize_terminal(&self.detail)
        )
    }
}

// Each configured hook, and a line starting with ⚠ for every problem
// (unknown events and types, untrusted files).
fn hook_doctor_lines() -> Vec<String> {
    hooks::doctor_lines()
}

// `ollama list` names carry a tag; a configured name without one means
// `:latest`.
fn ollama_has_model(installed: &[String], model: &str) -> bool {
    installed
        .iter()
        .any(|m| m == model || (!model.contains(':') && *m == format!("{model}:latest")))
}

// The live check of the model endpoint. Ollama answers for free on
// /api/tags, which also says whether the configured model is there; every
// other server pays one output token. A local setup never reaches a hosted
// address.
fn provider_check(p: &Provider, id: &str) -> DoctorCheck {
    let url = tui::sanitize_terminal(&p.base_url).into_owned();
    if id == "ollama" && p.protocol == config::Protocol::OllamaNative {
        let models = provider::ollama_models(&p.base_url);
        return if models.is_empty() {
            DoctorCheck::fail(
                "provider",
                format!(
                    "no answer or no models at Ollama {url} — is it running (ollama serve) \
                     and is the model pulled (ollama pull {})?",
                    p.model
                ),
            )
        } else if !ollama_has_model(&models, &p.model) {
            DoctorCheck::fail(
                "provider",
                format!(
                    "{} is not installed at Ollama {url} — ollama pull {}",
                    p.model, p.model
                ),
            )
        } else {
            DoctorCheck::pass("provider", format!("Ollama at {url} has {}", p.model))
        };
    }
    match provider::validate(p) {
        Ok(Some(served)) => DoctorCheck::pass(
            "provider",
            format!("{id} at {url} answers as {served} (one-token probe)"),
        ),
        Ok(None) => DoctorCheck::pass(
            "provider",
            format!("{id} at {url} answers as {} (one-token probe)", p.model),
        ),
        // The error can carry the server's response body.
        Err(e) => DoctorCheck::fail(
            "provider",
            format!("{id} at {url}: {}", e.chars().take(200).collect::<String>()),
        ),
    }
}

/// Every doctor check, in order. `live` is the session's provider (/doctor);
/// otherwise the provider is built as a headless run would build it.
fn doctor_checks(opts: &CliOptions, live: Option<&Provider>) -> Vec<DoctorCheck> {
    let mut out = Vec::new();

    let load = config::load_settings_diag();
    for i in &load.issues {
        out.push(DoctorCheck::fail(
            "settings",
            format!("{}: {}", i.source, i.error),
        ));
    }
    let mut settings = match load.settings.clone() {
        Some(s) => {
            out.push(DoctorCheck::pass(
                "settings",
                format!(
                    "provider={} model={} permission={}",
                    s.provider,
                    if s.model.is_empty() {
                        "(default)"
                    } else {
                        &s.model
                    },
                    s.permission
                ),
            ));
            Some(s)
        }
        None if load.any_present => {
            out.push(DoctorCheck::fail(
                "settings",
                "present but unusable — fix the file(s) above",
            ));
            None
        }
        // As a headless run would: --provider, or the first key set.
        None => {
            match unattended_settings(opts.provider.as_deref(), |k| config::load_key(k).is_some()) {
                Some(s) => {
                    out.push(DoctorCheck::note(
                        "settings",
                        format!(
                            "none — runs use {} from flags or the environment",
                            s.provider
                        ),
                    ));
                    Some(s)
                }
                None => {
                    out.push(DoctorCheck::fail(
                        "settings",
                        "not found — run `buildwithnexus init`, or pass --provider",
                    ));
                    None
                }
            }
        }
    };
    if let Some(s) = settings.as_mut() {
        if let Some(p) = &opts.provider {
            s.provider = p.clone();
        }
        if let Some(u) = &opts.base_url {
            s.base_url = Some(u.clone());
        }
        if let Some(m) = &opts.model {
            s.model = m.clone();
        }
    }

    // The key of the provider in use, and no other.
    if let Some(preset) = settings.as_ref().and_then(|s| config::preset(&s.provider)) {
        if preset.id == "custom" {
            let set = config::load_key(config::CUSTOM_KEY).is_some();
            out.push(DoctorCheck::note(
                config::CUSTOM_KEY,
                if set {
                    "set"
                } else {
                    "not set (optional for most servers)"
                },
            ));
        } else if !preset.env_key.is_empty() {
            out.push(match config::load_key(preset.env_key) {
                Some(_) => DoctorCheck::pass(preset.env_key, "set"),
                None => DoctorCheck::fail(
                    preset.env_key,
                    format!(
                        "not set (needed for {}) — export it or run `buildwithnexus init`",
                        preset.label
                    ),
                ),
            });
        }
    }

    if let Some(s) = &settings {
        match live {
            Some(p) => out.push(provider_check(p, &s.provider)),
            None => match build_provider(s) {
                Ok(p) => out.push(provider_check(&p, &s.provider)),
                Err(e) => out.push(DoctorCheck::fail("provider", e)),
            },
        }
    }

    // The probe runs the real backend once, so this reports whether shell
    // commands would actually be confined on this machine.
    if let Some(s) = &settings {
        if let Err(e) = sandbox::configure(&s.sandbox, s.sandbox_network) {
            out.push(DoctorCheck::fail("sandbox", e));
        }
    }
    let (glyph, text) = sandbox::doctor_summary();
    out.push(DoctorCheck::from_glyph("sandbox", glyph, text));

    out.push(match config::load_memory() {
        None => DoctorCheck::note("memory.md", "(empty)"),
        Some(m) => DoctorCheck::pass("memory.md", format!("{} chars", m.len())),
    });
    out.push(DoctorCheck::note(
        "home",
        config::home().display().to_string(),
    ));

    for line in hook_doctor_lines() {
        out.push(match line.strip_prefix('⚠') {
            Some(problem) => DoctorCheck::new("hooks", CheckState::Warn, problem.trim()),
            None => DoctorCheck::pass("hooks", line),
        });
    }

    out.extend(mcp_checks());

    for (bin, label) in [
        ("git", "version control"),
        ("cargo", "Rust build tool"),
        ("node", "Node.js runtime"),
        ("npm", "Node package manager"),
        ("python3", "Python runtime"),
        ("gh", "GitHub CLI (optional)"),
        ("docker", "Docker (optional)"),
        ("rg", "ripgrep (fast search, optional)"),
    ] {
        out.push(if crate::tools::find_on_path(bin).is_some() {
            DoctorCheck::pass(bin, label)
        } else {
            DoctorCheck::note(bin, format!("{label} — not found"))
        });
    }

    if crate::tools::is_wsl() {
        let home = config::home();
        out.push(if crate::tools::is_wsl_windows_mount(&home) {
            DoctorCheck::new(
                "wsl2",
                CheckState::Warn,
                format!(
                    "NEXUS_HOME is on a Windows mount ({}) — set it to a Linux path for faster I/O",
                    home.display()
                ),
            )
        } else {
            DoctorCheck::pass("wsl2", "native Linux filesystem")
        });
    }
    out
}

// "2 checks failed: provider, OPENAI_API_KEY" when anything failed.
fn doctor_summary_line(checks: &[DoctorCheck]) -> Option<String> {
    let failed: Vec<&str> = checks
        .iter()
        .filter(|c| c.state == CheckState::Fail)
        .map(|c| c.name.as_str())
        .collect();
    (!failed.is_empty()).then(|| {
        format!(
            "  {} check{} failed: {}",
            failed.len(),
            if failed.len() == 1 { "" } else { "s" },
            tui::sanitize_terminal(&failed.join(", "))
        )
    })
}

// `buildwithnexus doctor`: exits 1 when a check fails, so it can gate a
// pipeline; `--json doctor` prints one `check` event per line.
fn run_doctor(opts: &CliOptions) {
    if !report::is_json() {
        println!("buildwithnexus {VERSION} — doctor");
        println!();
    }
    let checks = doctor_checks(opts, None);
    let failed = checks.iter().any(|c| c.state == CheckState::Fail);
    if report::is_json() {
        for c in &checks {
            report::event(serde_json::json!({
                "type": "check",
                "name": c.name,
                "status": c.status(),
                "detail": c.detail,
            }));
        }
    } else {
        for c in &checks {
            println!("{}", c.line());
        }
        println!();
        if let Some(summary) = doctor_summary_line(&checks) {
            println!("{summary}");
        }
        // Offering installs needs someone to answer.
        if std::io::stdin().is_terminal() {
            check_and_offer_install_dependencies(true);
        }
    }
    if failed {
        std::process::exit(1);
    }
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
    fn a_task_typed_in_brainstorm_gets_a_hint_never_a_mode_change() {
        // The hint names PLAN; the mode stays where the person put it.
        let (target, hint) = mode_hint("build me a snake game", &Mode::Brainstorm).unwrap();
        assert_eq!(target, "PLAN");
        assert!(hint.contains("this looks like a task — Shift+Tab for PLAN"));
        // A long paste of notes reads as a plan-sized task: still only a hint.
        let notes =
            "we should build a cache layer, add retries and fix the flaky test. ".repeat(30);
        assert!(notes.len() > 2000);
        assert_eq!(mode_hint(&notes, &Mode::Brainstorm).unwrap().0, "PLAN");
        // A build task in PLAN suggests BUILD; matching modes say nothing.
        assert_eq!(
            mode_hint("fix the parser bug", &Mode::Plan).unwrap().0,
            "BUILD"
        );
        assert!(mode_hint("fix the parser bug", &Mode::Build).is_none());
        assert!(mode_hint("what if we used sqlite?", &Mode::Brainstorm).is_none());
    }

    fn git_fixture(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bwn-lib-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(&d)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(d.join("app.py"), "def main():\n    pass\n").unwrap();
        git(&["add", "."]);
        git(&["-c", "commit.gpgsign=false", "commit", "-qm", "init"]);
        d
    }

    fn log_count(d: &std::path::Path) -> usize {
        git_text(d, &["log", "--oneline"]).unwrap().lines().count()
    }

    #[test]
    fn commit_shows_the_draft_and_commits_only_after_c() {
        let d = git_fixture("commit");
        // Nothing staged: no draft is requested.
        let mut drafted = false;
        let r = commit_flow(
            &d,
            |_, _| {
                drafted = true;
                Ok("x".into())
            },
            &mut |_| Some("c".into()),
        );
        assert!(r.is_none() && !drafted);

        std::fs::write(d.join("app.py"), "def greet():\n    pass\n").unwrap();
        git_text(&d, &["add", "app.py"]).unwrap();
        let draft = |stat: &str, diff: &str| {
            assert!(stat.contains("app.py") && diff.contains("+def greet"));
            Ok("feat: add greet helper".to_string())
        };
        let answer = |script: &'static [&'static str]| {
            let mut i = 0;
            move |q: &str| {
                if q.contains("run git here anyway") {
                    return Some("y".to_string());
                }
                i += 1;
                script.get(i - 1).map(|a| a.to_string())
            }
        };
        // n: the draft is shown, git log is unchanged and the change stays staged.
        assert!(commit_flow(&d, draft, &mut answer(&["n"])).is_none());
        assert_eq!(log_count(&d), 1);
        assert!(!git_text(&d, &["diff", "--staged", "--stat"])
            .unwrap()
            .trim()
            .is_empty());
        // e, a new message, then c: bwn commits the edited message.
        let summary = commit_flow(&d, draft, &mut answer(&["e", "feat: greet people", "c"]))
            .expect("committed");
        assert!(summary.ends_with("feat: greet people"), "{summary}");
        assert_eq!(log_count(&d), 2);
        assert!(crate::checkpoint::committed_since_turn(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn undo_after_a_commit_says_commits_are_not_undone() {
        // /commit with no agent turn recorded in this folder.
        assert_eq!(
            undo_preamble(None, true),
            [
                "  commits are not undone by /undo — git reset --soft HEAD~1 keeps the changes",
                "  no agent turn recorded in this folder — /checkpoints lists what can be restored",
            ]
        );
        assert_eq!(
            undo_preamble(None, false),
            ["  no agent turn recorded in this folder — /checkpoints lists what can be restored"]
        );
        // A turn, then a commit made outside bwn (HEAD moved).
        let last = checkpoint::LastTurn {
            turn: checkpoint::Turn::default(),
            checkpoints: Vec::new(),
            this_session: true,
            head_moved: true,
        };
        assert_eq!(undo_preamble(Some(&last), false).len(), 1);
        let last = checkpoint::LastTurn {
            head_moved: false,
            ..last
        };
        assert!(undo_preamble(Some(&last), false).is_empty());
    }

    #[test]
    fn resume_answers_pick_filter_or_say_what_is_missing() {
        assert_eq!(resume_pick(None, 3), ResumePick::Cancel);
        assert_eq!(resume_pick(Some("  "), 3), ResumePick::Cancel);
        assert_eq!(resume_pick(Some("2"), 3), ResumePick::Pick(1));
        assert_eq!(resume_pick(Some("99"), 3), ResumePick::Missing(99));
        assert_eq!(resume_pick(Some("0"), 3), ResumePick::Missing(0));
        assert_eq!(
            resume_pick(Some("parser"), 3),
            ResumePick::Filter("parser".into())
        );
        let mk = |title: &str, cwd: &str| session::Session {
            schema_version: 1,
            id: title.into(),
            title: title.into(),
            cwd: cwd.into(),
            model: "m".into(),
            created_ms: 0,
            updated_ms: 0,
            msgs: vec![],
            name: None,
        };
        let all = vec![
            mk("fix the parser", "/work/api"),
            mk("add a flag", "/work/cli"),
        ];
        let hits = filter_sessions(&all, "PARSER");
        assert_eq!(hits.len(), 1);
        assert_eq!(filter_sessions(&all, "cli flag")[0].title, "add a flag");
        assert_eq!(filter_sessions(&all, "").len(), 2);
        let here = std::path::Path::new("/work/api");
        assert_eq!(session_folder(&all[0], here), "this folder");
        assert_eq!(session_folder(&all[1], here), "…/work/cli");
    }

    #[test]
    fn diff_lists_changed_and_new_files_with_one_summary() {
        let d = git_fixture("diff");
        std::fs::write(d.join("app.py"), "def main():\n    print('hi')\n").unwrap();
        std::fs::create_dir_all(d.join("pkg")).unwrap();
        std::fs::write(d.join("pkg/core.py"), "a = 1\nb = 2\n").unwrap();
        std::fs::create_dir_all(d.join("tests")).unwrap();
        std::fs::write(d.join("tests/test_core.py"), "x\n").unwrap();
        git_text(&d, &["add", "tests/test_core.py"]).unwrap();
        let (_, files) = changed_files(&d).unwrap();
        let listed: Vec<(&str, &str)> = files.iter().map(|f| (f.path.as_str(), f.kind())).collect();
        assert_eq!(
            listed,
            [
                ("app.py", "modified"),
                ("tests/test_core.py", "added"),
                ("pkg/", "new folder")
            ]
        );
        let app = &files[0];
        assert_eq!((app.added, app.removed), (1, 1));
        assert_eq!(files[2].added, 2, "lines in the new folder count");
        assert_eq!(
            diff_summary(&files),
            "3 files changed, 4 insertions(+), 1 deletion(-)"
        );
        // Porcelain with a rename keeps the new path only.
        assert_eq!(
            parse_porcelain("R  new.py\0old.py\0?? x/\0"),
            [
                ("R ".to_string(), "new.py".to_string()),
                ("??".into(), "x/".into())
            ]
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn copy_takes_the_last_answer_and_wraps_it_for_the_clipboard() {
        let t = vec![
            provider::Msg::User("q".into()),
            provider::Msg::Assistant {
                text: "first answer".into(),
                calls: vec![],
            },
            provider::Msg::Assistant {
                text: "  ".into(),
                calls: vec![],
            },
        ];
        assert_eq!(last_answer(&t), Some("first answer"));
        assert_eq!(last_answer(&[]), None);
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn rewind_cuts_the_conversation_just_before_the_prompt() {
        use provider::Msg;
        let answer = |t: &str| Msg::Assistant {
            text: t.into(),
            calls: vec![],
        };
        let mut t = vec![
            Msg::System("sys".into()),
            Msg::User("first".into()),
            answer("a1"),
            Msg::User("second\n\n[hook context]\nbranch main".into()),
            answer("a2"),
            Msg::User("third".into()),
            answer("a3"),
        ];
        let point = |index, prompt: &str| RewindPoint {
            index,
            started_ms: 0,
            prompt: prompt.into(),
        };
        // A hook appended context to "second": still found.
        assert!(rewind_transcript(&mut t, &point(3, "second")));
        assert_eq!(t.len(), 3);
        assert!(matches!(t.last(), Some(Msg::Assistant { text, .. }) if text == "a1"));
        // The first prompt of a conversation (recorded at index 0, behind the
        // system prompt the turn added) leaves an empty conversation.
        assert!(rewind_transcript(&mut t, &point(0, "first")));
        assert!(t.is_empty());
        // A prompt that is no longer there changes nothing.
        let mut t = vec![Msg::System("sys".into()), Msg::User("summary".into())];
        assert!(!rewind_transcript(&mut t, &point(1, "gone")));
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn a_cancelled_plan_stays_in_plan() {
        assert!(matches!(
            mode_after_plan(agent::PlanEnd::Executed),
            Mode::Build
        ));
        assert!(matches!(
            mode_after_plan(agent::PlanEnd::Cancelled),
            Mode::Plan
        ));
        assert!(matches!(
            mode_after_plan(agent::PlanEnd::Answered),
            Mode::Plan
        ));
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
        // The missing-value rule applies to --effort like every other option.
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
    fn what_you_type_is_what_the_model_gets() {
        let dir = std::env::temp_dir().join(format!("bwn-attach-exact-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app.py"), "line 1\nline 2\nline 3\n").unwrap();
        std::fs::write(dir.join("my notes.md"), "spaced\n").unwrap();
        // Prompts with no attachment are sent byte for byte.
        for typed in [
            "why does int(\"abc\") fail with 'abc'?",
            "line one\nline two",
            "tabs\tand  two spaces, \"double\" and 'single' quotes",
            "unbalanced \"quote and it's fine",
            "a \\ backslash and C:\\path\\x",
        ] {
            let (text, images) = extract_attachments(typed, &dir, true);
            assert_eq!(text, typed);
            assert!(images.is_empty());
        }
        // Attachments are replaced where they stand; the rest stays as typed.
        let typed = "see @app.py:1-2,\n\tthen \"explain\" it";
        let (text, _) = extract_attachments(typed, &dir, true);
        let file = format!("[file: {}]", dir.join("app.py").display());
        assert!(
            text.starts_with(&format!(
                "see {file},\n\tthen \"explain\" it\n\n[attached files]\n"
            )),
            "{text}"
        );
        assert!(
            text.contains("line 1\nline 2") && !text.contains("line 3"),
            "{text}"
        );
        // Quoted and escaped @paths with spaces are one attachment.
        for typed in [
            "read @\"my notes.md\" now",
            "read @'my notes.md' now",
            "read @my\\ notes.md now",
        ] {
            let (text, _) = extract_attachments(typed, &dir, true);
            let file = format!("[file: {}]", dir.join("my notes.md").display());
            assert!(
                text.starts_with(&format!("read {file} now\n\n")),
                "{typed}: {text}"
            );
        }
        // The composer's own tokens for dropped files round-trip.
        for name in ["my notes.md", "say \"hi\" notes.md", "back\\slash notes.md"] {
            std::fs::write(dir.join(name), "dropped\n").unwrap();
            let token = crate::tui::attachment_token(&dir.join(name));
            let (text, _) = extract_attachments(&format!("read {token}"), &dir, true);
            assert!(text.contains("dropped"), "{token}: {text}");
        }
        // A missing file stays exactly as typed.
        let (text, _) = extract_attachments("open @nope.py  please", &dir, true);
        assert_eq!(text, "open @nope.py  please");
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
        std::fs::write(dir.join("My Shot.png"), png).unwrap();
        let typed = format!(
            "what's in '{}'?\nand (shot.png)",
            dir.join("My Shot.png").display()
        );
        let (text, images) = extract_attachments(&typed, &dir, true);
        assert_eq!(images.len(), 2, "{text}");
        assert_eq!(
            text,
            "what's in [image: My Shot.png]?\nand ([image: shot.png])"
        );
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
        // Addresses /model remembers for local presets come next; a hosted
        // one is not a local server.
        let s = Settings {
            provider: "openai".into(),
            base_url: Some("https://api.openai.com/v1".into()),
            endpoints: [
                (
                    "ollama".to_string(),
                    "http://192.168.50.10:11434".to_string(),
                ),
                (
                    "openai".to_string(),
                    "https://api.openai.com/v1".to_string(),
                ),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let servers = local_servers(&s);
        assert_eq!(servers[0].base, "http://192.168.50.10:11434");
        assert_eq!(servers[0].preset, "ollama");
        assert_eq!(servers.len(), 5);
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
