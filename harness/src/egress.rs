//! The checked way out for sandboxed commands.
//!
//! A sandboxed command may connect only to this proxy (the backends enforce
//! that; see sandbox.rs and landlock.rs), and the proxy decides, per
//! connection, whether the host it asks for may be reached. Tools that make
//! HTTP(S) requests find it through `HTTPS_PROXY` / `HTTP_PROXY`, as npm,
//! cargo, pip, git and curl do.
//!
//! The decision (`check`) is a table lookup plus one name resolution:
//! - only ports 443 and 80, except on this machine (loopback), whose own
//!   servers commands may reach on any port;
//! - a host `network.deny` names is refused;
//! - a host is let through when it is a known package or source host
//!   (`KNOWN_HOSTS`), `network.allow` names it, or the user named it in a
//!   message of this session. A host that only fetched content or a tool
//!   result mentioned is not: that text may be an attacker's;
//! - the name is resolved here and every address checked: a private,
//!   link-local or metadata address is refused unless `network.allow`
//!   names the host, and loopback (this machine's own servers) is allowed.
//!   The connection goes to the address that was checked, so a second
//!   lookup cannot point it elsewhere.
//!
//! A refusal answers 403 with the reason, and is kept so the command's
//! output can say which host was refused and how to allow it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Package registries and source hosts commands commonly need. Writing to
/// any of them takes a credential, and the credential stores are hidden
/// from commands, so reaching them does not give a way out for stolen data
/// on its own.
pub const KNOWN_HOSTS: &[&str] = &[
    "registry.npmjs.org",
    "registry.yarnpkg.com",
    "pypi.org",
    "files.pythonhosted.org",
    "crates.io",
    "index.crates.io",
    "static.crates.io",
    "static.rust-lang.org",
    "github.com",
    "api.github.com",
    "codeload.github.com",
    "objects.githubusercontent.com",
    "raw.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "gitlab.com",
    "bitbucket.org",
    "proxy.golang.org",
    "sum.golang.org",
    "rubygems.org",
    "index.rubygems.org",
    "repo.maven.apache.org",
    "repo1.maven.org",
    "plugins.gradle.org",
    "services.gradle.org",
    "api.nuget.org",
    "pkg.julialang.org",
    "registry-1.docker.io",
    "deb.debian.org",
    "archive.ubuntu.com",
    "security.ubuntu.com",
    "dl-cdn.alpinelinux.org",
];

/// Hosts the user named in a message this session (lowercase, no port).
static USER_HOSTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Refusals not yet reported to a command's output: (host:port, reason).
static REFUSED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// The hosts the user named this session.
pub fn user_hosts() -> Vec<String> {
    USER_HOSTS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Replace them (a child session takes its parent's, not its task's).
pub fn set_user_hosts(hosts: Vec<String>) {
    *USER_HOSTS.lock().unwrap_or_else(|e| e.into_inner()) = hosts;
}

#[cfg(not(test))]
static USER_HOSTS_FROZEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// Unit tests run side by side in one process: each test thread keeps its own.
#[cfg(test)]
thread_local! {
    static USER_HOSTS_FROZEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// From here on no text adds hosts: a background workflow's task was
/// written by its parent's model, not the user.
pub fn freeze_user_hosts() {
    #[cfg(not(test))]
    USER_HOSTS_FROZEN.store(true, std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    USER_HOSTS_FROZEN.with(|f| f.set(true));
}

fn user_hosts_frozen() -> bool {
    #[cfg(not(test))]
    return USER_HOSTS_FROZEN.load(std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    USER_HOSTS_FROZEN.with(|f| f.get())
}

/// Remember the hosts a message the user typed names (URLs and bare
/// `host.tld` words), so commands may reach them this session.
pub fn note_user_text(text: &str) {
    if user_hosts_frozen() {
        return;
    }
    let mut hosts = USER_HOSTS.lock().unwrap_or_else(|e| e.into_inner());
    for h in hosts_in(text) {
        if !hosts.contains(&h) {
            hosts.push(h);
        }
    }
}

fn hosts_in(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in text.split(|c: char| c.is_whitespace() || "<>()[]{}\"'`,;".contains(c)) {
        let w = word.trim_end_matches(['.', ':', '!', '?']);
        let host = if w.contains("://") {
            url::Url::parse(w)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
        } else {
            let h = w.split('/').next().unwrap_or("");
            let h = h.rsplit_once(':').map_or(h, |(h, p)| {
                if p.chars().all(|c| c.is_ascii_digit()) {
                    h
                } else {
                    ""
                }
            });
            let labels: Vec<&str> = h.split('.').collect();
            let ok = labels.len() >= 2
                && labels.iter().all(|l| {
                    !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                })
                && labels
                    .last()
                    .is_some_and(|t| t.len() >= 2 && t.chars().all(|c| c.is_ascii_alphabetic()));
            ok.then(|| h.to_string())
        };
        if let Some(h) = host {
            let h = h.trim_end_matches('.').to_ascii_lowercase();
            if !h.is_empty() && !out.contains(&h) {
                out.push(h);
            }
        }
    }
    out
}

/// The refusals since the last call, for the command's output.
pub fn take_refusals() -> Vec<(String, String)> {
    std::mem::take(&mut *REFUSED.lock().unwrap_or_else(|e| e.into_inner()))
}

/// The note a command's output gets for `refused`.
pub fn refusal_note(refused: &[(String, String)]) -> Option<String> {
    if refused.is_empty() {
        return None;
    }
    let list = refused
        .iter()
        .map(|(h, why)| format!("{h} ({why})"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "the sandbox's network checker refused: {list}. Commands reach only package and source hosts, hosts the user named in a message, and hosts in network.allow in settings.json. Ask the user before adding one."
    ))
}

/// What `check` needs from the settings, read once per connection.
pub struct Rules {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    /// A settings file that doesn't load: its network.deny can't be read,
    /// so every connection is refused (roadmap 1.3; analysis I4).
    pub broken: Option<String>,
}

impl Rules {
    pub fn load(cwd: &Path) -> Rules {
        let mut allow = Vec::new();
        let mut deny = Vec::new();
        for r in crate::config::policy_rules(cwd)
            .into_iter()
            .filter(|r| r.network)
        {
            match r.effect {
                crate::config::RuleEffect::Allow => allow.push(r.rule),
                crate::config::RuleEffect::Deny => deny.push(r.rule),
                _ => {}
            }
        }
        Rules {
            allow,
            deny,
            broken: crate::config::broken_policy_file(cwd),
        }
    }
}

fn matches(pattern: &str, host: &str) -> bool {
    let p = pattern.trim().to_ascii_lowercase();
    let p = p.rsplit_once(':').map_or(p.as_str(), |(h, port)| {
        if port.chars().all(|c| c.is_ascii_digit()) {
            h
        } else {
            p.as_str()
        }
    });
    match p.strip_prefix("*.") {
        Some(suffix) => host.ends_with(&format!(".{suffix}")),
        None => p == host,
    }
}

/// Whether a command may connect to `host:port`, and where: the addresses
/// that were checked. Err is the reason for a refusal.
///
/// With `via_proxy` (an upstream proxy carries the connection), a name that
/// does not resolve here counts as public: the proxy reaches it, not this
/// machine, and the result is empty. One that does resolve is checked.
pub fn check(
    host: &str,
    port: u16,
    rules: &Rules,
    via_proxy: bool,
) -> Result<Vec<SocketAddr>, String> {
    let host = host
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let literal: Option<IpAddr> = host.parse().ok();
    let loopback = host == "localhost" || literal.is_some_and(|ip| ip.is_loopback());
    // This machine's own servers (a dev server on :3000) on any port; the
    // rest of the world on 443 and 80.
    if let Some(why) = &rules.broken {
        return Err(why.clone());
    }
    if port != 443 && port != 80 && !loopback {
        return Err(format!("port {port}: only 443 and 80 are open to commands"));
    }
    if rules.deny.iter().any(|p| matches(p, &host)) {
        return Err("network.deny names it".into());
    }
    let allowed_by_rule = rules.allow.iter().any(|p| matches(p, &host));
    let known = KNOWN_HOSTS.contains(&host.as_str())
        || USER_HOSTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&host);
    if !(allowed_by_rule || known || loopback) {
        return Err(
            "not a package or source host, not named by the user, not in network.allow".into(),
        );
    }
    let addrs: Vec<SocketAddr> = match (host.as_str(), port).to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(_) if via_proxy => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot resolve: {e}")),
    };
    if addrs.is_empty() {
        return if via_proxy {
            Ok(Vec::new())
        } else {
            Err("resolves to no address".into())
        };
    }
    for a in &addrs {
        let ip = a.ip();
        if crate::tools::is_blocked_ip(ip) && !allowed_by_rule {
            return Err(format!("{ip} is a link-local or metadata address"));
        }
        if crate::tools::is_private_ip(ip) && !ip.is_loopback() && !allowed_by_rule {
            return Err(format!("{ip} is on a private network"));
        }
        if ip.is_loopback() && !loopback && !allowed_by_rule {
            return Err(format!("resolves to this machine ({ip})"));
        }
    }
    Ok(addrs)
}

// Connections the proxy let a command make to another machine (roadmap
// 5.1): a command's output is someone else's text once it reached out.
static REACHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
pub(crate) fn note_reached_for_test() {
    REACHED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// How many connections to other machines commands have made so far.
pub fn reached() -> u64 {
    REACHED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The proxy, started on the first sandboxed command that may use the
/// network, and kept for the session.
pub struct Proxy {
    pub port: u16,
}

static PROXY: OnceLock<Option<Proxy>> = OnceLock::new();

/// The session's proxy, starting it if needed. None when it cannot listen.
pub fn proxy() -> Option<&'static Proxy> {
    PROXY.get_or_init(start).as_ref()
}

fn start() -> Option<Proxy> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    let port = listener.local_addr().ok()?.port();
    serve_listener(listener, false).ok()?;
    Some(Proxy { port })
}

/// Check every connection on `listener` for the session. `private` is the
/// session namespace's listener (netns.rs): there, "this machine" is the
/// namespace, so a loopback target is dialled inside it and the host's own
/// services stay out of reach.
pub fn serve_listener(listener: TcpListener, private: bool) -> std::io::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    std::thread::Builder::new()
        .name("bwn-egress".into())
        .spawn(move || {
            for conn in listener.incoming().flatten() {
                let cwd = cwd.clone();
                std::thread::spawn(move || {
                    let _ = serve(conn, &cwd, private);
                });
            }
        })
        .map(|_| ())
}

/// The environment a sandboxed command gets so its tools use the proxy.
pub fn proxy_env(port: u16) -> Vec<(&'static str, String)> {
    let url = format!("http://127.0.0.1:{port}");
    [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .into_iter()
    .map(|k| (k, url.clone()))
    .collect()
}

/// Names a command must not inherit: a NO_PROXY would send requests around
/// the proxy, where they fail.
pub const UNSET_ENV: &[&str] = &["NO_PROXY", "no_proxy"];

fn refuse(mut client: TcpStream, target: &str, why: &str) -> std::io::Result<()> {
    REFUSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((target.to_string(), why.to_string()));
    let body = format!("bwn sandbox: refused {target}: {why}\n");
    write!(
        client,
        "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

// One client connection: a CONNECT (HTTPS) or an absolute-form request
// (plain HTTP).
fn serve(client: TcpStream, cwd: &Path, private: bool) -> std::io::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(client.try_clone()?);
    let mut head = Vec::new();
    loop {
        let n = reader.read_until(b'\n', &mut head)?;
        if n == 0 || head.len() > 16 * 1024 {
            return Ok(());
        }
        if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let mut first = text.lines().next().unwrap_or("").split_whitespace();
    let (method, target) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    // git through bwn (roadmap 3.3): answered here, never dialled.
    if method.eq_ignore_ascii_case("POST")
        && target.trim_end_matches('/') == format!("http://{}", crate::gitbroker::BROKER_HOST)
    {
        let len = text
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0)
            .min(1 << 20);
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body)?;
        let reply = crate::gitbroker::answer(&body);
        let mut client = client;
        return write!(
            client,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
    }
    let rules = Rules::load(cwd);
    let (host, port, connect) = if method.eq_ignore_ascii_case("CONNECT") {
        let (h, p) = split_host_port(target, 443);
        (h, p, true)
    } else {
        match url::Url::parse(target) {
            Ok(u) if u.scheme() == "http" => (
                u.host_str().unwrap_or("").to_string(),
                u.port_or_known_default().unwrap_or(80),
                false,
            ),
            _ => return refuse(client, target, "not a proxy request"),
        }
    };
    let label = format!("{host}:{port}");
    let scheme = if connect { "https" } else { "http" };
    let hop = crate::net::shared().upstream_for(&format!("{scheme}://{label}/"));
    let addrs = match check(&host, port, &rules, hop.is_some()) {
        Ok(a) => a,
        Err(why) => return refuse(client, &label, &why),
    };
    if !(host == "localhost" || addrs.iter().all(|a| a.ip().is_loopback()) && !addrs.is_empty()) {
        REACHED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    // From the private namespace, "this machine" is the namespace: a
    // loopback target is dialled inside it, never on the host or through an
    // upstream proxy.
    // A name that resolves to loopback and elsewhere too (a network.allow
    // entry) still never reaches the host's loopback: its loopback
    // addresses are dialled inside, the rest as usual, with no upstream
    // proxy for any of it.
    let inside = private && addrs.iter().any(|a| a.ip().is_loopback());
    let upstream = match &hop {
        _ if inside => addrs.iter().find_map(|a| {
            if a.ip().is_loopback() {
                dial_private(*a)
            } else {
                TcpStream::connect_timeout(a, Duration::from_secs(10)).ok()
            }
        }),
        // bwn's own proxy carries it: a tunnel to the checked name.
        Some((proxy, auth)) => tunnel(proxy, auth.as_deref(), &label),
        None => addrs
            .iter()
            .find_map(|a| TcpStream::connect_timeout(a, Duration::from_secs(10)).ok()),
    };
    let Some(upstream) = upstream else {
        let mut c = client;
        return write!(c, "HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n");
    };
    client.set_read_timeout(None)?;
    let mut client_w = client.try_clone()?;
    if connect {
        client_w.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    } else {
        (&upstream).write_all(&head)?;
    }
    // Whatever the client sent after the head is already buffered.
    let pending = reader.buffer().to_vec();
    if !pending.is_empty() {
        (&upstream).write_all(&pending)?;
    }
    splice(reader.into_inner(), client_w, upstream)
}

// A loopback address inside the session's namespace; never the host's.
#[cfg(target_os = "linux")]
fn dial_private(a: SocketAddr) -> Option<TcpStream> {
    crate::netns::session().ok()?.dial(a).ok()
}

#[cfg(not(target_os = "linux"))]
fn dial_private(_: SocketAddr) -> Option<TcpStream> {
    None
}

// A CONNECT tunnel to `target` through the upstream proxy at `proxy`.
fn tunnel(proxy: &str, auth: Option<&str>, target: &str) -> Option<TcpStream> {
    let addr = proxy.to_socket_addrs().ok()?.next()?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(10)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(30))).ok()?;
    let auth = auth
        .map(|a| format!("Proxy-Authorization: {a}\r\n"))
        .unwrap_or_default();
    write!(
        s,
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n{auth}\r\n"
    )
    .ok()?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if s.read(&mut byte).ok()? == 0 || head.len() > 16 * 1024 {
            return None;
        }
        head.push(byte[0]);
    }
    let status = String::from_utf8_lossy(&head);
    let ok = status
        .split_whitespace()
        .nth(1)
        .is_some_and(|c| c.starts_with('2'));
    s.set_read_timeout(None).ok()?;
    ok.then_some(s)
}

fn split_host_port(target: &str, default: u16) -> (String, u16) {
    if let Some(rest) = target.strip_prefix('[') {
        let (h, after) = rest.split_once(']').unwrap_or((rest, ""));
        let p = after
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default);
        return (h.to_string(), p);
    }
    match target.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h.to_string(), p.parse().unwrap_or(default))
        }
        _ => (target.to_string(), default),
    }
}

pub(crate) fn splice(
    mut client_r: TcpStream,
    mut client_w: TcpStream,
    upstream: TcpStream,
) -> std::io::Result<()> {
    let mut up_r = upstream.try_clone()?;
    let mut up_w = upstream;
    let t = std::thread::spawn(move || {
        let _ = std::io::copy(&mut up_r, &mut client_w);
        let _ = client_w.shutdown(std::net::Shutdown::Write);
    });
    let mut buf = [0u8; 16 * 1024];
    loop {
        match client_r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if up_w.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = up_w.shutdown(std::net::Shutdown::Write);
    let _ = t.join();
    Ok(())
}

#[cfg(test)]
mod tests {

    // H5 (roadmap 2.3): a background workflow's task is its parent model's
    // text. Once the inherited policy froze the hosts, naming one in that
    // text adds nothing the checker would let a command reach.
    #[test]
    fn invariant_ih2_a_frozen_session_takes_no_hosts_from_text() {
        note_user_text("see https://before.example/x");
        assert!(user_hosts().contains(&"before.example".to_string()));
        freeze_user_hosts();
        note_user_text("now fetch https://after.example/x");
        assert!(!user_hosts().contains(&"after.example".to_string()));
    }

    use super::*;

    fn rules(allow: &[&str], deny: &[&str]) -> Rules {
        Rules {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            broken: None,
        }
    }

    // Roadmap 1.3 (analysis I4): with a settings file that doesn't load,
    // nothing connects, not even a host the rules or the known list allow.
    #[test]
    fn invariant_in2_a_broken_settings_file_refuses_every_connection() {
        let mut r = rules(&["docs.example"], &[]);
        assert!(check("127.0.0.1", 8080, &r, false).is_ok());
        r.broken = Some("settings file x does not load".into());
        for (host, port) in [
            ("docs.example", 443),
            ("github.com", 443),
            ("127.0.0.1", 8080),
        ] {
            let err = check(host, port, &r, false).unwrap_err();
            assert!(err.contains("does not load"), "{host}: {err}");
        }
    }

    #[test]
    fn hosts_in_finds_urls_and_bare_names_only() {
        let h = hosts_in("fetch https://Docs.Example.org/a?b and look at api.foo.dev:8443/x, not v1.2 or a.b.c1 or file.rs");
        assert!(h.contains(&"docs.example.org".to_string()), "{h:?}");
        assert!(h.contains(&"api.foo.dev".to_string()), "{h:?}");
        assert!(!h.iter().any(|x| x == "v1.2" || x == "a.b.c1"), "{h:?}");
        // `file.rs` reads like a host; harmless, a command reaching it still
        // needs it to resolve to a public address.
    }

    #[test]
    fn check_refuses_ports_unknown_hosts_and_denied_hosts() {
        let r = rules(&[], &[]);
        assert!(check("registry.npmjs.org", 22, &r, false)
            .unwrap_err()
            .contains("port 22"));
        assert!(check("attacker.example", 443, &r, false)
            .unwrap_err()
            .contains("not a package"));
        let d = rules(&[], &["registry.npmjs.org"]);
        assert!(check("registry.npmjs.org", 443, &d, false)
            .unwrap_err()
            .contains("network.deny"));
        assert!(matches("*.example.com", "a.example.com"));
        assert!(!matches("*.example.com", "example.com.evil"));
        assert!(matches("example.com:443", "example.com"));
    }

    #[test]
    fn check_refuses_private_and_metadata_addresses_even_when_named() {
        note_user_text("see http://10.1.2.3/ and http://169.254.169.254/latest");
        let r = rules(&[], &[]);
        assert!(check("10.1.2.3", 80, &r, false)
            .unwrap_err()
            .contains("private"));
        assert!(check("169.254.169.254", 80, &r, false)
            .unwrap_err()
            .contains("metadata"));
        // An explicit allow rule is the user's call.
        assert!(check("10.1.2.3", 80, &rules(&["10.1.2.3"], &[]), false).is_ok());
        // This machine's own servers stay reachable.
        assert!(check("127.0.0.1", 80, &r, false).is_ok());
    }

    #[test]
    fn user_named_hosts_are_let_through() {
        let r = rules(&[], &[]);
        note_user_text("deploy to localhost and https://LOCALHOST:443");
        assert!(check("localhost", 443, &r, false).is_ok());
    }

    // The proxy end to end: a refused host gets a 403 with the reason and is
    // reported once; an allowed loopback target is reached through CONNECT.
    #[test]
    fn proxy_refuses_and_connects() {
        let p = proxy().expect("proxy listens");
        let target = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let tport = target.local_addr().unwrap().port();
        let mut c = TcpStream::connect(("127.0.0.1", p.port)).unwrap();
        write!(
            c,
            "CONNECT attacker.example:443 HTTP/1.1\r\nHost: attacker.example:443\r\n\r\n"
        )
        .unwrap();
        let mut s = String::new();
        c.read_to_string(&mut s).unwrap();
        assert!(s.starts_with("HTTP/1.1 403"), "{s}");
        assert!(s.contains("not a package"), "{s}");
        let refused = take_refusals();
        assert!(
            refused.iter().any(|(h, _)| h == "attacker.example:443"),
            "{refused:?}"
        );
        assert!(refusal_note(&refused).unwrap().contains("network.allow"));
        // A server on this machine, any port: tunnelled both ways.
        let echo = std::thread::spawn(move || {
            let (mut sock, _) = target.accept().unwrap();
            let mut b = [0u8; 4];
            sock.read_exact(&mut b).unwrap();
            sock.write_all(&b).unwrap();
        });
        let mut c = TcpStream::connect(("127.0.0.1", p.port)).unwrap();
        write!(c, "CONNECT 127.0.0.1:{tport} HTTP/1.1\r\n\r\n").unwrap();
        let mut head = [0u8; 39];
        c.read_exact(&mut head).unwrap();
        assert!(
            head.starts_with(b"HTTP/1.1 200"),
            "{}",
            String::from_utf8_lossy(&head)
        );
        c.write_all(b"ping").unwrap();
        let mut back = [0u8; 4];
        c.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"ping");
        echo.join().unwrap();
        // Another machine on another port is refused by port.
        assert!(check("registry.npmjs.org", 8080, &rules(&[], &[]), false)
            .unwrap_err()
            .contains("only 443"));
    }

    #[test]
    fn base64_matches_the_standard_vectors() {
        let b = crate::net::base64;
        assert_eq!(b(b""), "");
        assert_eq!(b(b"f"), "Zg==");
        assert_eq!(b(b"fo"), "Zm8=");
        assert_eq!(b(b"foo"), "Zm9v");
        assert_eq!(b(b"user:p@ss"), "dXNlcjpwQHNz");
    }

    // Through an upstream proxy: CONNECT with the credential, then bytes
    // flow; a refusal from the upstream proxy is no tunnel.
    #[test]
    fn tunnel_goes_through_the_upstream_proxy() {
        let up = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = up.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            for (i, conn) in up.incoming().take(2).enumerate() {
                let mut c = conn.unwrap();
                let mut r = BufReader::new(c.try_clone().unwrap());
                let mut head = String::new();
                while !head.ends_with("\r\n\r\n") {
                    r.read_line(&mut head).unwrap();
                }
                assert!(
                    head.starts_with("CONNECT registry.npmjs.org:443 HTTP/1.1"),
                    "{head}"
                );
                if i == 0 {
                    assert!(
                        head.contains("Proxy-Authorization: Basic dXNlcjpwQHNz"),
                        "{head}"
                    );
                    c.write_all(b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
                    let mut b = [0u8; 2];
                    r.read_exact(&mut b).unwrap();
                    c.write_all(&b).unwrap();
                } else {
                    c.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").unwrap();
                }
            }
        });
        let mut t = tunnel(&addr, Some("Basic dXNlcjpwQHNz"), "registry.npmjs.org:443").unwrap();
        t.write_all(b"hi").unwrap();
        let mut b = [0u8; 2];
        t.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"hi");
        assert!(tunnel(&addr, None, "registry.npmjs.org:443").is_none());
        server.join().unwrap();
    }
}
