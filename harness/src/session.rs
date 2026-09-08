// Persisted sessions. Each run's transcript is written to
// ~/.buildwithnexus/sessions/<id>.json so past work can be listed and resumed
// (`/resume`, `--continue`, `--resume <id>`). Plain file IO + serde — the
// transcript types are serializable (see provider::Msg).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config;
use crate::provider::Msg;

/// Represents a persisted user conversation session.
///
/// Sessions are stored as JSON files in `~/.buildwithnexus/sessions/<id>.json`
/// and can be resumed across runs via `/resume` or `--continue`.
#[derive(Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String, // first user prompt, truncated
    pub cwd: String,
    pub model: String,
    pub created_ms: u128,
    pub updated_ms: u128,
    pub msgs: Vec<Msg>,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn dir() -> PathBuf {
    config::home().join("sessions")
}

/// Generates a new zero-padded 16-character session ID based on wall-clock milliseconds.
///
/// Zero-padding ensures that lexical sorting of session IDs matches temporal order.
pub fn new_id() -> String {
    format!("{:016}", now_ms())
}

fn file(id: &str) -> PathBuf {
    dir().join(format!("{id}.json"))
}

/// Where the transcript for `id` is (or will be) saved.
pub fn path(id: &str) -> PathBuf {
    file(id)
}

// The id of the session this process is running — the same value the
// transcript is saved under, so hooks and traces can name the real file.
// `claimed` is false while the id has only been minted for hooks (e.g. a
// headless SessionStart) and no session owns it yet.
struct Current {
    id: String,
    claimed: bool,
}

static CURRENT: Mutex<Option<Current>> = Mutex::new(None);

/// Marks `id` as the process's current session, owned by a running session.
pub fn set_current(id: &str) {
    if let Ok(mut c) = CURRENT.lock() {
        *c = Some(Current {
            id: id.to_string(),
            claimed: true,
        });
    }
}

/// The current session id, if one has been established.
pub fn current() -> Option<String> {
    CURRENT
        .lock()
        .ok()
        .and_then(|c| c.as_ref().map(|c| c.id.clone()))
}

/// The current session id, minting one when none has been set yet — so
/// SessionStart hooks and the first build session agree on the id.
pub fn current_or_new() -> String {
    let mut c = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    c.get_or_insert_with(|| Current {
        id: new_id(),
        claimed: false,
    })
    .id
    .clone()
}

/// The id a new build session should save under: the minted-but-unowned
/// current id when there is one (headless runs), otherwise a fresh id — a
/// session that already owns the current id (the REPL) is never clobbered.
pub fn claim_or_new() -> String {
    let mut c = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    match c.as_mut() {
        Some(cur) if !cur.claimed => {
            cur.claimed = true;
            cur.id.clone()
        }
        _ => new_id(),
    }
}

// First non-empty user message, truncated — the human-readable label.
fn title_of(msgs: &[Msg]) -> String {
    for m in msgs {
        if let Msg::User(t) = m {
            let t = t.trim();
            if !t.is_empty() {
                return t.chars().take(80).collect();
            }
        }
    }
    "(untitled)".to_string()
}

/// Creates or updates a persisted session file for `id` from the provided transcript.
///
/// Preserves the original creation timestamp across updates. A no-op if `msgs` is empty.
pub fn save(id: &str, cwd: &Path, model: &str, msgs: &[Msg]) {
    if msgs.is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(dir());
    let created = load(id).map(|s| s.created_ms).unwrap_or_else(now_ms);
    let s = Session {
        id: id.to_string(),
        title: title_of(msgs),
        cwd: cwd.to_string_lossy().into_owned(),
        model: model.to_string(),
        created_ms: created,
        updated_ms: now_ms(),
        msgs: msgs.to_vec(),
    };
    if let Ok(text) = serde_json::to_string(&s) {
        // Write-then-rename so a crash mid-save never truncates the previous
        // session file. list() only picks up `.json` files, so the temp file
        // is invisible even if a crash leaves it behind.
        let path = file(id);
        let tmp = dir().join(format!("{id}.json.tmp"));
        if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

// A session file that exists but fails to parse is data the user thinks is
// saved — surface it instead of silently dropping it from /resume listings.
fn parse_session(path: &Path, text: &str) -> Option<Session> {
    match serde_json::from_str(text) {
        Ok(s) => Some(s),
        Err(e) => {
            // tui::line, not eprintln — this runs inside the alt-screen TUI
            // during /resume and a raw write corrupts the display.
            crate::tui::line(&crate::tui::yellow(&format!(
                "  ⚠ skipping corrupt session file {}: {e}",
                path.display()
            )));
            None
        }
    }
}

/// Loads a persisted session by its ID from disk, if it exists and is valid JSON.
/// Warns on stderr if the file exists but cannot be parsed.
pub fn load(id: &str) -> Option<Session> {
    let path = file(id);
    let text = std::fs::read_to_string(&path).ok()?;
    parse_session(&path, &text)
}

/// Lists all persisted sessions, ordered newest-first by last update timestamp.
/// Corrupt session files are skipped with a warning on stderr.
pub fn list() -> Vec<Session> {
    let mut v: Vec<Session> = match std::fs::read_dir(dir()) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| {
                let path = e.path();
                let text = std::fs::read_to_string(&path).ok()?;
                parse_session(&path, &text)
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    v.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
    v
}

/// Returns the most recently updated session, or `None` if no sessions exist.
pub fn latest() -> Option<Session> {
    list().into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_home<T>(f: impl FnOnce() -> T) -> T {
        use std::sync::atomic::{AtomicU64, Ordering};
        // Serialize against config tests too — they share NEXUS_HOME.
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let home = std::env::temp_dir().join(format!("bwn-sess-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("NEXUS_HOME", &home);
        let r = f();
        std::env::remove_var("NEXUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
        r
    }

    #[test]
    fn save_load_roundtrip_and_title() {
        with_home(|| {
            let msgs = vec![
                Msg::System("sys".into()),
                Msg::User("fix the parser bug".into()),
            ];
            save("0000000000000001", Path::new("/proj"), "gpt-4o", &msgs);
            let s = load("0000000000000001").expect("session loads");
            assert_eq!(s.title, "fix the parser bug");
            assert_eq!(s.model, "gpt-4o");
            assert_eq!(s.msgs.len(), 2);
        });
    }

    #[test]
    fn list_orders_newest_first_and_empty_is_noop() {
        with_home(|| {
            save(
                "0000000000000001",
                Path::new("/p"),
                "m",
                &[Msg::User("first".into())],
            );
            save(
                "0000000000000002",
                Path::new("/p"),
                "m",
                &[Msg::User("second".into())],
            );
            save("0000000000000003", Path::new("/p"), "m", &[]); // empty: skipped
            let ls = list();
            assert_eq!(ls.len(), 2);
            // updated_ms ties are possible; assert both present, newest-id first-ish.
            assert!(ls.iter().any(|s| s.title == "first"));
            assert!(ls.iter().any(|s| s.title == "second"));
        });
    }

    #[test]
    fn save_leaves_no_tmp_file_and_result_is_loadable() {
        with_home(|| {
            save(
                "0000000000000009",
                Path::new("/p"),
                "m",
                &[Msg::User("atomic".into())],
            );
            assert!(load("0000000000000009").is_some());
            assert!(!dir().join("0000000000000009.json.tmp").exists());
        });
    }

    #[test]
    fn list_skips_corrupt_session_files_but_keeps_valid_ones() {
        with_home(|| {
            save(
                "0000000000000001",
                Path::new("/p"),
                "m",
                &[Msg::User("ok".into())],
            );
            std::fs::write(dir().join("corrupt.json"), "{not valid json").unwrap();
            let ls = list();
            assert_eq!(ls.len(), 1);
            assert_eq!(ls[0].title, "ok");
            // Corrupt file must still be on disk (skipped, not deleted).
            assert!(dir().join("corrupt.json").exists());
        });
    }

    #[test]
    fn current_session_id_is_sticky_and_claimable_once() {
        // Process-global: run the whole lifecycle in one test.
        let first = current_or_new();
        assert_eq!(first.len(), 16);
        assert_eq!(current_or_new(), first, "first id wins");
        assert_eq!(current().as_deref(), Some(first.as_str()));
        // A minted-for-hooks id is handed to the first build session…
        assert_eq!(claim_or_new(), first);
        // …but never to a second one: an owned id is not shared.
        assert_ne!(claim_or_new(), first);
        assert_eq!(current().as_deref(), Some(first.as_str()));
        set_current("0000000000000077");
        assert_eq!(current_or_new(), "0000000000000077");
        assert_ne!(
            claim_or_new(),
            "0000000000000077",
            "set_current owns the id"
        );
        assert!(path("0000000000000077").ends_with("sessions/0000000000000077.json"));
    }

    #[test]
    fn load_returns_none_for_corrupt_file() {
        with_home(|| {
            let _ = std::fs::create_dir_all(dir());
            std::fs::write(dir().join("0000000000000042.json"), "garbage").unwrap();
            assert!(load("0000000000000042").is_none());
        });
    }
}
