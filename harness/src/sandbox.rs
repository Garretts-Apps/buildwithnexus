//! Opt-in OS-level sandbox for shell commands.
//!
//! An extra layer *under* the permission gate: once a `run_command` / `bash`
//! / `check_work` call has been approved, the child process is confined so
//! that filesystem writes outside the working directory (and, optionally,
//! network access) fail at the OS level. Backends are external binaries —
//! `bwrap` (bubblewrap) on Linux and `sandbox-exec` (Seatbelt) on macOS — so
//! no crate dependency is added. Windows and WSL have no backend.
//!
//! Confined: writes anywhere but the working directory and the temp dirs;
//! the network when `sandbox_network` is false. Not confined: reads, the
//! agent's own file tools (already fenced to cwd), hooks, MCP servers.

use std::path::Path;
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
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::Bwrap => "bwrap (bubblewrap)",
            Backend::SandboxExec => "sandbox-exec (Seatbelt)",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Unsandboxed,
    Sandboxed(Backend),
    Refused,
}

/// The policy in one place: what `mode` does given whether a backend works.
pub fn decide(mode: Mode, backend: Option<Backend>) -> Decision {
    match (mode, backend) {
        (Mode::Off, _) | (Mode::Auto, None) => Decision::Unsandboxed,
        (Mode::Auto, Some(b)) | (Mode::Require, Some(b)) => Decision::Sandboxed(b),
        (Mode::Require, None) => Decision::Refused,
    }
}

// ── session state ─────────────────────────────────────────────────────────────
static MODE: AtomicU8 = AtomicU8::new(0); // 0 off, 1 auto, 2 require
static NETWORK: AtomicBool = AtomicBool::new(true);
static NOTICED: AtomicBool = AtomicBool::new(false);

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

/// Apply the `sandbox` / `sandbox_network` settings (or the `--sandbox`
/// flag). Err names the bad value; nothing changes in that case.
pub fn configure(mode: &str, network: bool) -> Result<(), String> {
    let m = Mode::parse(mode)
        .ok_or_else(|| format!("unknown sandbox mode '{mode}' — expected off, auto, or require"))?;
    set_mode(m);
    set_network(network);
    Ok(())
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

fn detect() -> Result<Backend, String> {
    if cfg!(windows) {
        return Err("no sandbox backend on Windows".into());
    }
    if crate::tools::is_wsl() {
        return Err("no sandbox backend inside WSL".into());
    }
    let (bin, args, backend): (&str, Vec<String>, Backend) = if cfg!(target_os = "macos") {
        (
            "sandbox-exec",
            vec![
                "-p".into(),
                "(version 1) (allow default)".into(),
                "sh".into(),
                "-c".into(),
                "true".into(),
            ],
            Backend::SandboxExec,
        )
    } else if cfg!(target_os = "linux") {
        (
            "bwrap",
            bwrap_args("true", Path::new("/"), true),
            Backend::Bwrap,
        )
    } else {
        return Err("no sandbox backend on this platform".into());
    };
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

// ── argv / profile construction (pure, unit-tested) ───────────────────────────
/// bubblewrap argv: everything read-only, fresh /dev, /proc and /tmp, the
/// workspace bound read-write at its own path. `~/.buildwithnexus` stays
/// read-only — checkpoints are written in-process, never by the child.
pub fn bwrap_args(cmd: &str, cwd: &Path, network: bool) -> Vec<String> {
    let cwd = cwd.to_string_lossy().into_owned();
    let mut a: Vec<String> = vec!["--unshare-all".into()];
    if network {
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
            "--setenv",
            "TMPDIR",
            "/tmp",
            "--bind",
            &cwd,
            &cwd,
            "--chdir",
            &cwd,
            "--",
            "sh",
            "-c",
            cmd,
        ]
        .map(str::to_string),
    );
    a
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
pub fn seatbelt_profile(cwd: &Path, tmpdir: Option<&Path>, network: bool) -> String {
    let mut p =
        String::from("(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*");
    for dir in [
        Some(cwd),
        Some(Path::new("/private/tmp")),
        Some(Path::new("/tmp")),
        tmpdir,
    ]
    .into_iter()
    .flatten()
    {
        p.push_str(&format!("\n  (subpath {})", sbpl_quote(dir)));
    }
    p.push_str(")\n(allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\") (regex #\"^/dev/tty\"))\n");
    if !network {
        p.push_str("(deny network*)\n");
    }
    p
}

fn command_for(backend: Backend, cmd: &str, cwd: &Path) -> Command {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut c = match backend {
        Backend::Bwrap => {
            let mut c = Command::new("bwrap");
            c.args(bwrap_args(cmd, &cwd, network()));
            c
        }
        Backend::SandboxExec => {
            let tmp = std::env::var_os("TMPDIR")
                .map(std::path::PathBuf::from)
                .and_then(|t| t.canonicalize().ok());
            let mut c = Command::new("sandbox-exec");
            c.args(["-p", &seatbelt_profile(&cwd, tmp.as_deref(), network())]);
            c.args(["sh", "-c", cmd]);
            c
        }
    };
    c.current_dir(&cwd);
    c
}

/// Build the sandboxed command for `cmd`, per the session policy.
/// `Ok(None)` = run it unsandboxed (mode off, or auto without a backend —
/// the latter says so once per session); `Err` = `require` with no backend.
pub fn wrap(cmd: &str, cwd: &Path) -> Result<Option<Command>, String> {
    let mode = mode();
    if mode == Mode::Off {
        return Ok(None);
    }
    match decide(mode, backend()) {
        Decision::Sandboxed(b) => Ok(Some(command_for(b, cmd, cwd))),
        Decision::Unsandboxed => {
            if !NOTICED.swap(true, Ordering::Relaxed) {
                let msg = format!(
                    "  sandbox: {} — running commands unsandboxed (sandbox=auto)",
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
            "sandbox: refusing to run the command — sandbox mode is `require` but no backend is usable ({}). Install bubblewrap (Linux), or switch with /sandbox auto|off.",
            backend_error().unwrap_or("no backend")
        )),
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
            Decision::Unsandboxed if mode == Mode::Auto => {
                "commands: NOT confined — auto mode runs unsandboxed without a backend"
            }
            Decision::Unsandboxed => "commands: not confined — /sandbox auto to enable",
            Decision::Refused => "commands: REFUSED — require mode with no usable backend",
        }
        .to_string(),
    );
    v.push(
        "scope: run_command/bash/check_work only; reads, file tools, and hooks are never sandboxed"
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
        (_, Err(e)) if mode == Mode::Auto => (
            "⚠",
            format!("mode=auto but no backend ({e}) — commands run unconfined"),
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
        assert_eq!(decide(Mode::Auto, None), Decision::Unsandboxed);
        assert_eq!(
            decide(Mode::Require, Some(Backend::SandboxExec)),
            Decision::Sandboxed(Backend::SandboxExec)
        );
        assert_eq!(decide(Mode::Require, None), Decision::Refused);
    }

    #[test]
    fn bwrap_argv_binds_only_the_workspace_rw() {
        let a = bwrap_args("echo hi", Path::new("/work/proj"), true);
        let s = a.join(" ");
        assert!(s.starts_with("--unshare-all --share-net --die-with-parent --new-session "));
        assert!(s.contains("--ro-bind / /"));
        assert!(s.contains("--tmpfs /tmp"));
        assert!(s.contains("--bind /work/proj /work/proj --chdir /work/proj"));
        // Exactly one read-write bind, and it is the workspace.
        assert_eq!(a.iter().filter(|x| *x == "--bind").count(), 1);
        assert_eq!(&a[a.len() - 4..], ["--", "sh", "-c", "echo hi"]);
        // Network off drops --share-net and nothing else.
        let off = bwrap_args("echo hi", Path::new("/work/proj"), false);
        assert!(!off.contains(&"--share-net".to_string()));
        assert_eq!(off.len(), a.len() - 1);
    }

    #[test]
    fn seatbelt_profile_denies_writes_except_workspace_and_tmp() {
        let p = seatbelt_profile(
            Path::new("/private/var/w/my \"proj\""),
            Some(Path::new("/private/var/folders/xy/T")),
            true,
        );
        assert!(p.starts_with("(version 1)\n(allow default)\n(deny file-write*)\n"));
        assert!(p.contains("(subpath \"/private/var/w/my \\\"proj\\\"\")"));
        assert!(p.contains("(subpath \"/private/tmp\")"));
        assert!(p.contains("(subpath \"/tmp\")"));
        assert!(p.contains("(subpath \"/private/var/folders/xy/T\")"));
        assert!(p.contains("(literal \"/dev/null\")"));
        assert!(!p.contains("network"));
        let no_net = seatbelt_profile(Path::new("/w"), None, false);
        assert!(no_net.ends_with("(deny network*)\n"));
        assert!(!no_net.contains("folders"));
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
            .args(bwrap_args("true", Path::new("/"), true))
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
            let mut c = command_for(Backend::Bwrap, cmd, &ws);
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
        let _ = std::fs::remove_dir_all(&ws);
    }
}
