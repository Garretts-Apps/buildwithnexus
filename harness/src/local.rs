// Local-model discovery for onboarding. Finds what's already installed so the
// setup walkthrough can offer real choices instead of a blank model prompt:
// Ollama's installed models (via its API, see provider::ollama_models) and
// GGUF files on disk for llama.cpp / LM Studio.

use std::path::{Path, PathBuf};

use crate::config;

// Where we tell users to drop GGUF files so the harness can find them.
pub fn models_dir() -> PathBuf {
    config::home().join("models")
}

// Directories scanned for *.gguf — our own models dir plus the common
// llama.cpp / LM Studio locations.
fn gguf_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![models_dir()];
    if let Ok(home) = std::env::var("HOME") {
        let h = PathBuf::from(home);
        dirs.push(h.join(".cache/lm-studio/models"));
        dirs.push(h.join(".lmstudio/models"));
        dirs.push(h.join(".local/share/models"));
    }
    dirs
}

// File names of every *.gguf found (deduped, sorted). Bounded recursion depth so
// a deep tree can't stall setup.
pub fn scan_gguf() -> Vec<String> {
    let mut found = Vec::new();
    for d in gguf_dirs() {
        collect_gguf(&d, 0, &mut found);
    }
    found.sort();
    found.dedup();
    found
}

fn collect_gguf(dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > 3 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_gguf(&p, depth + 1, out);
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("gguf")) {
            if let Some(name) = p.file_name() {
                out.push(name.to_string_lossy().into_owned());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_gguf_in_models_dir() {
        let _g = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("bwn-local-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("NEXUS_HOME", &home);
        let md = models_dir();
        std::fs::create_dir_all(md.join("sub")).unwrap();
        std::fs::write(md.join("qwen2.5-3b.gguf"), "").unwrap();
        std::fs::write(md.join("sub/llama3.2-1b.GGUF"), "").unwrap();
        std::fs::write(md.join("notes.txt"), "").unwrap();

        let found = scan_gguf();
        assert!(found.contains(&"qwen2.5-3b.gguf".to_string()));
        assert!(found.contains(&"llama3.2-1b.GGUF".to_string())); // case-insensitive ext
        assert!(!found.iter().any(|f| f.ends_with(".txt")));

        std::env::remove_var("NEXUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }
}
