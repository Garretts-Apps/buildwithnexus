// Claude Code-compatible lifecycle hooks. Project hooks run only after the user
// explicitly trusts that folder; a project hook can never grant a permission
// (only deny). Home settings are trusted implicitly.
//
// Hook types supported:
//   type: "command"  — shell command string (existing format)
//   type: "python"   — path to a Python script (uses python3 or python)
//   type: "script"   — any executable script path; runtime detected by extension
//
// Scripts in ~/.buildwithnexus/hooks/<Event>/*.{sh,py} are auto-discovered
// without requiring settings.json entries.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

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

struct Hook {
    event: String,
    matcher: String,
    cmd: HookCmd,
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

pub fn init(cwd: &Path, interactive: bool) {
    let mut list = Vec::new();

    // Explicit hooks from settings files.
    if let Ok(text) = std::fs::read_to_string(config::home().join("settings.json")) {
        parse_into(&text, Source::Home, &mut list);
    }
    let proj = cwd.join(".buildwithnexus/settings.json");
    if let Ok(text) = std::fs::read_to_string(&proj) {
        if project_trusted(cwd, &text, interactive) {
            parse_into(&text, Source::Project, &mut list);
        } else if interactive {
            tui::line(&tui::dim(
                "  (project hooks present but not trusted — skipped)",
            ));
        }
    }

    // Auto-discovered scripts from ~/.buildwithnexus/hooks/<Event>/.
    for event in &[
        "SessionStart",
        "SessionEnd",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
    ] {
        for script in config::discover_hook_scripts(event) {
            list.push(Hook {
                event: event.to_string(),
                matcher: "*".to_string(),
                cmd: HookCmd::Script(script),
                source: Source::Home,
            });
        }
    }

    let _ = HOOKS.set(Hooks { list });
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
                        out.push(Hook {
                            event: event.clone(),
                            matcher: matcher.clone(),
                            cmd,
                            source,
                        });
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

fn commands_for(event: &str, tool: Option<&str>) -> Vec<(HookCmd, Source, String)> {
    let Some(h) = HOOKS.get() else {
        return Vec::new();
    };
    h.list
        .iter()
        .filter(|hk| hk.event == event)
        .filter(|hk| tool.is_none_or(|t| matches(&hk.matcher, t)))
        .map(|hk| (hk.cmd.clone(), hk.source, hk.matcher.clone()))
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

// ── per-folder trust ────────────────────────────────────────────────────────
fn trust_path() -> PathBuf {
    config::home().join("trusted.json")
}

fn digest(s: &str) -> String {
    let mut h: u64 = 5381;
    for b in s.bytes() {
        h = (h << 5).wrapping_add(h).wrapping_add(b as u64);
    }
    format!("{h:016x}")
}

fn project_trusted(cwd: &Path, text: &str, interactive: bool) -> bool {
    let key = cwd
        .canonicalize()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let want = digest(text);
    let mut store: Value = std::fs::read_to_string(trust_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));

    if store[&key].as_str() == Some(want.as_str()) {
        return true;
    }
    if !interactive {
        return false;
    }

    let changed = store.get(&key).is_some();
    tui::line("");
    tui::line(&tui::yellow(&format!(
        "  ⚠ {}/.buildwithnexus/settings.json defines hooks that run shell commands.",
        cwd.display()
    )));
    if changed {
        tui::line(&tui::dim(
            "    (the file changed since you last trusted it)",
        ));
    }
    let ans = tui::ask(&format!(
        "  Trust this folder's hooks? {} ",
        tui::dim("[y/N]")
    ))
    .unwrap_or_default();
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
fn interpreter_for(path: &Path) -> (&'static str, Vec<&'static str>) {
    match path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .as_deref()
    {
        Some("py") | Some("python") => {
            // Prefer python3; fall back to python.
            if std::process::Command::new("python3")
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok()
            {
                ("python3", vec![])
            } else {
                ("python", vec![])
            }
        }
        Some("bash") => ("bash", vec![]),
        // Shell scripts: run as `sh /path/script.sh` — NOT `sh -c /path/script.sh`
        // (the -c form treats the path as a command string, not a script file).
        _ => ("sh", vec![]),
    }
}

fn run_hook_cmd(cmd: &HookCmd, payload: &Value, cwd: &Path) -> (i32, String, String) {
    match cmd {
        HookCmd::Shell(s) => run_shell(s, payload, cwd),
        HookCmd::Script(path) => run_script(path, payload, cwd),
    }
}

fn run_shell(cmd: &str, payload: &Value, cwd: &Path) -> (i32, String, String) {
    let mut c = if cfg!(windows) {
        let mut x = Command::new("cmd");
        x.args(["/C", cmd]);
        x
    } else {
        let mut x = Command::new("sh");
        x.args(["-c", cmd]);
        x
    };
    run_child(c.current_dir(cwd).env("BWN_PROJECT_DIR", cwd), payload)
}

fn run_script(path: &Path, payload: &Value, cwd: &Path) -> (i32, String, String) {
    let (interp, interp_args) = interpreter_for(path);
    let mut c = Command::new(interp);
    for a in interp_args {
        c.arg(a);
    }
    c.arg(path);
    run_child(c.current_dir(cwd).env("BWN_PROJECT_DIR", cwd), payload)
}

fn run_child(c: &mut Command, payload: &Value) -> (i32, String, String) {
    c.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            if !report::is_json() {
                tui::line(&tui::dim(&format!("  [hook] spawn failed: {e}")));
            }
            return (0, String::new(), String::new());
        }
    };
    if let Some(mut sin) = child.stdin.take() {
        let _ = sin.write_all(payload.to_string().as_bytes());
    }
    match child.wait_with_output() {
        Ok(o) => (
            o.status.code().unwrap_or(0),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        ),
        Err(e) => {
            if !report::is_json() {
                tui::line(&tui::dim(&format!("  [hook] wait failed: {e}")));
            }
            (0, String::new(), String::new())
        }
    }
}

fn decision_field(j: &Value) -> Option<&str> {
    j["hookSpecificOutput"]["permissionDecision"]
        .as_str()
        .or_else(|| j["permissionDecision"].as_str())
        .or_else(|| j["decision"].as_str())
}

pub fn pre_tool_use(tool: &str, input: &Value, cwd: &Path) -> PreDecision {
    let payload = json!({
        "hook_event_name": "PreToolUse", "session_id": std::process::id(),
        "tool_name": tool, "tool_input": input, "cwd": cwd.to_string_lossy()
    });
    for (cmd, source, matcher) in commands_for("PreToolUse", Some(tool)) {
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
        let (code, stdout, stderr) = run_hook_cmd(&cmd, &payload, cwd);
        trace::record_visible(
            "hook_result",
            format!("PreToolUse:{tool} exit {code}"),
            json!({
                "event": "PreToolUse",
                "tool": tool,
                "matcher": matcher,
                "source": source_label(source),
                "command": cmd_label(&cmd),
                "exit_code": code,
                "stdout": stdout,
                "stderr": stderr,
            }),
        );
        if code == 2 {
            let r = stderr.trim();
            return PreDecision::Deny(if r.is_empty() {
                "blocked by PreToolUse hook".into()
            } else {
                r.to_string()
            });
        }
        if let Ok(j) = serde_json::from_str::<Value>(&stdout) {
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
    let payload = json!({
        "hook_event_name": "PostToolUse", "session_id": std::process::id(),
        "tool_name": tool, "tool_input": input,
        "tool_response": {"content": response, "is_error": is_error},
        "cwd": cwd.to_string_lossy()
    });
    for (cmd, source, matcher) in cmds {
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
        let (code, stdout, stderr) = run_hook_cmd(&cmd, &payload, cwd);
        trace::record_visible(
            "hook_result",
            format!("PostToolUse:{tool} exit {code}"),
            json!({
                "event": "PostToolUse",
                "tool": tool,
                "matcher": matcher,
                "source": source_label(source),
                "command": cmd_label(&cmd),
                "exit_code": code,
                "stdout": stdout,
                "stderr": stderr,
            }),
        );
    }
}

pub fn user_prompt_submit(prompt: &str, cwd: &Path) -> Result<String, String> {
    let payload = json!({
        "hook_event_name": "UserPromptSubmit", "session_id": std::process::id(),
        "prompt": prompt, "cwd": cwd.to_string_lossy()
    });
    let mut ctx = String::new();
    for (cmd, source, matcher) in commands_for("UserPromptSubmit", None) {
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
        let (code, stdout, stderr) = run_hook_cmd(&cmd, &payload, cwd);
        trace::record_visible(
            "hook_result",
            format!("UserPromptSubmit exit {code}"),
            json!({
                "event": "UserPromptSubmit",
                "matcher": matcher,
                "source": source_label(source),
                "command": cmd_label(&cmd),
                "exit_code": code,
                "stdout": stdout,
                "stderr": stderr,
            }),
        );
        if code == 2 {
            let r = stderr.trim();
            return Err(if r.is_empty() {
                "blocked by UserPromptSubmit hook".into()
            } else {
                r.to_string()
            });
        }
        if !stdout.trim().is_empty() {
            ctx.push_str(stdout.trim());
            ctx.push('\n');
        }
    }
    Ok(ctx)
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
    let cmds = commands_for(event, None);
    if cmds.is_empty() {
        return;
    }
    let payload = json!({"hook_event_name": event, "session_id": std::process::id(), "cwd": cwd.to_string_lossy()});
    for (cmd, source, matcher) in cmds {
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
        let (code, stdout, stderr) = run_hook_cmd(&cmd, &payload, cwd);
        trace::record_visible(
            "hook_result",
            format!("{event} exit {code}"),
            json!({
                "event": event,
                "matcher": matcher,
                "source": source_label(source),
                "command": cmd_label(&cmd),
                "exit_code": code,
                "stdout": stdout,
                "stderr": stderr,
            }),
        );
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
    }

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
