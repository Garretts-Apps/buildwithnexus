//! The posture check: before a session's first model request, what the
//! session can reach and what this machine's network does, for the user to
//! accept or stop on.
//!
//! Two parts:
//! - the session's own reach: the permission mode, the sandbox and what it
//!   confines (writes, credential files, the network of commands), MCP
//!   servers, project settings;
//! - probes from bwn itself (which hooks, MCP servers and unsandboxed
//!   commands share): the model endpoint, name resolution, the cloud
//!   metadata address, and the ports listening on this machine. Each is
//!   open, blocked, not responding, or undetermined. Nothing is sent to a
//!   host the session would not contact anyway.
//!
//! The answer belongs to the session: `continue` or `resume` of a session
//! that accepted this same posture is not asked again, a changed posture
//! is, and every new session is. Headless, `--accept-posture` (or
//! BWN_ACCEPT_POSTURE=1) is the answer; without it the run stops before any
//! model request and says how to accept.

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// As it should be.
    Good,
    /// Worth a look before going on.
    Warn,
    /// Could not be told from here.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub section: &'static str,
    pub item: String,
    pub value: String,
    pub level: Level,
    /// The part of the row that decides whether the posture changed (the
    /// value may carry details that vary between runs).
    pub key: String,
}

fn row(section: &'static str, item: &str, value: String, level: Level, key: &str) -> Row {
    Row {
        section,
        item: item.to_string(),
        value,
        level,
        key: key.to_string(),
    }
}

static ACCEPT_FLAG: AtomicBool = AtomicBool::new(false);
static ACCEPTED: Mutex<Option<String>> = Mutex::new(None);

/// `--accept-posture`: the headless answer.
pub fn accept_by_flag() {
    ACCEPT_FLAG.store(true, Ordering::Relaxed);
}

fn accepted_by_flag() -> bool {
    ACCEPT_FLAG.load(Ordering::Relaxed)
        || std::env::var("BWN_ACCEPT_POSTURE").is_ok_and(|v| !v.is_empty() && v != "0")
}

// ── what the session can reach ────────────────────────────────────────────

fn tilde(p: &Path) -> String {
    match crate::config::user_home() {
        Some(h) if p.starts_with(&h) => format!("~/{}", p.strip_prefix(&h).unwrap().display()),
        _ => p.display().to_string(),
    }
}

fn session_rows(perm: crate::agent::Permission, cwd: &Path) -> Vec<Row> {
    use crate::agent::Permission as P;
    use crate::sandbox::{Decision, Mode, Net};
    let mut v = Vec::new();
    let mode = crate::agent::permission_name(perm);
    let (desc, level) = match perm {
        P::Auto => (
            "runs commands, edits and fetches without asking; asks again once it reads web or MCP content",
            Level::Warn,
        ),
        P::AcceptEdits => ("edits files without asking; asks before commands and fetches", Level::Good),
        P::Ask => ("asks before commands, edits and fetches", Level::Good),
        P::ReadOnly => ("reads only; changes are refused", Level::Good),
    };
    v.push(row(
        "session",
        "mode",
        format!("{mode}: {desc}"),
        level,
        mode,
    ));

    let smode = crate::sandbox::mode();
    let decision = crate::sandbox::decide(smode, crate::sandbox::backend());
    let backend = match decision {
        Decision::Sandboxed(b) => Some(b),
        _ => None,
    };
    let (sb, sl) = match (smode, backend) {
        (Mode::Off, _) => (
            "off (your setting): commands run unsandboxed".to_string(),
            Level::Warn,
        ),
        (_, Some(b)) => (b.label().to_string(), Level::Good),
        (_, None) => (
            format!(
                "none usable ({}): commands run unsandboxed",
                crate::sandbox::backend_error().unwrap_or("no backend")
            ),
            Level::Warn,
        ),
    };
    let skey = backend.map_or(smode.as_str(), |b| b.label());
    v.push(row("session", "sandbox", sb, sl, skey));

    if let Some(backend) = backend {
        v.push(row(
            "session",
            "writes",
            "only the workspace and temp folders".into(),
            Level::Good,
            "confined",
        ));
        let hidden = crate::sandbox::hidden_paths(&[cwd]);
        let (cv, cl, ck) = if crate::sandbox::hides_reads(cwd) {
            let names: Vec<String> = hidden.iter().map(|p| tilde(p)).collect();
            let shown = if names.is_empty() {
                "none found to hide".to_string()
            } else {
                format!("hidden from commands: {}", names.join(", "))
            };
            (shown, Level::Good, "hidden")
        } else {
            (
                "readable by commands (the workspace holds a credential store, which this sandbox cannot carve out)".into(),
                Level::Warn,
                "readable",
            )
        };
        v.push(row("session", "credential files", cv, cl, ck));
        let net = crate::sandbox::net_for(backend);
        let allow = crate::egress::Rules::load(cwd).allow.len();
        let (nv, nl, nk) = match net {
            Net::Off if !crate::sandbox::network() => {
                ("none (sandbox_network: false)".to_string(), Level::Good, "off")
            }
            Net::Off => (
                "none: this machine cannot hold commands to the checker (Landlock ABI 4 needed); set sandbox_egress_check: false to let them connect anywhere".to_string(),
                Level::Unknown,
                "off-unsupported",
            ),
            Net::Proxy(_) => (
                format!(
                    "only through the checker: {} package and source hosts, hosts you name, {allow} in network.allow; this machine's own servers on any port",
                    crate::egress::KNOWN_HOSTS.len()
                ),
                Level::Good,
                "checked",
            ),
            Net::Open => (
                "any host (sandbox_egress_check: false)".to_string(),
                Level::Warn,
                "open",
            ),
        };
        v.push(row("session", "command network", nv, nl, nk));
    } else {
        v.push(row(
            "session",
            "commands",
            if perm == P::Auto && smode != Mode::Off {
                "can write and read anywhere you can and reach any host; auto mode asks before each one".into()
            } else {
                "can write and read anywhere you can, and reach any host".into()
            },
            Level::Warn,
            "unconfined",
        ));
    }

    let settings = crate::config::load_settings();
    // Servers that are switched on (`"enabled": false` ones never start).
    let mcp: Vec<String> = settings
        .as_ref()
        .map(|s| {
            s.mcp_servers
                .iter()
                .filter(|(_, v)| v.get("enabled").and_then(|e| e.as_bool()) != Some(false))
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default();
    v.push(if mcp.is_empty() {
        row("session", "MCP servers", "none".into(), Level::Good, "none")
    } else {
        row(
            "session",
            "MCP servers",
            format!("{} (they run outside the sandbox)", mcp.join(", ")),
            Level::Warn,
            &mcp.join(","),
        )
    });
    let untrusted = crate::config::untrusted_project_files(cwd);
    let project = cwd.join(".buildwithnexus");
    v.push(if !untrusted.is_empty() {
        row(
            "session",
            "project settings",
            "present but not trusted: their hooks, servers and allow rules are ignored".into(),
            Level::Good,
            "untrusted",
        )
    } else if project.is_dir() {
        row(
            "session",
            "project settings",
            "trusted: their hooks and MCP servers run".into(),
            Level::Warn,
            "trusted",
        )
    } else {
        row(
            "session",
            "project settings",
            "none".into(),
            Level::Good,
            "none",
        )
    });
    v
}

// ── what this machine's network does ──────────────────────────────────────

/// What a TCP connect found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    Open,
    /// Refused: something answered that the port is closed, or a firewall
    /// rejected it.
    Refused,
    /// Nothing answered in time: dropped by a firewall, or nothing there.
    NoResponse,
    /// The name did not resolve.
    Unresolved,
}

pub fn probe(host: &str, port: u16, wait: Duration) -> Probe {
    let addrs: Vec<SocketAddr> = match (host, port).to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(_) => return Probe::Unresolved,
    };
    let Some(addr) = addrs.first() else {
        return Probe::Unresolved;
    };
    match TcpStream::connect_timeout(addr, wait) {
        Ok(_) => Probe::Open,
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Probe::NoResponse,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Probe::NoResponse,
        Err(_) => Probe::Refused,
    }
}

fn probe_word(p: Probe) -> &'static str {
    match p {
        Probe::Open => "open",
        Probe::Refused => "blocked (refused)",
        Probe::NoResponse => "not responding",
        Probe::Unresolved => "undetermined (the name does not resolve)",
    }
}

/// The ports listening on this machine, from /proc/net/tcp and tcp6, as
/// `address:port` (Linux). None elsewhere.
pub fn listening() -> Option<Vec<String>> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let mut out = Vec::new();
    for (file, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        out.extend(text.lines().skip(1).filter_map(|l| parse_listen(l, v6)));
    }
    out.sort();
    out.dedup();
    Some(out)
}

// One /proc/net/tcp line: `sl local_address rem_address st …`, addresses in
// hex, little-endian words; state 0A is LISTEN.
fn parse_listen(line: &str, v6: bool) -> Option<String> {
    let mut f = line.split_whitespace();
    let local = f.nth(1)?;
    let state = f.nth(1)?;
    if state != "0A" {
        return None;
    }
    let (addr, port) = local.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let ip = if v6 {
        let mut bytes = [0u8; 16];
        for (w, chunk) in addr.as_bytes().chunks(8).enumerate().take(4) {
            let word = u32::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
            bytes[w * 4..w * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        let ip = std::net::Ipv6Addr::from(bytes);
        match ip.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => format!("[{ip}]"),
        }
    } else {
        let word = u32::from_str_radix(addr, 16).ok()?;
        std::net::Ipv4Addr::from(word.to_le_bytes()).to_string()
    };
    Some(format!("{ip}:{port}"))
}

fn network_rows(endpoint: Option<(String, u16)>) -> Vec<Row> {
    let wait = Duration::from_millis(1500);
    let mut v = Vec::new();
    let upstream = endpoint.as_ref().and_then(|(h, p)| {
        let scheme = if *p == 443 { "https" } else { "http" };
        crate::net::shared().upstream_for(&format!("{scheme}://{h}:{p}/"))
    });
    // The endpoint and the metadata address are probed side by side.
    let ep = endpoint.clone();
    let up = upstream.clone();
    let model = std::thread::spawn(move || {
        let (h, p) = ep?;
        Some(match &up {
            Some((proxy, _)) => {
                let (ph, pp) = proxy.rsplit_once(':')?;
                (
                    format!("{h}:{p} via proxy {proxy}"),
                    probe(ph.trim_matches(['[', ']']), pp.parse().ok()?, wait),
                )
            }
            None => (format!("{h}:{p}"), probe(&h, p, wait)),
        })
    });
    let metadata = std::thread::spawn(move || probe("169.254.169.254", 80, wait));
    if let Ok(Some((label, p))) = model.join() {
        let level = if p == Probe::Open {
            Level::Good
        } else {
            Level::Warn
        };
        v.push(row(
            "network",
            &format!("model endpoint {label}"),
            probe_word(p).into(),
            level,
            probe_word(p),
        ));
    }
    if let Some((h, _)) = &endpoint {
        let resolves = upstream.is_some() || (h.as_str(), 0).to_socket_addrs().is_ok();
        v.push(row(
            "network",
            &format!("name lookup ({h})"),
            if upstream.is_some() {
                "left to the proxy".into()
            } else if resolves {
                "resolves".into()
            } else {
                "fails".into()
            },
            if resolves { Level::Good } else { Level::Warn },
            if resolves { "ok" } else { "fails" },
        ));
    }
    let md = metadata.join().unwrap_or(Probe::NoResponse);
    let (mv, ml) = match md {
        Probe::Open => (
            "open: a process that is not sandboxed (hooks, MCP servers, unsandboxed commands) can ask it for this machine's cloud credentials".to_string(),
            Level::Warn,
        ),
        p => (probe_word(p).to_string(), Level::Good),
    };
    v.push(row(
        "network",
        "cloud metadata 169.254.169.254:80",
        mv,
        ml,
        probe_word(md),
    ));
    match listening() {
        Some(ports) => {
            let exposed: Vec<&String> = ports
                .iter()
                .filter(|a| !a.starts_with("127.") && !a.starts_with("[::1]"))
                .collect();
            let level = if exposed.is_empty() {
                Level::Good
            } else {
                Level::Warn
            };
            let local = ports.len() - exposed.len();
            let local_part = match local {
                0 => String::new(),
                1 => "1 port on this machine only".to_string(),
                n => format!("{n} ports on this machine only"),
            };
            let value = if ports.is_empty() {
                "none".to_string()
            } else if exposed.is_empty() {
                local_part
            } else {
                let mut s = format!(
                    "{} (reachable from other machines)",
                    exposed
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                if local > 0 {
                    s.push_str(&format!("; {local_part}"));
                }
                s
            };
            // Loopback listeners come and go (dev servers); only the ones
            // other machines can reach decide whether the posture changed.
            let key = exposed
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(",");
            v.push(row(
                "network",
                "listening on this machine",
                value,
                level,
                &key,
            ));
        }
        None => v.push(row(
            "network",
            "listening on this machine",
            "undetermined (not read on this system)".into(),
            Level::Unknown,
            "unknown",
        )),
    }
    v
}

fn endpoint_of(base_url: &str) -> Option<(String, u16)> {
    let u = url::Url::parse(base_url).ok()?;
    Some((u.host_str()?.to_string(), u.port_or_known_default()?))
}

/// Every row of the posture, for this session in `cwd`.
pub fn gather(perm: crate::agent::Permission, cwd: &Path, base_url: &str) -> Vec<Row> {
    let mut rows = session_rows(perm, cwd);
    rows.extend(network_rows(endpoint_of(base_url)));
    rows
}

/// What decides whether a later run of this session faces the same posture.
pub fn fingerprint(rows: &[Row]) -> String {
    let text: String = rows
        .iter()
        .map(|r| {
            let item = r.item.split(" via ").next().unwrap_or(&r.item);
            format!("{}\u{1f}{}\u{1f}{}\n", r.section, item, r.key)
        })
        .collect();
    crate::hooks::sha256_hex(text.as_bytes())
}

/// The posture as lines for the terminal or a log.
pub fn render(rows: &[Row]) -> Vec<String> {
    let mut out =
        vec!["  Before the first model request, here is what this session can reach:".to_string()];
    let width = rows
        .iter()
        .map(|r| r.item.chars().count())
        .max()
        .unwrap_or(0)
        .min(44);
    let mut section = "";
    for r in rows {
        if r.section != section {
            section = r.section;
            out.push(match section {
                "session" => "  this session".to_string(),
                _ => "  this machine's network (what bwn, hooks and MCP servers use)".to_string(),
            });
        }
        let mark = match r.level {
            Level::Good => "✓",
            Level::Warn => "!",
            Level::Unknown => "?",
        };
        out.push(format!("    {mark} {:<width$}  {}", r.item, r.value));
    }
    out
}

fn store_path(sid: &str) -> PathBuf {
    let safe: String = sid
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    crate::config::home()
        .join("posture")
        .join(format!("{safe}.txt"))
}

fn stored(sid: &str) -> Option<String> {
    std::fs::read_to_string(store_path(sid))
        .ok()
        .map(|s| s.trim().to_string())
}

fn store(sid: &str, fp: &str) {
    let p = store_path(sid);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, format!("{fp}\n"));
}

/// What the person answered, however they were asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Accept,
    Stop,
    /// Nobody to ask (headless without --accept-posture).
    NoOne,
}

/// Run the check once per session before its first model request. Ok to
/// go on; Err stops the turn with what to do.
pub fn ensure(
    sid: &str,
    perm: crate::agent::Permission,
    cwd: &Path,
    base_url: &str,
) -> Result<(), String> {
    if ACCEPTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_deref()
        == Some(sid)
    {
        return Ok(());
    }
    let rows = gather(perm, cwd, base_url);
    let fp = fingerprint(&rows);
    let lines = render(&rows);
    let accept = |why: &str| {
        store(sid, &fp);
        *ACCEPTED.lock().unwrap_or_else(|e| e.into_inner()) = Some(sid.to_string());
        crate::report::info(&format!("  posture accepted {why}"));
        Ok(())
    };
    let editor = crate::agent::has_remote_approver();
    let show = || {
        // The heading lines, then one per row: warnings stand out. In an
        // editor (ACP) the question itself carries the posture, so the rows
        // stay out of the conversation.
        let mut levels = rows.iter().map(|r| r.level);
        for l in &lines {
            let is_row = l.starts_with("    ");
            match is_row.then(|| levels.next()).flatten() {
                Some(Level::Warn) if !editor => crate::report::notice(l),
                _ => crate::report::info(l),
            }
        }
    };
    show();
    if stored(sid).as_deref() == Some(fp.as_str()) {
        return accept("earlier for this session; unchanged since");
    }
    if accepted_by_flag() {
        return accept("by --accept-posture");
    }
    match ask(&lines) {
        Answer::Accept => accept("for this session"),
        Answer::Stop => Err(
            "stopped at the posture check: change the firewall, network or settings above, then run again"
                .into(),
        ),
        Answer::NoOne => Err(
            "stopped at the posture check: no one to accept it. Read the posture above, then pass --accept-posture (or set BWN_ACCEPT_POSTURE=1) to accept it for this session"
                .into(),
        ),
    }
}

fn ask(lines: &[String]) -> Answer {
    use std::io::IsTerminal;
    if let Some(answer) = crate::agent::ask_remote_posture(&lines.join("\n")) {
        return answer;
    }
    if crate::report::is_json() || !std::io::stdin().is_terminal() {
        return Answer::NoOne;
    }
    match crate::tui::ask("  accept this posture and continue? [y]es / [n]o, stop: ") {
        Some(a) if matches!(a.trim().to_ascii_lowercase().as_str(), "y" | "yes") => Answer::Accept,
        _ => Answer::Stop,
    }
}

/// `ensure`, as each turn calls it. Unit tests that drive turns skip it;
/// the posture's own tests call `ensure`.
pub fn before_turn(
    sid: &str,
    perm: crate::agent::Permission,
    cwd: &Path,
    base_url: &str,
) -> Result<(), String> {
    #[cfg(test)]
    {
        let _ = (sid, perm, cwd, base_url);
        Ok(())
    }
    #[cfg(not(test))]
    ensure(sid, perm, cwd, base_url)
}

/// Before any model request: the check for the current session, unless it
/// was already accepted. The mode is the one the gate runs under.
pub fn before_request(base_url: &str) -> Result<(), String> {
    #[cfg(test)]
    {
        let _ = base_url;
        Ok(())
    }
    #[cfg(not(test))]
    {
        let sid = crate::session::current().unwrap_or_default();
        if ACCEPTED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_deref()
            == Some(sid.as_str())
        {
            return Ok(());
        }
        let perm = crate::agent::parse_permission(&crate::hooks::permission_mode())
            .unwrap_or(crate::agent::Permission::Ask);
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        ensure(&sid, perm, &cwd, base_url)
    }
}

#[cfg(test)]
pub(crate) fn forget_accepted() {
    *ACCEPTED.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_lines_parse_for_ipv4_and_ipv6() {
        assert_eq!(
            parse_listen(
                "   0: 0100007F:0CEA 00000000:0000 0A 00000000:00000000 00:00000000 00000000",
                false
            ),
            Some("127.0.0.1:3306".into())
        );
        assert_eq!(
            parse_listen("   1: 00000000:0016 00000000:0000 0A 00000000", false),
            Some("0.0.0.0:22".into())
        );
        // Established, not listening.
        assert_eq!(
            parse_listen("   2: 0100007F:0CEA 0100007F:D3C4 01 0", false),
            None
        );
        assert_eq!(
            parse_listen("   0: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0000 0A 0", true),
            Some("[::1]:8080".into())
        );
        assert_eq!(
            parse_listen("   0: 0000000000000000FFFF00000100007F:1F90 00000000000000000000000000000000:0000 0A 0", true),
            Some("127.0.0.1:8080".into())
        );
    }

    #[test]
    fn probe_tells_open_from_refused() {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        assert_eq!(
            probe("127.0.0.1", port, Duration::from_secs(1)),
            Probe::Open
        );
        drop(l);
        assert_eq!(
            probe("127.0.0.1", port, Duration::from_secs(1)),
            Probe::Refused
        );
        assert_eq!(
            probe("no-such-host.invalid", 443, Duration::from_secs(1)),
            Probe::Unresolved
        );
    }

    #[test]
    fn the_fingerprint_ignores_loopback_listeners_and_values() {
        let a = vec![
            row(
                "network",
                "listening on this machine",
                "127.0.0.1:3000".into(),
                Level::Good,
                "",
            ),
            row(
                "network",
                "model endpoint h:443 via proxy p:1",
                "open".into(),
                Level::Good,
                "open",
            ),
        ];
        let mut b = a.clone();
        b[0].value = "127.0.0.1:3001".into();
        b[1].item = "model endpoint h:443 via proxy p:2".into();
        assert_eq!(fingerprint(&a), fingerprint(&b));
        b[0].key = "0.0.0.0:22".into();
        assert_ne!(fingerprint(&a), fingerprint(&b));
    }

    // Headless with no flag: the run stops, and says how to accept; with
    // the flag it goes on and the session remembers it.
    #[test]
    fn headless_needs_the_flag_and_the_answer_is_kept_per_session() {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("bwn-posture-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("NEXUS_HOME", &home);
        std::env::remove_var("BWN_ACCEPT_POSTURE");
        forget_accepted();
        let cwd = home.clone();
        let base = "http://127.0.0.1:9/v1";
        let p = crate::agent::Permission::Ask;
        let e = ensure("s1", p, &cwd, base).unwrap_err();
        assert!(e.contains("--accept-posture"), "{e}");
        std::env::set_var("BWN_ACCEPT_POSTURE", "1");
        assert!(ensure("s1", p, &cwd, base).is_ok());
        std::env::remove_var("BWN_ACCEPT_POSTURE");
        // The same session again (a later run): remembered.
        forget_accepted();
        assert!(ensure("s1", p, &cwd, base).is_ok());
        // Another session is asked afresh.
        forget_accepted();
        assert!(ensure("s2", p, &cwd, base).is_err());
        std::env::remove_var("NEXUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }
}
