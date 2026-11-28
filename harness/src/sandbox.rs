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
    /// Linux, when bubblewrap is missing but unprivileged user namespaces
    /// work: Landlock and seccomp inside bwn's own mount namespace, which
    /// also confines metadata, hides store names and keeps `.git` read-only
    /// (roadmap 2.2; `userns.rs`).
    Userns,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::Bwrap => "bwrap (bubblewrap)",
            Backend::SandboxExec => "sandbox-exec (Seatbelt)",
            Backend::Landlock => "landlock+seccomp (built-in)",
            Backend::Userns => "namespace+landlock+seccomp (built-in)",
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

/// Take back that allowance (a child session whose inherited policy
/// doesn't have it).
pub fn forget_unconfined() {
    UNCONFINED_ALLOWED.store(false, Ordering::Relaxed);
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
    #[cfg(test)]
    if let Some(m) = TEST_MODE.with(|m| m.get()) {
        return m;
    }
    match MODE.load(Ordering::Relaxed) {
        1 => Mode::Auto,
        2 => Mode::Require,
        _ => Mode::Off,
    }
}

// A sandbox mode for one test thread, so a test that needs the sandbox on
// doesn't switch it on under the tests running beside it.
#[cfg(test)]
thread_local! {
    pub(crate) static TEST_MODE: std::cell::Cell<Option<Mode>> = const { std::cell::Cell::new(None) };
}

pub fn set_network(allowed: bool) {
    NETWORK.store(allowed, Ordering::Relaxed);
}

pub fn network() -> bool {
    NETWORK.load(Ordering::Relaxed)
}

// Landlock alone holds a command to the checker by port only, so a command
// could still reach another machine listening on that port. There it gets
// no network unless the user allows it for this session, with
// `/sandbox network` or `--sandbox-network`; never from settings, a project
// or the model (owner decision; roadmap 2.2).
static LANDLOCK_NETWORK: AtomicBool = AtomicBool::new(false);

/// `/sandbox network` and `--sandbox-network`: on Landlock alone, let
/// commands use the network through the checker for this session.
pub fn allow_landlock_network_for_session() {
    LANDLOCK_NETWORK.store(true, Ordering::Relaxed);
}

pub fn landlock_network_allowed() -> bool {
    LANDLOCK_NETWORK.load(Ordering::Relaxed)
}

/// Take back the Landlock-alone network allowance (a child session whose
/// inherited policy doesn't have it; tests).
pub fn forget_landlock_network() {
    LANDLOCK_NETWORK.store(false, Ordering::Relaxed);
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

pub(crate) fn hidden_paths_in(
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

/// Credential stores that hold one of `roots` (a session run from inside
/// `~/.aws`, say). No backend can hide such a store without hiding the
/// workspace, so it stays readable; the posture check says so and auto mode
/// asks before commands.
pub(crate) fn stores_holding_a_root_in(
    home: &Path,
    bwn_home: &Path,
    expose: &[PathBuf],
    roots: &[&Path],
) -> Vec<PathBuf> {
    let roots: Vec<PathBuf> = roots
        .iter()
        .map(|r| r.canonicalize().unwrap_or_else(|_| r.to_path_buf()))
        .collect();
    crate::tools::credential_stores(home)
        .into_iter()
        .chain([bwn_home.to_path_buf()])
        .filter(|c| c.symlink_metadata().is_ok())
        .map(|c| c.canonicalize().unwrap_or(c))
        .filter(|c| c != Path::new("/") && !expose.iter().any(|e| c.starts_with(e)))
        .filter(|c| roots.iter().any(|r| r.starts_with(c)))
        .collect()
}

fn store_holds_a_root(roots: &[&Path]) -> bool {
    let Some(home) = crate::config::user_home() else {
        return false;
    };
    let expose = EXPOSE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    !stores_holding_a_root_in(&home, &crate::config::home(), &expose, roots).is_empty()
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
    /// In the session's private network namespace (netns.rs), whose only
    /// way out is the checker on this port of its own loopback. Servers a
    /// command starts there are reachable from the next command.
    Private(u16),
    /// Anything (`sandbox_egress_check: false`).
    Open,
}

impl Net {
    /// The checker's port, when the command goes out through it.
    pub fn checker_port(self) -> Option<u16> {
        match self {
            Net::Proxy(p) | Net::Private(p) => Some(p),
            Net::Off | Net::Open => None,
        }
    }
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
        // Held by the private network namespace, on any Landlock ABI.
        #[cfg(target_os = "linux")]
        Backend::Userns => true,
        #[cfg(target_os = "linux")]
        Backend::Bwrap | Backend::Landlock => crate::landlock::limits_connections(),
        #[cfg(not(target_os = "linux"))]
        Backend::Bwrap | Backend::Landlock | Backend::Userns => false,
    }
}

/// The network a command run now under `backend` gets. With the network
/// allowed and the check on, that is the proxy; where the backend cannot
/// hold a command to it, no network (the posture check says so).
pub fn net_for(backend: Backend) -> Net {
    if !network() {
        return Net::Off;
    }
    if backend == Backend::Landlock && !landlock_network_allowed() {
        return Net::Off;
    }
    if !egress_check() {
        return Net::Open;
    }
    // The namespace backend runs every command in the session's private
    // network namespace. If that can't be made, no network rather than the
    // host's.
    #[cfg(target_os = "linux")]
    if backend == Backend::Userns {
        return match crate::netns::session() {
            Ok(s) => Net::Private(s.port),
            Err(_) => Net::Off,
        };
    }
    // Bubblewrap runs in the same namespace where one can be made. Where it
    // can't (a setuid bubblewrap with unprivileged namespaces off), it keeps
    // the checker held by port, as before.
    #[cfg(target_os = "linux")]
    if backend == Backend::Bwrap {
        if let Ok(s) = crate::netns::session() {
            return Net::Private(s.port);
        }
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
            "userns" => {
                return crate::landlock::available()
                    .and_then(|()| crate::userns::available())
                    .map(|()| Backend::Userns)
            }
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
            // Without bubblewrap, bwn's own mount namespace closes the gaps
            // Landlock alone leaves, where user namespaces are allowed.
            Ok(()) if crate::userns::available().is_ok() => Ok(Backend::Userns),
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
        Net::Proxy(_) | Net::Private(_) => confine_exe(),
        _ => None,
    };
    // With Net::Private, bwrap itself was started in the session's private
    // network namespace (command_for), so sharing the network shares that.
    let share_net = match net {
        Net::Off => false,
        Net::Proxy(_) | Net::Private(_) => limiter.is_some(),
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
    if let Some(port) = net.checker_port() {
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
                // In the private namespace only seccomp's TCP-only rule is
                // needed; the port rule is for the host network.
                if let Net::Private(_) = net {
                    format!("private:{port}")
                } else {
                    port.to_string()
                },
                "--".into(),
            ]);
        }
    }
    a.extend(["sh", "-c", cmd].map(str::to_string));
    a
}

/// Start `c` (bubblewrap) in the session's private network namespace: it
/// joins the session's user and network namespaces before exec, and
/// bubblewrap's `--share-net` then shares that network.
#[cfg(target_os = "linux")]
pub(crate) fn join_session_net(c: &mut Command) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let (user, net) = crate::netns::join_fds()?;
    // SAFETY: setns is async-signal-safe; the fds stay open in bwn for the
    // session.
    unsafe {
        c.pre_exec(move || {
            if libc::setns(user, libc::CLONE_NEWUSER) != 0
                || libc::setns(net, libc::CLONE_NEWNET) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
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

/// The paths inside `roots` a sandboxed command must not write: each root's
/// `.git` and `.buildwithnexus`, and the nested repositories' `.git` (within
/// the same bounds bubblewrap uses).
pub fn protected_paths(roots: &[&Path]) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = roots
        .iter()
        .flat_map(|r| PROTECTED_IN_WORKSPACE.iter().map(|d| r.join(d)))
        .collect();
    for r in roots {
        for p in nested_git_entries(r) {
            if !v.contains(&p) {
                v.push(p);
            }
        }
    }
    v
}

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
    nested_git_scan(root).0
}

/// Whether the nested-repository scan covered every folder under each of
/// `roots`. When it stopped at its bounds, a `.git` past them stays
/// writable: the posture check says so and auto mode asks before commands.
pub fn nested_git_complete(roots: &[&Path]) -> bool {
    roots.iter().all(|r| nested_git_scan(r).1)
}

// The nested `.git` entries under `root`, and whether the scan finished
// within its bounds (NESTED_GIT_DIRS folders, NESTED_GIT_MAX found).
fn nested_git_scan(root: &Path) -> (Vec<PathBuf>, bool) {
    let mut found = Vec::new();
    let mut queue = std::collections::VecDeque::from([root.to_path_buf()]);
    let mut read = 0;
    while let Some(dir) = queue.pop_front() {
        if read >= NESTED_GIT_DIRS {
            return (found, false);
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
                        return (found, false);
                    }
                }
                continue; // a repository's own store holds no repositories
            }
            if t.is_dir() && name != "node_modules" && name != "target" {
                queue.push_back(e.path());
            }
        }
    }
    (found, true)
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
        Net::Off | Net::Private(_) => p.push_str("(deny network*)\n"),
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
            let net = net_for(backend);
            let mut c = Command::new("bwrap");
            c.args(bwrap_args(cmd, &cwd, net));
            #[cfg(target_os = "linux")]
            if let Net::Private(_) = net {
                join_session_net(&mut c)?;
            }
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
        Backend::Landlock | Backend::Userns => {
            #[cfg(target_os = "linux")]
            {
                crate::landlock::command_in(
                    cmd,
                    &cwd,
                    &crate::workdirs::list(),
                    net_for(backend),
                    backend == Backend::Userns,
                )?
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
    if let Some(port) = net_for(backend).checker_port() {
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

/// How an extension (a stdio MCP server, a hook) starts: inside the same
/// sandbox as commands, running `program` with `args` in `cwd` (roadmap 2.4;
/// analysis H1, H2). `unconfined` is the entry's own `"sandbox": false`,
/// which the posture check names. With the sandbox off, or with no backend
/// under `auto`, it runs as it did before (the posture check says so);
/// under `require` with no backend it is refused. Inside the sandbox the
/// credential variables are stripped, as for a command; outside, only when
/// `scrub` (an MCP server: third-party code; a hook outside keeps the
/// user's environment, as it always has). The caller adds the entry's own
/// environment.
pub fn extension_command(
    program: &str,
    args: &[String],
    cwd: &Path,
    unconfined: bool,
    scrub: bool,
) -> Result<Command, String> {
    let plain = || {
        let mut c = Command::new(program);
        c.args(args).current_dir(cwd);
        if scrub {
            scrub_credentials(&mut c);
        }
        c
    };
    if unconfined || mode() == Mode::Off {
        return Ok(plain());
    }
    match decide(mode(), backend()) {
        Decision::Sandboxed(b) => {
            let script = std::iter::once(program)
                .chain(args.iter().map(String::as_str))
                .map(sh_quote)
                .collect::<Vec<_>>()
                .join(" ");
            command_for(b, &format!("exec {script}"), cwd)
        }
        _ if mode() == Mode::Require => Err(format!(
            "sandbox: require, and no sandbox backend works here ({}); set \"sandbox\": false on this entry to run it unconfined",
            backend_error().unwrap_or("no backend")
        )),
        _ => Ok(plain()),
    }
}

// One shell word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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
        Decision::Sandboxed(Backend::Bwrap | Backend::SandboxExec | Backend::Userns) => true,
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
    let extra = crate::workdirs::list();
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let roots: Vec<&Path> = std::iter::once(cwd.as_path())
        .chain(extra.iter().map(PathBuf::as_path))
        .collect();
    // A store that holds the workspace is left visible on every backend
    // (hidden_paths drops it, so Landlock's check below would not see it).
    if store_holds_a_root(&roots) {
        return false;
    }
    match decide(mode(), backend()) {
        Decision::Sandboxed(Backend::Bwrap | Backend::SandboxExec) => true,
        #[cfg(target_os = "linux")]
        Decision::Sandboxed(Backend::Landlock | Backend::Userns) => {
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
pub(crate) mod tests {
    use super::*;

    // Owner decision (roadmap 2.2): Landlock alone holds a command to the
    // checker by port only, so there the network is off until the user
    // allows it for the session. With the egress check off, the namespace
    // backend gets an open network and Landlock alone gets none; the old
    // rule gave both the open network.
    #[test]
    fn invariant_landlock_alone_has_no_network_until_allowed() {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (net0, egress0) = (network(), egress_check());
        set_network(true);
        set_egress_check(false);
        forget_landlock_network();
        let before = (net_for(Backend::Landlock), net_for(Backend::Userns));
        allow_landlock_network_for_session();
        let after = net_for(Backend::Landlock);
        forget_landlock_network();
        set_network(net0);
        set_egress_check(egress0);
        assert_eq!(
            before,
            (Net::Off, Net::Open),
            "Landlock alone had a network"
        );
        assert_eq!(after, Net::Open, "allowing it for the session did not");
    }

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
        // The mode is process-wide; tests that read it hold the same lock.
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        let outside_dir = test_home_base();
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

    // A folder for test fixtures outside /tmp (bubblewrap replaces /tmp):
    // the real home, read once. Tests that point HOME at a fake home run
    // beside ones that read it, so every fixture takes its base from here.
    pub(crate) fn test_home_base() -> PathBuf {
        static BASE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        BASE.get_or_init(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|h| h.is_dir() && !h.starts_with("/tmp"))
                .unwrap_or_else(|| PathBuf::from("/var/tmp"))
        })
        .clone()
    }

    // IH-1 fixture (roadmap 2.4): a fake home (HOME points at it) with a
    // credential store and a workspace, an "outside" folder beside them,
    // and the sandbox in auto. Runs `f(home, ws, outside)` and restores
    // everything. Returns false (and runs nothing) where no backend works,
    // unless the CI cell forced one.
    pub(crate) fn with_confined_home(tag: &str, f: impl FnOnce(&Path, &Path, &Path)) -> bool {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(b) = backend() else {
            assert!(
                !matches!(
                    std::env::var("BWN_TEST_SANDBOX_BACKEND").as_deref(),
                    Ok("bwrap" | "userns" | "landlock" | "seatbelt")
                ),
                "a backend was forced but none works"
            );
            return false;
        };
        let _ = b;
        let base = crate::sandbox::tests::test_home_base();
        let root = base.join(format!("bwn-ih1-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/id_test"), "KEY-MATERIAL\n").unwrap();
        std::fs::create_dir_all(home.join("proj")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        let root = root.canonicalize().unwrap();
        let (home, outside) = (root.join("home"), root.join("outside"));
        let ws = home.join("proj");
        crate::config::TEST_USER_HOME.with(|h| *h.borrow_mut() = Some(home.clone()));
        TEST_MODE.with(|m| m.set(Some(Mode::Auto)));
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&home, &ws, &outside)));
        TEST_MODE.with(|m| m.set(None));
        crate::config::TEST_USER_HOME.with(|h| *h.borrow_mut() = None);
        let _ = std::fs::remove_dir_all(&root);
        if let Err(e) = r {
            std::panic::resume_unwind(e);
        }
        true
    }

    // IH-1 for extensions (roadmap 2.4): what a stdio MCP server or a hook
    // starts through runs confined like a command: it can't write outside
    // the workspace or read a credential store. `"sandbox": false` runs it
    // as before.
    #[cfg(unix)]
    #[test]
    fn invariant_ih1_an_extension_starts_inside_the_sandbox() {
        let ran = with_confined_home("ext", |home, ws, outside| {
            let script = format!(
                "echo x > '{}/w.txt' 2>/dev/null; cat '{}/.ssh/id_test' 2>/dev/null; echo done",
                outside.display(),
                home.display()
            );
            let run = |unconfined: bool| {
                extension_command(
                    "sh",
                    &["-c".to_string(), script.clone()],
                    ws,
                    unconfined,
                    true,
                )
                .unwrap()
                .stdin(Stdio::null())
                .output()
                .unwrap()
            };
            let confined = run(false);
            let out = String::from_utf8_lossy(&confined.stdout);
            assert!(out.contains("done"), "{confined:?}");
            assert!(!out.contains("KEY-MATERIAL"), "read the store: {out}");
            assert!(!outside.join("w.txt").exists(), "wrote outside");
            let open = run(true);
            assert!(String::from_utf8_lossy(&open.stdout).contains("KEY-MATERIAL"));
            assert!(outside.join("w.txt").exists());
        });
        eprintln!("ih1 extension ran: {ran}");
    }

    // A home folder with credential stores, outside /tmp (bubblewrap
    // replaces /tmp, and Landlock lets commands write and read it).
    #[cfg(target_os = "linux")]
    fn fake_home(tag: &str) -> (PathBuf, PathBuf, Vec<PathBuf>) {
        let base = crate::sandbox::tests::test_home_base();
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

    // Real bubblewrap: the SSH agent's socket is covered, so a command can't
    // connect to it even with the network open (the cover is a plain file).
    #[cfg(target_os = "linux")]
    #[test]
    fn invariant_bwrap_agent_socket_is_unreachable() {
        let usable = Command::new("bwrap")
            .args(bwrap_probe_args())
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success());
        if !usable {
            eprintln!("skip: bwrap unusable here");
            return;
        }
        let (home, ws, _) = fake_home("bwrap-agent");
        let sock = home.join("agent.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let hidden = hidden_paths_in(
            &home,
            &home.join(".buildwithnexus"),
            Some(sock.clone()),
            &[],
            &[&ws],
        );
        assert!(hidden.contains(&sock), "{hidden:?}");
        let probe = format!(
            "python3 -c 'import socket; s = socket.socket(socket.AF_UNIX); \
             s.connect(\"{}\"); print(\"CONNECTED\")'",
            sock.display()
        );
        let out = Command::new("bwrap")
            .args(bwrap_argv(&probe, &ws, &[], Net::Open, true, &hidden))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        drop(listener);
        let _ = std::fs::remove_dir_all(&home);
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("CONNECTED"),
            "reached the agent socket: {out:?}"
        );
        // The probe ran and failed on the connect, not on a missing python.
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Error"),
            "{out:?}"
        );
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
                c.pre_exec(move || crate::landlock::engage(fd, crate::landlock::Net::Off, false));
            }
            let o = c.output().unwrap();
            unsafe { libc::close(fd) };
            o
        };
        assert_hidden(run, &home);
        let _ = std::fs::remove_dir_all(&home);
    }

    // Analysis M5 (roadmap 2.2): under Landlock a command could list the
    // names in a hidden store (`ls ~/.ssh`), though not read them. Listing
    // is confined too now; folders beside a store, system folders and the
    // workspace still list.
    #[cfg(target_os = "linux")]
    #[test]
    fn invariant_m5_landlock_does_not_list_hidden_store_names() {
        use std::os::unix::process::CommandExt;
        if crate::landlock::available().is_err() {
            return;
        }
        let (home, ws, hidden) = fake_home("landlock-ls");
        std::fs::write(ws.join("in-ws.txt"), "x").unwrap();
        std::fs::create_dir_all(home.join(".config/app")).unwrap();
        std::fs::write(home.join(".config/app/app.toml"), "x").unwrap();
        let fd = crate::landlock::ruleset_with(&ws, &[], None, &hidden, Net::Off).unwrap();
        let ls = |dir: &Path| {
            let mut c = Command::new("ls");
            c.arg(dir).current_dir(&ws).stdin(Stdio::null());
            unsafe {
                c.pre_exec(move || crate::landlock::engage(fd, crate::landlock::Net::Off, false));
            }
            let o = c.output().unwrap();
            (
                o.status.success(),
                String::from_utf8_lossy(&o.stdout).into_owned(),
            )
        };
        let store = ls(&home.join(".ssh"));
        let gh = ls(&home.join(".config/gh"));
        let sibling = ls(&home.join(".config/app"));
        let usr = ls(Path::new("/usr"));
        let inside = ls(&ws);
        unsafe { libc::close(fd) };
        let _ = std::fs::remove_dir_all(&home);
        assert!(
            !store.0 && !store.1.contains("id_test"),
            "listed ~/.ssh: {store:?}"
        );
        assert!(
            !gh.0 && !gh.1.contains("hosts.yml"),
            "listed ~/.config/gh: {gh:?}"
        );
        assert!(
            sibling.0 && sibling.1.contains("app.toml"),
            "~/.config/app: {sibling:?}"
        );
        assert!(usr.0 && usr.1.contains("bin"), "/usr: {usr:?}");
        assert!(
            inside.0 && inside.1.contains("in-ws.txt"),
            "workspace: {inside:?}"
        );
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
    // Real Seatbelt (macOS runners): the keychain holds credentials too
    // (git's GitHub token through credential-osxkeychain, and bwn's own
    // decision-log key, roadmap 2.5). An item's access list trusts the
    // program that made it, so a sandboxed command running that program
    // would read it back unless the profile cuts off the keychain service.
    #[cfg(target_os = "macos")]
    #[test]
    fn invariant_seatbelt_keychain_is_unreachable() {
        let svc = format!("bwn-test-keychain-{}", std::process::id());
        let made = Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-s",
                &svc,
                "-a",
                "bwn",
                "-w",
                "KEYCHAIN-SECRET",
            ])
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success());
        if !made {
            eprintln!("skip: no writable keychain here");
            return;
        }
        // Readable outside the sandbox, so a refusal inside means something.
        let outside = Command::new("security")
            .args(["find-generic-password", "-s", &svc, "-w"])
            .output()
            .unwrap();
        let ws = std::env::temp_dir().join(format!("bwn-sb-kc-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let ws = ws.canonicalize().unwrap();
        let inside = Command::new("sandbox-exec")
            .args([
                "-p",
                &seatbelt_profile(&ws, &[], None, Net::Off, &[]),
                "security",
                "find-generic-password",
                "-s",
                &svc,
                "-w",
            ])
            .current_dir(&ws)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let _ = Command::new("security")
            .args(["delete-generic-password", "-s", &svc])
            .output();
        let _ = std::fs::remove_dir_all(&ws);
        assert!(
            String::from_utf8_lossy(&outside.stdout).contains("KEYCHAIN-SECRET"),
            "{outside:?}"
        );
        assert!(
            !String::from_utf8_lossy(&inside.stdout).contains("KEYCHAIN-SECRET"),
            "a sandboxed command read the keychain: {inside:?}"
        );
    }

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
    pub(crate) const NET_PROBE: &str = r#"python3 -c '
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
    pub(crate) fn assert_proxy_only(out: &std::process::Output) {
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
            c.pre_exec(move || crate::landlock::engage(fd, net, false));
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

    // Roadmap 2.2, SEC-2 on bubblewrap: started in the session's private
    // network namespace, a command reaches the checker and nothing else:
    // not this machine's address on the checker's port (which the port rule
    // let through on the host network), not the host's loopback, no UDP,
    // DNS, raw, netlink, packet or Unix sockets.
    #[cfg(target_os = "linux")]
    #[test]
    fn invariant_bwrap_netns_command_reaches_only_the_checker() {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let usable = Command::new("bwrap")
            .args(bwrap_probe_args())
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success());
        let exe = std::env::current_exe().ok().and_then(|t| {
            t.ancestors()
                .map(|a| a.join("buildwithnexus"))
                .find(|p| p.is_file())
        });
        if !usable || exe.is_none() {
            eprintln!("skip: bwrap or the built binary missing");
            return;
        }
        let Ok(s) = crate::netns::session() else {
            assert!(
                std::env::var("BWN_TEST_SANDBOX_BACKEND").as_deref() != Ok("bwrap"),
                "bubblewrap works but the session namespace can't be made"
            );
            return;
        };
        let host = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let hp = host.local_addr().unwrap().port();
        let _same_port = std::net::TcpListener::bind(("0.0.0.0", s.port)).ok();
        let outer = crate::userns::tests::outer_ip();
        let outer_arg = outer.map_or("-".to_string(), |ip| ip.to_string());
        std::env::set_var("BWN_TEST_CONFINE_EXE", exe.unwrap());
        let ws = std::env::temp_dir().join(format!("bwn-bwrap-netns-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let mut c = Command::new("bwrap");
        c.args(bwrap_argv(
            &format!(
                "{} {hp} {outer_arg} {}",
                crate::userns::tests::ESCAPE_PROBE,
                s.port
            ),
            &ws,
            &[],
            Net::Private(s.port),
            false,
            &[],
        ))
        .stdin(Stdio::null());
        join_session_net(&mut c).unwrap();
        let out = c.output().unwrap();
        std::env::remove_var("BWN_TEST_CONFINE_EXE");
        let _ = std::fs::remove_dir_all(&ws);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let got = crate::userns::tests::lines(&text);
        assert_eq!(got.get("checker").map(String::as_str), Some("ok"), "{text}");
        for (k, v) in &got {
            if k != "checker" {
                assert_ne!(v, "ok", "{k} was reachable under bubblewrap: {text}");
            }
        }
        for k in ["host_loopback", "public_v4", "udp", "unix", "dns"] {
            assert!(got.contains_key(k), "probe line {k} missing: {text}");
        }
    }

    // No inbound: a server a bubblewrap command starts in the session's
    // namespace is not reachable from this machine, on loopback or on its
    // own address, while it listens on every address.
    #[cfg(target_os = "linux")]
    #[test]
    fn invariant_bwrap_netns_listener_is_not_reachable_from_the_host() {
        use std::io::BufRead;
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let usable = Command::new("bwrap")
            .args(bwrap_probe_args())
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success());
        let exe = std::env::current_exe().ok().and_then(|t| {
            t.ancestors()
                .map(|a| a.join("buildwithnexus"))
                .find(|p| p.is_file())
        });
        if !usable || exe.is_none() {
            eprintln!("skip: bwrap or the built binary missing");
            return;
        }
        let Ok(s) = crate::netns::session() else {
            return;
        };
        std::env::set_var("BWN_TEST_CONFINE_EXE", exe.unwrap());
        let ws = std::env::temp_dir().join(format!("bwn-bwrap-inbound-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let script = "python3 -c 'import socket, time\ns = socket.socket(); s.bind((\"0.0.0.0\", 0)); s.listen(4)\nprint(s.getsockname()[1], flush=True)\ns.settimeout(4)\ntry:\n    s.accept(); print(\"ACCEPTED\", flush=True)\nexcept OSError:\n    print(\"NONE\", flush=True)'";
        let mut c = Command::new("bwrap");
        c.args(bwrap_argv(
            script,
            &ws,
            &[],
            Net::Private(s.port),
            false,
            &[],
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
        join_session_net(&mut c).unwrap();
        let mut child = c.spawn().unwrap();
        std::env::remove_var("BWN_TEST_CONFINE_EXE");
        let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        let port: u16 = line.trim().parse().expect(&line);
        let mut reached = Vec::new();
        let mut targets: Vec<std::net::IpAddr> = vec![[127, 0, 0, 1].into()];
        targets.extend(crate::userns::tests::outer_ip());
        for ip in targets {
            let a = std::net::SocketAddr::new(ip, port);
            if std::net::TcpStream::connect_timeout(&a, std::time::Duration::from_secs(1)).is_ok() {
                reached.push(a);
            }
        }
        let mut rest = String::new();
        let _ = std::io::Read::read_to_string(&mut out, &mut rest);
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&ws);
        // Another process on the host may hold that port number; what
        // matters is that the command's listener accepted nothing.
        assert!(
            !rest.contains("ACCEPTED"),
            "the host reached the listener: {reached:?}"
        );
        assert!(rest.contains("NONE"), "{rest}");
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
