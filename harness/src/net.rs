// Every HTTP client bwn creates is built here, so all of them behave the same
// behind a corporate network: they honor HTTPS_PROXY, HTTP_PROXY, ALL_PROXY
// and NO_PROXY (loopback is never proxied), and they trust the OS
// certificate store (or SSL_CERT_FILE / SSL_CERT_DIR) on top of the bundled
// webpki roots, so a TLS-inspecting proxy whose CA IT installed works.
//
// ureq 2 sets its proxy per agent, so a Client holds one agent per route
// (direct, and one per proxy URL) and picks by destination URL.

use std::net::IpAddr;
use std::sync::{Arc, OnceLock};

type Configure<'a> = &'a dyn Fn(ureq::AgentBuilder) -> ureq::AgentBuilder;

pub struct Client {
    direct: ureq::Agent,
    proxied: Vec<(String, ureq::Agent)>,
    env: ProxyEnv,
}

impl Client {
    /// Agents from `configure` (timeouts, redirects, resolver), one per
    /// route the process environment names.
    pub fn new(configure: impl Fn(ureq::AgentBuilder) -> ureq::AgentBuilder) -> Client {
        Client::with_env(ProxyEnv::from_env(), &configure)
    }

    /// A client whose proxy settings come from `get` instead of the process
    /// environment, for tests.
    #[cfg(test)]
    pub(crate) fn with_lookup(
        get: impl Fn(&str) -> Option<String>,
        configure: impl Fn(ureq::AgentBuilder) -> ureq::AgentBuilder,
    ) -> Client {
        Client::with_env(ProxyEnv::from_lookup(get), &configure)
    }

    fn with_env(env: ProxyEnv, configure: Configure) -> Client {
        let base = || configure(ureq::AgentBuilder::new().tls_config(tls_config()));
        let mut proxied: Vec<(String, ureq::Agent)> = Vec::new();
        for url in [&env.https, &env.http].into_iter().flatten() {
            if proxied.iter().any(|(u, _)| u == url) {
                continue;
            }
            // An unusable value fails every request that would use it with
            // the reason, instead of quietly connecting direct.
            let agent = match parse_proxy(url) {
                Ok(proxy) => base().proxy(proxy).build(),
                Err(why) => base().resolver(Unusable(why)).build(),
            };
            proxied.push((url.clone(), agent));
        }
        Client {
            direct: base().build(),
            proxied,
            env,
        }
    }

    /// The agent for `url`: through its proxy, or direct.
    pub fn agent_for(&self, url: &str) -> &ureq::Agent {
        self.env
            .proxy_for(url)
            .and_then(|p| self.proxied.iter().find(|(u, _)| u == p))
            .map_or(&self.direct, |(_, a)| a)
    }

    pub fn get(&self, url: &str) -> ureq::Request {
        self.agent_for(url).get(url)
    }

    pub fn post(&self, url: &str) -> ureq::Request {
        self.agent_for(url).post(url)
    }
}

/// A client with ureq's defaults, for one-off requests.
pub fn shared() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| Client::new(|b| b))
}

/// What to tell the user when `e` is a certificate the roots do not trust.
/// Retrying cannot fix it, so callers fail at once with this.
pub fn cert_error_hint(e: &ureq::Error) -> Option<&'static str> {
    e.to_string()
        .contains("invalid peer certificate")
        .then_some(
            "The server's certificate is not trusted. Behind a TLS-inspecting proxy, \
         install its CA in the OS certificate store or set SSL_CERT_FILE to a PEM file holding it.",
        )
}

// ── proxy environment ──────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct ProxyEnv {
    https: Option<String>,
    http: Option<String>,
    no_proxy: Vec<Rule>,
}

impl ProxyEnv {
    fn from_env() -> ProxyEnv {
        ProxyEnv::from_lookup(|k| std::env::var(k).ok())
    }

    // Lower case first, as curl reads them; an empty value counts as unset.
    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> ProxyEnv {
        let first = |keys: [&str; 2]| {
            keys.iter()
                .filter_map(|k| get(k))
                .map(|v| v.trim().to_string())
                .find(|v| !v.is_empty())
        };
        let all = first(["all_proxy", "ALL_PROXY"]);
        ProxyEnv {
            https: first(["https_proxy", "HTTPS_PROXY"]).or_else(|| all.clone()),
            http: first(["http_proxy", "HTTP_PROXY"]).or(all),
            no_proxy: first(["no_proxy", "NO_PROXY"])
                .map(|v| parse_no_proxy(&v))
                .unwrap_or_default(),
        }
    }

    /// The proxy URL for `url`, or None to connect direct.
    fn proxy_for(&self, url: &str) -> Option<&str> {
        let parsed = url::Url::parse(url).ok()?;
        let proxy = match parsed.scheme() {
            "https" | "wss" => self.https.as_deref(),
            "http" | "ws" => self.http.as_deref(),
            _ => None,
        }?;
        let host = Host::of(&parsed)?;
        let bypass = host.is_loopback() || self.no_proxy.iter().any(|r| r.matches(&host));
        (!bypass).then_some(proxy)
    }
}

enum Host {
    Name(String),
    Ip(IpAddr),
}

impl Host {
    fn of(url: &url::Url) -> Option<Host> {
        Some(match url.host()? {
            url::Host::Domain(d) => Host::Name(d.trim_end_matches('.').to_ascii_lowercase()),
            url::Host::Ipv4(v4) => Host::Ip(IpAddr::V4(v4)),
            url::Host::Ipv6(v6) => Host::Ip(v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4)),
        })
    }

    // Local model servers (Ollama, LM Studio, llama.cpp) listen here; a
    // corporate proxy cannot reach them, so no setting sends them there.
    // 0.0.0.0 is how OLLAMA_HOST is often written and means this machine.
    fn is_loopback(&self) -> bool {
        match self {
            Host::Name(n) => n == "localhost" || n.ends_with(".localhost"),
            Host::Ip(ip) => ip.is_loopback() || ip.is_unspecified(),
        }
    }
}

// One NO_PROXY entry: `*`, a domain (which also covers its subdomains;
// a leading `.` or `*.` is the same thing), an IP address, or a CIDR block.
// A port on an entry is accepted and ignored.
#[derive(Debug, PartialEq)]
enum Rule {
    Any,
    Domain(String),
    Net(IpAddr, u8),
}

fn parse_no_proxy(value: &str) -> Vec<Rule> {
    value
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter_map(|e| parse_rule(&e.to_ascii_lowercase()))
        .collect()
}

fn parse_rule(entry: &str) -> Option<Rule> {
    if entry == "*" {
        return Some(Rule::Any);
    }
    if let Some((addr, bits)) = entry.split_once('/') {
        let ip = unbracket(addr).parse::<IpAddr>().ok()?;
        let bits = bits.parse::<u8>().ok()?;
        let max = if ip.is_ipv4() { 32 } else { 128 };
        return (bits <= max).then(|| Rule::Net(canonical(ip), bits));
    }
    if let Ok(ip) = unbracket(entry).parse::<IpAddr>() {
        return Some(Rule::Net(canonical(ip), full_bits(ip)));
    }
    // [v6]:port, v4:port, name:port
    let hostpart = match entry.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => entry.rsplit_once(':').map_or(entry, |(h, _)| h),
    };
    if let Ok(ip) = hostpart.parse::<IpAddr>() {
        return Some(Rule::Net(canonical(ip), full_bits(ip)));
    }
    let name = hostpart.trim_start_matches("*.").trim_start_matches('.');
    let name = name.trim_end_matches('.');
    (!name.is_empty()).then(|| Rule::Domain(name.to_string()))
}

impl Rule {
    fn matches(&self, host: &Host) -> bool {
        match (self, host) {
            (Rule::Any, _) => true,
            (Rule::Domain(d), Host::Name(n)) => {
                n == d || (n.ends_with(d.as_str()) && n[..n.len() - d.len()].ends_with('.'))
            }
            (Rule::Net(net, bits), Host::Ip(ip)) => in_net(*ip, *net, *bits),
            _ => false,
        }
    }
}

fn unbracket(s: &str) -> &str {
    s.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(s)
}

fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

fn full_bits(ip: IpAddr) -> u8 {
    if canonical(ip).is_ipv4() {
        32
    } else {
        128
    }
}

fn in_net(ip: IpAddr, net: IpAddr, bits: u8) -> bool {
    match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(bits)).unwrap_or(0);
            u32::from(a) & mask == u32::from(b) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(bits)).unwrap_or(0);
            u128::from(a) & mask == u128::from(b) & mask
        }
        _ => false,
    }
}

// ureq takes `[http://][user:pass@]host[:port]`, without percent-decoding
// the credentials. TLS to the proxy itself (https://) and SOCKS are not
// built in.
fn parse_proxy(value: &str) -> Result<ureq::Proxy, String> {
    let rest = match value.split_once("://") {
        None => value,
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
        Some((scheme, _)) => {
            return Err(format!(
                "proxy {} uses {scheme}://, which bwn does not support; use an http:// proxy URL",
                redact_userinfo(value)
            ))
        }
    };
    let authority = rest.split('/').next().unwrap_or("");
    let target = match authority.rsplit_once('@') {
        Some((creds, hostport)) => {
            let (user, pass) = creds.split_once(':').unwrap_or((creds, ""));
            format!(
                "http://{}:{}@{hostport}",
                percent_decode(user),
                percent_decode(pass)
            )
        }
        None => format!("http://{authority}"),
    };
    ureq::Proxy::new(target)
        .map_err(|e| format!("proxy {} is not usable: {e}", redact_userinfo(value)))
}

fn redact_userinfo(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").map_or(("", url), |(s, r)| (s, r));
    let sep = if scheme.is_empty() { "" } else { "://" };
    // The host follows the last `@`: a password may hold an unencoded `@`
    // or `/`, and hiding too much beats showing part of it.
    match rest.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}{sep}***@{host}"),
        None => url.to_string(),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct Unusable(String);

impl ureq::Resolver for Unusable {
    fn resolve(&self, _netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
        Err(std::io::Error::other(self.0.clone()))
    }
}

// ── trust roots ────────────────────────────────────────────────────────────

// BWN_TLS_ROOTS=bundled trusts the bundled roots alone, as bwn did before
// 0.15; anything else adds what rustls-native-certs finds: SSL_CERT_FILE
// and SSL_CERT_DIR when either is set, the OS store otherwise.
fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let bundled_only = std::env::var("BWN_TLS_ROOTS")
                .is_ok_and(|v| v.trim().eq_ignore_ascii_case("bundled"));
            let mut roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            if !bundled_only {
                roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
            }
            Arc::new(client_config(roots))
        })
        .clone()
}

// The same provider and versions ureq's own default config uses.
fn client_config(roots: rustls::RootCertStore) -> rustls::ClientConfig {
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
        .expect("ring supports TLS 1.2 and 1.3")
        .with_root_certificates(roots)
        .with_no_client_auth()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::Mutex;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn scheme_picks_the_proxy_variable() {
        let e = ProxyEnv::from_lookup(env(&[
            ("HTTPS_PROXY", "http://secure:3128"),
            ("HTTP_PROXY", "http://plain:3128"),
        ]));
        assert_eq!(
            e.proxy_for("https://api.openai.com/v1"),
            Some("http://secure:3128")
        );
        assert_eq!(
            e.proxy_for("http://example.com/"),
            Some("http://plain:3128")
        );

        // ALL_PROXY fills in for either; lower case wins; empty is unset.
        let e = ProxyEnv::from_lookup(env(&[
            ("ALL_PROXY", "http://all:1"),
            ("https_proxy", "http://lower:2"),
            ("HTTPS_PROXY", "http://upper:3"),
            ("http_proxy", " "),
        ]));
        assert_eq!(e.proxy_for("https://a.example/"), Some("http://lower:2"));
        assert_eq!(e.proxy_for("http://a.example/"), Some("http://all:1"));

        assert_eq!(
            ProxyEnv::from_lookup(env(&[])).proxy_for("https://a.example/"),
            None
        );
    }

    #[test]
    fn loopback_is_never_proxied() {
        let e = ProxyEnv::from_lookup(env(&[
            ("HTTPS_PROXY", "http://proxy:3128"),
            ("HTTP_PROXY", "http://proxy:3128"),
        ]));
        for url in [
            "http://localhost:11434/api/tags",
            "http://LOCALHOST.:1234/v1",
            "http://llm.localhost:8080/v1",
            "http://127.0.0.1:8080/v1",
            "http://127.1.2.3/",
            "http://[::1]:11434",
            "http://[::ffff:127.0.0.1]/",
            "http://0.0.0.0:11434",
            "https://localhost:8443/v1",
        ] {
            assert_eq!(e.proxy_for(url), None, "{url}");
        }
        assert!(e.proxy_for("http://localhost.example.com/").is_some());
        assert!(e.proxy_for("http://10.0.0.5:11434/").is_some());
    }

    #[test]
    fn no_proxy_forms() {
        let e = ProxyEnv::from_lookup(env(&[
            ("HTTPS_PROXY", "http://proxy:3128"),
            (
                "NO_PROXY",
                "corp.example, .internal.test,*.svc.cluster.local ,exact.test:8443 \
                 10.0.0.0/8,192.168.1.7,[fd00::1],fc00::/7",
            ),
        ]));
        let direct = [
            "https://corp.example/",
            "https://git.corp.example/",
            "https://CORP.EXAMPLE./",
            "https://internal.test/",
            "https://a.b.internal.test/",
            "https://db.ns.svc.cluster.local/",
            "https://exact.test/",
            "https://10.20.30.40/",
            "https://192.168.1.7:8443/",
            "https://[fd00::1]/",
            "https://[fc12::5]/",
        ];
        for url in direct {
            assert_eq!(e.proxy_for(url), None, "{url} should bypass");
        }
        let proxied = [
            "https://notcorp.example/",
            "https://corp.example.org/",
            "https://11.0.0.1/",
            "https://192.168.1.8/",
            "https://[fe00::1]/",
            "https://api.openai.com/",
        ];
        for url in proxied {
            assert!(e.proxy_for(url).is_some(), "{url} should be proxied");
        }

        let all = ProxyEnv::from_lookup(env(&[("HTTPS_PROXY", "http://p:1"), ("no_proxy", "*")]));
        assert_eq!(all.proxy_for("https://api.openai.com/"), None);
        // Lower case wins here too.
        let lower = ProxyEnv::from_lookup(env(&[
            ("HTTPS_PROXY", "http://p:1"),
            ("no_proxy", "a.test"),
            ("NO_PROXY", "b.test"),
        ]));
        assert_eq!(lower.proxy_for("https://a.test/"), None);
        assert!(lower.proxy_for("https://b.test/").is_some());
        assert_eq!(parse_rule("10.0.0.0/33"), None);
        assert_eq!(parse_rule(""), None);
    }

    #[test]
    fn proxy_urls() {
        let p = format!(
            "{:?}",
            parse_proxy("http://u%40corp:p%3As@proxy.corp:3128/").unwrap()
        );
        assert!(p.contains("server: \"proxy.corp\""), "{p}");
        assert!(p.contains("port: 3128"), "{p}");
        assert!(p.contains("user: Some(\"u@corp\")"), "{p}");
        assert!(p.contains("password: Some(\"p:s\")"), "{p}");
        assert!(parse_proxy("proxy.corp:8080").is_ok());
        for bad in ["https://proxy:443", "socks5://proxy:1080"] {
            let e = parse_proxy(bad).unwrap_err();
            assert!(e.contains("http://"), "{e}");
        }
        let e = parse_proxy("socks5h://me:secret@proxy:1080").unwrap_err();
        assert!(!e.contains("secret"), "{e}");
        // An unencoded `@` in the password: the host follows the last one.
        let e = parse_proxy("socks5://me:s3cr@tpart@proxy:1080").unwrap_err();
        assert!(!e.contains("tpart"), "{e}");
        assert!(e.contains("socks5://***@proxy:1080"), "{e}");
        let e = parse_proxy("socks5://me:a/b@c@proxy:1080").unwrap_err();
        assert!(!e.contains("a/b") && !e.contains("c@"), "{e}");
    }

    // A one-line HTTP server that answers every request with `body` and
    // records each request line.
    fn server(body: &'static str) -> (u16, std::sync::Arc<Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                let _ = reader.read_line(&mut first);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                log.lock().unwrap().push(first.trim().to_string());
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (port, seen)
    }

    #[test]
    fn requests_take_the_route_the_environment_names() {
        let (origin, origin_seen) = server("direct");
        let (proxy, proxy_seen) = server("proxied");
        let proxy_url = format!("http://127.0.0.1:{proxy}");
        // Every name resolves to this machine, at the port asked for.
        let client = Client::with_lookup(
            env(&[
                ("HTTP_PROXY", proxy_url.as_str()),
                ("NO_PROXY", "skip.test"),
            ]),
            |b| {
                b.resolver(|netloc: &str| {
                    let port = netloc.rsplit(':').next().unwrap().parse().unwrap();
                    Ok(vec![std::net::SocketAddr::from(([127, 0, 0, 1], port))])
                })
            },
        );
        let get = |url: String| client.get(&url).call().unwrap().into_string().unwrap();

        assert_eq!(get(format!("http://via.test:{origin}/a")), "proxied");
        assert_eq!(get(format!("http://skip.test:{origin}/b")), "direct");
        assert_eq!(get(format!("http://api.skip.test:{origin}/c")), "direct");
        assert_eq!(get(format!("http://localhost:{origin}/d")), "direct");
        assert_eq!(get(format!("http://127.0.0.1:{origin}/e")), "direct");

        assert_eq!(
            *proxy_seen.lock().unwrap(),
            [format!("GET http://via.test:{origin}/a HTTP/1.1")]
        );
        assert_eq!(origin_seen.lock().unwrap().len(), 4);
    }

    // HTTPS goes through CONNECT. Neither end speaks TLS here, so every
    // request fails; what matters is which listener each one reached.
    #[test]
    fn https_tunnels_through_connect_unless_no_proxy_matches() {
        let origin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin_port = origin.local_addr().unwrap().port();
        let reached = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = reached.clone();
        std::thread::spawn(move || {
            for stream in origin.incoming() {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                drop(stream);
            }
        });
        let (proxy, proxy_seen) = server("");
        let proxy_url = format!("http://127.0.0.1:{proxy}");
        let client = Client::with_lookup(
            env(&[
                ("HTTPS_PROXY", proxy_url.as_str()),
                ("NO_PROXY", "skip.test,10.0.0.0/8"),
            ]),
            |b| {
                b.resolver(|netloc: &str| {
                    let port = netloc.rsplit(':').next().unwrap().parse().unwrap();
                    Ok(vec![std::net::SocketAddr::from(([127, 0, 0, 1], port))])
                })
            },
        );
        let hit = |url: String| {
            assert!(client.get(&url).call().is_err(), "{url}");
            reached.load(std::sync::atomic::Ordering::SeqCst)
        };

        assert_eq!(hit(format!("https://via.test:{origin_port}/")), 0);
        assert_eq!(
            *proxy_seen.lock().unwrap(),
            [format!("CONNECT via.test:{origin_port} HTTP/1.1")]
        );
        assert_eq!(hit(format!("https://skip.test:{origin_port}/")), 1);
        assert_eq!(hit(format!("https://api.skip.test:{origin_port}/")), 2);
        assert_eq!(hit(format!("https://10.1.2.3:{origin_port}/")), 3);
        assert_eq!(hit(format!("https://localhost:{origin_port}/")), 4);
        assert_eq!(hit(format!("https://[::1]:{origin_port}/")), 5);
        assert_eq!(proxy_seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_unusable_proxy_fails_its_requests_with_the_reason() {
        let client = Client::with_lookup(env(&[("HTTPS_PROXY", "socks5://proxy:1080")]), |b| b);
        let e = client.get("https://api.example.com/").call().unwrap_err();
        assert!(e.to_string().contains("use an http:// proxy URL"), "{e}");
    }
}
