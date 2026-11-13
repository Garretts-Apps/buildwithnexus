// Persisted sessions. Each run's transcript is kept in
// ~/.buildwithnexus/sessions/<id>.json so past work can be listed and resumed
// (`/resume`, `--continue`, `--resume <id>`).
//
// The file is a log (JSON Lines, schema 2; docs/VERSIONING.md has the layout):
// a header line, then one line per message. Saving after a tool round appends
// the round's messages and fsyncs them; the whole file is written again only
// when history changed (compaction, /rewind, /rename, a new model or title).
// Files in the single-object layout of schema 1 still load, and are written
// in the new layout the next time they are saved.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config;
use crate::provider::Msg;

/// Layout version of a session file, saved as `schema_version` (see
/// docs/VERSIONING.md). 2 is the log: a header line, then a line per message.
/// 1 was one JSON object holding every message; files written before 0.15
/// have no version and read as 1.
pub const SCHEMA_VERSION: u32 = 2;

fn schema_v1() -> u32 {
    1
}

/// Represents a persisted user conversation session.
///
/// Sessions are stored in `~/.buildwithnexus/sessions/<id>.json` and can be
/// resumed across runs via `/resume` or `--continue`. Serialized, this is
/// the schema 1 file layout, which is still read.
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

/// A saved session without its messages: listings and pickers read only
/// this, from the file's header line, so a session full of screenshots costs
/// one small read. Deserialized, it is a schema 1 file, whose messages are
/// counted in a scan and never built.
#[derive(Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub model: String,
    pub created_ms: u128,
    pub updated_ms: u128,
    #[serde(rename = "msgs", deserialize_with = "count_seq")]
    pub msg_count: usize,
    #[serde(default)]
    pub name: Option<String>,
}

impl SessionInfo {
    /// The name given with /rename, else the first prompt.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.title)
    }

    /// Whether the session was started in `cwd`.
    pub fn is_in(&self, cwd: &Path) -> bool {
        same_dir(Path::new(&self.cwd), cwd)
    }
}

// The length of a JSON array, skipping its elements without building them.
fn count_seq<'de, D: serde::Deserializer<'de>>(d: D) -> Result<usize, D::Error> {
    struct Count;
    impl<'de> serde::de::Visitor<'de> for Count {
        type Value = usize;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("an array")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<usize, A::Error> {
            let mut n = 0;
            while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                n += 1;
            }
            Ok(n)
        }
    }
    d.deserialize_seq(Count)
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

/// Where the transcript for `id` is (or will be) saved.
pub fn path(id: &str) -> PathBuf {
    dir().join(format!("{id}.json"))
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

// True while the latest save failed. A full disk or a read-only home fails
// every save after every tool round, so the user is warned when saving
// starts failing, not on each attempt, and again only after it has worked.
static SAVE_FAILING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the latest save reached the disk (true before any save), so an
/// exit message says "saved" only when it is.
pub fn last_save_ok() -> bool {
    !SAVE_FAILING.load(std::sync::atomic::Ordering::Relaxed)
}

/// Brings the session file for `id` up to date with the transcript: the
/// messages it does not hold yet are appended, or the file is written again
/// when the ones it holds are no longer the transcript's (compaction, a
/// rewind, a new model). Every message is compared in full.
///
/// Preserves the original creation timestamp and name. A no-op if `msgs` is
/// empty. Returns whether the session is on disk; a failure is also shown to
/// the user once (until a save succeeds again).
pub fn save(id: &str, cwd: &Path, model: &str, msgs: &[Msg]) -> bool {
    saved(id, save_now(id, cwd, model, msgs, Check::Full))
}

/// `save` for the end of a tool round, where its cost must not grow with the
/// conversation: a message saved earlier in this run is taken as unchanged
/// when its text still sits at the same place in memory with the same length
/// and the same first and last bytes, and is compared in full only when one
/// of those moved. `save`, which every turn starts and ends with, compares
/// everything.
pub fn save_round(id: &str, cwd: &Path, model: &str, msgs: &[Msg]) -> bool {
    saved(id, save_now(id, cwd, model, msgs, Check::Edges))
}

fn saved(_id: &str, result: Result<(), String>) -> bool {
    let was_failing = SAVE_FAILING.swap(result.is_err(), std::sync::atomic::Ordering::Relaxed);
    match result {
        Ok(()) => true,
        Err(e) => {
            if !was_failing {
                crate::report::notice(&format!(
                    "  session not saved: {e} — this conversation cannot be resumed until saving works again"
                ));
            }
            false
        }
    }
}

// ── The file ─────────────────────────────────────────────────────────────────

// Line 1. `updated_ms` and `messages` come last and are padded to a fixed
// width, so an append can bring them up to date in place: listings read this
// line and nothing else.
#[derive(Serialize, Deserialize)]
struct Header {
    schema_version: u32,
    id: String,
    title: String,
    cwd: String,
    model: String,
    created_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    updated_ms: u128,
    messages: usize,
}

// Every line after the first. An object, so a later version can add other
// kinds of record; one without `msg` is skipped.
#[derive(Deserialize)]
struct Record {
    #[serde(default)]
    msg: Option<Msg>,
}

// The fixed-width end of the header line, newline included.
fn header_tail(updated_ms: u128, messages: usize) -> String {
    format!(
        ",\"updated_ms\":{:>20},\"messages\":{:>10}}}\n",
        updated_ms.min(99_999_999_999_999_999_999),
        messages.min(9_999_999_999)
    )
}

const HEADER_TAIL_LEN: u64 = 58;

fn header_line(h: &Header) -> Result<String, String> {
    // The same fields, in the same order, as `Header` up to `name`.
    #[derive(Serialize)]
    struct Head<'a> {
        schema_version: u32,
        id: &'a str,
        title: &'a str,
        cwd: &'a str,
        model: &'a str,
        created_ms: u128,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<&'a str>,
    }
    let mut line = serde_json::to_string(&Head {
        schema_version: h.schema_version,
        id: &h.id,
        title: &h.title,
        cwd: &h.cwd,
        model: &h.model,
        created_ms: h.created_ms,
        name: h.name.as_deref(),
    })
    .map_err(|e| e.to_string())?;
    line.pop(); // the closing brace: the tail has its own
    line.push_str(&header_tail(h.updated_ms, h.messages));
    Ok(line)
}

fn push_record(out: &mut Vec<u8>, m: &Msg) -> Result<(), String> {
    out.extend_from_slice(b"{\"msg\":");
    serde_json::to_writer(&mut *out, m).map_err(|e| e.to_string())?;
    out.extend_from_slice(b"}\n");
    Ok(())
}

// What identifies the bytes this process last left in a session file: if any
// of these differ, something else wrote it (another bwn on the same session,
// `sessions rm`) and nothing may be appended on the strength of what this
// process remembers.
#[derive(Clone, Copy, PartialEq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    inode: u64,
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Stamp {
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(meta);
        #[cfg(not(unix))]
        let inode = 0;
        Stamp {
            len: meta.len(),
            modified: meta.modified().ok(),
            inode,
        }
    }
}

// ── What this process knows is in a session file ─────────────────────────────

// How closely `save` looks at the messages it has already written.
#[derive(Clone, Copy, PartialEq)]
enum Check {
    /// Every byte of every message.
    Full,
    /// Where each string is in memory, its length and its first and last
    /// bytes; every byte only for a message where one of those changed.
    Edges,
}

// A session file this process wrote, and what it holds.
struct Tracked {
    path: PathBuf,
    title: String,
    cwd: String,
    model: String,
    created_ms: u128,
    name: Option<String>,
    header_len: u64,
    stamp: Stamp,
    /// Per message on disk: its edges print and its full print.
    prints: Vec<(u64, u64)>,
}

// Newest last. A process saves a handful of sessions (its own, a subagent's,
// a review's); the oldest is forgotten past this many, and a forgotten
// session is simply written in full the next time it is saved.
const TRACKED_MAX: usize = 32;
static TRACKED: Mutex<Vec<Tracked>> = Mutex::new(Vec::new());

fn take_tracked(path: &Path) -> Option<Tracked> {
    let mut all = TRACKED.lock().unwrap_or_else(|e| e.into_inner());
    let at = all.iter().position(|t| t.path == path)?;
    Some(all.remove(at))
}

fn keep_tracked(t: Tracked) {
    let mut all = TRACKED.lock().unwrap_or_else(|e| e.into_inner());
    all.retain(|o| o.path != t.path);
    if all.len() >= TRACKED_MAX {
        all.remove(0);
    }
    all.push(t);
}

// A fingerprint of a message. `edges` hashes, for each string, its address,
// its length and up to EDGE bytes from each end; otherwise every byte and no
// address, so equal messages have equal prints wherever they live.
struct Print {
    h: u64,
    edges: bool,
}

const EDGE: usize = 64;

impl Print {
    fn of(m: &Msg, edges: bool) -> u64 {
        let mut p = Print {
            h: 0x9e37_79b9_7f4a_7c15,
            edges,
        };
        p.msg(m);
        p.h
    }

    fn word(&mut self, w: u64) {
        self.h = (self.h.rotate_left(5) ^ w).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    fn raw(&mut self, b: &[u8]) {
        let (chunks, rest) = b.as_chunks::<8>();
        for c in chunks {
            self.word(u64::from_le_bytes(*c));
        }
        let mut last = [0u8; 8];
        last[..rest.len()].copy_from_slice(rest);
        self.word(u64::from_le_bytes(last));
    }

    fn str(&mut self, s: &str) {
        let b = s.as_bytes();
        self.word(b.len() as u64);
        if !self.edges {
            self.raw(b);
        } else {
            self.word(b.as_ptr() as usize as u64);
            if b.len() <= 2 * EDGE {
                self.raw(b);
            } else {
                self.raw(&b[..EDGE]);
                self.raw(&b[b.len() - EDGE..]);
            }
        }
    }

    fn images(&mut self, images: &[(String, String)]) {
        self.word(images.len() as u64);
        for (kind, data) in images {
            self.str(kind);
            self.str(data);
        }
    }

    fn json(&mut self, v: &serde_json::Value) {
        use serde_json::Value;
        match v {
            Value::Null => self.word(0),
            Value::Bool(b) => self.word(1 + u64::from(*b)),
            Value::Number(n) => {
                self.word(3);
                self.word(n.as_f64().map_or(0, f64::to_bits));
                self.word(n.as_i64().map_or(0, |i| i as u64) ^ n.as_u64().unwrap_or(0));
            }
            Value::String(s) => {
                self.word(4);
                self.str(s);
            }
            Value::Array(a) => {
                self.word(5);
                self.word(a.len() as u64);
                for v in a {
                    self.json(v);
                }
            }
            Value::Object(o) => {
                self.word(6);
                self.word(o.len() as u64);
                for (k, v) in o {
                    self.str(k);
                    self.json(v);
                }
            }
        }
    }

    fn msg(&mut self, m: &Msg) {
        match m {
            Msg::System(t) => {
                self.word(1);
                self.str(t);
            }
            Msg::User(t) => {
                self.word(2);
                self.str(t);
            }
            Msg::UserImages { text, images } => {
                self.word(3);
                self.str(text);
                self.images(images);
            }
            Msg::Assistant { text, calls } => {
                self.word(4);
                self.str(text);
                self.word(calls.len() as u64);
                for c in calls {
                    self.str(&c.id);
                    self.str(&c.name);
                    self.json(&c.input);
                }
            }
            Msg::Tool(results) => {
                self.word(5);
                self.word(results.len() as u64);
                for r in results {
                    self.str(&r.id);
                    self.str(&r.content);
                    self.word(u64::from(r.is_error));
                    self.images(&r.images);
                }
            }
        }
    }
}

impl Tracked {
    // Whether the file's messages are still the first messages of `msgs`.
    fn still_leads(&mut self, msgs: &[Msg], check: Check) -> bool {
        if msgs.len() < self.prints.len() {
            return false;
        }
        for (m, (edges, full)) in msgs.iter().zip(&mut self.prints) {
            let now = Print::of(m, true);
            if check == Check::Edges && now == *edges {
                continue;
            }
            if Print::of(m, false) != *full {
                return false;
            }
            // Same message at a new address (the transcript was rebuilt).
            *edges = now;
        }
        true
    }

    // Appends the messages the file does not hold yet: one write for the
    // records (so two processes on one session never interleave halves),
    // the header's count and time brought up to date in place, one fsync.
    fn append(&mut self, msgs: &[Msg]) -> Result<(), String> {
        let new = &msgs[self.prints.len()..];
        if new.is_empty() {
            return Ok(());
        }
        let io = |e: std::io::Error| e.to_string();
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(io)?;
        let mut head = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(io)?;
        // Both handles must be the file this process left: the header is
        // patched at an offset that is only right for that file.
        if Stamp::of(&log.metadata().map_err(io)?) != self.stamp
            || Stamp::of(&head.metadata().map_err(io)?) != self.stamp
        {
            return Err("the session file changed underneath".into());
        }
        let tail_at = self.header_len - HEADER_TAIL_LEN;
        let mut tail = [0u8; HEADER_TAIL_LEN as usize];
        head.seek(SeekFrom::Start(tail_at)).map_err(io)?;
        head.read_exact(&mut tail).map_err(io)?;
        if !tail.starts_with(b",\"updated_ms\":") || !tail.ends_with(b"}\n") {
            return Err("the session file's header is not the one written".into());
        }

        let mut records = Vec::new();
        let mut prints = Vec::with_capacity(new.len());
        for m in new {
            push_record(&mut records, m)?;
            prints.push((Print::of(m, true), Print::of(m, false)));
        }
        log.write_all(&records).map_err(io)?;
        let total = self.prints.len() + new.len();
        head.seek(SeekFrom::Start(tail_at)).map_err(io)?;
        head.write_all(header_tail(now_ms(), total).as_bytes())
            .map_err(io)?;
        // The records are on disk before this returns, like every save
        // before the log: a crash loses at most the round being written.
        log.sync_data().map_err(io)?;
        self.stamp = Stamp::of(&log.metadata().map_err(io)?);
        self.prints.extend(prints);
        Ok(())
    }
}

fn save_now(id: &str, cwd: &Path, model: &str, msgs: &[Msg], check: Check) -> Result<(), String> {
    if msgs.is_empty() {
        return Ok(());
    }
    let path = path(id);
    let title = title_of(msgs);
    let cwd = cwd.to_string_lossy();
    let mut kept = None;
    if let Some(mut t) = take_tracked(&path) {
        if t.title == title
            && t.cwd == cwd
            && t.model == model
            && t.still_leads(msgs, check)
            && t.append(msgs).is_ok()
        {
            keep_tracked(t);
            return Ok(());
        }
        // History changed, the file did, or the append failed part-way:
        // writing the whole file again puts every one of those right.
        kept = Some((t.created_ms, t.name));
    }
    let _ = std::fs::create_dir_all(dir());
    // The file's own header wins: another process may have renamed the
    // session since this one last wrote it.
    let before = match read_info(&path) {
        Some(Ok(info)) => Some((info.created_ms, info.name)),
        _ => kept,
    };
    let (created_ms, name) = before.unwrap_or_else(|| (now_ms(), None));
    let header = Header {
        schema_version: SCHEMA_VERSION,
        id: id.to_string(),
        title,
        cwd: cwd.into_owned(),
        model: model.to_string(),
        created_ms,
        name,
        updated_ms: now_ms(),
        messages: msgs.len(),
    };
    write_whole(&path, header, msgs).map(keep_tracked)
}

// Writes the whole file: to a temp file, flushed, then renamed over the old
// one, so a crash leaves either the old file or the new one, never a mix
// (and a schema 1 file is replaced only by a complete schema 2 one).
fn write_whole(path: &Path, header: Header, msgs: &[Msg]) -> Result<Tracked, String> {
    let line = header_line(&header)?;
    let mut text = line.clone().into_bytes();
    let mut prints = Vec::with_capacity(msgs.len());
    for m in msgs {
        push_record(&mut text, m)?;
        prints.push((Print::of(m, true), Print::of(m, false)));
    }
    // list() only picks up `.json` files, so the temp file is invisible
    // even if a crash leaves it behind.
    crate::config::atomic_write(path, &text, crate::config::Perm::Default)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    let stamp = std::fs::metadata(path)
        .map(|m| Stamp::of(&m))
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(Tracked {
        path: path.to_path_buf(),
        title: header.title,
        cwd: header.cwd,
        model: header.model,
        created_ms: header.created_ms,
        name: header.name,
        header_len: line.len() as u64,
        stamp,
        prints,
    })
}

/// Names a saved session (/rename). The name replaces the first prompt in
/// listings and survives later saves.
pub fn rename(id: &str, name: &str) -> Result<(), String> {
    let Some(s) = load(id) else {
        return Err("nothing to name yet — the session is saved after its first message".into());
    };
    let name = name.trim();
    let path = path(id);
    // The header changes length, so the file is written again; the next
    // save reads the name back from it.
    let _ = take_tracked(&path);
    let header = Header {
        schema_version: SCHEMA_VERSION,
        id: s.id,
        title: s.title,
        cwd: s.cwd,
        model: s.model,
        created_ms: s.created_ms,
        name: (!name.is_empty()).then(|| name.chars().take(80).collect()),
        updated_ms: s.updated_ms,
        messages: s.msgs.len(),
    };
    write_whole(&path, header, &s.msgs).map(|_| ())
}

/// Deletes a saved session; returns it so the caller can say which.
pub fn remove(id: &str) -> Result<Session, String> {
    let Some(s) = load(id) else {
        return Err(format!("no session '{id}' — bwn sessions lists them"));
    };
    let _ = take_tracked(&path(id));
    std::fs::remove_file(path(id)).map_err(|e| format!("could not delete {id}: {e}"))?;
    Ok(s)
}

// A session file that exists but fails to parse is data the user thinks is
// saved — surface it instead of silently dropping it from /resume listings.
fn warn_corrupt(path: &Path, e: &str) {
    // tui::line, not eprintln — this runs inside the alt-screen TUI
    // during /resume and a raw write corrupts the display.
    crate::tui::line(&crate::tui::yellow(&format!(
        "  ⚠ skipping corrupt session file {}: {e}",
        path.display()
    )));
}

fn newer_schema(v: u32) -> String {
    format!(
        "it was saved by a newer bwn (session schema {v}; this one reads up to {SCHEMA_VERSION})"
    )
}

// The whole session from a file's text, in either layout. In the log, a
// last line that is not a whole record is what a crash mid-append leaves:
// it is dropped and every record before it kept.
fn parse_session(text: &str) -> Result<Session, String> {
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    let Ok(h) = serde_json::from_str::<Header>(first) else {
        return serde_json::from_str::<Session>(text).map_err(|e| e.to_string());
    };
    if h.schema_version > SCHEMA_VERSION {
        return Err(newer_schema(h.schema_version));
    }
    let mut msgs = Vec::with_capacity(h.messages.min(1 << 16));
    let mut lines = rest.split('\n').enumerate().peekable();
    while let Some((i, line)) = lines.next() {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Record>(line) {
            Ok(r) => msgs.extend(r.msg),
            // Nothing but blank lines after it: the torn end of the file.
            Err(_) if lines.clone().all(|(_, l)| l.is_empty()) => break,
            Err(e) => return Err(format!("line {}: {e}", i + 2)),
        }
    }
    Ok(Session {
        schema_version: h.schema_version,
        id: h.id,
        title: h.title,
        cwd: h.cwd,
        model: h.model,
        created_ms: h.created_ms,
        updated_ms: h.updated_ms,
        msgs,
        name: h.name,
    })
}

/// Loads a persisted session by its ID from disk, if it exists and is valid.
/// Warns in the transcript if the file exists but cannot be parsed.
pub fn load(id: &str) -> Option<Session> {
    let path = path(id);
    let text = std::fs::read_to_string(&path).ok()?;
    count_read(text.len());
    match parse_session(&text) {
        Ok(s) => Some(s),
        Err(e) => {
            warn_corrupt(&path, &e);
            None
        }
    }
}

// Bytes this thread has read from session files, so a test can hold a
// listing to its headers.
#[cfg(test)]
thread_local! {
    static BYTES_READ: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn count_read(_n: usize) {
    #[cfg(test)]
    BYTES_READ.with(|b| b.set(b.get() + _n));
}

// What a listing shows for the session file at `path`, from its header line
// alone; a schema 1 file is one object and is scanned whole. None when the
// file cannot be read, Err when it is not a session.
fn read_info(path: &Path) -> Option<Result<SessionInfo, String>> {
    // A header is a few hundred bytes: one small read finds its end.
    const FIRST_READ: usize = 2048;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; FIRST_READ];
    let mut n = 0;
    while n < FIRST_READ {
        match f.read(&mut buf[n..]).ok()? {
            0 => break,
            got => n += got,
        }
    }
    buf.truncate(n);
    count_read(n);
    let header = |buf: &[u8]| {
        let end = buf.iter().position(|b| *b == b'\n').unwrap_or(buf.len());
        serde_json::from_slice::<Header>(&buf[..end]).ok()
    };
    let mut found = header(&buf);
    if found.is_none() && n == FIRST_READ {
        // A header longer than the first read (a very long folder name),
        // or a schema 1 file.
        let more = f.read_to_end(&mut buf).ok()?;
        count_read(more);
        found = header(&buf);
    }
    Some(match found {
        Some(h) if h.schema_version > SCHEMA_VERSION => Err(newer_schema(h.schema_version)),
        Some(h) => Ok(SessionInfo {
            id: h.id,
            title: h.title,
            cwd: h.cwd,
            model: h.model,
            created_ms: h.created_ms,
            updated_ms: h.updated_ms,
            msg_count: h.messages,
            name: h.name,
        }),
        None => serde_json::from_slice::<SessionInfo>(&buf).map_err(|e| e.to_string()),
    })
}

/// Lists all persisted sessions, ordered newest-first by last update timestamp.
/// Only each file's header line is read. Corrupt session files are skipped
/// with a warning in the transcript.
pub fn list() -> Vec<SessionInfo> {
    let mut v: Vec<SessionInfo> = match std::fs::read_dir(dir()) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| {
                let path = e.path();
                match read_info(&path)? {
                    Ok(info) => Some(info),
                    Err(e) => {
                        warn_corrupt(&path, &e);
                        None
                    }
                }
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    v.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
    v
}

/// Returns the most recently updated session, or `None` if no sessions exist.
pub fn latest() -> Option<Session> {
    list().first().and_then(|s| load(&s.id))
}

/// The sessions started in `cwd`, newest first.
pub fn list_for(cwd: &Path) -> Vec<SessionInfo> {
    list().into_iter().filter(|s| s.is_in(cwd)).collect()
}

/// The newest session started in `cwd`.
pub fn latest_for(cwd: &Path) -> Option<Session> {
    list_for(cwd).first().and_then(|s| load(&s.id))
}

/// Every session, the ones started in `cwd` first; each group newest first.
pub fn list_here_first(cwd: &Path) -> Vec<SessionInfo> {
    let (mut here, others): (Vec<SessionInfo>, Vec<SessionInfo>) =
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
                        crate::report::plural(n)
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
    fn save_reports_a_failed_write() {
        with_home(|| {
            let msgs = [Msg::User("hi".into())];
            // A directory where the session file belongs: the rename fails.
            std::fs::create_dir_all(path("0000000000000010")).unwrap();
            assert!(!save("0000000000000010", Path::new("/p"), "m", &msgs));
            std::fs::remove_dir_all(path("0000000000000010")).unwrap();
            assert!(save("0000000000000010", Path::new("/p"), "m", &msgs));
            assert!(load("0000000000000010").is_some());
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
                    images: Vec::new(),
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
            let text = std::fs::read_to_string(path("0000000000000005")).unwrap();
            let header = text.lines().next().unwrap();
            let v: serde_json::Value = serde_json::from_str(header).unwrap();
            assert_eq!(v["schema_version"], SCHEMA_VERSION);
            assert_eq!(SCHEMA_VERSION, 2);
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
                path("0000000000000006"),
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

    // ── The log (schema 2) ──────────────────────────────────────────────────

    fn json(msgs: &[Msg]) -> String {
        serde_json::to_string(msgs).unwrap()
    }

    // What a tool round adds: the model's turn and the tools' answer.
    fn round(i: usize) -> [Msg; 2] {
        [
            Msg::Assistant {
                text: format!("reading module {i}"),
                calls: vec![crate::provider::ToolCall {
                    id: format!("c{i}"),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": format!("src/m{i}.rs"), "limit": 200}),
                }],
            },
            Msg::Tool(vec![crate::provider::ToolResult {
                id: format!("c{i}"),
                content: format!("// module {i}\n").repeat(40),
                is_error: false,
                images: Vec::new(),
            }]),
        ]
    }

    // As a bwn started later would find the session: nothing remembered.
    fn new_process(id: &str) {
        let _ = take_tracked(&path(id));
    }

    #[cfg(unix)]
    fn inode(id: &str) -> u64 {
        std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(path(id)).unwrap())
    }
    #[cfg(not(unix))]
    fn inode(_id: &str) -> u64 {
        0
    }

    fn file(id: &str) -> String {
        std::fs::read_to_string(path(id)).unwrap()
    }

    #[test]
    fn the_header_ends_in_a_tail_of_one_fixed_width() {
        assert_eq!(header_tail(0, 0).len() as u64, HEADER_TAIL_LEN);
        assert_eq!(
            header_tail(1_790_000_000_000, 412).len() as u64,
            HEADER_TAIL_LEN
        );
        assert_eq!(
            header_tail(u128::MAX, usize::MAX).len() as u64,
            HEADER_TAIL_LEN
        );
        let h = Header {
            schema_version: SCHEMA_VERSION,
            id: "i".into(),
            title: "t \"quoted\"\nline".into(),
            cwd: "/p".into(),
            model: "m".into(),
            created_ms: 7,
            name: Some("n".into()),
            updated_ms: 9,
            messages: 3,
        };
        let line = header_line(&h).unwrap();
        assert!(line.ends_with('\n') && line.matches('\n').count() == 1);
        let back: Header = serde_json::from_str(&line).unwrap();
        assert_eq!((back.updated_ms, back.messages, back.created_ms), (9, 3, 7));
        assert_eq!(back.name.as_deref(), Some("n"));
        assert_eq!(back.title, h.title);
    }

    #[test]
    fn a_schema_1_file_loads_lists_and_is_migrated_by_the_next_save() {
        with_home(|| {
            let id = "0000000000000020";
            let _ = std::fs::create_dir_all(dir());
            // As 0.19 wrote it: one object, every message inside.
            let old = r#"{"schema_version":1,"id":"0000000000000020","title":"old task","cwd":"/p","model":"m","created_ms":11,"updated_ms":22,"msgs":[{"System":"sys"},{"User":"old task"},{"Assistant":{"text":"done","calls":[]}}],"name":"kept name"}"#;
            std::fs::write(path(id), old).unwrap();

            let listed = list();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].msg_count, 3);
            assert_eq!(listed[0].label(), "kept name");
            let s = load(id).expect("a schema 1 file loads");
            assert_eq!((s.schema_version, s.msgs.len()), (1, 3));
            assert_eq!(file(id), old, "reading never rewrites it");

            // Resumed and saved: the same session, in the new layout.
            let mut msgs = s.msgs;
            msgs.push(Msg::User("and one more thing".into()));
            assert!(save(id, Path::new("/p"), "m", &msgs));
            let text = file(id);
            assert!(text.starts_with("{\"schema_version\":2,"), "{text}");
            assert_eq!(
                text.lines().count(),
                1 + 4,
                "a header, then a line a message"
            );
            let s = load(id).unwrap();
            assert_eq!(s.schema_version, 2);
            assert_eq!(json(&s.msgs), json(&msgs));
            assert_eq!(s.created_ms, 11, "the creation time is kept");
            assert_eq!(s.label(), "kept name");
            assert_eq!(s.title, "old task");
            assert_eq!(list()[0].msg_count, 4);
            // Replaced by a rename of a complete file: nothing is left over.
            let left: Vec<_> = std::fs::read_dir(dir()).unwrap().collect();
            assert_eq!(left.len(), 1);
        });
    }

    #[test]
    fn a_round_is_appended_and_the_whole_log_loads_back() {
        with_home(|| {
            let id = "0000000000000021";
            let cwd = Path::new("/p");
            let mut msgs = vec![Msg::System("sys".into()), Msg::User("fix it".into())];
            assert!(save(id, cwd, "m", &msgs));
            let (first_inode, mut len) = (inode(id), file(id).len());
            let created = load(id).unwrap().created_ms;
            // A screenshot, then forty rounds on top of it.
            let shot = "QUJD".repeat(50_000);
            msgs.push(Msg::UserImages {
                text: "see this".into(),
                images: vec![("image/png".into(), shot.clone())],
            });
            assert!(save_round(id, cwd, "m", &msgs));
            len = file(id).len().max(len);
            for i in 0..40 {
                msgs.extend(round(i));
                assert!(save_round(id, cwd, "m", &msgs));
                let now = file(id).len();
                let mut added = Vec::new();
                for m in &msgs[msgs.len() - 2..] {
                    push_record(&mut added, m).unwrap();
                }
                assert_eq!(
                    now - len,
                    added.len(),
                    "round {i} wrote its two records only"
                );
                len = now;
            }
            assert_eq!(
                inode(id),
                first_inode,
                "the file was appended to, never replaced"
            );
            let text = file(id);
            assert_eq!(text.matches(&shot).count(), 1, "the image is on disk once");
            assert!(text.ends_with("}\n"));
            assert_eq!(text.lines().count(), 1 + msgs.len());

            let s = load(id).unwrap();
            assert_eq!(json(&s.msgs), json(&msgs));
            assert_eq!(s.created_ms, created);
            assert!(s.updated_ms >= created);
            // The header was kept up to date in place.
            let info = &list()[0];
            assert_eq!(info.msg_count, msgs.len());
            assert_eq!(info.updated_ms, s.updated_ms);
            // Nothing new: nothing written.
            assert!(save_round(id, cwd, "m", &msgs) && save(id, cwd, "m", &msgs));
            assert_eq!(file(id), text);
        });
    }

    #[test]
    fn a_torn_last_line_is_dropped_and_the_rest_kept() {
        with_home(|| {
            let id = "0000000000000022";
            let cwd = Path::new("/p");
            let mut msgs = vec![Msg::User("first".into())];
            save(id, cwd, "m", &msgs);
            for i in 0..3 {
                msgs.extend(round(i));
                save_round(id, cwd, "m", &msgs);
            }
            let whole = file(id);
            // A crash part-way through an append: half a record, no newline.
            for torn in [
                format!("{whole}{{\"msg\":{{\"User\":\"half a mess"),
                whole[..whole.len() - 9].to_string(),
                format!("{whole}\0\0\0\0\0\0\0\0"),
            ] {
                std::fs::write(path(id), &torn).unwrap();
                let s = load(id).expect("the rest of the log loads");
                let kept = if torn.len() < whole.len() {
                    msgs.len() - 1
                } else {
                    msgs.len()
                };
                assert_eq!(json(&s.msgs), json(&msgs[..kept]));
                assert_eq!(list().len(), 1, "and it is still listed");
            }
            // A broken line with records after it is damage, not a torn end.
            let mut lines: Vec<&str> = whole.lines().collect();
            lines[2] = "{\"msg\":{\"User\":\"half";
            std::fs::write(path(id), lines.join("\n") + "\n").unwrap();
            assert!(load(id).is_none());

            // Carrying on after a torn append writes the file whole again:
            // this process, whose stamp no longer matches…
            std::fs::write(path(id), format!("{whole}{{\"msg\":{{\"Us")).unwrap();
            msgs.extend(round(9));
            assert!(save_round(id, cwd, "m", &msgs));
            assert_eq!(json(&load(id).unwrap().msgs), json(&msgs));
            assert!(file(id).ends_with("}\n"));
            // …and a later one, resuming what loaded.
            let torn = file(id);
            std::fs::write(path(id), &torn[..torn.len() - 20]).unwrap();
            new_process(id);
            let mut resumed = load(id).unwrap().msgs;
            assert_eq!(resumed.len(), msgs.len() - 1);
            resumed.push(Msg::User("carry on".into()));
            assert!(save(id, cwd, "m", &resumed));
            assert_eq!(json(&load(id).unwrap().msgs), json(&resumed));
            for line in file(id).lines() {
                serde_json::from_str::<serde_json::Value>(line).expect("every line is whole");
            }
        });
    }

    #[test]
    fn rewritten_history_writes_the_file_again() {
        with_home(|| {
            let id = "0000000000000023";
            let cwd = Path::new("/p");
            let mut msgs = vec![Msg::System("sys".into()), Msg::User("big job".into())];
            save(id, cwd, "m", &msgs);
            for i in 0..6 {
                msgs.extend(round(i));
                save_round(id, cwd, "m", &msgs);
            }
            let created = load(id).unwrap().created_ms;

            // Compaction: the middle of the conversation becomes a summary.
            let tail = msgs.split_off(msgs.len() - 2);
            msgs.truncate(1);
            msgs.push(Msg::User("big job\n\n[summary of earlier work]".into()));
            msgs.extend(tail);
            assert!(save_round(id, cwd, "m", &msgs));
            let s = load(id).unwrap();
            assert_eq!(json(&s.msgs), json(&msgs));
            assert_eq!(file(id).lines().count(), 1 + msgs.len());
            assert_eq!(s.created_ms, created);
            assert_eq!(list()[0].msg_count, msgs.len());

            // An old tool result cleared where it lies (same count).
            msgs.extend(round(7));
            save_round(id, cwd, "m", &msgs);
            if let Msg::Tool(r) = &mut msgs[3] {
                r[0].content = "[cleared]".into();
            }
            msgs.extend(round(8));
            assert!(save_round(id, cwd, "m", &msgs));
            assert_eq!(json(&load(id).unwrap().msgs), json(&msgs));

            // The same messages rebuilt elsewhere in memory are not a
            // rewrite: the round is appended.
            let before = inode(id);
            let mut msgs = msgs.clone();
            msgs.extend(round(9));
            assert!(save_round(id, cwd, "m", &msgs));
            assert_eq!(inode(id), before);
            assert_eq!(json(&load(id).unwrap().msgs), json(&msgs));

            // A change the round check cannot see (same place, same length,
            // same edges) is put right by the save every turn ends with.
            let Msg::System(sys) = &mut msgs[0] else {
                panic!()
            };
            *sys = "s".repeat(400);
            save(id, cwd, "m", &msgs);
            let Msg::System(sys) = &mut msgs[0] else {
                panic!()
            };
            sys.replace_range(200..203, "XYZ");
            assert!(save(id, cwd, "m", &msgs));
            assert_eq!(json(&load(id).unwrap().msgs), json(&msgs));

            // A rewind drops messages; a new model is a new header.
            msgs.truncate(3);
            assert!(save_round(id, cwd, "m", &msgs));
            assert_eq!(load(id).unwrap().msgs.len(), 3);
            assert!(save_round(id, cwd, "other-model", &msgs));
            assert_eq!(load(id).unwrap().model, "other-model");
            assert_eq!(list()[0].model, "other-model");
            // A rename mid-session survives the rounds after it.
            rename(id, "the big job").unwrap();
            msgs.extend(round(10));
            assert!(save_round(id, cwd, "other-model", &msgs));
            let s = load(id).unwrap();
            assert_eq!(s.label(), "the big job");
            assert_eq!(json(&s.msgs), json(&msgs));
            assert_eq!(s.created_ms, created);
        });
    }

    #[test]
    fn a_resumed_session_is_appended_to_again() {
        with_home(|| {
            let id = "0000000000000024";
            let cwd = Path::new("/p");
            let mut msgs = vec![Msg::User("start".into())];
            save(id, cwd, "m", &msgs);
            msgs.extend(round(0));
            save_round(id, cwd, "m", &msgs);
            let created = load(id).unwrap().created_ms;

            // Another run: `bwn continue`.
            new_process(id);
            let s = load(id).unwrap();
            let mut msgs = s.msgs;
            msgs.push(Msg::User("next".into()));
            assert!(save(id, cwd, "m", &msgs));
            let after_resume = inode(id);
            for i in 1..4 {
                msgs.extend(round(i));
                assert!(save_round(id, cwd, "m", &msgs));
            }
            assert_eq!(inode(id), after_resume, "appended after the first save");
            let s = load(id).unwrap();
            assert_eq!(json(&s.msgs), json(&msgs));
            assert_eq!(s.created_ms, created);
            assert_eq!(list()[0].msg_count, msgs.len());
        });
    }

    #[test]
    fn two_runs_on_one_session_never_leave_half_a_record() {
        with_home(|| {
            let id = "0000000000000025";
            let cwd = Path::new("/p");
            let base = vec![Msg::User("shared".into())];
            save(id, cwd, "m", &base);
            // The first run's memory of the file, set aside while a second
            // run (which knows nothing) saves its own continuation.
            let first = take_tracked(&path(id)).unwrap();
            let mut theirs = base.clone();
            theirs.push(Msg::User("from the second run".into()));
            assert!(save(id, cwd, "m", &theirs));
            new_process(id);
            keep_tracked(first);
            // The first run's next round: the file is not the one it left,
            // so it writes its own view whole instead of appending.
            let mut ours = base.clone();
            ours.extend(round(0));
            assert!(save_round(id, cwd, "m", &ours));
            assert_eq!(json(&load(id).unwrap().msgs), json(&ours));
            for line in file(id).lines() {
                serde_json::from_str::<serde_json::Value>(line).expect("every line is whole");
            }
        });
    }

    #[test]
    fn a_failed_append_is_reported_and_not_called_saved() {
        with_home(|| {
            let id = "0000000000000026";
            let cwd = Path::new("/p");
            let mut msgs = vec![Msg::User("hi".into())];
            assert!(save(id, cwd, "m", &msgs));
            // The file goes away and a directory takes its place: neither
            // the append nor the rewrite can land.
            std::fs::remove_file(path(id)).unwrap();
            std::fs::create_dir_all(path(id)).unwrap();
            msgs.extend(round(0));
            assert!(!save_round(id, cwd, "m", &msgs));
            assert!(!last_save_ok());
            msgs.extend(round(1));
            assert!(!save_round(id, cwd, "m", &msgs));
            // Saving works again: everything since is written.
            std::fs::remove_dir_all(path(id)).unwrap();
            msgs.extend(round(2));
            assert!(save_round(id, cwd, "m", &msgs));
            assert!(last_save_ok());
            assert_eq!(json(&load(id).unwrap().msgs), json(&msgs));
        });
    }

    #[test]
    fn a_listing_reads_each_header_and_nothing_else() {
        with_home(|| {
            let cwd = Path::new("/p");
            let mut total = 0;
            for n in 0..3 {
                let id = format!("000000000000003{n}");
                let mut msgs = vec![Msg::User(format!("task {n}"))];
                save(&id, cwd, "m", &msgs);
                msgs.push(Msg::UserImages {
                    text: "a screenshot".into(),
                    images: vec![("image/png".into(), "QUJD".repeat(100_000))],
                });
                for i in 0..10 {
                    msgs.extend(round(i));
                    save_round(&id, cwd, "m", &msgs);
                }
                total += file(&id).len();
            }
            assert!(total > 1_200_000, "{total}");
            BYTES_READ.with(|b| b.set(0));
            let listed = list_here_first(cwd);
            let read = BYTES_READ.with(|b| b.get());
            assert_eq!(listed.len(), 3);
            assert!(listed.iter().all(|s| s.msg_count == 22 && s.is_in(cwd)));
            assert!(read <= 3 * 2048, "read {read} bytes of {total}");
            // Opening one reads that one.
            BYTES_READ.with(|b| b.set(0));
            assert_eq!(load("0000000000000031").unwrap().msgs.len(), 22);
            let read = BYTES_READ.with(|b| b.get());
            assert!(read > 400_000 && read < total / 2, "{read}");
        });
    }

    #[test]
    fn a_file_from_a_newer_bwn_is_named_as_such() {
        with_home(|| {
            let _ = std::fs::create_dir_all(dir());
            let line = header_line(&Header {
                schema_version: SCHEMA_VERSION + 1,
                id: "0000000000000040".into(),
                title: "t".into(),
                cwd: "/p".into(),
                model: "m".into(),
                created_ms: 1,
                name: None,
                updated_ms: 2,
                messages: 0,
            })
            .unwrap();
            std::fs::write(path("0000000000000040"), &line).unwrap();
            assert!(parse_session(&line).is_err_and(|e| e.contains("newer bwn")));
            assert!(load("0000000000000040").is_none());
            assert!(list().is_empty());
            // A record kind this version does not know is skipped.
            let text = format!(
                "{}{{\"msg\":{{\"User\":\"hi\"}}}}\n{{\"note\":\"later versions\"}}\n",
                line.replace("\"schema_version\":3", "\"schema_version\":2")
            );
            assert!(parse_session(&text).is_ok_and(|s| s.msgs.len() == 1));
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
