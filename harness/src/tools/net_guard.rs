//! The network guard: which hosts the fetch and browser tools may reach, and the resolver that enforces it.

use super::*;

// ── network guard ─────────────────────────────────────────────────────────────
// Link-local addresses and cloud metadata endpoints hand out instance
// credentials, so the fetch tools never reach them: not by literal, not by a
// hostname that resolves there, not through a redirect. Loopback stays
// usable for dev servers.
pub(super) fn is_blocked_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => v4.is_link_local() || v4.octets() == [100, 100, 100, 200],
        IpAddr::V6(v6) => {
            (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6.segments() == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254]
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_blocked_ip(IpAddr::V4(v4)))
        }
    }
}

pub(super) const METADATA_HOSTS: &[&str] = &[
    "metadata",
    "metadata.google.internal",
    "metadata.goog",
    "instance-data",
    "instance-data.ec2.internal",
];

pub(super) fn url_host(url: &str) -> Option<String> {
    // The host the request reaches, as the URL parser reads it: `2852039166`,
    // `0xa9.254.169.254`, `%31%36%39.254.169.254` and a backslash before an
    // `@` all name an address that a hand-cut authority would miss.
    if let Some(host) = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().filter(|h| !h.is_empty()).map(String::from))
    {
        let host = crate::net::canonical_authority(&host);
        return Some(host.trim_matches(['[', ']']).to_string());
    }
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next()?;
    let hostport = authority.rsplit('@').next()?;
    let host = if let Some(v6) = hostport.strip_prefix('[') {
        v6.split(']').next()?
    } else {
        hostport.split(':').next()?
    };
    Some(host.trim_end_matches('.').to_ascii_lowercase())
}

/// Why a fetch tool must not request `url`, if it must not.
pub fn blocked_url(url: &str) -> Option<String> {
    let host = url_host(url)?;
    let literal = host.parse::<std::net::IpAddr>().is_ok_and(is_blocked_ip);
    (literal || METADATA_HOSTS.contains(&host.as_str())).then(|| {
        format!("refusing to fetch {url}: link-local and cloud metadata addresses are blocked")
    })
}

pub(super) struct GuardedResolver;

impl ureq::Resolver for GuardedResolver {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
        use std::net::ToSocketAddrs;
        let addrs: Vec<_> = netloc.to_socket_addrs()?.collect();
        if addrs.iter().any(|a| is_blocked_ip(a.ip())) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{netloc} resolves to a link-local or metadata address"),
            ));
        }
        Ok(addrs)
    }
}

// The client every fetch tool uses. Redirects are followed by web_get, not
// ureq, so each hop is checked again and takes its own route: through a
// proxy only the proxy resolves names, and GuardedResolver never sees them.
pub(super) fn web_client() -> &'static crate::net::Client {
    static CLIENT: OnceLock<crate::net::Client> = OnceLock::new();
    CLIENT.get_or_init(|| crate::net::Client::new(|b| b.resolver(GuardedResolver).redirects(0)))
}

pub(super) enum WebError {
    Blocked(String),
    Http(Box<ureq::Error>),
}

impl std::fmt::Display for WebError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebError::Blocked(why) => f.write_str(why),
            WebError::Http(e) => e.fmt(f),
        }
    }
}

pub(super) const MAX_REDIRECTS: usize = 5;

thread_local! {
    // The folder whose network.deny rules the running fetch tool answers to,
    // so a redirect cannot lead it somewhere a rule refuses.
    static FETCH_CWD: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

pub(super) struct FetchPolicy;

impl FetchPolicy {
    pub(super) fn set(cwd: &Path) -> FetchPolicy {
        FETCH_CWD.with_borrow_mut(|c| *c = Some(cwd.to_path_buf()));
        FetchPolicy
    }
}

impl Drop for FetchPolicy {
    fn drop(&mut self) {
        FETCH_CWD.with_borrow_mut(|c| *c = None);
    }
}

// network.deny for a redirect's target: the call itself was checked before
// it ran, but where the server sends it on was not.
pub(super) fn redirect_denied(to: &url::Url) -> Option<String> {
    let cwd = FETCH_CWD.with_borrow(|c| c.clone())?;
    let host = url_authority(to.as_str())?;
    let why = crate::agent::network_denied(&cwd, &host)?;
    Some(format!("refusing to follow the redirect to {to}: {why}"))
}

pub(super) fn web_get(url: &str, user_agent: Option<&str>) -> Result<ureq::Response, WebError> {
    web_get_with(web_client(), url, user_agent)
}

pub(super) fn web_get_with(
    client: &crate::net::Client,
    url: &str,
    user_agent: Option<&str>,
) -> Result<ureq::Response, WebError> {
    let mut url = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        if let Some(why) = blocked_url(&url) {
            return Err(WebError::Blocked(why));
        }
        let mut req = client.get(&url);
        if let Some(ua) = user_agent {
            req = req.set("User-Agent", ua);
        }
        let resp = req.call().map_err(|e| WebError::Http(Box::new(e)))?;
        let next = matches!(resp.status(), 301 | 302 | 303 | 307 | 308)
            .then(|| resp.header("location"))
            .flatten()
            .and_then(|to| url::Url::parse(&url).ok()?.join(to).ok());
        match next {
            Some(next) => {
                if let Some(why) = redirect_denied(&next) {
                    return Err(WebError::Blocked(why));
                }
                url = next.to_string();
            }
            None => return Ok(resp),
        }
    }
    Err(WebError::Blocked(format!(
        "stopped after {MAX_REDIRECTS} redirects at {url}"
    )))
}

pub(super) fn http_get_with_retry(
    url: &str,
    user_agent: Option<&str>,
) -> Result<ureq::Response, WebError> {
    let mut attempts = 0;
    let max_attempts = 4;
    let mut delay_ms = 500;

    loop {
        attempts += 1;
        match web_get(url, user_agent) {
            Ok(resp) => return Ok(resp),
            Err(WebError::Http(e))
                if matches!(*e, ureq::Error::Status(code, _)
                    if code == 429 || (500..=504).contains(&code))
                    && attempts < max_attempts =>
            {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                delay_ms *= 2;
                continue;
            }
            Err(WebError::Http(e))
                if matches!(*e, ureq::Error::Transport(_)) && attempts < max_attempts =>
            {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                delay_ms *= 2;
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}
