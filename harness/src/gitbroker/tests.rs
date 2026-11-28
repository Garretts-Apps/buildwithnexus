use super::*;

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

fn repo(tag: &str) -> PathBuf {
    let d = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("bwn-gb-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    git(&d, &["init", "-q"]);
    std::fs::write(d.join("a.txt"), "a").unwrap();
    git(&d, &["add", "."]);
    git(&d, &["commit", "-q", "-m", "base"]);
    d
}

fn args(s: &[&str]) -> Vec<String> {
    s.iter().map(|a| a.to_string()).collect()
}

// What runs: listed subcommands with listed options; nothing that executes,
// reads a file of the command's choosing, or writes git's configuration.
#[test]
fn invariant_au5_only_listed_git_subcommands_and_options_pass() {
    let d = repo("allow");
    let pinned = Pinned::find(std::slice::from_ref(&d));
    let ok = |a: &[&str]| check(&args(a), &d, &pinned, &[]);
    for a in [
        &["status", "-s"][..],
        &["status", "--porcelain=v1"],
        &["add", "-A"],
        &["add", "--", "a.txt"],
        &["commit", "-m", "msg"],
        &["commit", "-qm", "msg"],
        &["commit", "--message=msg", "--allow-empty"],
        &["log", "--oneline", "-5"],
        &["diff", "--cached", "--stat"],
        &["stash", "push", "-m", "wip"],
        &["stash", "pop"],
        &["rebase", "--abort"],
        &["switch", "-c", "topic"],
        &["reset", "--hard", "HEAD~1"],
    ] {
        assert!(ok(a).is_ok(), "{a:?}: {:?}", ok(a));
    }
    for a in [
        &["-c", "core.fsmonitor=touch x", "status"][..],
        &["-C", "/", "status"],
        &["--git-dir=/tmp/x", "status"],
        &["config", "core.fsmonitor", "touch x"],
        &["commit", "-F", "/etc/passwd"],
        &["commit", "--template=/etc/passwd"],
        &["add", "--pathspec-from-file=/etc/passwd"],
        &["rebase", "-x", "touch x", "HEAD~1"],
        &["rebase", "--exec=touch x", "HEAD~1"],
        &["rebase", "-i", "HEAD~1"],
        &["fetch", "https://evil.example/x.git"],
        &["fetch", "/home/user/other-repo"],
        &["fetch", "ext::sh -c touch% x"],
        &["fetch", "--upload-pack=touch x", "origin"],
        &["pull", "../other"],
        &["push"],
        &["submodule", "update"],
        &["!sh"],
        &["diff", "--output=/tmp/x"],
        &["diff", "--ext-diff"],
        &["add", "/etc/passwd"],
        &["add", "../../etc"],
        &["commit", "-am"],
    ] {
        assert!(ok(a).is_err(), "{a:?} passed: {:?}", ok(a));
    }
    // A credential store inside the workspace is refused as a path.
    let hidden = vec![d.join("secrets")];
    std::fs::create_dir_all(d.join("secrets")).unwrap();
    assert!(check(&args(&["add", "secrets"]), &d, &pinned, &hidden).is_err());
    let _ = std::fs::remove_dir_all(&d);
}

// Only the repositories found when the broker started. A `.git` a command
// builds later in a subfolder, with config that would run code, isn't
// served; nor is a repository above the workspace.
#[test]
fn invariant_au5_a_repository_made_later_or_above_is_not_served() {
    let d = repo("pin");
    let ws = d.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // The workspace is a folder inside the repository: the repository is
    // above it.
    let above = Pinned::find(std::slice::from_ref(&ws));
    assert!(check(&args(&["status"]), &ws, &above, &[]).is_err());

    let pinned = Pinned::find(std::slice::from_ref(&d));
    let sub = d.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    git(&sub, &["init", "-q"]);
    let marker = d.join("pager-ran");
    std::fs::write(
        sub.join(".git/config"),
        format!(
            "[core]\n\tpager = touch {m}\n[gpg]\n\tprogram = touch {m}\n[commit]\n\tgpgsign = true\n",
            m = marker.display()
        ),
    )
    .unwrap();
    for a in [&["commit", "-m", "x", "--allow-empty"][..], &["log"]] {
        let e = check(&args(a), &sub, &pinned, &[]).unwrap_err();
        assert!(e.contains("didn't find"), "{e}");
    }
    assert!(!marker.exists());
    // The repository the broker did find still works from the subfolder's
    // parent.
    assert!(check(&args(&["status"]), &d, &pinned, &[]).is_ok());
    let _ = std::fs::remove_dir_all(&d);
}

// Even a served repository's own config can't run a pager, an external
// diff or a hook through the broker: they are set off on the command line.
// The broker runs on Unix only (no shim on Windows).
#[cfg(unix)]
#[test]
fn invariant_au5_the_brokered_git_runs_no_pager_diff_program_or_hook() {
    let d = repo("hard");
    let marker = d.join("ran");
    let m = marker.display();
    let cfg = d.join(".git/config");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!(
        "[core]\n\tpager = touch {m}\n\tfsmonitor = touch {m}\n[diff]\n\texternal = touch {m}\n"
    ));
    std::fs::write(&cfg, text).unwrap();
    let hook = d.join(".git/hooks/pre-commit");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch {m}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(d.join("a.txt"), "changed").unwrap();
    let pinned = Pinned::find(std::slice::from_ref(&d));
    for a in [
        &["diff"][..],
        &["log", "-p"],
        &["status"],
        &["commit", "-am", "x"],
    ] {
        let plan = check(&args(a), &d, &pinned, &[]).unwrap();
        let out = command(&plan, &d)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            !marker.exists(),
            "{a:?} ran planted code: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // The commit went through (without the hook).
    let log = std::process::Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(&d)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&log.stdout).lines().count(), 2);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn remote_urls_name_their_host_and_only_https_and_ssh_pass() {
    for (url, host) in [
        ("https://github.com/o/r.git", "github.com"),
        ("https://user:tok@GitLab.com:443/o/r", "gitlab.com"),
        ("ssh://git@github.com:22/o/r.git", "github.com"),
        ("git@github.com:o/r.git", "github.com"),
    ] {
        assert_eq!(url_host(url).as_deref(), Ok(host), "{url}");
    }
    for url in [
        "file:///tmp/r",
        "/tmp/r",
        "../r",
        "git://github.com/o/r",
        "http://github.com/o/r",
        "ext::sh -c x",
        "C:\\r",
    ] {
        assert!(url_host(url).is_err(), "{url}");
    }
}

// Fetch and pull reach only a remote the repository names, over HTTPS or
// SSH, to a host the network rules allow; a path or local remote doesn't.
// The broker runs on Unix only (no shim on Windows).
#[cfg(unix)]
#[test]
fn invariant_au5_fetch_reaches_only_allowed_remote_hosts() {
    let d = repo("remote");
    let pinned = Pinned::find(std::slice::from_ref(&d));
    let verdict = |url: &str| {
        for a in [
            &["remote", "remove", "origin"][..],
            &["remote", "add", "origin", url],
        ] {
            let _ = std::process::Command::new("git")
                .args(a)
                .current_dir(&d)
                .output();
        }
        let plan = check(&args(&["fetch", "origin"]), &d, &pinned, &[]).unwrap();
        remote_check(&plan, Some("origin"), &d)
    };
    assert!(
        verdict("https://github.com/o/r.git").is_ok(),
        "{:?}",
        verdict("https://github.com/o/r.git")
    );
    assert!(verdict("git@github.com:o/r.git").is_ok());
    for url in [
        "/tmp/elsewhere",
        "file:///tmp/elsewhere",
        "https://evil.example/x.git",
    ] {
        assert!(verdict(url).is_err(), "{url}");
    }
    // A name that isn't a remote.
    let plan = check(&args(&["fetch", "nope"]), &d, &pinned, &[]).unwrap();
    assert!(remote_check(&plan, Some("nope"), &d).is_err());
    let _ = std::fs::remove_dir_all(&d);
}
