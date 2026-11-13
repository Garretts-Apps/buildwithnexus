// The HTTP client bwn's callers see, over ureq 3. Its surface is the small
// part of ureq 2's they used (set, timeout, call, send_json, an Err for 4xx
// and 5xx carrying the response, transport errors with a kind), so call
// sites read as they did; what ureq 3 does differently is settled here:
//
// - Status codes: the agents never treat a status as an error
//   (http_status_as_error(false)); `Request::send` turns 400 and up into
//   `Error::Status` with the response, as ureq 2 did, and returns 3xx as Ok
//   when redirects are off.
// - TLS: ureq is built without a TLS feature. `Tls` below wraps the socket in
//   rustls with net.rs's ClientConfig (bundled webpki roots plus the OS store
//   or SSL_CERT_FILE/SSL_CERT_DIR), which ureq's own TlsConfig cannot hold:
//   its root list takes certificates, and webpki-roots ships trust anchors.
// - Proxies: ureq 3 tunnels every request with CONNECT. `Forward` keeps plain
//   http:// in absolute form (`GET http://host/path`) as ureq 2 and curl send
//   it, since proxies commonly refuse CONNECT to port 80.
// - Resolver: a `Resolver` sees `host:port` as in ureq 2; the addresses it
//   returns are the ones connected to, so a check there has no rebinding
//   window. Through a proxy it resolves the proxy, never the target.
// - Timeouts: `timeout` is the whole request (timeout_global),
//   `timeout_read` each socket read (timeout_per_read), `timeout_connect` the
//   connection setup; defaults as ureq 2's (30 s connect, 5 redirects).
// - Pool: ureq 2 kept an idle connection until the server closed it; ureq 3
//   drops it after 15 s, which would lose provider::prewarm's connection
//   while the user types. Five minutes; a connection the server closed is
//   still noticed before reuse.
// - Bodies: `into_string` keeps ureq 2's 10 MB cap and strict UTF-8,
//   `into_reader` and `into_json` are unlimited, as before; callers cap what
//   they read.
//
// Connectors and resolvers live in `ureq::unversioned`, which may change in
// a minor release, so Cargo.toml pins ureq to ~3.5.

use std::fmt;
use std::io::{self, Read};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs};
use ureq::unversioned::transport::{
    Buffers, ConnectProxyConnector, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout,
    TcpConnector, Transport as Conn, TransportAdapter,
};

/// Resolves `host:port` to the addresses a request connects to.
pub trait Resolver: Send + Sync + 'static {
    fn resolve(&self, netloc: &str) -> io::Result<Vec<SocketAddr>>;
}

impl<F> Resolver for F
where
    F: Fn(&str) -> io::Result<Vec<SocketAddr>> + Send + Sync + 'static,
{
    fn resolve(&self, netloc: &str) -> io::Result<Vec<SocketAddr>> {
        self(netloc)
    }
}

// Field names are the ones ureq 2's agent printed, which a test reads.
#[derive(Clone, Debug)]
struct Settings {
    timeout: Option<Duration>,
    timeout_read: Option<Duration>,
    timeout_connect: Option<Duration>,
    redirects: u32,
}

/// What `Client::new` configures on every agent it builds.
pub struct Builder {
    settings: Settings,
    resolver: Option<Arc<dyn Resolver>>,
}

impl Builder {
    pub(super) fn new() -> Builder {
        Builder {
            settings: Settings {
                timeout: None,
                timeout_read: None,
                timeout_connect: Some(Duration::from_secs(30)),
                redirects: 5,
            },
            resolver: None,
        }
    }

    /// The whole request: connecting, sending, and reading the body.
    pub fn timeout(mut self, d: Duration) -> Builder {
        self.settings.timeout = Some(d);
        self
    }

    /// Each read from the socket; a stream that keeps sending never trips it.
    pub fn timeout_read(mut self, d: Duration) -> Builder {
        self.settings.timeout_read = Some(d);
        self
    }

    pub fn timeout_connect(mut self, d: Duration) -> Builder {
        self.settings.timeout_connect = Some(d);
        self
    }

    /// Redirects to follow; 0 returns the 3xx itself.
    pub fn redirects(mut self, n: u32) -> Builder {
        self.settings.redirects = n;
        self
    }

    pub fn resolver(mut self, r: impl Resolver) -> Builder {
        self.resolver = Some(Arc::new(r));
        self
    }
}

pub(super) enum Route {
    Direct,
    Proxy(ureq::Proxy),
    // Every request fails with this reason instead of connecting direct.
    Unusable(String),
}

#[derive(Clone)]
pub struct Agent {
    inner: ureq::Agent,
    settings: Settings,
}

impl fmt::Debug for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Agent")
            .field("config", &self.settings)
            .finish()
    }
}

impl Agent {
    pub(super) fn build(b: &Builder, route: Route) -> Agent {
        let s = &b.settings;
        let config = |proxy: Option<ureq::Proxy>| {
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .proxy(proxy)
                .max_redirects(s.redirects)
                .timeout_global(s.timeout)
                .timeout_connect(s.timeout_connect)
                .timeout_per_read(s.timeout_read)
                .max_idle_age(Duration::from_secs(300))
                .build()
        };
        let (proxy, unusable) = match route {
            Route::Direct => (None, None),
            Route::Proxy(p) => (Some(p), None),
            Route::Unusable(why) => (None, Some(why)),
        };
        let resolver = Resolve {
            custom: b.resolver.clone(),
            unusable,
        };
        let connector = ()
            .chain(Forward {
                plain: config(None),
            })
            .chain(ConnectProxyConnector::default())
            .chain(Dial(TcpConnector::default()))
            .chain(Tls);
        Agent {
            inner: ureq::Agent::with_parts(config(proxy), connector, resolver),
            settings: s.clone(),
        }
    }

    pub fn get(&self, url: &str) -> Request {
        Request::new(self.clone(), false, url)
    }

    pub fn post(&self, url: &str) -> Request {
        Request::new(self.clone(), true, url)
    }
}

// ── requests ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Request {
    agent: Agent,
    post: bool,
    url: String,
    headers: Vec<(String, String)>,
    timeout: Option<Duration>,
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Header values carry keys; names are enough.
        let names: Vec<&str> = self.headers.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("Request")
            .field("method", &if self.post { "POST" } else { "GET" })
            .field("url", &self.url)
            .field("headers", &names)
            .finish()
    }
}

impl Request {
    fn new(agent: Agent, post: bool, url: &str) -> Request {
        Request {
            agent,
            post,
            url: url.to_string(),
            headers: Vec::new(),
            timeout: None,
        }
    }

    /// Sets a header, replacing one of the same name.
    pub fn set(mut self, name: &str, value: &str) -> Request {
        self.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// The whole request, overriding the agent's `timeout`.
    pub fn timeout(mut self, d: Duration) -> Request {
        self.timeout = Some(d);
        self
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Whether a header of that name is set.
    pub fn has(&self, name: &str) -> bool {
        self.headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(name))
    }

    pub fn call(self) -> Result<Response, Error> {
        self.send(None)
    }

    pub fn send_json(self, data: impl serde::Serialize) -> Result<Response, Error> {
        match serde_json::to_vec(&data) {
            Ok(body) => self.send_json_bytes(&body),
            Err(e) => Err(self.invalid(format!("serializing the JSON body: {e}"))),
        }
    }

    /// A body that is already serialised JSON, sent as it is: a caller that
    /// retries keeps the bytes and sends them again.
    pub fn send_json_bytes(self, body: &[u8]) -> Result<Response, Error> {
        self.typed("application/json").send(Some(body))
    }

    pub fn send_string(self, data: &str) -> Result<Response, Error> {
        self.typed("text/plain; charset=utf-8")
            .send(Some(data.as_bytes()))
    }

    pub fn send_form(self, pairs: &[(&str, &str)]) -> Result<Response, Error> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish();
        self.typed("application/x-www-form-urlencoded")
            .send(Some(body.as_bytes()))
    }

    fn typed(self, content_type: &str) -> Request {
        if self.has("content-type") {
            self
        } else {
            self.set("Content-Type", content_type)
        }
    }

    fn invalid(&self, why: String) -> Error {
        Error::Transport(Transport::new(
            ErrorKind::InvalidUrl,
            &self.url,
            Some(why),
            None,
        ))
    }

    fn send(self, body: Option<&[u8]>) -> Result<Response, Error> {
        // As ureq 2 did: the URL parser's spelling (spaces and non-ASCII
        // percent-encoded, IDNA host names), without the fragment.
        let target = match url::Url::parse(&self.url) {
            Ok(mut u) => {
                u.set_fragment(None);
                u.to_string()
            }
            Err(e) => return Err(self.invalid(format!("failed to parse URL: {e:?}"))),
        };
        let inner = &self.agent.inner;
        let result = if self.post {
            let req = prepare(inner.post(&target), &self.headers, self.timeout);
            match body {
                Some(b) => req.send(b),
                None => req.send_empty(),
            }
        } else {
            prepare(inner.get(&target), &self.headers, self.timeout).call()
        };
        match result {
            Ok(resp) => {
                let resp = Response::from_http(resp);
                match resp.status() {
                    code @ 400.. => Err(Error::Status(code, resp)),
                    _ => Ok(resp),
                }
            }
            Err(e) => Err(Error::Transport(classify(&self.url, e))),
        }
    }
}

fn prepare<B>(
    mut req: ureq::RequestBuilder<B>,
    headers: &[(String, String)],
    timeout: Option<Duration>,
) -> ureq::RequestBuilder<B> {
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    match timeout {
        Some(t) => req.config().timeout_global(Some(t)).build(),
        None => req,
    }
}

// ── responses ──────────────────────────────────────────────────────────────

// ureq 2's into_string limit.
const INTO_STRING_LIMIT: usize = 10 * 1024 * 1024;

// Boxed: an Err carries one, and Results should stay small.
pub struct Response(Box<Parts>);

struct Parts {
    url: String,
    status: u16,
    headers: ureq::http::HeaderMap,
    body: ureq::Body,
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Response")
            .field("url", &self.0.url)
            .field("status", &self.0.status)
            .finish()
    }
}

impl Response {
    fn from_http(resp: ureq::http::Response<ureq::Body>) -> Response {
        use ureq::ResponseExt;
        let url = resp.get_uri().to_string();
        let (parts, body) = resp.into_parts();
        Response(Box::new(Parts {
            url,
            status: parts.status.as_u16(),
            headers: parts.headers,
            body,
        }))
    }

    /// The URL that answered, after any redirects.
    pub fn get_url(&self) -> &str {
        &self.0.url
    }

    pub fn status(&self) -> u16 {
        self.0.status
    }

    /// The first value of a header, when it is text.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.0.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// Every value of a header that is text.
    pub fn all(&self, name: &str) -> Vec<&str> {
        self.0
            .headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect()
    }

    /// The body, decompressed, unlimited.
    pub fn into_reader(self) -> Box<dyn Read + Send + Sync + 'static> {
        Box::new(BodyRead(self.0.body.into_reader()))
    }

    /// The body as UTF-8 text, at most 10 MB.
    pub fn into_string(self) -> io::Result<String> {
        let mut buf = Vec::new();
        self.into_reader()
            .take(INTO_STRING_LIMIT as u64 + 1)
            .read_to_end(&mut buf)?;
        if buf.len() > INTO_STRING_LIMIT {
            return Err(io::Error::other("response too big for into_string"));
        }
        String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn into_json<T: serde::de::DeserializeOwned>(self) -> io::Result<T> {
        serde_json::from_reader(self.into_reader()).map_err(|e| match e.io_error_kind() {
            Some(kind) => io::Error::new(kind, e),
            None => io::Error::new(io::ErrorKind::InvalidData, e),
        })
    }
}

// A body read error as ureq 2 reported it: a timeout is TimedOut, and other
// ureq errors are plain io errors with their text.
struct BodyRead(ureq::BodyReader<'static>);

impl Read for BodyRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf).map_err(plain_io)
    }
}

fn plain_io(e: io::Error) -> io::Error {
    if !e.get_ref().is_some_and(|x| x.is::<ureq::Error>()) {
        return e;
    }
    let kind = e.kind();
    match e.into_inner().map(|x| x.downcast::<ureq::Error>()) {
        Some(Ok(u)) => match *u {
            ureq::Error::Timeout(_) => timed_out("timed out reading response"),
            ureq::Error::Io(io) => plain_io(io),
            other => io::Error::new(kind, other.to_string()),
        },
        Some(Err(other)) => io::Error::new(kind, other),
        None => io::Error::from(kind),
    }
}

fn timed_out(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, msg.to_string())
}

// ── errors ─────────────────────────────────────────────────────────────────

/// ureq 2's error shape: a status of 400 or more with its response, or a
/// failure to get one.
#[derive(Debug)]
pub enum Error {
    Status(u16, Response),
    Transport(Transport),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Status(code, resp) => write!(f, "{}: status code {code}", resp.get_url()),
            Error::Transport(t) => t.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Status(..) => None,
            Error::Transport(t) => std::error::Error::source(t),
        }
    }
}

/// What kind of failure kept a response from arriving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidUrl,
    Dns,
    ConnectionFailed,
    TooManyRedirects,
    Io,
    InvalidProxyUrl,
    ProxyConnect,
    ProxyUnauthorized,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ErrorKind::InvalidUrl => "Bad URL",
            ErrorKind::Dns => "Dns Failed",
            ErrorKind::ConnectionFailed => "Connection Failed",
            ErrorKind::TooManyRedirects => "Too Many Redirects",
            ErrorKind::Io => "Network Error",
            ErrorKind::InvalidProxyUrl => "Malformed proxy",
            ErrorKind::ProxyConnect => "Proxy failed to connect",
            ErrorKind::ProxyUnauthorized => "Provided proxy credentials are incorrect",
        })
    }
}

#[derive(Debug)]
pub struct Transport {
    kind: ErrorKind,
    url: String,
    message: Option<String>,
    source: Option<io::Error>,
}

impl Transport {
    fn new(kind: ErrorKind, url: &str, message: Option<String>, source: Option<io::Error>) -> Self {
        Transport {
            kind,
            url: url.to_string(),
            message,
            source,
        }
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

// `url: Kind: message: source`, as ureq 2 wrote it.
impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.url, self.kind)?;
        if let Some(m) = &self.message {
            write!(f, ": {m}")?;
        }
        if let Some(s) = &self.source {
            write!(f, ": {s}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Transport {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

// A failure the connectors and resolver below already classified, carried
// through ureq as Error::Other.
#[derive(Debug)]
struct Tagged {
    kind: ErrorKind,
    message: Option<String>,
    source: Option<io::Error>,
}

impl fmt::Display for Tagged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.kind)?;
        if let Some(m) = &self.message {
            write!(f, ": {m}")?;
        }
        if let Some(s) = &self.source {
            write!(f, ": {s}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Tagged {}

fn tag(kind: ErrorKind, message: impl Into<String>, source: io::Error) -> ureq::Error {
    ureq::Error::Other(Box::new(Tagged {
        kind,
        message: Some(message.into()),
        source: Some(source),
    }))
}

// An io error that may carry a ureq error inside, as one io error.
fn io_of(e: ureq::Error) -> io::Error {
    match e {
        ureq::Error::Io(io) => plain_io(io),
        ureq::Error::Timeout(_) => timed_out("timed out"),
        other => io::Error::other(other.to_string()),
    }
}

fn classify(url: &str, e: ureq::Error) -> Transport {
    use ureq::Error as U;
    let t = |kind, message: Option<String>, source| Transport::new(kind, url, message, source);
    match e {
        U::Other(b) => match b.downcast::<Tagged>() {
            Ok(tagged) => t(tagged.kind, tagged.message, tagged.source),
            Err(other) => t(ErrorKind::Io, None, Some(io::Error::other(other))),
        },
        U::Io(io) => {
            let io = plain_io(io);
            // A connector error that crossed an io boundary.
            match io.get_ref().and_then(|x| x.downcast_ref::<Tagged>()) {
                Some(_) => {
                    let tagged = io.into_inner().and_then(|x| x.downcast::<Tagged>().ok());
                    let tagged = tagged.expect("checked above");
                    t(tagged.kind, tagged.message, tagged.source)
                }
                None => t(ErrorKind::Io, None, Some(io)),
            }
        }
        U::Timeout(ureq::Timeout::Connect) => t(
            ErrorKind::ConnectionFailed,
            Some("Connect error".into()),
            Some(timed_out("connection timed out")),
        ),
        U::Timeout(ureq::Timeout::Resolve) => t(
            ErrorKind::Dns,
            Some(format!("resolve dns name for {url}")),
            Some(timed_out("timed out")),
        ),
        U::Timeout(_) => t(
            ErrorKind::Io,
            None,
            Some(timed_out("timed out reading response")),
        ),
        U::HostNotFound => t(
            ErrorKind::Dns,
            Some(format!("No ip address for {url}")),
            None,
        ),
        U::ConnectProxyFailed(why) => {
            let kind = if why.contains(" 407/") || why.contains(" 401/") {
                ErrorKind::ProxyUnauthorized
            } else {
                ErrorKind::ProxyConnect
            };
            t(kind, Some(why), None)
        }
        U::ConnectionFailed => t(ErrorKind::ConnectionFailed, None, None),
        U::BadUri(why) => t(ErrorKind::InvalidUrl, Some(why), None),
        U::Http(e) => t(ErrorKind::InvalidUrl, Some(e.to_string()), None),
        U::TooManyRedirects => t(ErrorKind::TooManyRedirects, None, None),
        U::InvalidProxyUrl => t(ErrorKind::InvalidProxyUrl, None, None),
        other => t(ErrorKind::Io, Some(other.to_string()), None),
    }
}

// ── resolver ───────────────────────────────────────────────────────────────

struct Resolve {
    custom: Option<Arc<dyn Resolver>>,
    unusable: Option<String>,
}

impl fmt::Debug for Resolve {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolve")
            .field("custom", &self.custom.is_some())
            .finish()
    }
}

impl ureq::unversioned::resolver::Resolver for Resolve {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let netloc = match (uri.scheme(), uri.authority()) {
            (Some(s), Some(a)) => DefaultResolver::host_and_port(s, a),
            _ => None,
        }
        .ok_or_else(|| ureq::Error::BadUri(format!("{uri} has no host or known port")))?;
        let lookup_failed =
            |e: io::Error| tag(ErrorKind::Dns, format!("resolve dns name '{netloc}'"), e);
        if let Some(why) = &self.unusable {
            return Err(lookup_failed(io::Error::other(why.clone())));
        }
        let Some(custom) = &self.custom else {
            return DefaultResolver::default()
                .resolve(uri, config, timeout)
                .map_err(|e| match e {
                    ureq::Error::Io(io) => lookup_failed(io),
                    ureq::Error::HostNotFound => tag(
                        ErrorKind::Dns,
                        format!("No ip address for {netloc}"),
                        io::Error::from(io::ErrorKind::NotFound),
                    ),
                    other => other,
                });
        };
        let addrs = custom.resolve(&netloc).map_err(lookup_failed)?;
        let mut out = self.empty();
        for a in addrs.into_iter().take(16) {
            out.push(a);
        }
        if out.is_empty() {
            return Err(tag(
                ErrorKind::Dns,
                format!("No ip address for {netloc}"),
                io::Error::from(io::ErrorKind::NotFound),
            ));
        }
        Ok(out)
    }
}

// ── connectors ─────────────────────────────────────────────────────────────

// TCP, with a failure to connect told apart from a failure after it, as
// ureq 2's ConnectionFailed was: only the first is safe to retry.
#[derive(Debug)]
struct Dial(TcpConnector);

impl<In: Conn> Connector<In> for Dial {
    type Out = <TcpConnector as Connector<In>>::Out;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        let dialing = chained.is_none();
        self.0.connect(details, chained).map_err(|e| match e {
            ureq::Error::Io(io) if dialing => tag(ErrorKind::ConnectionFailed, "Connect error", io),
            ureq::Error::Timeout(_) if dialing => tag(
                ErrorKind::ConnectionFailed,
                "Connect error",
                timed_out("connection timed out"),
            ),
            other => other,
        })
    }
}

// Plain http:// through an HTTP proxy: connect to the proxy and send the
// request in absolute form. https:// falls through to ureq's CONNECT.
#[derive(Debug)]
struct Forward {
    // The agent's config without its proxy, for the hop to the proxy itself.
    plain: ureq::config::Config,
}

impl<In: Conn> Connector<In> for Forward {
    type Out = Either<In, AbsoluteForm>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        if let Some(t) = chained {
            return Ok(Some(Either::A(t)));
        }
        let Some(proxy) = details.config.proxy() else {
            return Ok(None);
        };
        if details.needs_tls() || proxy.protocol() != ureq::ProxyProtocol::Http {
            return Ok(None);
        }
        let proxy_uri = proxy.uri();
        let hop = ConnectionDetails {
            uri: proxy_uri,
            addrs: details
                .resolver
                .resolve(proxy_uri, details.config, details.timeout)?,
            config: &self.plain,
            request_level: details.request_level,
            resolver: details.resolver,
            now: details.now,
            timeout: details.timeout,
            current_time: details.current_time.clone(),
            run_connector: details.run_connector.clone(),
            register_shutdown: details.register_shutdown.clone(),
        };
        let inner = (details.run_connector)(&hop)?;
        let target = details.uri;
        let host = target.host().unwrap_or_default();
        let origin = match target.port_u16() {
            Some(port) => format!("http://{host}:{port}"),
            None => format!("http://{host}"),
        };
        let auth = (proxy.username().is_some() || proxy.password().is_some()).then(|| {
            let creds = format!(
                "{}:{}",
                proxy.username().unwrap_or_default(),
                proxy.password().unwrap_or_default()
            );
            format!(
                "Proxy-Authorization: Basic {}\r\n",
                base64(creds.as_bytes())
            )
        });
        Ok(Some(Either::B(AbsoluteForm {
            inner,
            origin,
            auth,
            fresh: true,
        })))
    }
}

// Rewrites the request line of each request on the connection from
// `GET /path` to `GET http://host/path` and adds the proxy's credentials.
// A request starts with the first write after the previous response was
// read (bodies follow their headers without a read in between).
#[derive(Debug)]
struct AbsoluteForm {
    inner: Box<dyn Conn>,
    origin: String,
    auth: Option<String>,
    fresh: bool,
}

impl AbsoluteForm {
    fn rewrite(&mut self, amount: usize) -> Result<usize, ureq::Error> {
        let buf = self.inner.buffers().output();
        let head = &buf[..amount];
        let Some(sp) = head.iter().position(|&b| b == b' ') else {
            return Ok(amount);
        };
        if head.get(sp + 1) != Some(&b'/') {
            return Ok(amount);
        }
        // Credentials go right after the request line, once it is whole.
        let eol = head.windows(2).position(|w| w == b"\r\n").map(|i| i + 2);
        let line_end = eol.unwrap_or(amount);
        let mut out = Vec::with_capacity(amount + self.origin.len() + 128);
        out.extend_from_slice(&head[..=sp]);
        out.extend_from_slice(self.origin.as_bytes());
        out.extend_from_slice(&head[sp + 1..line_end]);
        if let Some(a) = self.auth.as_ref().filter(|_| eol.is_some()) {
            out.extend_from_slice(a.as_bytes());
        }
        out.extend_from_slice(&head[line_end..]);
        if out.len() > buf.len() {
            return Err(ureq::Error::Other(
                "request headers too large to send through the proxy".into(),
            ));
        }
        buf[..out.len()].copy_from_slice(&out);
        Ok(out.len())
    }
}

impl Conn for AbsoluteForm {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        let amount = if std::mem::take(&mut self.fresh) {
            self.rewrite(amount)?
        } else {
            amount
        };
        self.inner.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.fresh = true;
        self.inner.await_input(timeout)
    }

    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }
}

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

// rustls with net.rs's client config, on whatever the chain connected:
// the server, or a CONNECT tunnel through the proxy.
#[derive(Debug)]
struct Tls;

impl<In: Conn> Connector<In> for Tls {
    type Out = Either<In, TlsTransport>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        let Some(transport) = chained else {
            return Ok(None);
        };
        if !details.needs_tls() || transport.is_tls() {
            return Ok(Some(Either::A(transport)));
        }
        let host = details.uri.host().unwrap_or_default();
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| {
            tag(
                ErrorKind::Dns,
                format!("parsing '{host}'"),
                io::Error::new(io::ErrorKind::InvalidInput, e),
            )
        })?;
        let mut conn = rustls::ClientConnection::new(super::tls_config(), name).map_err(|e| {
            tag(
                ErrorKind::Io,
                "tls connection creation failed",
                io::Error::other(e),
            )
        })?;
        let mut sock = TransportAdapter::new(transport.boxed(), details.current_time.clone());
        sock.set_timeout(details.timeout);
        conn.complete_io(&mut sock).map_err(|e| {
            let source = match ureq::Error::from(e) {
                ureq::Error::Io(io) => plain_io(io),
                ureq::Error::Timeout(_) => timed_out("timed out during the TLS handshake"),
                ureq::Error::Other(b) if b.is::<Tagged>() => return ureq::Error::Other(b),
                other => io_of(other),
            };
            tag(
                ErrorKind::ConnectionFailed,
                "tls connection init failed",
                source,
            )
        })?;
        Ok(Some(Either::B(TlsTransport {
            buffers: LazyBuffers::new(
                details.config.input_buffer_size(),
                details.config.output_buffer_size(),
            ),
            stream: rustls::StreamOwned::new(conn, sock),
        })))
    }
}

struct TlsTransport {
    buffers: LazyBuffers,
    stream: rustls::StreamOwned<rustls::ClientConnection, TransportAdapter>,
}

impl fmt::Debug for TlsTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsTransport").finish()
    }
}

impl Conn for TlsTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        use std::io::Write;
        self.stream.get_mut().set_timeout(timeout);
        let output = &self.buffers.output()[..amount];
        self.stream.write_all(output)?;
        Ok(())
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        let input = self.buffers.input_append_buf();
        let amount = self.stream.read(input)?;
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }

    fn is_open(&mut self) -> bool {
        self.stream.get_mut().get_mut().is_open()
    }

    fn is_tls(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), want, "{input}");
        }
    }
}
