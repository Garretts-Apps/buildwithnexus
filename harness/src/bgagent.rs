//! Background agents (roadmap 5.2, CP-4): children of the session, never a
//! daemon, each in a git worktree of its own and the session's sandbox.
//!
//! `/bg <task>` (or `buildwithnexus bg run <task>`) makes a worktree on a
//! new branch, and runs the task there as a child `bwn run --json` with the
//! session's policy to narrow to (2.3). It has no terminal, so a call that
//! would ask is refused: it can't answer its own approvals. Its edits are
//! committed only once its checks pass (3.8), each commit naming its
//! evidence record (3.7). The work comes back only through
//! `buildwithnexus bg merge <branch>`, a command the person runs, which
//! merges only when every commit on the branch verifies against its record.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A background agent that was started.
pub struct Started {
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
    pub child: std::process::Child,
}

/// Start `task` in a new worktree of `cwd`, with `args` from the session
/// (`workflow::session_child_args`), committing what passes its checks.
pub fn start(task: &str, cwd: &Path, args: Vec<String>) -> Result<Started, String> {
    let (path, branch, base) = crate::agent::make_isolated_worktree(cwd)?;
    let bin = std::env::current_exe().map_err(|e| format!("cannot find bwn ({e})"))?;
    let mut args = args;
    // Its own commits are how its work comes back, so they are made.
    let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
    args.insert(at, "--auto-commit".into());
    if args.last().map(String::as_str) != Some(task) {
        args.push("--".into());
        args.push(task.into());
    }
    let child = Command::new(bin)
        .args(&args)
        .current_dir(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start the background agent ({e})"))?;
    Ok(Started {
        path,
        branch,
        base,
        child,
    })
}

fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let no_hooks = format!("core.hooksPath={}", cwd.join(".bwn/no-hooks").display());
    let o = Command::new("git")
        .args(["-c", "core.fsmonitor=false", "-c", &no_hooks])
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
    }
}

// The branch → experiment-record index (roadmap 5.8), in bwn's folder.
fn experiments_index() -> PathBuf {
    crate::config::home().join("experiments.json")
}

fn experiment_of(branch: &str) -> Option<String> {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(experiments_index()).ok()?).ok()?;
    v[branch].as_str().map(str::to_string)
}

/// Roadmap 5.8: run `task` as `n` background agents, each in a worktree
/// of its own, and score each by the verification engine: whether every
/// commit verifies against its evidence record, whether its checks passed,
/// and how much it changed. The comparison is an evidence record of its
/// own; merging a variant names it.
pub fn experiment(
    cwd: &Path,
    n: usize,
    task: &str,
) -> Result<(String, Vec<serde_json::Value>), String> {
    let mut variants = Vec::new();
    for i in 0..n {
        let s = start(task, cwd, crate::workflow::session_child_args(task))?;
        let branch = s.branch.clone();
        let base = s.base.clone();
        let ok = s
            .child
            .wait_with_output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        let commits =
            git(cwd, &["rev-list", &format!("{base}..refs/heads/{branch}")]).unwrap_or_default();
        let commits: Vec<&str> = commits.lines().filter(|l| !l.is_empty()).collect();
        let verified = !commits.is_empty()
            && commits
                .iter()
                .all(|c| crate::evidence::verify(cwd, c).is_ok());
        let changed = git(
            cwd,
            &["diff", "--numstat", &format!("{base}..refs/heads/{branch}")],
        )
        .unwrap_or_default()
        .lines()
        .count();
        let head = git(cwd, &["rev-parse", &format!("refs/heads/{branch}")])
            .unwrap_or_default()
            .trim()
            .to_string();
        // Verified work first, then a run that ended well, then the smaller change.
        let score = i64::from(verified) * 1000 + i64::from(ok) * 100 - changed.min(99) as i64;
        variants.push(serde_json::json!({
            "variant": i + 1, "branch": branch, "head": head, "commits": commits.len(),
            "verified": verified, "run_succeeded": ok, "files_changed": changed, "score": score,
        }));
    }
    let rec = serde_json::json!({
        "version": 1,
        "kind": "experiment",
        "time": crate::knowledge::chrono_now_iso(),
        "task": task,
        "variants": variants,
    });
    let hash = crate::evidence::store(&rec).ok_or("could not write the experiment record")?;
    let mut index: serde_json::Value = std::fs::read_to_string(experiments_index())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    for v in rec["variants"].as_array().into_iter().flatten() {
        if let Some(b) = v["branch"].as_str() {
            index[b] = hash.clone().into();
        }
    }
    crate::config::ensure_home();
    let _ = std::fs::write(experiments_index(), index.to_string());
    Ok((
        hash,
        rec["variants"].as_array().cloned().unwrap_or_default(),
    ))
}

/// Merge `branch` into the current branch of `cwd` when every commit it
/// adds verifies against its evidence record; otherwise nothing is merged
/// and each commit that doesn't verify is named.
pub fn merge(cwd: &Path, branch: &str) -> Result<String, String> {
    if branch.starts_with('-') || branch.contains("..") {
        return Err(format!("`{branch}` isn't a branch name"));
    }
    git(
        cwd,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ],
    )
    .map_err(|_| format!("no branch `{branch}`"))?;
    let commits = git(
        cwd,
        &[
            "rev-list",
            "--reverse",
            &format!("HEAD..refs/heads/{branch}"),
        ],
    )?;
    let commits: Vec<&str> = commits.lines().filter(|l| !l.is_empty()).collect();
    if commits.is_empty() {
        return Err(format!("`{branch}` has nothing this branch doesn't"));
    }
    let failed: Vec<String> = commits
        .iter()
        .filter_map(|c| {
            crate::evidence::verify(cwd, c)
                .err()
                .map(|why| format!("  {}: {why}", &c[..12.min(c.len())]))
        })
        .collect();
    if !failed.is_empty() {
        return Err(format!(
            "not merged: {} of {} commit(s) on {branch} don't verify against an evidence record:\n{}",
            failed.len(),
            commits.len(),
            failed.join("\n")
        ));
    }
    // A variant of an experiment: the merge names the comparison it won.
    let message = match experiment_of(branch) {
        Some(h) => format!(
            "Merge background work from {branch}\n\n{}: {h}\n",
            crate::evidence::EXPERIMENT_TRAILER
        ),
        None => format!("Merge background work from {branch}\n"),
    };
    git(
        cwd,
        &[
            "merge",
            "--no-ff",
            "-m",
            &message,
            &format!("refs/heads/{branch}"),
        ],
    )?;
    Ok(format!(
        "merged {branch}: {} commit(s), each verified against its evidence record",
        commits.len()
    ))
}

static RUNNING: std::sync::Mutex<Vec<Started>> = std::sync::Mutex::new(Vec::new());

/// `/bg <task>`: start it and keep it with the session, which says when
/// it finishes (`poll`).
pub fn start_in_session(task: &str, cwd: &Path) -> Result<String, String> {
    let s = start(task, cwd, crate::workflow::session_child_args(task))?;
    let line = format!(
        "background agent on branch {} (in {}); it can't answer its own approvals, and its work merges only with `buildwithnexus bg merge {}`",
        s.branch,
        s.path.display(),
        s.branch
    );
    RUNNING.lock().unwrap_or_else(|e| e.into_inner()).push(s);
    Ok(line)
}

/// One notice per background agent that finished since the last call.
pub fn poll() -> Vec<String> {
    let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    let mut notes = Vec::new();
    running.retain_mut(|s| match s.child.try_wait() {
        Ok(Some(status)) => {
            notes.push(format!(
                "  background agent on {} finished ({}): `buildwithnexus bg merge {}` merges what verifies",
                s.branch,
                if status.success() { "done" } else { "it didn't succeed" },
                s.branch
            ));
            false
        }
        _ => true,
    });
    notes
}

/// `buildwithnexus bg run <task>` and `buildwithnexus bg merge <branch>`.
pub fn cli(args: &[String]) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    match args {
        [verb, rest @ ..] if verb == "run" && !rest.is_empty() => {
            let task = rest.join(" ");
            // Outside a session, the settings are the session.
            if let Some(s) = crate::config::load_settings() {
                crate::workflow::set_launch(
                    &s.provider,
                    &s.model,
                    s.base_url.as_deref().unwrap_or(""),
                    &s.permission,
                );
            }
            let child_args = crate::workflow::session_child_args(&task);
            match start(&task, &cwd, child_args) {
                Ok(s) => {
                    let out = s.child.wait_with_output();
                    let code = out.as_ref().ok().and_then(|o| o.status.code());
                    println!(
                        "background agent finished (exit {}): its work is on branch {} in {}; `buildwithnexus bg merge {}` merges what verifies",
                        code.map_or("?".into(), |c| c.to_string()),
                        s.branch,
                        s.path.display(),
                        s.branch
                    );
                    if let Ok(o) = out {
                        for l in String::from_utf8_lossy(&o.stdout).lines() {
                            println!("  {l}");
                        }
                    }
                    0
                }
                Err(e) => {
                    eprintln!("buildwithnexus: {e}");
                    1
                }
            }
        }
        [verb, n, rest @ ..] if verb == "try" && !rest.is_empty() => {
            let Ok(n) = n.parse::<usize>().map(|n| n.clamp(1, 8)) else {
                eprintln!("usage: buildwithnexus bg try <n> <task>");
                return 2;
            };
            if let Some(s) = crate::config::load_settings() {
                crate::workflow::set_launch(
                    &s.provider,
                    &s.model,
                    s.base_url.as_deref().unwrap_or(""),
                    &s.permission,
                );
            }
            match experiment(&cwd, n, &rest.join(" ")) {
                Ok((hash, variants)) => {
                    println!(
                        "experiment {} ({} variants), best first:",
                        &hash[..12],
                        variants.len()
                    );
                    let mut v = variants;
                    v.sort_by_key(|x| -x["score"].as_i64().unwrap_or(0));
                    for x in v {
                        println!(
                            "  {}  score {}  verified {}  files {}",
                            x["branch"].as_str().unwrap_or("?"),
                            x["score"],
                            x["verified"],
                            x["files_changed"]
                        );
                    }
                    println!("`buildwithnexus bg merge <branch>` merges one; the merge names this experiment");
                    0
                }
                Err(e) => {
                    eprintln!("buildwithnexus: {e}");
                    1
                }
            }
        }
        [verb, branch] if verb == "merge" => match merge(&cwd, branch) {
            Ok(line) => {
                println!("{line}");
                0
            }
            Err(e) => {
                println!("{e}");
                1
            }
        },
        _ => {
            eprintln!(
                "usage: buildwithnexus bg run <task> | bg try <n> <task> | bg merge <branch>"
            );
            2
        }
    }
}
