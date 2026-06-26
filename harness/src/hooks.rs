// Claude Code-compatible lifecycle hooks, hardened against the clone-and-own
// attack: a project's .buildwithnexus/settings.json can register shell commands,
// so project hooks run ONLY after the user explicitly trusts that folder, and a
// project hook can never *grant* a permission (only deny). Home settings are
// trusted implicitly. Everything is parsed once per session, not per tool call.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::config;
use crate::tui;

#[derive(Clone, Copy, PartialEq)]
enum Source {
    Home,
    Project,
}

struct Hook {
    event: String,
    matcher: String,
    command: String,
    source: Source,
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

// Load + parse settings once. Home is implicitly trusted; project hooks load
// only if the folder is trusted (prompting once when interactive). Must be
// called before the agent runs.
pub fn init(cwd: &Path, interactive: bool) {
    let mut list = Vec::new();
    if let Ok(text) = std::fs::read_to_string(config::home().join("settings.json")) {
        parse_into(&text, Source::Home, &mut list);
    }
    let proj = cwd.join(".buildwithnexus/settings.json");
    if let Ok(text) = std::fs::read_to_string(&proj) {
        if project_trusted(cwd, &text, interactive) {
            parse_into(&text, Source::Project, &mut list);
        } else if interactive {
            tui::line(&tui::dim("  (project hooks present but not trusted — skipped)"));
        }
    }
    let _ = HOOKS.set(Hooks { list });
}

fn parse_into(text: &str, source: Source, out: &mut Vec<Hook>) {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return };
    let Some(events) = v["hooks"].as_object() else { return };
    for (event, groups) in events {
        let Some(groups) = groups.as_array() else { continue };
        for g in groups {
            let matcher = g["matcher"].as_str().unwrap_or("*").to_string();
            if let Some(hs) = g["hooks"].as_array() {
                for h in hs {
                    if h["type"].as_str() == Some("command") {
                        if let Some(cmd) = h["command"].as_str() {
                            out.push(Hook { event: event.clone(), matcher: matcher.clone(), command: cmd.to_string(), source });
                        }
                    }
                }
            }
        }
    }
}

fn matches(matcher: &str, tool: &str) -> bool {
    let m = matcher.trim();
    m.is_empty() || m == "*" || m.split('|').any(|p| p.trim() == tool)
}

// (command, source) pairs for an event, optionally filtered by tool name.
fn commands_for(event: &str, tool: Option<&str>) -> Vec<(&'static str, Source)> {
    let Some(h) = HOOKS.get() else { return Vec::new() };
    h.list.iter()
        .filter(|hk| hk.event == event)
        .filter(|hk| tool.is_none_or(|t| matches(&hk.matcher, t)))
        .map(|hk| (hk.command.as_str(), hk.source))
        .collect()
}

// ── per-folder trust ────────────────────────────────────────────────────────
fn trust_path() -> std::path::PathBuf {
    config::home().join("trusted.json")
}

// Non-cryptographic content hash (djb2) — only used to detect that a trusted
// settings file changed since consent, so a no-dep hash is sufficient.
fn digest(s: &str) -> String {
    let mut h: u64 = 5381;
    for b in s.bytes() {
        h = (h << 5).wrapping_add(h).wrapping_add(b as u64);
    }
    format!("{h:016x}")
}

fn project_trusted(cwd: &Path, text: &str, interactive: bool) -> bool {
    let key = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf()).to_string_lossy().into_owned();
    let want = digest(text);
    let mut store: Value = std::fs::read_to_string(trust_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));

    if store[&key].as_str() == Some(want.as_str()) {
        return true;
    }
    if !interactive {
        return false; // never run untrusted project hooks unattended
    }

    let changed = store.get(&key).is_some();
    tui::line("");
    tui::line(&tui::yellow(&format!("  ⚠ {}/.buildwithnexus/settings.json defines hooks that run shell commands.", cwd.display())));
    if changed {
        tui::line(&tui::dim("    (the file changed since you last trusted it)"));
    }
    let ans = tui::ask(&format!("  Trust this folder's hooks? {} ", tui::dim("[y/N]"))).unwrap_or_default();
    if matches!(ans.trim().to_lowercase().as_str(), "y" | "yes") {
        store[key] = json!(want);
        if let Ok(t) = serde_json::to_string_pretty(&store) {
            config::ensure_home();
            let _ = std::fs::write(trust_path(), t);
        }
        true
    } else {
        false
    }
}

// ── execution ────────────────────────────────────────────────────────────────
fn run_one(cmd: &str, payload: &Value, cwd: &Path) -> (i32, String, String) {
    let mut c = if cfg!(windows) {
        let mut x = Command::new("cmd");
        x.args(["/C", cmd]);
        x
    } else {
        let mut x = Command::new("sh");
        x.args(["-c", cmd]);
        x
    };
    c.current_dir(cwd).env("BWN_PROJECT_DIR", cwd)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let Ok(mut child) = c.spawn() else { return (0, String::new(), String::new()) };
    if let Some(mut sin) = child.stdin.take() {
        let _ = sin.write_all(payload.to_string().as_bytes());
    }
    match child.wait_with_output() {
        Ok(o) => (
            o.status.code().unwrap_or(0),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        ),
        Err(_) => (0, String::new(), String::new()),
    }
}

fn decision_field(j: &Value) -> Option<&str> {
    j["hookSpecificOutput"]["permissionDecision"].as_str()
        .or_else(|| j["permissionDecision"].as_str())
        .or_else(|| j["decision"].as_str())
}

pub fn pre_tool_use(tool: &str, input: &Value, cwd: &Path) -> PreDecision {
    let payload = json!({
        "hook_event_name": "PreToolUse", "session_id": std::process::id(),
        "tool_name": tool, "tool_input": input, "cwd": cwd.to_string_lossy()
    });
    for (cmd, source) in commands_for("PreToolUse", Some(tool)) {
        let (code, stdout, stderr) = run_one(cmd, &payload, cwd);
        if code == 2 {
            let r = stderr.trim();
            return PreDecision::Deny(if r.is_empty() { "blocked by PreToolUse hook".into() } else { r.to_string() });
        }
        if let Ok(j) = serde_json::from_str::<Value>(&stdout) {
            match decision_field(&j) {
                Some("deny") | Some("block") => {
                    let reason = j["hookSpecificOutput"]["permissionDecisionReason"].as_str()
                        .or_else(|| j["reason"].as_str()).unwrap_or("denied by hook");
                    return PreDecision::Deny(reason.to_string());
                }
                // A project hook may only deny, never grant — otherwise a hostile
                // repo could disarm the permission gate.
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
    let payload = json!({
        "hook_event_name": "PostToolUse", "session_id": std::process::id(),
        "tool_name": tool, "tool_input": input,
        "tool_response": {"content": response, "is_error": is_error},
        "cwd": cwd.to_string_lossy()
    });
    for (cmd, _) in cmds {
        let _ = run_one(cmd, &payload, cwd);
    }
}

pub fn user_prompt_submit(prompt: &str, cwd: &Path) -> Result<String, String> {
    let payload = json!({
        "hook_event_name": "UserPromptSubmit", "session_id": std::process::id(),
        "prompt": prompt, "cwd": cwd.to_string_lossy()
    });
    let mut ctx = String::new();
    for (cmd, _) in commands_for("UserPromptSubmit", None) {
        let (code, stdout, stderr) = run_one(cmd, &payload, cwd);
        if code == 2 {
            let r = stderr.trim();
            return Err(if r.is_empty() { "blocked by UserPromptSubmit hook".into() } else { r.to_string() });
        }
        if !stdout.trim().is_empty() {
            ctx.push_str(stdout.trim());
            ctx.push('\n');
        }
    }
    Ok(ctx)
}

pub fn notify(event: &str, cwd: &Path) {
    let cmds = commands_for(event, None);
    if cmds.is_empty() {
        return;
    }
    let payload = json!({"hook_event_name": event, "session_id": std::process::id(), "cwd": cwd.to_string_lossy()});
    for (cmd, _) in cmds {
        let _ = run_one(cmd, &payload, cwd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── matches ─────────────────────────────────────────────────────────────
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
    }

    // ── digest ──────────────────────────────────────────────────────────────
    #[test]
    fn digest_is_stable_and_sensitive() {
        assert_eq!(digest("hello"), digest("hello"));
        assert_ne!(digest("hello"), digest("hello!"));
        assert_eq!(digest("hello").len(), 16);
    }

    #[test]
    fn digest_empty() {
        assert_eq!(digest("").len(), 16);
    }

    // ── decision_field ──────────────────────────────────────────────────────
    #[test]
    fn decision_field_reads_all_shapes() {
        assert_eq!(decision_field(&json!({"hookSpecificOutput": {"permissionDecision": "deny"}})), Some("deny"));
        assert_eq!(decision_field(&json!({"permissionDecision": "allow"})), Some("allow"));
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

    // ── parse_into ──────────────────────────────────────────────────────────
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
        assert_eq!(out[0].command, "echo hi");
    }

    #[test]
    fn parse_into_defaults_matcher_to_star() {
        let text = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"x"}]}]}}"#;
        let mut out = Vec::new();
        parse_into(text, Source::Project, &mut out);
        assert_eq!(out[0].matcher, "*");
    }

    #[test]
    fn parse_into_skips_non_command_hooks() {
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
