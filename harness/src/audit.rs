//! The decision log (roadmap 2.5; analysis SO-7): every gate decision,
//! approval, exception, startup-check acceptance and verification result,
//! one JSON object per line, in one file per session under
//! `~/.buildwithnexus/audit/`.
//!
//! Each line carries `prev`, the SHA-256 of the previous line's raw bytes
//! (the first line's is the session's genesis value). Removing, editing or
//! reordering a line breaks the chain from there on, and
//! `bwn-audit-verify`, a separate program that shares no code with this
//! writer, finds it. What it can't find on its own is the whole file
//! rewritten with a consistent chain; the anchor (roadmap 2.5, D3: the OS
//! keyring and commit trailers) covers that.
//!
//! The folder is bwn's own, which sandboxed commands can't read or write
//! and file tools refuse. Text is redacted (key-like tokens) and capped
//! before it is written.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// The most a free-text field keeps.
const DETAIL_CAP: usize = 400;

/// What kind of decision a line records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The permission gate's answer for one tool call.
    Gate,
    /// A person, or a PermissionRequest hook, answered a prompt.
    Approval,
    /// A session-wide exception (commands unconfined, network on Landlock
    /// alone).
    Exception,
    /// The startup posture check was accepted or refused.
    Posture,
    /// A verification ran and passed or failed.
    Verify,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Gate => "gate",
            Kind::Approval => "approval",
            Kind::Exception => "exception",
            Kind::Posture => "posture",
            Kind::Verify => "verify",
        }
    }
}

/// One decision to record.
pub struct Entry<'a> {
    pub kind: Kind,
    /// Who decided: `user`, `hook`, `rule`, `gate`, `policy`, `flag`, `check`.
    pub actor: &'a str,
    /// `allow`, `deny`, `accept`, `stop`, `pass`, `fail`.
    pub decision: &'a str,
    /// The tool or subject (`run_command`, `posture`, a check's name).
    pub subject: &'a str,
    /// Why, in a few words: the rule, the reason, the prompt.
    pub detail: &'a str,
}

struct Chain {
    sid: String,
    path: PathBuf,
    seq: u64,
    prev: String,
}

static CHAIN: Mutex<Option<Chain>> = Mutex::new(None);

/// The first `prev` of a session's chain.
/// It names the session, as the file does, so a verifier given only the
/// file knows where the chain must start.
pub fn genesis(sid: &str) -> String {
    hex(&sha256(
        format!("bwn-audit-genesis:{}", safe_id(sid)).as_bytes(),
    ))
}

fn safe_id(sid: &str) -> String {
    sid.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Where a session's log is.
pub fn path_for(sid: &str) -> PathBuf {
    crate::config::home()
        .join("audit")
        .join(format!("{}.jsonl", safe_id(sid)))
}

/// The hash of the last line written this session, the chain's head.
pub fn head() -> Option<(String, String, u64)> {
    let g = CHAIN.lock().unwrap_or_else(|e| e.into_inner());
    g.as_ref().map(|c| (c.sid.clone(), c.prev.clone(), c.seq))
}

/// Record one decision. Never fails the caller: a log that can't be
/// written says so once on stderr.
pub fn record(e: Entry) {
    let sid = crate::session::current().unwrap_or_else(|| "no-session".to_string());
    record_in(&sid, e);
}

fn record_in(sid: &str, e: Entry) {
    let sid = sid.to_string();
    let mut g = CHAIN.lock().unwrap_or_else(|e| e.into_inner());
    if g.as_ref().is_none_or(|c| c.sid != sid) {
        *g = Some(open(&sid));
    }
    let Some(chain) = g.as_mut() else { return };
    let clip = |s: &str| {
        let r = crate::provider::redact(s);
        let mut t: String = r.chars().take(DETAIL_CAP).collect();
        if r.chars().count() > DETAIL_CAP {
            t.push('…');
        }
        t
    };
    let line = serde_json::json!({
        "seq": chain.seq + 1,
        "time": now_rfc3339(),
        "kind": e.kind.as_str(),
        "actor": e.actor,
        "decision": e.decision,
        "subject": clip(e.subject),
        "detail": clip(e.detail),
        "prev": chain.prev,
    })
    .to_string();
    if let Some(dir) = chain.path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let wrote = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&chain.path)
        .and_then(|mut f| f.write_all(format!("{line}\n").as_bytes()));
    match wrote {
        Ok(()) => {
            chain.seq += 1;
            chain.prev = hex(&sha256(line.as_bytes()));
        }
        Err(err) => {
            static TOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !TOLD.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!(
                    "buildwithnexus: warning: the decision log {} can't be written: {err}",
                    chain.path.display()
                );
            }
        }
    }
}

// Pick up a session's chain where its file ends (a resumed session).
fn open(sid: &str) -> Chain {
    let path = path_for(sid);
    let mut seq = 0;
    let mut prev = genesis(sid);
    if let Ok(text) = std::fs::read_to_string(&path) {
        for line in text.lines().filter(|l| !l.is_empty()) {
            seq += 1;
            prev = hex(&sha256(line.as_bytes()));
        }
    }
    Chain {
        sid: sid.to_string(),
        path,
        seq,
        prev,
    }
}

fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let millis = now.subsec_millis();
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn sha256(b: &[u8]) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA256, b)
        .as_ref()
        .to_vec()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ── the anchor (roadmap 2.5, D3: the OS keyring and commit trailers) ──────
//
// The chain's head is written outside the log: to the OS keyring at the end
// of each turn and of the session, and into a trailer on each commit bwn
// makes. `bwn audit head` prints the keyring's copy for
// `bwn-audit-verify --head`, which then also catches a log rewritten with a
// consistent chain or cut short. Sandboxed commands can't reach the keyring
// (on macOS the profile refuses the keychain daemon; on Linux the session
// bus is out of their reach under the checker or with the network off). A
// process outside the sandbox, running as the user, can; a pushed commit
// trailer is the witness against that.

const SERVICE: &str = "buildwithnexus-audit";
/// No keyring call may hold bwn up longer than this.
const KEYRING_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Where the head is anchored on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Keyring {
    /// macOS: the login keychain, through `security`.
    MacKeychain,
    /// Linux: the Secret Service, through `secret-tool`.
    SecretService,
    /// A folder standing in for the keyring (debug builds: tests).
    TestDir(PathBuf),
}

impl Keyring {
    pub fn label(&self) -> &'static str {
        match self {
            Keyring::MacKeychain => "the macOS keychain",
            Keyring::SecretService => "the Secret Service keyring",
            Keyring::TestDir(_) => "a test keyring",
        }
    }
}

/// The keyring this machine offers, if any. Never prompts.
pub fn keyring() -> Option<Keyring> {
    #[cfg(debug_assertions)]
    if let Some(d) = std::env::var_os("BWN_TEST_KEYRING") {
        return (!d.is_empty()).then(|| Keyring::TestDir(PathBuf::from(d)));
    }
    // Unit tests never touch a real keychain.
    if cfg!(test) {
        return None;
    }
    if cfg!(target_os = "macos") && crate::tools::find_on_path("security").is_some() {
        return Some(Keyring::MacKeychain);
    }
    if cfg!(target_os = "linux")
        && std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
        && crate::tools::find_on_path("secret-tool").is_some()
    {
        return Some(Keyring::SecretService);
    }
    None
}

// Run a keyring tool with a hard limit, stdin optional. Some(stdout) on
// success.
fn run_limited(program: &str, args: &[&str], stdin: Option<&str>) -> Option<String> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(program)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let _ = pipe.write_all(text.as_bytes());
    }
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                if let Some(mut o) = child.stdout.take() {
                    let _ = std::io::Read::read_to_string(&mut o, &mut out);
                }
                return status.success().then_some(out);
            }
            Ok(None) if start.elapsed() < KEYRING_WAIT => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

pub(crate) fn keyring_set(k: &Keyring, account: &str, value: &str) -> bool {
    match k {
        Keyring::MacKeychain => run_limited(
            "security",
            &[
                "add-generic-password",
                "-U",
                "-s",
                SERVICE,
                "-a",
                account,
                "-w",
                value,
            ],
            None,
        )
        .is_some(),
        Keyring::SecretService => run_limited(
            "secret-tool",
            &[
                "store",
                "--label=bwn decision log head",
                "service",
                SERVICE,
                "account",
                account,
            ],
            Some(value),
        )
        .is_some(),
        Keyring::TestDir(d) => {
            std::fs::create_dir_all(d).is_ok()
                && std::fs::write(d.join(format!("{account}.head")), value).is_ok()
        }
    }
}

pub(crate) fn keyring_get(k: &Keyring, account: &str) -> Option<String> {
    let v = match k {
        Keyring::MacKeychain => run_limited(
            "security",
            &["find-generic-password", "-s", SERVICE, "-a", account, "-w"],
            None,
        ),
        Keyring::SecretService => run_limited(
            "secret-tool",
            &["lookup", "service", SERVICE, "account", account],
            None,
        ),
        Keyring::TestDir(d) => std::fs::read_to_string(d.join(format!("{account}.head"))).ok(),
    }?;
    let v = v.trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// Write the chain's head to the keyring (turn end, session end). A failure
/// says so once; it never stops the session.
pub fn anchor() {
    let Some((sid, head, seq)) = head() else {
        return;
    };
    if seq == 0 {
        return;
    }
    let Some(k) = keyring() else { return };
    if !keyring_set(&k, &safe_id(&sid), &format!("{seq} {head}")) {
        static TOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !TOLD.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "buildwithnexus: warning: could not write the decision log's head to {}",
                k.label()
            );
        }
    }
}

/// The head the keyring holds for a session: (number of decisions, head).
pub fn anchored_head(sid: &str) -> Option<(u64, String)> {
    let v = keyring_get(&keyring()?, &safe_id(sid))?;
    let (n, h) = v.split_once(' ')?;
    Some((n.parse().ok()?, h.to_string()))
}

/// The trailer a commit bwn makes carries, so pushed history witnesses the
/// chain's head.
pub fn commit_trailer() -> Option<String> {
    let (sid, head, seq) = head()?;
    (seq > 0).then(|| format!("Bwn-Decision-Log: {} {seq} {head}", safe_id(&sid)))
}

/// `bwn audit head [session]` and `bwn audit path [session]`.
pub fn cli(args: &[String]) -> i32 {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let sid = args
        .get(1)
        .cloned()
        .or_else(latest_session)
        .unwrap_or_default();
    match sub {
        "head" if !sid.is_empty() => match anchored_head(&sid) {
            Some((n, h)) => {
                println!("{h}");
                eprintln!(
                    "{n} decisions in session {sid}, anchored in {}",
                    keyring().map(|k| k.label()).unwrap_or("?")
                );
                0
            }
            None => {
                eprintln!(
                    "no anchored head for session {sid} ({})",
                    keyring().map_or("no keyring on this machine", |k| k.label())
                );
                1
            }
        },
        "path" if !sid.is_empty() => {
            println!("{}", path_for(&sid).display());
            0
        }
        _ => {
            eprintln!(
                "usage: buildwithnexus audit head [session] | audit path [session]\n  then: bwn-audit-verify \"$(buildwithnexus audit path)\" --head \"$(buildwithnexus audit head)\""
            );
            2
        }
    }
}

// The most recently written session log's id.
fn latest_session() -> Option<String> {
    let dir = crate::config::home().join("audit");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .and_then(|e| {
            e.path()
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
}

#[cfg(test)]
pub(crate) fn forget_for_test() {
    *CHAIN.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    // The log is a new place a secret could land (a command carrying an
    // Authorization header): key-like tokens are redacted and text capped.
    #[test]
    fn a_logged_secret_is_redacted_and_text_is_capped() {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("bwn-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("NEXUS_HOME", &home);
        forget_for_test();
        let key = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        record_in(
            "audit-redact-test",
            Entry {
                kind: Kind::Gate,
                actor: "gate",
                decision: "allow",
                subject: "run_command",
                detail: &format!("curl -H 'Authorization: Bearer {key}' https://x"),
            },
        );
        record_in(
            "audit-redact-test",
            Entry {
                kind: Kind::Gate,
                actor: "gate",
                decision: "allow",
                subject: "run_command",
                detail: &"y".repeat(5000),
            },
        );
        let text = std::fs::read_to_string(path_for("audit-redact-test")).unwrap();
        forget_for_test();
        std::env::remove_var("NEXUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
        assert!(!text.contains(key), "{text}");
        let long = text.lines().nth(1).unwrap();
        assert!(long.len() < 1000, "{} bytes", long.len());
    }

    #[test]
    fn dates_are_civil() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_419), (2025, 11, 27));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}
