// Model Context Protocol client: persistent stdio / Streamable HTTP
// connections, the initialize handshake, paginated tool discovery, and the
// dispatch of `mcp__<server>__<tool>` calls. One process-wide registry holds
// a connection per configured server for the life of the session; servers
// connect lazily in background threads so startup never waits on them, and a
// hung or crashed server is reported once and dropped from the tool surface.

use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config;

pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
const PREFIX: &str = "mcp__";
// stderr of a stdio server is kept as a rolling buffer for diagnostics.
const STDERR_CAP: usize = 16 * 1024;
// Hard cap on an HTTP response body (JSON or SSE) so a runaway stream can't
// eat memory before the timeout fires.
const MAX_HTTP_BODY: usize = 8 * 1024 * 1024;
// A server that keeps handing out cursors forever is broken; stop paging.
const MAX_LIST_PAGES: usize = 100;
// Margin over the slowest server's timeout when a caller waits for discovery
// (the handshake plus the first list page can each take a full timeout).
const WAIT_MARGIN: Duration = Duration::from_secs(5);

// ── configuration ───────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ServerConfig {
    pub name: String,
    pub transport: Transport,
    pub timeout: Duration,
    pub enabled: bool,
}

impl ServerConfig {
    pub fn transport_label(&self) -> &'static str {
        match self.transport {
            Transport::Stdio { .. } => "stdio",
            Transport::Http { .. } => "http",
        }
    }
}

fn string_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|x| match x.as_str() {
                    Some(s) => s.to_string(),
                    None => x.to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn string_map(v: &Value) -> Vec<(String, String)> {
    v.as_object()
        .map(|m| {
            m.iter()
                .map(|(k, x)| {
                    let val = match x.as_str() {
                        Some(s) => s.to_string(),
                        None => x.to_string(),
                    };
                    (k.clone(), val)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parses one `mcp_servers` entry. `type` is optional: a `url` means
/// Streamable HTTP, a `command` means stdio. The legacy shape
/// `{ "command": "...", "args": [...] }` is still accepted unchanged.
pub fn parse_server(name: &str, v: &Value) -> Result<ServerConfig, String> {
    let Some(obj) = v.as_object() else {
        return Err(format!("mcp_servers.{name} must be a JSON object"));
    };
    let kind = match obj.get("type").and_then(Value::as_str) {
        Some("stdio") => "stdio",
        Some("http") | Some("streamable-http") | Some("streamable_http") => "http",
        Some("sse") => {
            return Err(format!(
                "mcp_servers.{name}: legacy SSE transport is not supported — use a Streamable HTTP endpoint (type \"http\")"
            ))
        }
        Some(other) => {
            return Err(format!(
                "mcp_servers.{name}: unknown type '{other}' (expected \"stdio\" or \"http\")"
            ))
        }
        None if obj.get("url").is_some() => "http",
        None if obj.get("command").is_some() => "stdio",
        None => {
            return Err(format!(
                "mcp_servers.{name}: needs a `command` (stdio) or a `url` (http)"
            ))
        }
    };
    let transport = if kind == "http" {
        let url = obj
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .ok_or_else(|| format!("mcp_servers.{name}: http transport needs a `url`"))?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(format!(
                "mcp_servers.{name}: url must start with http:// or https://"
            ));
        }
        Transport::Http {
            url: url.to_string(),
            headers: string_map(obj.get("headers").unwrap_or(&Value::Null)),
        }
    } else {
        let command = obj
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| format!("mcp_servers.{name}: stdio transport needs a `command`"))?;
        Transport::Stdio {
            command: command.to_string(),
            args: string_list(obj.get("args").unwrap_or(&Value::Null)),
            env: string_map(obj.get("env").unwrap_or(&Value::Null)),
        }
    };
    let timeout = match obj.get("timeout_secs") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_SECS,
        Some(t) => match t.as_u64().or_else(|| t.as_f64().map(|f| f as u64)) {
            Some(n) if n > 0 => n,
            _ => {
                return Err(format!(
                    "mcp_servers.{name}: timeout_secs must be a positive number"
                ))
            }
        },
    };
    let enabled = obj.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    Ok(ServerConfig {
        name: name.to_string(),
        transport,
        timeout: Duration::from_secs(timeout),
        enabled,
    })
}

// ── tool naming ─────────────────────────────────────────────────────────────

// Provider APIs only accept `[A-Za-z0-9_-]` in tool names.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The name a discovered tool is advertised under: `mcp__<server>__<tool>`.
pub fn mangle(server: &str, tool: &str) -> String {
    format!("{PREFIX}{}__{}", sanitize(server), sanitize(tool))
}

pub fn is_mcp_tool(name: &str) -> bool {
    name.starts_with(PREFIX)
}

/// Splits a mangled name back into `(server, tool)`. Names of tools the
/// registry knows are resolved exactly; otherwise the first `__` after the
/// prefix separates server from tool.
pub fn demangle(name: &str) -> Option<(String, String)> {
    if let Some(t) = find_tool(name) {
        return Some((t.server, t.name));
    }
    let rest = name.strip_prefix(PREFIX)?;
    let idx = rest.find("__")?;
    let (server, tool) = (&rest[..idx], &rest[idx + 2..]);
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server.to_string(), tool.to_string()))
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpTool {
    pub server: String,
    /// The server's own tool name (what goes in `tools/call`).
    pub name: String,
    /// The mangled name the model sees.
    pub mangled: String,
    pub description: String,
    pub input_schema: Value,
    /// `annotations.readOnlyHint == true` — allowed under readonly permission.
    pub read_only: bool,
}

pub fn parse_tool(server: &str, v: &Value) -> Option<McpTool> {
    let name = v["name"].as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    let mut schema = match v.get("inputSchema") {
        Some(s) if s.is_object() => s.clone(),
        _ => json!({"type": "object", "properties": {}}),
    };
    // Providers reject a parameters schema without a top-level type.
    if schema.get("type").is_none() {
        schema["type"] = json!("object");
    }
    let description = match v["description"].as_str().map(str::trim) {
        Some(d) if !d.is_empty() => d.to_string(),
        _ => format!("Tool `{name}` on MCP server `{server}`."),
    };
    Some(McpTool {
        server: server.to_string(),
        name: name.to_string(),
        mangled: mangle(server, name),
        description,
        input_schema: schema,
        read_only: v["annotations"]["readOnlyHint"].as_bool() == Some(true),
    })
}

// Tool definitions hold `&'static str`; discovered names are interned once so
// repeated `defs()` calls (one per agent turn) never re-leak.
static INTERNED: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

pub fn intern(s: &str) -> &'static str {
    let mut set = INTERNED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(hit) = set.iter().find(|x| **x == s) {
        return hit;
    }
    let leaked: &'static str = Box::leak(s.to_string().into_boxed_str());
    set.push(leaked);
    leaked
}

// ── JSON-RPC framing ────────────────────────────────────────────────────────

/// One newline-delimited JSON-RPC request line. The body is compact JSON, so
/// it can never contain a raw newline.
pub fn request_line(id: u64, method: &str, params: Option<&Value>) -> String {
    let mut m = json!({"jsonrpc": "2.0", "id": id, "method": method});
    if let Some(p) = params {
        m["params"] = p.clone();
    }
    format!("{m}\n")
}

pub fn notification_line(method: &str, params: Option<&Value>) -> String {
    let mut m = json!({"jsonrpc": "2.0", "method": method});
    if let Some(p) = params {
        m["params"] = p.clone();
    }
    format!("{m}\n")
}

#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// A reply to one of our requests (`result` or `error` present).
    Response {
        id: Value,
        body: Value,
    },
    /// A server-initiated request we must answer (e.g. `ping`).
    Request {
        id: Value,
        method: String,
    },
    Notification {
        method: String,
    },
}

pub fn classify(line: &str) -> Option<Incoming> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    if !v.is_object() {
        return None;
    }
    let method = v["method"].as_str().map(str::to_string);
    let has_id = v.get("id").is_some_and(|id| !id.is_null());
    match (method, has_id) {
        (Some(method), true) => Some(Incoming::Request {
            id: v["id"].clone(),
            method,
        }),
        (Some(method), false) => Some(Incoming::Notification { method }),
        (None, true) => Some(Incoming::Response {
            id: v["id"].clone(),
            body: v,
        }),
        (None, false) => None,
    }
}

#[derive(Debug, PartialEq)]
pub enum RpcError {
    /// The server answered with a JSON-RPC error object: the connection is fine.
    Remote(String),
    /// Timeout, exit, or I/O failure: the connection is unusable.
    Transport(String),
}

impl RpcError {
    pub fn message(&self) -> &str {
        match self {
            RpcError::Remote(m) | RpcError::Transport(m) => m,
        }
    }
}

/// Splits a response body into its result, mapping `error` to `RpcError::Remote`.
pub fn unpack(body: Value) -> Result<Value, RpcError> {
    if let Some(e) = body.get("error") {
        let code = e["code"].as_i64().unwrap_or(0);
        let msg = e["message"].as_str().unwrap_or("unknown error");
        return Err(RpcError::Remote(format!("{msg} (code {code})")));
    }
    Ok(body.get("result").cloned().unwrap_or(Value::Null))
}

// The reply to a server-initiated request: `ping` gets an empty result,
// anything else a method-not-found error so the server never waits on us.
fn answer_server_request(id: &Value, method: &str) -> String {
    let m = if method == "ping" {
        json!({"jsonrpc": "2.0", "id": id, "result": {}})
    } else {
        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not supported by client"}})
    };
    format!("{m}\n")
}

pub trait Rpc: Send {
    fn request(&mut self, method: &str, params: Option<&Value>) -> Result<Value, RpcError>;
    fn notify(&mut self, method: &str, params: Option<&Value>) -> Result<(), RpcError>;
}

// ── stdio transport ─────────────────────────────────────────────────────────

pub struct StdioConn {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    next_id: u64,
    timeout: Duration,
}

impl StdioConn {
    pub fn spawn(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        timeout: Duration,
    ) -> Result<Self, String> {
        let mut child = Command::new(command)
            .args(args)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to spawn `{command}`: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin pipe")?;
        let stdout = child.stdout.take().ok_or("no stdout pipe")?;
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(err_pipe) = child.stderr.take() {
            let buf = Arc::clone(&stderr);
            std::thread::spawn(move || {
                for line in BufReader::new(err_pipe).lines() {
                    let Ok(line) = line else { break };
                    if let Ok(mut b) = buf.lock() {
                        b.push_str(&line);
                        b.push('\n');
                        if b.len() > STDERR_CAP {
                            let cut = b.len() - STDERR_CAP;
                            let at = b
                                .char_indices()
                                .map(|(i, _)| i)
                                .find(|&i| i >= cut)
                                .unwrap_or(b.len());
                            b.drain(..at);
                        }
                    }
                }
            });
        }
        Ok(StdioConn {
            child,
            stdin,
            rx,
            stderr,
            next_id: 1,
            timeout,
        })
    }

    /// The last few stderr lines, for error messages.
    pub fn stderr_tail(&self) -> String {
        let buf = self.stderr.lock().map(|b| b.clone()).unwrap_or_default();
        let lines: Vec<&str> = buf.lines().rev().take(5).collect();
        lines.into_iter().rev().collect::<Vec<_>>().join("\n")
    }

    fn with_stderr(&self, msg: String) -> String {
        let tail = self.stderr_tail();
        if tail.is_empty() {
            msg
        } else {
            format!("{msg}\nserver stderr:\n{tail}")
        }
    }

    fn send_line(&mut self, line: &str) -> Result<(), RpcError> {
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|e| RpcError::Transport(self.with_stderr(format!("server stdin closed: {e}"))))
    }
}

impl Rpc for StdioConn {
    fn request(&mut self, method: &str, params: Option<&Value>) -> Result<Value, RpcError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send_line(&request_line(id, method, params))?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(RpcError::Transport(format!(
                    "no reply to {method} within {}s",
                    self.timeout.as_secs()
                )));
            }
            match self.rx.recv_timeout(remaining) {
                Ok(line) => match classify(&line) {
                    Some(Incoming::Response { id: rid, body }) if rid == json!(id) => {
                        return unpack(body);
                    }
                    Some(Incoming::Request { id, method }) => {
                        // Best effort; a failed answer surfaces on our next write.
                        let _ = self.send_line(&answer_server_request(&id, &method));
                    }
                    _ => {}
                },
                Err(RecvTimeoutError::Timeout) => {
                    return Err(RpcError::Transport(format!(
                        "no reply to {method} within {}s",
                        self.timeout.as_secs()
                    )));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(RpcError::Transport(
                        self.with_stderr(format!("server exited before answering {method}")),
                    ));
                }
            }
        }
    }

    fn notify(&mut self, method: &str, params: Option<&Value>) -> Result<(), RpcError> {
        self.send_line(&notification_line(method, params))
    }
}

impl Drop for StdioConn {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── Streamable HTTP transport ───────────────────────────────────────────────

pub struct HttpConn {
    url: String,
    headers: Vec<(String, String)>,
    session_id: Option<String>,
    agent: ureq::Agent,
    next_id: u64,
}

impl HttpConn {
    pub fn new(url: &str, headers: &[(String, String)], timeout: Duration) -> Self {
        HttpConn {
            url: url.to_string(),
            headers: headers.to_vec(),
            session_id: None,
            agent: ureq::AgentBuilder::new()
                .timeout(timeout)
                .timeout_read(timeout)
                .build(),
            next_id: 1,
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    fn post(&mut self, body: &Value) -> Result<ureq::Response, RpcError> {
        let mut req = self
            .agent
            .post(&self.url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .set("MCP-Protocol-Version", PROTOCOL_VERSION);
        for (k, v) in &self.headers {
            req = req.set(k, v);
        }
        if let Some(sid) = &self.session_id {
            req = req.set("Mcp-Session-Id", sid);
        }
        let resp = match req.send_string(&body.to_string()) {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                let snippet: String = text.chars().take(200).collect();
                let hint = if code == 404 && self.session_id.is_some() {
                    " (session expired)"
                } else {
                    ""
                };
                return Err(RpcError::Transport(format!(
                    "HTTP {code}{hint}: {}",
                    snippet.trim()
                )));
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(RpcError::Transport(format!("{t}")));
            }
        };
        if let Some(sid) = resp.header("mcp-session-id") {
            self.session_id = Some(sid.to_string());
        }
        Ok(resp)
    }
}

/// Finds the response with `id` in a JSON body — a single object, or a
/// batch array.
pub fn response_for(body: &Value, id: u64) -> Option<Value> {
    let want = json!(id);
    match body {
        Value::Array(items) => items.iter().find(|m| m["id"] == want).cloned(),
        Value::Object(_) if body["id"] == want => Some(body.clone()),
        _ => None,
    }
}

/// Reads an SSE stream until the event carrying the response to `id`
/// arrives. Each event's `data:` lines are joined by newlines and parsed as
/// one JSON-RPC message; other events (notifications, server requests) are
/// skipped.
pub fn read_sse_response(reader: impl Read, id: u64) -> Result<Value, RpcError> {
    let mut data = String::new();
    let mut total = 0usize;
    let mut lines = BufReader::new(reader).lines();
    loop {
        let line = match lines.next() {
            Some(Ok(l)) => l,
            Some(Err(e)) => return Err(RpcError::Transport(format!("reading event stream: {e}"))),
            None => {
                // Stream closed — check a trailing event without a blank line.
                if let Ok(v) = serde_json::from_str::<Value>(&data) {
                    if let Some(r) = response_for(&v, id) {
                        return unpack(r);
                    }
                }
                return Err(RpcError::Transport(
                    "event stream ended without a response".into(),
                ));
            }
        };
        total += line.len();
        if total > MAX_HTTP_BODY {
            return Err(RpcError::Transport("event stream exceeded size cap".into()));
        }
        if line.is_empty() {
            if !data.is_empty() {
                if let Ok(v) = serde_json::from_str::<Value>(&data) {
                    if let Some(r) = response_for(&v, id) {
                        return unpack(r);
                    }
                }
                data.clear();
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
        // `event:`, `id:`, `retry:` and comment lines carry nothing we need.
    }
}

impl Rpc for HttpConn {
    fn request(&mut self, method: &str, params: Option<&Value>) -> Result<Value, RpcError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut body = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(p) = params {
            body["params"] = p.clone();
        }
        let resp = self.post(&body)?;
        let ctype = resp
            .header("content-type")
            .unwrap_or("")
            .to_ascii_lowercase();
        if ctype.starts_with("text/event-stream") {
            return read_sse_response(resp.into_reader(), id);
        }
        let mut text = String::new();
        resp.into_reader()
            .take(MAX_HTTP_BODY as u64)
            .read_to_string(&mut text)
            .map_err(|e| RpcError::Transport(format!("reading response: {e}")))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| {
            RpcError::Transport(format!(
                "response is not JSON ({e}): {}",
                text.chars().take(120).collect::<String>().trim()
            ))
        })?;
        match response_for(&v, id) {
            Some(r) => unpack(r),
            None => Err(RpcError::Transport(format!(
                "response did not answer request {id}"
            ))),
        }
    }

    fn notify(&mut self, method: &str, params: Option<&Value>) -> Result<(), RpcError> {
        let mut body = json!({"jsonrpc": "2.0", "method": method});
        if let Some(p) = params {
            body["params"] = p.clone();
        }
        // 202 Accepted with no body is the normal answer; any 2xx is fine.
        self.post(&body).map(|_| ())
    }
}

// ── protocol flows (transport-agnostic) ─────────────────────────────────────

/// `initialize` + `notifications/initialized`. Returns "name version" of the
/// server for status displays.
pub fn handshake(rpc: &mut dyn Rpc) -> Result<String, RpcError> {
    let params = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {"name": "buildwithnexus", "version": env!("CARGO_PKG_VERSION")}
    });
    let res = rpc.request("initialize", Some(&params))?;
    rpc.notify("notifications/initialized", None)?;
    let name = res["serverInfo"]["name"].as_str().unwrap_or("");
    let version = res["serverInfo"]["version"].as_str().unwrap_or("");
    Ok(format!("{name} {version}").trim().to_string())
}

/// `tools/list`, following `nextCursor` until the server stops handing one out.
pub fn list_tools(rpc: &mut dyn Rpc, server: &str) -> Result<Vec<McpTool>, RpcError> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    for _ in 0..MAX_LIST_PAGES {
        let params = cursor.as_ref().map(|c| json!({"cursor": c}));
        let res = rpc.request("tools/list", params.as_ref())?;
        if let Some(tools) = res["tools"].as_array() {
            out.extend(tools.iter().filter_map(|t| parse_tool(server, t)));
        }
        match res["nextCursor"].as_str() {
            Some(c) if !c.is_empty() && seen.insert(c.to_string()) => {
                cursor = Some(c.to_string());
            }
            _ => break,
        }
    }
    Ok(out)
}

/// Flattens a `tools/call` result to `(text, is_error)`: text blocks are
/// concatenated, other content types are summarized in brackets.
pub fn render_result(res: &Value) -> (String, bool) {
    let is_error = res["isError"].as_bool() == Some(true);
    let mut parts: Vec<String> = Vec::new();
    if let Some(items) = res["content"].as_array() {
        for item in items {
            match item["type"].as_str().unwrap_or("") {
                "text" => parts.push(item["text"].as_str().unwrap_or("").to_string()),
                "image" => parts.push("[image omitted]".into()),
                "audio" => parts.push("[audio omitted]".into()),
                "resource" => {
                    let r = &item["resource"];
                    match r["text"].as_str() {
                        Some(t) => parts.push(t.to_string()),
                        None => parts.push(format!(
                            "[resource omitted: {}]",
                            r["uri"].as_str().unwrap_or("?")
                        )),
                    }
                }
                "resource_link" => parts.push(format!(
                    "[resource link: {}]",
                    item["uri"].as_str().unwrap_or("?")
                )),
                other => parts.push(format!("[{other} omitted]")),
            }
        }
    }
    if parts.is_empty() {
        if let Some(s) = res.get("structuredContent") {
            parts.push(s.to_string());
        }
    }
    (parts.join("\n"), is_error)
}

pub fn call_tool(rpc: &mut dyn Rpc, name: &str, args: &Value) -> Result<(String, bool), RpcError> {
    let arguments = if args.is_object() {
        args.clone()
    } else {
        json!({})
    };
    let res = rpc.request(
        "tools/call",
        Some(&json!({"name": name, "arguments": arguments})),
    )?;
    Ok(render_result(&res))
}

// ── session registry ────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Disabled,
    Invalid(String),
    Connecting,
    Connected,
    Failed(String),
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Status::Disabled => "disabled",
            Status::Invalid(_) => "invalid",
            Status::Connecting => "connecting",
            Status::Connected => "connected",
            Status::Failed(_) => "failed",
        }
    }
}

struct ServerState {
    config: Option<ServerConfig>,
    status: Status,
    tools: Vec<McpTool>,
    conn: Option<Box<dyn Rpc>>,
    server_info: String,
}

struct Registry {
    started: bool,
    generation: u64,
    pending: usize,
    servers: BTreeMap<String, Arc<Mutex<ServerState>>>,
    notices: Vec<(String, bool)>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    started: false,
    generation: 0,
    pending: 0,
    servers: BTreeMap::new(),
    notices: Vec::new(),
});
static DISCOVERY_DONE: Condvar = Condvar::new();

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_state(s: &Arc<Mutex<ServerState>>) -> std::sync::MutexGuard<'_, ServerState> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

// A live connection plus the server's "name version" and its tools.
type Connected = (Box<dyn Rpc>, String, Vec<McpTool>);

fn connect(cfg: &ServerConfig) -> Result<Connected, String> {
    let mut conn: Box<dyn Rpc> = match &cfg.transport {
        Transport::Stdio { command, args, env } => {
            Box::new(StdioConn::spawn(command, args, env, cfg.timeout)?)
        }
        Transport::Http { url, headers } => Box::new(HttpConn::new(url, headers, cfg.timeout)),
    };
    let info =
        handshake(conn.as_mut()).map_err(|e| format!("initialize failed: {}", e.message()))?;
    let tools = list_tools(conn.as_mut(), &cfg.name)
        .map_err(|e| format!("tools/list failed: {}", e.message()))?;
    Ok((conn, info, tools))
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Starts connecting every configured server in the background. Idempotent:
/// later calls are no-ops until `reload()`.
pub fn start_background() {
    let servers = config::load_settings()
        .map(|s| s.mcp_servers)
        .unwrap_or_default();
    start_with(&servers);
}

pub fn start_with(servers: &BTreeMap<String, Value>) {
    let mut reg = registry();
    if reg.started {
        return;
    }
    reg.started = true;
    reg.generation += 1;
    let generation = reg.generation;
    for (name, raw) in servers {
        let (config, status) = match parse_server(name, raw) {
            Ok(cfg) if !cfg.enabled => (Some(cfg), Status::Disabled),
            Ok(cfg) => (Some(cfg), Status::Connecting),
            Err(e) => {
                reg.notices.push((format!("mcp: {name} {e}"), false));
                (None, Status::Invalid(e))
            }
        };
        let state = Arc::new(Mutex::new(ServerState {
            config: config.clone(),
            status: status.clone(),
            tools: Vec::new(),
            conn: None,
            server_info: String::new(),
        }));
        reg.servers.insert(name.clone(), Arc::clone(&state));
        if status != Status::Connecting {
            continue;
        }
        let Some(cfg) = config else { continue };
        reg.pending += 1;
        let name = name.clone();
        std::thread::spawn(move || {
            let result = connect(&cfg);
            let mut reg = registry();
            let notice = match result {
                Ok((conn, info, tools)) => {
                    let n = tools.len();
                    let mut st = lock_state(&state);
                    if generation == reg.generation {
                        st.conn = Some(conn);
                        st.tools = tools;
                        st.server_info = info;
                        st.status = Status::Connected;
                    }
                    (
                        format!("mcp: {name} connected, {n} tool{}", plural(n)),
                        true,
                    )
                }
                Err(e) => {
                    let mut st = lock_state(&state);
                    if generation == reg.generation {
                        st.status = Status::Failed(e.clone());
                    }
                    (format!("mcp: {name} failed: {e}"), false)
                }
            };
            if generation == reg.generation {
                reg.pending = reg.pending.saturating_sub(1);
                reg.notices.push(notice);
                DISCOVERY_DONE.notify_all();
            }
        });
    }
}

/// Blocks until every server has finished connecting (or failed), or `max`
/// elapses. Returns true when discovery is complete.
pub fn wait_ready(max: Duration) -> bool {
    let deadline = Instant::now() + max;
    let mut reg = registry();
    while reg.pending > 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let (guard, _) = DISCOVERY_DONE
            .wait_timeout(reg, remaining)
            .unwrap_or_else(|e| e.into_inner());
        reg = guard;
    }
    true
}

/// The longest wait a caller should ever spend on discovery: the slowest
/// server's timeout (handshake) plus a margin.
pub fn wait_budget() -> Duration {
    let reg = registry();
    let slowest = reg
        .servers
        .values()
        .filter_map(|s| lock_state(s).config.as_ref().map(|c| c.timeout))
        .max()
        .unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    slowest + WAIT_MARGIN
}

/// Starts discovery if needed and waits for it, bounded by the timeouts.
pub fn ensure_ready() -> bool {
    start_background();
    let budget = wait_budget();
    wait_ready(budget)
}

/// Startup/disconnect messages queued by background threads, oldest first,
/// as `(message, ok)`.
pub fn drain_notices() -> Vec<(String, bool)> {
    std::mem::take(&mut registry().notices)
}

fn server_states() -> Vec<(String, Arc<Mutex<ServerState>>)> {
    registry()
        .servers
        .iter()
        .map(|(n, s)| (n.clone(), Arc::clone(s)))
        .collect()
}

/// Every tool of every connected server.
pub fn tools() -> Vec<McpTool> {
    let mut out = Vec::new();
    for (_, state) in server_states() {
        let st = lock_state(&state);
        if st.status == Status::Connected {
            out.extend(st.tools.iter().cloned());
        }
    }
    out
}

pub fn find_tool(mangled: &str) -> Option<McpTool> {
    for (_, state) in server_states() {
        let st = lock_state(&state);
        if let Some(t) = st.tools.iter().find(|t| t.mangled == mangled) {
            return Some(t.clone());
        }
    }
    None
}

/// Read-only under the permission gate only when the server says so.
pub fn is_read_only(mangled: &str) -> bool {
    find_tool(mangled).is_some_and(|t| t.read_only)
}

/// Calls `tool` on `server` over its persistent connection. A transport
/// failure marks the server failed, drops its tools for the session, and
/// queues one notice.
pub fn call(server: &str, tool: &str, args: &Value) -> Result<(String, bool), String> {
    let state = {
        let reg = registry();
        reg.servers.get(server).map(Arc::clone)
    };
    let Some(state) = state else {
        return Err(format!(
            "MCP server '{server}' not found in settings.json mcp_servers"
        ));
    };
    let mut st = lock_state(&state);
    match &st.status {
        Status::Connected => {}
        Status::Disabled => return Err(format!("MCP server '{server}' is disabled in settings")),
        Status::Connecting => {
            return Err(format!(
                "MCP server '{server}' is still connecting; retry shortly"
            ))
        }
        Status::Invalid(e) | Status::Failed(e) => {
            return Err(format!("MCP server '{server}' is unavailable: {e}"))
        }
    }
    let Some(conn) = st.conn.as_mut() else {
        return Err(format!("MCP server '{server}' has no connection"));
    };
    match call_tool(conn.as_mut(), tool, args) {
        Ok(r) => Ok(r),
        Err(RpcError::Remote(m)) => Err(format!("MCP server '{server}' error: {m}")),
        Err(RpcError::Transport(m)) => {
            st.conn = None;
            st.tools.clear();
            st.status = Status::Failed(m.clone());
            drop(st);
            registry().notices.push((
                format!("mcp: {server} disconnected ({m}); its tools are gone for this session — /mcp reload to retry"),
                false,
            ));
            Err(format!("MCP server '{server}' disconnected: {m}"))
        }
    }
}

/// Drops every connection and re-reads settings; discovery restarts in the
/// background. Threads still connecting from the previous generation are
/// ignored when they finish.
pub fn reload() {
    let old: Vec<Arc<Mutex<ServerState>>> = {
        let mut reg = registry();
        reg.started = false;
        reg.pending = 0;
        reg.notices.clear();
        std::mem::take(&mut reg.servers).into_values().collect()
    };
    for s in old {
        lock_state(&s).conn = None;
    }
    start_background();
}

/// Kills child servers on the way out.
pub fn shutdown() {
    let old: Vec<Arc<Mutex<ServerState>>> = {
        let mut reg = registry();
        reg.started = false;
        reg.pending = 0;
        std::mem::take(&mut reg.servers).into_values().collect()
    };
    for s in old {
        lock_state(&s).conn = None;
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ServerReport {
    pub name: String,
    pub transport: String,
    pub status: Status,
    pub server_info: String,
    pub tools: Vec<McpTool>,
}

pub fn report() -> Vec<ServerReport> {
    server_states()
        .into_iter()
        .map(|(name, state)| {
            let st = lock_state(&state);
            ServerReport {
                name,
                transport: st
                    .config
                    .as_ref()
                    .map(|c| c.transport_label().to_string())
                    .unwrap_or_else(|| "?".into()),
                status: st.status.clone(),
                server_info: st.server_info.clone(),
                tools: st.tools.clone(),
            }
        })
        .collect()
}

// ── management commands (shared by `/mcp …` and `buildwithnexus mcp …`) ────

fn clip(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max - 1).collect::<String>())
    }
}

/// Status table lines, one per configured server.
pub fn status_lines() -> Vec<String> {
    let reports = report();
    if reports.is_empty() {
        let mut lines = vec![
            "no MCP servers configured".into(),
            "add one: /mcp add <name> <command> [args...]  or  /mcp add <name> --url <url> [--header K=V]"
                .into(),
        ];
        // An unloadable settings file hides every server; say so rather
        // than claiming none exist.
        if config::load_settings().is_none() {
            lines.push(
                "settings could not be loaded, so mcp_servers was ignored — run `buildwithnexus doctor`"
                    .into(),
            );
        }
        return lines;
    }
    let mut lines = Vec::new();
    for r in reports {
        let n = r.tools.len();
        let detail = match &r.status {
            Status::Connected => format!(
                "{n} tool{}{}",
                plural(n),
                if r.server_info.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", r.server_info)
                }
            ),
            Status::Failed(e) | Status::Invalid(e) => clip(e, 100),
            Status::Disabled => "enabled: false".into(),
            Status::Connecting => "discovery in progress".into(),
        };
        lines.push(format!(
            "{:<16} {:<6} {:<10} {detail}",
            r.name,
            r.transport,
            r.status.label()
        ));
    }
    lines
}

/// Detail lines for one server: status, then each tool with its description.
pub fn server_lines(name: &str) -> Result<Vec<String>, String> {
    let r = report()
        .into_iter()
        .find(|r| r.name == name)
        .ok_or_else(|| format!("no MCP server named '{name}' — /mcp to list them"))?;
    let mut lines = vec![format!(
        "{name} ({}) — {}{}",
        r.transport,
        r.status.label(),
        match &r.status {
            Status::Failed(e) | Status::Invalid(e) => format!(": {e}"),
            _ => String::new(),
        }
    )];
    if r.status == Status::Connected {
        if r.tools.is_empty() {
            lines.push("  (no tools)".into());
        }
        for t in &r.tools {
            let tag = if t.read_only { " [read-only]" } else { "" };
            lines.push(format!("  {}{tag}", t.mangled));
            lines.push(format!("      {}", clip(&t.description, 160)));
        }
    }
    Ok(lines)
}

/// Parses `add <name> <command> [args...]` / `add <name> --url <url>
/// [--header K=V]…` into a settings entry. Options (`--url`, `--header`,
/// `--env`, `--timeout`) are read until the first bare token, which starts
/// the stdio command; everything after it belongs to the server.
pub fn parse_add(args: &[String]) -> Result<(String, Value), String> {
    let usage = "usage: mcp add <name> <command> [args...]  |  mcp add <name> --url <url> [--header K=V]...";
    let name = args.first().map(|s| s.trim()).unwrap_or("");
    if name.is_empty() || name.starts_with('-') {
        return Err(usage.into());
    }
    let mut url: Option<String> = None;
    let mut headers = serde_json::Map::new();
    let mut env = serde_json::Map::new();
    let mut timeout: Option<u64> = None;
    let mut i = 1;
    let kv = |flag: &str, raw: &str| -> Result<(String, Value), String> {
        match raw.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() => Ok((k.trim().to_string(), json!(v))),
            _ => Err(format!("{flag} expects KEY=VALUE, got '{raw}'")),
        }
    };
    while i < args.len() {
        let a = args[i].as_str();
        let value = |i: usize| -> Result<&String, String> {
            args.get(i + 1).ok_or_else(|| format!("{a} needs a value"))
        };
        match a {
            "--url" => {
                url = Some(value(i)?.clone());
                i += 2;
            }
            "--header" | "-H" => {
                let (k, v) = kv(a, value(i)?)?;
                headers.insert(k, v);
                i += 2;
            }
            "--env" | "-e" => {
                let (k, v) = kv(a, value(i)?)?;
                env.insert(k, v);
                i += 2;
            }
            "--timeout" => {
                let t: u64 = value(i)?
                    .parse()
                    .ok()
                    .filter(|t| *t > 0)
                    .ok_or("--timeout expects a positive number of seconds")?;
                timeout = Some(t);
                i += 2;
            }
            _ => break,
        }
    }
    let command: Vec<&String> = args[i..].iter().collect();
    let mut entry = serde_json::Map::new();
    match (url, command.is_empty()) {
        (Some(_), false) => {
            return Err("give either --url or a command, not both".into());
        }
        (None, true) => return Err(usage.into()),
        (Some(u), true) => {
            if !(u.starts_with("http://") || u.starts_with("https://")) {
                return Err("--url must start with http:// or https://".into());
            }
            entry.insert("type".into(), json!("http"));
            entry.insert("url".into(), json!(u));
            if !headers.is_empty() {
                entry.insert("headers".into(), Value::Object(headers));
            }
        }
        (None, false) => {
            if !headers.is_empty() {
                return Err("--header only applies to --url servers".into());
            }
            entry.insert("type".into(), json!("stdio"));
            entry.insert("command".into(), json!(command[0]));
            let rest: Vec<&String> = command[1..].to_vec();
            if !rest.is_empty() {
                entry.insert("args".into(), json!(rest));
            }
            if !env.is_empty() {
                entry.insert("env".into(), Value::Object(env));
            }
        }
    }
    if let Some(t) = timeout {
        entry.insert("timeout_secs".into(), json!(t));
    }
    Ok((name.to_string(), Value::Object(entry)))
}

/// Runs one management command and returns the lines to print. `connect`
/// makes `add`/`remove` reload the live session afterwards (the REPL); the
/// CLI passes false so scripting never spawns servers.
pub fn manage(args: &[String], connect: bool) -> Result<Vec<String>, String> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    match sub {
        "list" | "ls" | "status" => {
            ensure_ready();
            Ok(status_lines())
        }
        "reload" => {
            reload();
            wait_ready(wait_budget());
            let mut lines = status_lines();
            lines.extend(drain_notices().into_iter().map(|(m, _)| m));
            Ok(lines)
        }
        "add" => {
            let (name, entry) = parse_add(&args[1..])?;
            config::update_settings_json(|obj| {
                let servers = obj.entry("mcp_servers").or_insert_with(|| json!({}));
                if !servers.is_object() {
                    *servers = json!({});
                }
                servers[&name] = entry.clone();
            })?;
            let mut lines = vec![format!(
                "added MCP server '{name}' to {}",
                config::settings_path().display()
            )];
            if connect {
                reload();
                wait_ready(wait_budget());
                lines.extend(drain_notices().into_iter().map(|(m, _)| m));
            }
            Ok(lines)
        }
        "remove" | "rm" => {
            let name = args.get(1).map(|s| s.trim()).unwrap_or("");
            if name.is_empty() {
                return Err("usage: mcp remove <name>".into());
            }
            let mut found = false;
            config::update_settings_json(|obj| {
                if let Some(servers) = obj.get_mut("mcp_servers").and_then(Value::as_object_mut) {
                    found = servers.remove(name).is_some();
                }
            })?;
            if !found {
                return Err(format!(
                    "no MCP server named '{name}' in {} (project-level settings must be edited by hand)",
                    config::settings_path().display()
                ));
            }
            let mut lines = vec![format!("removed MCP server '{name}'")];
            if connect {
                reload();
                wait_ready(wait_budget());
                lines.extend(drain_notices().into_iter().map(|(m, _)| m));
            }
            Ok(lines)
        }
        "help" | "-h" | "--help" => Ok(vec![
            "mcp                       list servers (transport, status, tool count)".into(),
            "mcp <name>                list a server's tools".into(),
            "mcp add <name> <command> [args...]".into(),
            "mcp add <name> --url <url> [--header K=V]... [--timeout <secs>]".into(),
            "mcp remove <name>".into(),
            "mcp reload                reconnect every server".into(),
        ]),
        name => {
            ensure_ready();
            server_lines(name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/fake_mcp_server.py"
    );

    fn fixture_conn(timeout: Duration) -> StdioConn {
        StdioConn::spawn("python3", &[FIXTURE.to_string()], &[], timeout).expect("spawn fixture")
    }

    // ── config ──────────────────────────────────────────────────────────
    #[test]
    fn parse_infers_transport_and_keeps_legacy_shape() {
        let legacy = parse_server("a", &json!({"command": "npx", "args": ["-y", "x"]})).unwrap();
        assert_eq!(
            legacy.transport,
            Transport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "x".into()],
                env: vec![]
            }
        );
        assert_eq!(legacy.timeout, Duration::from_secs(30));
        assert!(legacy.enabled);
        assert_eq!(legacy.transport_label(), "stdio");

        let http = parse_server(
            "b",
            &json!({"url": "https://h/mcp", "headers": {"Authorization": "Bearer t"}, "timeout_secs": 5, "enabled": false}),
        )
        .unwrap();
        assert_eq!(
            http.transport,
            Transport::Http {
                url: "https://h/mcp".into(),
                headers: vec![("Authorization".into(), "Bearer t".into())]
            }
        );
        assert_eq!(http.timeout, Duration::from_secs(5));
        assert!(!http.enabled);
        assert_eq!(http.transport_label(), "http");

        let env = parse_server(
            "c",
            &json!({"type": "stdio", "command": "x", "env": {"K": "v", "N": 1}}),
        )
        .unwrap();
        assert_eq!(
            env.transport,
            Transport::Stdio {
                command: "x".into(),
                args: vec![],
                env: vec![("K".into(), "v".into()), ("N".into(), "1".into())]
            }
        );
    }

    #[test]
    fn parse_rejects_bad_entries() {
        assert!(parse_server("a", &json!("nope"))
            .unwrap_err()
            .contains("JSON object"));
        assert!(parse_server("a", &json!({}))
            .unwrap_err()
            .contains("command"));
        assert!(parse_server("a", &json!({"type": "http"}))
            .unwrap_err()
            .contains("url"));
        assert!(
            parse_server("a", &json!({"type": "sse", "url": "http://x"}))
                .unwrap_err()
                .contains("SSE")
        );
        assert!(parse_server("a", &json!({"type": "grpc"}))
            .unwrap_err()
            .contains("unknown type"));
        assert!(parse_server("a", &json!({"url": "ftp://x"}))
            .unwrap_err()
            .contains("http://"));
        assert!(
            parse_server("a", &json!({"command": "x", "timeout_secs": 0}))
                .unwrap_err()
                .contains("timeout_secs")
        );
    }

    // ── naming ──────────────────────────────────────────────────────────
    #[test]
    fn mangle_prefixes_and_sanitizes() {
        assert_eq!(mangle("fs", "read_file"), "mcp__fs__read_file");
        assert_eq!(mangle("my server", "a.b/c"), "mcp__my_server__a_b_c");
        assert!(is_mcp_tool("mcp__x__y"));
        assert!(!is_mcp_tool("mcp_call"));
        assert_eq!(
            demangle("mcp__fs__read_file"),
            Some(("fs".into(), "read_file".into()))
        );
        // A tool name containing `__` splits at the first separator.
        assert_eq!(
            demangle("mcp__fs__read__file"),
            Some(("fs".into(), "read__file".into()))
        );
        assert_eq!(demangle("mcp__fs"), None);
        assert_eq!(demangle("read_file"), None);
    }

    #[test]
    fn parse_tool_reads_schema_description_and_read_only_hint() {
        let t = parse_tool(
            "srv",
            &json!({"name": "q", "description": " Query things ", "inputSchema": {"type": "object", "properties": {"x": {"type": "string"}}}, "annotations": {"readOnlyHint": true}}),
        )
        .unwrap();
        assert_eq!(t.mangled, "mcp__srv__q");
        assert_eq!(t.description, "Query things");
        assert_eq!(t.input_schema["properties"]["x"]["type"], "string");
        assert!(t.read_only);

        let bare = parse_tool("srv", &json!({"name": "w"})).unwrap();
        assert!(!bare.read_only);
        assert_eq!(bare.input_schema["type"], "object");
        assert!(bare.description.contains("`w`"));
        // A schema without `type` gets one; providers reject it otherwise.
        let untyped = parse_tool(
            "srv",
            &json!({"name": "u", "inputSchema": {"properties": {}}}),
        )
        .unwrap();
        assert_eq!(untyped.input_schema["type"], "object");
        assert!(parse_tool("srv", &json!({"name": ""})).is_none());
        assert!(parse_tool("srv", &json!({"description": "x"})).is_none());
    }

    #[test]
    fn intern_returns_the_same_pointer_for_equal_strings() {
        let a = intern("mcp__intern__test");
        let b = intern(&String::from("mcp__intern__test"));
        assert!(std::ptr::eq(a, b));
        assert_eq!(a, "mcp__intern__test");
    }

    // ── framing ─────────────────────────────────────────────────────────
    #[test]
    fn request_lines_are_single_line_json_rpc() {
        let line = request_line(7, "tools/list", Some(&json!({"cursor": "a\nb"})));
        assert!(line.ends_with('\n'));
        assert_eq!(line.trim_end().matches('\n').count(), 0);
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "tools/list");
        assert_eq!(v["params"]["cursor"], "a\nb");
        let n: Value =
            serde_json::from_str(&notification_line("notifications/initialized", None)).unwrap();
        assert!(n.get("id").is_none());
        assert!(n.get("params").is_none());
    }

    #[test]
    fn classify_separates_responses_requests_and_notifications() {
        assert_eq!(
            classify(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#),
            Some(Incoming::Response {
                id: json!(1),
                body: json!({"jsonrpc":"2.0","id":1,"result":{}})
            })
        );
        assert_eq!(
            classify(r#"{"jsonrpc":"2.0","id":"s1","method":"ping"}"#),
            Some(Incoming::Request {
                id: json!("s1"),
                method: "ping".into()
            })
        );
        assert_eq!(
            classify(r#"{"jsonrpc":"2.0","method":"notifications/message","params":{}}"#),
            Some(Incoming::Notification {
                method: "notifications/message".into()
            })
        );
        assert_eq!(classify("not json"), None);
        assert_eq!(classify("[1,2]"), None);
        assert_eq!(classify(r#"{"jsonrpc":"2.0"}"#), None);
    }

    #[test]
    fn unpack_maps_error_objects() {
        assert_eq!(
            unpack(json!({"id": 1, "result": {"a": 1}})),
            Ok(json!({"a": 1}))
        );
        assert_eq!(
            unpack(json!({"id": 1, "error": {"code": -32601, "message": "nope"}})),
            Err(RpcError::Remote("nope (code -32601)".into()))
        );
    }

    #[test]
    fn server_requests_get_answered() {
        let ping: Value = serde_json::from_str(&answer_server_request(&json!(3), "ping")).unwrap();
        assert_eq!(ping["id"], 3);
        assert_eq!(ping["result"], json!({}));
        let other: Value = serde_json::from_str(&answer_server_request(
            &json!("x"),
            "sampling/createMessage",
        ))
        .unwrap();
        assert_eq!(other["error"]["code"], -32601);
    }

    #[test]
    fn sse_parsing_finds_the_matching_response() {
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n\
                    : comment\n\
                    data: {\"jsonrpc\":\"2.0\",\n\
                    data:  \"id\":4,\"result\":{\"ok\":true}}\n\n";
        assert_eq!(
            read_sse_response(Cursor::new(body), 4),
            Ok(json!({"ok": true}))
        );
        // Trailing event without the closing blank line still counts.
        let tail = "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":1}";
        assert_eq!(read_sse_response(Cursor::new(tail), 2), Ok(json!(1)));
        let none = "data: {\"jsonrpc\":\"2.0\",\"id\":9,\"result\":1}\n\n";
        assert!(matches!(
            read_sse_response(Cursor::new(none), 2),
            Err(RpcError::Transport(_))
        ));
    }

    #[test]
    fn response_for_handles_batches() {
        let batch = json!([{"id": 1, "result": "a"}, {"id": 2, "result": "b"}]);
        assert_eq!(response_for(&batch, 2).unwrap()["result"], "b");
        assert!(response_for(&batch, 3).is_none());
        assert!(response_for(&json!({"id": 5, "result": 1}), 5).is_some());
        assert!(response_for(&json!({"id": 6, "result": 1}), 5).is_none());
    }

    // ── flows over a scripted transport ─────────────────────────────────
    struct Mock {
        responses: Vec<Value>,
        calls: Vec<(String, Option<Value>)>,
        notified: Vec<String>,
    }

    impl Rpc for Mock {
        fn request(&mut self, method: &str, params: Option<&Value>) -> Result<Value, RpcError> {
            self.calls.push((method.to_string(), params.cloned()));
            if self.responses.is_empty() {
                return Err(RpcError::Transport("script exhausted".into()));
            }
            Ok(self.responses.remove(0))
        }
        fn notify(&mut self, method: &str, _params: Option<&Value>) -> Result<(), RpcError> {
            self.notified.push(method.to_string());
            Ok(())
        }
    }

    #[test]
    fn handshake_sends_initialize_then_initialized() {
        let mut m = Mock {
            responses: vec![json!({"serverInfo": {"name": "srv", "version": "1.2"}})],
            calls: vec![],
            notified: vec![],
        };
        assert_eq!(handshake(&mut m).unwrap(), "srv 1.2");
        let (method, params) = &m.calls[0];
        assert_eq!(method, "initialize");
        let p = params.as_ref().unwrap();
        assert_eq!(p["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(p["clientInfo"]["name"], "buildwithnexus");
        assert_eq!(p["clientInfo"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(p["capabilities"], json!({}));
        assert_eq!(m.notified, vec!["notifications/initialized".to_string()]);
    }

    #[test]
    fn list_tools_follows_next_cursor_until_exhausted() {
        let mut m = Mock {
            responses: vec![
                json!({"tools": [{"name": "a"}], "nextCursor": "p2"}),
                json!({"tools": [{"name": "b"}], "nextCursor": "p3"}),
                json!({"tools": [{"name": "c"}]}),
            ],
            calls: vec![],
            notified: vec![],
        };
        let tools = list_tools(&mut m, "s").unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert_eq!(m.calls.len(), 3);
        assert_eq!(m.calls[0].1, None);
        assert_eq!(m.calls[1].1, Some(json!({"cursor": "p2"})));
        assert_eq!(m.calls[2].1, Some(json!({"cursor": "p3"})));
    }

    #[test]
    fn list_tools_stops_on_repeated_or_empty_cursor() {
        let mut m = Mock {
            responses: vec![
                json!({"tools": [{"name": "a"}], "nextCursor": "same"}),
                json!({"tools": [{"name": "b"}], "nextCursor": "same"}),
                json!({"tools": [{"name": "never"}]}),
            ],
            calls: vec![],
            notified: vec![],
        };
        let tools = list_tools(&mut m, "s").unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(m.calls.len(), 2);

        let mut empty = Mock {
            responses: vec![json!({"tools": [], "nextCursor": ""})],
            calls: vec![],
            notified: vec![],
        };
        assert!(list_tools(&mut empty, "s").unwrap().is_empty());
        assert_eq!(empty.calls.len(), 1);
    }

    #[test]
    fn render_result_flattens_content_and_flags_errors() {
        let (text, is_err) = render_result(&json!({"content": [
            {"type": "text", "text": "one"},
            {"type": "image", "data": "..", "mimeType": "image/png"},
            {"type": "audio", "data": ".."},
            {"type": "resource", "resource": {"uri": "file:///a", "text": "inline"}},
            {"type": "resource", "resource": {"uri": "file:///b", "blob": ".."}},
            {"type": "resource_link", "uri": "https://x"},
            {"type": "text", "text": "two"}
        ]}));
        assert_eq!(
            text,
            "one\n[image omitted]\n[audio omitted]\ninline\n[resource omitted: file:///b]\n[resource link: https://x]\ntwo"
        );
        assert!(!is_err);
        let (t, e) =
            render_result(&json!({"content": [{"type": "text", "text": "bad"}], "isError": true}));
        assert_eq!(t, "bad");
        assert!(e);
        let (s, _) = render_result(&json!({"structuredContent": {"n": 1}}));
        assert_eq!(s, r#"{"n":1}"#);
    }

    #[test]
    fn call_tool_sends_name_and_object_arguments() {
        let mut m = Mock {
            responses: vec![json!({"content": [{"type": "text", "text": "ok"}]})],
            calls: vec![],
            notified: vec![],
        };
        assert_eq!(
            call_tool(&mut m, "t", &json!("not an object")).unwrap(),
            ("ok".into(), false)
        );
        assert_eq!(m.calls[0].1, Some(json!({"name": "t", "arguments": {}})));
    }

    // ── stdio transport against the Python fixture ──────────────────────
    #[test]
    fn stdio_handshake_discovery_and_call_round_trip() {
        let mut conn = fixture_conn(Duration::from_secs(20));
        assert_eq!(handshake(&mut conn).unwrap(), "fake-mcp 0.1");
        let tools = list_tools(&mut conn, "fake").unwrap();
        assert_eq!(tools.len(), 2, "two tools across two pages");
        assert_eq!(tools[0].mangled, "mcp__fake__echo");
        assert!(!tools[0].read_only);
        assert_eq!(tools[1].mangled, "mcp__fake__add");
        assert!(tools[1].read_only);
        assert_eq!(
            call_tool(&mut conn, "echo", &json!({"text": "hi"})).unwrap(),
            ("echo: hi".into(), false)
        );
        assert_eq!(
            call_tool(&mut conn, "add", &json!({"a": 2, "b": 3})).unwrap(),
            ("5".into(), false)
        );
        assert_eq!(
            call_tool(&mut conn, "fail", &json!({})).unwrap(),
            ("boom".into(), true)
        );
        assert_eq!(
            call_tool(&mut conn, "image", &json!({})).unwrap().0,
            "before\n[image omitted]\nafter"
        );
        let err = call_tool(&mut conn, "nope", &json!({})).unwrap_err();
        assert!(matches!(err, RpcError::Remote(ref m) if m.contains("unknown tool")));
        // stderr was drained into the buffer, not lost or blocking.
        let deadline = Instant::now() + Duration::from_secs(5);
        while conn.stderr_tail().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(conn.stderr_tail().contains("fake-mcp: starting"));
    }

    #[test]
    fn stdio_timeout_is_a_transport_error() {
        let mut conn = fixture_conn(Duration::from_millis(300));
        handshake(&mut conn).unwrap();
        let err = call_tool(&mut conn, "sleep", &json!({"seconds": 5})).unwrap_err();
        assert!(matches!(err, RpcError::Transport(ref m) if m.contains("no reply")));
    }

    #[test]
    fn stdio_exit_is_a_transport_error_with_stderr() {
        let mut conn = fixture_conn(Duration::from_secs(10));
        handshake(&mut conn).unwrap();
        conn.notify("exit", None).unwrap();
        let err = conn.request("tools/list", None).unwrap_err();
        let RpcError::Transport(m) = err else {
            panic!("expected transport error");
        };
        assert!(m.contains("exited") || m.contains("stdin closed"), "{m}");
    }

    #[test]
    fn stdio_spawn_failure_is_reported() {
        let err = StdioConn::spawn(
            "definitely-not-a-binary-xyz",
            &[],
            &[],
            Duration::from_secs(1),
        )
        .err()
        .unwrap();
        assert!(err.contains("failed to spawn"));
    }

    // ── HTTP transport against an in-process listener ───────────────────
    fn read_http_request(stream: &mut std::net::TcpStream) -> (Vec<(String, String)>, String) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut first = String::new();
        reader.read_line(&mut first).unwrap();
        let mut headers = Vec::new();
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                let k = k.trim().to_ascii_lowercase();
                let v = v.trim().to_string();
                if k == "content-length" {
                    len = v.parse().unwrap_or(0);
                }
                headers.push((k, v));
            }
        }
        let mut body = vec![0u8; len];
        std::io::Read::read_exact(&mut reader, &mut body).unwrap();
        (headers, String::from_utf8_lossy(&body).into_owned())
    }

    fn header<'a>(h: &'a [(String, String)], k: &str) -> Option<&'a str> {
        h.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    #[test]
    fn http_transport_handles_json_sse_and_session_ids() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::<(Vec<(String, String)>, String)>::new()));
        let seen2 = Arc::clone(&seen);
        std::thread::spawn(move || {
            for (n, stream) in listener.incoming().enumerate() {
                let mut stream = stream.unwrap();
                let req = read_http_request(&mut stream);
                let body: Value = serde_json::from_str(&req.1).unwrap();
                seen2.lock().unwrap().push(req);
                let id = body["id"].clone();
                let resp = match n {
                    // initialize → plain JSON plus a session id.
                    0 => {
                        let b = json!({"jsonrpc": "2.0", "id": id, "result": {"serverInfo": {"name": "h", "version": "9"}}}).to_string();
                        format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: sess-42\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}", b.len())
                    }
                    // notifications/initialized → 202, no body.
                    1 => "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                    // tools/list → SSE with a notification first, then the answer.
                    2 => {
                        let note = json!({"jsonrpc": "2.0", "method": "notifications/message"}).to_string();
                        let ans = json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{"name": "t"}]}}).to_string();
                        let b = format!("event: message\ndata: {note}\n\nevent: message\ndata: {ans}\n\n");
                        format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}", b.len())
                    }
                    _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\nConnection: close\r\n\r\ngone".to_string(),
                };
                stream.write_all(resp.as_bytes()).unwrap();
                stream.flush().unwrap();
                if n >= 3 {
                    break;
                }
            }
        });

        let headers = vec![("Authorization".to_string(), "Bearer tok".to_string())];
        let mut conn = HttpConn::new(
            &format!("http://127.0.0.1:{port}/mcp"),
            &headers,
            Duration::from_secs(5),
        );
        assert_eq!(handshake(&mut conn).unwrap(), "h 9");
        assert_eq!(conn.session_id(), Some("sess-42"));
        let tools = list_tools(&mut conn, "h").unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].mangled, "mcp__h__t");
        let err = conn.request("tools/list", None).unwrap_err();
        assert!(
            matches!(err, RpcError::Transport(ref m) if m.contains("404") && m.contains("session expired"))
        );

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 4);
        let (h0, b0) = &seen[0];
        assert_eq!(header(h0, "authorization"), Some("Bearer tok"));
        assert_eq!(header(h0, "mcp-protocol-version"), Some(PROTOCOL_VERSION));
        assert!(header(h0, "accept").unwrap().contains("text/event-stream"));
        assert_eq!(header(h0, "mcp-session-id"), None);
        assert!(b0.contains("\"initialize\""));
        // Everything after initialize carries the session id back.
        assert_eq!(header(&seen[1].0, "mcp-session-id"), Some("sess-42"));
        assert!(seen[1].1.contains("notifications/initialized"));
        assert_eq!(header(&seen[2].0, "mcp-session-id"), Some("sess-42"));
    }

    // ── registry: one end-to-end pass over the real fixture ─────────────
    // A single test owns the process-wide registry so parallel tests never
    // race on reload; `mcp_call`'s "not found" test only needs its server to
    // be absent, which stays true here.
    #[test]
    fn registry_discovers_gates_calls_and_reports() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "fake".to_string(),
            json!({"command": "python3", "args": [FIXTURE], "timeout_secs": 20}),
        );
        servers.insert("off".to_string(), json!({"command": "x", "enabled": false}));
        servers.insert("bad".to_string(), json!({"type": "grpc"}));
        servers.insert(
            "missing".to_string(),
            json!({"command": "definitely-not-a-binary-xyz"}),
        );
        shutdown();
        start_with(&servers);
        assert!(wait_ready(Duration::from_secs(30)), "discovery finished");

        let names: Vec<String> = tools().into_iter().map(|t| t.mangled).collect();
        assert_eq!(names, ["mcp__fake__echo", "mcp__fake__add"]);
        assert!(is_read_only("mcp__fake__add"));
        assert!(!is_read_only("mcp__fake__echo"));
        assert!(!is_read_only("mcp__unknown__tool"));
        assert!(!crate::tools::is_mutating("mcp__fake__add"));
        assert!(crate::tools::is_mutating("mcp__fake__echo"));
        assert!(crate::tools::is_mutating("mcp__unknown__tool"));
        // Advertised to the model with the server's schema; readonly keeps
        // only the readOnlyHint tool.
        let defs = crate::tools::defs(false);
        let echo = defs.iter().find(|d| d.name == "mcp__fake__echo").unwrap();
        assert_eq!(echo.description, "Echo text back to the caller");
        assert_eq!(echo.schema["required"], json!(["text"]));
        let ro: Vec<&str> = crate::tools::defs_readonly()
            .iter()
            .map(|d| d.name)
            .filter(|n| is_mcp_tool(n))
            .collect();
        assert_eq!(ro, ["mcp__fake__add"]);
        assert_eq!(
            crate::tools::preview("mcp__fake__echo", &json!({})),
            "MCP call: fake/echo"
        );

        assert_eq!(
            call("fake", "echo", &json!({"text": "reg"})).unwrap(),
            ("echo: reg".into(), false)
        );
        let out = crate::tools::run(
            "mcp__fake__add",
            &json!({"a": 40, "b": 2}),
            std::path::Path::new("."),
        );
        assert!(!out.is_error);
        assert_eq!(out.content, "42");
        let failed = crate::tools::run("mcp__fake__fail", &json!({}), std::path::Path::new("."));
        assert!(failed.is_error);
        assert_eq!(failed.content, "boom");
        let legacy = crate::tools::run(
            "mcp_call",
            &json!({"server": "fake", "tool": "echo", "arguments": {"text": "old"}}),
            std::path::Path::new("."),
        );
        assert!(!legacy.is_error);
        assert_eq!(legacy.content, "echo: old");
        assert!(call("off", "x", &json!({}))
            .unwrap_err()
            .contains("disabled"));
        assert!(call("bad", "x", &json!({}))
            .unwrap_err()
            .contains("unavailable"));
        assert!(call("nope", "x", &json!({}))
            .unwrap_err()
            .contains("not found"));

        let notices = drain_notices();
        assert!(notices.contains(&("mcp: fake connected, 2 tools".to_string(), true)));
        assert!(notices
            .iter()
            .any(|(m, ok)| !ok && m.starts_with("mcp: missing failed: failed to spawn")));
        assert!(notices
            .iter()
            .any(|(m, ok)| !ok && m.contains("unknown type 'grpc'")));

        let snapshot = report();
        let by_name = |n: &str| snapshot.iter().find(|r| r.name == n).unwrap().clone();
        assert_eq!(by_name("fake").status, Status::Connected);
        assert_eq!(by_name("fake").server_info, "fake-mcp 0.1");
        assert_eq!(by_name("off").status, Status::Disabled);
        assert!(matches!(by_name("bad").status, Status::Invalid(_)));
        assert!(matches!(by_name("missing").status, Status::Failed(_)));
        let lines = status_lines();
        assert!(lines
            .iter()
            .any(|l| l.starts_with("fake") && l.contains("connected") && l.contains("2 tools")));
        let detail = server_lines("fake").unwrap();
        assert!(detail
            .iter()
            .any(|l| l.contains("mcp__fake__add [read-only]")));
        assert!(server_lines("nope").is_err());

        // A hung call drops the server for the session, once.
        let hung = call("fake", "sleep", &json!({"seconds": 60}));
        assert!(hung.unwrap_err().contains("disconnected"));
        assert!(tools().is_empty());
        assert!(matches!(by_name("fake").status, Status::Connected)); // snapshot from before
        assert!(matches!(
            report().iter().find(|r| r.name == "fake").unwrap().status,
            Status::Failed(_)
        ));
        let n = drain_notices();
        assert_eq!(n.len(), 1);
        assert!(n[0].0.contains("fake disconnected"));
        assert!(call("fake", "echo", &json!({}))
            .unwrap_err()
            .contains("unavailable"));
        shutdown();
    }

    // ── management parsing ──────────────────────────────────────────────
    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parse_add_builds_stdio_and_http_entries() {
        let (name, entry) =
            parse_add(&s(&["fs", "npx", "-y", "@x/server", "--root", "."])).unwrap();
        assert_eq!(name, "fs");
        assert_eq!(
            entry,
            json!({"type": "stdio", "command": "npx", "args": ["-y", "@x/server", "--root", "."]})
        );
        let (_, entry) = parse_add(&s(&[
            "h",
            "--url",
            "https://h/mcp",
            "--header",
            "Authorization=Bearer a=b",
            "--timeout",
            "7",
        ]))
        .unwrap();
        assert_eq!(
            entry,
            json!({"type": "http", "url": "https://h/mcp", "headers": {"Authorization": "Bearer a=b"}, "timeout_secs": 7})
        );
        let (_, entry) = parse_add(&s(&["e", "--env", "TOKEN=x", "cmd"])).unwrap();
        assert_eq!(
            entry,
            json!({"type": "stdio", "command": "cmd", "env": {"TOKEN": "x"}})
        );
    }

    #[test]
    fn parse_add_rejects_malformed_input() {
        assert!(parse_add(&s(&[])).unwrap_err().contains("usage"));
        assert!(parse_add(&s(&["only"])).unwrap_err().contains("usage"));
        assert!(parse_add(&s(&["x", "--url", "ftp://h"]))
            .unwrap_err()
            .contains("http://"));
        assert!(parse_add(&s(&["x", "--url", "http://h", "cmd"]))
            .unwrap_err()
            .contains("not both"));
        assert!(
            parse_add(&s(&["x", "--header", "bad", "--url", "http://h"]))
                .unwrap_err()
                .contains("KEY=VALUE")
        );
        assert!(parse_add(&s(&["x", "--header", "A=b", "cmd"]))
            .unwrap_err()
            .contains("--url"));
        assert!(parse_add(&s(&["x", "--timeout", "0", "cmd"]))
            .unwrap_err()
            .contains("positive"));
        assert!(parse_add(&s(&["x", "--url"]))
            .unwrap_err()
            .contains("needs a value"));
    }
}
