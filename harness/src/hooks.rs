// Claude Code-compatible lifecycle hooks. Users register shell commands in
// settings.json at the same events Claude Code exposes; each hook receives the
// event as JSON on stdin and can block via exit code 2 or a permissionDecision.
// No regex crate: matchers are "*", an exact tool name, or a |-separated list.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::config;

pub enum PreDecision {
    Continue, // defer to the normal permission gate
    Allow,    // hook explicitly approved — skip the gate
    Deny(String),
}

// User settings first, then project (.buildwithnexus/settings.json); both apply.
fn settings_files(cwd: &Path) -> Vec<std::path::PathBuf> {
    vec![config::home().join("settings.json"), cwd.join(".buildwithnexus/settings.json")]
}

fn matches(matcher: &str, tool: &str) -> bool {
    let m = matcher.trim();
    m.is_empty() || m == "*" || m.split('|').any(|p| p.trim() == tool)
}

fn commands_for(event: &str, tool: Option<&str>, cwd: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for f in settings_files(cwd) {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        let Some(groups) = v["hooks"][event].as_array() else { continue };
        for g in groups {
            if let Some(t) = tool {
                if !matches(g["matcher"].as_str().unwrap_or("*"), t) {
                    continue;
                }
            }
            if let Some(hs) = g["hooks"].as_array() {
                for h in hs {
                    if h["type"].as_str() == Some("command") {
                        if let Some(cmd) = h["command"].as_str() {
                            out.push(cmd.to_string());
                        }
                    }
                }
            }
        }
    }
    out
}

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
    for cmd in commands_for("PreToolUse", Some(tool), cwd) {
        let (code, stdout, stderr) = run_one(&cmd, &payload, cwd);
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
                Some("allow") => return PreDecision::Allow,
                _ => {}
            }
        }
    }
    PreDecision::Continue
}

pub fn post_tool_use(tool: &str, input: &Value, response: &str, is_error: bool, cwd: &Path) {
    let cmds = commands_for("PostToolUse", Some(tool), cwd);
    if cmds.is_empty() {
        return;
    }
    let payload = json!({
        "hook_event_name": "PostToolUse", "session_id": std::process::id(),
        "tool_name": tool, "tool_input": input,
        "tool_response": {"content": response, "is_error": is_error},
        "cwd": cwd.to_string_lossy()
    });
    for cmd in cmds {
        let _ = run_one(&cmd, &payload, cwd);
    }
}

// Returns extra context to inject (hook stdout), or Err(reason) if blocked.
pub fn user_prompt_submit(prompt: &str, cwd: &Path) -> Result<String, String> {
    let payload = json!({
        "hook_event_name": "UserPromptSubmit", "session_id": std::process::id(),
        "prompt": prompt, "cwd": cwd.to_string_lossy()
    });
    let mut ctx = String::new();
    for cmd in commands_for("UserPromptSubmit", None, cwd) {
        let (code, stdout, stderr) = run_one(&cmd, &payload, cwd);
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

// SessionStart / Stop / SessionEnd — fire-and-forget notifications.
pub fn notify(event: &str, cwd: &Path) {
    let cmds = commands_for(event, None, cwd);
    if cmds.is_empty() {
        return;
    }
    let payload = json!({"hook_event_name": event, "session_id": std::process::id(), "cwd": cwd.to_string_lossy()});
    for cmd in cmds {
        let _ = run_one(&cmd, &payload, cwd);
    }
}
