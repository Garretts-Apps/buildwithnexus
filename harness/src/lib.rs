//! An agentic AI coding CLI with 2 ms startup — one self-contained binary,
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
//! | [`mcp_auth`] | OAuth for HTTP MCP servers: discovery, PKCE login, saved tokens, refresh |
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

mod acp;
pub mod agent;
pub mod checkpoint;
pub mod config;
pub mod graphics;
pub mod highlight;
pub mod hooks;
pub mod knowledge;
pub mod local;
pub mod mcp;
pub mod mcp_auth;
pub mod media;
pub mod net;
pub mod onboarding;
pub mod provider;
pub mod report;
pub mod rules;
pub mod sandbox;
pub mod screenshot;
pub mod session;
pub mod sixel;
pub mod tools;
pub mod trace;
pub mod tui;
pub mod update;
pub mod usage;
pub mod verifier;
pub mod workdirs;
pub mod workflow;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// `--trust-project-allow <keys>` (or BWN_TRUST_PROJECT_ALLOW): the keys
    /// asked about on their own (base_url, permission) that the digest may
    /// trust too.
    trust_project_allow: Option<String>,
    /// `--base-url <url>`: the model endpoint, over the settings value.
    base_url: Option<String>,
    /// Words before `--` that look like options but are none of ours, in
    /// order. Commands with options of their own (`mcp add --url`) read
    /// them; every other command refuses them as a usage error.
    unknown_flags: Vec<String>,
    /// `--worktree <name>`: run the session in .bwn/worktrees/<name> on
    /// branch bwn/<name>.
    worktree: Option<String>,
    /// `--add-dir <path>`, repeatable: more folders to work in.
    add_dirs: Vec<String>,
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
    "--trust-project-allow",
    "--worktree",
    "--add-dir",
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
        if flag == "--add-dir" {
            let dir = inline
                .map(str::to_string)
                .or_else(|| it.next().filter(|v| !v.starts_with('-')))
                .filter(|v| !v.trim().is_empty())
                .ok_or("--add-dir requires a folder; see `buildwithnexus --help`")?;
            opts.add_dirs.push(dir);
            continue;
        }
        let slot = match flag {
            "--provider" => &mut opts.provider,
            "--model" => &mut opts.model,
            "--permission-mode" | "--permission" => &mut opts.permission_mode,
            "--sandbox" => &mut opts.sandbox,
            "--prompt" => &mut opts.prompt,
            "--effort" => &mut opts.effort,
            "--max-budget-usd" => &mut budget_raw,
            "--trust-project" => &mut opts.trust_project,
            "--trust-project-allow" => &mut opts.trust_project_allow,
            "--base-url" => &mut opts.base_url,
            "--worktree" => &mut opts.worktree,
            _ => {
                let named = !looks_like_option(&arg);
                rest.push(arg);
                // `mcp add <name> npx -y pkg --model m`: the server's own
                // command line, word for word, never bwn's options.
                if named && rest.len() > 2 && rest[..2] == ["mcp", "add"] {
                    literal_from = Some(rest.len());
                    rest.extend(it);
                    break;
                }
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
    fn an_mcp_servers_command_line_is_kept_word_for_word() {
        let (opts, rest) = parse(&[
            "--json", "mcp", "add", "--force", "fs", "npx", "-y", "pkg", "--json", "--model", "m",
            "--", "x",
        ])
        .unwrap();
        assert!(opts.json, "options before the name are still bwn's");
        assert!(!opts.yes && opts.model.is_none());
        assert_eq!(
            rest,
            [
                "mcp", "add", "--force", "fs", "npx", "-y", "pkg", "--json", "--model", "m", "--",
                "x"
            ]
        );
        assert!(opts.unknown_flags.iter().all(|f| f == "--force"));
        // Other commands still take -y and --model wherever they are.
        let (opts, rest) = parse(&["mcp", "list", "-y"]).unwrap();
        assert!(opts.yes);
        assert_eq!(rest, ["mcp", "list"]);
    }

    #[test]
    fn add_dir_repeats_in_either_spelling_and_needs_a_folder() {
        let (opts, rest) = parse(&["--add-dir", "../a", "run", "--add-dir=/b", "x"]).unwrap();
        assert_eq!(opts.add_dirs, ["../a", "/b"]);
        assert_eq!(rest, ["run", "x"]);
        assert!(opts.unknown_flags.is_empty());
        let err = parse(&["--add-dir", "--json", "run", "x"]).unwrap_err();
        assert!(err.contains("--add-dir requires a folder"), "{err}");
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

/// Exit code of a process a shell reports as killed by SIGPIPE (128 + 13).
const EXIT_BROKEN_PIPE: i32 = 141;

// Rust ignores SIGPIPE so that a closed socket or hook pipe is an error to
// handle, and println! then panics once the reader of our stdout is gone
// (`bwn run ... | head -1`). Only that panic ends the run, quietly, with the
// code a shell gives SIGPIPE; every other panic goes on as before. SIGPIPE
// itself stays ignored: sockets and child pipes depend on it.
fn exit_quietly_on_closed_stdout() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("");
        if message.starts_with("failed printing to std") && message.contains("Broken pipe") {
            std::process::exit(EXIT_BROKEN_PIPE);
        }
        prev(info);
    }));
}

pub fn run() {
    exit_quietly_on_closed_stdout();
    #[cfg(unix)]
    {
        sandbox::cloexec_inherited_fds();
        sandbox::hide_startup_credentials();
    }
    tui::init_console();
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
    hooks::set_trust_digest(opts.trust_project.clone(), opts.trust_project_allow.clone());
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
    // A relative --add-dir names a folder from where bwn was started, even
    // when --worktree moves the session.
    let launch_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let add_dirs: Vec<PathBuf> = opts
        .add_dirs
        .iter()
        .map(|d| tools::resolve(&launch_dir, d.trim()))
        .collect();
    if let Some(name) = &opts.worktree {
        if let Err((code, e)) = enter_session_worktree(name) {
            eprintln!("buildwithnexus: --worktree: {e}");
            std::process::exit(code);
        }
    }
    // Checked here, before any request; their notices come with the
    // session's other startup lines.
    if !add_dirs.is_empty() {
        let cwd = std::env::current_dir().unwrap_or(launch_dir);
        for dir in &add_dirs {
            if let Err(e) = workdirs::add(&dir.to_string_lossy(), &cwd) {
                eprintln!("buildwithnexus: --add-dir: {e}");
                std::process::exit(2);
            }
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
                if let Some(name) = untrusted_repo_command(&input.argv, &cwd) {
                    eprintln!(
                        "buildwithnexus: /{} comes from this repo and is off until the folder is trusted: --trust-project <digest> (`buildwithnexus trust --print` shows it)",
                        tui::sanitize_terminal(&name)
                    );
                    std::process::exit(2);
                }
                if let Some(msg) = unloaded_repo_command(&input.argv, &cwd) {
                    eprintln!("buildwithnexus: {msg}");
                    std::process::exit(2);
                }
                if let Some(msg) = unknown_slash_command(&input.argv) {
                    eprintln!("buildwithnexus: {msg}");
                    std::process::exit(2);
                }
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
        "acp" => {
            if let Some(extra) = args.get(1) {
                eprintln!("buildwithnexus acp: unexpected argument '{extra}'; the editor talks to it on stdin");
                std::process::exit(2);
            }
            std::process::exit(acp::serve(&opts))
        }
        "mcp" => match mcp::manage(&args[1..], false, &mut |l| println!("  {l}")) {
            Ok(lines) => {
                for l in lines {
                    println!("  {l}");
                }
            }
            Err(e) => {
                eprintln!("buildwithnexus mcp: {e}");
                // A refusal to overwrite or a failed login is not a usage
                // mistake.
                std::process::exit(mcp::exit_code(&e));
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
fn print_sessions_json(all: &[session::SessionInfo]) {
    for s in all {
        report::event(serde_json::json!({
            "type": "session",
            "id": s.id,
            "title": s.title,
            "cwd": s.cwd,
            "model": s.model,
            "created_ms": s.created_ms as u64,
            "updated_ms": s.updated_ms as u64,
            "messages": s.msg_count,
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
            crate::report::plural(dirty)
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
    // The interactive start-up puts the local server's window off until the
    // first prompt is up; setup (above) and every other caller ask in place.
    let defer = INTERACTIVE_STARTUP.swap(false, Ordering::Relaxed);
    let (mut provider, probe_due) = build_provider_deferring(&settings, None, defer)?;
    SERVER_PROBE_DUE.store(probe_due, Ordering::Relaxed);
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
    let local = config::preset(&settings.provider).is_some_and(|p| p.local)
        || is_loopback_url(&provider.base_url);
    agent::set_max_parallel_helpers(settings.max_parallel_helpers.unwrap_or_else(|| {
        agent::default_parallel_helpers(local, provider::served_slots(&provider))
    }));
    agent::set_max_parallel_tools(
        settings
            .max_parallel_tools
            .unwrap_or(agent::DEFAULT_MAX_PARALLEL_TOOLS),
    );
    if let Some(note) = agent::max_parallel_helpers_note(settings.max_parallel_helpers) {
        eprintln!("{}", tui::yellow(&format!("buildwithnexus: {note}")));
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

pub(crate) fn is_loopback_url(u: &str) -> bool {
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
/// probe before it is written to disk. For the custom endpoint an empty key
/// means none, not the saved one.
pub(crate) fn build_provider_with_key(s: &Settings, key: Option<&str>) -> Result<Provider, String> {
    build_provider_deferring(s, key, false).map(|(p, _)| p)
}

// `build_provider_with_key`, optionally without asking a local model server
// for its window (a blocking request): the second value is then whether that
// question is still to be asked, off the main thread.
fn build_provider_deferring(
    s: &Settings,
    key: Option<&str>,
    defer_probe: bool,
) -> Result<(Provider, bool), String> {
    let mut probe_due = false;
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
        // Each endpoint has its own saved key.
        let api_key = match key {
            Some(k) => Some(k.to_string()).filter(|k| !k.is_empty()),
            None => config::load_custom_key(&base_url),
        };
        if api_key.is_none() && key.is_none() && config::unbound_custom_key().is_some() {
            report::notice(&format!(
                "  a CUSTOM_API_KEY saved by an earlier version is not tied to an endpoint, so it is not sent to {} — /model offers it for an endpoint, and `buildwithnexus login` saves one for this one",
                config::endpoint_origin(&base_url)
            ));
        }
        api_key
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
    // The exception is a `…/v1` address that Ollama itself answers: its
    // OpenAI-compat endpoint cannot take num_ctx, so the prompt is silently
    // cut to the server's default window. That one is sent natively.
    let mut protocol = preset.protocol;
    if protocol == config::Protocol::OllamaNative
        && s.base_url
            .as_deref()
            .is_some_and(|u| u.trim_end_matches('/').ends_with("/v1"))
        && provider::ollama_models_checked(&base_url).is_none()
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
    if let Some(n) = s.context_tokens {
        if provider.protocol == config::Protocol::OllamaNative {
            // Pre-seed the probe cache: the native path uses the override as
            // num_ctx without ever querying /api/show.
            let _ = provider.ollama_ctx.set(Some(n));
        }
    } else if provider.protocol == config::Protocol::OllamaNative
        || preset.local
        || is_loopback_url(&provider.base_url)
    {
        if defer_probe {
            probe_due = true;
        } else {
            ask_local_server(&mut provider);
        }
    }
    media::set_vision_override(s.vision);
    if s.context_tokens.is_some() {
        provider::remember_window(&provider);
    }
    Ok((provider, probe_due))
}

// Set while the interactive UI is starting: the provider it builds leaves the
// local model server's window to be asked off the main thread (see
// `repl::spawn_startup_probes`), so the first prompt never waits on a server
// that is slow or not there.
static INTERACTIVE_STARTUP: AtomicBool = AtomicBool::new(false);
// That question is still to be asked.
static SERVER_PROBE_DUE: AtomicBool = AtomicBool::new(false);

// Asks the local model server what window (and, where it says, vision and
// slots) it loaded the model with. Blocks on the network: up to the probe's
// timeout when the server is slow.
fn ask_local_server(provider: &mut Provider) {
    if provider.protocol == config::Protocol::OllamaNative {
        if let Some(n) = provider::ollama_ctx(provider) {
            // Detected window replaces the hardcoded 8k local default so
            // compaction thresholds match what the model can actually hold.
            provider.context_tokens = n as usize;
        }
        return;
    }
    // llama.cpp, LM Studio and vLLM report the window they loaded the
    // model with; the 8k guess stands only when they say nothing.
    let served = provider::served_model(&provider.base_url, &provider.model);
    if let Some(n) = served.window {
        provider.context_tokens = n;
        provider::remember_window(provider);
    }
    if let Some(v) = served.vision {
        provider::remember_vision(provider, v);
    }
    if let Some(n) = served.slots {
        provider::remember_slots(provider, n);
    }
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
            // A command that takes an argument and got none is filled from
            // the piped text; with neither, it is run empty and says so.
            if args.is_empty() && config::command_takes_arguments(&cmd) {
                match stdin.map(str::trim).filter(|s| !s.is_empty()) {
                    Some(piped) => return (config::command_prompt(&cmd, piped), Vec::new()),
                    None => report::notice(&format!(
                        "  /{} takes an argument and none was given — it runs with the gap empty",
                        tui::sanitize_terminal(&cmd.name)
                    )),
                }
            }
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

    // The repository's instruction files: acknowledged once in the terminal
    // UI; until then a headless run says so in one line on stderr.
    let repo = config::repo_instructions(&cwd);
    if let Some(repo) = repo.as_ref().filter(|r| !r.acknowledged(&cwd)) {
        let note = tui::sanitize_terminal(&repo.notice()).into_owned();
        eprintln!(
            "{}",
            tui::yellow(&format!(
                "buildwithnexus: {note} (not reviewed — open bwn in this folder once to review them)"
            ))
        );
    }

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
        if let Some(repo) = repo.as_ref().filter(|r| r.acknowledged(&cwd)) {
            let note = tui::sanitize_terminal(&repo.notice()).into_owned();
            println!("{}", tui::dim(&format!("  {note}")));
        }
        println!();
        // Off the critical path: five `which` probes cost real startup latency,
        // and with interactive=false this only prints when something is missing.
        std::thread::spawn(check_and_offer_install_dependencies);
    }
    // Added folders, and the instruction files they bring, are named
    // before the model reads them (a `notice` event in --json mode).
    for n in workdirs::notices(&cwd) {
        report::notice(&format!("  {n}"));
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
    // A PermissionRequest or PreToolUse hook may answer for the terminal.
    let hook_answers = hooks::has_event("PermissionRequest") || hooks::has_event("PreToolUse");
    if unattended && perm == Permission::Ask && hook_answers {
        eprintln!(
            "{}",
            tui::yellow(
                "buildwithnexus: no terminal to approve changes: edits and commands your \
                 PermissionRequest or PreToolUse hooks do not allow will be blocked."
            )
        );
    } else if unattended && perm == Permission::Ask {
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
    let blocked_calls = agent::blocked_without_terminal();
    let blocked = blocked_calls.len();
    if r.is_ok() && blocked > 0 {
        r = Err(blocked_line(
            &blocked_calls,
            agent::blocked_needing_a_person().len(),
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
                    crate::report::plural(blocking)
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

// The closing line of a run whose changes nobody could approve: what was
// blocked, by its approval label, and nothing about the rest of the run.
// `need_person` of them are calls auto mode asks about too (sensitive
// paths, dangerous commands, ask rules), so auto is not offered for those.
fn blocked_line(calls: &[String], need_person: usize) -> String {
    let n = calls.len();
    let shown: Vec<&str> = calls.iter().take(3).map(String::as_str).collect();
    let more = match n.saturating_sub(shown.len()) {
        0 => String::new(),
        k => format!(" and {k} more"),
    };
    let it = if n == 1 { "it" } else { "them" };
    let advice = if need_person == 0 {
        format!("Re-run with --permission-mode auto to allow {it}.")
    } else if need_person >= n {
        format!(
            "{} always {} a person to approve, even with --permission-mode auto: run bwn in a terminal to approve {it}.",
            if n == 1 { "This one" } else { "These" },
            if n == 1 { "needs" } else { "need" }
        )
    } else {
        format!(
            "{need_person} of them always need a person to approve, even with --permission-mode auto (run bwn in a terminal for those); auto allows the rest."
        )
    };
    format!(
        "{n} change{} blocked for lack of approval and not made: {}{more}. {advice}",
        if n == 1 { " was" } else { "s were" },
        shown.join("; "),
    )
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
                    if crate::session::last_save_ok() {
                        format!(
                            " — session {id} saved; resume with `buildwithnexus resume {id} <task>`"
                        )
                    } else {
                        format!(" — session {id} could not be saved")
                    }
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
    if let Some(u) = &opts.base_url {
        settings.base_url = Some(u.clone());
    }
    if onboarding::login(&settings).is_none() {
        std::process::exit(1);
    }
}

fn interactive(initial_prompt: Option<String>, opts: CliOptions) {
    // MCP login hints say /mcp login here, not `bwn mcp login`.
    mcp_auth::set_in_session();
    provider::set_in_session();
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
    // Every launch past setup scaffolds, so existing users also get the
    // directory skeleton and starter Agents.md if they're missing; setup
    // left early writes nothing.
    config::scaffold_home();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let raw = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    // Before the provider is built: base_url, permission and sandbox may
    // come from the project only once the user trusts it.
    hooks::trust_project(&cwd, raw);
    INTERACTIVE_STARTUP.store(true, Ordering::Relaxed);
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

mod repl;
pub use repl::kill_local_server;
use repl::*;

mod review;
use review::*;

mod commands;
pub(crate) use commands::show_instruction_files;
use commands::*;

// ── Mode ──────────────────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
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
        eprintln!("⎘ attached {n} image{}", crate::report::plural(n));
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
        if !git_attachment_may_run(word, cwd) {
            return None;
        }
        if let Ok(diff_text) = git_text(cwd, &["diff", "HEAD", "--no-ext-diff", "--no-textconv"]) {
            if !diff_text.trim().is_empty() {
                text_attachments.push(format!("[git diff HEAD]\n{}", diff_text));
                return Some("[git diff HEAD]".to_string());
            }
        }
    } else if raw_path == "status" || raw_path == "git:status" {
        if !git_attachment_may_run(word, cwd) {
            return None;
        }
        if let Ok(stat_text) = git_text(cwd, &["status", "-s"]) {
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
    let p = tools::resolve(cwd, raw_path);
    // Credentials must never ride along in a prompt unnoticed; the
    // token stays as typed and nothing is read.
    if p.exists() && tools::is_sensitive(&p) {
        tui::line(&tui::yellow(&format!(
            "  ⚠ {} not attached — it is a sensitive file (keys, credentials, secrets)",
            tui::sanitize_terminal(&p.display().to_string())
        )));
    } else if image_exts.contains(&ext.as_str()) && p.exists() {
        let too_big = std::fs::metadata(&p)
            .ok()
            .and_then(|m| tools::image_too_big(raw_path, m.len()));
        if let Vision::No(why) = vision {
            tui::line(&tui::yellow(&format!("  ⚠ {why}")));
        } else if let Some(why) = too_big {
            // As read_file refuses it: nothing that size is sent.
            tui::line(&tui::yellow(&format!(
                "  ⚠ {} and attach that.",
                tui::sanitize_terminal(&why)
            )));
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
    } else if is_at && !p.exists() && !is_special_mention(raw_path) {
        tui::line(&tui::yellow(&format!(
            "  ⚠ {} not found — sent as typed",
            tui::sanitize_terminal(word)
        )));
    }
    None
}

// The `@` words that are not file names: they have their own meaning (and
// their own notice) when they attach nothing.
fn is_special_mention(raw_path: &str) -> bool {
    matches!(
        raw_path,
        "" | "diff" | "git:diff" | "status" | "git:status" | "rules"
    ) || ["kb:", "rule:", "url:", "web:", "symbol:"]
        .iter()
        .any(|p| raw_path.starts_with(p))
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
    print!("{}", usage_text());
}

// `--help`: every command, subcommand and option, the exit codes, and the
// session's commands by section (/help explains each).
fn usage_text() -> String {
    let mut out = format!(
        "buildwithnexus {VERSION} — agentic AI CLI harness\n\n\
         USAGE:\n\
         \x20 buildwithnexus (also `bwn`)       interactive session (all modes, full TUI)\n\
         \x20 buildwithnexus run <task>         execute a task (agentic BUILD loop); also -p, --print\n\
         \x20 ... | buildwithnexus run [task]   piped text is the task, or context after it (1 MiB)\n\
         \x20 buildwithnexus plan <task>        decompose, approve, then execute (--yes: no approval)\n\
         \x20 buildwithnexus brainstorm <q>     chat with tools (grep, fetch, read, etc.)\n\
         \x20 buildwithnexus continue [task]    this folder's latest session; also -c, --continue\n\
         \x20 buildwithnexus resume [<id> [task]]  a saved session; also -r, --resume\n\
         \x20 buildwithnexus sessions           list saved sessions (this folder first)\n\
         \x20 buildwithnexus sessions rm <id>   delete a session; also delete, remove\n\
         \x20 buildwithnexus sessions export <id> [file.md]  a session as Markdown\n\
         \x20 buildwithnexus init               (re)configure provider / model / key; also setup\n\
         \x20 buildwithnexus init --agents-md   write AGENTS.md from this repository\n\
         \x20 buildwithnexus login              replace the provider's API key (checked first)\n\
         \x20 buildwithnexus providers          list built-in providers\n\
         \x20 buildwithnexus doctor             diagnose setup (keys, tools, connectivity)\n\
         \x20 buildwithnexus update [--check]   install the latest release (--check: exit 10 if behind)\n\
         \x20 buildwithnexus review [--base <ref>|--staged] [focus]  read-only review (exit 9: blocking)\n\
         \x20 buildwithnexus trust --print      this folder's settings digest, for --trust-project\n\
         \x20 buildwithnexus mcp [list|<name>|add|remove|login|logout|reload]  manage MCP servers (mcp help)\n\
         \x20 buildwithnexus acp                Agent Client Protocol server on stdio (Zed, JetBrains, Neovim)\n\
         \x20 buildwithnexus version | help     also -v, -V, --version, -h, --help\n\
         \x20 older spellings still accepted: build, headless, --headless (run); da-init (init)\n\n\
         OPTIONS:\n\
         \x20 --provider <name>             override the configured provider\n\
         \x20 --model <name>                override the configured model\n\
         \x20 --base-url <url>              model endpoint, e.g. a gateway (--provider custom\n\
         \x20                               reads CUSTOM_API_KEY from the environment)\n\
         \x20 --permission-mode <mode>      ask, accept-edits, auto, or readonly (plan reads as\n\
         \x20                               readonly); also --permission\n\
         \x20 --sandbox <mode>              off, auto, or require (OS sandbox for shell commands)\n\
         \x20 --worktree <name>             work in .bwn/worktrees/<name> on branch bwn/<name>\n\
         \x20 --add-dir <path>              also read and change files in <path> (repeatable)\n\
         \x20 --prompt <text>               initial interactive prompt\n\
         \x20 --effort <level>              reasoning depth: off, low, medium, high\n\
         \x20 --max-budget-usd <n>          stop before the next request once spend exceeds n\n\
         \x20 --trust-project <digest>      trust exactly this project settings content for this\n\
         \x20                               run (from `trust --print`; or BWN_TRUST_PROJECT)\n\
         \x20 --trust-project-allow <keys>  let that digest trust base_url and/or permission\n\
         \x20                               too, e.g. base_url,permission (or\n\
         \x20                               BWN_TRUST_PROJECT_ALLOW)\n\
         \x20 --plain                       line mode: no alternate screen or cursor control\n\
         \x20                               (TERM=dumb does the same)\n\
         \x20 --json                        structured headless output\n\
         \x20 --yes, -y                     auto-approve the plan and execute (plan)\n\
         \x20 --legacy-exit-codes           exit 0 when a run stops short without failing\n\
         \x20 --                            stop parsing options (run -- <task>)\n\n\
         EXIT CODES (headless; --json ends with a result event naming the outcome):\n\
         \x20 0 success   1 failed   2 usage error   3 changes blocked or denied\n\
         \x20 4 hook blocked the task   5 budget limit   6 step limit\n\
         \x20 7 checks fail   8 verification failed   9 review found blocking issues\n\
         \x20 130/143 interrupted (SIGINT/SIGTERM)\n\n\
         INTERACTIVE (/help in a session explains each):\n"
    );
    let mut sections: Vec<(&str, Vec<&str>)> = Vec::new();
    for c in tui::COMMANDS {
        let names = std::iter::once(c.name).chain(c.aliases.iter().copied());
        match sections.iter_mut().find(|(t, _)| *t == c.section) {
            Some((_, list)) => list.extend(names),
            None => sections.push((c.section, names.collect())),
        }
    }
    let title_w = sections.iter().map(|(t, _)| t.len()).max().unwrap_or(0);
    for (title, names) in sections {
        // Wrapped to 80 columns under the section's name.
        let mut line = format!("  {title:<title_w$} ");
        let indent = line.len();
        for n in names {
            if line.len() + 1 + n.len() > 80 && line.len() > indent {
                out.push_str(line.trim_end());
                out.push('\n');
                line = " ".repeat(indent);
            }
            line.push(' ');
            line.push_str(n);
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(&format!(
        "  !<cmd> runs a shell command · @<path> attaches a file · Shift+Tab cycles mode\n\
         \x20 Esc stops the agent · Esc Esc on an empty line rewinds · Tab completes\n\n\
         What leaves your machine, and where keys are kept: {DATA_DOCS}\n"
    ));
    out
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

// What an Ollama setup sends as its context window. Ollama's OpenAI-compatible
// `/v1` endpoint takes no num_ctx, so a prompt past the server's default
// window is cut without a word; only the native API carries it.
fn ollama_window_check(p: &Provider) -> Option<DoctorCheck> {
    match p.protocol {
        config::Protocol::OllamaNative => Some(DoctorCheck::note(
            "context",
            format!("{} tokens, sent as num_ctx", p.context_tokens),
        )),
        config::Protocol::OpenAi
            if p.base_url.trim_end_matches('/').ends_with("/v1")
                && provider::ollama_models_checked(&p.base_url).is_some() =>
        {
            Some(DoctorCheck::from_glyph(
                "context",
                "⚠",
                "this is Ollama addressed through its /v1 endpoint, which takes no num_ctx: \
                 prompts are cut at the server's default window and the footer's window is a guess. \
                 Use provider ollama (`buildwithnexus init`), or set context_tokens",
            ))
        }
        _ => None,
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
    if let Some((s, preset)) = settings
        .as_ref()
        .and_then(|s| config::preset(&s.provider).map(|p| (s, p)))
    {
        if preset.id == "custom" {
            let url = s.base_url.as_deref().unwrap_or(preset.base_url);
            let origin = config::endpoint_origin(url);
            out.push(DoctorCheck::note(
                config::CUSTOM_KEY,
                if config::load_custom_key(url).is_some() {
                    format!("set for {origin}")
                } else {
                    format!("not set for {origin} (optional for most servers)")
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
            Some(p) => {
                out.push(provider_check(p, &s.provider));
                out.extend(ollama_window_check(p));
            }
            None => match build_provider(s) {
                Ok(p) => {
                    out.push(provider_check(&p, &s.provider));
                    out.extend(ollama_window_check(&p));
                }
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
    if let Some(note) = settings
        .as_ref()
        .and_then(|s| agent::max_parallel_helpers_note(s.max_parallel_helpers))
    {
        out.push(DoctorCheck::note("helpers", note));
    }

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
            match install_command(bin) {
                Some(how) => {
                    DoctorCheck::note(bin, format!("{label} — not found; install it with: {how}"))
                }
                None => DoctorCheck::note(bin, format!("{label} — not found")),
            }
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
            crate::report::plural(failed.len()),
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

/// Lists a missing tool bwn itself needs (git) as plain advice at session
/// start; it never installs anything. `doctor` names every missing tool with
/// its install command in its own checks. The name is kept for its callers.
pub fn check_and_offer_install_dependencies() {
    let brew = crate::tools::find_on_path("brew").is_some();
    let apt = crate::tools::find_on_path("apt-get").is_some();
    for &(bin, brew_pkg, apt_pkg, page, why, needed) in DEPENDENCIES {
        if !needed || crate::tools::find_on_path(bin).is_some() {
            continue;
        }
        let how = install_hint(brew_pkg, apt_pkg, page, brew, apt, cfg!(windows));
        tui::line(&tui::yellow(&format!(
            "  · {bin} not found — {why}; install it with: {how}"
        )));
    }
}

// How to install `bin` here, when it is one of DEPENDENCIES.
fn install_command(bin: &str) -> Option<String> {
    let &(_, brew_pkg, apt_pkg, page, _, _) = DEPENDENCIES.iter().find(|d| d.0 == bin)?;
    Some(install_hint(
        brew_pkg,
        apt_pkg,
        page,
        crate::tools::find_on_path("brew").is_some(),
        crate::tools::find_on_path("apt-get").is_some(),
        cfg!(windows),
    ))
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
mod tests;
