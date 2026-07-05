use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[cfg(not(test))]
use crate::config;

const MAX_SNAPSHOT_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub cwd: PathBuf,
    pub path: PathBuf,
    pub action: String,
    pub created_ms: u128,
    pub existed: bool,
    pub content: String,
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

fn now_ms() -> u128 {
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

pub fn record(cwd: &Path, path: &Path, action: &str) {
    let existed = path.exists();
    let content = if existed {
        match fs::metadata(path) {
            Ok(m) if m.len() <= MAX_SNAPSHOT_BYTES => fs::read_to_string(path).unwrap_or_default(),
            _ => String::new(),
        }
    } else {
        String::new()
    };
    let created_ms = now_ms();
    let id = format!("{}-{}", created_ms, sanitize_id_part(action));
    let cp = Checkpoint {
        id: id.clone(),
        cwd: cwd.to_path_buf(),
        path: path.to_path_buf(),
        action: action.to_string(),
        created_ms,
        existed,
        content,
    };
    let checkpoint_dir = dir(cwd);
    let _ = fs::create_dir_all(&checkpoint_dir);
    if let Ok(body) = serde_json::to_string_pretty(&cp) {
        let _ = fs::write(checkpoint_dir.join(format!("{id}.json")), body);
    }
}

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
    items.sort_by_key(|cp| std::cmp::Reverse(cp.created_ms));
    items
}

pub fn undo_latest(cwd: &Path) -> Result<Checkpoint, String> {
    let Some(cp) = list(cwd).into_iter().next() else {
        return Err("no checkpoints for this directory".into());
    };
    if cp.existed {
        if let Some(parent) = cp.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        fs::write(&cp.path, &cp.content)
            .map_err(|e| format!("cannot restore {}: {e}", cp.path.display()))?;
    } else if cp.path.exists() {
        fs::remove_file(&cp.path)
            .map_err(|e| format!("cannot remove {}: {e}", cp.path.display()))?;
    }
    let _ = fs::remove_file(dir(cwd).join(format!("{}.json", cp.id)));
    Ok(cp)
}
