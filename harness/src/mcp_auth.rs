// OAuth for HTTP MCP servers, as the MCP authorization spec lays it out: a
// 401 names the server's protected-resource metadata (RFC 9728), which names
// its authorization server, whose metadata (RFC 8414) gives the endpoints.
// bwn registers itself there (RFC 7591) unless settings name a client id,
// then signs in with authorization code + PKCE (S256) through the browser and
// a one-shot loopback listener on 127.0.0.1. Tokens are kept owner-only in
// NEXUS_HOME/mcp-auth/<server>.json, bound to the server's URL, and refreshed
// when they expire or the server stops honoring them.
//
// Only `bwn mcp login` (and `/mcp login`) ever opens a browser: background
// discovery and headless runs report the server as needing that command.
// Token values leave this module only in an Authorization header; whatever a
// server sends back has them scrubbed before anyone reads it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;

use crate::config;

/// How long `mcp login` waits for the browser to come back.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
// A token this close to its expiry is refreshed before it is sent.
const REFRESH_MARGIN_SECS: u64 = 60;
const MAX_BODY: u64 = 1024 * 1024;
const REDACTED: &str = "[redacted]";

// ── settings: mcp_servers.<name>.oauth ──────────────────────────────────────

/// `mcp_servers.<name>.oauth`: a client registered ahead of time, for
/// authorization servers without dynamic registration.
#[derive(Clone, Default, PartialEq)]
pub struct OAuthSettings {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// Space-separated, as the `scope` parameter carries them.
    pub scopes: Option<String>,
    /// A fixed loopback port, for a client registered with an exact
    /// redirect URI.
    pub callback_port: Option<u16>,
}

// Never prints the secret: ServerConfig is Debug.
impl std::fmt::Debug for OAuthSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthSettings")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| REDACTED),
            )
            .field("scopes", &self.scopes)
            .field("callback_port", &self.callback_port)
            .finish()
    }
}

pub fn parse_settings(name: &str, v: Option<&Value>) -> Result<OAuthSettings, String> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(OAuthSettings::default());
    };
    let Some(obj) = v.as_object() else {
        return Err(format!("mcp_servers.{name}.oauth must be a JSON object"));
    };
    let text = |k: &str| -> Result<Option<String>, String> {
        match obj.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_string())),
            Some(_) => Err(format!(
                "mcp_servers.{name}.oauth.{k} must be a non-empty string"
            )),
        }
    };
    let scopes = match obj.get("scopes") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.split_whitespace().collect::<Vec<_>>().join(" ")),
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => Some(
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        Some(_) => {
            return Err(format!(
                "mcp_servers.{name}.oauth.scopes must be a list of strings"
            ))
        }
    }
    .filter(|s| !s.is_empty());
    let callback_port = match obj.get("callback_port") {
        None | Some(Value::Null) => None,
        Some(p) => match p.as_u64() {
            Some(n) if (1..=65535).contains(&n) => Some(n as u16),
            _ => {
                return Err(format!(
                    "mcp_servers.{name}.oauth.callback_port must be a port number"
                ))
            }
        },
    };
    Ok(OAuthSettings {
        client_id: text("client_id")?,
        client_secret: text("client_secret")?,
        scopes,
        callback_port,
    })
}

// ── the saved login ─────────────────────────────────────────────────────────

/// One server's sign-in, as saved in NEXUS_HOME/mcp-auth/<server>.json.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Login {
    /// The MCP server URL this token was issued for; a different URL in
    /// settings never gets it.
    pub url: String,
    /// The resource indicator (RFC 8707) sent with every token request.
    pub resource: String,
    pub issuer: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revocation_endpoint: Option<String>,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// `client_secret_post` or `client_secret_basic` when there is a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_method: Option<String>,
    pub redirect_uri: String,
    /// The client came from dynamic registration, so a later login may
    /// reuse it on the same redirect URI.
    #[serde(default)]
    pub registered: bool,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds; absent when the server gave no lifetime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl std::fmt::Debug for Login {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Login")
            .field("url", &self.url)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl Login {
    fn secrets(&self) -> Vec<String> {
        [
            Some(&self.access_token),
            self.refresh_token.as_ref(),
            self.client_secret.as_ref(),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect()
    }

    fn client(&self) -> Client {
        Client {
            id: self.client_id.clone(),
            secret: self.client_secret.clone(),
            auth_method: self.token_endpoint_auth_method.clone(),
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn auth_dir() -> PathBuf {
    config::home().join("mcp-auth")
}

// Server names are settings keys and may hold anything; the file name keeps
// the safe characters, and a name that needed changing gets a hash so two
// such names never share a file.
fn file_stem(server: &str) -> String {
    let clean: String = server
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean == server && !clean.is_empty() {
        return clean;
    }
    let mut h: u32 = 0x811c_9dc5;
    for b in server.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{clean}-{h:08x}")
}

pub fn login_path(server: &str) -> PathBuf {
    auth_dir().join(format!("{}.json", file_stem(server)))
}

fn read_login(server: &str) -> Option<Login> {
    let text = std::fs::read_to_string(login_path(server)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The saved login for `server`, only when it was made for `url`.
pub fn load(server: &str, url: &str) -> Option<Login> {
    read_login(server).filter(|l| l.url == url)
}

fn save(server: &str, login: &Login) -> Result<PathBuf, String> {
    let dir = auth_dir();
    config::ensure_private_dir(&dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = login_path(server);
    let text = serde_json::to_string_pretty(login).map_err(|e| e.to_string())?;
    if config::write_private(&path, &text) {
        Ok(path)
    } else {
        Err(format!("could not write {}", path.display()))
    }
}

/// One line on a server's sign-in for `/mcp`: never the token itself.
pub fn describe(server: &str, url: &str) -> String {
    let Some(login) = load(server, url) else {
        return "not signed in".into();
    };
    let now = unix_now();
    match login.expires_at {
        Some(t) if t <= now && login.refresh_token.is_some() => {
            "signed in (token expired; refreshed on next use)".into()
        }
        Some(t) if t <= now => "signed in (token expired)".into(),
        Some(t) => format!("signed in (token expires in {})", human_secs(t - now)),
        None => "signed in".into(),
    }
}

fn human_secs(s: u64) -> String {
    match s {
        0..=89 => format!("{s}s"),
        90..=5399 => format!("{}m", (s + 30) / 60),
        5400..=129_599 => format!("{}h", (s + 1800) / 3600),
        _ => format!("{}d", (s + 43_200) / 86_400),
    }
}

static IN_SESSION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The interactive UI is running: login hints name `/mcp login`, which
/// reconnects the session, rather than `bwn mcp login`.
pub fn set_in_session() {
    IN_SESSION.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// What to run to sign in to `server`: `bwn mcp login <name>` (quoted for a
/// shell when the name needs it), or `/mcp login <name>` inside a session.
pub fn login_hint(server: &str, reason: &str) -> String {
    let in_session = IN_SESSION.load(std::sync::atomic::Ordering::Relaxed);
    login_hint_for(server, reason, in_session)
}

fn login_hint_for(server: &str, reason: &str, in_session: bool) -> String {
    let cmd = if in_session {
        format!("type /mcp login {server}")
    } else {
        let quoted = shlex::try_quote(server).unwrap_or(server.into());
        format!("run `bwn mcp login {quoted}`")
    };
    if reason.is_empty() {
        cmd
    } else {
        format!("{cmd} ({reason})")
    }
}

// ── the token a connection sends ────────────────────────────────────────────

/// The OAuth side of one HTTP connection: which token to send, what to do
/// when it is refused, and which strings to scrub from what comes back.
pub struct Session {
    server: String,
    url: String,
    // A token is only ever sent over TLS, or to this machine.
    allowed: bool,
    login: Option<Login>,
    // Every token this connection has held, so an old one echoed back after
    // a refresh is still scrubbed.
    seen: Vec<String>,
    // Why the saved login stopped working, for the needs-login message.
    refused: Option<String>,
}

impl Session {
    pub fn new(server: &str, url: &str) -> Session {
        let mut s = Session {
            server: server.to_string(),
            url: url.to_string(),
            allowed: Url::parse(url).is_ok_and(|u| secure(&u)),
            login: None,
            seen: Vec::new(),
            refused: None,
        };
        s.adopt(s.saved_login());
        s
    }

    fn saved_login(&self) -> Option<Login> {
        self.allowed
            .then(|| load(&self.server, &self.url))
            .flatten()
    }

    fn adopt(&mut self, login: Option<Login>) {
        if let Some(l) = &login {
            for t in l.secrets() {
                if !self.seen.contains(&t) {
                    self.seen.push(t);
                }
            }
        }
        self.login = login;
    }

    /// The access token to send, refreshed first when it is about to expire.
    pub fn bearer(&mut self) -> Option<String> {
        let due = self.login.as_ref().is_some_and(|l| {
            l.refresh_token.is_some()
                && l.expires_at
                    .is_some_and(|t| t <= unix_now() + REFRESH_MARGIN_SECS)
        });
        if due {
            // A failure here still sends the old token; a 401 then decides.
            let _ = self.refresh_now();
        }
        self.login.as_ref().map(|l| l.access_token.clone())
    }

    /// After a 401 to a request that carried `sent`: true when there is a
    /// newer token worth one retry.
    pub fn recover(&mut self, sent: Option<&str>) -> bool {
        // Another process may have signed in or refreshed since this one
        // loaded the file.
        if let Some(disk) = self.saved_login() {
            if Some(disk.access_token.as_str()) != sent {
                self.adopt(Some(disk));
                return true;
            }
        }
        sent.is_some() && self.refresh_now().is_ok()
    }

    fn refresh_now(&mut self) -> Result<(), String> {
        let Some(login) = self.login.clone() else {
            return Err("not signed in".into());
        };
        match refresh(&login) {
            Ok(new) => {
                // A refresh that cannot be saved still serves this session.
                let _ = save(&self.server, &new);
                self.adopt(Some(new));
                Ok(())
            }
            Err(e) => {
                // A concurrent refresh elsewhere rotated the refresh token.
                if let Some(disk) = self.saved_login() {
                    if disk.refresh_token != login.refresh_token {
                        self.adopt(Some(disk));
                        return Ok(());
                    }
                }
                if e.definitive {
                    self.refused = Some(e.message.clone());
                    self.login = None;
                }
                Err(e.message)
            }
        }
    }

    /// Why the server needs a login: empty when it never had one, or what
    /// refused the saved one.
    pub fn login_reason(&self) -> String {
        match &self.refused {
            Some(why) => format!("the saved sign-in was refused: {why}"),
            None => String::new(),
        }
    }

    /// `s` with every token this connection has held replaced.
    pub fn scrub(&self, s: &str) -> String {
        scrub_str(s, &self.seen)
    }

    pub fn scrub_value(&self, v: &mut Value) {
        scrub_json(v, &self.seen);
    }
}

fn scrub_str(s: &str, secrets: &[String]) -> String {
    let mut out = s.to_string();
    for t in secrets.iter().filter(|t| t.len() >= 4) {
        if out.contains(t.as_str()) {
            out = out.replace(t.as_str(), REDACTED);
        }
    }
    out
}

fn scrub_json(v: &mut Value, secrets: &[String]) {
    match v {
        Value::String(s) => {
            if secrets
                .iter()
                .any(|t| t.len() >= 4 && s.contains(t.as_str()))
            {
                *s = scrub_str(s, secrets);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| scrub_json(x, secrets)),
        Value::Object(m) => m.values_mut().for_each(|x| scrub_json(x, secrets)),
        _ => {}
    }
}

// ── discovery ───────────────────────────────────────────────────────────────

/// The parameters of a `Bearer` challenge in a WWW-Authenticate header.
#[derive(Debug, Default, PartialEq)]
pub struct Challenge {
    pub resource_metadata: Option<String>,
    pub scope: Option<String>,
    pub error: Option<String>,
}

/// Reads the `Bearer` challenge out of a WWW-Authenticate value, which may
/// list other schemes around it.
pub fn parse_challenge(header: &str) -> Challenge {
    let mut out = Challenge::default();
    let chars: Vec<char> = header.chars().collect();
    let mut i = 0;
    let mut in_bearer = false;
    while i < chars.len() {
        while i < chars.len() && (chars[i] == ',' || chars[i].is_whitespace()) {
            i += 1;
        }
        let start = i;
        while i < chars.len() && !matches!(chars[i], '=' | ',' | ' ' | '\t') {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        if word.is_empty() {
            i += 1;
            continue;
        }
        if i < chars.len() && chars[i] == '=' {
            i += 1;
            let value = if i < chars.len() && chars[i] == '"' {
                i += 1;
                let mut v = String::new();
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    v.push(chars[i]);
                    i += 1;
                }
                i += 1;
                v
            } else {
                let s = i;
                while i < chars.len() && chars[i] != ',' && !chars[i].is_whitespace() {
                    i += 1;
                }
                chars[s..i].iter().collect()
            };
            if in_bearer {
                match word.to_ascii_lowercase().as_str() {
                    "resource_metadata" => out.resource_metadata = Some(value),
                    "scope" => out.scope = Some(value).filter(|s| !s.trim().is_empty()),
                    "error" => out.error = Some(value),
                    _ => {}
                }
            }
        } else {
            // A bare word starts a new challenge (or is a token68 value).
            in_bearer = word.eq_ignore_ascii_case("bearer");
        }
    }
    out
}

fn is_loopback(u: &Url) -> bool {
    match u.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip).is_loopback(),
        Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip).is_loopback(),
        None => false,
    }
}

/// HTTPS, or plain HTTP to this machine: the only places a code, a token
/// or a client secret is sent.
pub fn secure(u: &Url) -> bool {
    u.scheme() == "https" || (u.scheme() == "http" && is_loopback(u))
}

fn secure_url(raw: &str, what: &str) -> Result<Url, String> {
    let u = Url::parse(raw).map_err(|e| format!("{what} '{raw}' is not a URL: {e}"))?;
    // Metadata is held to the same rule: over plain HTTP it could send the
    // sign-in to someone else's authorization server.
    if !secure(&u) {
        return Err(format!(
            "{what} {raw} is not https:// (or on this machine), so it is not used for signing in"
        ));
    }
    Ok(u)
}

fn origin(u: &Url) -> String {
    let mut o = format!("{}://{}", u.scheme(), u.host_str().unwrap_or(""));
    if let Some(p) = u.port() {
        o.push_str(&format!(":{p}"));
    }
    o
}

/// Where RFC 9728 puts protected-resource metadata for `mcp_url`: the
/// path-specific location first, then the host's.
pub fn resource_metadata_urls(mcp_url: &Url) -> Vec<String> {
    let base = origin(mcp_url);
    let path = mcp_url.path().trim_end_matches('/');
    let mut out = Vec::new();
    if !path.is_empty() {
        out.push(format!("{base}/.well-known/oauth-protected-resource{path}"));
    }
    out.push(format!("{base}/.well-known/oauth-protected-resource"));
    out
}

/// Where RFC 8414 and OpenID Connect Discovery put an issuer's metadata,
/// in the order the MCP spec tries them.
pub fn server_metadata_urls(issuer: &Url) -> Vec<String> {
    let base = origin(issuer);
    let path = issuer.path().trim_end_matches('/');
    if path.is_empty() {
        vec![
            format!("{base}/.well-known/oauth-authorization-server"),
            format!("{base}/.well-known/openid-configuration"),
        ]
    } else {
        vec![
            format!("{base}/.well-known/oauth-authorization-server{path}"),
            format!("{base}/.well-known/openid-configuration{path}"),
            format!("{base}{path}/.well-known/openid-configuration"),
        ]
    }
}

/// RFC 9728's `resource` must name this server: same origin, and the MCP
/// URL's path inside the resource's.
pub fn resource_matches(resource: &str, mcp_url: &Url) -> bool {
    let Ok(r) = Url::parse(resource) else {
        return false;
    };
    if origin(&r) != origin(mcp_url) {
        return false;
    }
    let rp = r.path().trim_end_matches('/');
    let mp = mcp_url.path().trim_end_matches('/');
    mp == rp || mp.starts_with(&format!("{rp}/"))
}

#[derive(Debug, Default)]
struct ServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    revocation_endpoint: Option<String>,
    token_auth_methods: Vec<String>,
    // `iss` must come back on the redirect (RFC 9207).
    iss_required: bool,
}

fn same_issuer(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// Checks one authorization server metadata document against the issuer it
/// was fetched for.
fn parse_server_metadata(v: &Value, issuer: &str) -> Result<ServerMetadata, String> {
    let got = v["issuer"].as_str().unwrap_or("");
    if !same_issuer(got, issuer) {
        return Err(format!(
            "authorization server metadata names issuer '{got}', not {issuer}"
        ));
    }
    let endpoint = |k: &str| -> Result<Option<String>, String> {
        match v[k].as_str().filter(|s| !s.is_empty()) {
            None => Ok(None),
            Some(raw) => secure_url(raw, k).map(|_| Some(raw.to_string())),
        }
    };
    let authorization_endpoint = endpoint("authorization_endpoint")?
        .ok_or("authorization server metadata has no authorization_endpoint")?;
    let token_endpoint =
        endpoint("token_endpoint")?.ok_or("authorization server metadata has no token_endpoint")?;
    let list = |k: &str| -> Option<Vec<String>> {
        v[k].as_array().map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
    };
    if let Some(methods) = list("code_challenge_methods_supported") {
        if !methods.iter().any(|m| m == "S256") {
            return Err(
                "the authorization server does not support PKCE with S256, which bwn requires"
                    .into(),
            );
        }
    }
    Ok(ServerMetadata {
        issuer: issuer.to_string(),
        authorization_endpoint,
        token_endpoint,
        registration_endpoint: endpoint("registration_endpoint")?,
        revocation_endpoint: endpoint("revocation_endpoint")?,
        token_auth_methods: list("token_endpoint_auth_methods_supported").unwrap_or_default(),
        iss_required: v["authorization_response_iss_parameter_supported"] == json!(true),
    })
}

// Requests to the server and its authorization server go through the shared
// client (proxy and CA settings) and never follow a redirect: a token
// request answered with a 307 would resend its secrets elsewhere.
fn http() -> crate::net::Client {
    crate::net::Client::new(|b| b.timeout(HTTP_TIMEOUT).redirects(0))
}

fn transport_error(url: &str, e: &ureq::Error) -> String {
    if let Some(hint) = crate::net::cert_error_hint(e) {
        return format!("{url}: {hint}");
    }
    crate::provider::redact(&format!("{url}: {e}"))
}

// Status and JSON body of an answer, error statuses included; Err only when
// nothing usable came back.
fn json_reply(url: &str, r: Result<ureq::Response, ureq::Error>) -> Result<(u16, Value), String> {
    let resp = match r {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return Err(transport_error(url, &e)),
    };
    let status = resp.status();
    let mut text = String::new();
    resp.into_reader()
        .take(MAX_BODY)
        .read_to_string(&mut text)
        .map_err(|e| format!("{url}: reading the reply: {e}"))?;
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

fn get_json(client: &crate::net::Client, url: &str) -> Result<Option<Value>, String> {
    let (status, v) = json_reply(
        url,
        client.get(url).set("Accept", "application/json").call(),
    )?;
    Ok(((200..300).contains(&status) && v.is_object()).then_some(v))
}

struct Discovered {
    resource: String,
    scope: Option<String>,
    meta: ServerMetadata,
}

// The 401 a token-less request gets, and its challenge.
fn probe(client: &crate::net::Client, mcp_url: &str) -> Result<(u16, Challenge), String> {
    let body = json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {
            "protocolVersion": crate::mcp::PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "buildwithnexus", "version": env!("CARGO_PKG_VERSION")}
        }
    });
    let r = client
        .post(mcp_url)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json, text/event-stream")
        .set("MCP-Protocol-Version", crate::mcp::PROTOCOL_VERSION)
        .send_string(&body.to_string());
    let resp = match r {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return Err(transport_error(mcp_url, &e)),
    };
    let challenge = resp
        .header("www-authenticate")
        .map(parse_challenge)
        .unwrap_or_default();
    Ok((resp.status(), challenge))
}

fn discover(
    client: &crate::net::Client,
    name: &str,
    mcp_url: &Url,
    settings: &OAuthSettings,
) -> Result<Discovered, String> {
    let (status, challenge) = probe(client, mcp_url.as_str())?;
    let mut candidates = Vec::new();
    if let Some(m) = &challenge.resource_metadata {
        secure_url(m, "resource_metadata")?;
        candidates.push(m.clone());
    }
    candidates.extend(resource_metadata_urls(mcp_url));
    let mut prm = None;
    for c in candidates {
        if let Some(v) = get_json(client, &c)? {
            prm = Some(v);
            break;
        }
    }
    let (resource, issuer, supported) = match prm {
        Some(v) => {
            let resource = v["resource"]
                .as_str()
                .unwrap_or(mcp_url.as_str())
                .to_string();
            if !resource_matches(&resource, mcp_url) {
                return Err(format!(
                    "{name}'s protected-resource metadata is for {resource}, not {mcp_url}"
                ));
            }
            let issuer = v["authorization_servers"]
                .as_array()
                .and_then(|a| a.iter().filter_map(Value::as_str).next())
                .ok_or_else(|| {
                    format!("{name}'s protected-resource metadata names no authorization server")
                })?
                .to_string();
            let supported = v["scopes_supported"].as_array().map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            });
            (resource, issuer, supported)
        }
        // Servers from before RFC 9728 was adopted are their own
        // authorization server.
        None if status == 401 => (mcp_url.to_string(), origin(mcp_url), None),
        None => {
            return Err(format!(
                "{name} did not ask for a login (HTTP {status}) and publishes no OAuth metadata"
            ))
        }
    };
    let issuer_url = secure_url(&issuer, "authorization server")?;
    let mut meta = None;
    for c in server_metadata_urls(&issuer_url) {
        if let Some(v) = get_json(client, &c)? {
            meta = Some(parse_server_metadata(&v, &issuer)?);
            break;
        }
    }
    let meta =
        meta.ok_or_else(|| format!("no authorization server metadata found for {issuer}"))?;
    let scope = settings
        .scopes
        .clone()
        .or(challenge.scope)
        .or(supported)
        .filter(|s| !s.trim().is_empty());
    Ok(Discovered {
        resource,
        scope,
        meta,
    })
}

// ── clients ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Client {
    id: String,
    secret: Option<String>,
    auth_method: Option<String>,
}

fn register(
    http: &crate::net::Client,
    endpoint: &str,
    redirect_uri: &str,
    scope: Option<&str>,
) -> Result<Client, String> {
    let mut body = json!({
        "client_name": "buildwithnexus",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    if let Some(s) = scope {
        body["scope"] = json!(s);
    }
    let (status, v) = json_reply(
        endpoint,
        http.post(endpoint)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
            .send_string(&body.to_string()),
    )?;
    let id = v["client_id"].as_str().filter(|s| !s.is_empty());
    match (status, id) {
        (200..=299, Some(id)) => Ok(Client {
            id: id.to_string(),
            secret: v["client_secret"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            auth_method: v["token_endpoint_auth_method"].as_str().map(str::to_string),
        }),
        _ => Err(format!(
            "client registration at {endpoint} failed (HTTP {status}): {}",
            oauth_error_text(&v)
        )),
    }
}

fn oauth_error_text(v: &Value) -> String {
    let error = v["error"].as_str().unwrap_or("no error code");
    let text = match v["error_description"].as_str() {
        Some(d) if !d.is_empty() => format!("{error}: {d}"),
        _ => error.to_string(),
    };
    crate::provider::redact(&text.chars().take(300).collect::<String>())
}

// ── PKCE and random values ──────────────────────────────────────────────────

fn base64(bytes: &[u8], url_safe: bool) -> String {
    let table: &[u8; 64] = if url_safe {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
    } else {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
    };
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(table[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
        if !url_safe {
            for _ in chunk.len()..3 {
                out.push('=');
            }
        }
    }
    out
}

/// `bytes` of OS randomness, base64url without padding.
fn random_string(bytes: usize) -> Result<String, String> {
    use ring::rand::SecureRandom;
    let mut buf = vec![0u8; bytes];
    ring::rand::SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| "the system random source failed".to_string())?;
    Ok(base64(&buf, true))
}

/// The S256 code challenge for `verifier` (RFC 7636 §4.2).
pub fn code_challenge(verifier: &str) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    base64(d.as_ref(), true)
}

// ── the loopback redirect ───────────────────────────────────────────────────

/// What one request to the loopback listener means for the login.
#[derive(Debug, PartialEq)]
pub enum Callback {
    /// Not the redirect (a favicon, a stray probe): answer 404, keep waiting.
    Ignore,
    Code(String),
    Fail(String),
}

/// Judges the request target of a request to the loopback listener.
pub fn check_callback(target: &str, state: &str, issuer: &str, iss_required: bool) -> Callback {
    let Ok(u) = Url::parse(&format!("http://127.0.0.1{target}")) else {
        return Callback::Ignore;
    };
    if u.path() != "/callback" {
        return Callback::Ignore;
    }
    let q = |k: &str| {
        u.query_pairs()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.into_owned())
    };
    // The state is checked first: a redirect this login did not start says
    // nothing else worth trusting.
    if q("state").as_deref() != Some(state) {
        return Callback::Fail(
            "the browser came back with a state this login did not send, so the redirect was \
             not ours — nothing was saved; run the login again"
                .into(),
        );
    }
    match q("iss") {
        Some(iss) if !same_issuer(&iss, issuer) => {
            return Callback::Fail(format!(
                "the redirect came from issuer '{}', not {issuer}",
                crate::provider::redact(&iss)
            ))
        }
        None if iss_required => {
            return Callback::Fail("the redirect is missing the issuer (iss) it must carry".into())
        }
        _ => {}
    }
    if let Some(e) = q("error") {
        let desc = q("error_description")
            .map(|d| format!(": {d}"))
            .unwrap_or_default();
        let text: String = format!("{e}{desc}").chars().take(300).collect();
        return Callback::Fail(format!("the authorization server refused: {text}"));
    }
    match q("code").filter(|c| !c.is_empty()) {
        Some(code) => Callback::Code(code),
        None => Callback::Fail("the redirect carried no authorization code".into()),
    }
}

fn read_request_target(stream: &TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream.take(16 * 1024));
    let mut first = String::new();
    reader.read_line(&mut first).ok()?;
    // Drain the headers so the browser sees a clean reply.
    let mut line = String::new();
    while reader.read_line(&mut line).ok()? > 2 {
        line.clear();
    }
    let mut parts = first.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) => Some(target.to_string()),
        _ => None,
    }
}

fn answer(mut stream: &TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>buildwithnexus</title>\
         <p style=\"font-family:sans-serif\">{message}</p>"
    );
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

/// Serves the loopback listener until the redirect arrives, the deadline
/// passes, or `tick` (called between connections) gives up.
pub fn wait_for_callback(
    listener: &TcpListener,
    state: &str,
    issuer: &str,
    iss_required: bool,
    deadline: Instant,
    tick: &mut dyn FnMut() -> Result<(), String>,
) -> Result<String, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("loopback listener: {e}"))?;
    loop {
        tick()?;
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let Some(target) = read_request_target(&stream) else {
                    answer(&stream, "400 Bad Request", "Not a sign-in redirect.");
                    continue;
                };
                match check_callback(&target, state, issuer, iss_required) {
                    Callback::Ignore => answer(&stream, "404 Not Found", "Not found."),
                    Callback::Code(code) => {
                        answer(
                            &stream,
                            "200 OK",
                            "Signed in. You can close this tab and return to the terminal.",
                        );
                        return Ok(code);
                    }
                    Callback::Fail(why) => {
                        answer(
                            &stream,
                            "400 Bad Request",
                            "Sign-in failed. Return to the terminal for details.",
                        );
                        return Err(why);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(
                        "timed out waiting for the browser to finish signing in".to_string()
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("loopback listener: {e}")),
        }
    }
}

// ── token requests ──────────────────────────────────────────────────────────

// Client authentication on a token-endpoint request: a public client names
// itself in the form; a secret goes in a Basic header (RFC 6749 §2.3.1)
// unless the client was set up for client_secret_post.
fn authenticate(
    req: ureq::Request,
    client: &Client,
    form: &mut Vec<(&'static str, String)>,
) -> ureq::Request {
    match &client.secret {
        Some(secret) if client.auth_method.as_deref() == Some("client_secret_post") => {
            form.push(("client_id", client.id.clone()));
            form.push(("client_secret", secret.clone()));
            req
        }
        Some(secret) => {
            let enc =
                |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
            let pair = format!("{}:{}", enc(&client.id), enc(secret));
            req.set(
                "Authorization",
                &format!("Basic {}", base64(pair.as_bytes(), false)),
            )
        }
        None => {
            form.push(("client_id", client.id.clone()));
            req
        }
    }
}

struct TokenError {
    message: String,
    /// The grant itself was refused: only a new login fixes it.
    definitive: bool,
}

fn token_request(
    http: &crate::net::Client,
    endpoint: &str,
    client: &Client,
    mut form: Vec<(&'static str, String)>,
) -> Result<Value, TokenError> {
    let req = authenticate(
        http.post(endpoint).set("Accept", "application/json"),
        client,
        &mut form,
    );
    let pairs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (status, v) =
        json_reply(endpoint, req.send_form(&pairs)).map_err(|message| TokenError {
            message,
            definitive: false,
        })?;
    if (200..300).contains(&status) && v["access_token"].as_str().is_some_and(|t| !t.is_empty()) {
        let kind = v["token_type"].as_str().unwrap_or("Bearer");
        if !kind.eq_ignore_ascii_case("bearer") {
            return Err(TokenError {
                message: format!("the token endpoint issued a '{kind}' token, not a Bearer token"),
                definitive: true,
            });
        }
        return Ok(v);
    }
    let code = v["error"].as_str().unwrap_or("");
    Err(TokenError {
        message: format!(
            "the token endpoint refused (HTTP {status}): {}",
            oauth_error_text(&v)
        ),
        definitive: matches!(
            code,
            "invalid_grant" | "invalid_client" | "unauthorized_client"
        ),
    })
}

// Fills a login's token fields from a token response; a refresh that hands
// out no new refresh token keeps the old one.
fn apply_tokens(login: &mut Login, v: &Value) {
    login.access_token = v["access_token"].as_str().unwrap_or("").to_string();
    if let Some(r) = v["refresh_token"].as_str().filter(|r| !r.is_empty()) {
        login.refresh_token = Some(r.to_string());
    }
    login.expires_at = v["expires_in"]
        .as_u64()
        .or_else(|| v["expires_in"].as_str().and_then(|s| s.parse().ok()))
        .map(|s| unix_now().saturating_add(s));
    if let Some(s) = v["scope"].as_str() {
        login.scope = Some(s.to_string());
    }
}

fn refresh(login: &Login) -> Result<Login, TokenError> {
    let Some(rt) = login.refresh_token.clone() else {
        return Err(TokenError {
            message: "no refresh token was issued".into(),
            definitive: true,
        });
    };
    let form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", rt),
        ("resource", login.resource.clone()),
    ];
    let v = token_request(&http(), &login.token_endpoint, &login.client(), form)?;
    let mut new = login.clone();
    apply_tokens(&mut new, &v);
    Ok(new)
}

// ── login and logout ────────────────────────────────────────────────────────

// Binds the loopback listener: the configured port, else the port of the
// client registered last time (so it can be reused), else any free port.
fn bind_loopback(
    settings: &OAuthSettings,
    previous: Option<&Login>,
) -> Result<(TcpListener, bool), String> {
    if let Some(p) = settings.callback_port {
        return TcpListener::bind(("127.0.0.1", p))
            .map(|l| (l, false))
            .map_err(|e| format!("cannot listen on 127.0.0.1:{p} (oauth.callback_port): {e}"));
    }
    if let Some(port) = previous
        .and_then(|l| Url::parse(&l.redirect_uri).ok())
        .and_then(|u| u.port())
    {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", port)) {
            return Ok((l, true));
        }
    }
    TcpListener::bind(("127.0.0.1", 0))
        .map(|l| (l, false))
        .map_err(|e| format!("cannot open a loopback listener: {e}"))
}

/// Signs in to `server` at `url` through the browser and saves the tokens.
/// `say` gets the progress lines (the URL among them) as they happen.
pub fn login(
    server: &str,
    url: &str,
    settings: &OAuthSettings,
    say: &mut dyn FnMut(&str),
) -> Result<PathBuf, String> {
    let mcp_url = Url::parse(url).map_err(|e| format!("{url} is not a URL: {e}"))?;
    if !secure(&mcp_url) {
        return Err(format!(
            "{server} is at {url}; signing in needs an https:// URL (or one on this machine)"
        ));
    }
    let http = http();
    let found = discover(&http, server, &mcp_url, settings)?;
    let meta = &found.meta;
    let previous = read_login(server)
        .filter(|l| l.url == url && l.registered && same_issuer(&l.issuer, &meta.issuer));
    let (listener, reused_port) = bind_loopback(settings, previous.as_ref())?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("loopback listener: {e}"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let (client, registered) = if let Some(id) = &settings.client_id {
        // Basic unless the server takes the secret only in the form.
        let offers = |m: &str| meta.token_auth_methods.iter().any(|x| x == m);
        let method = if offers("client_secret_post") && !offers("client_secret_basic") {
            "client_secret_post"
        } else {
            "client_secret_basic"
        };
        let c = Client {
            id: id.clone(),
            secret: settings.client_secret.clone(),
            auth_method: settings.client_secret.as_ref().map(|_| method.to_string()),
        };
        (c, false)
    } else if let Some(prev) = previous.filter(|p| reused_port && p.redirect_uri == redirect_uri) {
        (prev.client(), true)
    } else if let Some(endpoint) = &meta.registration_endpoint {
        (
            register(&http, endpoint, &redirect_uri, found.scope.as_deref())?,
            true,
        )
    } else {
        return Err(format!(
            "{server}'s authorization server has no dynamic client registration; register a \
             client there with redirect URI http://127.0.0.1:<port>/callback and set \
             mcp_servers.{server}.oauth.client_id (and oauth.callback_port if it needs an exact port)"
        ));
    };

    let verifier = random_string(48)?;
    let state = random_string(32)?;
    let mut auth_url = Url::parse(&meta.authorization_endpoint)
        .map_err(|e| format!("authorization_endpoint: {e}"))?;
    {
        let mut q = auth_url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &client.id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("state", &state)
            .append_pair("code_challenge", &code_challenge(&verifier))
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", &found.resource);
        if let Some(s) = &found.scope {
            q.append_pair("scope", s);
        }
    }
    let auth_url = auth_url.to_string();

    say(&format!("Signing in to {server}: opening the browser…"));
    say(&format!("If it does not open, visit {auth_url}"));
    // The opener runs beside the listener: one that waits for the browser
    // (a console browser in this terminal) must not hold up the redirect.
    let (tx, rx) = mpsc::channel();
    let target = auth_url.clone();
    std::thread::spawn(move || {
        let _ = tx.send(crate::tools::spawn_opener(&target));
    });
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    let mut opener_done = false;
    let mut tick = || -> Result<(), String> {
        if !opener_done {
            if let Ok(result) = rx.try_recv() {
                opener_done = true;
                let problem = match result {
                    Ok(s) if s.success() => None,
                    Ok(s) => Some(format!("the opener exited with {s}")),
                    Err(e) => Some(e.to_string()),
                };
                if let Some(p) = problem {
                    say(&format!(
                        "Could not open a browser ({p}); open the URL above to continue."
                    ));
                }
                say(&format!(
                    "Waiting up to {} minutes for the browser…",
                    LOGIN_TIMEOUT.as_secs() / 60
                ));
            }
        }
        crate::tui::poll_typeahead();
        if crate::tui::interrupted() {
            crate::tui::consume_interrupt();
            return Err("cancelled".into());
        }
        Ok(())
    };
    let code = wait_for_callback(
        &listener,
        &state,
        &meta.issuer,
        meta.iss_required,
        deadline,
        &mut tick,
    )?;
    drop(listener);

    let form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        ("redirect_uri", redirect_uri.clone()),
        ("code_verifier", verifier),
        ("resource", found.resource.clone()),
    ];
    let v = token_request(&http, &meta.token_endpoint, &client, form).map_err(|e| e.message)?;
    let mut login = Login {
        url: url.to_string(),
        resource: found.resource.clone(),
        issuer: meta.issuer.clone(),
        token_endpoint: meta.token_endpoint.clone(),
        revocation_endpoint: meta.revocation_endpoint.clone(),
        client_id: client.id.clone(),
        client_secret: client.secret.clone(),
        token_endpoint_auth_method: client.auth_method.clone(),
        redirect_uri,
        registered,
        access_token: String::new(),
        refresh_token: None,
        expires_at: None,
        scope: found.scope.clone(),
    };
    apply_tokens(&mut login, &v);
    save(server, &login)
}

/// Forgets `server`'s login, asking its authorization server to revoke the
/// token first when it offers that. Returns what happened, one line each,
/// or None when there was no login.
pub fn logout(server: &str) -> Result<Option<Vec<String>>, String> {
    let path = login_path(server);
    let Some(login) = read_login(server) else {
        return Ok(None);
    };
    let mut lines = Vec::new();
    if let Some(endpoint) = login
        .revocation_endpoint
        .as_deref()
        .filter(|e| Url::parse(e).is_ok_and(|u| secure(&u)))
    {
        // Revoking the refresh token ends the grant; the access token
        // alone is all there is without one.
        let (token, hint) = match &login.refresh_token {
            Some(rt) => (rt.clone(), "refresh_token"),
            None => (login.access_token.clone(), "access_token"),
        };
        let form = vec![("token", token), ("token_type_hint", hint.to_string())];
        match token_request_status(endpoint, &login.client(), form) {
            Ok(()) => lines.push("the authorization server revoked the token".into()),
            Err(e) => lines.push(format!(
                "could not revoke the token at the authorization server ({}); it expires on its own",
                scrub_str(&e, &login.secrets())
            )),
        }
    }
    std::fs::remove_file(&path).map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
    lines.insert(
        0,
        format!("signed out of {server}; removed {}", path.display()),
    );
    Ok(Some(lines))
}

// A revocation request (RFC 7009): any 2xx is success, the body is not read.
fn token_request_status(
    endpoint: &str,
    client: &Client,
    mut form: Vec<(&'static str, String)>,
) -> Result<(), String> {
    let req = authenticate(http().post(endpoint), client, &mut form);
    let pairs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
    match req.send_form(&pairs) {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code}")),
        Err(e) => Err(transport_error(endpoint, &e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    // ── settings ────────────────────────────────────────────────────────
    #[test]
    fn oauth_settings_parse_and_reject() {
        assert_eq!(parse_settings("a", None).unwrap(), OAuthSettings::default());
        let s = parse_settings(
            "a",
            Some(&json!({"client_id": " cli ", "client_secret": "sec", "scopes": ["read", "write"], "callback_port": 8765})),
        )
        .unwrap();
        assert_eq!(s.client_id.as_deref(), Some("cli"));
        assert_eq!(s.client_secret.as_deref(), Some("sec"));
        assert_eq!(s.scopes.as_deref(), Some("read write"));
        assert_eq!(s.callback_port, Some(8765));
        let spaced = parse_settings("a", Some(&json!({"scopes": " read   write "}))).unwrap();
        assert_eq!(spaced.scopes.as_deref(), Some("read write"));
        for (bad, field) in [
            (json!("x"), "must be a JSON object"),
            (json!({"client_id": ""}), "oauth.client_id"),
            (json!({"client_id": 7}), "oauth.client_id"),
            (json!({"scopes": [1]}), "oauth.scopes"),
            (json!({"callback_port": 0}), "oauth.callback_port"),
            (json!({"callback_port": 70000}), "oauth.callback_port"),
        ] {
            let e = parse_settings("a", Some(&bad)).unwrap_err();
            assert!(e.contains(field), "{bad}: {e}");
        }
    }

    #[test]
    fn debug_output_never_shows_a_secret() {
        let s = OAuthSettings {
            client_secret: Some("shh-client-secret".into()),
            ..Default::default()
        };
        assert!(!format!("{s:?}").contains("shh-client-secret"));
        let l = sample_login("http://127.0.0.1:1/mcp");
        let shown = format!("{l:?}");
        assert!(!shown.contains("at-secret-access"), "{shown}");
        assert!(!shown.contains("rt-secret-refresh"), "{shown}");
    }

    // ── discovery ───────────────────────────────────────────────────────
    #[test]
    fn challenge_parsing_reads_only_the_bearer_params() {
        let c = parse_challenge(
            r#"Bearer resource_metadata="https://m.example/.well-known/oauth-protected-resource", scope="files:read files:write""#,
        );
        assert_eq!(
            c.resource_metadata.as_deref(),
            Some("https://m.example/.well-known/oauth-protected-resource")
        );
        assert_eq!(c.scope.as_deref(), Some("files:read files:write"));
        // Another scheme first, an unquoted value, an escaped quote.
        let c = parse_challenge(
            r#"Basic realm="x", scope="not-ours", bearer error=invalid_token, resource_metadata="https://m/a\"b""#,
        );
        assert_eq!(c.error.as_deref(), Some("invalid_token"));
        assert_eq!(c.resource_metadata.as_deref(), Some("https://m/a\"b"));
        assert_eq!(c.scope, None);
        assert_eq!(parse_challenge("Basic realm=\"r\""), Challenge::default());
        assert_eq!(parse_challenge(""), Challenge::default());
    }

    #[test]
    fn metadata_urls_follow_rfc_9728_and_rfc_8414() {
        assert_eq!(
            resource_metadata_urls(&url("https://m.example:8443/v1/mcp/")),
            [
                "https://m.example:8443/.well-known/oauth-protected-resource/v1/mcp",
                "https://m.example:8443/.well-known/oauth-protected-resource",
            ]
        );
        assert_eq!(
            resource_metadata_urls(&url("https://m.example")),
            ["https://m.example/.well-known/oauth-protected-resource"]
        );
        assert_eq!(
            server_metadata_urls(&url("https://auth.example/tenant1")),
            [
                "https://auth.example/.well-known/oauth-authorization-server/tenant1",
                "https://auth.example/.well-known/openid-configuration/tenant1",
                "https://auth.example/tenant1/.well-known/openid-configuration",
            ]
        );
        assert_eq!(
            server_metadata_urls(&url("https://auth.example/")),
            [
                "https://auth.example/.well-known/oauth-authorization-server",
                "https://auth.example/.well-known/openid-configuration",
            ]
        );
    }

    #[test]
    fn resource_metadata_must_name_this_server() {
        let mcp = url("https://m.example/api/mcp");
        assert!(resource_matches("https://m.example/api/mcp", &mcp));
        assert!(resource_matches("https://m.example/api/mcp/", &mcp));
        assert!(resource_matches("https://m.example/api", &mcp));
        assert!(resource_matches("https://m.example", &mcp));
        assert!(!resource_matches("https://m.example/api/mcp2", &mcp));
        assert!(!resource_matches("https://m.example/other", &mcp));
        assert!(!resource_matches("https://evil.example/api/mcp", &mcp));
        assert!(!resource_matches("http://m.example/api/mcp", &mcp));
        assert!(!resource_matches("https://m.example:444/api/mcp", &mcp));
        assert!(!resource_matches("not a url", &mcp));
    }

    #[test]
    fn credentials_go_only_over_tls_or_to_this_machine() {
        for ok in [
            "https://a.example/x",
            "http://127.0.0.1:9/x",
            "http://127.9.9.9/x",
            "http://localhost:1/x",
            "http://[::1]:2/x",
        ] {
            assert!(secure(&url(ok)), "{ok}");
        }
        for bad in [
            "http://a.example/x",
            "http://127.0.0.1.evil.example/x",
            "http://10.0.0.1/x",
            "ftp://127.0.0.1/x",
        ] {
            assert!(!secure(&url(bad)), "{bad}");
        }
        assert!(secure_url("http://a.example/t", "token_endpoint")
            .unwrap_err()
            .contains("not https://"));
    }

    #[test]
    fn server_metadata_is_checked_against_its_issuer() {
        let good = json!({
            "issuer": "https://auth.example/t",
            "authorization_endpoint": "https://auth.example/t/authorize",
            "token_endpoint": "https://auth.example/t/token",
            "registration_endpoint": "https://auth.example/t/register",
            "code_challenge_methods_supported": ["plain", "S256"],
            "authorization_response_iss_parameter_supported": true,
        });
        let m = parse_server_metadata(&good, "https://auth.example/t/").unwrap();
        assert_eq!(m.token_endpoint, "https://auth.example/t/token");
        assert_eq!(
            m.registration_endpoint.as_deref(),
            Some("https://auth.example/t/register")
        );
        assert!(m.iss_required);

        let mut v = good.clone();
        v["issuer"] = json!("https://evil.example/t");
        assert!(parse_server_metadata(&v, "https://auth.example/t")
            .unwrap_err()
            .contains("names issuer"));
        let mut v = good.clone();
        v["token_endpoint"] = json!("http://auth.example/t/token");
        assert!(parse_server_metadata(&v, "https://auth.example/t")
            .unwrap_err()
            .contains("token_endpoint"));
        let mut v = good.clone();
        v["code_challenge_methods_supported"] = json!(["plain"]);
        assert!(parse_server_metadata(&v, "https://auth.example/t")
            .unwrap_err()
            .contains("S256"));
        // No list at all: S256 is still what gets sent.
        let mut v = good.clone();
        v.as_object_mut()
            .unwrap()
            .remove("code_challenge_methods_supported");
        assert!(parse_server_metadata(&v, "https://auth.example/t").is_ok());
        let mut v = good;
        v.as_object_mut().unwrap().remove("authorization_endpoint");
        assert!(parse_server_metadata(&v, "https://auth.example/t")
            .unwrap_err()
            .contains("authorization_endpoint"));
    }

    // ── PKCE ────────────────────────────────────────────────────────────
    #[test]
    fn base64_matches_rfc_4648_vectors() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, encoded) in cases {
            assert_eq!(base64(plain.as_bytes(), false), encoded);
            assert_eq!(
                base64(plain.as_bytes(), true),
                encoded.trim_end_matches('=')
            );
        }
        assert_eq!(base64(&[0xfb, 0xff], true), "-_8");
        assert_eq!(base64(&[0xfb, 0xff], false), "+/8=");
    }

    #[test]
    fn code_challenge_matches_rfc_7636_appendix_b() {
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifiers_are_long_unreserved_and_fresh() {
        let a = random_string(48).unwrap();
        let b = random_string(48).unwrap();
        assert_eq!(a.len(), 64);
        assert!((43..=128).contains(&a.len()));
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(a, b);
    }

    // ── loopback redirect ───────────────────────────────────────────────
    #[test]
    fn callback_checks_state_before_anything_else() {
        let iss = "https://auth.example/t";
        let check = |t: &str| check_callback(t, "st8", iss, false);
        assert_eq!(check("/favicon.ico"), Callback::Ignore);
        assert_eq!(check("/"), Callback::Ignore);
        assert_eq!(
            check("/callback?code=abc&state=st8"),
            Callback::Code("abc".into())
        );
        assert_eq!(
            check(&format!("/callback?code=abc&state=st8&iss={iss}")),
            Callback::Code("abc".into())
        );
        for forged in [
            "/callback?code=abc&state=other",
            "/callback?code=abc",
            "/callback?state=other&error=access_denied",
        ] {
            match check(forged) {
                Callback::Fail(m) => assert!(m.contains("state"), "{forged}: {m}"),
                other => panic!("{forged}: {other:?}"),
            }
        }
        match check("/callback?code=abc&state=st8&iss=https://evil.example") {
            Callback::Fail(m) => assert!(m.contains("issuer"), "{m}"),
            other => panic!("{other:?}"),
        }
        match check_callback("/callback?code=abc&state=st8", "st8", iss, true) {
            Callback::Fail(m) => assert!(m.contains("iss"), "{m}"),
            other => panic!("{other:?}"),
        }
        match check("/callback?state=st8&error=access_denied&error_description=no+thanks") {
            Callback::Fail(m) => assert!(m.contains("access_denied: no thanks"), "{m}"),
            other => panic!("{other:?}"),
        }
        match check("/callback?state=st8") {
            Callback::Fail(m) => assert!(m.contains("no authorization code"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    fn get(port: u16, target: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn the_listener_skips_strays_and_answers_the_redirect() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let browser = std::thread::spawn(move || {
            let stray = get(port, "/favicon.ico");
            let done = get(port, "/callback?code=c0de&state=s");
            (stray, done)
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let code = wait_for_callback(&listener, "s", "https://i", false, deadline, &mut || Ok(()));
        assert_eq!(code.unwrap(), "c0de");
        let (stray, done) = browser.join().unwrap();
        assert!(stray.starts_with("HTTP/1.1 404"), "{stray}");
        assert!(done.starts_with("HTTP/1.1 200"), "{done}");
        assert!(done.contains("Signed in"), "{done}");
    }

    #[test]
    fn the_listener_rejects_a_forged_state_and_gives_up_on_time() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let browser = std::thread::spawn(move || get(port, "/callback?code=c&state=forged"));
        let deadline = Instant::now() + Duration::from_secs(10);
        let err = wait_for_callback(&listener, "s", "https://i", false, deadline, &mut || Ok(()))
            .unwrap_err();
        assert!(err.contains("state"), "{err}");
        assert!(browser.join().unwrap().starts_with("HTTP/1.1 400"));

        let started = Instant::now();
        let deadline = started + Duration::from_millis(200);
        let err = wait_for_callback(&listener, "s", "https://i", false, deadline, &mut || Ok(()))
            .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));

        let far = Instant::now() + Duration::from_secs(60);
        let err = wait_for_callback(&listener, "s", "https://i", false, far, &mut || {
            Err("cancelled".into())
        })
        .unwrap_err();
        assert_eq!(err, "cancelled");
    }

    // ── saved logins ────────────────────────────────────────────────────
    fn sample_login(url: &str) -> Login {
        Login {
            url: url.into(),
            resource: url.into(),
            issuer: "http://127.0.0.1:1/auth".into(),
            token_endpoint: "http://127.0.0.1:1/auth/token".into(),
            revocation_endpoint: None,
            client_id: "cli".into(),
            client_secret: None,
            token_endpoint_auth_method: None,
            redirect_uri: "http://127.0.0.1:2/callback".into(),
            registered: true,
            access_token: "at-secret-access".into(),
            refresh_token: Some("rt-secret-refresh".into()),
            expires_at: Some(unix_now() + 3600),
            scope: None,
        }
    }

    #[test]
    fn file_names_are_safe_and_never_shared() {
        assert_eq!(file_stem("github"), "github");
        assert_eq!(file_stem("my-server_2"), "my-server_2");
        let slashed = file_stem("../a/b");
        assert!(slashed.starts_with("___a_b-"), "{slashed}");
        assert!(!slashed.contains('/') && !slashed.contains('.'));
        assert_ne!(file_stem("a/b"), file_stem("a_b"));
        assert_ne!(file_stem("a/b"), file_stem("a.b"));
        assert!(file_stem("").starts_with('-'));
    }

    #[test]
    fn a_saved_login_is_owner_only_and_bound_to_its_url() {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("bwn-mcp-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("NEXUS_HOME", &home);
        let url = "http://127.0.0.1:1/mcp";
        let path = save("srv", &sample_login(url)).unwrap();
        assert_eq!(path, home.join("mcp-auth").join("srv.json"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode();
            assert_eq!(mode(&path) & 0o777, 0o600);
            assert_eq!(mode(&home.join("mcp-auth")) & 0o777, 0o700);
        }
        assert!(load("srv", url).is_some());
        // Settings that point the name somewhere else never get its token.
        assert!(load("srv", "http://127.0.0.1:1/other").is_none());
        assert!(Session::new("srv", "http://127.0.0.1:9/elsewhere")
            .bearer()
            .is_none());
        assert_eq!(
            Session::new("srv", url).bearer().as_deref(),
            Some("at-secret-access")
        );
        assert_eq!(describe("srv", url), "signed in (token expires in 60m)");
        assert_eq!(describe("srv", "http://127.0.0.1:1/other"), "not signed in");
        let mut expired = sample_login(url);
        expired.expires_at = Some(1);
        save("srv", &expired).unwrap();
        assert_eq!(
            describe("srv", url),
            "signed in (token expired; refreshed on next use)"
        );

        // Plain HTTP to another machine is never sent a token, even one
        // saved for that very URL.
        let remote = "http://mcp.example/mcp";
        save("plain", &sample_login(remote)).unwrap();
        assert!(Session::new("plain", remote).bearer().is_none());
        assert!(!Session::new("plain", remote).recover(None));

        // No revocation endpoint: the file just goes.
        let lines = logout("srv").unwrap().unwrap();
        assert!(lines[0].starts_with("signed out of srv"), "{lines:?}");
        assert!(!path.exists());
        assert!(logout("srv").unwrap().is_none());
        std::env::remove_var("NEXUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn scrubbing_reaches_every_string_in_a_reply() {
        let s = Session {
            server: "s".into(),
            url: "https://m".into(),
            allowed: true,
            login: None,
            seen: vec!["at-old-token".into(), "rt-refresh-token".into()],
            refused: None,
        };
        let mut v = json!({
            "content": [{"type": "text", "text": "got Bearer at-old-token and rt-refresh-token"}],
            "nested": {"list": ["at-old-token"]},
            "n": 1
        });
        s.scrub_value(&mut v);
        assert_eq!(
            v["content"][0]["text"],
            "got Bearer [redacted] and [redacted]"
        );
        assert_eq!(v["nested"]["list"][0], "[redacted]");
        assert_eq!(v["n"], 1);
        assert_eq!(s.scrub("HTTP 500: at-old-token"), "HTTP 500: [redacted]");
    }

    #[test]
    fn a_refresh_keeps_what_the_server_did_not_replace() {
        let mut l = sample_login("https://m/mcp");
        apply_tokens(
            &mut l,
            &json!({"access_token": "at-new", "expires_in": "120"}),
        );
        assert_eq!(l.access_token, "at-new");
        assert_eq!(l.refresh_token.as_deref(), Some("rt-secret-refresh"));
        let left = l.expires_at.unwrap() - unix_now();
        assert!((118..=120).contains(&left), "{left}");
        apply_tokens(
            &mut l,
            &json!({"access_token": "at-3", "refresh_token": "rt-3"}),
        );
        assert_eq!(l.refresh_token.as_deref(), Some("rt-3"));
        assert_eq!(l.expires_at, None);
        // An absurd lifetime is "far off", not an overflow (a panic here,
        // or a wrap to "already expired" that refreshes on every request).
        apply_tokens(
            &mut l,
            &json!({"access_token": "at-4", "expires_in": u64::MAX}),
        );
        assert!(l.expires_at.unwrap() > unix_now() + 3600);
    }

    #[test]
    fn login_hints_quote_names_and_follow_the_session() {
        assert_eq!(
            login_hint_for("remote", "", false),
            "run `bwn mcp login remote`"
        );
        assert_eq!(
            login_hint_for("my server", "", false),
            "run `bwn mcp login 'my server'`"
        );
        assert_eq!(login_hint_for("remote", "", true), "type /mcp login remote");
        assert_eq!(
            login_hint_for("remote", "the saved sign-in was refused: x", false),
            "run `bwn mcp login remote` (the saved sign-in was refused: x)"
        );
        assert_eq!(human_secs(45), "45s");
        assert_eq!(human_secs(3000), "50m");
        assert_eq!(human_secs(3 * 3600), "3h");
        assert_eq!(human_secs(3 * 86_400), "3d");
    }
}
