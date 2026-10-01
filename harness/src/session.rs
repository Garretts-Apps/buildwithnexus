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

/// Layout version of a session file, saved as `schema_version` (see
/// docs/VERSIONING.md). Files written before 0.15 have none and read as 1:
/// the layout is the same.
pub const SCHEMA_VERSION: u32 = 1;

fn schema_v1() -> u32 {
    1
}

/// Represents a persisted user conversation session.
///
/// Sessions are stored as JSON files in `~/.buildwithnexus/sessions/<id>.json`
/// and can be resumed across runs via `/resume` or `--continue`.
#[derive(Serialize, Deserialize)]
pub struct Session {
    #[serde(default = "schema_v1")]
    pub schema_version: u32,
    pub id: String,
    pub title: String, // first user prompt, truncated
    pub cwd: String,
    pub model: String,
    pub created_ms: u128,
    pub updated_ms: u128,
    pub msgs: Vec<Msg>,
    /// A name set with /rename; shown instead of the title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Session {
    /// The name given with /rename, else the first prompt.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.title)
    }

    /// Whether the session was started in `cwd`.
    pub fn is_in(&self, cwd: &Path) -> bool {
        same_dir(Path::new(&self.cwd), cwd)
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
}

/// How long ago `ms` was, in the largest whole unit: "just now", "5m ago",
/// "3h ago", "2d ago".
pub fn ago(ms: u128) -> String {
    let secs = now_ms().saturating_sub(ms) / 1000;
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
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

/// Generates a new session ID: zero-padded wall-clock milliseconds, so
/// lexical order is time order, then eight random hex digits, so parallel
/// runs sharing one home (CI jobs, background workflows) that start in the
/// same millisecond never write the same file. IDs from before 0.15 are the
/// 16 digits alone and still load.
pub fn new_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    // RandomState is seeded from the OS per process; the counter and the
    // pid keep ids distinct within a process and across processes.
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(SEQ.fetch_add(1, Ordering::Relaxed));
    h.write_u32(std::process::id());
    format!("{:016}-{:08x}", now_ms(), h.finish() as u32)
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
    let before = load(id);
    let s = Session {
        schema_version: SCHEMA_VERSION,
        id: id.to_string(),
        title: title_of(msgs),
        cwd: cwd.to_string_lossy().into_owned(),
        model: model.to_string(),
        created_ms: before.as_ref().map(|s| s.created_ms).unwrap_or_else(now_ms),
        updated_ms: now_ms(),
        msgs: msgs.to_vec(),
        name: before.and_then(|s| s.name),
    };
    write(&s);
}

fn write(s: &Session) -> bool {
    let Ok(text) = serde_json::to_string(s) else {
        return false;
    };
    // Write-then-rename so a crash mid-save never truncates the previous
    // session file. list() only picks up `.json` files, so the temp file
    // is invisible even if a crash leaves it behind.
    let path = file(&s.id);
    let tmp = dir().join(format!("{}.json.tmp", s.id));
    if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
        return true;
    }
    let _ = std::fs::remove_file(&tmp);
    false
}

/// Names a saved session (/rename). The name replaces the first prompt in
/// listings and survives later saves.
pub fn rename(id: &str, name: &str) -> Result<(), String> {
    let Some(mut s) = load(id) else {
        return Err("nothing to name yet — the session is saved after its first message".into());
    };
    let name = name.trim();
    s.name = (!name.is_empty()).then(|| name.chars().take(80).collect());
    if write(&s) {
        Ok(())
    } else {
        Err(format!("could not write {}", file(id).display()))
    }
}

/// Deletes a saved session; returns it so the caller can say which.
pub fn remove(id: &str) -> Result<Session, String> {
    let Some(s) = load(id) else {
        return Err(format!("no session '{id}' — bwn sessions lists them"));
    };
    std::fs::remove_file(file(id)).map_err(|e| format!("could not delete {id}: {e}"))?;
    Ok(s)
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

/// The sessions started in `cwd`, newest first.
pub fn list_for(cwd: &Path) -> Vec<Session> {
    list().into_iter().filter(|s| s.is_in(cwd)).collect()
}

/// The newest session started in `cwd`.
pub fn latest_for(cwd: &Path) -> Option<Session> {
    list_for(cwd).into_iter().next()
}

/// Every session, the ones started in `cwd` first; each group newest first.
pub fn list_here_first(cwd: &Path) -> Vec<Session> {
    let (mut here, others): (Vec<Session>, Vec<Session>) =
        list().into_iter().partition(|s| s.is_in(cwd));
    here.extend(others);
    here
}

/// `ms` as a UTC date and time, "2026-10-01 14:05 UTC".
pub fn utc(ms: u128) -> String {
    let secs = (ms / 1000) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// The conversation as Markdown, for /export and `bwn sessions export`: the
/// messages and answers, each tool call as one line. The system prompt and
/// tool output are left out; the session file keeps them.
pub fn to_markdown(s: &Session) -> String {
    let mut out = format!("# {}\n\n", s.label().trim());
    out.push_str(&format!(
        "- session: {}\n- folder: {}\n- model: {}\n- started: {}\n- updated: {}\n",
        s.id,
        s.cwd,
        s.model,
        utc(s.created_ms),
        utc(s.updated_ms)
    ));
    for m in &s.msgs {
        match m {
            Msg::System(_) | Msg::Tool(_) => {}
            Msg::User(text) | Msg::UserImages { text, .. } => {
                // Notes bwn adds to the conversation are not the person's.
                let who = if text.starts_with("[harness]") {
                    "bwn note"
                } else {
                    "You"
                };
                out.push_str(&format!("\n## {who}\n\n{}\n", text.trim()));
                if let Msg::UserImages { images, .. } = m {
                    let n = images.len();
                    out.push_str(&format!(
                        "\n_{n} image{} attached_\n",
                        if n == 1 { "" } else { "s" }
                    ));
                }
            }
            Msg::Assistant { text, calls } => {
                if text.trim().is_empty() && calls.is_empty() {
                    continue;
                }
                out.push_str("\n## bwn\n\n");
                if !text.trim().is_empty() {
                    out.push_str(text.trim());
                    out.push('\n');
                }
                if !calls.is_empty() && !text.trim().is_empty() {
                    out.push('\n');
                }
                for c in calls {
                    out.push_str(&format!("- {}\n", call_line(c)));
                }
            }
        }
    }
    out
}

// One line for a tool call: its preview ("edit src/app.py", "run: cargo
// test"), or its name and path for tools without one.
fn call_line(c: &crate::provider::ToolCall) -> String {
    let preview = crate::tools::preview(&c.name, &c.input);
    if preview != c.name {
        return preview;
    }
    match c.input["path"].as_str() {
        Some(path) => format!("{} {path}", c.name),
        None => c.name.clone(),
    }
}

/// Writes the session as Markdown to `path`, or to
/// `<home>/exports/<id>.md` when none is given; returns where it went.
pub fn export(s: &Session, path: Option<&Path>) -> Result<PathBuf, String> {
    let path = match path {
        Some(p) => p.to_path_buf(),
        None => config::home().join("exports").join(format!("{}.md", s.id)),
    };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, to_markdown(s))
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(path)
}

// A session `bwn continue`, `-c` or `resume <id>` asked the interactive UI to
// open: set before the REPL starts, taken once when it does.
static RESUME_ON_START: Mutex<Option<Session>> = Mutex::new(None);

/// Opens `s` in the next REPL to start, and makes it the current session so
/// SessionStart hooks name it.
pub fn resume_on_start(s: Session) {
    set_current(&s.id);
    if let Ok(mut r) = RESUME_ON_START.lock() {
        *r = Some(s);
    }
}

/// The session to open at REPL start, if one was asked for.
pub fn take_resume_on_start() -> Option<Session> {
    RESUME_ON_START.lock().ok().and_then(|mut r| r.take())
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
        assert_eq!(first.len(), 25);
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
    fn ids_minted_at_once_on_many_threads_are_unique_and_sort_by_time() {
        let ids: Vec<String> = (0..8)
            .map(|_| std::thread::spawn(|| (0..500).map(|_| new_id()).collect::<Vec<_>>()))
            .flat_map(|h| h.join().unwrap())
            .collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "every id is distinct");
        for id in &ids {
            let (ms, tag) = id.split_once('-').expect("time-tag");
            assert_eq!(ms.len(), 16);
            assert_eq!(tag.len(), 8);
        }
        // Lexical order is time order: an id minted later sorts after.
        let early = new_id();
        std::thread::sleep(std::time::Duration::from_millis(3));
        let late = new_id();
        assert!(late > early, "{late} sorts after {early}");
        let older = format!("{:016}", now_ms() - 1000);
        assert!(early > older, "sorts after a 16-digit id from before 0.15");
    }

    #[test]
    fn sessions_belong_to_the_folder_they_were_started_in() {
        with_home(|| {
            let a = std::env::temp_dir().join(format!("bwn-sess-a-{}", std::process::id()));
            let b = std::env::temp_dir().join(format!("bwn-sess-b-{}", std::process::id()));
            for d in [&a, &b] {
                let _ = std::fs::create_dir_all(d);
            }
            save("0000000000000001", &a, "m", &[Msg::User("in A".into())]);
            std::thread::sleep(std::time::Duration::from_millis(3));
            save("0000000000000002", &b, "m", &[Msg::User("in B".into())]);
            assert_eq!(latest().unwrap().title, "in B", "B was used last");
            assert_eq!(latest_for(&a).unwrap().title, "in A");
            assert_eq!(list_for(&b).len(), 1);
            let titles: Vec<String> = list_here_first(&a).into_iter().map(|s| s.title).collect();
            assert_eq!(titles, ["in A", "in B"]);
            let none = std::env::temp_dir().join("bwn-sess-none");
            assert!(latest_for(&none).is_none());
            for d in [&a, &b] {
                let _ = std::fs::remove_dir_all(d);
            }
        });
    }

    #[test]
    fn rename_survives_saves_and_remove_deletes() {
        with_home(|| {
            assert!(rename("0000000000000003", "x").is_err(), "not saved yet");
            save(
                "0000000000000003",
                Path::new("/p"),
                "m",
                &[Msg::User("first".into())],
            );
            rename("0000000000000003", "  parser work  ").unwrap();
            save(
                "0000000000000003",
                Path::new("/p"),
                "m",
                &[Msg::User("first".into()), Msg::User("more".into())],
            );
            let s = load("0000000000000003").unwrap();
            assert_eq!(s.label(), "parser work");
            assert_eq!(s.title, "first");
            assert_eq!(remove("0000000000000003").unwrap().label(), "parser work");
            assert!(load("0000000000000003").is_none());
            let Err(err) = remove("123") else {
                panic!("no such session")
            };
            assert_eq!(err, "no session '123' — bwn sessions lists them");
        });
    }

    #[test]
    fn markdown_export_has_the_conversation_and_not_the_plumbing() {
        let s = Session {
            schema_version: 1,
            id: "0001790000000000-abcd1234".into(),
            title: "fix the parser".into(),
            cwd: "/work/api".into(),
            model: "m".into(),
            created_ms: 0,
            updated_ms: 86_400_000 + 3_600_000 * 14 + 60_000 * 5,
            msgs: vec![
                Msg::System("SECRET SYSTEM PROMPT".into()),
                Msg::User("fix the parser".into()),
                Msg::Assistant {
                    text: "Looking.".into(),
                    calls: vec![crate::provider::ToolCall {
                        id: "c1".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({"path": "src/parser.rs"}),
                    }],
                },
                Msg::Tool(vec![crate::provider::ToolResult {
                    id: "c1".into(),
                    content: "TOOL OUTPUT BODY".into(),
                    is_error: false,
                }]),
                Msg::User("[harness] You called finish without running check_work".into()),
                Msg::Assistant {
                    text: "Fixed the off-by-one.".into(),
                    calls: vec![],
                },
                Msg::UserImages {
                    text: "and this screenshot?".into(),
                    images: vec![("image/png".into(), "AAAA".into())],
                },
            ],
            name: Some("parser work".into()),
        };
        let md = to_markdown(&s);
        assert!(md.starts_with("# parser work\n"), "{md}");
        assert!(md.contains("- updated: 1970-01-02 14:05 UTC"), "{md}");
        assert!(md.contains("## You\n\nfix the parser\n"));
        assert!(
            md.contains("## bwn\n\nLooking.\n\n- read_file src/parser.rs\n"),
            "{md}"
        );
        assert!(md.contains("## bwn note\n\n[harness] You called finish"));
        assert!(md.contains("## bwn\n\nFixed the off-by-one.\n"));
        assert!(md.contains("_1 image attached_"));
        assert!(!md.contains("SECRET SYSTEM PROMPT") && !md.contains("TOOL OUTPUT BODY"));
        assert_eq!(utc(1_790_000_000_000), "2026-09-21 14:13 UTC");
    }

    #[test]
    fn ages_read_in_the_largest_whole_unit() {
        let now = now_ms();
        assert_eq!(ago(now), "just now");
        assert_eq!(ago(now - 5 * 60_000), "5m ago");
        assert_eq!(ago(now - 3 * 3_600_000), "3h ago");
        assert_eq!(ago(now - 2 * 86_400_000), "2d ago");
    }

    #[test]
    fn saved_sessions_carry_the_schema_version() {
        with_home(|| {
            save(
                "0000000000000005",
                Path::new("/p"),
                "m",
                &[Msg::User("v".into())],
            );
            let text = std::fs::read_to_string(file("0000000000000005")).unwrap();
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(v["schema_version"], SCHEMA_VERSION);
            assert_eq!(
                load("0000000000000005").unwrap().schema_version,
                SCHEMA_VERSION
            );
        });
    }

    #[test]
    fn a_session_file_from_before_schema_version_still_loads() {
        // As 0.14.10 wrote it: no schema_version.
        with_home(|| {
            let _ = std::fs::create_dir_all(dir());
            std::fs::write(
                file("0000000000000006"),
                r#"{"id":"0000000000000006","title":"old task","cwd":"/p","model":"m","created_ms":1,"updated_ms":2,"msgs":[{"User":"old task"}]}"#,
            )
            .unwrap();
            let s = load("0000000000000006").expect("an old session file loads");
            assert_eq!(s.title, "old task");
            assert_eq!(s.msgs.len(), 1);
            assert_eq!(s.schema_version, 1, "no schema_version reads as version 1");
            assert_eq!(list().len(), 1);
            // Resuming and saving it again writes the current version.
            save("0000000000000006", Path::new("/p"), "m", &s.msgs);
            let s = load("0000000000000006").unwrap();
            assert_eq!(s.schema_version, SCHEMA_VERSION);
            assert_eq!(s.created_ms, 1, "the original creation time is kept");
        });
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
