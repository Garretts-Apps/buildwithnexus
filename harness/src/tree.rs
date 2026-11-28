//! What the working tree holds, so verification sees every change, whatever
//! tool made it (roadmap 3.1; analysis V1, V2, V5): a file written with
//! `write_file`, edited with `sed` through `run_command`, deleted, or
//! changed by a helper all count.
//!
//! In a git work tree a snapshot is the set of paths `git status` reports
//! as changed or untracked, each with a hash of its contents (a file that
//! was already dirty and is edited again still reads `M`; its hash moves).
//! Outside git it is every file's size and modification time, from a walk
//! that skips what bwn's walkers skip. Past [`MAX_FILES`] the snapshot is
//! "too large", which never equals anything: verification runs. bwn's own
//! `.bwn/` (helper worktrees, `--worktree` sessions) is left out.

use std::collections::BTreeMap;
use std::path::Path;

/// Files a walk outside git looks at before giving up.
const MAX_FILES: usize = 20_000;

/// The working tree at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Snapshot {
    /// Relative path to a fingerprint of what is there ("-" when deleted).
    Files(BTreeMap<String, String>),
    /// Couldn't be taken, or too large to take: equal to nothing, so a
    /// comparison always says "changed" (fail closed).
    Unknown,
}

impl Snapshot {
    /// Whether `self` and `other` are the same tree. `Unknown` is never the
    /// same as anything.
    pub fn same_as(&self, other: &Snapshot) -> bool {
        match (self, other) {
            (Snapshot::Files(a), Snapshot::Files(b)) => a == b,
            _ => false,
        }
    }

    /// Paths that differ between `before` and `self`.
    pub fn changed_since(&self, before: &Snapshot) -> Vec<String> {
        match (before, self) {
            (Snapshot::Files(a), Snapshot::Files(b)) => {
                let mut out: Vec<String> = a
                    .keys()
                    .chain(b.keys())
                    .filter(|k| a.get(*k) != b.get(*k))
                    .cloned()
                    .collect();
                out.sort();
                out.dedup();
                out
            }
            _ => Vec::new(),
        }
    }
}

/// The files that differ between `before` and `now`, as paths: a git
/// key (`XY:rel`) is under its work tree's top, a walk key under its root.
pub fn changed_paths(before: &Snapshot, now: &Snapshot, cwd: &Path) -> Vec<std::path::PathBuf> {
    now.changed_since(before)
        .into_iter()
        .map(|k| {
            let (root, key) = match k.split_once('\u{0}') {
                Some((r, key)) => (std::path::PathBuf::from(r), key.to_string()),
                None => (cwd.to_path_buf(), k),
            };
            match crate::config::find_git_root(&root) {
                // Git keys carry a two-letter status; walk keys don't.
                Some(top) => top.join(key.get(3..).unwrap_or(&key)),
                None => root.join(key),
            }
        })
        .collect()
}

/// The working tree now: `cwd`'s (its whole git work tree when it is in
/// one) and every folder added to the session, which commands may change
/// too.
pub fn take(cwd: &Path) -> Snapshot {
    let mut all = BTreeMap::new();
    let roots = std::iter::once(cwd.to_path_buf()).chain(crate::workdirs::list());
    for (i, root) in roots.enumerate() {
        match take_one(&root) {
            Snapshot::Files(files) => {
                for (k, v) in files {
                    // The first root's keys stay as they are.
                    let key = if i == 0 {
                        k
                    } else {
                        format!("{}\u{0}{k}", root.display())
                    };
                    all.insert(key, v);
                }
            }
            Snapshot::Unknown => return Snapshot::Unknown,
        }
    }
    Snapshot::Files(all)
}

fn take_one(cwd: &Path) -> Snapshot {
    if crate::config::find_git_root(cwd).is_some() {
        git_snapshot(cwd).unwrap_or(Snapshot::Unknown)
    } else {
        walk_snapshot(cwd)
    }
}

fn excluded(rel: &str) -> bool {
    rel == ".bwn" || rel.starts_with(".bwn/")
}

fn git_snapshot(cwd: &Path) -> Option<Snapshot> {
    // Paths relative to cwd; no fsmonitor, no hooks (status runs none),
    // untracked files listed one by one.
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotePath=false",
            "status",
            "--porcelain=v1",
            "-z",
            // The whole work tree, not just under cwd: a command can
            // edit a file above it.
            "--untracked-files=all",
        ])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let top = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| std::path::PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))?;
    let mut files = BTreeMap::new();
    let mut fields = out.stdout.split(|b| *b == 0).peekable();
    while let Some(entry) = fields.next() {
        if entry.len() < 4 {
            continue;
        }
        let status = &entry[..2];
        let rel = String::from_utf8_lossy(&entry[3..]).into_owned();
        // A rename or copy names its source in the next field.
        if status[0] == b'R' || status[0] == b'C' {
            fields.next();
        }
        if excluded(&rel) {
            continue;
        }
        let full = top.join(&rel);
        files.insert(
            format!("{}:{rel}", String::from_utf8_lossy(status)),
            fingerprint(&full),
        );
    }
    Some(Snapshot::Files(files))
}

// A file's contents as a short hash; "-" when it isn't there.
fn fingerprint(p: &Path) -> String {
    match std::fs::read(p) {
        Ok(bytes) => ring::digest::digest(&ring::digest::SHA256, &bytes)
            .as_ref()
            .iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect(),
        Err(_) if p.is_dir() => "dir".into(),
        Err(_) => "-".into(),
    }
}

fn walk_snapshot(cwd: &Path) -> Snapshot {
    const SKIP: &[&str] = &[
        ".git",
        ".bwn",
        "node_modules",
        "target",
        ".venv",
        "__pycache__",
    ];
    let mut files = BTreeMap::new();
    let mut stack = vec![cwd.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(t) = e.file_type() else { continue };
            let name = e.file_name();
            if t.is_dir() {
                if !SKIP.iter().any(|s| name == *s) {
                    stack.push(e.path());
                }
                continue;
            }
            let rel = e
                .path()
                .strip_prefix(cwd)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let Ok(m) = e.metadata() else { continue };
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            // The change time too: `touch -r` can put a modification time
            // back, but nothing a user runs can reset the change time.
            #[cfg(unix)]
            let ctime = {
                use std::os::unix::fs::MetadataExt;
                format!("{}.{}", m.ctime(), m.ctime_nsec())
            };
            #[cfg(not(unix))]
            let ctime = "";
            files.insert(rel, format!("{}:{mtime}:{ctime}", m.len()));
            if files.len() > MAX_FILES {
                return Snapshot::Unknown;
            }
        }
    }
    Snapshot::Files(files)
}

/// The dependencies the project's manifests under `cwd` name, as
/// `manifest:name` (roadmap 3.2): `package.json`, `Cargo.toml`,
/// `requirements.txt` and `go.mod`.
pub fn manifest_dependencies(cwd: &Path) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    if let Some(v) = std::fs::read_to_string(cwd.join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    {
        for sec in [
            "dependencies",
            "devDependencies",
            "optionalDependencies",
            "peerDependencies",
        ] {
            for k in v[sec].as_object().into_iter().flat_map(|m| m.keys()) {
                out.insert(format!("npm:{k}"));
            }
        }
    }
    if let Ok(t) = std::fs::read_to_string(cwd.join("Cargo.toml")) {
        let mut in_deps = false;
        for line in t.lines().map(str::trim) {
            if line.starts_with('[') {
                let h = line.trim_matches(|c| c == '[' || c == ']');
                in_deps = h.ends_with("dependencies");
                // `[dependencies.serde]` names one by itself.
                if let Some((_, name)) = h.split_once("dependencies.") {
                    out.insert(format!("cargo:{}", name.trim()));
                    in_deps = false;
                }
                continue;
            }
            if in_deps {
                if let Some((name, _)) = line.split_once('=') {
                    let name = name.trim().trim_matches('"');
                    if !name.is_empty() && !name.starts_with('#') {
                        out.insert(format!("cargo:{name}"));
                    }
                }
            }
        }
    }
    if let Ok(t) = std::fs::read_to_string(cwd.join("requirements.txt")) {
        for line in t.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
                continue;
            }
            let name: String = line
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                .collect();
            if !name.is_empty() {
                out.insert(format!("pypi:{}", name.to_ascii_lowercase()));
            }
        }
    }
    if let Ok(t) = std::fs::read_to_string(cwd.join("go.mod")) {
        let mut block = false;
        for line in t.lines().map(str::trim) {
            let entry = if block {
                if line == ")" {
                    block = false;
                    continue;
                }
                line
            } else if line == "require (" {
                block = true;
                continue;
            } else if let Some(r) = line.strip_prefix("require ") {
                r
            } else {
                continue;
            };
            if let Some(m) = entry
                .split_whitespace()
                .next()
                .filter(|m| !m.starts_with("//"))
            {
                out.insert(format!("go:{m}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
    }

    // Every way a file changes shows: an edit (also of a file already
    // dirty), a new file, a deletion; bwn's own .bwn/ doesn't.
    #[test]
    fn a_git_snapshot_sees_every_change_but_bwns_own() {
        let d = std::env::temp_dir().join(format!("bwn-snapshot-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        git(&d, &["init", "-q"]);
        std::fs::write(d.join("a.txt"), "1").unwrap();
        std::fs::write(d.join("b.txt"), "1").unwrap();
        git(&d, &["add", "."]);
        git(&d, &["commit", "-q", "-m", "base"]);
        std::fs::write(d.join("a.txt"), "dirty").unwrap();
        let base = take(&d);
        // The same dirty file edited again.
        std::fs::write(d.join("a.txt"), "dirtier").unwrap();
        let s1 = take(&d);
        assert!(!s1.same_as(&base));
        let changed = s1.changed_since(&base);
        assert!(
            changed.iter().all(|k| k.ends_with(":a.txt")) && !changed.is_empty(),
            "{changed:?}"
        );
        // bwn's own folder doesn't count.
        std::fs::create_dir_all(d.join(".bwn/worktrees/x")).unwrap();
        std::fs::write(d.join(".bwn/worktrees/x/f"), "x").unwrap();
        assert!(take(&d).same_as(&s1));
        // A new file and a deletion do.
        std::fs::write(d.join("c.txt"), "new").unwrap();
        assert!(!take(&d).same_as(&s1));
        let s2 = take(&d);
        std::fs::remove_file(d.join("b.txt")).unwrap();
        assert!(!take(&d).same_as(&s2));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn manifests_name_their_dependencies() {
        let d = std::env::temp_dir().join(format!("bwn-deps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("package.json"),
            r#"{"dependencies":{"a":"1"},"devDependencies":{"b":"2"}}"#,
        )
        .unwrap();
        std::fs::write(
            d.join("Cargo.toml"),
            "[package]\nname = \"x\"\n[dependencies]\nserde = \"1\"\n[dev-dependencies]\ntempfile = \"3\"\n[dependencies.ring]\nversion = \"0.17\"\n",
        )
        .unwrap();
        std::fs::write(
            d.join("requirements.txt"),
            "# c\nRequests>=2\n-r other.txt\nflask\n",
        )
        .unwrap();
        std::fs::write(
            d.join("go.mod"),
            "module m\nrequire (\n  github.com/x/y v1\n)\nrequire golang.org/z v2\n",
        )
        .unwrap();
        let got: Vec<String> = manifest_dependencies(&d).into_iter().collect();
        assert_eq!(
            got,
            [
                "cargo:ring",
                "cargo:serde",
                "cargo:tempfile",
                "go:github.com/x/y",
                "go:golang.org/z",
                "npm:a",
                "npm:b",
                "pypi:flask",
                "pypi:requests"
            ]
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_walk_snapshot_sees_edits_and_unknown_is_never_the_same() {
        let d = std::env::temp_dir().join(format!("bwn-snapshot-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("node_modules")).unwrap();
        std::fs::write(d.join("a.txt"), "1").unwrap();
        let base = take(&d);
        assert!(take(&d).same_as(&base));
        std::fs::write(d.join("node_modules/x"), "skipped").unwrap();
        assert!(take(&d).same_as(&base));
        std::fs::write(d.join("a.txt"), "22").unwrap();
        assert!(!take(&d).same_as(&base));
        assert!(!Snapshot::Unknown.same_as(&Snapshot::Unknown));
        // A same-size edit with the modification time put back still shows.
        #[cfg(unix)]
        {
            let before = take(&d);
            let t = std::fs::metadata(d.join("a.txt"))
                .unwrap()
                .modified()
                .unwrap();
            std::fs::write(d.join("a.txt"), "33").unwrap();
            std::fs::File::options()
                .write(true)
                .open(d.join("a.txt"))
                .unwrap()
                .set_modified(t)
                .unwrap();
            assert!(!take(&d).same_as(&before));
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
