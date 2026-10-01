use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[cfg(not(test))]
use crate::config;

const MAX_SNAPSHOT_BYTES: u64 = 2 * 1024 * 1024;

/// Represents a file modification snapshot recorded prior to an edit tool operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub cwd: PathBuf,
    pub path: PathBuf,
    pub action: String,
    pub created_ms: u128,
    pub existed: bool,
    pub content: String,
    /// Process-wide sequence number, part of the id so several checkpoints in
    /// the same millisecond (a fast multi-file batch) can never collide, and
    /// the tiebreaker for restore ordering. Old files default to 0.
    #[serde(default)]
    pub seq: u64,
    /// False when the original contents could not be captured (file too large
    /// or not valid UTF-8). Restore refuses to overwrite such files rather than
    /// clobbering them with an empty string. Defaults to true so checkpoint
    /// files written before this field existed keep restoring as before.
    #[serde(default = "default_snapshotted")]
    pub snapshotted: bool,
    /// Unix permission bits of the original file, so restore does not leave an
    /// executable script non-executable. None on other platforms and in
    /// checkpoints written before this field existed.
    #[serde(default)]
    pub mode: Option<u32>,
    /// What the agent's turn left the file as (see `fingerprint`), written
    /// when the turn ends. Restore compares it with the file on disk, so an
    /// edit made by hand afterwards is never overwritten without asking.
    /// None in checkpoints written before 0.15 or by a turn that never ended;
    /// those restore as before.
    #[serde(default)]
    pub after: Option<String>,
    /// The task of the turn that made the change, shown by /checkpoints.
    #[serde(default)]
    pub task: Option<String>,
}

fn default_snapshotted() -> bool {
    true
}

#[cfg(not(test))]
fn dir(_cwd: &Path) -> PathBuf {
    config::home().join("checkpoints")
}

#[cfg(test)]
fn dir(cwd: &Path) -> PathBuf {
    std::env::temp_dir()
        .join("bwn-checkpoints-test")
        .join(sanitize_id_part(&cwd.to_string_lossy()))
}

// Set when a user prompt starts a top-level agent run. Bare /undo restores
// exactly the files that run touched — the recovery path for a partial
// multi-file edit (agent changed 3 files, broke 2, or Esc landed mid-batch).
static TURN_START_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn mark_turn_start() {
    mark_turn_start_at(now_ms());
}

fn mark_turn_start_at(ms: u128) {
    TURN_START_MS.store(ms as u64, std::sync::atomic::Ordering::Relaxed);
    if let Ok(mut c) = COMMITTED.lock() {
        c.clear();
    }
}

// Folders where bwn's /commit made a commit since the last agent turn, so
// /undo can say that commits are not something it undoes.
static COMMITTED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Records that /commit committed in `cwd`.
pub fn note_commit(cwd: &Path) {
    if let Ok(mut c) = COMMITTED.lock() {
        c.push(cwd.to_path_buf());
    }
}

/// Whether /commit committed in `cwd` since the last agent turn began.
pub fn committed_since_turn(cwd: &Path) -> bool {
    COMMITTED.lock().is_ok_and(|c| c.iter().any(|p| p == cwd))
}

/// What an undo restored, and the files it left alone because they changed
/// after the agent's edit and the person chose to keep their version.
#[derive(Debug, Default)]
pub struct Undone {
    pub restored: Vec<Checkpoint>,
    pub kept: Vec<PathBuf>,
}

// ── turns ─────────────────────────────────────────────────────────────────────
// The last agent turn in each folder is kept on disk, so a relaunched bwn
// can still offer to undo it, and /undo can say what a turn changed that
// checkpoints do not cover.

/// The last agent turn in a folder.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Turn {
    pub cwd: PathBuf,
    pub started_ms: u128,
    /// The task's first line.
    pub task: String,
    /// Files the turn changed that no checkpoint covers (shell commands),
    /// relative to the folder; filled in when the turn ends. Empty outside
    /// a git repository.
    #[serde(default)]
    pub untracked: Vec<String>,
    /// HEAD when the turn began, so /undo can tell a commit made since.
    #[serde(default)]
    pub head: Option<String>,
}

// Checkpoints kept per folder; pruning starts once a folder has PRUNE_AT,
// so it happens now and then rather than on every turn.
pub const KEEP_CHECKPOINTS: usize = 500;
const PRUNE_AT: usize = 600;

// The turn now running: its task (stamped on each checkpoint) and the git
// state it started from, by folder.
static CURRENT_TASK: Mutex<Option<String>> = Mutex::new(None);
static TURN_SNAPSHOTS: Mutex<Vec<(PathBuf, Option<GitSnap>)>> = Mutex::new(Vec::new());
// Turns this process began, by folder: a relaunch must ask before undoing.
static OWN_TURNS: Mutex<Vec<(PathBuf, u128)>> = Mutex::new(Vec::new());
// Folders already pruned by this process.
static PRUNED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn turn_file(cwd: &Path) -> PathBuf {
    let key = cwd.to_string_lossy();
    let mut hash = FNV_OFFSET;
    for b in key.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    dir(cwd).join("turns").join(format!("{hash:016x}.json"))
}

fn save_turn(turn: &Turn) {
    let path = turn_file(&turn.cwd);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(body) = serde_json::to_string_pretty(turn) {
        let _ = write_atomic(&path, body.as_bytes());
    }
}

fn load_turn(cwd: &Path) -> Option<Turn> {
    let text = fs::read_to_string(turn_file(cwd)).ok()?;
    serde_json::from_str::<Turn>(&text)
        .ok()
        .filter(|t| t.cwd == cwd)
}

/// Starts an agent turn in `cwd`: records it on disk, notes the git state
/// so shell changes can be told apart, and prunes this folder's oldest
/// checkpoints once per process. Returns how many were pruned.
pub fn begin_turn(cwd: &Path, task: &str) -> usize {
    let started = now_ms();
    mark_turn_start_at(started);
    let label: String = task
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(80)
        .collect();
    if let Ok(mut t) = CURRENT_TASK.lock() {
        *t = Some(label.clone());
    }
    let pruned = prune_once(cwd);
    let snap = git_snapshot(cwd);
    save_turn(&Turn {
        cwd: cwd.to_path_buf(),
        started_ms: started,
        task: label,
        untracked: Vec::new(),
        head: snap.as_ref().and_then(|s| s.head.clone()),
    });
    if let Ok(mut own) = OWN_TURNS.lock() {
        own.retain(|(c, _)| c != cwd);
        own.push((cwd.to_path_buf(), started));
    }
    if let Ok(mut snaps) = TURN_SNAPSHOTS.lock() {
        snaps.retain(|(c, _)| c != cwd);
        snaps.push((cwd.to_path_buf(), snap));
    }
    pruned
}

/// Ends the turn begun in `cwd`: records what the agent left each file as
/// (see `changed_after_agent`) and which changed files no checkpoint covers.
pub fn end_turn(cwd: &Path) {
    if let Ok(mut t) = CURRENT_TASK.lock() {
        *t = None;
    }
    let Some(mut turn) = load_turn(cwd) else {
        return;
    };
    seal_since(cwd, turn.started_ms);
    let before = TURN_SNAPSHOTS.lock().ok().and_then(|mut snaps| {
        let i = snaps.iter().position(|(c, _)| c == cwd)?;
        snaps.swap_remove(i).1
    });
    if let (Some(before), Some(after)) = (before, git_snapshot(cwd)) {
        let covered: HashSet<PathBuf> = list(cwd)
            .into_iter()
            .filter(|cp| cp.created_ms >= turn.started_ms)
            .map(|cp| normalized(&cp.path))
            .collect();
        // A new folder counts as covered when checkpoints hold files in it.
        turn.untracked = changed_between(&before, &after)
            .into_iter()
            .map(|rel| after.top.join(rel))
            .filter(|p| {
                let p = normalized(p);
                !covered.iter().any(|c| c.starts_with(&p))
            })
            .map(|p| shown_path(&p, cwd))
            .collect();
    }
    save_turn(&turn);
}

/// The last agent turn in `cwd`, with the checkpoints it recorded.
pub struct LastTurn {
    pub turn: Turn,
    pub checkpoints: Vec<Checkpoint>,
    /// Begun by this process; otherwise it is from an earlier run.
    pub this_session: bool,
    /// HEAD moved since the turn began: a commit /undo will not undo.
    pub head_moved: bool,
}

pub fn last_turn(cwd: &Path) -> Option<LastTurn> {
    let turn = load_turn(cwd)?;
    let checkpoints = list(cwd)
        .into_iter()
        .filter(|cp| cp.created_ms >= turn.started_ms)
        .collect();
    let this_session = OWN_TURNS
        .lock()
        .is_ok_and(|own| own.iter().any(|(c, t)| c == cwd && *t == turn.started_ms));
    let head_moved = match &turn.head {
        Some(before) => git_head(cwd).is_some_and(|now| &now != before),
        None => false,
    };
    Some(LastTurn {
        turn,
        checkpoints,
        this_session,
        head_moved,
    })
}

/// Restores every checkpoint the last turn in `cwd` recorded. Newest-first
/// restore order means a file edited several times in the turn ends at its
/// pre-turn contents. A file changed after the turn ended is restored only
/// when `overwrite` says yes for it.
pub fn undo_last_turn(
    cwd: &Path,
    overwrite: &mut dyn FnMut(&Path) -> bool,
) -> Result<Undone, String> {
    let Some(last) = last_turn(cwd) else {
        return Err(
            "no agent turn recorded in this folder — /checkpoints lists what can be restored"
                .into(),
        );
    };
    if last.checkpoints.is_empty() {
        return Err(
            "the last agent turn made no file changes — use /undo latest, /undo <id>, or /undo all"
                .into(),
        );
    }
    restore_set(cwd, last.checkpoints, overwrite)
}

// Deletes this folder's oldest checkpoints beyond KEEP_CHECKPOINTS, once per
// process and only when the store has grown past PRUNE_AT files.
pub fn prune_once(cwd: &Path) -> usize {
    if let Ok(mut done) = PRUNED.lock() {
        if done.iter().any(|d| d == cwd) {
            return 0;
        }
        done.push(cwd.to_path_buf());
    }
    let files = fs::read_dir(dir(cwd)).map(|rd| rd.count()).unwrap_or(0);
    if files <= PRUNE_AT {
        return 0;
    }
    let all = list(cwd);
    if all.len() <= KEEP_CHECKPOINTS {
        return 0;
    }
    let mut pruned = 0;
    for cp in &all[KEEP_CHECKPOINTS..] {
        if fs::remove_file(dir(cwd).join(format!("{}.json", cp.id))).is_ok() {
            pruned += 1;
        }
    }
    pruned
}

// ── git state, for changes checkpoints do not cover ───────────────────────────
// Each dirty path's status and size and modification time: a path whose
// entry appears, changes or goes away during a turn was changed by it.
struct GitSnap {
    top: PathBuf,
    head: Option<String>,
    entries: HashMap<String, (String, Option<(u64, u128)>)>,
}

fn git_out(cwd: &Path, args: &[&str]) -> Option<String> {
    let o = std::process::Command::new("git")
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

// git honours the repository's own config, which can name programs to run;
// bwn only runs it itself when every repository-level key is inert.
fn git_allowed(cwd: &Path) -> bool {
    crate::tools::skips_prompt_safely("git status", cwd)
}

fn git_head(cwd: &Path) -> Option<String> {
    if !git_allowed(cwd) {
        return None;
    }
    git_out(cwd, &["rev-parse", "--verify", "-q", "HEAD"]).map(|h| h.trim().to_string())
}

fn git_snapshot(cwd: &Path) -> Option<GitSnap> {
    if !git_allowed(cwd) {
        return None;
    }
    let top = PathBuf::from(git_out(cwd, &["rev-parse", "--show-toplevel"])?.trim());
    let head = git_out(cwd, &["rev-parse", "--verify", "-q", "HEAD"]).map(|h| h.trim().to_string());
    let status = git_out(
        cwd,
        &["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
    )?;
    let mut entries = HashMap::new();
    let mut parts = status.split('\0');
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        let (xy, path) = (&entry[..2], &entry[3..]);
        if xy.contains(['R', 'C']) {
            parts.next(); // the rename's source path
        }
        let stamp = fs::metadata(top.join(path)).ok().map(|m| {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            (m.len(), mtime)
        });
        entries.insert(path.to_string(), (xy.to_string(), stamp));
    }
    Some(GitSnap { top, head, entries })
}

// Paths (relative to the repository) whose state differs between two
// snapshots, sorted.
fn changed_between(before: &GitSnap, after: &GitSnap) -> Vec<String> {
    let mut changed: Vec<String> = after
        .entries
        .iter()
        .filter(|(p, state)| before.entries.get(*p) != Some(*state))
        .map(|(p, _)| p.clone())
        .chain(
            before
                .entries
                .keys()
                .filter(|p| !after.entries.contains_key(*p))
                .cloned(),
        )
        .collect();
    changed.sort();
    changed.dedup();
    changed
}

// A path with its directory resolved, so a checkpoint's path and git's
// compare equal through symlinks such as /tmp on macOS.
fn normalized(p: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(p) {
        return real;
    }
    match (
        p.parent().and_then(|d| fs::canonicalize(d).ok()),
        p.file_name(),
    ) {
        (Some(dir), Some(name)) => dir.join(name),
        _ => p.to_path_buf(),
    }
}

/// `path` relative to `cwd` when it is inside it, as /undo and /checkpoints
/// show it.
pub fn shown_path(path: &Path, cwd: &Path) -> String {
    let real = normalized(path);
    let root = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    real.strip_prefix(&root)
        .or_else(|_| path.strip_prefix(cwd))
        .map(|r| r.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

/// The note an approval adds for a file that cannot be snapshotted, so the
/// person knows before saying yes that /undo will not restore it.
pub fn undo_note(paths: &[PathBuf]) -> Option<String> {
    let p = paths
        .iter()
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() > MAX_SNAPSHOT_BYTES))?;
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string());
    Some(format!(
        "{name} is too large to snapshot — /undo cannot restore it"
    ))
}

/// Records what the agent left each file as for checkpoints made since
/// `since_ms` (see `changed_after_agent`). `end_turn` does this for a task;
/// a conversational turn, which is not the folder's last task for bare
/// /undo, calls it directly.
pub fn seal_since(cwd: &Path, since_ms: u128) {
    for mut cp in list(cwd)
        .into_iter()
        .filter(|cp| cp.created_ms >= since_ms && cp.after.is_none())
    {
        cp.after = Some(fingerprint(&cp.path));
        if let Ok(body) = serde_json::to_string_pretty(&cp) {
            let _ = write_atomic(&dir(cwd).join(format!("{}.json", cp.id)), body.as_bytes());
        }
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A compact, stable description of a file's current state: `absent`,
/// `dir`, or its length and FNV-1a hash. Stable across releases (unlike
/// std's hasher), since it is compared with values saved by older runs.
pub(crate) fn fingerprint(path: &Path) -> String {
    use std::io::Read;
    let Ok(meta) = fs::metadata(path) else {
        return "absent".into();
    };
    if meta.is_dir() {
        return "dir".into();
    }
    let Ok(mut f) = fs::File::open(path) else {
        return format!("{}:unreadable", meta.len());
    };
    let (mut hash, mut len) = (FNV_OFFSET, 0u64);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                len += n as u64;
                hash = fnv(hash, &buf[..n]);
            }
            Err(_) => return format!("{}:unreadable", meta.len()),
        }
    }
    format!("{len}:{hash:016x}")
}

fn fnv(mut hash: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

// The file as the checkpoint found it, in `fingerprint`'s terms; None when
// its contents were not captured.
fn fingerprint_before(cp: &Checkpoint) -> Option<String> {
    if !cp.existed {
        return Some("absent".into());
    }
    cp.snapshotted.then(|| {
        let bytes = cp.content.as_bytes();
        format!("{}:{:016x}", bytes.len(), fnv(FNV_OFFSET, bytes))
    })
}

/// Files in `set` that changed after the agent last edited them: the file
/// on disk no longer matches what the newest checkpoint for that path
/// recorded when its turn ended, or, when `set` spans several turns, a
/// later turn found the file other than an earlier one left it (an edit by
/// hand in between, which restoring past the earlier turn would drop).
/// Checkpoints without that record (older files, an unfinished turn) are
/// never reported.
pub fn changed_after_agent(cwd: &Path, set: &[Checkpoint]) -> Vec<PathBuf> {
    let all = list(cwd);
    let mut seen = HashSet::new();
    let mut changed = Vec::new();
    for cp in set {
        if !seen.insert(cp.path.clone()) {
            continue;
        }
        let newest = all.iter().find(|c| c.path == cp.path).unwrap_or(cp);
        let after_newest = newest
            .after
            .as_ref()
            .is_some_and(|after| fingerprint(&cp.path) != *after);
        // `set` is newest first; each pair is an earlier edit and the next.
        let mut edits: Vec<&Checkpoint> = set.iter().filter(|c| c.path == cp.path).collect();
        edits.reverse();
        let between_turns = edits.windows(2).any(|w| match (&w[0].after, &w[1].after) {
            // Edits of one turn share the record of how it ended.
            (Some(left), Some(next)) if left != next => {
                fingerprint_before(w[1]).is_some_and(|found| found != *left)
            }
            _ => false,
        });
        if after_newest || between_turns {
            changed.push(cp.path.clone());
        }
    }
    changed
}

/// Returns the current Unix timestamp in milliseconds.
pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn sanitize_id_part(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Records a file modification checkpoint before an edit tool mutates `path`.
/// Stores the previous contents (or empty if the file did not exist) in `~/.buildwithnexus/checkpoints`.
pub fn record(cwd: &Path, path: &Path, action: &str) {
    let existed = path.exists();
    // Capture the previous contents. Oversized or non-UTF-8 files cannot be
    // snapshotted; mark them so restore never overwrites them with nothing.
    let (content, snapshotted) = if existed {
        match fs::metadata(path) {
            Ok(m) if m.len() <= MAX_SNAPSHOT_BYTES => match fs::read_to_string(path) {
                Ok(c) => (c, true),
                Err(_) => (String::new(), false),
            },
            _ => (String::new(), false),
        }
    } else {
        (String::new(), true)
    };
    let mode = if existed { file_mode(path) } else { None };
    let created_ms = now_ms();
    // A fast multi-file batch records several checkpoints in one millisecond;
    // a timestamp-only id made them overwrite each other on disk.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let id = format!("{}-{}-{}", created_ms, seq, sanitize_id_part(action));
    let cp = Checkpoint {
        id: id.clone(),
        cwd: cwd.to_path_buf(),
        path: path.to_path_buf(),
        action: action.to_string(),
        created_ms,
        existed,
        content,
        snapshotted,
        seq,
        mode,
        after: None,
        task: CURRENT_TASK.lock().ok().and_then(|t| t.clone()),
    };
    let checkpoint_dir = dir(cwd);
    let _ = fs::create_dir_all(&checkpoint_dir);
    if let Ok(body) = serde_json::to_string_pretty(&cp) {
        // Atomic: a truncated snapshot would silently drop out of list() —
        // that's a recovery point lost exactly when recovery matters.
        let _ = write_atomic(&checkpoint_dir.join(format!("{id}.json")), body.as_bytes());
    }
}

#[cfg(unix)]
fn file_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> Option<u32> {
    None
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

// Same-directory temp + rename; a crash mid-write never leaves a partial file.
fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    write_atomic_mode(path, contents, None)
}

fn write_atomic_mode(path: &Path, contents: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let mut name = match path.file_name() {
        Some(n) => n.to_os_string(),
        None => return Err(std::io::Error::other("path has no file name")),
    };
    name.push(format!(".tmp-{}", std::process::id()));
    let tmp = path.with_file_name(name);
    fs::write(&tmp, contents)?;
    if let Some(m) = mode {
        set_mode(&tmp, m).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
    }
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Returns all recorded checkpoints for the given workspace directory, sorted newest first.
pub fn list(cwd: &Path) -> Vec<Checkpoint> {
    let Ok(rd) = fs::read_dir(dir(cwd)) else {
        return Vec::new();
    };
    let mut items: Vec<Checkpoint> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| fs::read_to_string(e.path()).ok())
        .filter_map(|s| serde_json::from_str::<Checkpoint>(&s).ok())
        .filter(|cp| cp.cwd == cwd)
        .collect();
    items.sort_by_key(|cp| std::cmp::Reverse((cp.created_ms, cp.seq)));
    items
}

fn check_restorable(cp: &Checkpoint) -> Result<(), String> {
    if cp.existed && !cp.snapshotted {
        let shown = shown_path(&cp.path, &cp.cwd);
        return Err(format!(
            "cannot restore {shown}: it was not snapshotted (over 2 MiB or not UTF-8 text) — \
             `git checkout -- {shown}` restores the committed version"
        ));
    }
    Ok(())
}

// Where a restore writes: a symlink's target, so the rename in write_atomic
// does not replace the link itself with a regular file. canonicalize follows
// the whole chain; for a dangling link the literal target is still right.
fn write_target(path: &Path) -> PathBuf {
    let Ok(link) = fs::read_link(path) else {
        return path.to_path_buf();
    };
    let target = match path.parent() {
        Some(dir) if link.is_relative() => dir.join(link),
        _ => link,
    };
    fs::canonicalize(&target).unwrap_or(target)
}

// A link swapped in after the checkpoint (by an approved command) must not
// carry the restore outside the working tree.
fn inside_tree(target: &Path, cwd: &Path) -> bool {
    let Ok(root) = fs::canonicalize(cwd) else {
        return false;
    };
    let real = fs::canonicalize(target).or_else(|_| {
        let parent = target.parent().ok_or(std::io::ErrorKind::NotFound)?;
        let name = target.file_name().ok_or(std::io::ErrorKind::NotFound)?;
        fs::canonicalize(parent).map(|p| p.join(name))
    });
    real.is_ok_and(|r| r.starts_with(root))
}

fn restore_one(cp: &Checkpoint) -> Result<(), String> {
    check_restorable(cp)?;
    if cp.existed {
        let target = write_target(&cp.path);
        if target.parent().is_some_and(Path::exists) && !inside_tree(&target, &cp.cwd) {
            return Err(format!(
                "cannot restore {}: it now resolves outside the working tree ({})",
                cp.path.display(),
                target.display()
            ));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let mode = cp.mode.or_else(|| file_mode(&target));
        write_atomic_mode(&target, cp.content.as_bytes(), mode)
            .map_err(|e| format!("cannot restore {}: {e}", cp.path.display()))?;
    } else if cp.path.symlink_metadata().is_ok() {
        fs::remove_file(&cp.path)
            .map_err(|e| format!("cannot remove {}: {e}", cp.path.display()))?;
    }
    Ok(())
}

// A single restore asks before overwriting a file changed since the agent
// edited it; a "no" restores nothing and keeps the checkpoint.
fn restore_single(
    cwd: &Path,
    cp: &Checkpoint,
    overwrite: &mut dyn FnMut(&Path) -> bool,
) -> Result<(), String> {
    check_restorable(cp)?;
    if !changed_after_agent(cwd, std::slice::from_ref(cp)).is_empty() && !overwrite(&cp.path) {
        return Err(format!(
            "kept your version of {} — nothing restored",
            cp.path.display()
        ));
    }
    restore_one(cp)?;
    let _ = fs::remove_file(dir(cwd).join(format!("{}.json", cp.id)));
    Ok(())
}

/// Restores the most recently recorded checkpoint for the workspace, removing its checkpoint file.
pub fn undo_latest(
    cwd: &Path,
    overwrite: &mut dyn FnMut(&Path) -> bool,
) -> Result<Checkpoint, String> {
    let Some(cp) = list(cwd).into_iter().next() else {
        return Err("no checkpoints for this directory".into());
    };
    restore_single(cwd, &cp, overwrite)?;
    Ok(cp)
}

/// Restores a specific checkpoint by its unique ID (`<timestamp>-<action>`), removing its checkpoint file.
pub fn undo_by_id(
    cwd: &Path,
    id: &str,
    overwrite: &mut dyn FnMut(&Path) -> bool,
) -> Result<Checkpoint, String> {
    let all = list(cwd);
    let Some(cp) = all.into_iter().find(|c| c.id == id) else {
        return Err(format!("checkpoint id not found: {id}"));
    };
    restore_single(cwd, &cp, overwrite)?;
    Ok(cp)
}

/// Restores all checkpoints recorded at or after `since_ms`, rolling back multiple edits in reverse chronological order.
pub fn undo_all_since(
    cwd: &Path,
    since_ms: u128,
    overwrite: &mut dyn FnMut(&Path) -> bool,
) -> Result<Undone, String> {
    let set: Vec<Checkpoint> = list(cwd)
        .into_iter()
        .filter(|cp| cp.created_ms >= since_ms)
        .collect();
    if set.is_empty() {
        return Err("no checkpoints found in that timeframe".into());
    }
    restore_set(cwd, set, overwrite)
}

// All or nothing where it can be known up front: one unrestorable checkpoint
// refuses the whole set before any file is touched, so an undo never leaves
// the tree half old, half new. Files changed after the agent's edit are
// asked about one by one; a kept file keeps its checkpoints, and the rest of
// the set is still restored. A write that still fails midway is reported per
// file rather than stopping the rest.
fn restore_set(
    cwd: &Path,
    set: Vec<Checkpoint>,
    overwrite: &mut dyn FnMut(&Path) -> bool,
) -> Result<Undone, String> {
    let blocked: Vec<String> = set
        .iter()
        .filter_map(|cp| check_restorable(cp).err())
        .collect();
    if !blocked.is_empty() {
        return Err(format!(
            "nothing restored — {} checkpoint{} cannot be restored:\n  {}",
            blocked.len(),
            if blocked.len() == 1 { "" } else { "s" },
            blocked.join("\n  ")
        ));
    }
    let kept: Vec<PathBuf> = changed_after_agent(cwd, &set)
        .into_iter()
        .filter(|p| !overwrite(p))
        .collect();
    let mut restored = Vec::new();
    let mut failed = Vec::new();
    for cp in set.into_iter().filter(|cp| !kept.contains(&cp.path)) {
        match restore_one(&cp) {
            Ok(()) => {
                let _ = fs::remove_file(dir(cwd).join(format!("{}.json", cp.id)));
                restored.push(cp);
            }
            Err(e) => failed.push(e),
        }
    }
    if failed.is_empty() {
        return Ok(Undone { restored, kept });
    }
    let mut msg = format!(
        "{} restore{} failed:",
        failed.len(),
        if failed.len() == 1 { "" } else { "s" }
    );
    for e in &failed {
        msg.push_str(&format!("\n  ✗ {e}"));
    }
    for cp in &restored {
        msg.push_str(&format!(
            "\n  ✓ restored {} ({})",
            cp.path.display(),
            cp.action
        ));
    }
    Err(msg)
}

/// Performs a hard rollback of the workspace using `git checkout -- .`, discarding all unstaged working directory changes.
/// Untracked files are left alone: the confirmation prompt promises only
/// `git checkout -- .`, and `git clean` would delete work bwn never recorded.
pub fn git_rollback(cwd: &Path) -> Result<String, String> {
    let o = std::process::Command::new("git")
        .args(["checkout", "--", "."])
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    let mut out = String::from_utf8_lossy(&o.stdout).into_owned();
    out.push_str(&String::from_utf8_lossy(&o.stderr));
    if !o.status.success() {
        return Err(format!("git checkout -- . failed: {}", out.trim()));
    }
    Ok(if out.trim().is_empty() {
        "working tree reset cleanly".to_string()
    } else {
        out.trim().to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn undo_last_turn_reverts_a_partial_multi_file_batch() {
        let d = std::env::temp_dir().join(format!("bwn-cp-turn-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();

        // Pre-turn edit: must NOT be reverted by a last-turn undo.
        let earlier = d.join("earlier.txt");
        fs::write(&earlier, "old").unwrap();
        record(&d, &earlier, "edit_file");
        fs::write(&earlier, "edited before the turn").unwrap();

        // The agent turn begins, then edits three files and dies mid-batch —
        // one of them twice, so restore ordering matters.
        std::thread::sleep(std::time::Duration::from_millis(5));
        begin_turn(&d, "fix three files");
        let files: Vec<_> = (1..=3).map(|i| d.join(format!("f{i}.txt"))).collect();
        for (i, f) in files.iter().enumerate() {
            fs::write(f, format!("original {i}")).unwrap();
            record(&d, f, "edit_file");
            fs::write(f, format!("broken {i}")).unwrap();
        }
        record(&d, &files[0], "edit_file");
        fs::write(&files[0], "broken again 0").unwrap();

        let restored = undo_last_turn(&d, &mut |_| true)
            .expect("turn undo must succeed")
            .restored;
        assert_eq!(restored.len(), 4, "all in-turn checkpoints restored");
        for (i, f) in files.iter().enumerate() {
            assert_eq!(
                fs::read_to_string(f).unwrap(),
                format!("original {i}"),
                "file {i} back to pre-turn contents"
            );
        }
        assert_eq!(
            fs::read_to_string(&earlier).unwrap(),
            "edited before the turn",
            "pre-turn work is untouched"
        );

        // The turn's checkpoints are consumed; a second bare undo says so
        // instead of silently rewinding older history.
        assert!(undo_last_turn(&d, &mut |_| true).is_err());
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(dir(&d));
    }

    #[test]
    fn undo_asks_before_overwriting_a_hand_edit_made_after_the_agent() {
        let d = scratch("hand");
        let app = d.join("app.py");
        let test = d.join("test_app.py");
        fs::write(&app, "def main():\n").unwrap();
        fs::write(&test, "main()\n").unwrap();
        // The agent's turn: two edits, then the turn ends and is sealed.
        record(&d, &app, "edit_file");
        fs::write(&app, "def run():\n").unwrap();
        record(&d, &test, "edit_file");
        fs::write(&test, "run()\n").unwrap();
        seal_since(&d, 0);
        assert!(list(&d).iter().all(|cp| cp.after.is_some()));
        // Then I add a line by hand.
        fs::write(&app, "def run():\n# my own note\n").unwrap();
        assert_eq!(changed_after_agent(&d, &list(&d)), vec![app.clone()]);

        // "No": my line stays, the turn's other file is restored, and the
        // kept file's checkpoint is still there for later.
        let mut asked = Vec::new();
        let undone = undo_all_since(&d, 0, &mut |p| {
            asked.push(p.to_path_buf());
            false
        })
        .unwrap();
        assert_eq!(asked, vec![app.clone()], "asked once, about app.py only");
        assert_eq!(undone.kept, vec![app.clone()]);
        assert_eq!(undone.restored.len(), 1);
        assert_eq!(
            fs::read_to_string(&app).unwrap(),
            "def run():\n# my own note\n"
        );
        assert_eq!(fs::read_to_string(&test).unwrap(), "main()\n");
        assert_eq!(list(&d).len(), 1);

        // A single restore refuses without a yes, and consumes nothing.
        let err = undo_latest(&d, &mut |_| false).unwrap_err();
        assert!(err.contains("kept your version"), "{err}");
        assert_eq!(
            fs::read_to_string(&app).unwrap(),
            "def run():\n# my own note\n"
        );
        assert_eq!(list(&d).len(), 1);
        // "Yes" overwrites.
        undo_latest(&d, &mut |_| true).unwrap();
        assert_eq!(fs::read_to_string(&app).unwrap(), "def main():\n");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_hand_edit_between_two_agent_turns_is_asked_about() {
        let d = scratch("between");
        let app = d.join("app.py");
        fs::write(&app, "v0\n").unwrap();
        // Turn 1 edits app.py and ends.
        record(&d, &app, "edit_file");
        fs::write(&app, "v1\n").unwrap();
        seal_since(&d, 0);
        // I add a line by hand; then turn 2 edits app.py twice and ends.
        fs::write(&app, "v1\n# my note\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        let turn2 = now_ms();
        record(&d, &app, "edit_file");
        fs::write(&app, "v2a\n# my note\n").unwrap();
        record(&d, &app, "edit_file");
        fs::write(&app, "v2b\n# my note\n").unwrap();
        seal_since(&d, turn2);
        // Undoing turn 2 alone restores my line: nothing to ask.
        let turn: Vec<Checkpoint> = list(&d)
            .into_iter()
            .filter(|c| c.created_ms >= turn2)
            .collect();
        assert!(changed_after_agent(&d, &turn).is_empty());
        // Going back past turn 1 would drop it: asked, and no keeps it.
        assert_eq!(changed_after_agent(&d, &list(&d)), vec![app.clone()]);
        let undone = undo_all_since(&d, 0, &mut |_| false).unwrap();
        assert_eq!(undone.kept, vec![app.clone()]);
        assert_eq!(fs::read_to_string(&app).unwrap(), "v2b\n# my note\n");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn unchanged_files_and_unsealed_checkpoints_restore_without_asking() {
        let d = scratch("noask");
        let a = d.join("a.txt");
        fs::write(&a, "old").unwrap();
        record(&d, &a, "edit_file");
        fs::write(&a, "agent").unwrap();
        // Not sealed (a checkpoint from before 0.15): no record to compare.
        fs::write(&a, "agent, then me").unwrap();
        assert!(changed_after_agent(&d, &list(&d)).is_empty());
        seal_since(&d, 0);
        // Sealed now, and the file still matches: no question either.
        let mut asked = false;
        undo_all_since(&d, 0, &mut |_| {
            asked = true;
            false
        })
        .unwrap();
        assert!(!asked);
        assert_eq!(fs::read_to_string(&a).unwrap(), "old");

        // A file the agent deleted and I re-created counts as changed.
        let b = d.join("b.txt");
        fs::write(&b, "keep").unwrap();
        record(&d, &b, "remove_path");
        fs::remove_file(&b).unwrap();
        seal_since(&d, 0);
        assert_eq!(list(&d)[0].after.as_deref(), Some("absent"));
        fs::write(&b, "new by hand").unwrap();
        assert_eq!(changed_after_agent(&d, &list(&d)), vec![b.clone()]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn the_last_turn_is_kept_on_disk_for_a_relaunch() {
        let d = scratch("turnfile");
        let f = d.join("app.py");
        fs::write(&f, "v1").unwrap();
        begin_turn(&d, "rename main to run\n\n[attached files]\n…");
        record(&d, &f, "edit_file");
        fs::write(&f, "v2").unwrap();
        end_turn(&d);
        let last = last_turn(&d).expect("turn recorded");
        assert!(last.this_session);
        assert_eq!(last.turn.task, "rename main to run");
        assert_eq!(last.checkpoints.len(), 1);
        assert_eq!(
            last.checkpoints[0].task.as_deref(),
            Some("rename main to run")
        );
        // A relaunch: same files on disk, no memory of the turn.
        OWN_TURNS.lock().unwrap().retain(|(c, _)| c != &d);
        let last = last_turn(&d).expect("still on disk");
        assert!(
            !last.this_session,
            "an earlier run's turn is offered, not assumed"
        );
        let undone = undo_last_turn(&d, &mut |_| true).unwrap();
        assert_eq!(undone.restored.len(), 1);
        assert_eq!(fs::read_to_string(&f).unwrap(), "v1");
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(dir(&d));
    }

    #[test]
    fn a_turn_notes_files_changed_outside_checkpoints() {
        let d = scratch("shell");
        git(&d, &["init", "-q"]);
        fs::write(d.join("app.py"), "print('hi')\n").unwrap();
        fs::write(d.join("lib.py"), "x = 1\n").unwrap();
        git(&d, &["add", "."]);
        git(&d, &["commit", "-qm", "init"]);
        begin_turn(&d, "use sed to change the message");
        // A file tool edit (checkpointed) and a shell edit (not).
        record(&d, &d.join("lib.py"), "edit_file");
        fs::write(d.join("lib.py"), "x = 2\n").unwrap();
        fs::write(d.join("app.py"), "print('hello')\n").unwrap();
        fs::write(d.join("new.txt"), "made by a command\n").unwrap();
        // A new folder whose files were written by file tools is covered.
        fs::create_dir_all(d.join("pkg")).unwrap();
        record(&d, &d.join("pkg/core.py"), "write_file");
        fs::write(d.join("pkg/core.py"), "a = 1\n").unwrap();
        end_turn(&d);
        let last = last_turn(&d).unwrap();
        assert_eq!(last.turn.untracked, ["app.py", "new.txt"]);
        assert!(!last.head_moved);
        // A commit after the turn is noticed.
        git(&d, &["add", "."]);
        git(&d, &["commit", "-qm", "later"]);
        assert!(last_turn(&d).unwrap().head_moved);
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(dir(&d));
    }

    #[test]
    fn old_checkpoints_are_pruned_once_beyond_the_limit() {
        let d = scratch("prune");
        let f = d.join("f.txt");
        fs::write(&f, "x").unwrap();
        for _ in 0..PRUNE_AT + 5 {
            record(&d, &f, "edit_file");
        }
        let newest = list(&d)[0].id.clone();
        assert_eq!(prune_once(&d), PRUNE_AT + 5 - KEEP_CHECKPOINTS);
        let left = list(&d);
        assert_eq!(left.len(), KEEP_CHECKPOINTS);
        assert_eq!(left[0].id, newest, "the newest are kept");
        assert_eq!(prune_once(&d), 0, "once per process");
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(dir(&d));
    }

    #[test]
    fn approvals_note_files_too_large_to_snapshot() {
        let d = scratch("note");
        let big = d.join("big.log");
        fs::write(&big, vec![b'a'; MAX_SNAPSHOT_BYTES as usize + 1]).unwrap();
        let small = d.join("small.txt");
        fs::write(&small, "ok").unwrap();
        assert_eq!(
            undo_note(&[small.clone(), big]).as_deref(),
            Some("big.log is too large to snapshot — /undo cannot restore it")
        );
        assert!(undo_note(&[small, d.join("missing.txt")]).is_none());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn fingerprint_tells_contents_apart_and_is_stable() {
        let d = scratch("fp");
        let f = d.join("f.txt");
        assert_eq!(fingerprint(&f), "absent");
        assert_eq!(fingerprint(&d), "dir");
        fs::write(&f, "hello").unwrap();
        // FNV-1a 64 of "hello": fixed, so values saved by older runs compare.
        assert_eq!(fingerprint(&f), "5:a430d84680aabd0b");
        fs::write(&f, "hellp").unwrap();
        assert_ne!(fingerprint(&f), "5:a430d84680aabd0b");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn test_record_and_undo_by_id() {
        let d = std::env::temp_dir().join(format!("bwn-cp-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let file_path = d.join("test.txt");
        fs::write(&file_path, "initial content").unwrap();

        record(&d, &file_path, "edit_file");
        fs::write(&file_path, "modified content").unwrap();

        let cps = list(&d);
        assert!(!cps.is_empty());
        let cp_id = &cps[0].id;

        let res = undo_by_id(&d, cp_id, &mut |_| true);
        assert!(res.is_ok());
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "initial content");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn test_binary_file_is_not_snapshotted_and_restore_refuses() {
        let d = std::env::temp_dir().join(format!("bwn-cp-bin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let file_path = d.join("blob.bin");
        // Invalid UTF-8: read_to_string fails, so the content cannot be captured.
        fs::write(&file_path, [0xFF, 0xFE, 0x00, 0x9F]).unwrap();

        record(&d, &file_path, "edit_file");
        fs::write(&file_path, b"overwritten").unwrap();

        let cps = list(&d);
        assert!(!cps.is_empty());
        assert!(!cps[0].snapshotted);
        assert!(cps[0].content.is_empty());

        let res = undo_by_id(&d, &cps[0].id, &mut |_| true);
        assert!(res.is_err(), "restore must refuse un-snapshotted content");
        let err = res.unwrap_err();
        assert!(err.contains("not snapshotted"), "{err}");
        assert!(err.contains("`git checkout -- blob.bin`"), "{err}");
        // The real file must be untouched and the checkpoint not consumed.
        assert_eq!(fs::read(&file_path).unwrap(), b"overwritten");
        assert!(!list(&d).is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn test_oversized_file_is_not_snapshotted() {
        let d = std::env::temp_dir().join(format!("bwn-cp-big-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let file_path = d.join("big.txt");
        fs::write(&file_path, vec![b'a'; MAX_SNAPSHOT_BYTES as usize + 1]).unwrap();

        record(&d, &file_path, "edit_file");

        let cps = list(&d);
        assert!(!cps.is_empty());
        assert!(!cps[0].snapshotted);
        assert!(cps[0].content.is_empty());
        assert!(restore_one(&cps[0]).is_err());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn test_missing_file_checkpoint_still_restores_by_deletion() {
        let d = std::env::temp_dir().join(format!("bwn-cp-new-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let file_path = d.join("created.txt");

        record(&d, &file_path, "write_file"); // file does not exist yet
        fs::write(&file_path, "new content").unwrap();

        let cps = list(&d);
        assert!(cps[0].snapshotted); // nothing to capture, but state is complete
        assert!(undo_by_id(&d, &cps[0].id, &mut |_| true).is_ok());
        assert!(!file_path.exists());
        let _ = fs::remove_dir_all(&d);
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bwn-cp-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(dir(&d));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn undo_set_with_an_unrestorable_checkpoint_touches_nothing() {
        let d = scratch("partial");
        let text = d.join("a.txt");
        fs::write(&text, "original").unwrap();
        record(&d, &text, "edit_file");
        fs::write(&text, "edited").unwrap();
        let blob = d.join("b.bin");
        fs::write(&blob, [0xFF, 0xFE]).unwrap();
        record(&d, &blob, "edit_file");
        fs::write(&blob, b"edited").unwrap();

        let err = undo_all_since(&d, 0, &mut |_| true).unwrap_err();
        assert!(err.contains("nothing restored"), "got: {err}");
        assert!(err.contains("b.bin"), "names the blocker: {err}");
        assert_eq!(fs::read_to_string(&text).unwrap(), "edited");
        assert_eq!(list(&d).len(), 2, "no checkpoint consumed");
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn restore_preserves_the_original_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("mode");
        let f = d.join("run.sh");
        fs::write(&f, "#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o750)).unwrap();
        record(&d, &f, "edit_file");
        fs::write(&f, "broken").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o600)).unwrap();

        let cp = undo_latest(&d, &mut |_| true).unwrap();
        assert_eq!(cp.mode, Some(0o750));
        assert_eq!(fs::read_to_string(&f).unwrap(), "#!/bin/sh\necho hi\n");
        let mode = fs::metadata(&f).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o750);
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn restore_writes_through_a_symlink_to_its_target() {
        let d = scratch("link");
        let real = d.join("real.txt");
        let link = d.join("link.txt");
        fs::write(&real, "original").unwrap();
        std::os::unix::fs::symlink("real.txt", &link).unwrap();
        record(&d, &link, "edit_file");
        fs::write(&link, "edited").unwrap();

        undo_latest(&d, &mut |_| true).unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "original");
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn restore_refuses_a_link_that_now_points_outside_the_tree() {
        let d = scratch("escape");
        let outside = scratch("escape-target");
        let victim = outside.join("victim.txt");
        fs::write(&victim, "keep me").unwrap();
        let file = d.join("notes.txt");
        fs::write(&file, "original").unwrap();
        record(&d, &file, "edit_file");
        // An approved command later swaps the file for a link out of the tree.
        fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(&victim, &file).unwrap();

        let err = undo_latest(&d, &mut |_| true).unwrap_err();
        assert!(err.contains("outside the working tree"), "{err}");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep me");
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&outside);
    }

    fn git(d: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(d)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn git_rollback_keeps_untracked_files_and_reports_failure() {
        let d = scratch("git");
        git(&d, &["init", "-q"]);
        fs::write(d.join("tracked.txt"), "committed").unwrap();
        git(&d, &["add", "."]);
        git(&d, &["commit", "-qm", "init"]);
        fs::write(d.join("tracked.txt"), "edited").unwrap();
        fs::write(d.join("untracked.txt"), "new work").unwrap();

        git_rollback(&d).unwrap();
        assert_eq!(
            fs::read_to_string(d.join("tracked.txt")).unwrap(),
            "committed"
        );
        assert!(d.join("untracked.txt").exists(), "no git clean");

        // Not a repository (a .git pointing nowhere, so no parent repo is
        // found either): git exits non-zero and that must surface as Err.
        let outside = scratch("nogit");
        fs::write(outside.join(".git"), "gitdir: /nonexistent/bwn-test\n").unwrap();
        let err = git_rollback(&outside).unwrap_err();
        assert!(err.contains("failed"), "got: {err}");
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn test_pre_snapshotted_checkpoint_json_defaults_to_restorable() {
        // Checkpoint files written before the `snapshotted` field existed must
        // deserialize as restorable (serde default keeps old behavior).
        let j = r#"{"id":"1-edit","cwd":"/x","path":"/x/f","action":"edit",
                    "created_ms":1,"existed":true,"content":"hi"}"#;
        let cp: Checkpoint = serde_json::from_str(j).unwrap();
        assert!(cp.snapshotted);
        assert_eq!(cp.content, "hi");
    }
}
