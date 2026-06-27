// Black-box integration tests: drive the real `buildwithnexus` binary against an
// in-process mock OpenAI server, asserting on the structured `--json` event
// stream. No network, no live model — every response is scripted, so the agent
// loop, permission gate, hooks, and subagent recursion are exercised
// deterministically. Edge cases (invalid tool args, out-of-cwd reads, sensitive
// paths, catastrophic commands, the iteration cap, the HTTPS guard) get a
// scenario each.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_buildwithnexus");

// ── unique temp dirs (no external deps, no Date/random) ─────────────────────
static SEQ: AtomicU64 = AtomicU64::new(0);
fn tmp(tag: &str) -> PathBuf {
    let id = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("bwn-it-{tag}-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

// ── mock OpenAI server ──────────────────────────────────────────────────────
// Serves `script` POST responses in order (GETs get a canned empty list and do
// not consume the script), then closes. Connection: close per request so the
// pooled client opens a fresh connection each time and we never multiplex.
fn serve(script: Vec<String>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        let mut served = 0usize;
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let method = read_request(&mut stream);
            let body = if method == "POST" {
                let b = script.get(served).cloned().unwrap_or_else(|| finish("auto"));
                served += 1;
                b
            } else {
                r#"{"object":"list","data":[]}"#.to_string()
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            if method == "POST" && served >= script.len() {
                break; // all scripted responses delivered
            }
        }
    });
    port
}

// Read one HTTP request, draining its body, and return the method.
fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() {
        return String::new();
    }
    let method = first.split_whitespace().next().unwrap_or("").to_string();
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    if len > 0 {
        let mut body = vec![0u8; len];
        let _ = reader.read_exact(&mut body);
    }
    method
}

// ── OpenAI chat-completion response builders ────────────────────────────────
fn tool_call(id: &str, name: &str, args: Value) -> String {
    // OpenAI requires `arguments` to be a JSON *string*.
    json!({"choices": [{"message": {"content": "", "tool_calls": [
        {"id": id, "type": "function",
         "function": {"name": name, "arguments": args.to_string()}}
    ]}}]}).to_string()
}

// A tool call whose arguments are deliberately not valid JSON.
fn tool_call_raw_args(id: &str, name: &str, raw_args: &str) -> String {
    json!({"choices": [{"message": {"content": "", "tool_calls": [
        {"id": id, "type": "function",
         "function": {"name": name, "arguments": raw_args}}
    ]}}]}).to_string()
}

fn finish(summary: &str) -> String {
    tool_call("done", "finish", json!({"summary": summary}))
}

// ── harness: write config, run the binary, parse events ─────────────────────
fn write_config(home: &Path, provider: &str, permission: &str, port: u16) {
    let cfg = json!({
        "provider": provider,
        "model": "test-model",
        "permission": permission,
        "base_url": format!("http://127.0.0.1:{port}/v1"),
    });
    std::fs::write(home.join("config.json"), cfg.to_string()).unwrap();
}

struct Run {
    success: bool,
    events: Vec<Value>,
    stderr: String,
}

impl Run {
    fn has_event(&self, ty: &str) -> bool {
        self.events.iter().any(|e| e["type"] == ty)
    }
    fn find(&self, ty: &str) -> Option<&Value> {
        self.events.iter().find(|e| e["type"] == ty)
    }
    // Every event of a type, concatenated, for substring assertions on reasons.
    fn text_of(&self, ty: &str) -> String {
        self.events.iter().filter(|e| e["type"] == ty).map(|e| e.to_string()).collect()
    }
}

fn run(home: &Path, cwd: &Path, task: &str) -> Run {
    let out = Command::new(BIN)
        .args(["--json", "run", task])
        .current_dir(cwd)
        .env("NEXUS_HOME", home)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null()) // non-terminal → anything that would prompt is denied, never hangs
        .output()
        .expect("spawn binary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let events = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    Run {
        success: out.status.success(),
        events,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

// ── scenarios ───────────────────────────────────────────────────────────────

#[test]
fn executes_tool_then_finishes() {
    let home = tmp("home");
    let cwd = tmp("proj");
    let port = serve(vec![
        tool_call("c1", "write_file", json!({"path": "out.txt", "content": "hello world"})),
        finish("wrote the file"),
    ]);
    write_config(&home, "ollama", "auto", port);

    let r = run(&home, &cwd, "create a file");
    assert!(r.success, "stderr: {}", r.stderr);
    assert_eq!(std::fs::read_to_string(cwd.join("out.txt")).unwrap(), "hello world");
    assert_eq!(r.find("finish").unwrap()["summary"], "wrote the file");
    assert!(r.has_event("tool_call"));
    assert!(r.has_event("tool_result"));
}

#[test]
fn invalid_tool_args_are_fed_back_not_executed() {
    let home = tmp("home");
    let cwd = tmp("proj");
    let port = serve(vec![
        tool_call_raw_args("c1", "write_file", "{not valid json"),
        finish("recovered"),
    ]);
    write_config(&home, "ollama", "auto", port);

    let r = run(&home, &cwd, "do a thing");
    assert!(r.success, "stderr: {}", r.stderr);
    assert!(r.has_event("tool_denied"));
    assert!(r.text_of("tool_denied").contains("not valid JSON"));
    // The bogus write must NOT have happened.
    assert!(!cwd.join("out.txt").exists());
}

#[test]
fn readonly_allows_out_of_cwd_read() {
    // Since 0.10.2, read-only mode allows reads anywhere on the filesystem.
    let home = tmp("home");
    let cwd = tmp("proj");
    let port = serve(vec![
        tool_call("c1", "read_file", json!({"path": "/etc/hostname"})),
        finish("done"),
    ]);
    write_config(&home, "ollama", "readonly", port);

    let r = run(&home, &cwd, "read a system file");
    assert!(!r.has_event("tool_denied"), "reads outside cwd should be allowed in readonly mode");
}

#[test]
fn readonly_blocks_mutation() {
    let home = tmp("home");
    let cwd = tmp("proj");
    let port = serve(vec![
        tool_call("c1", "write_file", json!({"path": "x.txt", "content": "nope"})),
        finish("done"),
    ]);
    write_config(&home, "ollama", "readonly", port);

    let r = run(&home, &cwd, "write a file");
    assert!(r.text_of("tool_denied").contains("read-only"));
    assert!(!cwd.join("x.txt").exists());
}

#[test]
fn sensitive_path_auto_denies_without_hanging() {
    let home = tmp("home");
    let cwd = tmp("proj");
    // A `.pem` is sensitive → confirmation required even under `auto`; with no
    // terminal that resolves to a denial instead of blocking forever.
    let port = serve(vec![
        tool_call("c1", "read_file", json!({"path": "server.pem"})),
        finish("done"),
    ]);
    write_config(&home, "ollama", "auto", port);

    let r = run(&home, &cwd, "read the cert");
    assert!(r.has_event("tool_denied"));
    assert!(r.text_of("tool_denied").contains("sensitive"));
}

#[test]
fn catastrophic_command_auto_denies() {
    let home = tmp("home");
    let cwd = tmp("proj");
    let port = serve(vec![
        tool_call("c1", "run_command", json!({"command": "rm -rf /"})),
        finish("done"),
    ]);
    write_config(&home, "ollama", "auto", port);

    let r = run(&home, &cwd, "clean up");
    assert!(r.has_event("tool_denied"));
    assert!(r.text_of("tool_denied").contains("dangerous"));
}

#[test]
fn reaches_iteration_cap_and_exits_nonzero() {
    let home = tmp("home");
    let cwd = tmp("proj");
    // 30 non-finishing tool calls → the loop hits MAX_ITERS and errors out.
    let script: Vec<String> = (0..30)
        .map(|_| tool_call("c", "list_dir", json!({"path": "."})))
        .collect();
    let port = serve(script);
    write_config(&home, "ollama", "auto", port);

    let r = run(&home, &cwd, "loop forever");
    assert!(!r.success, "expected non-zero exit at the iteration cap");
    assert!(r.stderr.contains("step limit") || r.stderr.contains("limit"), "stderr: {}", r.stderr);
}

#[test]
fn https_guard_rejects_keyed_http_endpoint() {
    let home = tmp("home");
    let cwd = tmp("proj");
    // OpenAI is keyed; an http base_url must be refused before any request.
    let cfg = json!({
        "provider": "openai", "model": "gpt-4o", "permission": "auto",
        "base_url": "http://127.0.0.1:9/v1",
    });
    std::fs::write(home.join("config.json"), cfg.to_string()).unwrap();

    let out = Command::new(BIN)
        .args(["--json", "run", "hi"])
        .current_dir(&cwd)
        .env("NEXUS_HOME", &home)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("non-HTTPS"));
}

#[cfg(unix)]
#[test]
fn home_pre_tool_use_hook_can_deny() {
    let home = tmp("home");
    let cwd = tmp("proj");
    // Home hooks are implicitly trusted; this one blocks every write_file.
    let settings = json!({
        "hooks": {
            "PreToolUse": [
                { "matcher": "write_file",
                  "hooks": [{ "type": "command",
                              "command": "echo blocked-by-home-hook >&2; exit 2" }] }
            ]
        }
    });
    std::fs::write(home.join("settings.json"), settings.to_string()).unwrap();
    let port = serve(vec![
        tool_call("c1", "write_file", json!({"path": "out.txt", "content": "x"})),
        finish("done"),
    ]);
    write_config(&home, "ollama", "auto", port);

    let r = run(&home, &cwd, "write a file");
    assert!(r.text_of("tool_denied").contains("blocked-by-home-hook"));
    assert!(!cwd.join("out.txt").exists());
}

#[test]
fn spawn_subagent_recurses_and_returns() {
    let home = tmp("home");
    let cwd = tmp("proj");
    // Parent delegates; subagent finishes; parent then finishes.
    let port = serve(vec![
        tool_call("c1", "spawn_subagent", json!({"task": "do the subtask"})),
        finish("subagent done"),  // depth-1 loop
        finish("parent done"),    // depth-0 loop, after the subagent returns
    ]);
    write_config(&home, "ollama", "auto", port);

    let r = run(&home, &cwd, "delegate something");
    assert!(r.success, "stderr: {}", r.stderr);
    // Two finish events: one from the subagent, one from the parent.
    let finishes: Vec<&Value> = r.events.iter().filter(|e| e["type"] == "finish").collect();
    assert_eq!(finishes.len(), 2);
    assert!(finishes.iter().any(|e| e["summary"] == "parent done"));
    assert!(finishes.iter().any(|e| e["summary"] == "subagent done"));
}

#[test]
fn json_events_are_one_object_per_line() {
    let home = tmp("home");
    let cwd = tmp("proj");
    let port = serve(vec![finish("ok")]);
    write_config(&home, "ollama", "auto", port);

    let r = run(&home, &cwd, "just finish");
    assert!(r.success);
    // Every emitted line parsed as a standalone JSON object with a "type".
    assert!(!r.events.is_empty());
    assert!(r.events.iter().all(|e| e["type"].is_string()));
}
