//! Project walks: the .gitignore subset, the skip list, symlink-loop guards, and the walkers behind grep, find, list_tree and @ completion.

use super::*;

// ── .gitignore subset for the file walkers ──────────────────────────────────
// Blank lines and `#` comments, `!` negation, a trailing `/` for folders
// only, a `/` anywhere but the end anchoring the pattern to its file's
// folder, and `*`, `?`, `[…]` and `**` globs. Nested .gitignore files apply
// below their folder, and the last matching rule wins. Sensitive paths are
// hidden by their own check, which no `!` pattern undoes.

#[derive(Clone)]
pub(super) struct IgnoreRule {
    pub(super) base: PathBuf,
    pub(super) parts: Vec<String>,
    pub(super) anchored: bool,
    pub(super) dir_only: bool,
    pub(super) negate: bool,
}

#[derive(Clone, Default)]
pub(super) struct Ignores {
    pub(super) rules: Vec<IgnoreRule>,
}

impl Ignores {
    // The .gitignore files from the repository top (the nearest folder up
    // from `root` holding .git) down to `root`; only root's own outside git.
    pub(super) fn for_root(root: &Path) -> Ignores {
        let mut chain = Vec::new();
        let mut dir = Some(root);
        let mut in_git = false;
        while let Some(d) = dir {
            chain.push(d.to_path_buf());
            if d.join(".git").exists() {
                in_git = true;
                break;
            }
            dir = d.parent();
        }
        if !in_git {
            chain.truncate(1);
        }
        let mut ig = Ignores::default();
        for d in chain.iter().rev() {
            if let Some(next) = ig.with_dir(d) {
                ig = next;
            }
        }
        ig
    }

    // The rules for walking into `dir`: these plus its own .gitignore, or
    // None when it has none (the caller keeps using these).
    pub(super) fn with_dir(&self, dir: &Path) -> Option<Ignores> {
        let text = fs::read_to_string(dir.join(".gitignore")).ok()?;
        let mut next = self.clone();
        next.rules.extend(parse_gitignore(&text, dir));
        Some(next)
    }

    pub(super) fn ignored(&self, path: &Path, is_dir: bool) -> bool {
        let mut ignored = false;
        for rule in &self.rules {
            if rule.matches(path, is_dir) {
                ignored = !rule.negate;
            }
        }
        ignored
    }
}

pub(super) fn parse_gitignore(text: &str, base: &Path) -> Vec<IgnoreRule> {
    let mut rules = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (negate, pat) = match line.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, line.strip_prefix('\\').unwrap_or(line)),
        };
        let dir_only = pat.ends_with('/');
        let pat = pat.trim_end_matches('/');
        let anchored = pat.contains('/');
        let parts: Vec<String> = pat
            .trim_start_matches('/')
            .split('/')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        if parts.is_empty() {
            continue;
        }
        rules.push(IgnoreRule {
            base: base.to_path_buf(),
            parts,
            anchored,
            dir_only,
            negate,
        });
    }
    rules
}

impl IgnoreRule {
    pub(super) fn matches(&self, path: &Path, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        let Ok(rel) = path.strip_prefix(&self.base) else {
            return false;
        };
        if !self.anchored {
            // Only the last name matters; this runs per rule per walked entry.
            return rel
                .file_name()
                .is_some_and(|n| ignore_glob(&self.parts[0], &n.to_string_lossy()));
        }
        let names: Vec<std::borrow::Cow<str>> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect();
        ignore_parts_match(&self.parts, &names)
    }
}

// `**` matches any run of folders, including none. Like ignore_glob, one
// pass that backtracks only to the last `**`.
pub(super) fn ignore_parts_match<S: AsRef<str>>(pat: &[String], names: &[S]) -> bool {
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < names.len() {
        match pat.get(pi) {
            Some(p) if p == "**" => {
                star = Some((pi, ni));
                pi += 1;
            }
            Some(p) if ignore_glob(p, names[ni].as_ref()) => {
                pi += 1;
                ni += 1;
            }
            _ => match star {
                Some((sp, sn)) => {
                    pi = sp + 1;
                    ni = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    pat[pi..].iter().all(|p| p == "**")
}

// One name against one gitignore glob: `*` any run, `?` one char, `[…]`
// a set (with `!` or `^` negating and `a-z` ranges). Unlike the shell, git
// lets `*` match a leading dot. The pattern comes from the checkout, so it
// is matched in one pass that backtracks only to the last `*`: at most
// pattern × name steps, however the pattern is built.
pub(super) fn ignore_glob(pat: &str, name: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if p.get(pi) == Some(&'*') {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some(next) = glob_char(&p, pi, n[ni]) {
            pi = next;
            ni += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

// Where the pattern goes on after `p[pi]` matches the one character `c`.
pub(super) fn glob_char(p: &[char], pi: usize, c: char) -> Option<usize> {
    match *p.get(pi)? {
        '?' => Some(pi + 1),
        '[' => {
            // No closing bracket: the `[` is a plain character.
            let Some(close) = p[pi + 1..]
                .iter()
                .position(|&x| x == ']')
                .map(|i| pi + 1 + i)
            else {
                return (c == '[').then_some(pi + 1);
            };
            let mut set = &p[pi + 1..close];
            let neg = matches!(set.first(), Some('!') | Some('^'));
            if neg {
                set = &set[1..];
            }
            let mut hit = false;
            let mut i = 0;
            while i < set.len() {
                if i + 2 < set.len() && set[i + 1] == '-' {
                    hit |= set[i] <= c && c <= set[i + 2];
                    i += 3;
                } else {
                    hit |= set[i] == c;
                    i += 1;
                }
            }
            (hit != neg).then_some(close + 1)
        }
        lit => (lit == c).then_some(pi + 1),
    }
}

// What a project walk collects and how far it has got. Folders reached
// through a symlink are remembered by their real path, so a link back up the
// tree (or a ring of links) is not walked again under ever longer paths.
pub(super) struct Walk<'a> {
    pub(super) out: &'a mut Vec<PathBuf>,
    pub(super) seen: &'a mut usize,
    pub(super) dirs: bool,
    pub(super) files: bool,
    pub(super) skip_sensitive: bool,
    pub(super) linked: HashSet<PathBuf>,
}

impl Walk<'_> {
    pub(super) fn run(&mut self, root: &Path) {
        if let Ok(real) = fs::canonicalize(root) {
            self.linked.insert(real);
        }
        self.dir(root, &Ignores::for_root(root));
    }

    pub(super) fn dir(&mut self, root: &Path, ignores: &Ignores) {
        if *self.seen >= MAX_SEARCH_FILES {
            return;
        }
        let Ok(rd) = fs::read_dir(root) else {
            return;
        };
        for entry in rd.filter_map(|e| e.ok()) {
            if *self.seen >= MAX_SEARCH_FILES {
                break;
            }
            let path = entry.path();
            // The listing already knows the type; only a symlink costs a stat.
            let (is_dir, is_file, is_link) = match entry.file_type() {
                Ok(t) if !t.is_symlink() => (t.is_dir(), t.is_file(), false),
                _ => (path.is_dir(), path.is_file(), true),
            };
            if is_dir {
                if skip_dir(&path)
                    || ignores.ignored(&path, true)
                    || !self.follow(&path, root, is_link)
                {
                    continue;
                }
                if self.dirs {
                    *self.seen += 1;
                    self.out.push(path.clone());
                }
                match ignores.with_dir(&path) {
                    Some(inner) => self.dir(&path, &inner),
                    None => self.dir(&path, ignores),
                }
            } else if self.files
                && is_file
                && !(self.skip_sensitive && is_sensitive(&path))
                && !ignores.ignored(&path, false)
            {
                *self.seen += 1;
                self.out.push(path);
            }
        }
    }

    // A linked folder is walked unless it holds the folder being walked or
    // was already reached through a link.
    pub(super) fn follow(&mut self, path: &Path, parent: &Path, is_link: bool) -> bool {
        if !is_link {
            return true;
        }
        let Ok(real) = fs::canonicalize(path) else {
            return false;
        };
        if fs::canonicalize(parent).is_ok_and(|p| p.starts_with(&real)) {
            return false;
        }
        self.linked.insert(real)
    }
}

pub(super) fn collect_files(root: &Path, out: &mut Vec<PathBuf>, seen: &mut usize) {
    let (dirs, files, skip_sensitive) = (false, true, true);
    let linked = HashSet::new();
    Walk {
        out,
        seen,
        dirs,
        files,
        skip_sensitive,
        linked,
    }
    .run(root);
}

pub(super) fn collect_paths(
    root: &Path,
    out: &mut Vec<PathBuf>,
    seen: &mut usize,
    dirs: bool,
    files: bool,
) {
    let linked = HashSet::new();
    Walk {
        out,
        seen,
        dirs,
        files,
        skip_sensitive: false,
        linked,
    }
    .run(root);
}

/// The entries of `dir` (name, is_folder) that a project walk rooted at
/// `root` would see: the skip list, .gitignore rules from the repository top
/// down and sensitive files hide the rest, and so does a `dir` that sits
/// inside a hidden folder. For `@` path completion.
pub fn visible_entries(root: &Path, dir: &Path) -> Vec<(String, bool)> {
    let (root, rel) = match dir.strip_prefix(root) {
        Ok(rel) => (root, rel.to_path_buf()),
        Err(_) => (dir, PathBuf::new()),
    };
    let mut ignores = Ignores::for_root(root);
    let mut at = root.to_path_buf();
    for part in rel.components() {
        let std::path::Component::Normal(name) = part else {
            // `..` or the like: no walk covers it, so nothing is hidden by one.
            return list_entries(dir, &Ignores::default());
        };
        at.push(name);
        if skip_dir(&at) || ignores.ignored(&at, true) {
            return Vec::new();
        }
        if let Some(inner) = ignores.with_dir(&at) {
            ignores = inner;
        }
    }
    list_entries(dir, &ignores)
}

pub(super) fn list_entries(dir: &Path, ignores: &Ignores) -> Vec<(String, bool)> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let path = e.path();
            let is_dir = path.is_dir();
            let hidden = if is_dir {
                skip_dir(&path) || ignores.ignored(&path, true)
            } else {
                is_sensitive(&path) || ignores.ignored(&path, false)
            };
            (!hidden).then(|| (e.file_name().to_string_lossy().into_owned(), is_dir))
        })
        .collect()
}

/// Every file a project walk sees (skip list, .gitignore, sensitive files
/// hidden), relative to `cwd` with `/` separators.
pub fn project_files(cwd: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut seen = 0;
    collect_files(cwd, &mut files, &mut seen);
    files.iter().map(|p| display_path(p, cwd)).collect()
}

/// The paths among `files` whose name starts with or contains `query`
/// (case-insensitive): starts-with matches first, then shorter paths. For
/// `@` completion by name.
pub fn rank_by_name(files: &[String], query: &str, limit: usize) -> Vec<String> {
    let q = query.to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(bool, &String)> = files
        .iter()
        .filter_map(|p| {
            let name = p.rsplit('/').next().unwrap_or(p).to_lowercase();
            let starts = name.starts_with(&q);
            (starts || name.contains(&q)).then_some((!starts, p))
        })
        .collect();
    hits.sort_by(|a, b| (a.0, a.1.len(), a.1).cmp(&(b.0, b.1.len(), b.1)));
    hits.into_iter()
        .take(limit)
        .map(|(_, p)| p.clone())
        .collect()
}

pub(super) fn simple_glob_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let text = text.to_lowercase();
    let pattern = pattern.as_str();
    let text = text.as_str();
    if pattern.is_empty() || pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return text.contains(pattern);
    }
    let anchored_start = !pattern.starts_with('*');
    let anchored_end = !pattern.ends_with('*');
    let parts: Vec<&str> = pattern.split('*').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return true;
    }
    let mut rest = text;
    for (idx, part) in parts.iter().enumerate() {
        let Some(pos) = rest.find(part) else {
            return false;
        };
        if idx == 0 && anchored_start && pos != 0 {
            return false;
        }
        rest = &rest[pos + part.len()..];
    }
    if anchored_end {
        if let Some(last) = parts.last() {
            return text.ends_with(last);
        }
    }
    true
}

pub(super) fn display_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub(super) fn collect_tree(
    root: &Path,
    cwd: &Path,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<String>,
    max_entries: usize,
) {
    let ignores = Ignores::for_root(root);
    collect_tree_in(root, &ignores, cwd, depth, max_depth, out, max_entries);
}

// Gitignored entries are left out of the tree; skipped folders (build
// output, dependencies) are listed but not opened.
pub(super) fn collect_tree_in(
    root: &Path,
    ignores: &Ignores,
    cwd: &Path,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<String>,
    max_entries: usize,
) {
    if out.len() >= max_entries || depth > max_depth {
        return;
    }
    let Ok(rd) = fs::read_dir(root) else {
        return;
    };
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        if out.len() >= max_entries {
            break;
        }
        let path = entry.path();
        let is_dir = path.is_dir();
        if ignores.ignored(&path, is_dir) {
            continue;
        }
        let suffix = if is_dir { "/" } else { "" };
        out.push(format!("{}{}", display_path(&path, cwd), suffix));
        if is_dir && !skip_dir(&path) {
            let inner = ignores.with_dir(&path);
            let next = inner.as_ref().unwrap_or(ignores);
            collect_tree_in(&path, next, cwd, depth + 1, max_depth, out, max_entries);
        }
    }
}

#[cfg(test)]
mod walker_tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn walks_follow_links_out_but_not_round_a_loop() {
        let d = std::env::temp_dir().join(format!("bwn-walk-links-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let outside = d.with_extension("outside");
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(d.join("src")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(d.join("src/a.rs"), "x").unwrap();
        fs::write(outside.join("b.rs"), "y").unwrap();
        // A link back up the tree, and one to a folder outside it.
        std::os::unix::fs::symlink("..", d.join("src/up")).unwrap();
        std::os::unix::fs::symlink(&outside, d.join("ext")).unwrap();
        let mut files = project_files(&d);
        files.sort();
        assert_eq!(files, ["ext/b.rs", "src/a.rs"]);
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&outside);
    }
    use serde_json::json;

    fn fixture(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bwn-walk-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let put = |rel: &str, text: &str| {
            let p = d.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        };
        fs::create_dir_all(d.join(".git")).unwrap();
        put(
            ".gitignore",
            "*.log\n/out/\nbuild-*/\n!keep.log\n!.env\nsecret?.txt\n",
        );
        put("src/app.py", "needle\n");
        put("src/debug.log", "needle\n");
        put("src/keep.log", "needle\n");
        put("out/gen.py", "needle\n");
        put("src/out/real.py", "needle\n");
        put("build-x/a.py", "needle\n");
        put("secret1.txt", "needle\n");
        put(".env", "needle\n");
        put(".venv/lib/site.py", "needle\n");
        put("env2/pyvenv.cfg", "home = /usr\n");
        put("env2/lib/x.py", "needle\n");
        put("pkg/.gitignore", "fixtures/\n*.tmp\n");
        put("pkg/fixtures/f.py", "needle\n");
        put("pkg/a.tmp", "needle\n");
        put("pkg/mod.py", "needle\n");
        d
    }

    fn found(d: &Path) -> Vec<String> {
        let r = run("grep_files", &json!({"pattern": "needle"}), d);
        let mut v: Vec<String> = r
            .content
            .lines()
            .map(|l| l.split(':').next().unwrap_or("").to_string())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn walks_skip_gitignored_paths_and_virtualenvs() {
        let d = fixture("ignore");
        // Ignored: *.log (but !keep.log), /out/ only at the top, build-*/,
        // secret?.txt, a nested .gitignore's own rules, and virtualenvs.
        assert_eq!(
            found(&d),
            [
                "pkg/mod.py",
                "src/app.py",
                "src/keep.log",
                "src/out/real.py"
            ]
        );
        let r = run("find_files", &json!({"pattern": "*.py"}), &d);
        assert!(
            !r.content.contains(".venv") && !r.content.contains("env2/"),
            "{}",
            r.content
        );
        assert!(!r.content.contains("fixtures"), "{}", r.content);
        let r = run("find_paths", &json!({"pattern": "*"}), &d);
        assert!(
            !r.content.contains("build-x") && !r.content.contains("debug.log"),
            "{}",
            r.content
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_negation_never_shows_a_sensitive_file() {
        let d = fixture("negate");
        // `!.env` in .gitignore does not bring .env back.
        assert!(!found(&d).iter().any(|p| p.contains(".env")));
        let r = run("find_files", &json!({"pattern": "*env*"}), &d);
        assert!(!r.content.lines().any(|l| l == ".env"), "{}", r.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn files_are_found_by_name_anywhere_in_the_tree() {
        let d = fixture("named");
        let files = project_files(&d);
        assert_eq!(rank_by_name(&files, "mod", 10), ["pkg/mod.py"]);
        assert_eq!(rank_by_name(&files, "APP", 10), ["src/app.py"]);
        // Ignored and sensitive files are not offered.
        assert!(rank_by_name(&files, "debug", 10).is_empty());
        assert!(rank_by_name(&files, ".env", 10).is_empty());
        assert!(rank_by_name(&files, "", 10).is_empty());
        // Names that start with the query come before names that contain it.
        let list = ["a/xfile_1.py".to_string(), "b/file_1.py".to_string()];
        assert_eq!(
            rank_by_name(&list, "file_1", 10),
            ["b/file_1.py", "a/xfile_1.py"]
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn gitignore_globs_follow_git() {
        assert!(ignore_glob("*.log", "a.log"));
        assert!(ignore_glob("*.log", ".hidden.log"));
        assert!(ignore_glob("secret?.txt", "secret1.txt"));
        assert!(!ignore_glob("secret?.txt", "secret12.txt"));
        assert!(ignore_glob("[a-c]x", "bx") && !ignore_glob("[!a-c]x", "bx"));
        let parts = |s: &str| s.split('/').map(str::to_string).collect::<Vec<_>>();
        assert!(ignore_parts_match(&parts("**/gen"), &parts("a/b/gen")));
        assert!(ignore_parts_match(&parts("**/gen"), &parts("gen")));
        assert!(ignore_parts_match(
            &parts("docs/**/*.md"),
            &parts("docs/a/b.md")
        ));
        assert!(!ignore_parts_match(
            &parts("docs/*.md"),
            &parts("docs/a/b.md")
        ));
        assert!(ignore_parts_match(&parts("gen/**"), &parts("gen/x/y")));
        assert!(ignore_glob("a*b*c", "a-b-b-c") && !ignore_glob("a*b*c", "a-b-b-d"));
        assert!(ignore_glob("[x", "[x") && !ignore_glob("[x", "x"));
    }

    // A checkout's .gitignore is the repository's to write: a pattern built
    // to backtrack must not stall grep_files (or @ completion) on a long name.
    #[test]
    fn a_hostile_gitignore_pattern_cannot_stall_a_walk() {
        let d = std::env::temp_dir().join(format!("bwn-walk-redos-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join(".git")).unwrap();
        let deep: PathBuf = std::iter::repeat_n("a", 30).collect();
        fs::create_dir_all(d.join(&deep)).unwrap();
        fs::write(d.join(&deep).join("a".repeat(60)), "needle\n").unwrap();
        fs::write(d.join("app.py"), "needle\n").unwrap();
        let star_pattern = format!("{}b", "*a".repeat(16));
        let deep_pattern = format!("{}b", "**/a/".repeat(10));
        fs::write(
            d.join(".gitignore"),
            format!("{star_pattern}\n{deep_pattern}\n"),
        )
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let root = d.clone();
        std::thread::spawn(move || {
            let _ = tx.send(run("grep_files", &json!({"pattern": "needle"}), &root).content);
        });
        let found = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("grep_files was still matching .gitignore patterns after 5 s");
        let _ = fs::remove_dir_all(&d);
        assert!(found.contains("app.py"), "{found}");
        assert!(found.contains(&"a".repeat(60)), "{found}");
    }
}
