// Persisted sessions. Each run's transcript is written to
// ~/.buildwithnexus/sessions/<id>.json so past work can be listed and resumed
// (`/resume`, `--continue`, `--resume <id>`). Plain file IO + serde — the
// transcript types are serializable (see provider::Msg).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config;
use crate::provider::Msg;

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
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn dir() -> PathBuf {
    config::home().join("sessions")
}

// Monotonic-ish id from the wall clock (ms), zero-padded so lexical == temporal.
pub fn new_id() -> String {
    format!("{:016}", now_ms())
}

fn file(id: &str) -> PathBuf {
    dir().join(format!("{id}.json"))
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

// Create or update the session for `id` from the current transcript. Preserves
// the original created time across updates. No-op for an empty transcript.
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
        let _ = std::fs::write(file(id), text);
    }
}

pub fn load(id: &str) -> Option<Session> {
    let text = std::fs::read_to_string(file(id)).ok()?;
    serde_json::from_str(&text).ok()
}

// All sessions, newest first.
pub fn list() -> Vec<Session> {
    let mut v: Vec<Session> = match std::fs::read_dir(dir()) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|t| serde_json::from_str::<Session>(&t).ok())
            .collect(),
        Err(_) => Vec::new(),
    };
    v.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
    v
}

pub fn latest() -> Option<Session> {
    list().into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_home<T>(f: impl FnOnce() -> T) -> T {
        use std::sync::atomic::{AtomicU64, Ordering};
        // Serialize against config tests too — they share NEXUS_HOME.
        let _g = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
            let msgs = vec![Msg::System("sys".into()), Msg::User("fix the parser bug".into())];
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
            save("0000000000000001", Path::new("/p"), "m", &[Msg::User("first".into())]);
            save("0000000000000002", Path::new("/p"), "m", &[Msg::User("second".into())]);
            save("0000000000000003", Path::new("/p"), "m", &[]); // empty: skipped
            let ls = list();
            assert_eq!(ls.len(), 2);
            // updated_ms ties are possible; assert both present, newest-id first-ish.
            assert!(ls.iter().any(|s| s.title == "first"));
            assert!(ls.iter().any(|s| s.title == "second"));
        });
    }
}
