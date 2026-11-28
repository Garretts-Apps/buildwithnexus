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
