// Model Context Protocol client: persistent stdio / Streamable HTTP
// connections, the initialize handshake, paginated tool discovery, and the
// dispatch of `mcp__<server>__<tool>` calls. Resources and prompts are a
// separate read-only surface (`mcp_resources`, `mcp_prompts`), listed at
// connect time and never pasted into the prompt. One process-wide registry holds
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
    /// Let the server's `readOnlyHint` annotations skip the permission gate.
    /// Off by default: the hint is the server's own claim, and a malicious
    /// or compromised server can mark a destructive tool read-only.
    pub trust_read_only_hints: bool,
    /// `timeout_secs` was set for this server: a headless run waits for it
    /// that long instead of HEADLESS_WAIT.
    pub timeout_set: bool,
    /// When set, only these tool names are advertised. `None` means every
    /// tool the server lists (the same as before the key existed).
    pub include_tools: Option<Vec<String>>,
    /// Tool names that are never advertised. A name here is dropped even
    /// when `include_tools` also lists it.
    pub exclude_tools: Vec<String>,
    /// `oauth`: a pre-registered client for `mcp login` (http only).
    pub oauth: crate::mcp_auth::OAuthSettings,
}

impl ServerConfig {
    pub fn transport_label(&self) -> &'static str {
        match self.transport {
            Transport::Stdio { .. } => "stdio",
            Transport::Http { .. } => "http",
        }
    }
}

/// `include_tools` / `exclude_tools`: absent is `None` (no list). A present
/// list must be strings; an empty `include_tools` advertises nothing.
fn tool_name_list(
    server: &str,
    key: &str,
    v: Option<&Value>,
) -> Result<Option<Vec<String>>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => {
            let mut names = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                match item.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    Some(name) => names.push(name.to_string()),
                    None => {
                        return Err(format!(
                            "mcp_servers.{server}.{key}[{i}] must be a non-empty tool name"
                        ))
                    }
                }
            }
            Ok(Some(names))
        }
        Some(_) => Err(format!(
            "mcp_servers.{server}.{key} must be a list of tool names"
        )),
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
    if kind == "stdio" && obj.get("oauth").is_some_and(|o| !o.is_null()) {
        return Err(format!(
            "mcp_servers.{name}: oauth applies to http servers only"
        ));
    }
    let oauth = crate::mcp_auth::parse_settings(name, obj.get("oauth"))?;
    let enabled = obj.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let trust_read_only_hints = obj
        .get("trust_read_only_hints")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let include_tools = tool_name_list(name, "include_tools", obj.get("include_tools"))?;
    let exclude_tools =
        tool_name_list(name, "exclude_tools", obj.get("exclude_tools"))?.unwrap_or_default();
    Ok(ServerConfig {
        name: name.to_string(),
        transport,
        timeout: Duration::from_secs(timeout),
        enabled,
        trust_read_only_hints,
        timeout_set: !matches!(obj.get("timeout_secs"), None | Some(Value::Null)),
        include_tools,
        exclude_tools,
        oauth,
    })
}

/// Tools a server config advertises. `include` of `None` keeps every tool;
/// `Some` is an allow-list, and an empty allow-list keeps none. `exclude`
/// always drops a match, including one the allow-list names. A name matches
/// the server's own tool name or the mangled `mcp__<server>__<tool>` name.
/// The second value is how many tools were dropped.
pub fn select_tools(
    tools: Vec<McpTool>,
    include: Option<&[String]>,
    exclude: &[String],
) -> (Vec<McpTool>, usize) {
    let before = tools.len();
    let named = |list: &[String], tool: &McpTool| {
        list.iter().any(|n| n == &tool.name || n == &tool.mangled)
    };
    let kept = tools
        .into_iter()
        .filter(|tool| {
            if named(exclude, tool) {
                return false;
            }
            match include {
                Some(list) => named(list, tool),
                None => true,
            }
        })
        .collect::<Vec<_>>();
    let hidden = before - kept.len();
    (kept, hidden)
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
    /// HTTP 401 with no token that works; holds why (empty when the server
    /// was never signed in to).
    Unauthorized(String),
}

impl RpcError {
    pub fn message(&self) -> &str {
        match self {
            RpcError::Remote(m) | RpcError::Transport(m) | RpcError::Unauthorized(m) => m,
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

// Longest line read from a stdio server: one JSON-RPC message, held to the
// same cap as an HTTP body so a server that never sends a newline cannot
// grow memory without bound.
const MAX_STDIO_LINE: usize = MAX_HTTP_BODY;

// One line from `r` without its line ending, decoded lossily so a stray
// non-UTF-8 byte costs that message instead of the whole connection.
// Ok(None) at EOF; Err when the line runs past `cap`.
fn read_capped_line<R: BufRead>(r: &mut R, cap: usize) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    let n = r.take(cap as u64 + 1).read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    } else if buf.len() > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("a line over {} MB", cap / (1024 * 1024)),
        ));
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

pub struct StdioConn {
    child: Child,
    stdin: ChildStdin,
    // A line from the server, or why the reader gave up on it.
    rx: Receiver<Result<String, String>>,
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
        let mut cmd = Command::new(command);
        // A stdio server no longer inherits the provider keys and other
        // credential-shaped variables (the same set run_command strips):
        // a server is third-party code and gets only the secrets meant for
        // it. Those come from the entry's own `env` map, applied after the
        // scrub so a declared variable always reaches it, or from
        // `shell_env_passthrough` for a variable to inherit from the shell.
        crate::sandbox::scrub_credentials(&mut cmd);
        let mut child = cmd
            .args(args)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to spawn `{command}`: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin pipe")?;
        let stdout = child.stdout.take().ok_or("no stdout pipe")?;
        // Bounded, so a server flooding notifications between our requests
        // waits on its own stdout instead of queueing in our memory.
        let (tx, rx) = mpsc::sync_channel::<Result<String, String>>(16);
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            loop {
                match read_capped_line(&mut r, MAX_STDIO_LINE) {
                    Ok(Some(line)) => {
                        if tx.send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        // The stream is out of step now; the pending request
                        // fails with this and the connection is done.
                        let _ = tx.send(Err(format!("server sent {e}")));
                        break;
                    }
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(err_pipe) = child.stderr.take() {
            let buf = Arc::clone(&stderr);
            std::thread::spawn(move || {
                // Keep reading past bad bytes and huge lines: a server whose
                // stderr nobody drains blocks once the pipe fills.
                let mut r = BufReader::new(err_pipe);
                loop {
                    let line = match read_capped_line(&mut r, STDERR_CAP) {
                        Ok(Some(line)) => line,
                        Ok(None) => break,
                        Err(_) => "[a stderr line over the cap was dropped]".to_string(),
                    };
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
                Ok(Err(e)) => {
                    return Err(RpcError::Transport(
                        self.with_stderr(format!("{e} while waiting for {method}")),
                    ));
                }
                Ok(Ok(line)) => match classify(&line) {
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
    agent: crate::net::Agent,
    next_id: u64,
    // OAuth for a server whose settings send no Authorization header.
    auth: Option<crate::mcp_auth::Session>,
}

impl HttpConn {
    pub fn new(url: &str, headers: &[(String, String)], timeout: Duration) -> Self {
        HttpConn {
            url: url.to_string(),
            headers: headers.to_vec(),
            session_id: None,
            agent: crate::net::Client::new(|b| b.timeout(timeout).timeout_read(timeout))
                .agent_for(url)
                .clone(),
            next_id: 1,
            auth: None,
        }
    }

    /// Sends `server`'s saved OAuth token (see mcp_auth) and answers a 401
    /// with a refresh or a needs-login error. An Authorization header in
    /// settings wins: that server is left as configured.
    pub fn with_oauth(mut self, server: &str) -> Self {
        if !has_authorization(&self.headers) {
            self.auth = Some(crate::mcp_auth::Session::new(server, &self.url));
        }
        self
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    fn scrub(&self, s: &str) -> String {
        match &self.auth {
            Some(a) => a.scrub(s),
            None => s.to_string(),
        }
    }

    fn post(&mut self, body: &Value) -> Result<crate::net::Response, RpcError> {
        let mut retried = false;
        loop {
            let bearer = self.auth.as_mut().and_then(|a| a.bearer());
            let mut req = self
                .agent
                .post(&self.url)
                .set("Content-Type", "application/json")
                .set("Accept", "application/json, text/event-stream")
                .set("MCP-Protocol-Version", PROTOCOL_VERSION);
            for (k, v) in &self.headers {
                req = req.set(k, v);
            }
            if let Some(t) = &bearer {
                req = req.set("Authorization", &format!("Bearer {t}"));
            }
            if let Some(sid) = &self.session_id {
                req = req.set("Mcp-Session-Id", sid);
            }
            let resp = match req.send_string(&body.to_string()) {
                Ok(r) => r,
                // A login is what is missing when a token was refused or
                // the server asks for one; any other 401 (an API key the
                // settings lack) is reported as it came.
                Err(crate::net::Error::Status(401, r))
                    if self.auth.is_some() && (bearer.is_some() || asks_for_bearer(&r)) =>
                {
                    let auth = self.auth.as_mut().expect("checked above");
                    if !retried && auth.recover(bearer.as_deref()) {
                        retried = true;
                        continue;
                    }
                    return Err(RpcError::Unauthorized(auth.login_reason()));
                }
                Err(crate::net::Error::Status(code, r)) => {
                    let text = r.into_string().unwrap_or_default();
                    let snippet: String = text.chars().take(200).collect();
                    let hint = if code == 404 && self.session_id.is_some() {
                        " (session expired)"
                    } else {
                        ""
                    };
                    return Err(RpcError::Transport(
                        self.scrub(&format!("HTTP {code}{hint}: {}", snippet.trim())),
                    ));
                }
                Err(crate::net::Error::Transport(t)) => {
                    return Err(RpcError::Transport(self.scrub(&format!("{t}"))));
                }
            };
            if let Some(sid) = resp.header("mcp-session-id") {
                self.session_id = Some(sid.to_string());
            }
            return Ok(resp);
        }
    }
}

fn asks_for_bearer(r: &crate::net::Response) -> bool {
    r.all("www-authenticate").iter().any(|h| {
        h.split(|c: char| c == ',' || c.is_whitespace())
            .any(|w| w.eq_ignore_ascii_case("bearer"))
    })
}

fn has_authorization(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .any(|(k, _)| k.trim().eq_ignore_ascii_case("authorization"))
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
    // Whatever the server sends back reaches the model, the transcript and
    // the terminal: a server echoing the token it was sent has it scrubbed.
    fn request(&mut self, method: &str, params: Option<&Value>) -> Result<Value, RpcError> {
        let result = self.request_raw(method, params);
        let Some(auth) = &self.auth else {
            return result;
        };
        match result {
            Ok(mut v) => {
                auth.scrub_value(&mut v);
                Ok(v)
            }
            Err(RpcError::Remote(m)) => Err(RpcError::Remote(auth.scrub(&m))),
            Err(RpcError::Transport(m)) => Err(RpcError::Transport(auth.scrub(&m))),
            Err(e) => Err(e),
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

impl HttpConn {
    fn request_raw(&mut self, method: &str, params: Option<&Value>) -> Result<Value, RpcError> {
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
}

// ── protocol flows (transport-agnostic) ─────────────────────────────────────

/// What `initialize` said. `resources` and `prompts` are the server's
/// advertised capabilities, not a promise that the lists are non-empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handshake {
    pub info: String,
    pub resources: bool,
    pub prompts: bool,
}

fn capability_on(res: &Value, key: &str) -> bool {
    !matches!(
        res["capabilities"].get(key),
        None | Some(Value::Null) | Some(Value::Bool(false))
    )
}

/// `initialize` + `notifications/initialized`.
pub fn handshake(rpc: &mut dyn Rpc) -> Result<Handshake, RpcError> {
    let params = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {"name": "buildwithnexus", "version": env!("CARGO_PKG_VERSION")}
    });
    let res = rpc.request("initialize", Some(&params))?;
    rpc.notify("notifications/initialized", None)?;
    let name = res["serverInfo"]["name"].as_str().unwrap_or("");
    let version = res["serverInfo"]["version"].as_str().unwrap_or("");
    Ok(Handshake {
        info: format!("{name} {version}").trim().to_string(),
        resources: capability_on(&res, "resources"),
        prompts: capability_on(&res, "prompts"),
    })
}

/// One list method, following `nextCursor` until the server stops handing
/// one out, repeats a cursor, or `MAX_LIST_PAGES` is reached.
fn each_page(
    rpc: &mut dyn Rpc,
    method: &str,
    mut on_page: impl FnMut(&Value),
) -> Result<(), RpcError> {
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    for _ in 0..MAX_LIST_PAGES {
        let params = cursor.as_ref().map(|c| json!({"cursor": c}));
        let res = rpc.request(method, params.as_ref())?;
        on_page(&res);
        match res["nextCursor"].as_str() {
            Some(c) if !c.is_empty() && seen.insert(c.to_string()) => {
                cursor = Some(c.to_string());
            }
            _ => return Ok(()),
        }
    }
    Ok(())
}

/// `tools/list`, following `nextCursor` until the server stops handing one out.
pub fn list_tools(rpc: &mut dyn Rpc, server: &str) -> Result<Vec<McpTool>, RpcError> {
    let mut out = Vec::new();
    each_page(rpc, "tools/list", |res| {
        if let Some(tools) = res["tools"].as_array() {
            out.extend(tools.iter().filter_map(|t| parse_tool(server, t)));
        }
    })?;
    Ok(out)
}

/// A resource or a URI template the server published. A template is not a
/// readable URI; the model fills it and passes the result to `resources/read`.
#[derive(Clone, Debug, PartialEq)]
pub struct McpResource {
    pub server: String,
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime: String,
    pub template: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PromptArg {
    pub name: String,
    pub description: String,
    pub required: bool,
}

/// A prompt template the server published.
#[derive(Clone, Debug, PartialEq)]
pub struct McpPrompt {
    pub server: String,
    pub name: String,
    pub description: String,
    pub arguments: Vec<PromptArg>,
}

fn non_empty(v: &Value) -> Option<&str> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn parse_resource(server: &str, v: &Value, template: bool) -> Option<McpResource> {
    let uri = non_empty(if template {
        &v["uriTemplate"]
    } else {
        &v["uri"]
    })?;
    let name = non_empty(&v["name"]).unwrap_or(uri);
    Some(McpResource {
        server: server.to_string(),
        uri: uri.to_string(),
        name: name.to_string(),
        description: non_empty(&v["description"]).unwrap_or("").to_string(),
        mime: non_empty(&v["mimeType"]).unwrap_or("").to_string(),
        template,
    })
}

fn parse_prompt(server: &str, v: &Value) -> Option<McpPrompt> {
    let name = non_empty(&v["name"])?;
    let arguments = v["arguments"]
        .as_array()
        .map(|args| {
            args.iter()
                .filter_map(|a| {
                    let name = non_empty(&a["name"])?;
                    Some(PromptArg {
                        name: name.to_string(),
                        description: non_empty(&a["description"]).unwrap_or("").to_string(),
                        required: a["required"].as_bool() == Some(true),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(McpPrompt {
        server: server.to_string(),
        name: name.to_string(),
        description: non_empty(&v["description"]).unwrap_or("").to_string(),
        arguments,
    })
}

fn list_named(
    rpc: &mut dyn Rpc,
    method: &str,
    key: &str,
    mut take: impl FnMut(&Value),
) -> Result<(), RpcError> {
    each_page(rpc, method, |res| {
        if let Some(items) = res[key].as_array() {
            for item in items {
                take(item);
            }
        }
    })
}

/// `resources/list` plus `resources/templates/list` when the server has it.
/// A missing templates method is an empty template list, not a failure:
/// templates are optional even when resources are advertised.
pub fn list_resources(rpc: &mut dyn Rpc, server: &str) -> Result<Vec<McpResource>, RpcError> {
    let mut out = Vec::new();
    list_named(rpc, "resources/list", "resources", |item| {
        if let Some(r) = parse_resource(server, item, false) {
            out.push(r);
        }
    })?;
    match list_named(
        rpc,
        "resources/templates/list",
        "resourceTemplates",
        |item| {
            if let Some(r) = parse_resource(server, item, true) {
                out.push(r);
            }
        },
    ) {
        Ok(()) => {}
        Err(RpcError::Remote(m)) if method_missing(&m) => {}
        Err(e) => return Err(e),
    }
    Ok(out)
}

pub fn list_prompts(rpc: &mut dyn Rpc, server: &str) -> Result<Vec<McpPrompt>, RpcError> {
    let mut out = Vec::new();
    list_named(rpc, "prompts/list", "prompts", |item| {
        if let Some(p) = parse_prompt(server, item) {
            out.push(p);
        }
    })?;
    Ok(out)
}

fn method_missing(message: &str) -> bool {
    message.contains("code -32601") || message.to_ascii_lowercase().contains("method not found")
}

/// Tool-result sized. A resource or prompt larger than this is cut, the same
/// way other tool output is.
const CONTEXT_CAP: usize = 16 * 1024;

fn cap_text(s: String) -> String {
    if s.len() <= CONTEXT_CAP {
        return s;
    }
    let mut out = crate::config::cut_at_char_boundary(&s, CONTEXT_CAP).to_string();
    out.push_str("\n…[truncated]");
    out
}

/// Flattens `resources/read` contents. Text is kept. A blob is named and
/// omitted: it is not a document the model can read as text.
pub fn render_resource_read(res: &Value) -> String {
    let mut parts = Vec::new();
    let items = res["contents"].as_array();
    if let Some(items) = items {
        for item in items {
            let uri = item["uri"].as_str().unwrap_or("?");
            if let Some(text) = item["text"].as_str() {
                if items.len() > 1 {
                    parts.push(format!("{uri}\n{text}"));
                } else {
                    parts.push(text.to_string());
                }
            } else if item.get("blob").is_some() {
                let mime = item["mimeType"]
                    .as_str()
                    .unwrap_or("application/octet-stream");
                parts.push(format!("[blob omitted: {uri} {mime}]"));
            } else {
                parts.push(format!("[empty resource: {uri}]"));
            }
        }
    }
    if parts.is_empty() {
        return "the server returned no content".into();
    }
    cap_text(parts.join("\n\n"))
}

fn render_blocks(items: &[Value]) -> String {
    render_result(&json!({"content": items})).0
}

/// Flattens `prompts/get` messages into role-labelled text.
pub fn render_prompt(res: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(desc) = non_empty(&res["description"]) {
        parts.push(desc.to_string());
    }
    if let Some(msgs) = res["messages"].as_array() {
        for m in msgs {
            let role = m["role"].as_str().unwrap_or("user");
            let body = if let Some(items) = m["content"].as_array() {
                render_blocks(items)
            } else if m["content"].is_object() {
                render_blocks(std::slice::from_ref(&m["content"]))
            } else if let Some(text) = m["content"].as_str() {
                text.to_string()
            } else {
                String::new()
            };
            parts.push(format!("{role}:\n{body}"));
        }
    }
    if parts.is_empty() {
        return "the server returned no prompt".into();
    }
    cap_text(parts.join("\n\n"))
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
    /// The server wants an OAuth login; holds why, as `Unauthorized` does.
    NeedsAuth(String),
}

impl Status {
    /// The error a failed status carries, empty otherwise.
    pub fn detail(&self) -> &str {
        match self {
            Status::Invalid(e) | Status::Failed(e) | Status::NeedsAuth(e) => e,
            _ => "",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Status::Disabled => "disabled",
            Status::Invalid(_) => "invalid",
            Status::Connecting => "connecting",
            Status::Connected => "connected",
            Status::Failed(_) => "failed",
            Status::NeedsAuth(_) => "needs login",
        }
    }
}

struct ServerState {
    config: Option<ServerConfig>,
    status: Status,
    tools: Vec<McpTool>,
    resources: Vec<McpResource>,
    prompts: Vec<McpPrompt>,
    conn: Option<Box<dyn Rpc>>,
    server_info: String,
}

fn clear_surface(st: &mut ServerState) {
    st.conn = None;
    st.tools.clear();
    st.resources.clear();
    st.prompts.clear();
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

// A live connection and what discovery kept. A resources or prompts list
// that fails does not drop the tools: the note says which list failed.
struct Connected {
    conn: Box<dyn Rpc>,
    info: String,
    tools: Vec<McpTool>,
    hidden: usize,
    resources: Vec<McpResource>,
    prompts: Vec<McpPrompt>,
    notes: Vec<String>,
}

fn list_optional<T>(
    notes: &mut Vec<String>,
    label: &str,
    result: Result<Vec<T>, RpcError>,
) -> Vec<T> {
    match result {
        Ok(v) => v,
        Err(e) => {
            notes.push(format!("{label} failed: {}", e.message()));
            Vec::new()
        }
    }
}

// Never signs in: a server that wants a login comes back NeedsAuth, and only
// `mcp login` opens a browser.
fn connect(cfg: &ServerConfig) -> Result<Connected, Status> {
    let mut conn: Box<dyn Rpc> = match &cfg.transport {
        Transport::Stdio { command, args, env } => {
            Box::new(StdioConn::spawn(command, args, env, cfg.timeout).map_err(Status::Failed)?)
        }
        Transport::Http { url, headers } => {
            Box::new(HttpConn::new(url, headers, cfg.timeout).with_oauth(&cfg.name))
        }
    };
    let failed = |step: &str, e: RpcError| match e {
        RpcError::Unauthorized(m) => Status::NeedsAuth(m),
        e => Status::Failed(format!("{step} failed: {}", e.message())),
    };
    let hello = handshake(conn.as_mut()).map_err(|e| failed("initialize", e))?;
    let listed = list_tools(conn.as_mut(), &cfg.name).map_err(|e| failed("tools/list", e))?;
    let (tools, hidden) = select_tools(listed, cfg.include_tools.as_deref(), &cfg.exclude_tools);
    let mut notes = Vec::new();
    let resources = if hello.resources {
        list_optional(
            &mut notes,
            "resources/list",
            list_resources(conn.as_mut(), &cfg.name),
        )
    } else {
        Vec::new()
    };
    let prompts = if hello.prompts {
        list_optional(
            &mut notes,
            "prompts/list",
            list_prompts(conn.as_mut(), &cfg.name),
        )
    } else {
        Vec::new()
    };
    Ok(Connected {
        conn,
        info: hello.info,
        tools,
        hidden,
        resources,
        prompts,
        notes,
    })
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
            resources: Vec::new(),
            prompts: Vec::new(),
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
                Ok(connected) => {
                    let n = connected.tools.len();
                    let context =
                        context_suffix(connected.resources.len(), connected.prompts.len());
                    let notes = if connected.notes.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", connected.notes.join("; "))
                    };
                    let mut st = lock_state(&state);
                    if generation == reg.generation {
                        st.conn = Some(connected.conn);
                        st.tools = connected.tools;
                        st.resources = connected.resources;
                        st.prompts = connected.prompts;
                        st.server_info = connected.info;
                        st.status = Status::Connected;
                    }
                    let hidden = if connected.hidden == 0 {
                        String::new()
                    } else {
                        format!(
                            " ({} hidden by include_tools/exclude_tools)",
                            connected.hidden
                        )
                    };
                    (
                        format!(
                            "mcp: {name} connected, {n} tool{}{context}{hidden}{notes}",
                            crate::report::plural(n)
                        ),
                        true,
                    )
                }
                Err(status) => {
                    let notice = match &status {
                        Status::NeedsAuth(why) => format!(
                            "mcp: {name} needs login: {}",
                            crate::mcp_auth::login_hint(&name, why)
                        ),
                        other => format!("mcp: {name} failed: {}", other.detail()),
                    };
                    let mut st = lock_state(&state);
                    if generation == reg.generation {
                        st.status = status;
                    }
                    (notice, false)
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

/// How long a headless run waits for a server that sets no `timeout_secs`
/// before it starts without that server's tools.
pub const HEADLESS_WAIT: Duration = Duration::from_secs(5);

/// Discovery for a headless run: waits HEADLESS_WAIT (or the longest
/// `timeout_secs` a server sets) and returns one notice per server still
/// connecting, which the run goes on without.
pub fn ensure_ready_headless() -> Vec<String> {
    start_background();
    let wait = server_states()
        .into_iter()
        .filter_map(|(_, s)| {
            lock_state(&s).config.as_ref().map(|c| {
                if c.timeout_set {
                    c.timeout
                } else {
                    HEADLESS_WAIT
                }
            })
        })
        .max()
        .unwrap_or(HEADLESS_WAIT);
    if wait_ready(wait) {
        return Vec::new();
    }
    server_states()
        .into_iter()
        .filter(|(_, s)| lock_state(s).status == Status::Connecting)
        .map(|(name, _)| {
            format!(
                "mcp: {name} not ready after {}s — skipped (timeout_secs in settings)",
                wait.as_secs()
            )
        })
        .collect()
}

/// For a gate refusal of `mangled`: the tool says it is read-only, but its
/// server's hints are not trusted, so here is the setting that would let it
/// run without asking.
pub fn untrusted_read_only_hint(mangled: &str) -> Option<String> {
    for (server, state) in server_states() {
        let st = lock_state(&state);
        if let Some(t) = st.tools.iter().find(|t| t.mangled == mangled) {
            let trusted = st.config.as_ref().is_some_and(|c| c.trust_read_only_hints);
            return (t.read_only && !trusted).then(|| {
                format!(
                    "{server}/{} says it is read-only; set \"trust_read_only_hints\": true on \
                     mcp_servers.{server} to let its read-only tools run without asking",
                    t.name
                )
            });
        }
    }
    None
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

/// Read-only under the permission gate only when the server says so AND the
/// user opted in to trusting that server's hints (`trust_read_only_hints`).
pub fn is_read_only(mangled: &str) -> bool {
    for (_, state) in server_states() {
        let st = lock_state(&state);
        if let Some(t) = st.tools.iter().find(|t| t.mangled == mangled) {
            let trusted = st.config.as_ref().is_some_and(|c| c.trust_read_only_hints);
            return t.read_only && trusted;
        }
    }
    false
}

/// One request on a connected server. A transport failure marks the server
/// failed and drops its tools, resources and prompts for the session.
fn with_server<T>(
    server: &str,
    f: impl FnOnce(&mut dyn Rpc) -> Result<T, RpcError>,
) -> Result<T, String> {
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
        Status::NeedsAuth(why) => {
            return Err(format!(
                "MCP server '{server}' needs login: {}",
                crate::mcp_auth::login_hint(server, why)
            ))
        }
    }
    let Some(conn) = st.conn.as_mut() else {
        return Err(format!("MCP server '{server}' has no connection"));
    };
    match f(conn.as_mut()) {
        Ok(r) => Ok(r),
        Err(RpcError::Remote(m)) => Err(format!("MCP server '{server}' error: {m}")),
        Err(RpcError::Unauthorized(why)) => {
            clear_surface(&mut st);
            st.status = Status::NeedsAuth(why.clone());
            drop(st);
            let hint = crate::mcp_auth::login_hint(server, &why);
            registry().notices.push((
                format!("mcp: {server} needs login: {hint}; its tools are gone until then"),
                false,
            ));
            Err(format!("MCP server '{server}' needs login: {hint}"))
        }
        Err(RpcError::Transport(m)) => {
            clear_surface(&mut st);
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

/// Calls `tool` on `server` over its persistent connection. A transport
/// failure marks the server failed, drops its tools for the session, and
/// queues one notice.
pub fn call(server: &str, tool: &str, args: &Value) -> Result<(String, bool), String> {
    with_server(server, |rpc| call_tool(rpc, tool, args))
}

fn context_suffix(resources: usize, prompts: usize) -> String {
    match (resources, prompts) {
        (0, 0) => String::new(),
        (r, 0) => format!(", {r} resource{}", crate::report::plural(r)),
        (0, p) => format!(", {p} prompt{}", crate::report::plural(p)),
        (r, p) => format!(
            ", {r} resource{}, {p} prompt{}",
            crate::report::plural(r),
            crate::report::plural(p)
        ),
    }
}

fn connected_states() -> Vec<(String, Arc<Mutex<ServerState>>)> {
    server_states()
        .into_iter()
        .filter(|(_, s)| lock_state(s).status == Status::Connected)
        .collect()
}

/// Resources and URI templates of every connected server.
pub fn resources() -> Vec<McpResource> {
    let mut out = Vec::new();
    for (_, state) in connected_states() {
        out.extend(lock_state(&state).resources.iter().cloned());
    }
    out
}

/// Prompts of every connected server.
pub fn prompts() -> Vec<McpPrompt> {
    let mut out = Vec::new();
    for (_, state) in connected_states() {
        out.extend(lock_state(&state).prompts.iter().cloned());
    }
    out
}

fn known_server(name: &str) -> bool {
    server_states().iter().any(|(n, _)| n == name)
}

fn resolve_one(label: &str, owners: &[String]) -> Result<String, String> {
    match owners {
        [] => Err(format!("no connected MCP server has published {label}")),
        [one] => Ok(one.clone()),
        many => Err(format!(
            "{label} is published by {} — pass server to choose one",
            many.join(", ")
        )),
    }
}

fn filter_server(server: Option<&str>, name: &str) -> bool {
    server.is_none_or(|want| want == name)
}

/// The listing `mcp_resources` returns when no URI was passed.
pub fn resource_list_text(server: Option<&str>) -> Result<String, String> {
    if let Some(name) = server {
        if !known_server(name) {
            return Err(format!(
                "MCP server '{name}' not found in settings.json mcp_servers"
            ));
        }
    }
    let items: Vec<McpResource> = resources()
        .into_iter()
        .filter(|r| filter_server(server, &r.server))
        .collect();
    if items.is_empty() {
        return Ok("no MCP server has published a resource".into());
    }
    let mut lines = Vec::new();
    let extra = items.len().saturating_sub(200);
    for r in items.iter().take(200) {
        let mime = if r.mime.is_empty() {
            String::new()
        } else {
            format!("  {}", r.mime)
        };
        let kind = if r.template { "  template" } else { "" };
        lines.push(format!("{}  {}  {}{mime}{kind}", r.server, r.name, r.uri));
        if !r.description.is_empty() {
            lines.push(format!(
                "    {}",
                crate::tools::clamp_preview(&r.description, 160)
            ));
        }
    }
    if extra > 0 {
        lines.push(format!("{extra} more not shown"));
    }
    Ok(lines.join("\n"))
}

/// `resources/read`. A URI that was not listed is still read when `server`
/// names the server (a filled-in template). A blob comes back omitted.
pub fn read_resource(server: Option<&str>, uri: &str) -> Result<String, String> {
    let uri = uri.trim();
    if uri.is_empty() {
        return Err("uri is required".into());
    }
    let target = if let Some(name) = server {
        if !known_server(name) {
            return Err(format!(
                "MCP server '{name}' not found in settings.json mcp_servers"
            ));
        }
        name.to_string()
    } else {
        let owners: Vec<String> = resources()
            .into_iter()
            .filter(|r| !r.template && r.uri == uri)
            .map(|r| r.server)
            .collect();
        resolve_one(&format!("resource {uri}"), &owners)?
    };
    let res = with_server(&target, |rpc| {
        rpc.request("resources/read", Some(&json!({"uri": uri})))
    })?;
    Ok(render_resource_read(&res))
}

/// The listing `mcp_prompts` returns when no name was passed.
pub fn prompt_list_text(server: Option<&str>) -> Result<String, String> {
    if let Some(name) = server {
        if !known_server(name) {
            return Err(format!(
                "MCP server '{name}' not found in settings.json mcp_servers"
            ));
        }
    }
    let items: Vec<McpPrompt> = prompts()
        .into_iter()
        .filter(|p| filter_server(server, &p.server))
        .collect();
    if items.is_empty() {
        return Ok("no MCP server has published a prompt".into());
    }
    let mut lines = Vec::new();
    let extra = items.len().saturating_sub(200);
    for p in items.iter().take(200) {
        let args = if p.arguments.is_empty() {
            String::new()
        } else {
            let names: Vec<String> = p
                .arguments
                .iter()
                .map(|a| {
                    if a.required {
                        a.name.clone()
                    } else {
                        format!("{}?", a.name)
                    }
                })
                .collect();
            format!("  ({})", names.join(", "))
        };
        lines.push(format!("{}  {}{args}", p.server, p.name));
        if !p.description.is_empty() {
            lines.push(format!(
                "    {}",
                crate::tools::clamp_preview(&p.description, 160)
            ));
        }
    }
    if extra > 0 {
        lines.push(format!("{extra} more not shown"));
    }
    Ok(lines.join("\n"))
}

fn string_arguments(v: &Value) -> Value {
    let Some(obj) = v.as_object() else {
        return json!({});
    };
    let mut out = serde_json::Map::new();
    for (k, val) in obj {
        let s = match val {
            Value::String(s) => s.clone(),
            Value::Null => continue,
            other => other.to_string(),
        };
        out.insert(k.clone(), Value::String(s));
    }
    Value::Object(out)
}

/// `prompts/get`. Arguments are sent as strings, which is what the protocol
/// asks for. A required argument missing from a listed prompt is refused
/// before the request.
pub fn get_prompt(server: Option<&str>, name: &str, arguments: &Value) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("name is required".into());
    }
    let args = string_arguments(arguments);
    let listed: Vec<McpPrompt> = prompts()
        .into_iter()
        .filter(|p| p.name == name && filter_server(server, &p.server))
        .collect();
    let target = if let Some(server_name) = server {
        if !known_server(server_name) {
            return Err(format!(
                "MCP server '{server_name}' not found in settings.json mcp_servers"
            ));
        }
        if let Some(prompt) = listed.iter().find(|p| p.server == server_name) {
            missing_prompt_args(prompt, &args)?;
        }
        server_name.to_string()
    } else {
        let owners: Vec<String> = listed.iter().map(|p| p.server.clone()).collect();
        let target = resolve_one(&format!("prompt {name}"), &owners)?;
        if let Some(prompt) = listed.iter().find(|p| p.server == target) {
            missing_prompt_args(prompt, &args)?;
        }
        target
    };
    let res = with_server(&target, |rpc| {
        rpc.request(
            "prompts/get",
            Some(&json!({"name": name, "arguments": args})),
        )
    })?;
    Ok(render_prompt(&res))
}

fn argument_given(args: &Value, name: &str) -> bool {
    args.as_object().is_some_and(|obj| {
        obj.get(name)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
    })
}

fn missing_prompt_args(prompt: &McpPrompt, args: &Value) -> Result<(), String> {
    let missing: Vec<&str> = prompt
        .arguments
        .iter()
        .filter(|a| a.required && !argument_given(args, &a.name))
        .map(|a| a.name.as_str())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "prompt {} needs {}",
            prompt.name,
            missing.join(", ")
        ))
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
    pub resources: Vec<McpResource>,
    pub prompts: Vec<McpPrompt>,
    /// How an http server is signed in to; None for stdio.
    pub auth: Option<String>,
}

// "signed in (token expires in 50m)", "not signed in", or the header that
// stands in for a login. Never the token.
fn auth_state(cfg: &ServerConfig) -> Option<String> {
    match &cfg.transport {
        Transport::Http { headers, .. } if has_authorization(headers) => {
            Some("Authorization header from settings".into())
        }
        Transport::Http { url, .. } => Some(crate::mcp_auth::describe(&cfg.name, url)),
        Transport::Stdio { .. } => None,
    }
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
                resources: st.resources.clone(),
                prompts: st.prompts.clone(),
                auth: st.config.as_ref().and_then(auth_state),
            }
        })
        .collect()
}

// ── management commands (shared by `/mcp …` and `buildwithnexus mcp …`) ────

/// Status table lines, one per configured server.
fn status_lines() -> Vec<String> {
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
        // A login is worth a mention only where there is one.
        let signed_in = r
            .auth
            .as_deref()
            .filter(|a| a.starts_with("signed in"))
            .map(|a| format!(" · {a}"))
            .unwrap_or_default();
        let detail = match &r.status {
            Status::Connected => format!(
                "{n} tool{}{}{}{signed_in}",
                crate::report::plural(n),
                context_suffix(r.resources.len(), r.prompts.len()),
                if r.server_info.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", r.server_info)
                }
            ),
            Status::Failed(e) | Status::Invalid(e) => crate::tools::clamp_preview(e, 100),
            Status::NeedsAuth(why) => {
                crate::tools::clamp_preview(&crate::mcp_auth::login_hint(&r.name, why), 100)
            }
            Status::Disabled => "enabled: false".into(),
            Status::Connecting => "discovery in progress".into(),
        };
        lines.push(format!(
            "{:<16} {:<6} {:<11} {detail}",
            r.name,
            r.transport,
            r.status.label()
        ));
    }
    lines
}

/// Detail lines for one server: status, then each tool with its description.
fn server_lines(name: &str) -> Result<Vec<String>, String> {
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
            Status::NeedsAuth(why) => format!(": {}", crate::mcp_auth::login_hint(name, why)),
            _ => String::new(),
        }
    )];
    if let Some(auth) = &r.auth {
        lines.push(format!("  auth: {auth}"));
    }
    if r.status == Status::Connected {
        if r.tools.is_empty() {
            lines.push("  (no tools)".into());
        }
        for t in &r.tools {
            let tag = if t.read_only { " [read-only]" } else { "" };
            lines.push(format!("  {}{tag}", t.mangled));
            lines.push(format!(
                "      {}",
                crate::tools::clamp_preview(&t.description, 160)
            ));
        }
        for res in &r.resources {
            let kind = if res.template { " template" } else { "" };
            lines.push(format!("  resource {} {}{kind}", res.name, res.uri));
        }
        for prompt in &r.prompts {
            lines.push(format!("  prompt {}", prompt.name));
        }
    }
    Ok(lines)
}

/// Parses `add <name> <command> [args...]` / `add <name> --url <url>
/// [--header K=V]…` into a settings entry. Options (`--url`, `--header`,
/// `--env`, `--timeout`, `--client-id`, `--callback-port`) are read until the
/// first bare token, which starts the stdio command; everything after it
/// belongs to the server.
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
    let mut oauth = serde_json::Map::new();
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
            // Claude Code's form: `mcp add <name> -- <command> [args...]`.
            "--" => {
                i += 1;
                break;
            }
            "--client-id" => {
                oauth.insert("client_id".into(), json!(value(i)?));
                i += 2;
            }
            "--callback-port" => {
                let p: u16 = value(i)?
                    .parse()
                    .ok()
                    .filter(|p| *p > 0)
                    .ok_or("--callback-port expects a port number")?;
                oauth.insert("callback_port".into(), json!(p));
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
            if !oauth.is_empty() {
                entry.insert("oauth".into(), Value::Object(oauth));
            }
        }
        (None, false) => {
            if !headers.is_empty() {
                return Err("--header only applies to --url servers".into());
            }
            if !oauth.is_empty() {
                return Err("--client-id and --callback-port only apply to --url servers".into());
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

/// The end of the error `mcp add` gives for a name already configured.
pub const EXISTS: &str = "already exists — use --force to replace it";

// `--force` anywhere before the server's own command line, removed.
fn take_force(args: &[String]) -> (bool, Vec<String>) {
    let mut force = false;
    let mut out = Vec::new();
    let mut it = args.iter();
    let mut named = false;
    while let Some(a) = it.next() {
        if a == "--force" {
            force = true;
            continue;
        }
        out.push(a.clone());
        if !named {
            named = true;
            continue;
        }
        match a.as_str() {
            "--url" | "--header" | "-H" | "--env" | "-e" | "--timeout" | "--client-id"
            | "--callback-port" => {
                out.extend(it.next().cloned());
            }
            _ => {
                out.extend(it.cloned());
                break;
            }
        }
    }
    (force, out)
}

// How `mcp login` / `mcp logout` errors start when the command was right
// but could not be carried out.
const LOGIN_FAILED: &str = "login failed";
const LOGOUT_FAILED: &str = "logout failed";
const NOT_SIGNED_IN: &str = "not signed in to";

/// The exit status for a `buildwithnexus mcp` error: 1 when the command was
/// fine but could not be carried out, 2 for a usage mistake.
pub fn exit_code(err: &str) -> i32 {
    let failed = [LOGIN_FAILED, LOGOUT_FAILED, NOT_SIGNED_IN]
        .iter()
        .any(|p| err.starts_with(p));
    if failed || err.ends_with(EXISTS) {
        1
    } else {
        2
    }
}

/// Runs one management command and returns the lines to print. `connect`
/// makes `add`/`remove`/`login` reload the live session afterwards (the
/// REPL); the CLI passes false so scripting never spawns servers. `say`
/// prints progress a command shows before it returns (the login URL).
pub fn manage(
    args: &[String],
    connect: bool,
    say: &mut dyn FnMut(&str),
) -> Result<Vec<String>, String> {
    // Server names, serverInfo, tool descriptions and error text come from
    // servers and settings files: neutralize them here, once, for both
    // `/mcp` and `buildwithnexus mcp`.
    let safe = |s: String| crate::tui::sanitize_terminal(&s).into_owned();
    let mut say_safe = |s: &str| say(&safe(s.to_string()));
    manage_lines(args, connect, &mut say_safe)
        .map(|lines| lines.into_iter().map(safe).collect())
        .map_err(safe)
}

// The configured server `name`, as settings (user and trusted project
// layers) define it now.
fn configured(name: &str) -> Result<ServerConfig, String> {
    let servers = config::load_settings()
        .map(|s| s.mcp_servers)
        .unwrap_or_default();
    let raw = servers
        .get(name)
        .ok_or_else(|| format!("no MCP server named '{name}' — /mcp to list them"))?;
    parse_server(name, raw)
}

fn login(name: &str, connect: bool, say: &mut dyn FnMut(&str)) -> Result<Vec<String>, String> {
    let cfg = configured(name)?;
    let failed = |e: String| format!("{LOGIN_FAILED} for {name}: {e}");
    let Transport::Http { url, headers } = &cfg.transport else {
        return Err(failed(
            "it is a stdio server; signing in applies to http servers".into(),
        ));
    };
    if has_authorization(headers) {
        return Err(failed(format!(
            "it sends an Authorization header from settings; remove it from \
             mcp_servers.{name}.headers to sign in with OAuth instead"
        )));
    }
    let path = crate::mcp_auth::login(name, url, &cfg.oauth, say).map_err(failed)?;
    let mut lines = vec![format!("signed in to {name}; saved to {}", path.display())];
    if connect {
        reload();
        wait_ready(wait_budget());
        lines.extend(drain_notices().into_iter().map(|(m, _)| m));
    } else {
        // An http server spawns nothing, so the CLI can prove the token.
        lines.push(match connect_once(&cfg) {
            Ok((n, extra)) => {
                format!(
                    "{name} connected, {n} tool{}{extra}",
                    crate::report::plural(n)
                )
            }
            Err(e) => format!("{name} did not connect with the new login: {e}"),
        });
    }
    Ok(lines)
}

fn connect_once(cfg: &ServerConfig) -> Result<(usize, String), String> {
    connect(cfg)
        .map(|c| {
            (
                c.tools.len(),
                context_suffix(c.resources.len(), c.prompts.len()),
            )
        })
        .map_err(|s| match s {
            Status::NeedsAuth(why) if why.is_empty() => "it still asks for a login".into(),
            other => other.detail().to_string(),
        })
}

fn manage_lines(
    args: &[String],
    connect: bool,
    say: &mut dyn FnMut(&str),
) -> Result<Vec<String>, String> {
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
            let (force, add_args) = take_force(&args[1..]);
            let (name, entry) = parse_add(&add_args)?;
            let exists = std::fs::read_to_string(config::settings_path())
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .is_some_and(|v| v["mcp_servers"].get(&name).is_some());
            if exists && !force {
                return Err(format!("{name} {EXISTS}"));
            }
            config::update_settings_json(|obj| {
                let servers = obj.entry("mcp_servers").or_insert_with(|| json!({}));
                if !servers.is_object() {
                    *servers = json!({});
                }
                servers[&name] = entry.clone();
            })?;
            let mut lines = vec![format!(
                "{} MCP server '{name}' {} {}",
                if exists { "replaced" } else { "added" },
                if exists { "in" } else { "to" },
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
        "login" => {
            let name = args.get(1).map(|s| s.trim()).unwrap_or("");
            if name.is_empty() {
                return Err("usage: mcp login <name>".into());
            }
            login(name, connect, say)
        }
        "logout" => {
            let name = args.get(1).map(|s| s.trim()).unwrap_or("");
            if name.is_empty() {
                return Err("usage: mcp logout <name>".into());
            }
            let mut lines = crate::mcp_auth::logout(name)
                .map_err(|e| format!("{LOGOUT_FAILED} for {name}: {e}"))?
                .ok_or_else(|| format!("{NOT_SIGNED_IN} {name}"))?;
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
            "mcp add [--force] <name> <command> [args...]".into(),
            "mcp add <name> --url <url> [--header K=V]... [--timeout <secs>]".into(),
            "        [--client-id <id>] [--callback-port <port>]".into(),
            "mcp remove <name>".into(),
            "mcp login <name>          sign in to an http server that asks for OAuth".into(),
            "mcp logout <name>         revoke and forget that sign-in".into(),
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

    // The Store `python3` stub spawns and exits non-zero. These fixtures need
    // a real interpreter; without one the test is skipped rather than failed.
    fn python3_runs() -> bool {
        Command::new("python3")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

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

    #[test]
    fn tool_filters_are_optional_and_exclude_wins() {
        let open = parse_server("fs", &json!({"command": "fs"})).unwrap();
        assert!(open.include_tools.is_none());
        assert!(open.exclude_tools.is_empty());

        let cfg = parse_server(
            "fs",
            &json!({"command": "fs", "include_tools": [" read ", "write"], "exclude_tools": ["write"]}),
        )
        .unwrap();
        assert_eq!(
            cfg.include_tools.as_deref(),
            Some(["read".to_string(), "write".to_string()].as_slice())
        );
        assert_eq!(cfg.exclude_tools, ["write".to_string()]);

        let tools = vec![
            parse_tool("fs", &json!({"name": "read"})).unwrap(),
            parse_tool("fs", &json!({"name": "write"})).unwrap(),
            parse_tool("fs", &json!({"name": "delete"})).unwrap(),
        ];
        let (kept, hidden) = select_tools(tools.clone(), None, &[]);
        assert_eq!(kept.len(), 3);
        assert_eq!(hidden, 0);
        let (kept, hidden) = select_tools(
            tools.clone(),
            cfg.include_tools.as_deref(),
            &cfg.exclude_tools,
        );
        assert_eq!(
            kept.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["read"]
        );
        assert_eq!(hidden, 2);
        let (kept, _) = select_tools(tools.clone(), Some(&["mcp__fs__delete".into()]), &[]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "delete");
        let (kept, hidden) = select_tools(tools, Some(&[]), &[]);
        assert!(kept.is_empty());
        assert_eq!(hidden, 3);

        assert!(
            parse_server("fs", &json!({"command": "fs", "include_tools": "read"}))
                .unwrap_err()
                .contains("list of tool names")
        );
        assert!(
            parse_server("fs", &json!({"command": "fs", "exclude_tools": [""]}))
                .unwrap_err()
                .contains("non-empty")
        );
    }

    #[test]
    fn oauth_settings_belong_to_http_servers() {
        let cfg = parse_server(
            "h",
            &json!({"url": "https://h/mcp", "oauth": {"client_id": "cli", "callback_port": 9876}}),
        )
        .unwrap();
        assert_eq!(cfg.oauth.client_id.as_deref(), Some("cli"));
        assert_eq!(cfg.oauth.callback_port, Some(9876));
        assert_eq!(
            parse_server("h", &json!({"url": "https://h/mcp"}))
                .unwrap()
                .oauth,
            crate::mcp_auth::OAuthSettings::default()
        );
        assert!(
            parse_server("s", &json!({"command": "x", "oauth": {"client_id": "c"}}))
                .unwrap_err()
                .contains("http servers only")
        );
        assert!(parse_server(
            "h",
            &json!({"url": "https://h", "oauth": {"callback_port": "x"}})
        )
        .unwrap_err()
        .contains("callback_port"));
    }

    #[test]
    fn login_failures_exit_1_and_mistakes_exit_2() {
        assert_eq!(exit_code(&format!("fs {EXISTS}")), 1);
        assert_eq!(exit_code("login failed for h: state mismatch"), 1);
        assert_eq!(exit_code("logout failed for h: cannot remove"), 1);
        assert_eq!(exit_code("not signed in to h"), 1);
        assert_eq!(exit_code("usage: mcp login <name>"), 2);
        assert_eq!(exit_code("no MCP server named 'x' — /mcp to list them"), 2);
        assert_eq!(Status::NeedsAuth(String::new()).label(), "needs login");
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
        let hello = handshake(&mut m).unwrap();
        assert_eq!(hello.info, "srv 1.2");
        assert!(!hello.resources);
        assert!(!hello.prompts);
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

    #[test]
    fn list_resources_pages_and_keeps_templates() {
        let mut m = Mock {
            responses: vec![
                json!({"resources": [{"uri": "mem://a", "name": "A", "description": "one"}], "nextCursor": "p2"}),
                json!({"resources": [{"uri": "mem://b", "name": "B"}]}),
                json!({"resourceTemplates": [{"uriTemplate": "mem://{id}", "name": "by id", "mimeType": "text/plain"}]}),
            ],
            calls: vec![],
            notified: vec![],
        };
        let got = list_resources(&mut m, "s").unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].uri, "mem://a");
        assert!(!got[0].template);
        assert_eq!(got[1].uri, "mem://b");
        assert!(got[2].template);
        assert_eq!(got[2].uri, "mem://{id}");
        assert_eq!(got[2].mime, "text/plain");
        assert_eq!(m.calls[0].0, "resources/list");
        assert_eq!(m.calls[1].1, Some(json!({"cursor": "p2"})));
        assert_eq!(m.calls[2].0, "resources/templates/list");
    }

    #[test]
    fn render_prompt_keeps_text_and_omits_other_blocks() {
        let text = render_prompt(&json!({
            "description": "a review",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image", "data": "..", "mimeType": "image/png"}
                ]
            }]
        }));
        assert_eq!(text, "a review\n\nuser:\nlook\n[image omitted]");
    }

    // ── stdio transport against the Python fixture ──────────────────────
    #[test]
    fn stdio_handshake_discovery_and_call_round_trip() {
        if !python3_runs() {
            eprintln!("skipping: python3 is not a working interpreter");
            return;
        }
        let mut conn = fixture_conn(Duration::from_secs(20));
        let hello = handshake(&mut conn).unwrap();
        assert_eq!(hello.info, "fake-mcp 0.1");
        assert!(hello.resources);
        assert!(hello.prompts);
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
        let resources = list_resources(&mut conn, "fake").unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].uri, "file:///note.txt");
        assert!(!resources[0].template);
        let read = conn
            .request("resources/read", Some(&json!({"uri": "file:///note.txt"})))
            .unwrap();
        assert_eq!(render_resource_read(&read), "hello from the resource");
        let blob = conn
            .request("resources/read", Some(&json!({"uri": "file:///blob.bin"})))
            .unwrap();
        assert_eq!(
            render_resource_read(&blob),
            "[blob omitted: file:///blob.bin application/octet-stream]"
        );
        let prompts = list_prompts(&mut conn, "fake").unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].arguments[0].required);
        let got = conn
            .request(
                "prompts/get",
                Some(&json!({"name": "review", "arguments": {"diff": "one line"}})),
            )
            .unwrap();
        assert_eq!(render_prompt(&got), "user:\nReview this:\none line");
    }

    #[test]
    fn stdio_timeout_is_a_transport_error() {
        if !python3_runs() {
            eprintln!("skipping: python3 is not a working interpreter");
            return;
        }
        let mut conn = fixture_conn(Duration::from_millis(300));
        handshake(&mut conn).unwrap();
        let err = call_tool(&mut conn, "sleep", &json!({"seconds": 5})).unwrap_err();
        assert!(matches!(err, RpcError::Transport(ref m) if m.contains("no reply")));
    }

    #[test]
    fn stdio_exit_is_a_transport_error_with_stderr() {
        if !python3_runs() {
            eprintln!("skipping: python3 is not a working interpreter");
            return;
        }
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

    #[test]
    fn read_capped_line_strips_endings_decodes_lossily_and_caps() {
        let mut r = std::io::Cursor::new(b"one\r\ntw\xffo\nlast".to_vec());
        assert_eq!(read_capped_line(&mut r, 64).unwrap().unwrap(), "one");
        assert_eq!(
            read_capped_line(&mut r, 64).unwrap().unwrap(),
            "tw\u{fffd}o"
        );
        assert_eq!(read_capped_line(&mut r, 64).unwrap().unwrap(), "last");
        assert!(read_capped_line(&mut r, 64).unwrap().is_none());
        // Exactly at the cap is fine; one byte past it without a newline is not.
        let mut r = std::io::Cursor::new(b"abcd\nabcde\n".to_vec());
        assert_eq!(read_capped_line(&mut r, 4).unwrap().unwrap(), "abcd");
        assert!(read_capped_line(&mut r, 4).is_err());
    }

    // A one-shot stdio "server": a shell script that answers request 1.
    #[cfg(unix)]
    fn sh_conn(script: &str, env: &[(String, String)]) -> StdioConn {
        StdioConn::spawn(
            "sh",
            &["-c".to_string(), script.to_string()],
            env,
            Duration::from_secs(10),
        )
        .expect("spawn sh")
    }

    #[cfg(unix)]
    #[test]
    fn stdio_survives_a_non_utf8_line() {
        let mut conn = sh_conn(
            r#"printf 'junk \377\376\n'; read l; printf '{"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n'; sleep 5"#,
            &[],
        );
        assert_eq!(conn.request("ping", None).unwrap(), json!({"ok": true}));
    }

    #[cfg(unix)]
    #[test]
    fn stdio_line_over_the_cap_is_a_transport_error() {
        let mut conn = sh_conn(
            "read l; head -c 9000000 /dev/zero | tr '\\0' a; echo; sleep 5",
            &[],
        );
        let err = conn.request("ping", None).unwrap_err();
        assert!(
            matches!(err, RpcError::Transport(ref m) if m.contains("over 8 MB")),
            "{err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stdio_server_gets_declared_env_but_not_inherited_credentials() {
        // A credential-shaped name no other test touches.
        std::env::set_var("BWN_MCP_SCRUB_TEST_API_KEY", "leaked");
        let mut conn = sh_conn(
            r#"read l; printf '{"jsonrpc":"2.0","id":1,"result":{"inherited":"%s","declared":"%s"}}\n' "$BWN_MCP_SCRUB_TEST_API_KEY" "$BWN_MCP_DECLARED_TOKEN"; sleep 5"#,
            &[("BWN_MCP_DECLARED_TOKEN".into(), "given".into())],
        );
        let got = conn.request("ping", None).unwrap();
        std::env::remove_var("BWN_MCP_SCRUB_TEST_API_KEY");
        assert_eq!(got, json!({"inherited": "", "declared": "given"}));
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
        assert_eq!(handshake(&mut conn).unwrap().info, "h 9");
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

    type Seen = Arc<Mutex<Vec<Vec<(String, String)>>>>;

    // One canned reply per connection, for the 401 paths; returns the port
    // and each request's headers.
    fn serve_replies(replies: Vec<String>) -> (u16, Seen) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        std::thread::spawn(move || {
            for (stream, reply) in listener.incoming().zip(replies) {
                let mut stream = stream.unwrap();
                let (headers, _) = read_http_request(&mut stream);
                seen2.lock().unwrap().push(headers);
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        (port, seen)
    }

    fn unauthorized(challenge: Option<&str>) -> String {
        let h = challenge
            .map(|c| format!("WWW-Authenticate: {c}\r\n"))
            .unwrap_or_default();
        format!(
            "HTTP/1.1 401 Unauthorized\r\n{h}Content-Length: 6\r\nConnection: close\r\n\r\nno-key"
        )
    }

    #[test]
    fn a_bearer_challenge_means_needs_login_and_other_401s_stay_errors() {
        // No saved login can exist under this name.
        let server = format!("unit-401-{}", std::process::id());
        let bearer =
            r#"Bearer resource_metadata="http://127.0.0.1:1/.well-known/oauth-protected-resource""#;
        let (port, seen) = serve_replies(vec![unauthorized(Some(bearer))]);
        let url = format!("http://127.0.0.1:{port}/mcp");
        let mut conn = HttpConn::new(&url, &[], Duration::from_secs(5)).with_oauth(&server);
        assert_eq!(
            handshake(&mut conn).unwrap_err(),
            RpcError::Unauthorized(String::new())
        );
        assert_eq!(header(&seen.lock().unwrap()[0], "authorization"), None);

        // An API-key server that refuses: what it said, not a login hint.
        let (port, _) = serve_replies(vec![unauthorized(Some("Basic realm=\"k\""))]);
        let url = format!("http://127.0.0.1:{port}/mcp");
        let mut conn = HttpConn::new(&url, &[], Duration::from_secs(5)).with_oauth(&server);
        assert!(matches!(
            handshake(&mut conn).unwrap_err(),
            RpcError::Transport(m) if m.contains("HTTP 401") && m.contains("no-key")
        ));

        // An Authorization header in settings opts the server out of OAuth.
        let (port, seen) = serve_replies(vec![unauthorized(Some(bearer))]);
        let url = format!("http://127.0.0.1:{port}/mcp");
        let headers = vec![("authorization".to_string(), "Bearer static".to_string())];
        let mut conn = HttpConn::new(&url, &headers, Duration::from_secs(5)).with_oauth(&server);
        assert!(matches!(
            handshake(&mut conn).unwrap_err(),
            RpcError::Transport(m) if m.contains("HTTP 401")
        ));
        assert_eq!(
            header(&seen.lock().unwrap()[0], "authorization"),
            Some("Bearer static")
        );
    }

    // ── registry: one end-to-end pass over the real fixture ─────────────
    // A single test owns the process-wide registry so parallel tests never
    // race on reload; `mcp_call`'s "not found" test only needs its server to
    // be absent, which stays true here.
    #[test]
    fn registry_discovers_gates_calls_and_reports() {
        if !python3_runs() {
            eprintln!("skipping: python3 is not a working interpreter");
            return;
        }
        let mut servers = BTreeMap::new();
        servers.insert(
            "fake".to_string(),
            json!({"command": "python3", "args": [FIXTURE], "timeout_secs": 20, "trust_read_only_hints": true}),
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
        // A server's output reaches the model labelled as someone else's.
        assert_eq!(
            out.content,
            crate::tools::untrusted_label("mcp__fake__add", "42")
        );
        let failed = crate::tools::run("mcp__fake__fail", &json!({}), std::path::Path::new("."));
        assert!(failed.is_error);
        assert_eq!(failed.content, "boom");
        let legacy = crate::tools::run(
            "mcp_call",
            &json!({"server": "fake", "tool": "echo", "arguments": {"text": "old"}}),
            std::path::Path::new("."),
        );
        assert!(!legacy.is_error);
        assert_eq!(
            legacy.content,
            crate::tools::untrusted_label("mcp_call", "echo: old")
        );
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
        assert!(notices.contains(&(
            "mcp: fake connected, 2 tools, 1 resource, 1 prompt".to_string(),
            true
        )));
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
        assert!(detail
            .iter()
            .any(|l| l.contains("resource note file:///note.txt")));
        assert!(detail.iter().any(|l| l.contains("prompt review")));
        assert!(server_lines("nope").is_err());
        let listed = crate::tools::run("mcp_resources", &json!({}), std::path::Path::new("."));
        assert!(!listed.is_error);
        assert!(listed.content.contains("file:///note.txt"));
        let read = crate::tools::run(
            "mcp_resources",
            &json!({"uri": "file:///note.txt"}),
            std::path::Path::new("."),
        );
        assert!(!read.is_error);
        assert_eq!(
            read.content,
            crate::tools::untrusted_label("mcp_resources", "hello from the resource")
        );
        let missing = crate::tools::run(
            "mcp_prompts",
            &json!({"name": "review"}),
            std::path::Path::new("."),
        );
        assert!(missing.is_error && missing.content.contains("needs diff"));
        let prompt = crate::tools::run(
            "mcp_prompts",
            &json!({"name": "review", "arguments": {"diff": "+added"}}),
            std::path::Path::new("."),
        );
        assert!(!prompt.is_error, "{}", prompt.content);
        assert_eq!(prompt.content, "user:\nReview this:\n+added");
        assert!(crate::tools::defs(false)
            .iter()
            .any(|d| d.name == "mcp_resources"));
        assert!(crate::tools::defs(false)
            .iter()
            .any(|d| d.name == "mcp_prompts"));
        assert!(!crate::tools::is_mutating("mcp_resources"));
        assert!(!crate::tools::is_mutating("mcp_prompts"));

        // A hung call drops the server for the session, once.
        let hung = call("fake", "sleep", &json!({"seconds": 60}));
        assert!(hung.unwrap_err().contains("disconnected"));
        assert!(tools().is_empty());
        assert!(resources().is_empty());
        assert!(prompts().is_empty());
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
    fn force_is_taken_only_before_the_server_command() {
        assert_eq!(
            take_force(&s(&["--force", "fs", "npx", "x"])),
            (true, s(&["fs", "npx", "x"]))
        );
        assert_eq!(
            take_force(&s(&[
                "h",
                "--timeout",
                "7",
                "--force",
                "--url",
                "https://h"
            ])),
            (true, s(&["h", "--timeout", "7", "--url", "https://h"]))
        );
        // After the command it is the server's own argument.
        assert_eq!(
            take_force(&s(&["fs", "srv", "--force"])),
            (false, s(&["fs", "srv", "--force"]))
        );
        assert_eq!(
            take_force(&s(&[
                "h",
                "--client-id",
                "c",
                "--callback-port",
                "9",
                "--force",
                "--url",
                "https://h"
            ])),
            (
                true,
                s(&[
                    "h",
                    "--client-id",
                    "c",
                    "--callback-port",
                    "9",
                    "--url",
                    "https://h"
                ])
            )
        );
        let set = parse_server("x", &json!({"command": "c", "timeout_secs": 9})).unwrap();
        let unset = parse_server("x", &json!({"command": "c"})).unwrap();
        assert!(set.timeout_set && !unset.timeout_set);
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
        // Claude Code's `--` before the command is accepted, once.
        let (_, entry) = parse_add(&s(&["fs", "--", "npx", "-y", "--", "p"])).unwrap();
        assert_eq!(
            entry,
            json!({"type": "stdio", "command": "npx", "args": ["-y", "--", "p"]})
        );
        let (_, entry) = parse_add(&s(&["e", "--env", "TOKEN=x", "cmd"])).unwrap();
        assert_eq!(
            entry,
            json!({"type": "stdio", "command": "cmd", "env": {"TOKEN": "x"}})
        );
        let (_, entry) = parse_add(&s(&[
            "o",
            "--url",
            "https://o/mcp",
            "--client-id",
            "my-app",
            "--callback-port",
            "33418",
        ]))
        .unwrap();
        assert_eq!(
            entry,
            json!({"type": "http", "url": "https://o/mcp", "oauth": {"client_id": "my-app", "callback_port": 33418}})
        );
        assert!(parse_server("o", &entry).is_ok());
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
        assert!(parse_add(&s(&["x", "--client-id", "c", "cmd"]))
            .unwrap_err()
            .contains("--url"));
        assert!(
            parse_add(&s(&["x", "--callback-port", "0", "--url", "http://h"]))
                .unwrap_err()
                .contains("port number")
        );
    }
}
