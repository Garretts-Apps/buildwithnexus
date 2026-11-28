//! Language-server diagnostics after an edit (roadmap 4.6).
//!
//! With `"lsp": {"rust": {"command": "rust-analyzer", "extensions": ["rs"]}}`
//! bwn starts the server (inside the sandbox, like an MCP server) the first
//! time a file it serves is edited, tells it the file's new text, waits a
//! moment for its diagnostics and adds the errors and warnings to the edit's
//! result, so the model sees what it broke without a build. A project file's
//! `lsp` waits for trust: it names a program to run.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

struct Server {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    rx: Receiver<Value>,
    versions: HashMap<String, i64>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

static SERVERS: std::sync::Mutex<Option<HashMap<String, Option<Server>>>> =
    std::sync::Mutex::new(None);

fn send(stdin: &mut std::process::ChildStdin, msg: &Value) -> std::io::Result<()> {
    let body = msg.to_string();
    write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    stdin.flush()
}

// One framed message from the server, or None at the end of its output.
fn read_message(r: &mut impl BufRead) -> Option<Value> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0u8; len?.min(16 << 20)];
    r.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn uri(p: &Path) -> String {
    format!("file://{}", p.display())
}

fn start(cfg: &Value, cwd: &Path) -> Option<Server> {
    let command = cfg["command"].as_str()?;
    let args: Vec<String> = cfg["args"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let unconfined = cfg["sandbox"].as_bool() == Some(false);
    let mut cmd = crate::sandbox::extension_command(command, &args, cwd, unconfined, true).ok()?;
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        while let Some(m) = read_message(&mut r) {
            if tx.send(m).is_err() {
                break;
            }
        }
    });
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "processId": std::process::id(),
            "rootUri": uri(cwd),
            "capabilities": {"textDocument": {"publishDiagnostics": {}}},
        }}),
    )
    .ok()?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        match rx.recv_timeout(left) {
            Ok(m) if m["id"] == 1 => break,
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
    )
    .ok()?;
    Some(Server {
        child,
        stdin,
        rx,
        versions: HashMap::new(),
    })
}

/// What the language servers say about the files a file-edit tool just
/// changed, as lines to add to its result; None when no server serves them.
pub fn diagnostics_after(tool: &str, input: &Value, cwd: &Path) -> Option<String> {
    let servers = crate::config::load_settings_from_dir(cwd)?.lsp;
    if servers.is_empty() {
        return None;
    }
    let mut out = String::new();
    for path in crate::tools::touched_paths(tool, input, cwd) {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let Some((name, cfg)) = servers.iter().find(|(_, c)| {
            c["extensions"]
                .as_array()
                .is_some_and(|a| a.iter().any(|e| e.as_str() == Some(ext)))
        }) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let wait = Duration::from_millis(cfg["wait_ms"].as_u64().unwrap_or(3000).min(30_000));
        let mut guard = SERVERS.lock().unwrap_or_else(|e| e.into_inner());
        let map = guard.get_or_insert_with(HashMap::new);
        let slot = map.entry(name.clone()).or_insert_with(|| start(cfg, cwd));
        let Some(server) = slot.as_mut() else {
            continue;
        };
        let u = uri(&path);
        let lang = cfg["language_id"].as_str().unwrap_or(name.as_str());
        let sent = match server.versions.get_mut(&u) {
            Some(v) => {
                *v += 1;
                send(
                    &mut server.stdin,
                    &json!({"jsonrpc": "2.0", "method": "textDocument/didChange", "params": {
                        "textDocument": {"uri": u, "version": *v},
                        "contentChanges": [{"text": text}],
                    }}),
                )
            }
            None => {
                server.versions.insert(u.clone(), 1);
                send(
                    &mut server.stdin,
                    &json!({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {
                        "textDocument": {"uri": u, "languageId": lang, "version": 1, "text": text},
                    }}),
                )
            }
        };
        if sent.is_err() {
            *slot = None;
            continue;
        }
        // The diagnostics for this file, the last that arrive in the wait.
        let deadline = Instant::now() + wait;
        let mut diags: Option<Vec<Value>> = None;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match server.rx.recv_timeout(left) {
                Ok(m)
                    if m["method"] == "textDocument/publishDiagnostics"
                        && m["params"]["uri"] == u =>
                {
                    diags = m["params"]["diagnostics"].as_array().cloned();
                    // One answer is usually all a server sends for a change.
                    if diags.is_some() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    *slot = None;
                    break;
                }
            }
        }
        let lines: Vec<String> = diags
            .unwrap_or_default()
            .iter()
            .filter(|d| d["severity"].as_u64().unwrap_or(1) <= 2)
            .take(20)
            .map(|d| {
                let sev = if d["severity"].as_u64() == Some(2) {
                    "warning"
                } else {
                    "error"
                };
                format!(
                    "  {}:{}:{} {sev}: {}",
                    path.strip_prefix(cwd).unwrap_or(&path).display(),
                    d["range"]["start"]["line"].as_u64().unwrap_or(0) + 1,
                    d["range"]["start"]["character"].as_u64().unwrap_or(0) + 1,
                    d["message"]
                        .as_str()
                        .unwrap_or("")
                        .lines()
                        .next()
                        .unwrap_or("")
                )
            })
            .collect();
        if !lines.is_empty() {
            out.push_str(&format!("\n\n[{name} diagnostics]\n{}", lines.join("\n")));
        }
    }
    (!out.is_empty()).then_some(out)
}
