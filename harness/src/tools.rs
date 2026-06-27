// The tool surface the agent can call. Definitions are data; execution is a
// single `match` — no registry, no dyn dispatch (Casey: don't build the plugin
// system before there are plugins).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value, // JSON Schema for the input object
}

pub struct Outcome {
    pub content: String,
    pub is_error: bool,
    pub finished: bool, // the `finish` tool ends the loop
}

fn ok(s: impl Into<String>) -> Outcome { Outcome { content: s.into(), is_error: false, finished: false } }
fn err(s: impl Into<String>) -> Outcome { Outcome { content: s.into(), is_error: true, finished: false } }

const MAX_READ: usize = 100 * 1024;
const MAX_OUT: usize = 16 * 1024;

// Exposed only for the criterion perf suite (see provider::bench).
#[doc(hidden)]
pub mod bench {
    use std::path::{Path, PathBuf};
    pub fn normalize(p: &Path) -> PathBuf { super::normalize(p) }
    pub fn truncate(s: String, max: usize) -> String { super::truncate(s, max) }
}

pub fn defs(include_subagent: bool) -> Vec<ToolDef> {
    let mut v = vec![
        ToolDef { name: "read_file", description: "Read a UTF-8 text file and return its contents. Works anywhere on the filesystem.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}) },
        ToolDef { name: "list_dir", description: "List entries in a directory. Works anywhere on the filesystem.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}) },
        ToolDef { name: "write_file", description: "Create or overwrite a file with the given contents.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}) },
        ToolDef { name: "edit_file", description: "Replace the unique occurrence of `old` with `new` in a file.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"old":{"type":"string"},"new":{"type":"string"}},"required":["path","old","new"]}) },
        ToolDef { name: "run_command", description: "Run a shell command (grep, find, git, cargo, etc.) and return its output. Use this for searching, building, or any shell operation.",
            schema: json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}) },
        ToolDef { name: "finish", description: "Signal the task is complete with a short summary for the user.",
            schema: json!({"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"]}) },
        ToolDef { name: "save_memory", description: "Save a short note to persistent memory so it's available in future sessions. Use for preferences, recurring facts, or things the user says to remember.",
            schema: json!({"type":"object","properties":{"note":{"type":"string","description":"Short fact or preference to remember (one line)"}},"required":["note"]}) },
        ToolDef { name: "fetch_url", description: "Fetch the content of a URL via HTTP GET. Returns the response body as text. Use for reading documentation, API responses, changelogs, or any web content.",
            schema: json!({"type":"object","properties":{"url":{"type":"string","description":"HTTP or HTTPS URL to fetch"}},"required":["url"]}) },
    ];
    if include_subagent {
        v.push(ToolDef {
            name: "spawn_subagent",
            description: "Delegate a self-contained sub-task to a fresh agent with its own context window. Set isolate=true to run it in an isolated git worktree. Returns the subagent's summary.",
            schema: json!({"type":"object","properties":{
                "task":{"type":"string"},
                "role":{"type":"string","enum":["engineer","researcher"]},
                "isolate":{"type":"boolean"}
            },"required":["task"]}),
        });
    }
    v
}

// Mutating tools pass through the permission gate; reads never do.
pub fn is_mutating(name: &str) -> bool {
    matches!(name, "write_file" | "edit_file" | "run_command")
}

// Commands that are unambiguously read-only (grep, find, cat, etc.) — allowed
// even in ReadOnly permission mode despite run_command being generically mutating.
pub fn is_readonly_command(cmd: &str) -> bool {
    let lower = cmd.trim().to_lowercase();
    let first = lower.split_whitespace().next().unwrap_or("");
    let base = first.rsplit('/').next().unwrap_or(first);
    matches!(base, "grep" | "egrep" | "fgrep" | "rg" | "find" | "cat"
               | "ls" | "head" | "tail" | "wc" | "sort" | "uniq" | "diff"
               | "tree" | "stat" | "file" | "jq" | "sed")
        || lower.starts_with("git log")
        || lower.starts_with("git status")
        || lower.starts_with("git diff")
        || lower.starts_with("git show")
        || lower.starts_with("git branch")
        || lower.starts_with("git tag")
        || lower.starts_with("git remote")
}

// A one-line, human-readable preview of what a call will do (shown at the gate).
pub fn preview(name: &str, input: &Value) -> String {
    match name {
        "write_file" => format!("write {}", input["path"].as_str().unwrap_or("?")),
        "edit_file" => format!("edit {}", input["path"].as_str().unwrap_or("?")),
        "run_command" => format!("run: {}", input["command"].as_str().unwrap_or("?")),
        "spawn_subagent" => format!("subagent: {}", input["task"].as_str().unwrap_or("?")),
        "fetch_url" => format!("GET {}", input["url"].as_str().unwrap_or("?")),
        _ => name.to_string(),
    }
}

fn resolve(cwd: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) }
}

// Marker placed on a tool call whose JSON arguments failed to parse, so the
// agent can feed the model a clear error instead of running with empty fields.
pub const INVALID_ARGS: &str = "__bwn_invalid_args__";

// The filesystem path a call touches (for confinement checks), resolved against
// cwd. None for non-path tools.
pub fn touched_path(name: &str, input: &Value, cwd: &Path) -> Option<PathBuf> {
    match name {
        "read_file" | "list_dir" | "write_file" | "edit_file" => {
            Some(resolve(cwd, input["path"].as_str().unwrap_or("")))
        }
        _ => None,
    }
}

// Lexically fold `.`/`..` without touching the filesystem (works for paths that
// don't exist yet, e.g. write targets).
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => { out.pop(); }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// True if the path resolves outside the working directory.
pub fn escapes_cwd(p: &Path, cwd: &Path) -> bool {
    let base = cwd.canonicalize().unwrap_or_else(|_| normalize(cwd));
    !normalize(p).starts_with(&base)
}

// Paths that should never be read/written without explicit confirmation, even in
// auto mode — credential stores and key material are the prime exfil targets.
pub fn is_sensitive(p: &Path) -> bool {
    let s = normalize(p).to_string_lossy().to_lowercase();
    let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    s.contains("/.ssh/")
        || s.contains("/.buildwithnexus/")
        || s.contains("/.aws/")
        || s.contains("/.gnupg/")
        || name == ".env.keys"
        || name == ".env"
        || name.starts_with(".env.")
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || name.ends_with(".pem")
}

// ── WSL2 filesystem boundary guard ───────────────────────────────────────────

/// True when running inside WSL2 (Windows Subsystem for Linux).
pub fn is_wsl() -> bool {
    // WSL sets WSL_DISTRO_NAME in every shell launched from Windows Terminal.
    if std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some() {
        return true;
    }
    // Fallback: /proc/version contains "Microsoft" or "WSL" on WSL kernels.
    std::fs::read_to_string("/proc/version")
        .map(|v| { let l = v.to_lowercase(); l.contains("microsoft") || l.contains("wsl") })
        .unwrap_or(false)
}

/// True when `path` is on a Windows drive mount (e.g. /mnt/c/) inside WSL2.
/// Writes to these paths cross the WSL2 → Windows boundary and always need
/// confirmation so the agent can't silently mutate the host filesystem.
pub fn is_wsl_windows_mount(path: &Path) -> bool {
    if !is_wsl() { return false; }
    use std::path::Component;
    let normed = normalize(path);
    let comps = normed.components().collect::<Vec<_>>();
    // /mnt/<single-letter>/ → Windows drive mount
    if comps.len() >= 3 {
        if let (Component::RootDir, Component::Normal(mnt), Component::Normal(drive)) =
            (&comps[0], &comps[1], &comps[2])
        {
            return mnt.to_str() == Some("mnt")
                && drive.to_str().map(|d| d.len() == 1 && d.is_ascii()).unwrap_or(false);
        }
    }
    false
}

/// True when a shell command string references /mnt/<drive>/ paths in WSL2.
/// Catches `cp /mnt/c/foo .`, `rm /mnt/d/bar`, etc.
pub fn command_touches_wsl_mount(cmd: &str) -> bool {
    if !is_wsl() { return false; }
    // Simple heuristic: look for /mnt/<single-letter>/ token in the command.
    cmd.split_whitespace().any(|tok| {
        tok.starts_with("/mnt/") && tok.len() > 6 && tok.as_bytes().get(5).map(|b| b.is_ascii_alphabetic()).unwrap_or(false)
    })
}

// Commands so destructive they require confirmation in every mode.
pub fn catastrophic(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    let nospace: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    nospace.contains("rm-rf/")          // rm -rf of an absolute path
        || nospace.contains("rm-fr/")
        || nospace.contains(":(){:|:&};:") // fork bomb
        || nospace.contains("mkfs")
        || nospace.contains("of=/dev/")    // dd onto a device
        || nospace.contains(">/dev/sd")
        || nospace.contains(">/dev/nvme")
        || nospace.contains("chmod-r777/")
}

fn truncate(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    // Back off to a UTF-8 char boundary — String::truncate / slicing panics if
    // `max` lands inside a multibyte glyph, which arbitrary file/command output
    // can easily produce.
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("\n…[truncated]");
    out
}

pub fn run(name: &str, input: &Value, cwd: &Path) -> Outcome {
    match name {
        "read_file" => {
            let p = resolve(cwd, input["path"].as_str().unwrap_or(""));
            match fs::read_to_string(&p) {
                Ok(c) => ok(truncate(c, MAX_READ)),
                Err(e) => err(format!("cannot read {}: {e}", p.display())),
            }
        }
        "list_dir" => {
            let p = resolve(cwd, input["path"].as_str().unwrap_or("."));
            match fs::read_dir(&p) {
                Ok(rd) => {
                    let mut names: Vec<String> = rd.filter_map(|e| e.ok()).map(|e| {
                        let n = e.file_name().to_string_lossy().into_owned();
                        if e.path().is_dir() { format!("{n}/") } else { n }
                    }).collect();
                    names.sort();
                    ok(names.join("\n"))
                }
                Err(e) => err(format!("cannot list {}: {e}", p.display())),
            }
        }
        "write_file" => {
            let p = resolve(cwd, input["path"].as_str().unwrap_or(""));
            let content = input["content"].as_str().unwrap_or("");
            if let Some(dir) = p.parent() {
                let _ = fs::create_dir_all(dir);
            }
            match fs::write(&p, content) {
                Ok(_) => ok(format!("wrote {} ({} bytes)", p.display(), content.len())),
                Err(e) => err(format!("cannot write {}: {e}", p.display())),
            }
        }
        "edit_file" => {
            let p = resolve(cwd, input["path"].as_str().unwrap_or(""));
            let old = input["old"].as_str().unwrap_or("");
            let new = input["new"].as_str().unwrap_or("");
            let body = match fs::read_to_string(&p) {
                Ok(b) => b,
                Err(e) => return err(format!("cannot read {}: {e}", p.display())),
            };
            if old.is_empty() {
                return err("`old` text not found in file");
            }
            // One pass to locate, a partial pass to confirm uniqueness — instead
            // of matches().count() (materializes all) plus a separate replace.
            let Some(first) = body.find(old) else {
                return err("`old` text not found in file");
            };
            if body[first + old.len()..].contains(old) {
                return err("`old` text is not unique — add surrounding context");
            }
            match fs::write(&p, body.replacen(old, new, 1)) {
                Ok(_) => ok(format!("edited {}", p.display())),
                Err(e) => err(format!("cannot write {}: {e}", p.display())),
            }
        }
        "run_command" => {
            let cmd = input["command"].as_str().unwrap_or("");
            let output = if cfg!(windows) {
                Command::new("cmd").args(["/C", cmd]).current_dir(cwd).output()
            } else {
                Command::new("sh").args(["-c", cmd]).current_dir(cwd).output()
            };
            match output {
                Ok(o) => {
                    let mut s = String::new();
                    s.push_str(&String::from_utf8_lossy(&o.stdout));
                    let e = String::from_utf8_lossy(&o.stderr);
                    if !e.trim().is_empty() {
                        s.push_str("\n[stderr]\n");
                        s.push_str(&e);
                    }
                    let code = o.status.code().unwrap_or(-1);
                    s.push_str(&format!("\n[exit {code}]"));
                    Outcome { content: truncate(s, MAX_OUT), is_error: !o.status.success(), finished: false }
                }
                Err(e) => err(format!("failed to spawn: {e}")),
            }
        }
        "fetch_url" => {
            let url = input["url"].as_str().unwrap_or("");
            if url.is_empty() {
                return err("url is required");
            }
            match ureq::get(url).call() {
                Ok(resp) => {
                    match resp.into_string() {
                        Ok(body) => ok(truncate(body, MAX_OUT)),
                        Err(e) => err(format!("failed to read response body: {e}")),
                    }
                }
                Err(e) => err(format!("fetch failed: {e}")),
            }
        }
        "finish" => Outcome {
            content: input["summary"].as_str().unwrap_or("done").to_string(),
            is_error: false,
            finished: true,
        },
        other => err(format!("unknown tool: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ── truncate ────────────────────────────────────────────────────────────
    #[test]
    fn truncate_shorter_than_max_unchanged() {
        assert_eq!(truncate("hello".into(), 100), "hello");
    }

    #[test]
    fn truncate_appends_marker() {
        let out = truncate("abcdefghij".into(), 5);
        assert!(out.starts_with("abcde"));
        assert!(out.contains("[truncated]"));
    }

    #[test]
    fn truncate_backs_off_to_char_boundary() {
        // "é" is two bytes; cutting at an odd byte must not panic and must yield
        // valid UTF-8.
        let s = "a".to_string() + &"é".repeat(50); // 1 + 100 bytes
        let out = truncate(s, 4); // 4 lands mid-glyph
        assert!(out.is_char_boundary(out.find('\n').unwrap_or(out.len())));
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn truncate_emoji_boundary() {
        let s = "🎉".repeat(10); // 4 bytes each
        let out = truncate(s, 5); // mid-emoji
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn truncate_exactly_max_unchanged() {
        assert_eq!(truncate("abcde".into(), 5), "abcde");
    }

    // ── is_sensitive ────────────────────────────────────────────────────────
    #[test]
    fn sensitive_paths() {
        for p in [
            "/home/u/.ssh/id_rsa",
            "/home/u/.aws/credentials",
            "/home/u/.gnupg/secring.gpg",
            "/home/u/.buildwithnexus/.env.keys",
            "/proj/.env",
            "/proj/.env.local",
            "/proj/server.pem",
            "/home/u/id_ed25519",
        ] {
            assert!(is_sensitive(Path::new(p)), "{p} should be sensitive");
        }
    }

    #[test]
    fn non_sensitive_paths() {
        for p in ["/proj/src/main.rs", "/proj/README.md", "/proj/environment.txt"] {
            assert!(!is_sensitive(Path::new(p)), "{p} should not be sensitive");
        }
    }

    #[test]
    fn sensitive_is_case_insensitive() {
        assert!(is_sensitive(Path::new("/home/U/.SSH/known_hosts")));
    }

    // ── escapes_cwd ─────────────────────────────────────────────────────────
    #[test]
    fn escapes_cwd_detects_parent_traversal() {
        let cwd = PathBuf::from("/proj/work");
        assert!(escapes_cwd(Path::new("/proj/work/../../etc/passwd"), &cwd));
        assert!(escapes_cwd(Path::new("/etc/passwd"), &cwd));
    }

    #[test]
    fn escapes_cwd_allows_inside() {
        let cwd = PathBuf::from("/proj/work");
        // Use a non-existent dir so canonicalize falls back to lexical normalize.
        assert!(!escapes_cwd(Path::new("/proj/work/src/a.rs"), &cwd));
        assert!(!escapes_cwd(Path::new("/proj/work/a/../b.rs"), &cwd));
    }

    // ── catastrophic ────────────────────────────────────────────────────────
    #[test]
    fn catastrophic_commands() {
        for c in [
            "rm -rf /",
            "rm   -rf   /",
            "rm -fr /home",
            ":(){ :|:& };:",
            "mkfs.ext4 /dev/sda1",
            "dd if=/dev/zero of=/dev/sda",
            "echo x > /dev/sda",
            "chmod -R 777 /",
        ] {
            assert!(catastrophic(c), "{c} should be catastrophic");
        }
    }

    #[test]
    fn non_catastrophic_commands() {
        for c in ["ls -la", "rm -rf ./build", "cargo test", "git status", "rm file.txt"] {
            assert!(!catastrophic(c), "{c} should be allowed");
        }
    }

    // ── normalize ───────────────────────────────────────────────────────────
    #[test]
    fn normalize_folds_dot_segments() {
        assert_eq!(normalize(Path::new("a/./b/../c")), PathBuf::from("a/c"));
        assert_eq!(normalize(Path::new("./x")), PathBuf::from("x"));
    }

    // ── is_mutating / touched_path / preview ────────────────────────────────
    #[test]
    fn mutating_classification() {
        assert!(is_mutating("write_file"));
        assert!(is_mutating("edit_file"));
        assert!(is_mutating("run_command"));
        assert!(!is_mutating("read_file"));
        assert!(!is_mutating("list_dir"));
        assert!(!is_mutating("finish"));
    }

    #[test]
    fn touched_path_only_for_file_tools() {
        let cwd = Path::new("/proj");
        assert!(touched_path("read_file", &json!({"path": "a.rs"}), cwd).is_some());
        assert!(touched_path("run_command", &json!({"command": "ls"}), cwd).is_none());
        assert!(touched_path("finish", &json!({}), cwd).is_none());
    }

    #[test]
    fn touched_path_resolves_relative_to_cwd() {
        let cwd = Path::new("/proj");
        assert_eq!(
            touched_path("read_file", &json!({"path": "src/a.rs"}), cwd),
            Some(PathBuf::from("/proj/src/a.rs"))
        );
    }

    #[test]
    fn preview_strings() {
        assert_eq!(preview("write_file", &json!({"path": "a"})), "write a");
        assert_eq!(preview("run_command", &json!({"command": "ls"})), "run: ls");
        assert_eq!(preview("read_file", &json!({})), "read_file");
    }

    #[test]
    fn defs_includes_subagent_only_when_requested() {
        assert!(!defs(false).iter().any(|d| d.name == "spawn_subagent"));
        assert!(defs(true).iter().any(|d| d.name == "spawn_subagent"));
    }

    // ── run: filesystem tools against a tempdir ─────────────────────────────
    fn tempdir() -> PathBuf {
        // Unique-enough path without external deps or Date/random (which the
        // harness forbids): use the test thread name + an atomic counter.
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let tn = std::thread::current().name().unwrap_or("t").replace("::", "_");
        let dir = std::env::temp_dir().join(format!("bwn-test-{tn}-{id}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn run_write_then_read_roundtrip() {
        let d = tempdir();
        let w = run("write_file", &json!({"path": "f.txt", "content": "hi"}), &d);
        assert!(!w.is_error);
        let r = run("read_file", &json!({"path": "f.txt"}), &d);
        assert_eq!(r.content, "hi");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_write_creates_parent_dirs() {
        let d = tempdir();
        let w = run("write_file", &json!({"path": "a/b/c.txt", "content": "x"}), &d);
        assert!(!w.is_error);
        assert!(d.join("a/b/c.txt").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_read_missing_file_errors() {
        let d = tempdir();
        let r = run("read_file", &json!({"path": "nope.txt"}), &d);
        assert!(r.is_error);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_edit_unique_match() {
        let d = tempdir();
        run("write_file", &json!({"path": "f.txt", "content": "alpha beta gamma"}), &d);
        let e = run("edit_file", &json!({"path": "f.txt", "old": "beta", "new": "BETA"}), &d);
        assert!(!e.is_error);
        let r = run("read_file", &json!({"path": "f.txt"}), &d);
        assert_eq!(r.content, "alpha BETA gamma");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_edit_non_unique_match_errors() {
        let d = tempdir();
        run("write_file", &json!({"path": "f.txt", "content": "x x"}), &d);
        let e = run("edit_file", &json!({"path": "f.txt", "old": "x", "new": "y"}), &d);
        assert!(e.is_error);
        assert!(e.content.contains("not unique"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_edit_not_found_errors() {
        let d = tempdir();
        run("write_file", &json!({"path": "f.txt", "content": "abc"}), &d);
        let e = run("edit_file", &json!({"path": "f.txt", "old": "zzz", "new": "y"}), &d);
        assert!(e.is_error);
        assert!(e.content.contains("not found"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_edit_empty_old_errors() {
        let d = tempdir();
        run("write_file", &json!({"path": "f.txt", "content": "abc"}), &d);
        let e = run("edit_file", &json!({"path": "f.txt", "old": "", "new": "y"}), &d);
        assert!(e.is_error);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_list_dir_sorted() {
        let d = tempdir();
        run("write_file", &json!({"path": "b.txt", "content": ""}), &d);
        run("write_file", &json!({"path": "a.txt", "content": ""}), &d);
        let r = run("list_dir", &json!({"path": "."}), &d);
        let lines: Vec<&str> = r.content.lines().collect();
        assert_eq!(lines, vec!["a.txt", "b.txt"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_command_captures_output_and_exit() {
        let d = tempdir();
        let r = run("run_command", &json!({"command": "echo hello"}), &d);
        assert!(!r.is_error);
        assert!(r.content.contains("hello"));
        assert!(r.content.contains("[exit 0]"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_command_nonzero_is_error() {
        let d = tempdir();
        let r = run("run_command", &json!({"command": "exit 3"}), &d);
        assert!(r.is_error);
        assert!(r.content.contains("[exit 3]"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_finish_sets_finished_flag() {
        let d = tempdir();
        let r = run("finish", &json!({"summary": "all done"}), &d);
        assert!(r.finished);
        assert_eq!(r.content, "all done");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_unknown_tool_errors() {
        let d = tempdir();
        let r = run("bogus", &json!({}), &d);
        assert!(r.is_error);
        let _ = fs::remove_dir_all(&d);
    }
}
