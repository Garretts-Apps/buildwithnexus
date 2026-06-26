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

pub fn defs() -> Vec<ToolDef> {
    vec![
        ToolDef { name: "read_file", description: "Read a UTF-8 text file and return its contents.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}) },
        ToolDef { name: "list_dir", description: "List entries in a directory.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}) },
        ToolDef { name: "write_file", description: "Create or overwrite a file with the given contents.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}) },
        ToolDef { name: "edit_file", description: "Replace the unique occurrence of `old` with `new` in a file.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"old":{"type":"string"},"new":{"type":"string"}},"required":["path","old","new"]}) },
        ToolDef { name: "run_command", description: "Run a shell command in the working directory and return its output.",
            schema: json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}) },
        ToolDef { name: "finish", description: "Signal the task is complete with a short summary for the user.",
            schema: json!({"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"]}) },
    ]
}

// Mutating tools pass through the permission gate; reads never do.
pub fn is_mutating(name: &str) -> bool {
    matches!(name, "write_file" | "edit_file" | "run_command")
}

// A one-line, human-readable preview of what a call will do (shown at the gate).
pub fn preview(name: &str, input: &Value) -> String {
    match name {
        "write_file" => format!("write {}", input["path"].as_str().unwrap_or("?")),
        "edit_file" => format!("edit {}", input["path"].as_str().unwrap_or("?")),
        "run_command" => format!("run: {}", input["command"].as_str().unwrap_or("?")),
        _ => name.to_string(),
    }
}

fn resolve(cwd: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) }
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        s.truncate(max);
        s.push_str("\n…[truncated]");
    }
    s
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
            let count = body.matches(old).count();
            if old.is_empty() || count == 0 {
                return err("`old` text not found in file");
            }
            if count > 1 {
                return err(format!("`old` text is not unique ({count} matches) — add context"));
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
        "finish" => Outcome {
            content: input["summary"].as_str().unwrap_or("done").to_string(),
            is_error: false,
            finished: true,
        },
        other => err(format!("unknown tool: {other}")),
    }
}
