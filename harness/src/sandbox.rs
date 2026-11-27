//! OS-level sandbox for shell commands. On by default (`auto`) since 0.21:
//! `"sandbox": "off"` or `--sandbox off` restores the old behavior.
//!
//! An extra layer *under* the permission gate: once a `run_command` / `bash`
//! / `check_work` / `start_server` / `python_tool` call has been approved,
//! the child process is confined so that filesystem writes outside the
//! working directory (and, optionally, network access) fail at the OS level.
//! Linux prefers `bwrap` when it works, and otherwise uses the built-in
//! Landlock and seccomp backend. macOS uses `sandbox-exec`. Windows has none.
//!
//! Confined: writes anywhere but the working directory and the temp dirs;
//! reads of the user's credential stores and the SSH agent's socket (see
//! `hidden_paths`); the network when `sandbox_network` is false. Not
//! confined: other reads, the agent's own file tools (already fenced to cwd
//! and gated on sensitive paths), hooks, MCP servers.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::OnceLock;

/// Session policy: `off` (never), `auto` (when a backend works, otherwise run
/// unsandboxed with a one-time notice), `require` (refuse without a backend).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    Auto,
    Require,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Mode::Off),
            "auto" => Some(Mode::Auto),
            "require" => Some(Mode::Require),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Auto => "auto",
            Mode::Require => "require",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Bwrap,
    SandboxExec,
    /// Linux, when bubblewrap is missing or cannot make a namespace.
    Landlock,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::Bwrap => "bwrap (bubblewrap)",
            Backend::SandboxExec => "sandbox-exec (Seatbelt)",
            Backend::Landlock => "landlock+seccomp (built-in)",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Unsandboxed,
    Sandboxed(Backend),
    Refused,
    /// `auto` with no working backend: commands are refused until a person
    /// allows them to run unconfined for this session (decision D1).
    NeedsException,
}

/// The policy in one place: what `mode` does given whether a backend works.
pub fn decide(mode: Mode, backend: Option<Backend>) -> Decision {
    match (mode, backend) {
        (Mode::Off, _) => Decision::Unsandboxed,
        (Mode::Auto, None) => Decision::NeedsException,
        (Mode::Auto, Some(b)) | (Mode::Require, Some(b)) => Decision::Sandboxed(b),
        (Mode::Require, None) => Decision::Refused,
    }
}

// ── session state ─────────────────────────────────────────────────────────────
static MODE: AtomicU8 = AtomicU8::new(0); // 0 off, 1 auto, 2 require
static NETWORK: AtomicBool = AtomicBool::new(true);
static NOTICED: AtomicBool = AtomicBool::new(false);
// A person allowed commands to run unconfined for this session (D1). Never
// saved: a resumed session asks again.
static UNCONFINED_ALLOWED: AtomicBool = AtomicBool::new(false);

/// Record that a person allowed commands to run unconfined for this session.
pub fn allow_unconfined_for_session() {
    UNCONFINED_ALLOWED.store(true, Ordering::Relaxed);
}

/// Whether a person allowed unconfined commands for this session.
pub fn unconfined_allowed() -> bool {
    UNCONFINED_ALLOWED.load(Ordering::Relaxed)
}

/// Whether the next confined tool call needs that person's answer first.
pub fn needs_exception() -> bool {
    decide(mode(), backend()) == Decision::NeedsException && !unconfined_allowed()
}

#[cfg(test)]
pub(crate) fn forget_unconfined_allowed() {
    UNCONFINED_ALLOWED.store(false, Ordering::Relaxed);
}

pub fn set_mode(mode: Mode) {
    let v = match mode {
        Mode::Off => 0,
        Mode::Auto => 1,
        Mode::Require => 2,
    };
    MODE.store(v, Ordering::Relaxed);
}

pub fn mode() -> Mode {
    match MODE.load(Ordering::Relaxed) {
        1 => Mode::Auto,
        2 => Mode::Require,
        _ => Mode::Off,
    }
}

pub fn set_network(allowed: bool) {
    NETWORK.store(allowed, Ordering::Relaxed);
}

pub fn network() -> bool {
    NETWORK.load(Ordering::Relaxed)
}

// ── credential stores hidden from commands ───────────────────────────────────
// Sandboxed commands may read the whole filesystem except the user's
// credential stores (~/.ssh, ~/.aws, ~/.config/gh, ~/.netrc, bwn's own
// folder, …, see tools::credential_stores) and the SSH agent's socket. A
// command reaches a path however it spells it, so this is enforced by the
// backend at the resolved path, not by reading the command. Paths listed in
// `sandbox_expose` stay readable.

static EXPOSE: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// The `sandbox_expose` list as set.
pub fn exposed() -> Vec<PathBuf> {
    EXPOSE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Set the `sandbox_expose` list (`~` expanded).
pub fn set_expose(paths: &[String]) {
    let home = crate::config::user_home();
    let v = paths
        .iter()
        .map(|p| match (p.strip_prefix("~/"), &home) {
            (Some(rest), Some(h)) => h.join(rest),
            _ => PathBuf::from(p),
        })
        .map(|p| p.canonicalize().unwrap_or(p))
        .collect();
    *EXPOSE.lock().unwrap_or_else(|e| e.into_inner()) = v;
}

/// The paths a command run in `roots` (the workspace first, then folders
/// added with --add-dir) must not read: each credential store that exists,
/// bwn's own folder and the SSH agent's socket, resolved through symlinks.
/// A store that holds a root is left alone (hiding it would hide the
/// workspace), and so is one the user exposed.
pub fn hidden_paths(roots: &[&Path]) -> Vec<PathBuf> {
    let Some(home) = crate::config::user_home() else {
        return Vec::new();
    };
    let expose = EXPOSE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    hidden_paths_in(
        &home,
        &crate::config::home(),
        std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from),
        &expose,
        roots,
    )
}

fn hidden_paths_in(
    home: &Path,
    bwn_home: &Path,
    agent_socket: Option<PathBuf>,
    expose: &[PathBuf],
    roots: &[&Path],
) -> Vec<PathBuf> {
    let roots: Vec<PathBuf> = roots
        .iter()
        .map(|r| r.canonicalize().unwrap_or_else(|_| r.to_path_buf()))
        .collect();
    let mut out: Vec<PathBuf> = Vec::new();
    let candidates = crate::tools::credential_stores(home)
        .into_iter()
        .chain([bwn_home.to_path_buf()])
        .chain(agent_socket);
    for c in candidates {
        if c.symlink_metadata().is_err() {
            continue;
        }
        let c = c.canonicalize().unwrap_or(c);
        if c == Path::new("/") || out.contains(&c) {
            continue;
        }
        if roots.iter().any(|r| r.starts_with(&c)) || expose.iter().any(|e| c.starts_with(e)) {
            continue;
        }
        out.push(c);
    }
    out
}

/// Appended to a sandboxed command's output when it failed on a hidden path.
pub fn hidden_note(output: &str, hidden: &[PathBuf]) -> Option<String> {
    let l = output.to_ascii_lowercase();
    let denied = l.contains("permission denied")
        || l.contains("operation not permitted")
        || l.contains("no such file")
        || l.contains("not found");
    let hit = hidden
        .iter()
        .find(|p| output.contains(p.to_string_lossy().as_ref()))?;
    denied.then(|| {
        format!(
            "{} is hidden from sandboxed commands (credentials stay out of reach of a command the model chose). To let commands read it, add it to \"sandbox_expose\" in settings.json.",
            hit.display()
        )
    })
}

// ── network for commands ─────────────────────────────────────────────────────

/// What a confined command may do on the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Net {
    /// No sockets at all (`sandbox_network: false`, or no way to check).
    Off,
    /// Out only through the egress checker's proxy on this loopback port
    /// (see egress.rs); listening is allowed, so a dev server works.
    Proxy(u16),
    /// Anything (`sandbox_egress_check: false`).
    Open,
}

static EGRESS_CHECK: AtomicBool = AtomicBool::new(true);

/// `sandbox_egress_check` from the settings.
pub fn set_egress_check(on: bool) {
    EGRESS_CHECK.store(on, Ordering::Relaxed);
}

pub fn egress_check() -> bool {
    EGRESS_CHECK.load(Ordering::Relaxed)
}

/// Whether `backend` can hold a command to the proxy: Seatbelt always;
/// on Linux, a kernel whose Landlock limits connections (ABI 4).
pub fn can_check_egress(backend: Backend) -> bool {
    match backend {
        Backend::SandboxExec => true,
        #[cfg(target_os = "linux")]
        Backend::Bwrap | Backend::Landlock => crate::landlock::limits_connections(),
        #[cfg(not(target_os = "linux"))]
        Backend::Bwrap | Backend::Landlock => false,
    }
}

/// The network a command run now under `backend` gets. With the network
/// allowed and the check on, that is the proxy; where the backend cannot
/// hold a command to it, no network (the posture check says so).
pub fn net_for(backend: Backend) -> Net {
    if !network() {
        return Net::Off;
    }
    if !egress_check() {
        return Net::Open;
    }
    if !can_check_egress(backend) {
        return Net::Off;
    }
    match crate::egress::proxy() {
        Some(p) => Net::Proxy(p.port),
        None => Net::Off,
    }
}

/// Apply the `sandbox` / `sandbox_network` settings (or the `--sandbox`
/// flag). Err names the bad value; nothing changes in that case.
pub fn configure(mode: &str, network: bool) -> Result<(), String> {
    let m = Mode::parse(mode)
        .ok_or_else(|| format!("unknown sandbox mode '{mode}' — expected off, auto, or require"))?;
    set_mode(m);
    set_network(network);
    Ok(())
}

/// Apply a `sandbox` value from settings. A value bwn can't parse falls back
/// to `require`, the strictest mode, and the error says so: a typo never
/// leaves commands unconfined (roadmap 1.3; analysis M13).
pub fn configure_setting(mode: &str, network: bool) -> Result<(), String> {
    configure(mode, network).map_err(|e| {
        set_mode(Mode::Require);
        set_network(network);
        format!("{e}; using require until it is fixed")
    })
}

// ── backend detection (once per session) ──────────────────────────────────────
fn probe() -> &'static Result<Backend, String> {
    static PROBE: OnceLock<Result<Backend, String>> = OnceLock::new();
    PROBE.get_or_init(detect)
}

/// The usable backend, if any. Probed once; `bwrap` that is installed but
/// cannot create namespaces (unprivileged user namespaces disabled) counts as
/// unavailable, so `auto` falls back and `require` refuses.
pub fn backend() -> Option<Backend> {
    probe().as_ref().ok().copied()
}

/// Why no backend is usable (None when one is).
pub fn backend_error() -> Option<&'static str> {
    probe().as_ref().err().map(String::as_str)
}

fn first_line(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no output")
        .to_string()
}

fn probe_external(bin: &str, args: Vec<String>, backend: Backend) -> Result<Backend, String> {
    let out = Command::new(bin)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match out {
        Ok(o) if o.status.success() => Ok(backend),
        Ok(o) => Err(format!(
            "{bin} is installed but unusable: {}",
            first_line(&o.stderr)
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "{bin} not found on PATH{}",
            if backend == Backend::Bwrap {
                " (install bubblewrap)"
            } else {
                ""
            }
        )),
        Err(e) => Err(format!("cannot run {bin}: {e}")),
    }
}

fn detect() -> Result<Backend, String> {
    // Debug builds only: the invariant suite runs once per backend on the
    // same machine (roadmap 1.12). A release binary ignores this, so nothing
    // outside a test build can switch the sandbox backend off.
    #[cfg(debug_assertions)]
    if let Ok(forced) = std::env::var("BWN_TEST_SANDBOX_BACKEND") {
        match forced.as_str() {
            "none" => return Err("no backend (BWN_TEST_SANDBOX_BACKEND=none)".into()),
            #[cfg(target_os = "linux")]
            "landlock" => return crate::landlock::available().map(|()| Backend::Landlock),
            #[cfg(target_os = "linux")]
            "bwrap" => return probe_external("bwrap", bwrap_probe_args(), Backend::Bwrap),
            _ => {}
        }
    }
    #[cfg(target_os = "windows")]
    {
        return Err("no sandbox backend on Windows".into());
    }
    #[cfg(target_os = "macos")]
    {
        return probe_external(
            "sandbox-exec",
            vec![
                "-p".into(),
                "(version 1) (allow default)".into(),
                "sh".into(),
                "-c".into(),
                "true".into(),
            ],
            Backend::SandboxExec,
        );
    }
    #[cfg(target_os = "linux")]
    {
        detect_linux()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        Err("no sandbox backend on this platform".into())
    }
}

/// Bubblewrap when it works; otherwise Landlock. Bubblewrap is stricter
/// (private `/tmp`, read-only `.git`). Landlock also covers WSL.
#[cfg(target_os = "linux")]
fn detect_linux() -> Result<Backend, String> {
    let bwrap = if crate::tools::is_wsl() {
        Err("bubblewrap is not used inside WSL".to_string())
    } else {
        probe_external("bwrap", bwrap_probe_args(), Backend::Bwrap)
    };
    match bwrap {
        Ok(b) => Ok(b),
        Err(bwrap_err) => match crate::landlock::available() {
            Ok(()) => Ok(Backend::Landlock),
            Err(land) => Err(format!("{bwrap_err}; Landlock is also unavailable: {land}")),
        },
    }
}

// ── argv / profile construction (pure, unit-tested) ───────────────────────────
/// bubblewrap argv: everything read-only, fresh /dev, /proc, /tmp and /run,
/// the workspace bound read-write at its own path. `~/.buildwithnexus` stays
/// read-only — checkpoints are written in-process, never by the child — and
/// so do the workspace's `.git` and `.buildwithnexus`, where a planted hook or
/// settings file would run later outside the sandbox.
///
/// A protected entry that does not exist yet gets a read-only empty tmpfs at
/// its path, so the command cannot create it either (a new `.git/config`
/// with `core.fsmonitor` would run on the next git call). bwrap leaves an
/// empty directory behind as the mount point. It is not removed afterwards:
/// a background server may still be using it, and git ignores an empty
/// `.git` directory.
///
/// Folders added with --add-dir are bound read-write the same way, each with
/// its own `.git` and `.buildwithnexus` kept read-only.
pub fn bwrap_args(cmd: &str, cwd: &Path, net: Net) -> Vec<String> {
    let extra = crate::workdirs::list();
    let roots: Vec<&Path> = std::iter::once(cwd)
        .chain(extra.iter().map(PathBuf::as_path))
        .collect();
    let hidden = hidden_paths(&roots);
    bwrap_argv(cmd, cwd, &extra, net, true, &hidden)
}

// The availability probe runs at `/` and must not mount anything there.
fn bwrap_probe_args() -> Vec<String> {
    bwrap_argv("true", Path::new("/"), &[], Net::Open, false, &[])
}

fn bwrap_argv(
    cmd: &str,
    cwd: &Path,
    extra: &[PathBuf],
    net: Net,
    protect: bool,
    hidden: &[PathBuf],
) -> Vec<String> {
    let roots: Vec<&Path> = std::iter::once(cwd)
        .chain(extra.iter().map(PathBuf::as_path))
        .collect();
    let mut protected: Vec<(String, bool)> = roots
        .iter()
        .flat_map(|r| PROTECTED_IN_WORKSPACE.iter().map(|d| r.join(d)))
        .filter(|_| protect)
        .map(|p| {
            let exists = p.symlink_metadata().is_ok();
            (p.to_string_lossy().into_owned(), exists)
        })
        .collect();
    if protect {
        for r in &roots {
            protected.extend(
                nested_git_entries(r)
                    .into_iter()
                    .map(|p| (p.to_string_lossy().into_owned(), true)),
            );
        }
    }
    let cwd = cwd.to_string_lossy().into_owned();
    // The proxy is held by bwn's own binary inside the sandbox. Without it
    // the command gets no network rather than the host's (roadmap 1.6;
    // analysis M8(b)).
    let limiter = match net {
        Net::Proxy(_) => confine_exe(),
        _ => None,
    };
    let share_net = match net {
        Net::Off => false,
        Net::Proxy(_) => limiter.is_some(),
        Net::Open => true,
    };
    let mut a: Vec<String> = vec!["--unshare-all".into()];
    if share_net {
        a.push("--share-net".into());
    }
    a.extend(
        [
            "--die-with-parent",
            "--new-session",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/run",
            "--setenv",
            "TMPDIR",
            "/tmp",
            "--bind",
            &cwd,
            &cwd,
        ]
        .map(str::to_string),
    );
    for dir in extra {
        let dir = dir.to_string_lossy().into_owned();
        a.extend(["--bind".to_string(), dir.clone(), dir]);
    }
    for (p, exists) in &protected {
        if *exists {
            a.extend(["--ro-bind".to_string(), p.clone(), p.clone()]);
        } else {
            a.extend([
                "--tmpfs".to_string(),
                p.clone(),
                "--remount-ro".to_string(),
                p.clone(),
            ]);
        }
    }
    // Credential stores: an empty read-only folder over each folder, the
    // empty /dev/null over each file. /tmp and /run are already private,
    // so a path under them (the usual SSH agent socket) is gone anyway and
    // has no mount point to cover.
    for p in hidden {
        if p.starts_with("/tmp") || p.starts_with("/run") {
            continue;
        }
        let s = p.to_string_lossy().into_owned();
        if p.is_dir() {
            a.extend([
                "--tmpfs".to_string(),
                s.clone(),
                "--remount-ro".to_string(),
                s,
            ]);
        } else {
            a.extend(["--ro-bind".to_string(), "/dev/null".to_string(), s]);
        }
    }
    a.extend(["--chdir", &cwd, "--"].map(str::to_string));
    // Out only through the proxy: bwn itself, bound in where no hidden path
    // can cover it, limits the network and then runs the command.
    if let Net::Proxy(port) = net {
        if let Some(exe) = limiter {
            let at = a.len() - 3;
            a.splice(
                at..at,
                [
                    "--ro-bind".to_string(),
                    exe.to_string_lossy().into_owned(),
                    CONFINE_EXE.to_string(),
                ],
            );
            a.extend([
                CONFINE_EXE.to_string(),
                "__confine-net".into(),
                port.to_string(),
                "--".into(),
            ]);
        }
    }
    a.extend(["sh", "-c", cmd].map(str::to_string));
    a
}

// bwn's own binary; tests point at the built one (a test binary does not
// handle `__confine-net`).
fn confine_exe() -> Option<PathBuf> {
    #[cfg(test)]
    {
        // A test binary would take `__confine-net` for a test filter and
        // run the suite: use the built bwn binary, found in an ancestor of
        // the test binary, or leave the network unlimited.
        // Set and empty: no binary to limit the network with.
        if let Some(p) = std::env::var_os("BWN_TEST_CONFINE_EXE") {
            return (!p.is_empty()).then(|| PathBuf::from(p));
        }
        std::env::current_exe().ok().and_then(|t| {
            t.ancestors()
                .map(|a| a.join("buildwithnexus"))
                .find(|p| p.is_file())
        })
    }
    #[cfg(not(test))]
    std::env::current_exe().ok()
}

/// Where bwn's own binary is bound inside bubblewrap to limit the network.
const CONFINE_EXE: &str = "/run/bwn-confine";

/// Workspace entries a sandboxed command may read but never write.
const PROTECTED_IN_WORKSPACE: &[&str] = &[".git", ".buildwithnexus"];

// How far the search for nested repositories goes: at most this many
// folders read, and this many `.git` entries kept, per root. Every sandboxed
// command pays for the search, so it is bounded; it runs breadth-first, so
// shallow repositories are found before the budget is spent.
const NESTED_GIT_MAX: usize = 64;
const NESTED_GIT_DIRS: usize = 10_000;

/// The `.git` entries (folders, or a submodule's `.git` file) of
/// repositories nested inside `root`, besides its own. A command that could
/// write one could plant a hook or a `core.fsmonitor` that runs, unsandboxed,
/// the next time anyone uses git in that folder; so they are kept read-only
/// like the top-level `.git`. Only entries that exist now are found: a
/// sandboxed `git clone` into the workspace still works. Symlinks are never
/// followed, and `node_modules` and `target` (dependency and build output)
/// are not searched.
pub fn nested_git_entries(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut queue = std::collections::VecDeque::from([root.to_path_buf()]);
    let mut read = 0;
    while let Some(dir) = queue.pop_front() {
        if read >= NESTED_GIT_DIRS {
            break;
        }
        read += 1;
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(t) = e.file_type() else {
                continue;
            };
            if t.is_symlink() {
                continue;
            }
            let name = e.file_name();
            if name == ".git" {
                if dir != root {
                    found.push(e.path());
                    if found.len() >= NESTED_GIT_MAX {
                        return found;
                    }
                }
                continue; // a repository's own store holds no repositories
            }
            if t.is_dir() && name != "node_modules" && name != "target" {
                queue.push_back(e.path());
            }
        }
    }
    found
}

/// Environment a sandboxed child never inherits: session buses and agent
/// sockets reach outside the sandbox, and provider keys and other credentials
/// (see `is_credential`) are not the command's to have.
fn scrubbed_env_keeping<I: IntoIterator<Item = String>>(
    names: I,
    passthrough: &[String],
) -> Vec<String> {
    names
        .into_iter()
        .filter(|n| {
            matches!(n.as_str(), "DBUS_SESSION_BUS_ADDRESS" | "SSH_AUTH_SOCK")
                || (is_credential(n) && !passthrough.iter().any(|p| p == n))
        })
        .collect()
}

/// Suffixes of the variable names treated as credentials (compared to the
/// upper-cased name): `*_API_KEY`, `*_API_TOKEN`, `*_TOKEN` (GITHUB_TOKEN,
/// NPM_TOKEN), `*_SECRET`, `*_SECRETS` and `*_PASSWORD`.
pub const CREDENTIAL_SUFFIXES: &[&str] = &[
    "_API_KEY",
    "_API_TOKEN",
    "_TOKEN",
    "_SECRET",
    "_SECRETS",
    "_PASSWORD",
];

/// A credential a command the agent runs does not inherit: a name ending in
/// one of `CREDENTIAL_SUFFIXES`, any `AWS_*` variable (AWS_SECRET_ACCESS_KEY,
/// AWS_SESSION_TOKEN, AWS_PROFILE, …), or a provider preset's key variable
/// (`HF_TOKEN`). Windows names are case-insensitive. A name the user lists in
/// `shell_env_passthrough` is kept (see `credential_env`).
fn is_credential(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    CREDENTIAL_SUFFIXES.iter().any(|s| upper.ends_with(s))
        || upper.starts_with("AWS_")
        || crate::config::PRESETS
            .iter()
            .any(|p| !p.env_key.is_empty() && p.env_key == upper)
}

/// The provider credentials in `names` that a command the agent runs must
/// not inherit, sandboxed or not, minus those the user lets through with
/// `shell_env_passthrough`. Hooks are the user's own and keep everything.
pub fn credential_env<I: IntoIterator<Item = String>>(
    names: I,
    passthrough: &[String],
) -> Vec<String> {
    names
        .into_iter()
        .filter(|n| is_credential(n) && !passthrough.iter().any(|p| p == n))
        .collect()
}

fn passthrough() -> Vec<String> {
    crate::config::load_settings()
        .map(|s| s.shell_env_passthrough)
        .unwrap_or_default()
}

fn env_names() -> impl Iterator<Item = String> {
    std::env::vars_os().filter_map(|(k, _)| k.into_string().ok())
}

/// Marks every descriptor above 2 that bwn inherited close-on-exec, so no
/// command, hook, MCP server or tmux session it starts gets them. A CI
/// runner hands its control pipe down to the job's processes, and a command
/// that wrote to it would end the job. Runs first in `run`.
#[cfg(unix)]
pub fn cloexec_inherited_fds() {
    let fds: Vec<i32> = ["/proc/self/fd", "/dev/fd"]
        .iter()
        .find_map(|d| std::fs::read_dir(d).ok())
        .map(|dir| {
            dir.flatten()
                .filter_map(|e| e.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_else(|| (3..1024).collect());
    for fd in fds.into_iter().filter(|fd| *fd > 2) {
        // SAFETY: fcntl on a descriptor number; an unused one fails with
        // EBADF and is skipped.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

/// Moves the provider keys bwn was started with to the heap and blanks the
/// originals, which `/proc/<pid>/environ` (Linux) and `ps eww` (macOS) read:
/// a command or tool that reads bwn's own environment finds `****`, while
/// `std::env::var` still returns the key. Runs first in `run`, before any
/// thread starts.
#[cfg(unix)]
pub fn hide_startup_credentials() {
    extern "C" {
        static mut environ: *const *mut libc::c_char;
    }
    let saved: Vec<(String, std::ffi::OsString)> = credential_env(env_names(), &[])
        .into_iter()
        .filter_map(|n| std::env::var_os(&n).map(|v| (n, v)))
        .collect();
    if saved.is_empty() {
        return;
    }
    // SAFETY: single-threaded at this point; each entry is a NUL-terminated
    // `NAME=value` string the process owns, overwritten within its length.
    unsafe {
        let mut p = environ;
        while !p.is_null() && !(*p).is_null() {
            let entry = *p;
            let bytes = std::ffi::CStr::from_ptr(entry).to_bytes();
            if let Some(eq) = bytes.iter().position(|b| *b == b'=') {
                if saved.iter().any(|(n, _)| n.as_bytes() == &bytes[..eq]) {
                    for i in eq + 1..bytes.len() {
                        *entry.add(i) = b'*' as libc::c_char;
                    }
                }
            }
            p = p.add(1);
        }
    }
    // Unset first: BSD setenv may copy a value back over the old one in
    // place; a fresh setenv always allocates.
    for (n, v) in saved {
        std::env::remove_var(&n);
        std::env::set_var(n, v);
    }
}

/// Removes provider keys from a command the agent is about to run without
/// the sandbox, so an approved `env` or `printenv` cannot hand the key to the
/// model.
pub fn scrub_credentials(c: &mut Command) {
    for k in credential_env(env_names(), &passthrough()) {
        c.env_remove(k);
    }
}

/// The provider credentials a command started in a tmux session must unset
/// itself: a tmux server that is already running (bwn started inside tmux)
/// gives a new session the environment it was started with, not this one,
/// so the names in `server_env` count too. Only valid shell names.
pub fn credentials_to_unset<I: IntoIterator<Item = String>>(server_env: I) -> Vec<String> {
    let mut names = credential_env(env_names().chain(server_env), &passthrough());
    names.retain(|n| {
        !n.is_empty()
            && !n.starts_with(|c: char| c.is_ascii_digit())
            && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    });
    names.sort();
    names.dedup();
    names
}

fn sbpl_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    let mut q = String::with_capacity(s.len() + 2);
    q.push('"');
    for ch in s.chars() {
        if ch == '"' || ch == '\\' {
            q.push('\\');
        }
        q.push(ch);
    }
    q.push('"');
    q
}

/// Seatbelt profile: allow everything, deny writes, re-allow them under the
/// workspace and the temp dirs. Callers pass realpath'd paths so `/var/…`
/// and `/private/var/…` agree.
/// Folders added with --add-dir (`extra`) are writable like the workspace.
pub fn seatbelt_profile(
    cwd: &Path,
    extra: &[PathBuf],
    tmpdir: Option<&Path>,
    net: Net,
    hidden: &[PathBuf],
) -> String {
    let mut p =
        String::from("(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*");
    let roots: Vec<&Path> = std::iter::once(cwd)
        .chain(extra.iter().map(PathBuf::as_path))
        .collect();
    for dir in roots
        .iter()
        .copied()
        .chain([Path::new("/private/tmp"), Path::new("/tmp")])
        .chain(tmpdir)
    {
        p.push_str(&format!("\n  (subpath {})", sbpl_quote(dir)));
    }
    p.push_str(")\n(allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\") (regex #\"^/dev/tty\"))\n");
    p.push_str("(deny file-write*");
    for root in &roots {
        for d in PROTECTED_IN_WORKSPACE {
            p.push_str(&format!("\n  (subpath {})", sbpl_quote(&root.join(d))));
        }
        for nested in nested_git_entries(root) {
            p.push_str(&format!("\n  (subpath {})", sbpl_quote(&nested)));
        }
    }
    p.push_str(")\n(deny appleevent-send)\n");
    match net {
        Net::Off => p.push_str("(deny network*)\n"),
        // Out only to the proxy on loopback; listening and accepting stay
        // allowed so a dev server works.
        Net::Proxy(port) => p.push_str(&format!(
            "(deny network*)\n(allow network-outbound (remote ip \"localhost:{port}\"))\n(allow network-bind (local ip \"*:*\"))\n(allow network-inbound (local ip \"*:*\"))\n"
        )),
        Net::Open => {}
    }
    // Credential stores: no reads of a folder's contents or of a file, and
    // no connection to the SSH agent's socket (a connect is not a read).
    if !hidden.is_empty() {
        p.push_str("(deny file-read*");
        for h in hidden {
            let q = sbpl_quote(h);
            if h.is_dir() {
                p.push_str(&format!("\n  (subpath {q})"));
            } else {
                p.push_str(&format!("\n  (literal {q})"));
            }
        }
        p.push_str(")\n(deny network-outbound");
        for h in hidden {
            p.push_str(&format!(
                "\n  (remote unix-socket (path-literal {}))",
                sbpl_quote(h)
            ));
        }
        p.push_str(")\n");
    }
    p
}

fn command_for(backend: Backend, cmd: &str, cwd: &Path) -> Result<Command, String> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut c = match backend {
        Backend::Bwrap => {
            let mut c = Command::new("bwrap");
            c.args(bwrap_args(cmd, &cwd, net_for(backend)));
            c
        }
        Backend::SandboxExec => {
            let tmp = std::env::var_os("TMPDIR")
                .map(std::path::PathBuf::from)
                .and_then(|t| t.canonicalize().ok());
            let mut c = Command::new("sandbox-exec");
            let extra = crate::workdirs::list();
            let roots: Vec<&Path> = std::iter::once(cwd.as_path())
                .chain(extra.iter().map(PathBuf::as_path))
                .collect();
            let hidden = hidden_paths(&roots);
            c.args([
                "-p",
                &seatbelt_profile(&cwd, &extra, tmp.as_deref(), net_for(backend), &hidden),
            ]);
            c.args(["sh", "-c", cmd]);
            c
        }
        Backend::Landlock => {
            #[cfg(target_os = "linux")]
            {
                crate::landlock::command(cmd, &cwd, &crate::workdirs::list(), net_for(backend))?
            }
            #[cfg(not(target_os = "linux"))]
            {
                return Err("landlock is only available on Linux".into());
            }
        }
    };
    c.current_dir(&cwd);
    for k in scrubbed_env_keeping(env_names(), &passthrough()) {
        c.env_remove(k);
    }
    if let Net::Proxy(port) = net_for(backend) {
        for k in crate::egress::UNSET_ENV {
            c.env_remove(k);
        }
        for (k, v) in crate::egress::proxy_env(port) {
            c.env(k, v);
        }
    }
    Ok(c)
}

/// Build the sandboxed command for `cmd`, per the session policy.
/// `Ok(None)` = run it unsandboxed (mode off, or auto without a backend —
/// the latter says so once per session); `Err` = `require` with no backend.
pub fn wrap(cmd: &str, cwd: &Path) -> Result<Option<Command>, String> {
    wrap_in(mode(), cmd, cwd)
}

/// `wrap` under an explicit policy (tests, and callers that already read it).
pub fn wrap_in(mode: Mode, cmd: &str, cwd: &Path) -> Result<Option<Command>, String> {
    if mode == Mode::Off {
        return Ok(None);
    }
    match decide(mode, backend()) {
        Decision::Sandboxed(b) => command_for(b, cmd, cwd).map(Some),
        // D1: refused until a person allows it for this session. The gate
        // asks; this is the enforcement every path goes through.
        Decision::NeedsException if !unconfined_allowed() => Err(format!(
            "sandbox: refusing to run the command — no sandbox backend works here ({}), and commands run unconfined only when the user allows it for this session. Changing that is the user's decision.",
            backend_error().unwrap_or("no backend")
        )),
        Decision::Unsandboxed | Decision::NeedsException => {
            if !NOTICED.swap(true, Ordering::Relaxed) {
                let msg = format!(
                    "  sandbox: {} — running commands unsandboxed (sandbox=auto, allowed for this session)",
                    backend_error().unwrap_or("no backend")
                );
                if crate::report::mode() == crate::report::Mode::Json {
                    eprintln!("{}", msg.trim_start());
                } else {
                    crate::tui::line(&crate::tui::dim(&msg));
                }
            }
            Ok(None)
        }
        Decision::Refused => Err(format!(
            "sandbox: refusing to run the command — sandbox mode is `require` but no backend is usable ({}). On Linux, bubblewrap or Landlock is enough. Changing the sandbox mode is the user's decision.",
            backend_error().unwrap_or("no backend")
        )),
    }
}

/// Appended to a sandboxed command's output when it died writing `.git`.
pub const GIT_WRITE_BLOCKED: &str = "\
sandboxed commands cannot write .git, so a hook or git config they plant \
cannot run later outside the sandbox. git fetch, git pull, and git commit \
from the shell fail for that reason. bwn still commits its own file edits \
outside the sandbox when auto_commit is on. Tell the user which git step was \
refused and leave it to them; changing the sandbox is their decision, so do \
not suggest switching it off to get around this.";

/// Why a failed sandboxed command could not update `.git`, when the output
/// says so. `None` when this does not look like that denial.
pub fn git_write_blocked_note(output: &str) -> Option<&'static str> {
    let l = output.to_ascii_lowercase();
    let denied = l.contains("operation not permitted")
        || l.contains("read-only file system")
        || l.contains("readonly file system")
        || l.contains("permission denied");
    if denied && l.contains(".git") {
        Some(GIT_WRITE_BLOCKED)
    } else {
        None
    }
}

/// Whether sandboxed commands are kept from writing `.git` and
/// `.buildwithnexus`, where a planted hook or setting would run later outside
/// the sandbox. Bubblewrap and Seatbelt keep them read-only; Landlock can't
/// carve them out of the writable workspace (roadmap 1.8; analysis M1).
pub fn protects_policy_files() -> bool {
    match decide(mode(), backend()) {
        Decision::Sandboxed(Backend::Bwrap | Backend::SandboxExec) => true,
        Decision::Sandboxed(_)
        | Decision::Unsandboxed
        | Decision::Refused
        | Decision::NeedsException => false,
    }
}

/// Whether a command run now, in `cwd`, is kept from reading the credential
/// stores: bubblewrap and Seatbelt always hide them; Landlock does unless
/// the workspace holds one (a session run from the home folder). False when
/// commands run unsandboxed.
pub fn hides_reads(cwd: &Path) -> bool {
    // With no home folder to find the stores in, nothing is hidden: report
    // it, so the posture check warns and auto mode asks (roadmap 1.6).
    if mode() == Mode::Off || crate::config::user_home().is_none() {
        return false;
    }
    match decide(mode(), backend()) {
        Decision::Sandboxed(Backend::Bwrap | Backend::SandboxExec) => true,
        #[cfg(target_os = "linux")]
        Decision::Sandboxed(Backend::Landlock) => {
            let extra = crate::workdirs::list();
            let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
            let roots: Vec<&Path> = std::iter::once(cwd.as_path())
                .chain(extra.iter().map(PathBuf::as_path))
                .collect();
            crate::landlock::can_hide(&roots, &hidden_paths(&roots))
        }
        _ => false,
    }
}

/// True when the next shell command would run confined — drives the
/// `[sandboxed]` marker on the tool header.
pub fn would_confine() -> bool {
    mode() != Mode::Off && matches!(decide(mode(), backend()), Decision::Sandboxed(_))
}

/// Lines for `/sandbox status`.
pub fn status_lines() -> Vec<String> {
    let mode = mode();
    let mut v = vec![format!("sandbox: {}", mode.as_str())];
    v.push(match probe() {
        Ok(b) => format!("backend: {} — available", b.label()),
        Err(e) => format!("backend: none — {e}"),
    });
    if matches!(probe(), Ok(Backend::Landlock)) {
        v.push(
            "landlock cannot keep .git read-only inside the workspace; bubblewrap can, and is used when it works"
                .into(),
        );
    }
    v.push(format!(
        "network: {}",
        if network() {
            "allowed inside the sandbox (sandbox_network: true)"
        } else {
            "blocked inside the sandbox (sandbox_network: false)"
        }
    ));
    v.push(
        match decide(mode, backend()) {
            Decision::Sandboxed(_) => {
                "commands: confined — writes outside the workspace and temp dirs fail"
            }
            Decision::NeedsException if unconfined_allowed() => {
                "commands: NOT confined — you allowed unconfined commands for this session"
            }
            Decision::NeedsException => {
                "commands: refused until you allow them to run unconfined for this session"
            }
            Decision::Unsandboxed => "commands: not confined (sandbox off)",
            Decision::Refused => "commands: REFUSED — require mode with no usable backend",
        }
        .to_string(),
    );
    v.push(
        "scope: run_command/bash/check_work/start_server/python_tool; reads, file tools, and hooks are never sandboxed"
            .to_string(),
    );
    v
}

/// One `doctor` row: (glyph, text).
pub fn doctor_summary() -> (&'static str, String) {
    let mode = mode();
    match (decide(mode, backend()), probe()) {
        (Decision::Sandboxed(b), _) => (
            "✓",
            format!(
                "mode={} backend={} — commands confined",
                mode.as_str(),
                b.label()
            ),
        ),
        (Decision::Refused, Err(e)) => (
            "✗",
            format!("mode=require but no backend: {e} — commands will be refused"),
        ),
        (_, Ok(b)) => (
            "·",
            format!(
                "mode=off — commands unconfined ({} available; `/sandbox auto` to enable)",
                b.label()
            ),
        ),
        (_, Err(e)) if mode == Mode::Auto && unconfined_allowed() => (
            "⚠",
            format!("mode=auto but no backend ({e}) — commands run unconfined (allowed for this session)"),
        ),
        (_, Err(e)) if mode == Mode::Auto => (
            "⚠",
            format!(
                "mode=auto but no backend ({e}) — commands are refused until you allow them to run unconfined for a session"
            ),
        ),
        (_, Err(e)) => ("·", format!("mode=off — no backend available ({e})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parses_the_three_values_only() {
        assert_eq!(Mode::parse("off"), Some(Mode::Off));
        assert_eq!(Mode::parse(" Auto "), Some(Mode::Auto));
        assert_eq!(Mode::parse("REQUIRE"), Some(Mode::Require));
        assert_eq!(Mode::parse("on"), None);
        assert_eq!(Mode::parse(""), None);
        for m in [Mode::Off, Mode::Auto, Mode::Require] {
            assert_eq!(Mode::parse(m.as_str()), Some(m));
        }
    }

    #[test]
    fn policy_matrix() {
        let b = Some(Backend::Bwrap);
        assert_eq!(decide(Mode::Off, b), Decision::Unsandboxed);
        assert_eq!(decide(Mode::Off, None), Decision::Unsandboxed);
        assert_eq!(decide(Mode::Auto, b), Decision::Sandboxed(Backend::Bwrap));
        assert_eq!(decide(Mode::Auto, None), Decision::NeedsException);
        assert_eq!(
            decide(Mode::Require, Some(Backend::SandboxExec)),
            Decision::Sandboxed(Backend::SandboxExec)
        );
        assert_eq!(decide(Mode::Require, None), Decision::Refused);
        let note = git_write_blocked_note(
            "fatal: could not open '.git/FETCH_HEAD': Operation not permitted",
        );
        // Roadmap 1.9 (analysis A4, Authority invariant): the note tells the
        // model what happened, never how to switch the sandbox off.
        let note = note.unwrap();
        assert!(note.contains("auto_commit"));
        assert!(note.contains("user"));
        for nudge in ["sandbox off", "--sandbox", "\"off\"", "/sandbox"] {
            assert!(!note.contains(nudge), "{nudge}: {note}");
        }
        assert!(git_write_blocked_note("Read-only file system: .git/refs/heads/main").is_some());
        assert!(git_write_blocked_note("cargo: permission denied").is_none());
        assert!(git_write_blocked_note("updated .git/refs/heads/main").is_none());
        assert_eq!(
            decide(Mode::Auto, Some(Backend::Landlock)),
            Decision::Sandboxed(Backend::Landlock)
        );
    }

    // bwrap's argv is a Linux path list. On Windows `Path` renders `\`, so
    // these fixtures only run where bwrap itself runs.
    // D1 (roadmap 1.6): with sandbox auto and no backend, the execution
    // point refuses every command until a person allows unconfined commands
    // for the session, whatever approved it upstream (a hook's allow, a
    // saved approval, an allow rule). The refusal names no way around it.
    // Runs in the no-backend cells, Windows included.
    #[test]
    fn invariant_d1_wrap_refuses_commands_until_the_person_allows_them() {
        if backend().is_some() {
            return;
        }
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let m = mode();
        set_mode(Mode::Auto);
        forget_unconfined_allowed();
        let cwd = std::env::temp_dir();
        assert!(needs_exception());
        let err = wrap("echo hi", &cwd).unwrap_err();
        assert!(err.contains("the user allows it for this session"), "{err}");
        assert!(
            !err.contains("--sandbox off") && !err.contains("/sandbox"),
            "{err}"
        );
        allow_unconfined_for_session();
        assert!(!needs_exception());
        assert!(matches!(wrap("echo hi", &cwd), Ok(None)));
        forget_unconfined_allowed();
        set_mode(m);
    }

    // Roadmap 1.6: with no home folder there is no list of stores to hide,
    // so the sandbox must not claim to hide them.
    #[test]
    fn invariant_g3_no_home_folder_means_nothing_is_claimed_hidden() {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (home, profile) = (std::env::var_os("HOME"), std::env::var_os("USERPROFILE"));
        let m = mode();
        set_mode(Mode::Auto);
        std::env::set_var("HOME", "");
        std::env::set_var("USERPROFILE", "");
        let hides = hides_reads(&std::env::temp_dir());
        let listed = hidden_paths(&[&std::env::temp_dir()]);
        for (k, v) in [("HOME", home), ("USERPROFILE", profile)] {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        set_mode(m);
        assert!(listed.is_empty());
        assert!(!hides, "claimed to hide stores it could not find");
    }

    // Roadmap 1.6 (analysis M8(b)): when bwn can't find its own binary to
    // hold a command to the proxy, the command gets no network, not the
    // host's.
    #[cfg(unix)]
    #[test]
    fn invariant_m8b_no_network_limiter_means_no_network() {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("BWN_TEST_CONFINE_EXE", "");
        let a = bwrap_argv(
            "curl x",
            Path::new("/work/proj"),
            &[],
            Net::Proxy(8080),
            true,
            &[],
        );
        std::env::remove_var("BWN_TEST_CONFINE_EXE");
        assert!(!a.contains(&"--share-net".to_string()), "{a:?}");
        assert!(!a.contains(&"__confine-net".to_string()));
        std::env::set_var("BWN_TEST_CONFINE_EXE", "/usr/bin/true");
        let a = bwrap_argv(
            "curl x",
            Path::new("/work/proj"),
            &[],
            Net::Proxy(8080),
            true,
            &[],
        );
        std::env::remove_var("BWN_TEST_CONFINE_EXE");
        assert!(a.contains(&"--share-net".to_string()));
        assert!(a.contains(&"__confine-net".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn bwrap_argv_binds_only_the_workspace_rw() {
        // No hidden paths here: those depend on this machine's home folder
        // and have tests of their own below.
        let a = bwrap_argv(
            "echo hi",
            Path::new("/work/proj"),
            &[],
            Net::Open,
            true,
            &[],
        );
        let s = a.join(" ");
        assert!(s.starts_with("--unshare-all --share-net --die-with-parent --new-session "));
        assert!(s.contains("--ro-bind / /"));
        assert!(s.contains("--tmpfs /tmp"));
        assert!(s.contains("--bind /work/proj /work/proj --tmpfs /work/proj/.git"));
        assert!(s.contains("--remount-ro /work/proj/.buildwithnexus --chdir /work/proj"));
        // Exactly one read-write bind, and it is the workspace.
        assert_eq!(a.iter().filter(|x| *x == "--bind").count(), 1);
        assert_eq!(&a[a.len() - 4..], ["--", "sh", "-c", "echo hi"]);
        // Network off drops --share-net and nothing else.
        let off = bwrap_argv("echo hi", Path::new("/work/proj"), &[], Net::Off, true, &[]);
        assert!(!off.contains(&"--share-net".to_string()));
        assert_eq!(off.len(), a.len() - 1);
    }

    #[cfg(unix)]
    #[test]
    fn added_folders_are_bound_writable_with_their_git_read_only() {
        let extra = [PathBuf::from("/work/lib"), PathBuf::from("/srv/data")];
        let a = bwrap_argv("true", Path::new("/work/proj"), &extra, Net::Off, true, &[]);
        let s = a.join(" ");
        assert_eq!(a.iter().filter(|x| *x == "--bind").count(), 3, "{s}");
        let at = |needle: &str| {
            s.find(needle)
                .unwrap_or_else(|| panic!("{needle} missing: {s}"))
        };
        for d in ["/work/lib", "/srv/data"] {
            let bind = at(&format!("--bind {d} {d}"));
            // Their own .git and .buildwithnexus stay read-only, after the bind.
            assert!(bind < at(&format!("--tmpfs {d}/.git --remount-ro {d}/.git")));
            assert!(bind < at(&format!("--remount-ro {d}/.buildwithnexus")));
        }
        assert!(at("--chdir /work/proj") > at("/srv/data/.buildwithnexus"));
        let p = seatbelt_profile(Path::new("/w"), &extra, None, Net::Off, &[]);
        let (allow, deny) = p.split_once("(deny file-write*\n").unwrap();
        assert!(
            allow.contains("(subpath \"/work/lib\")") && allow.contains("(subpath \"/srv/data\")")
        );
        assert!(deny.contains("(subpath \"/work/lib/.git\")"), "{p}");
        assert!(
            deny.contains("(subpath \"/srv/data/.buildwithnexus\")"),
            "{p}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn nested_repositories_stay_read_only_too() {
        let ws = std::env::temp_dir().join(format!("bwn-sandbox-nested-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        for d in [
            ".git/modules/x",
            "vendor/lib/.git/hooks",
            "a/b/c/.git",
            "node_modules/dep/.git",
            "target/x/.git",
        ] {
            std::fs::create_dir_all(ws.join(d)).unwrap();
        }
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::write(ws.join("sub/.git"), "gitdir: ../.git/modules/x\n").unwrap();
        let mut found = nested_git_entries(&ws);
        found.sort();
        let want: Vec<PathBuf> = ["a/b/c/.git", "sub/.git", "vendor/lib/.git"]
            .iter()
            .map(|p| ws.join(p))
            .collect();
        assert_eq!(found, want);
        let s = bwrap_args("true", &ws, Net::Off).join(" ");
        let rw = s
            .find(&format!("--bind {w} {w} ", w = ws.display()))
            .unwrap();
        for p in &want {
            let bind = format!("--ro-bind {p} {p}", p = p.display());
            assert!(s.find(&bind).is_some_and(|at| at > rw), "{bind}: {s}");
        }
        let profile = seatbelt_profile(&ws, &[], None, Net::Open, &[]);
        let deny = profile.split_once("(deny file-write*\n").unwrap().1;
        for p in &want {
            assert!(deny.contains(&sbpl_quote(p)), "{p:?}: {profile}");
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[cfg(unix)]
    #[test]
    fn bwrap_argv_keeps_git_and_settings_read_only() {
        let ws = std::env::temp_dir().join(format!("bwn-sandbox-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::create_dir_all(ws.join(".buildwithnexus")).unwrap();
        let a = bwrap_args("true", &ws, Net::Off);
        let s = a.join(" ");
        let w = ws.display();
        assert!(s.contains("--tmpfs /run"), "{s}");
        // After the read-write workspace bind, so they win.
        let rw = format!("--bind {w} {w} ");
        let git = format!("--ro-bind {w}/.git {w}/.git");
        let bwn = format!("--ro-bind {w}/.buildwithnexus {w}/.buildwithnexus");
        let at = |needle: &str| {
            s.find(needle)
                .unwrap_or_else(|| panic!("{needle} missing: {s}"))
        };
        assert!(at(&rw) < at(&git) && at(&rw) < at(&bwn));
        assert!(at(&git) < at("--chdir"));
        let _ = std::fs::remove_dir_all(&ws);
        // Absent entries get a read-only tmpfs instead, so they cannot be
        // created from inside the sandbox.
        let absent = bwrap_args("true", Path::new("/work/none"), Net::Open).join(" ");
        assert!(
            absent.contains("--tmpfs /work/none/.git --remount-ro /work/none/.git"),
            "{absent}"
        );
        assert!(absent.contains(
            "--tmpfs /work/none/.buildwithnexus --remount-ro /work/none/.buildwithnexus"
        ));
    }

    #[test]
    fn sandboxed_children_lose_sockets_and_provider_keys() {
        let names = [
            "PATH",
            "HOME",
            "DBUS_SESSION_BUS_ADDRESS",
            "SSH_AUTH_SOCK",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "HF_TOKEN",
        ]
        .map(String::from);
        assert_eq!(
            scrubbed_env_keeping(names, &[]),
            [
                "DBUS_SESSION_BUS_ADDRESS",
                "SSH_AUTH_SOCK",
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "HF_TOKEN"
            ]
        );
    }

    #[test]
    fn agent_commands_lose_provider_keys_unless_passed_through() {
        let names = [
            "PATH",
            "ANTHROPIC_API_KEY",
            "custom_api_key",
            "HF_TOKEN",
            "GROQ_API_KEY",
            "SOME_API_TOKEN",
            "SSH_AUTH_SOCK",
        ]
        .map(String::from);
        assert_eq!(
            credential_env(names.clone(), &[]),
            [
                "ANTHROPIC_API_KEY",
                "custom_api_key",
                "HF_TOKEN",
                "GROQ_API_KEY",
                "SOME_API_TOKEN"
            ]
        );
        // A build that needs one names it in shell_env_passthrough.
        let keep = ["GROQ_API_KEY".to_string()];
        assert!(!credential_env(names.clone(), &keep).contains(&"GROQ_API_KEY".to_string()));
        assert!(!scrubbed_env_keeping(names, &keep).contains(&"GROQ_API_KEY".to_string()));
        // What a running tmux server holds is unset too; a name that is not
        // a shell name never reaches the command line.
        let unset = credentials_to_unset(
            ["TMUXONLY_API_KEY", "PATH", "BAD;NAME_API_KEY", "1X_API_KEY"].map(String::from),
        );
        assert!(unset.contains(&"TMUXONLY_API_KEY".to_string()), "{unset:?}");
        assert!(unset
            .iter()
            .all(|n| n != "PATH" && !n.contains(';') && !n.starts_with('1')));
    }

    #[test]
    fn agent_commands_lose_tokens_secrets_passwords_and_aws_variables() {
        let stripped = [
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "NPM_TOKEN",
            "CI_JOB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_ACCESS_KEY_ID",
            "AWS_PROFILE",
            "aws_secret_access_key",
            "DB_PASSWORD",
            "POSTGRES_PASSWORD",
            "CLIENT_SECRET",
            "APP_SECRETS",
            "github_token",
            "SOME_API_KEY",
            "SOME_API_TOKEN",
            "HF_TOKEN",
        ];
        let kept = [
            "PATH",
            "HOME",
            "SHELL",
            "USER",
            "LANG",
            "TERM",
            "EDITOR",
            "SSH_AUTH_SOCK",
            "TOKENIZERS_PARALLELISM",
            "PASSWORD_STORE_DIR",
            "SECRET_SANTA",
            "AWSOME_TOOL",
            "XDG_RUNTIME_DIR",
            "CARGO_HOME",
            "RUST_LOG",
        ];
        let names = || stripped.iter().chain(&kept).map(|s| s.to_string());
        assert_eq!(credential_env(names(), &[]), stripped);
        // The sandbox strips the same names, plus the sockets.
        let sandboxed = scrubbed_env_keeping(names(), &[]);
        for n in stripped {
            assert!(sandboxed.contains(&n.to_string()), "{n}");
        }
        assert!(sandboxed.contains(&"SSH_AUTH_SOCK".to_string()));
        for n in kept.iter().filter(|n| **n != "SSH_AUTH_SOCK") {
            assert!(!sandboxed.contains(&n.to_string()), "{n}");
        }
        // shell_env_passthrough keeps what the user lists, and only that.
        let keep = ["GITHUB_TOKEN".to_string(), "AWS_PROFILE".to_string()];
        let left = credential_env(names(), &keep);
        assert!(!left.contains(&"GITHUB_TOKEN".to_string()));
        assert!(!left.contains(&"AWS_PROFILE".to_string()));
        assert!(left.contains(&"GH_TOKEN".to_string()));
        assert!(left.contains(&"AWS_SESSION_TOKEN".to_string()));
        let left = scrubbed_env_keeping(names(), &keep);
        assert!(!left.contains(&"GITHUB_TOKEN".to_string()));
        assert!(left.contains(&"NPM_TOKEN".to_string()));
        // A tmux session's own `env -u` list follows the same rule.
        let unset = credentials_to_unset(["TMUX_ONLY_TOKEN".to_string()]);
        assert!(unset.contains(&"TMUX_ONLY_TOKEN".to_string()), "{unset:?}");
    }

    #[cfg(unix)]
    #[test]
    fn seatbelt_profile_denies_writes_except_workspace_and_tmp() {
        let p = seatbelt_profile(
            Path::new("/private/var/w/my \"proj\""),
            &[],
            Some(Path::new("/private/var/folders/xy/T")),
            Net::Open,
            &[],
        );
        assert!(p.starts_with("(version 1)\n(allow default)\n(deny file-write*)\n"));
        assert!(p.contains("(subpath \"/private/var/w/my \\\"proj\\\"\")"));
        assert!(p.contains("(subpath \"/private/tmp\")"));
        assert!(p.contains("(subpath \"/tmp\")"));
        assert!(p.contains("(subpath \"/private/var/folders/xy/T\")"));
        assert!(p.contains("(literal \"/dev/null\")"));
        assert!(
            p.contains("(deny file-write*\n  (subpath \"/private/var/w/my \\\"proj\\\"/.git\")")
        );
        assert!(p.contains("(subpath \"/private/var/w/my \\\"proj\\\"/.buildwithnexus\")"));
        assert!(p.contains("(deny appleevent-send)"));
        assert!(!p.contains("network"));
        let no_net = seatbelt_profile(Path::new("/w"), &[], None, Net::Off, &[]);
        assert!(no_net.ends_with("(deny network*)\n"));
        assert!(!no_net.contains("folders"));
    }

    #[test]
    fn invariant_in2_a_bad_sandbox_setting_falls_back_to_require() {
        // Roadmap 1.3 (analysis M13, IN-2): a value bwn can't parse never
        // leaves commands less confined than the strictest mode.
        let (m, n) = (mode(), network());
        set_mode(Mode::Off);
        let err = configure_setting("bogus", false).unwrap_err();
        assert!(err.contains("using require"), "{err}");
        assert_eq!(mode(), Mode::Require);
        assert!(configure_setting("auto", true).is_ok());
        assert_eq!(mode(), Mode::Auto);
        set_mode(m);
        set_network(n);
    }

    #[test]
    fn configure_rejects_unknown_modes_without_changing_state() {
        // Session state is process-global; leave it exactly as found.
        let (m, n) = (mode(), network());
        assert!(configure("sometimes", true).is_err());
        assert_eq!(mode(), m);
        assert_eq!(network(), n);
    }

    // Real bubblewrap: a sandboxed command can write inside the workspace but
    // not outside it. Skips (loudly) when bwrap is missing or cannot create
    // namespaces on this kernel.
    #[test]
    fn bwrap_confines_writes_to_the_workspace() {
        if !cfg!(target_os = "linux") {
            eprintln!("skip: bwrap integration test is Linux-only");
            return;
        }
        let probe = Command::new("bwrap")
            .args(bwrap_probe_args())
            .stdin(Stdio::null())
            .output();
        match probe {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                eprintln!("skip: bwrap unusable here: {}", first_line(&o.stderr));
                return;
            }
            Err(e) => {
                eprintln!("skip: bwrap not runnable: {e}");
                return;
            }
        }
        let id = std::process::id();
        let ws = std::env::temp_dir().join(format!("bwn-sandbox-ws-{id}"));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        // Outside the workspace AND outside /tmp (which the sandbox replaces
        // with a private tmpfs): the home directory, normally writable.
        let outside_dir = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .filter(|h| h.is_dir())
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        let outside = outside_dir.join(format!("bwn-sandbox-escape-{id}.txt"));
        let _ = std::fs::remove_file(&outside);
        let run = |cmd: &str| {
            let mut c = command_for(Backend::Bwrap, cmd, &ws).unwrap();
            c.stdin(Stdio::null()).output().unwrap()
        };

        let inside = run("echo ok > inside.txt");
        assert!(inside.status.success(), "{inside:?}");
        assert_eq!(
            std::fs::read_to_string(ws.join("inside.txt")).unwrap(),
            "ok\n"
        );

        let escape = run(&format!("echo pwned > '{}'", outside.display()));
        assert!(
            !escape.status.success(),
            "write outside the workspace must fail: {escape:?}"
        );
        assert!(
            !outside.exists(),
            "{} leaked out of the sandbox",
            outside.display()
        );

        // A folder added with --add-dir is writable; its .git is not.
        let added = std::env::temp_dir().join(format!("bwn-sandbox-added-{id}"));
        let _ = std::fs::remove_dir_all(&added);
        std::fs::create_dir_all(added.join(".git")).unwrap();
        let added = added.canonicalize().unwrap();
        crate::workdirs::clear();
        assert!(matches!(
            crate::workdirs::add(&added.display().to_string(), &ws),
            Ok(crate::workdirs::Added::New(_))
        ));
        let wrote = run(&format!("echo ok > '{}/a.txt'", added.display()));
        assert!(wrote.status.success(), "{wrote:?}");
        assert_eq!(
            std::fs::read_to_string(added.join("a.txt")).unwrap(),
            "ok\n"
        );
        let hook = run(&format!("echo x > '{}/.git/config'", added.display()));
        assert!(!hook.status.success(), "{hook:?}");
        assert!(!added.join(".git/config").exists());
        crate::workdirs::clear();
        let _ = std::fs::remove_dir_all(&added);
        let _ = std::fs::remove_dir_all(&ws);
    }

    // A home folder with credential stores, outside /tmp (bubblewrap
    // replaces /tmp, and Landlock lets commands write and read it).
    #[cfg(target_os = "linux")]
    fn fake_home(tag: &str) -> (PathBuf, PathBuf, Vec<PathBuf>) {
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_dir() && !h.starts_with("/tmp"))
            .unwrap_or_else(|| PathBuf::from("/var/tmp"));
        let home = base.join(format!("bwn-hide-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join(".config/gh")).unwrap();
        std::fs::create_dir_all(home.join("proj")).unwrap();
        std::fs::write(home.join(".ssh/id_test"), "KEY-MATERIAL\n").unwrap();
        std::fs::write(home.join(".config/gh/hosts.yml"), "GH-TOKEN\n").unwrap();
        std::fs::write(home.join(".netrc"), "NETRC\n").unwrap();
        std::fs::write(home.join("notes.txt"), "plain\n").unwrap();
        std::fs::write(home.join(".config/other.toml"), "other\n").unwrap();
        let home = home.canonicalize().unwrap();
        let ws = home.join("proj");
        std::os::unix::fs::symlink(home.join(".ssh"), ws.join("keys")).unwrap();
        let hidden = hidden_paths_in(&home, &home.join(".buildwithnexus"), None, &[], &[&ws]);
        (home, ws, hidden)
    }

    #[cfg(target_os = "linux")]
    fn assert_hidden(run: impl Fn(&str) -> std::process::Output, home: &Path) {
        let ok = |cmd: &str| {
            let o = run(cmd);
            assert!(o.status.success(), "{cmd}: {o:?}");
            String::from_utf8_lossy(&o.stdout).into_owned()
        };
        let denied = |cmd: &str| {
            let o = run(cmd);
            let out = String::from_utf8_lossy(&o.stdout);
            assert!(
                !out.contains("KEY-MATERIAL") && !out.contains("TOKEN") && !out.contains("NETRC"),
                "{cmd} read a hidden file: {o:?}"
            );
        };
        assert_eq!(
            ok(&format!("cat '{}/notes.txt'", home.display())),
            "plain\n"
        );
        assert_eq!(
            ok(&format!("cat '{}/.config/other.toml'", home.display())),
            "other\n"
        );
        assert_eq!(ok("echo x > made.txt && cat made.txt"), "x\n");
        denied(&format!("cat '{}/.ssh/id_test'", home.display()));
        denied("cat keys/id_test");
        denied(&format!("cd '{}' && cat .ssh/id_test", home.display()));
        denied(&format!("cat '{}/.config/gh/hosts.yml'", home.display()));
        denied(&format!("cat '{}/.netrc'", home.display()));
        denied(&format!(
            "cat $(printf '%s/.%s/id_test' '{}' ssh)",
            home.display()
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn hidden_paths_lists_existing_stores_and_spares_the_rest() {
        let (home, ws, hidden) = fake_home("list");
        assert!(hidden.contains(&home.join(".ssh")));
        assert!(hidden.contains(&home.join(".config/gh")));
        assert!(hidden.contains(&home.join(".netrc")));
        assert!(!hidden
            .iter()
            .any(|h| h.ends_with("notes.txt") || h.ends_with(".aws")));
        // Exposed by the user: readable again.
        let exposed = hidden_paths_in(
            &home,
            &home.join(".buildwithnexus"),
            None,
            &[home.join(".ssh")],
            &[&ws],
        );
        assert!(!exposed.contains(&home.join(".ssh")));
        // A workspace inside a store keeps it visible: hiding it would hide
        // the workspace too.
        let inside = hidden_paths_in(
            &home,
            &home.join(".buildwithnexus"),
            None,
            &[],
            &[&home.join(".config/gh")],
        );
        assert!(!inside.contains(&home.join(".config/gh")));
        // The agent socket is hidden wherever it is.
        let sock = home.join("agent.sock");
        std::fs::write(&sock, "").unwrap();
        let with_sock = hidden_paths_in(
            &home,
            &home.join(".buildwithnexus"),
            Some(sock.clone()),
            &[],
            &[&ws],
        );
        assert!(with_sock.contains(&sock));
        let note = hidden_note(
            &format!("cat: {}/.ssh/id_test: Permission denied", home.display()),
            &hidden,
        )
        .unwrap();
        assert!(note.contains("sandbox_expose"), "{note}");
        assert!(hidden_note("cat: x: Permission denied", &hidden).is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    // Real bubblewrap: the stores are empty from inside, however they are
    // reached, and everything else in the home folder still reads.
    #[cfg(target_os = "linux")]
    #[test]
    fn bwrap_hides_credential_stores() {
        let usable = Command::new("bwrap")
            .args(bwrap_probe_args())
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success());
        if !usable {
            eprintln!("skip: bwrap unusable here");
            return;
        }
        let (home, ws, hidden) = fake_home("bwrap");
        let run = |cmd: &str| {
            Command::new("bwrap")
                .args(bwrap_argv(cmd, &ws, &[], Net::Off, true, &hidden))
                .stdin(Stdio::null())
                .output()
                .unwrap()
        };
        assert_hidden(run, &home);
        let _ = std::fs::remove_dir_all(&home);
    }

    // Real Landlock: the same, through the read rules carved around the
    // stores; and a workspace that holds a store cannot hide it.
    #[cfg(target_os = "linux")]
    #[test]
    fn landlock_hides_credential_stores() {
        use std::os::unix::process::CommandExt;
        if crate::landlock::available().is_err() {
            eprintln!("skip: Landlock unavailable here");
            return;
        }
        let (home, ws, hidden) = fake_home("landlock");
        assert!(crate::landlock::can_hide(&[&ws], &hidden));
        assert!(!crate::landlock::can_hide(&[&home], &hidden));
        let run = |cmd: &str| {
            let fd = crate::landlock::ruleset_with(&ws, &[], None, &hidden, Net::Off).unwrap();
            let mut c = Command::new("sh");
            c.args(["-c", cmd]).current_dir(&ws).stdin(Stdio::null());
            unsafe {
                c.pre_exec(move || crate::landlock::engage(fd, crate::landlock::Net::Off));
            }
            let o = c.output().unwrap();
            unsafe { libc::close(fd) };
            o
        };
        assert_hidden(run, &home);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn seatbelt_profile_denies_reads_of_hidden_paths() {
        let dir = std::env::temp_dir().join(format!("bwn-sb-hidden-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".ssh")).unwrap();
        std::fs::write(dir.join(".netrc"), "x").unwrap();
        let hidden = [dir.join(".ssh"), dir.join(".netrc")];
        let p = seatbelt_profile(Path::new("/w"), &[], None, Net::Open, &hidden);
        let reads = p.split_once("(deny file-read*").unwrap().1;
        assert!(
            reads.contains(&format!("(subpath {})", sbpl_quote(&hidden[0]))),
            "{p}"
        );
        assert!(
            reads.contains(&format!("(literal {})", sbpl_quote(&hidden[1]))),
            "{p}"
        );
        assert!(p.contains("(deny network-outbound"), "{p}");
        assert!(p.contains(&format!(
            "(remote unix-socket (path-literal {}))",
            sbpl_quote(&hidden[0])
        )));
        // Nothing hidden, nothing added.
        assert!(
            !seatbelt_profile(Path::new("/w"), &[], None, Net::Open, &[]).contains("file-read")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Real Seatbelt (macOS runners): a hidden folder and file cannot be
    // read, a hidden socket cannot be connected to, and the rest reads.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_hides_credential_stores() {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        let base = home.join(format!("bwn-sb-real-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join(".ssh")).unwrap();
        std::fs::create_dir_all(base.join("proj")).unwrap();
        std::fs::write(base.join(".ssh/id_test"), "KEY-MATERIAL").unwrap();
        std::fs::write(base.join("notes.txt"), "plain").unwrap();
        let base = base.canonicalize().unwrap();
        let sock = base.join("agent.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let hidden = vec![base.join(".ssh"), sock.clone()];
        let ws = base.join("proj");
        let run = |cmd: &str| {
            Command::new("sandbox-exec")
                .args([
                    "-p",
                    &seatbelt_profile(&ws, &[], None, Net::Off, &hidden),
                    "sh",
                    "-c",
                    cmd,
                ])
                .current_dir(&ws)
                .stdin(Stdio::null())
                .output()
                .unwrap()
        };
        let key = run(&format!("cat '{}/.ssh/id_test'", base.display()));
        assert!(
            !String::from_utf8_lossy(&key.stdout).contains("KEY-MATERIAL"),
            "{key:?}"
        );
        let plain = run(&format!("cat '{}/notes.txt'", base.display()));
        assert_eq!(String::from_utf8_lossy(&plain.stdout), "plain", "{plain:?}");
        let connect = run(&format!("nc -U '{}' </dev/null", sock.display()));
        assert!(
            !connect.status.success(),
            "connected to a hidden socket: {connect:?}"
        );
        drop(listener);
        let _ = std::fs::remove_dir_all(&base);
    }

    // What a confined command can reach, checked by python3 inside it:
    // the proxy's port, nothing else over TCP, no UDP, no Unix sockets.
    #[cfg(target_os = "linux")]
    const NET_PROBE: &str = r#"python3 -c '
import socket, sys
def tcp(port):
    s = socket.socket()
    s.settimeout(2)
    try:
        s.connect(("127.0.0.1", port)); return "ok"
    except Exception as e:
        return type(e).__name__
def kind(fam, typ):
    try:
        socket.socket(fam, typ); return "ok"
    except Exception as e:
        return type(e).__name__
print("proxy", tcp(int(sys.argv[1])))
print("other", tcp(int(sys.argv[2])))
print("udp", kind(socket.AF_INET, socket.SOCK_DGRAM))
print("unix", kind(socket.AF_UNIX, socket.SOCK_STREAM))
' "#;

    #[cfg(target_os = "linux")]
    fn assert_proxy_only(out: &std::process::Output) {
        let s = String::from_utf8_lossy(&out.stdout);
        assert!(s.contains("proxy ok"), "{out:?}");
        assert!(
            !s.contains("other ok"),
            "reached a port that is not the proxy: {out:?}"
        );
        assert!(!s.contains("udp ok"), "{out:?}");
        assert!(!s.contains("unix ok"), "{out:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn landlock_limits_the_network_to_the_proxy() {
        use std::os::unix::process::CommandExt;
        if crate::landlock::available().is_err() || !crate::landlock::limits_connections() {
            eprintln!("skip: Landlock cannot limit connections here");
            return;
        }
        let proxy = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let other = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let (pp, op) = (
            proxy.local_addr().unwrap().port(),
            other.local_addr().unwrap().port(),
        );
        let ws = std::env::temp_dir();
        let net = Net::Proxy(pp);
        let fd = crate::landlock::ruleset_with(&ws, &[], None, &[], net).unwrap();
        let mut c = Command::new("sh");
        c.args(["-c", &format!("{NET_PROBE} {pp} {op}")])
            .stdin(Stdio::null());
        unsafe {
            c.pre_exec(move || crate::landlock::engage(fd, net));
        }
        let out = c.output().unwrap();
        unsafe { libc::close(fd) };
        assert_proxy_only(&out);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bwrap_limits_the_network_to_the_proxy() {
        // BWN_TEST_CONFINE_EXE is process-wide; the M8(b) test sets it too.
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let usable = Command::new("bwrap")
            .args(bwrap_probe_args())
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success());
        // The built bwn binary does the limiting inside bubblewrap.
        // The built binary sits in an ancestor of the test binary (how deep
        // depends on cargo's build-dir layout).
        let exe = std::env::current_exe().ok().and_then(|t| {
            t.ancestors()
                .map(|a| a.join("buildwithnexus"))
                .find(|p| p.is_file())
        });
        if !usable || !crate::landlock::limits_connections() || exe.is_none() {
            eprintln!("skip: bwrap, Landlock ABI 4 or the built binary missing");
            return;
        }
        let proxy = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let other = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let (pp, op) = (
            proxy.local_addr().unwrap().port(),
            other.local_addr().unwrap().port(),
        );
        std::env::set_var("BWN_TEST_CONFINE_EXE", exe.unwrap());
        let ws = std::env::temp_dir().join(format!("bwn-bwrap-net-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let args = bwrap_argv(
            &format!("{NET_PROBE} {pp} {op}"),
            &ws,
            &[],
            Net::Proxy(pp),
            false,
            &[],
        );
        let out = Command::new("bwrap")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        std::env::remove_var("BWN_TEST_CONFINE_EXE");
        let _ = std::fs::remove_dir_all(&ws);
        assert_proxy_only(&out);
    }

    // Real Seatbelt (macOS runners): out only to the proxy's port on
    // loopback.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_limits_the_network_to_the_proxy() {
        let proxy = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let other = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let (pp, op) = (
            proxy.local_addr().unwrap().port(),
            other.local_addr().unwrap().port(),
        );
        let ws = std::env::temp_dir().canonicalize().unwrap();
        let profile = seatbelt_profile(&ws, &[], None, Net::Proxy(pp), &[]);
        let run = |cmd: String| {
            Command::new("sandbox-exec")
                .args(["-p", &profile, "sh", "-c", &cmd])
                .current_dir(&ws)
                .stdin(Stdio::null())
                .output()
                .unwrap()
        };
        let to_proxy = run(format!("nc -z -G 2 127.0.0.1 {pp}"));
        assert!(to_proxy.status.success(), "{to_proxy:?}");
        let to_other = run(format!("nc -z -G 2 127.0.0.1 {op}"));
        assert!(!to_other.status.success(), "{to_other:?}");
    }
}
