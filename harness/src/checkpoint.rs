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
    undo_all_since(cwd, since as u128).map_err(|_| {
        "the last agent turn made no file changes — use /undo latest, /undo <id>, or /undo all"
            .to_string()
    })
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
/// Stores the previous contents (or empty if the file did not exist) in `.buildwithnexus/checkpoints`.
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
    };
    let checkpoint_dir = dir(cwd);
    let _ = fs::create_dir_all(&checkpoint_dir);
    if let Ok(body) = serde_json::to_string_pretty(&cp) {
        // Atomic: a truncated snapshot would silently drop out of list() —
        // that's a recovery point lost exactly when recovery matters.
        let _ = write_atomic(&checkpoint_dir.join(format!("{id}.json")), body.as_bytes());
    }
}

// Same-directory temp + rename; a crash mid-write never leaves a partial file.
fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut name = match path.file_name() {
        Some(n) => n.to_os_string(),
        None => return Err(std::io::Error::other("path has no file name")),
    };
    name.push(format!(".tmp-{}", std::process::id()));
    let tmp = path.with_file_name(name);
    fs::write(&tmp, contents)?;
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

fn restore_one(cp: &Checkpoint) -> Result<(), String> {
    if cp.existed {
        if !cp.snapshotted {
            return Err(format!(
                "cannot restore {}: original contents were not snapshotted (file was too large or not valid UTF-8); refusing to overwrite",
                cp.path.display()
            ));
        }
        if let Some(parent) = cp.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        write_atomic(&cp.path, cp.content.as_bytes())
            .map_err(|e| format!("cannot restore {}: {e}", cp.path.display()))?;
    } else if cp.path.exists() {
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
    let all = list(cwd);
    let mut restored = Vec::new();
    for cp in all {
        if cp.created_ms >= since_ms {
            restore_one(&cp)?;
            let _ = fs::remove_file(dir(cwd).join(format!("{}.json", cp.id)));
            restored.push(cp);
        }
    }
    if restored.is_empty() {
        Err("no checkpoints found in that timeframe".into())
    } else {
        Ok(restored)
    }
}

/// Performs a hard rollback of the workspace using `git checkout -- .`, discarding all unstaged working directory changes.
pub fn git_rollback(cwd: &Path) -> Result<String, String> {
    let mut out = String::new();
    let st = std::process::Command::new("git")
        .args(["checkout", "--", "."])
        .current_dir(cwd)
        .output();
    if let Ok(o) = st {
        out.push_str(&String::from_utf8_lossy(&o.stdout));
        out.push_str(&String::from_utf8_lossy(&o.stderr));
    } else {
        return Err("git checkout failed".into());
    }
    let st2 = std::process::Command::new("git")
        .args(["clean", "-fd"])
        .current_dir(cwd)
        .output();
    if let Ok(o) = st2 {
        out.push_str(&String::from_utf8_lossy(&o.stdout));
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
