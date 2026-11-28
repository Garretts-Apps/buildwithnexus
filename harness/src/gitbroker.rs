//! Git from sandboxed commands, through bwn (roadmap 3.3; analysis A4, SO-6).
//!
//! Sandboxed commands can't write `.git`: a hook or config they planted
//! there would run later, outside the sandbox, the next time anyone ran git.
//! So `git add` and `git commit` from the shell used to fail. Now `git` on a
//! sandboxed command's PATH is bwn's own binary (the shim), which hands its
//! arguments to bwn over a socket. bwn runs the real git outside the
//! sandbox, under rules the command can't change. The shim reaches bwn
//! through the session's egress proxy, the one way out of the sandbox.
//!
//!
//! - Only repositories bwn found when the broker started: each workspace
//!   root's own, and the nested ones inside it. A `.git` a command made
//!   later, or one above the workspace (a dotfiles repository in the home
//!   folder), is refused, and git is told exactly which `--git-dir` and
//!   `--work-tree` to use, so it never goes looking.
//! - Only listed subcommands, each with listed options. Nothing that
//!   executes (`-c`, `--exec`, `-x`, `--upload-pack`), reads a file of the
//!   command's choosing (`-F`, `--template`, `--pathspec-from-file`) or
//!   writes git's configuration. A path argument must lie in the workspace
//!   and outside the credential stores bwn hides.
//! - A fixed environment, closed stdin, no hooks (`core.hooksPath=/dev/null`),
//!   no fsmonitor, no pager, no editor, no external diff. The user's own
//!   hooks don't run for these commits either.
//! - Not at all in a read-only session or a review, and not where the
//!   sandbox can't keep `.git` read-only (Landlock alone): there a command
//!   could have planted the config this git would read.
//!
//! Every answer goes in the decision log. Fetching and pulling come with
//! the credential broker (roadmap 3.3, second part).

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

/// Environment variable holding the broker's token inside the sandbox.
pub const TOKEN_ENV: &str = "BWN_GIT_BROKER";

/// The name bwn answers to as the shim.
pub const SHIM_NAME: &str = "git";

// What one subcommand accepts. `flags` take no value; `valued` take one,
// inline (`--x=v`, `-mv`) or as the next argument. `paths`: its
// positionals are paths or revisions, checked to stay in the workspace.
struct Sub {
    name: &'static str,
    mutating: bool,
    flags: &'static [&'static str],
    valued: &'static [&'static str],
    // Leading words that are part of the subcommand (`stash push`).
    words: &'static [&'static str],
    // Reaches a remote: the first positional is a configured remote's name,
    // the rest are refs, not paths.
    remote: bool,
}

const LOG_FLAGS: &[&str] = &[
    "--oneline",
    "--stat",
    "--shortstat",
    "--name-only",
    "--name-status",
    "--graph",
    "--decorate",
    "--no-decorate",
    "--all",
    "-p",
    "--patch",
    "--no-patch",
    "-s",
    "--no-color",
    "--first-parent",
    "--reverse",
    "--merges",
    "--no-merges",
    "--abbrev-commit",
    "--follow",
];
const LOG_VALUED: &[&str] = &[
    "-n",
    "--max-count",
    "--format",
    "--pretty",
    "--since",
    "--until",
    "--after",
    "--before",
    "--author",
    "--grep",
    "--skip",
    "--color",
    "-U",
    "--unified",
    "--date",
];

const SUBS: &[Sub] = &[
    Sub {
        name: "status",
        mutating: false,
        flags: &[
            "-s",
            "--short",
            "-b",
            "--branch",
            "--long",
            "-z",
            "--ignored",
            "--no-renames",
            "-v",
        ],
        valued: &["--porcelain", "-u", "--untracked-files"],
        words: &[],
        remote: false,
    },
    Sub {
        name: "diff",
        mutating: false,
        flags: &[
            "--cached",
            "--staged",
            "--stat",
            "--shortstat",
            "--name-only",
            "--name-status",
            "-p",
            "--patch",
            "--no-color",
            "--check",
            "-w",
            "--ignore-all-space",
            "--quiet",
            "--exit-code",
            "-R",
            "--numstat",
            "--summary",
        ],
        valued: &["-U", "--unified", "--color", "--diff-filter"],
        words: &[],
        remote: false,
    },
    Sub {
        name: "log",
        mutating: false,
        flags: LOG_FLAGS,
        valued: LOG_VALUED,
        words: &[],
        remote: false,
    },
    Sub {
        name: "show",
        mutating: false,
        flags: LOG_FLAGS,
        valued: LOG_VALUED,
        words: &[],
        remote: false,
    },
    Sub {
        name: "rev-parse",
        mutating: false,
        flags: &[
            "--short",
            "--abbrev-ref",
            "--verify",
            "-q",
            "--quiet",
            "--show-toplevel",
            "--is-inside-work-tree",
        ],
        valued: &[],
        words: &[],
        remote: false,
    },
    Sub {
        name: "ls-files",
        mutating: false,
        flags: &[
            "-m",
            "--modified",
            "-o",
            "--others",
            "--exclude-standard",
            "-d",
            "--deleted",
            "-c",
            "--cached",
            "-z",
        ],
        valued: &[],
        words: &[],
        remote: false,
    },
    Sub {
        name: "branch",
        mutating: true,
        flags: &[
            "-a",
            "--all",
            "-r",
            "--remotes",
            "--list",
            "-v",
            "-vv",
            "--show-current",
            "-d",
            "-D",
            "--merged",
            "--no-merged",
        ],
        valued: &[],
        words: &[],
        remote: false,
    },
    Sub {
        name: "add",
        mutating: true,
        flags: &[
            "-A",
            "--all",
            "-u",
            "--update",
            "-N",
            "--intent-to-add",
            "-n",
            "--dry-run",
            "-v",
            "--verbose",
            "-f",
            "--force",
        ],
        valued: &[],
        words: &[],
        remote: false,
    },
    Sub {
        name: "commit",
        mutating: true,
        flags: &[
            "-a",
            "--all",
            "--amend",
            "--no-edit",
            "--allow-empty",
            "-q",
            "--quiet",
            "-v",
            "--verbose",
            "-s",
            "--signoff",
            "--no-verify",
            "-n",
        ],
        valued: &["-m", "--message", "--author", "--date"],
        words: &[],
        remote: false,
    },
    Sub {
        name: "restore",
        mutating: true,
        flags: &["--staged", "-S", "--worktree", "-W"],
        valued: &["-s", "--source"],
        words: &[],
        remote: false,
    },
    Sub {
        name: "reset",
        mutating: true,
        flags: &["--soft", "--mixed", "--hard", "-q", "--quiet"],
        valued: &[],
        words: &[],
        remote: false,
    },
    Sub {
        name: "switch",
        mutating: true,
        flags: &["--detach", "-f", "--force", "--discard-changes"],
        valued: &["-c", "--create", "-C", "--force-create"],
        words: &[],
        remote: false,
    },
    Sub {
        name: "checkout",
        mutating: true,
        flags: &["-f", "--force", "--detach", "-q", "--quiet"],
        valued: &["-b", "-B"],
        words: &[],
        remote: false,
    },
    Sub {
        name: "merge",
        mutating: true,
        flags: &[
            "--no-ff",
            "--ff-only",
            "--ff",
            "--abort",
            "--continue",
            "--squash",
            "--no-commit",
            "-q",
            "--quiet",
        ],
        valued: &["-m", "--message"],
        words: &[],
        remote: false,
    },
    Sub {
        name: "rebase",
        mutating: true,
        flags: &[
            "--continue",
            "--abort",
            "--skip",
            "--quit",
            "-q",
            "--quiet",
            "--autostash",
            "--no-autostash",
        ],
        valued: &["--onto"],
        words: &[],
        remote: false,
    },
    Sub {
        name: "stash",
        mutating: true,
        flags: &[
            "-u",
            "--include-untracked",
            "-k",
            "--keep-index",
            "-q",
            "--quiet",
            "--index",
        ],
        valued: &["-m", "--message"],
        words: &["push", "pop", "apply", "list", "drop", "show", "clear"],
        remote: false,
    },
    Sub {
        name: "fetch",
        mutating: true,
        flags: &[
            "-q",
            "--quiet",
            "-p",
            "--prune",
            "-t",
            "--tags",
            "-n",
            "--no-tags",
            "-v",
            "--verbose",
        ],
        valued: &["--depth"],
        words: &[],
        remote: true,
    },
    Sub {
        name: "pull",
        mutating: true,
        flags: &[
            "--rebase",
            "--no-rebase",
            "--ff-only",
            "--ff",
            "--no-ff",
            "-q",
            "--quiet",
            "-v",
            "--verbose",
            "--autostash",
            "--no-autostash",
        ],
        valued: &[],
        words: &[],
        remote: true,
    },
];

/// What bwn will run for a request, once it passed the checks.
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub git_dir: PathBuf,
    pub work_tree: PathBuf,
    pub args: Vec<String>,
    pub mutating: bool,
    /// Reaches a remote: its name, or None for the branch's default.
    pub remote: Option<Option<String>>,
}

/// The repositories the broker serves: (work tree, git dir), found once.
#[derive(Clone, Debug, Default)]
pub struct Pinned {
    repos: Vec<(PathBuf, PathBuf)>,
    roots: Vec<PathBuf>,
}

impl Pinned {
    /// Each root's own repository and the nested ones inside it, as they
    /// are now. A repository above a root isn't served.
    pub fn find(roots: &[PathBuf]) -> Pinned {
        let roots: Vec<PathBuf> = roots.iter().map(|r| canon(r)).collect();
        let mut repos = Vec::new();
        for r in &roots {
            if let Some(g) = gitdir_of(r) {
                repos.push((r.clone(), g));
            }
            for entry in crate::sandbox::nested_git_entries(r) {
                if let Some(top) = entry.parent() {
                    if let Some(g) = gitdir_of(top) {
                        repos.push((canon(top), g));
                    }
                }
            }
        }
        Pinned { repos, roots }
    }

    // The pinned repository `cwd` is in: the deepest whose work tree holds
    // it, with no other `.git` in between (one a command made later).
    fn repo_for(&self, cwd: &Path) -> Result<&(PathBuf, PathBuf), String> {
        let best = self
            .repos
            .iter()
            .filter(|(top, _)| cwd.starts_with(top))
            .max_by_key(|(top, _)| top.components().count())
            .ok_or("not in a repository bwn serves (bwn serves the workspace's own repositories, found when the session started)")?;
        let mut d = cwd;
        while d != best.0 {
            if d.join(".git").symlink_metadata().is_ok() {
                return Err(format!(
                    "{} has a .git bwn didn't find when the session started; it isn't served",
                    d.display()
                ));
            }
            d = d.parent().ok_or("outside the repository")?;
        }
        Ok(best)
    }
}

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

// The git dir of the repository whose top is exactly `top`: its `.git`
// folder, or the folder a `.git` file names, canonical.
fn gitdir_of(top: &Path) -> Option<PathBuf> {
    let dot = top.join(".git");
    let meta = dot.symlink_metadata().ok()?;
    if meta.is_dir() {
        // A plain repository needs HEAD; an empty `.git` isn't one.
        return dot.join("HEAD").is_file().then(|| canon(&dot));
    }
    if meta.is_file() {
        let text = std::fs::read_to_string(&dot).ok()?;
        let rel = text.trim().strip_prefix("gitdir:")?.trim();
        let g = top.join(rel);
        return g.join("HEAD").is_file().then(|| canon(&g));
    }
    None
}

// `p` with `.` and `..` worked out, without touching the disk.
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// Check a request against the rules above. `cwd` is where the command
/// ran; `hidden` the credential stores.
pub fn check(
    args: &[String],
    cwd: &Path,
    pinned: &Pinned,
    hidden: &[PathBuf],
) -> Result<Plan, String> {
    let cwd = canon(cwd);
    if !pinned.roots.iter().any(|r| cwd.starts_with(r)) {
        return Err("git runs through bwn only inside the workspace".into());
    }
    let (top, git_dir) = pinned.repo_for(&cwd)?;
    let Some(first) = args.first() else {
        return Err("no git subcommand".into());
    };
    // No option before the subcommand: `-c`, `-C`, `--git-dir`, `-p`...
    if first.starts_with('-') {
        return Err(format!(
            "`{first}` before the subcommand isn't allowed through bwn"
        ));
    }
    let sub = SUBS
        .iter()
        .find(|s| s.name == first)
        .ok_or_else(|| format!("`git {first}` isn't allowed through bwn"))?;
    let mut rest = &args[1..];
    let mut out = vec![sub.name.to_string()];
    if let Some(w) = rest.first().filter(|w| sub.words.contains(&w.as_str())) {
        out.push(w.clone());
        rest = &rest[1..];
    }
    let path_ok = |p: &str| -> Result<(), String> {
        let full = lexical(&cwd.join(p));
        let full = if full.exists() { canon(&full) } else { full };
        if !pinned.roots.iter().any(|r| full.starts_with(r)) {
            return Err(format!("`{p}` is outside the workspace"));
        }
        if hidden.iter().any(|h| full.starts_with(h)) {
            return Err(format!("`{p}` is a credential store bwn hides"));
        }
        Ok(())
    };
    let mut i = 0;
    let mut paths_only = false;
    let mut remote: Option<String> = None;
    while i < rest.len() {
        let a = &rest[i];
        i += 1;
        if paths_only {
            path_ok(a)?;
            out.push(a.clone());
            continue;
        }
        if a == "--" {
            paths_only = true;
            out.push(a.clone());
            continue;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (format!("--{n}"), Some(v)),
                None => (a.clone(), None),
            };
            if sub.flags.contains(&name.as_str()) && inline.is_none() {
                out.push(a.clone());
            } else if sub.valued.contains(&name.as_str()) {
                out.push(a.clone());
                if inline.is_none() {
                    let v = rest
                        .get(i)
                        .ok_or_else(|| format!("`{name}` needs a value"))?;
                    out.push(v.clone());
                    i += 1;
                }
            } else {
                return Err(format!(
                    "`git {} {name}` isn't allowed through bwn",
                    sub.name
                ));
            }
            continue;
        }
        if a.len() > 1 && a.starts_with('-') {
            // `-5` for log and show.
            if a[1..].chars().all(|c| c.is_ascii_digit()) && matches!(sub.name, "log" | "show") {
                out.push(a.clone());
                continue;
            }
            if sub.flags.contains(&a.as_str()) {
                out.push(a.clone());
                continue;
            }
            // A valued short option, its value inline or next.
            let opt = &a[..2];
            if sub.valued.contains(&opt) {
                out.push(a.clone());
                if a.len() == 2 {
                    let v = rest
                        .get(i)
                        .ok_or_else(|| format!("`{opt}` needs a value"))?;
                    out.push(v.clone());
                    i += 1;
                }
                continue;
            }
            // Bundled flags (`-am` is not: `-m` takes a value).
            let chars: Vec<String> = a[1..].chars().map(|c| format!("-{c}")).collect();
            if chars.iter().all(|c| sub.flags.contains(&c.as_str())) {
                out.push(a.clone());
                continue;
            }
            if let Some((last, init)) = chars.split_last() {
                if init.iter().all(|c| sub.flags.contains(&c.as_str()))
                    && sub.valued.contains(&last.as_str())
                {
                    out.push(a.clone());
                    let v = rest
                        .get(i)
                        .ok_or_else(|| format!("`{last}` needs a value"))?;
                    out.push(v.clone());
                    i += 1;
                    continue;
                }
            }
            return Err(format!("`git {} {a}` isn't allowed through bwn", sub.name));
        }
        if sub.remote {
            // The remote by its configured name, never a URL or a path; the
            // refs after it go as they are.
            if remote.is_none() {
                let ok = !a.is_empty()
                    && a.chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                    && !a.starts_with('.');
                if !ok {
                    return Err(format!(
                        "`{a}` isn't a remote's name (a URL or a path isn't allowed through bwn)"
                    ));
                }
                remote = Some(a.clone());
            } else if a.contains("::") || a.starts_with('/') {
                return Err(format!("`{a}` isn't a ref"));
            }
            out.push(a.clone());
            continue;
        }
        path_ok(a)?;
        out.push(a.clone());
    }
    Ok(Plan {
        git_dir: git_dir.clone(),
        work_tree: top.clone(),
        args: out,
        mutating: sub.mutating,
        remote: sub.remote.then_some(remote),
    })
}

/// The git command for a plan: a fixed environment, no hooks, no pager,
/// no editor, no external programs from the command's side.
pub fn command(plan: &Plan, cwd: &Path) -> std::process::Command {
    let mut c = std::process::Command::new("git");
    c.env_clear();
    for k in [
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "TZ",
        "SystemRoot",
    ] {
        if let Some(v) = std::env::var_os(k) {
            c.env(k, v);
        }
    }
    // Reaching a remote: the user's own ways in, which the sandbox never
    // sees (the credential broker, roadmap 3.3).
    if plan.remote.is_some() {
        for k in [
            "SSH_AUTH_SOCK",
            "GIT_SSH",
            "GIT_SSH_COMMAND",
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "NO_PROXY",
            "no_proxy",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
            "GIT_SSL_CAINFO",
            "XDG_CONFIG_HOME",
            "XDG_RUNTIME_DIR",
        ] {
            if let Some(v) = std::env::var_os(k) {
                c.env(k, v);
            }
        }
    }
    c.env("GIT_DIR", &plan.git_dir)
        .env("GIT_WORK_TREE", &plan.work_tree)
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    for kv in [
        "core.hooksPath=/dev/null",
        "core.fsmonitor=false",
        "core.pager=cat",
        "core.editor=true",
        "sequence.editor=true",
        "protocol.allow=never",
        "submodule.recurse=false",
        "fetch.recurseSubmodules=false",
    ] {
        c.args(["-c", kv]);
    }
    if plan.remote.is_some() {
        // Over HTTPS or SSH only, with the user's credential helpers.
        c.args([
            "-c",
            "protocol.https.allow=always",
            "-c",
            "protocol.ssh.allow=always",
        ]);
    } else {
        c.args(["-c", "credential.helper="]);
    }
    c.arg("--no-pager");
    let mut args = plan.args.clone();
    if matches!(args[0].as_str(), "diff" | "log" | "show") {
        args.insert(1, "--no-ext-diff".into());
        args.insert(1, "--no-textconv".into());
    }
    c.args(&args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null());
    c
}

/// Why this session can't run a mutating git command now, if it can't.
fn policy_refusal(mutating: bool) -> Option<&'static str> {
    if !crate::sandbox::protects_policy_files() {
        return Some("this sandbox can't keep .git read-only, so git doesn't run through bwn here");
    }
    if mutating && crate::hooks::permission_mode() == "readonly" {
        return Some("the session is read-only");
    }
    if mutating && crate::agent::reviewing() {
        return Some("a review makes no changes");
    }
    None
}

// Run plain git in the pinned repository for a value (a remote's URL).
fn git_value(plan: &Plan, cwd: &Path, args: &[&str]) -> Option<String> {
    let lookup = Plan {
        git_dir: plan.git_dir.clone(),
        work_tree: plan.work_tree.clone(),
        args: args.iter().map(|a| a.to_string()).collect(),
        mutating: false,
        remote: None,
    };
    let mut c = command(&lookup, cwd);
    let o = c.output().ok()?;
    let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
    (o.status.success() && !v.is_empty()).then_some(v)
}

/// The host a remote URL reaches, for HTTPS and SSH only.
pub fn url_host(url: &str) -> Result<String, String> {
    let url = url.trim();
    if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("ssh://"))
    {
        let auth = rest.split('/').next().unwrap_or("");
        let hostport = auth.rsplit('@').next().unwrap_or("");
        let host = if let Some(h) = hostport.strip_prefix('[') {
            h.split(']').next().unwrap_or("")
        } else {
            hostport.split(':').next().unwrap_or("")
        };
        return (!host.is_empty())
            .then(|| host.to_ascii_lowercase())
            .ok_or_else(|| "no host".into());
    }
    // scp-like: [user@]host:path, with no scheme and no slash before the colon.
    if !url.contains("://") {
        if let Some((left, path)) = url.split_once(':') {
            let host = left.rsplit('@').next().unwrap_or("");
            // `x::…` names a transport (`ext::`), not a host.
            if !path.starts_with(':')
                && !host.is_empty()
                && !host.contains('/')
                && !host.contains('\\')
                && host.len() > 1
            {
                return Ok(host.to_ascii_lowercase());
            }
        }
    }
    Err(format!("`{url}` isn't an HTTPS or SSH remote"))
}

// The remote a fetch or pull reaches must be one the repository names,
// over HTTPS or SSH, to a host the session's network rules allow.
fn remote_check(plan: &Plan, name: Option<&str>, cwd: &Path) -> Result<(), String> {
    let name = match name {
        Some(n) => n.to_string(),
        None => {
            let branch =
                git_value(plan, cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
            git_value(
                plan,
                cwd,
                &["config", "--get", &format!("branch.{branch}.remote")],
            )
            .unwrap_or_else(|| "origin".into())
        }
    };
    let remotes = git_value(plan, cwd, &["remote"]).unwrap_or_default();
    if !remotes.lines().any(|r| r.trim() == name) {
        return Err(format!("`{name}` isn't a remote of this repository"));
    }
    let url = git_value(plan, cwd, &["ls-remote", "--get-url", &name])
        .ok_or_else(|| format!("no URL for `{name}`"))?;
    let host = url_host(&url)?;
    let rules = crate::egress::Rules::load(&plan.work_tree);
    crate::egress::check(&host, 443, &rules, true)
        .map(|_| ())
        .map_err(|why| format!("{host}: {why}"))
}

/// What the broker answers.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Reply {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

fn refused(why: &str) -> Reply {
    Reply {
        code: 128,
        stdout: String::new(),
        stderr: format!("bwn: git through the sandbox: {why}\n"),
    }
}

/// Answer one request: check it, run it, record it.
pub fn serve(args: &[String], cwd: &Path, pinned: &Pinned) -> Reply {
    let roots: Vec<&Path> = pinned.roots.iter().map(PathBuf::as_path).collect();
    let hidden = crate::sandbox::hidden_paths(&roots);
    let subject = format!("git {}", args.join(" "));
    let decided =
        check(args, cwd, pinned, &hidden).and_then(|plan| match policy_refusal(plan.mutating) {
            Some(why) => Err(why.to_string()),
            None => Ok(plan),
        });
    let decided = decided.and_then(|plan| match &plan.remote {
        Some(name) => remote_check(&plan, name.as_deref(), cwd).map(|_| plan),
        None => Ok(plan),
    });
    let plan = match decided {
        Ok(p) => p,
        Err(why) => {
            crate::audit::record(crate::audit::Entry {
                kind: crate::audit::Kind::Gate,
                actor: "policy",
                decision: "deny",
                subject: &subject,
                detail: &why,
            });
            return refused(&why);
        }
    };
    crate::audit::record(crate::audit::Entry {
        kind: crate::audit::Kind::Gate,
        actor: "policy",
        decision: "allow",
        subject: &subject,
        detail: "git through the sandbox",
    });
    match command(&plan, cwd).output() {
        Ok(o) => Reply {
            code: o.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => refused(&format!("git could not start: {e}")),
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Request {
    token: String,
    args: Vec<String>,
    cwd: PathBuf,
}

// ── the broker: reached through the session's egress proxy ─────────────────
//
// Sandboxed commands may open only TCP sockets, to the proxy (landlock.rs),
// so the shim asks there: a POST to a name that never resolves, which the
// proxy answers itself. The token, in the shim's environment only, keeps
// other programs on this machine that can reach the proxy's port out.

/// The host the shim posts to; `.invalid` never resolves (RFC 2606).
pub const BROKER_HOST: &str = "bwn-git.invalid";

struct State {
    dir: PathBuf,
    token: String,
    pinned: Pinned,
}

static STATE: std::sync::OnceLock<Option<State>> = std::sync::OnceLock::new();

/// The folder to put first on a sandboxed command's PATH (holding `git`,
/// the shim) and the token the shim sends, set up on first use. None where
/// git doesn't go through bwn.
pub fn shim() -> Option<(&'static Path, &'static str)> {
    if !crate::sandbox::protects_policy_files() {
        return None;
    }
    STATE
        .get_or_init(start)
        .as_ref()
        .map(|s| (s.dir.as_path(), s.token.as_str()))
}

#[cfg(unix)]
fn start() -> Option<State> {
    use std::os::unix::fs::DirBuilderExt;
    let exe = shim_exe()?;
    let mut n = [0u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut n).ok()?;
    let token: String = n.iter().map(|b| format!("{b:02x}")).collect();
    let dir = std::env::temp_dir().canonicalize().ok()?.join(format!(
        "bwn-git-{}-{}",
        std::process::id(),
        &token[..8]
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&dir).ok()?;
    std::os::unix::fs::symlink(&exe, dir.join(SHIM_NAME)).ok()?;
    let roots: Vec<PathBuf> = std::env::current_dir()
        .into_iter()
        .chain(crate::workdirs::list())
        .collect();
    Some(State {
        dir,
        token,
        pinned: Pinned::find(&roots),
    })
}

#[cfg(not(unix))]
fn start() -> Option<State> {
    None
}

/// The proxy's answer to a POST for [`BROKER_HOST`]: the reply's JSON.
pub fn answer(body: &[u8]) -> String {
    let reply = match (
        serde_json::from_slice::<Request>(body),
        STATE.get().and_then(Option::as_ref),
    ) {
        (Ok(req), Some(st)) => {
            // Compared without stopping at the first difference.
            let (a, b) = (req.token.as_bytes(), st.token.as_bytes());
            let ok = a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0;
            if ok {
                serve(&req.args, &req.cwd, &st.pinned)
            } else {
                refused("not a request from this session's commands")
            }
        }
        (Err(e), _) => refused(&format!("bad request: {e}")),
        (_, None) => refused("no broker in this session"),
    };
    serde_json::to_string(&reply).unwrap_or_default()
}

// The program the shim runs as: bwn's own binary (a test sets another).
#[cfg(unix)]
fn shim_exe() -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    if let Some(p) = std::env::var_os("BWN_TEST_SHIM_EXE") {
        return (!p.is_empty()).then(|| PathBuf::from(p));
    }
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    std::env::current_exe().ok()
}

/// bwn run as `git` inside a sandbox: pass the arguments to the broker
/// through the proxy and print what it says. Exit status is git's.
pub fn shim_main(args: &[String]) -> i32 {
    let fail = |why: String| {
        eprintln!("bwn: git: {why}");
        128
    };
    let Some(token) = std::env::var_os(TOKEN_ENV) else {
        return fail("no broker (this `git` is bwn's, for sandboxed commands)".into());
    };
    let proxy = std::env::var("HTTP_PROXY").unwrap_or_default();
    let Some(addr) = proxy
        .strip_prefix("http://")
        .map(|a| a.trim_end_matches('/').to_string())
    else {
        return fail("the sandbox has no network, so git can't reach bwn".into());
    };
    let req = Request {
        token: token.to_string_lossy().into_owned(),
        args: args.to_vec(),
        cwd: std::env::current_dir().unwrap_or_default(),
    };
    let body = match serde_json::to_string(&req) {
        Ok(b) => b,
        Err(e) => return fail(e.to_string()),
    };
    let reply = std::net::TcpStream::connect(&addr)
        .and_then(|mut s| {
            write!(
                s,
                "POST http://{BROKER_HOST}/ HTTP/1.1\r\nHost: {BROKER_HOST}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )?;
            let mut text = String::new();
            s.read_to_string(&mut text)?;
            Ok(text)
        })
        .map_err(|e| e.to_string())
        .and_then(|text| {
            let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
            serde_json::from_str::<Reply>(body).map_err(|e| format!("{e}: {}", text.lines().next().unwrap_or("")))
        });
    match reply {
        Ok(r) => {
            print!("{}", r.stdout);
            eprint!("{}", r.stderr);
            let _ = std::io::stdout().flush();
            r.code
        }
        Err(e) => fail(format!("the broker didn't answer: {e}")),
    }
}

#[cfg(test)]
mod tests;
