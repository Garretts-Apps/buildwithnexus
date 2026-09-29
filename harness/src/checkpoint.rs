use std::fs;
use std::path::{Path, PathBuf};
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
    TURN_START_MS.store(now_ms() as u64, std::sync::atomic::Ordering::Relaxed);
}

/// Restores every checkpoint recorded since the current agent turn began.
/// Newest-first restore order means a file edited several times in the turn
/// ends at its pre-turn contents.
pub fn undo_last_turn(cwd: &Path) -> Result<Vec<Checkpoint>, String> {
    let since = TURN_START_MS.load(std::sync::atomic::Ordering::Relaxed);
    if since == 0 {
        return Err(
            "no agent turn recorded in this session — use /undo latest, /undo <id>, or /undo all"
                .into(),
        );
    }
    let set: Vec<Checkpoint> = list(cwd)
        .into_iter()
        .filter(|cp| cp.created_ms >= since as u128)
        .collect();
    if set.is_empty() {
        return Err(
            "the last agent turn made no file changes — use /undo latest, /undo <id>, or /undo all"
                .into(),
        );
    }
    restore_set(cwd, set)
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
        return Err(format!(
            "cannot restore {}: original contents were not snapshotted (file was too large or not valid UTF-8); refusing to overwrite",
            cp.path.display()
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

/// Restores the most recently recorded checkpoint for the workspace, removing its checkpoint file.
pub fn undo_latest(cwd: &Path) -> Result<Checkpoint, String> {
    let Some(cp) = list(cwd).into_iter().next() else {
        return Err("no checkpoints for this directory".into());
    };
    restore_one(&cp)?;
    let _ = fs::remove_file(dir(cwd).join(format!("{}.json", cp.id)));
    Ok(cp)
}

/// Restores a specific checkpoint by its unique ID (`<timestamp>-<action>`), removing its checkpoint file.
pub fn undo_by_id(cwd: &Path, id: &str) -> Result<Checkpoint, String> {
    let all = list(cwd);
    let Some(cp) = all.into_iter().find(|c| c.id == id) else {
        return Err(format!("checkpoint id not found: {id}"));
    };
    restore_one(&cp)?;
    let _ = fs::remove_file(dir(cwd).join(format!("{}.json", cp.id)));
    Ok(cp)
}

/// Restores all checkpoints recorded at or after `since_ms`, rolling back multiple edits in reverse chronological order.
pub fn undo_all_since(cwd: &Path, since_ms: u128) -> Result<Vec<Checkpoint>, String> {
    let set: Vec<Checkpoint> = list(cwd)
        .into_iter()
        .filter(|cp| cp.created_ms >= since_ms)
        .collect();
    if set.is_empty() {
        return Err("no checkpoints found in that timeframe".into());
    }
    restore_set(cwd, set)
}

// All or nothing where it can be known up front: one unrestorable checkpoint
// refuses the whole set before any file is touched, so an undo never leaves
// the tree half old, half new. A write that still fails midway is reported
// per file rather than stopping the rest.
fn restore_set(cwd: &Path, set: Vec<Checkpoint>) -> Result<Vec<Checkpoint>, String> {
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
    let mut restored = Vec::new();
    let mut failed = Vec::new();
    for cp in set {
        match restore_one(&cp) {
            Ok(()) => {
                let _ = fs::remove_file(dir(cwd).join(format!("{}.json", cp.id)));
                restored.push(cp);
            }
            Err(e) => failed.push(e),
        }
    }
    if failed.is_empty() {
        return Ok(restored);
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
        mark_turn_start();
        let files: Vec<_> = (1..=3).map(|i| d.join(format!("f{i}.txt"))).collect();
        for (i, f) in files.iter().enumerate() {
            fs::write(f, format!("original {i}")).unwrap();
            record(&d, f, "edit_file");
            fs::write(f, format!("broken {i}")).unwrap();
        }
        record(&d, &files[0], "edit_file");
        fs::write(&files[0], "broken again 0").unwrap();

        let restored = undo_last_turn(&d).expect("turn undo must succeed");
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
        assert!(undo_last_turn(&d).is_err());
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

        let res = undo_by_id(&d, cp_id);
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

        let res = undo_by_id(&d, &cps[0].id);
        assert!(res.is_err(), "restore must refuse un-snapshotted content");
        assert!(res.unwrap_err().contains("not snapshotted"));
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
        assert!(undo_by_id(&d, &cps[0].id).is_ok());
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

        let err = undo_all_since(&d, 0).unwrap_err();
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

        let cp = undo_latest(&d).unwrap();
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

        undo_latest(&d).unwrap();
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

        let err = undo_latest(&d).unwrap_err();
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
