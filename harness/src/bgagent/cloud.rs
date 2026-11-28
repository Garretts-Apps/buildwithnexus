//! Cloud background agents (roadmap 4.6, the parity table's "cloud or remote
//! delegation"): a background agent that runs on the repository's own
//! GitHub Actions runners instead of this machine.
//!
//! `buildwithnexus bg cloud init` writes `.github/workflows/bwn-cloud.yml`,
//! which runs bwn's GitHub Action. `buildwithnexus bg run --cloud <task>`
//! dispatches it (through `gh`) on a pushed commit. On the runner, bwn does
//! the task on a new branch in auto mode, committing what passes its checks,
//! each commit naming its evidence record. `bg publish` then pushes the branch
//! and, on `bwn-evidence/<branch>`, the records those commits name.
//! Back here, `buildwithnexus bg fetch <branch>` fetches both and keeps a
//! record only when its content hashes to its name. Then `bg merge`
//! verifies every commit against its record, as for a local agent.
//!
//! What a cloud record attests is what bwn saw on the runner, under the
//! workflow's settings: the checks that ran there and their results. The
//! merge still refuses any commit whose files don't match the record that
//! names it.

use std::path::Path;
use std::process::{Command, Stdio};

/// The workflow file `bg cloud init` writes, relative to the repository.
pub const WORKFLOW_PATH: &str = ".github/workflows/bwn-cloud.yml";
/// Branches a cloud agent works on; the workflow refuses any other name.
pub const BRANCH_PREFIX: &str = "bwn/cloud-";
/// Where the records a cloud branch's commits name are pushed.
pub const EVIDENCE_PREFIX: &str = "bwn-evidence/";

/// The workflow. Inputs reach its scripts only as environment variables, so
/// a task's text can't become a command, and the branch is checked against
/// the cloud prefix before anything uses it.
pub fn workflow() -> String {
    WORKFLOW_TEMPLATE.replace("__VERSION__", env!("CARGO_PKG_VERSION"))
}

const WORKFLOW_TEMPLATE: &str = r#"# Written by `buildwithnexus bg cloud init`. A cloud background agent: bwn
# does the task on a new branch in auto mode, committing only what passes
# its checks, and pushes the branch and its evidence records.
# `buildwithnexus bg fetch <branch>` brings them back and `bg merge` merges
# what verifies.
#
# The model: set the repository variables BWN_PROVIDER and BWN_MODEL (and
# BWN_BASE_URL for a gateway), and the provider's key as a secret.
name: bwn cloud agent
on:
  workflow_dispatch:
    inputs:
      task:
        description: The task
        required: true
      branch:
        description: Branch for the work (bwn/cloud-...)
        required: true
      base:
        description: Commit to start from
        required: true

permissions:
  contents: write

concurrency:
  group: bwn-cloud-${{ inputs.branch }}

jobs:
  agent:
    runs-on: ubuntu-latest
    timeout-minutes: 60
    steps:
      - name: Check the branch name
        env:
          BRANCH: ${{ inputs.branch }}
        run: |
          [[ "$BRANCH" =~ ^bwn/cloud-[A-Za-z0-9._-]+$ ]] || { echo "::error::not a cloud branch: $BRANCH"; exit 1; }
          echo "NEXUS_HOME=$RUNNER_TEMP/bwn-home" >> "$GITHUB_ENV"

      # No token is left in .git/config: the agent and its model can read
      # that file. Only the publish step gets one.
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          ref: ${{ inputs.base }}
          fetch-depth: 0
          persist-credentials: false

      - name: Start the branch
        env:
          BRANCH: ${{ inputs.branch }}
        run: |
          git switch -c "$BRANCH"
          git config user.name "bwn cloud agent"
          git config user.email "bwn-cloud@users.noreply.github.com"

      - uses: actions/setup-node@949feb2413d6458794dcd2491c4babbbce0c15c1 # v7.1.0
        with:
          node-version: 20

      - name: Run the task
        uses: Garretts-Apps/buildwithnexus@v__VERSION__
        with:
          command: run
          prompt: ${{ inputs.task }}
          permission-mode: auto
          provider: ${{ vars.BWN_PROVIDER }}
          model: ${{ vars.BWN_MODEL }}
          base-url: ${{ vars.BWN_BASE_URL }}
          args: --auto-commit
          allow-outcomes: failed,step_limit,budget_stop
        env:
          ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}
          OPENAI_API_KEY: ${{ secrets.OPENAI_API_KEY }}
          OPENROUTER_API_KEY: ${{ secrets.OPENROUTER_API_KEY }}
          CUSTOM_API_KEY: ${{ secrets.CUSTOM_API_KEY }}

      # What passed its checks is committed, so it is published whatever the
      # run's outcome. The token reaches git through its environment only.
      - name: Push the branch and its evidence
        if: ${{ !cancelled() }}
        env:
          BRANCH: ${{ inputs.branch }}
          GH_TOKEN: ${{ github.token }}
        run: bwn bg publish "$BRANCH"
"#;

fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    super::git(cwd, args)
}

fn valid_branch(branch: &str) -> Result<(), String> {
    let rest = branch
        .strip_prefix(BRANCH_PREFIX)
        .ok_or_else(|| format!("`{branch}` isn't a cloud branch ({BRANCH_PREFIX}...)"))?;
    if rest.is_empty()
        || rest.contains("..")
        || !rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(format!("`{branch}` isn't a cloud branch name"));
    }
    Ok(())
}

/// `bg cloud init`: write the workflow, unless one is there already.
pub fn init(cwd: &Path, force: bool) -> Result<String, String> {
    let root = git(cwd, &["rev-parse", "--show-toplevel"])?;
    let path = Path::new(root.trim()).join(WORKFLOW_PATH);
    if path.exists() && !force {
        return Err(format!(
            "{} exists; `bg cloud init --force` replaces it",
            path.display()
        ));
    }
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(&path, workflow()).map_err(|e| e.to_string())?;
    Ok(format!(
        "wrote {}: commit and push it, set BWN_PROVIDER and BWN_MODEL as repository variables and the provider's key as a secret, then `buildwithnexus bg run --cloud <task>`",
        path.display()
    ))
}

/// The `gh` arguments that dispatch the workflow; each value is one
/// argument, never shell text.
pub fn dispatch_args(task: &str, branch: &str, base: &str, git_ref: &str) -> Vec<String> {
    vec![
        "workflow".into(),
        "run".into(),
        "bwn-cloud.yml".into(),
        "--ref".into(),
        git_ref.into(),
        "-f".into(),
        format!("task={task}"),
        "-f".into(),
        format!("branch={branch}"),
        "-f".into(),
        format!("base={base}"),
    ]
}

/// `bg run --cloud <task>`: dispatch the workflow on HEAD, which the remote
/// must already have, as must the workflow file.
pub fn dispatch(cwd: &Path, task: &str) -> Result<String, String> {
    let head = git(cwd, &["rev-parse", "HEAD"])?.trim().to_string();
    let git_ref = git(cwd, &["symbolic-ref", "--short", "HEAD"])
        .map_err(|_| "HEAD isn't on a branch; check out the branch to start from".to_string())?
        .trim()
        .to_string();
    let upstream = format!("refs/remotes/origin/{git_ref}");
    git(cwd, &["rev-parse", "--verify", "-q", &upstream])
        .map_err(|_| format!("origin has no branch `{git_ref}`; push it first"))?;
    git(cwd, &["merge-base", "--is-ancestor", "HEAD", &upstream])
        .map_err(|_| format!("HEAD isn't on origin/{git_ref}; push it first"))?;
    git(cwd, &["cat-file", "-e", &format!("{upstream}:{WORKFLOW_PATH}")]).map_err(|_| {
        format!("origin/{git_ref} has no {WORKFLOW_PATH}; `buildwithnexus bg cloud init`, commit and push it")
    })?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let branch = format!("{BRANCH_PREFIX}{stamp:x}-{}", std::process::id());
    let out = Command::new("gh")
        .args(dispatch_args(task, &branch, &head, &git_ref))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run gh ({e}); install the GitHub CLI and `gh auth login`"))?;
    if !out.status.success() {
        return Err(format!(
            "gh workflow run failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    remember(&branch, &head)?;
    Ok(format!(
        "cloud agent dispatched on {} from {}: when its run finishes (`gh run list --workflow bwn-cloud.yml`), `buildwithnexus bg fetch {branch}` brings it back and `bg merge {branch}` merges what verifies",
        branch,
        &head[..12.min(head.len())]
    ))
}

// Cloud branches this home dispatched, and the commit each started from:
// `fetch` takes only these, so a branch someone else pushed under the
// cloud prefix (with records that hash to their names, as anyone's can)
// isn't brought in.
fn dispatched_dir() -> std::path::PathBuf {
    crate::config::home().join("cloud")
}

fn record_name(branch: &str) -> String {
    branch.replace('/', "%")
}

fn remember(branch: &str, base: &str) -> Result<(), String> {
    crate::config::ensure_home();
    std::fs::create_dir_all(dispatched_dir()).map_err(|e| e.to_string())?;
    let rec = serde_json::json!({"branch": branch, "base": base});
    std::fs::write(dispatched_dir().join(record_name(branch)), rec.to_string())
        .map_err(|e| e.to_string())
}

fn dispatched_base(branch: &str) -> Option<String> {
    let text = std::fs::read_to_string(dispatched_dir().join(record_name(branch))).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    (v["branch"] == branch).then(|| v["base"].as_str().map(str::to_string))?
}

// The records the commits of HEAD not on any remote name.
fn named_records(cwd: &Path) -> Result<Vec<String>, String> {
    let fmt = format!(
        "--format=%(trailers:key={},valueonly)",
        crate::evidence::TRAILER
    );
    let out = git(cwd, &["log", &fmt, "HEAD", "--not", "--remotes"])?;
    let mut hashes: Vec<String> = out
        .split_whitespace()
        .map(str::to_string)
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .collect();
    hashes.sort();
    hashes.dedup();
    Ok(hashes)
}

/// `bg publish <branch>` (on the runner): push HEAD as `branch`, and on
/// `bwn-evidence/<branch>` a commit holding the records its commits name.
pub fn publish(cwd: &Path, branch: &str) -> Result<String, String> {
    valid_branch(branch)?;
    let hashes = named_records(cwd)?;
    let mut entries = String::new();
    for h in &hashes {
        let bytes = crate::evidence::raw(h).ok_or_else(|| format!("no record {h} in this home"))?;
        let blob = hash_object(cwd, &bytes)?;
        entries.push_str(&format!("100644 blob {blob}\t{h}.json\n"));
    }
    let tree = mktree(cwd, &entries)?;
    let commit = git(
        cwd,
        &[
            "-c",
            "user.name=bwn",
            "-c",
            "user.email=bwn@localhost",
            "commit-tree",
            &tree,
            "-m",
            &format!("Evidence records for {branch}"),
        ],
    )?
    .trim()
    .to_string();
    let mut push = Command::new("git");
    push.args(["-c", "core.fsmonitor=false", "push", "--force", "origin"])
        .arg(format!("HEAD:refs/heads/{branch}"))
        .arg(format!("{commit}:refs/heads/{EVIDENCE_PREFIX}{branch}"))
        .current_dir(cwd)
        .stdin(Stdio::null());
    // On the runner the checkout keeps no credentials; the workflow hands
    // this step its token, which goes to git as configuration in the
    // environment, never on a command line or in .git/config.
    if let Some(token) = std::env::var("GH_TOKEN").ok().filter(|t| !t.is_empty()) {
        let auth = crate::net::base64(format!("x-access-token:{token}").as_bytes());
        push.env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader")
            .env("GIT_CONFIG_VALUE_0", format!("AUTHORIZATION: basic {auth}"));
    }
    let o = push.output().map_err(|e| format!("cannot run git: {e}"))?;
    if !o.status.success() {
        return Err(format!(
            "push failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ));
    }
    Ok(format!(
        "pushed {branch} and {} evidence record(s) on {EVIDENCE_PREFIX}{branch}",
        hashes.len()
    ))
}

fn piped(cwd: &Path, args: &[&str], input: &[u8]) -> Result<String, String> {
    use std::io::Write;
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run git: {e}"))?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input)
        .map_err(|e| e.to_string())?;
    let o = child.wait_with_output().map_err(|e| e.to_string())?;
    if !o.status.success() {
        return Err(String::from_utf8_lossy(&o.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn hash_object(cwd: &Path, bytes: &[u8]) -> Result<String, String> {
    piped(cwd, &["hash-object", "-w", "--stdin"], bytes)
}

fn mktree(cwd: &Path, entries: &str) -> Result<String, String> {
    piped(cwd, &["mktree"], entries.as_bytes())
}

/// `bg fetch <branch>`: fetch a cloud branch and its evidence, keeping each
/// record only when its content hashes to its name.
pub fn fetch(cwd: &Path, branch: &str) -> Result<String, String> {
    valid_branch(branch)?;
    let base = dispatched_base(branch).ok_or_else(|| {
        format!("`{branch}` wasn't dispatched from here (`bg run --cloud`); nothing fetched")
    })?;
    let ev_ref = format!("refs/bwn-evidence/{branch}");
    git(
        cwd,
        &[
            "fetch",
            "--no-tags",
            "origin",
            &format!("+refs/heads/{branch}:refs/heads/{branch}"),
            &format!("+refs/heads/{EVIDENCE_PREFIX}{branch}:{ev_ref}"),
        ],
    )?;
    // The work must start from the commit it was dispatched on.
    if git(
        cwd,
        &[
            "merge-base",
            "--is-ancestor",
            &base,
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_err()
    {
        let _ = git(cwd, &["update-ref", "-d", &format!("refs/heads/{branch}")]);
        return Err(format!(
            "`{branch}` doesn't start from {}, the commit it was dispatched on; not kept",
            &base[..12.min(base.len())]
        ));
    }
    let names = git(cwd, &["ls-tree", "--name-only", &ev_ref])?;
    let (mut kept, mut refused) = (0, Vec::new());
    for name in names.lines().filter(|l| !l.is_empty()) {
        let Some(hash) = name.strip_suffix(".json") else {
            refused.push(format!("{name}: not a record"));
            continue;
        };
        let bytes = Command::new("git")
            .args(["cat-file", "blob", &format!("{ev_ref}:{name}")])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| e.to_string())?
            .stdout;
        match crate::evidence::import(hash, &bytes) {
            Ok(()) => kept += 1,
            Err(why) => refused.push(why),
        }
    }
    let mut line = format!(
        "fetched {branch}: kept {kept} evidence record(s); `buildwithnexus bg merge {branch}` merges what verifies"
    );
    if !refused.is_empty() {
        line.push_str(&format!(
            "\n  refused {} record(s):\n  {}",
            refused.len(),
            refused.join("\n  ")
        ));
    }
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_workflow_passes_inputs_only_through_the_environment() {
        let wf = workflow();
        // No `${{ inputs.* }}` inside a run: script.
        let mut in_run = false;
        for line in wf.lines() {
            let t = line.trim_start();
            if t.starts_with("run:") {
                in_run = true;
                assert!(!t.contains("${{"), "{line}");
                continue;
            }
            if in_run && (t.starts_with("- ") || t.starts_with("uses:") || t.starts_with("env:")) {
                in_run = false;
            }
            if in_run {
                assert!(!t.contains("${{ inputs."), "an input in a script: {line}");
            }
        }
        assert!(wf.contains("^bwn/cloud-[A-Za-z0-9._-]+$"));
        // The checkout keeps no credentials, and the token appears only in
        // the publish step, after the agent has run.
        assert!(wf.contains("persist-credentials: false"));
        let publish = wf.find("- name: Push the branch and its evidence").unwrap();
        assert!(
            !wf[..publish].contains("github.token"),
            "a token before the publish step"
        );
        assert_eq!(wf.matches("github.token").count(), 1);
        // Actions are pinned by commit, as in the repository's own CI.
        for line in wf.lines().filter(|l| l.contains("uses: actions/")) {
            let sha = line
                .split('@')
                .nth(1)
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap();
            assert!(
                sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
                "{line}"
            );
        }
        assert!(wf.contains(&format!("buildwithnexus@v{}", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn a_task_is_one_argument_to_gh() {
        let a = dispatch_args("rm -rf /; $(evil) `x`", "bwn/cloud-1", "abc", "main");
        assert!(a.contains(&"task=rm -rf /; $(evil) `x`".to_string()));
        assert_eq!(a.iter().filter(|x| x.starts_with("task=")).count(), 1);
    }

    #[test]
    fn only_cloud_branch_names_are_accepted() {
        assert!(valid_branch("bwn/cloud-1a2b-77").is_ok());
        for b in [
            "main",
            "bwn/cloud-",
            "bwn/cloud-../x",
            "bwn/cloud-a b",
            "bwn/cloud-a;b",
            "-x",
        ] {
            assert!(valid_branch(b).is_err(), "{b}");
        }
    }
}
