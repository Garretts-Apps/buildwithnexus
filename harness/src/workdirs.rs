// Extra working folders: `--add-dir <path>` and `/add-dir <path>`.
//
// A folder added for the session is a second root for the file tools: they
// may write in it as in the session's folder (sensitive paths still ask, and
// a link that leads out of it is still outside), searches and `@` completion
// cover it, the sandbox binds it writable, and helpers and workflows inherit
// it. Nothing in it is trusted: its settings, hooks, commands, agents and
// skills never load, and its AGENTS.md reaches the model only after a notice
// names it. The list lasts for this process; a resumed session starts
// without it.

use std::path::{Path, PathBuf};

use crate::config;
use crate::tools;

#[cfg(not(test))]
static DIRS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

// Unit tests run side by side in one process: each test thread keeps its own.
#[cfg(test)]
thread_local! {
    static DIRS: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(not(test))]
fn with_dirs<T>(f: impl FnOnce(&mut Vec<PathBuf>) -> T) -> T {
    f(&mut DIRS.lock().unwrap_or_else(|e| e.into_inner()))
}

#[cfg(test)]
fn with_dirs<T>(f: impl FnOnce(&mut Vec<PathBuf>) -> T) -> T {
    DIRS.with(|d| f(&mut d.borrow_mut()))
}

/// The added folders, canonical, in the order they were added.
pub fn list() -> Vec<PathBuf> {
    with_dirs(|d| d.clone())
}

pub fn count() -> usize {
    with_dirs(|d| d.len())
}

/// What `add` did with a folder.
#[derive(Debug, PartialEq)]
pub enum Added {
    New(PathBuf),
    /// Already reachable: inside the working folder or an added one.
    Covered(String),
}

/// Adds `raw` (relative to `cwd`, `~` expanded) for the rest of the session.
/// Refused: a path that is not an existing folder, the filesystem root, a
/// folder holding the home folder, bwn's own folder and credential stores.
pub fn add(raw: &str, cwd: &Path) -> Result<Added, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("name a folder to add".into());
    }
    let given = tools::resolve(cwd, raw);
    let dir = match given.canonicalize() {
        Ok(d) => d,
        Err(_) => return Err(format!("{} is missing", given.display())),
    };
    if !dir.is_dir() {
        return Err(format!("{} is not a folder", dir.display()));
    }
    too_wide(&dir)?;
    if !tools::escapes_cwd(&dir, cwd) {
        return Ok(Added::Covered(format!(
            "{} is already inside the working folder",
            dir.display()
        )));
    }
    with_dirs(|d| {
        if let Some(root) = d.iter().find(|r| !tools::escapes_cwd(&dir, r)) {
            return Ok(Added::Covered(format!(
                "{} is already added (inside {})",
                dir.display(),
                root.display()
            )));
        }
        d.push(dir.clone());
        Ok(Added::New(dir))
    })
}

// Folders whose write access would reach far past a project.
fn too_wide(dir: &Path) -> Result<(), String> {
    let shown = dir.display();
    if dir.parent().is_none() {
        return Err(format!(
            "{shown} is too wide to add: it is the filesystem root"
        ));
    }
    let canon = |p: PathBuf| p.canonicalize().unwrap_or(p);
    let user_home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(|h| canon(PathBuf::from(h)));
    if user_home.as_deref().is_some_and(|h| h.starts_with(dir)) {
        return Err(format!(
            "{shown} is too wide to add: it holds your home folder — add the project folder instead"
        ));
    }
    let nexus = canon(config::home());
    if nexus.starts_with(dir) || dir.starts_with(&nexus) {
        return Err(format!(
            "{shown} cannot be added: it holds bwn's own settings ({})",
            nexus.display()
        ));
    }
    if tools::is_sensitive(dir) {
        return Err(format!("{shown} cannot be added: it is a credential store"));
    }
    Ok(())
}

/// The added folder `p` lies in, if any (links resolved).
pub fn containing(p: &Path) -> Option<PathBuf> {
    list().into_iter().find(|r| !tools::escapes_cwd(p, r))
}

/// Instruction files of the added folders (`instruction_files` names, the
/// first present at each folder's top).
pub fn instruction_files(cwd: &Path) -> Vec<config::InstructionFile> {
    config::load_root_instructions(cwd, &list())
}

/// One line per added folder and per instruction file it brings, for the
/// start of a session or `/add-dir`.
pub fn notices(cwd: &Path) -> Vec<String> {
    let files = instruction_files(cwd);
    list()
        .into_iter()
        .flat_map(|dir| {
            let mut out = vec![format!(
                "also working in {} — the agent may read and change files there",
                dir.display()
            )];
            out.extend(files.iter().filter(|f| f.path.starts_with(&dir)).map(|f| {
                format!(
                    "instructions from added folder {}: {} (not reviewed)",
                    dir.display(),
                    f.label
                )
            }));
            out
        })
        .collect()
}

/// Heading of the system prompt section about added folders.
pub const PROMPT_HEAD: &str = "[Added working folders — read and write]";

/// The system prompt section naming the added folders and their
/// instruction files, or None without any. `compact` cuts the instructions
/// for small context windows.
pub fn prompt_section(cwd: &Path, compact: bool) -> Option<String> {
    let mut s = prompt_marker()?;
    let files = instruction_files(cwd);
    if !files.is_empty() {
        s.push_str(
            "\n[Instructions from added folders — not reviewed by the user]\n\
             They apply to files in their own folder only.\n",
        );
        for f in &files {
            let body = match f.content.char_indices().nth(500) {
                Some((cut, _)) if compact => format!("{}…", &f.content[..cut]),
                _ => f.content.clone(),
            };
            s.push_str(&format!("\n--- {} ---\n{body}\n", f.path.display()));
        }
    }
    Some(s)
}

/// The part of `prompt_section` that changes when a folder is added: the
/// heading and the folder list. A system prompt without it is stale.
pub fn prompt_marker() -> Option<String> {
    let dirs = list();
    if dirs.is_empty() {
        return None;
    }
    let mut s = format!(
        "{PROMPT_HEAD}\nBesides the workspace, the user added these folders for this session. \
         You may read, search and change files in them; use absolute paths:\n"
    );
    for d in &dirs {
        s.push_str(&format!("- {}\n", d.display()));
    }
    Some(s)
}

/// `--add-dir` flags that hand the folders to a child process (workflows).
pub fn child_args() -> Vec<String> {
    list()
        .into_iter()
        .map(|d| format!("--add-dir={}", d.display()))
        .collect()
}

#[cfg(test)]
pub(crate) fn clear() {
    with_dirs(|d| d.clear());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "bwn-workdirs-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p.canonicalize().unwrap()
    }

    #[test]
    fn a_folder_is_added_once_and_only_when_it_is_a_new_root() {
        clear();
        let cwd = tmpdir("cwd");
        let other = tmpdir("other");
        std::fs::create_dir_all(other.join("sub")).unwrap();
        std::fs::create_dir_all(cwd.join("inner")).unwrap();
        assert_eq!(
            add(&other.display().to_string(), &cwd),
            Ok(Added::New(other.clone()))
        );
        assert!(matches!(
            add(&other.display().to_string(), &cwd),
            Ok(Added::Covered(_))
        ));
        assert!(matches!(
            add(&other.join("sub").display().to_string(), &cwd),
            Ok(Added::Covered(_))
        ));
        assert!(matches!(add("inner", &cwd), Ok(Added::Covered(_))));
        assert_eq!(list(), std::slice::from_ref(&other));
        assert_eq!(containing(&other.join("sub/x.rs")), Some(other.clone()));
        assert_eq!(containing(&cwd.join("x.rs")), None);
        assert_eq!(child_args(), [format!("--add-dir={}", other.display())]);
        let marker = prompt_marker().unwrap();
        assert!(
            marker.starts_with(PROMPT_HEAD) && marker.contains(&format!("- {}", other.display()))
        );
        clear();
        assert!(prompt_marker().is_none());
    }

    #[test]
    fn missing_files_and_too_wide_folders_are_refused() {
        clear();
        let cwd = tmpdir("cwd2");
        std::fs::write(cwd.join("f.txt"), "x").unwrap();
        assert!(add("nope", &cwd).unwrap_err().contains("missing"));
        assert!(add("f.txt", &cwd).unwrap_err().contains("not a folder"));
        assert!(add("/", &cwd).unwrap_err().contains("filesystem root"));
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            if let Some(parent) = home
                .canonicalize()
                .ok()
                .and_then(|h| h.parent().map(Path::to_path_buf))
            {
                if parent.parent().is_some() {
                    let e = add(&parent.display().to_string(), &cwd).unwrap_err();
                    assert!(e.contains("home folder"), "{e}");
                }
            }
        }
        let ssh = tmpdir("keys").join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        assert!(add(&ssh.display().to_string(), &cwd)
            .unwrap_err()
            .contains("credential"));
        assert!(list().is_empty());
    }
}
