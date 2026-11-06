//! `bwn review` and /review: gathering the diff, running the reviewers, and reporting their findings.

use super::*;

// ── review ───────────────────────────────────────────────────────────────────

/// `bwn review` exits with this when a finding is blocking.
pub(super) const EXIT_REVIEW_BLOCKING: i32 = 9;
// Blocking findings of the headless review in this process.
pub(super) static REVIEW_BLOCKING: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
// The diff a review sends is cut here; the model can read files for more.
pub(super) const MAX_REVIEW_DIFF_BYTES: usize = 200 * 1024;

/// What to review: `--base <ref>` (the branch since it forked, plus
/// uncommitted changes), `--staged`, or by default everything not yet
/// committed; any other words are the focus.
#[derive(Debug, Default, PartialEq)]
pub(super) struct ReviewRequest {
    pub(super) base: Option<String>,
    pub(super) staged: bool,
    pub(super) focus: String,
}

impl ReviewRequest {
    pub(super) fn parse(args: &[String]) -> Result<Self, String> {
        let mut req = ReviewRequest::default();
        let mut focus = Vec::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let (flag, inline) = a
                .split_once('=')
                .map_or((a.as_str(), None), |(k, v)| (k, Some(v)));
            match flag {
                "--base" => {
                    let v = inline
                        .map(str::to_string)
                        .or_else(|| it.next().cloned())
                        .filter(|v| !v.trim().is_empty() && !v.starts_with('-'))
                        .ok_or("--base needs a git ref (e.g. --base origin/main)")?;
                    req.base = Some(v);
                }
                "--staged" => req.staged = true,
                f if looks_like_option(f) => {
                    return Err(unknown_option_among(f, &["--base", "--staged"]))
                }
                _ => focus.push(a.clone()),
            }
        }
        if req.staged && req.base.is_some() {
            return Err("give --base or --staged, not both".into());
        }
        req.focus = focus.join(" ");
        Ok(req)
    }

    // What the review covers, in words and as git diff arguments.
    pub(super) fn diffs(&self) -> Vec<(String, Vec<String>)> {
        match (&self.base, self.staged) {
            (Some(b), _) => vec![
                (format!("changes since {b}"), vec![format!("{b}...HEAD")]),
                ("uncommitted changes".into(), vec!["HEAD".into()]),
            ],
            (None, true) => vec![("staged changes".into(), vec!["--staged".into()])],
            (None, false) => vec![("uncommitted changes".into(), vec!["HEAD".into()])],
        }
    }
}

// The diff text for `req`, each part headed; empty when nothing changed.
pub(super) fn review_diff(req: &ReviewRequest, cwd: &std::path::Path) -> Result<String, String> {
    if let Some(base) = &req.base {
        check_review_base(base, cwd)?;
    }
    let mut out = String::new();
    for (what, args) in req.diffs() {
        let mut cmd = vec!["--no-pager", "diff", "--no-color", "--no-ext-diff"];
        cmd.extend(args.iter().map(String::as_str));
        let text = git_in(cwd, &cmd).map_err(|e| format!("git diff failed: {e}"))?;
        if !text.trim().is_empty() {
            out.push_str(&format!("### {what}\n```diff\n{text}\n```\n"));
        }
    }
    // A file git does not track yet is not yet committed either.
    if !req.staged {
        out.push_str(&untracked_diff(cwd));
    }
    if out.len() > MAX_REVIEW_DIFF_BYTES {
        let cut = config::cut_at_char_boundary(&out, MAX_REVIEW_DIFF_BYTES).len();
        out.truncate(cut);
        out.push_str("\n…(diff cut at 200 KiB — read the files for the rest)\n");
    }
    Ok(out)
}

// `base...HEAD` needs the base and a commit both sides share. A CI
// checkout (actions/checkout's default fetch-depth 1) has neither, and
// git's own error is a usage line naming nothing.
pub(super) fn check_review_base(base: &str, cwd: &std::path::Path) -> Result<(), String> {
    const CI: &str = "in GitHub Actions, give actions/checkout `fetch-depth: 0`";
    if git_in(
        cwd,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{base}^{{commit}}"),
        ],
    )
    .is_err()
    {
        let remotes = git_in(cwd, &["remote"]).unwrap_or_default();
        let fetch = match base.split_once('/') {
            Some((remote, branch)) if remotes.lines().any(|r| r == remote) => {
                format!("git fetch {remote} {branch}")
            }
            Some(("origin", branch)) => format!("git fetch origin {branch}"),
            _ => format!("git fetch origin {base}"),
        };
        return Err(format!(
            "`{base}` is not in this checkout — fetch it (`{fetch}`), or {CI}"
        ));
    }
    if git_in(cwd, &["merge-base", base, "HEAD"]).is_err() {
        return Err(format!(
            "HEAD has no history in common with `{base}` here — in a shallow clone, `git fetch --unshallow`; {CI}"
        ));
    }
    Ok(())
}

// Files git neither tracks nor ignores, each as a new-file diff. Key and
// credential files are named but not sent, as the file tools hide them;
// links and binary files are named only.
pub(super) fn untracked_diff(cwd: &std::path::Path) -> String {
    use std::io::Read;
    let Ok(list) = git_in(cwd, &["ls-files", "--others", "--exclude-standard", "-z"]) else {
        return String::new();
    };
    let mut body = String::new();
    let mut left_out = Vec::new();
    for rel in list.split('\0').filter(|p| !p.is_empty()) {
        if body.len() > MAX_REVIEW_DIFF_BYTES {
            break;
        }
        let path = cwd.join(rel);
        if tools::is_sensitive(&path) {
            left_out.push(rel);
            continue;
        }
        let regular = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file());
        let mut bytes = Vec::new();
        let read = regular
            && std::fs::File::open(&path)
                .and_then(|f| f.take(MAX_REVIEW_DIFF_BYTES as u64).read_to_end(&mut bytes))
                .is_ok();
        body.push_str(&format!("--- /dev/null\n+++ b/{rel}\n"));
        match std::str::from_utf8(&bytes) {
            Ok(text) if read && !bytes.contains(&0) => {
                for l in text.lines() {
                    body.push('+');
                    body.push_str(l);
                    body.push('\n');
                }
            }
            _ => body.push_str("(not text: a link, a folder or a binary file)\n"),
        }
    }
    let mut out = String::new();
    if !body.is_empty() {
        out.push_str(&format!(
            "### new files, not yet tracked\n```diff\n{body}```\n"
        ));
    }
    if !left_out.is_empty() {
        out.push_str(&format!(
            "(new files left out because they may hold keys or credentials: {})\n",
            left_out.join(", ")
        ));
    }
    out
}

pub(super) fn review_task(req: &ReviewRequest, diff: &str) -> String {
    let focus = if req.focus.is_empty() {
        String::new()
    } else {
        format!("Focus on: {}.\n", req.focus)
    };
    format!(
        "Review this change as a careful senior reviewer. Read the files around it when \
         the diff alone is not enough. You cannot edit anything.\n{focus}\n\
         End with the findings, one per line, exactly in this form:\n\
         - [blocking] path/to/file.rs:42 — what is wrong and why it matters\n\
         Severities: blocking (a bug, security hole or data loss that must be fixed \
         before merging), major, minor, nit. Leave out the location when there is none. \
         If there is nothing to report, say: No findings.\n\n{diff}"
    )
}

#[derive(Debug, PartialEq)]
pub(super) struct Finding {
    pub(super) severity: String,
    pub(super) path: Option<String>,
    pub(super) line: Option<u64>,
    pub(super) message: String,
}

// `- [blocking] src/a.rs:42 — message` lines of a review answer.
pub(super) fn parse_findings(text: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let l = raw.trim().trim_start_matches(['-', '*', '•']).trim_start();
        let Some(rest) = l.strip_prefix('[') else {
            continue;
        };
        let Some((sev, body)) = rest.split_once(']') else {
            continue;
        };
        let severity = sev.trim().to_ascii_lowercase();
        if !matches!(severity.as_str(), "blocking" | "major" | "minor" | "nit") {
            continue;
        }
        let body = body.trim();
        let (loc, message) = match body.split_once(" — ").or_else(|| body.split_once(" - ")) {
            Some((loc, msg)) if !loc.contains(' ') => (Some(loc.trim_matches('`')), msg.trim()),
            _ => (None, body),
        };
        let (path, line) = match loc.and_then(|l| l.rsplit_once(':')) {
            Some((p, n)) if n.parse::<u64>().is_ok() => (Some(p.to_string()), n.parse().ok()),
            _ => (loc.map(str::to_string), None),
        };
        out.push(Finding {
            severity,
            path,
            line,
            message: message.to_string(),
        });
    }
    out
}

// `bwn review`: one read-only review turn over the diff; a `finding`
// event per issue, and exit 9 when one is blocking.
pub(super) fn headless_review(
    p: &Provider,
    req: &ReviewRequest,
    cwd: &std::path::Path,
) -> Result<(), String> {
    let diff = review_diff(req, cwd)?;
    if diff.trim().is_empty() {
        report::notice("  nothing to review: no changes");
        return Ok(());
    }
    let mut transcript = Vec::new();
    let sid = session::claim_or_new();
    let text = agent::run_review(p, &review_task(req, &diff), cwd, &mut transcript, &sid)?;
    let findings = parse_findings(&text);
    for f in &findings {
        report::finding(&f.severity, f.path.as_deref(), f.line, &f.message);
    }
    let blocking = findings.iter().filter(|f| f.severity == "blocking").count();
    REVIEW_BLOCKING.store(blocking, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

// `/review [--base <ref>|--staged] [focus]`: the same read-only review in
// the session's conversation.
pub(super) fn handle_review(
    provider: &Provider,
    args: &str,
    cwd: &std::path::Path,
    transcript: &mut Vec<provider::Msg>,
    sid: &str,
) {
    let words = shlex::split(args).unwrap_or_default();
    let req = match ReviewRequest::parse(&words) {
        Ok(r) => r,
        Err(e) => {
            tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
            tui::line(&tui::dim(
                "  usage: /review [--base <ref> | --staged] [focus]",
            ));
            return;
        }
    };
    let diff = match review_diff(&req, cwd) {
        Ok(d) if d.trim().is_empty() => {
            tui::line(&tui::dim("  nothing to review: no changes"));
            return;
        }
        Ok(d) => d,
        Err(e) => {
            tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
            return;
        }
    };
    let what: Vec<String> = req.diffs().into_iter().map(|(w, _)| w).collect();
    tui::line(&tui::accent(&format!(
        "  /review — {} (read-only)",
        what.join(" and ")
    )));
    tui::line("");
    if let Err(e) = agent::run_review(provider, &review_task(&req, &diff), cwd, transcript, sid) {
        tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
    }
    tui::bell();
}

/// Returns false when the permission gate or a PreToolUse hook refused the
/// project checks (which run the project's own build/test commands).
pub(super) fn handle_verify_audit(perm: Permission, cwd: &std::path::Path) -> bool {
    let check_input = serde_json::json!({});
    if let Some(reason) = agent::hook_gate(perm, "check_work", &check_input, cwd) {
        tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&reason))));
        tui::bell();
        return false;
    }
    tui::line(&tui::accent(
        "  verifying workspace against rules and tests",
    ));

    let mut changed_files = Vec::new();
    if let Ok(o) = std::process::Command::new("git")
        .args(["status", "-s"])
        .current_dir(cwd)
        .output()
    {
        if o.status.success() {
            let out = String::from_utf8_lossy(&o.stdout);
            for line in out.lines() {
                if line.len() > 3 {
                    let path = line[3..].trim().to_string();
                    changed_files.push(path);
                }
            }
        }
    }
    if changed_files.is_empty() {
        tui::line(&tui::dim(
            "  no modified git files — checking recent files in the workspace…",
        ));
        if let Ok(rd) = std::fs::read_dir(cwd) {
            for e in rd.flatten() {
                if let Some(s) = e.path().to_str() {
                    if !s.contains(".git") && !s.contains("target") && !s.contains("node_modules") {
                        changed_files.push(e.path().to_string_lossy().to_string());
                    }
                }
            }
        }
    }

    // Actually run the project's checks (build/test/lint) and feed the
    // verdict into the verifier's tests dimension instead of inferring it.
    let check = tools::run("check_work", &check_input, cwd);
    let tests_passed = if verify_check_had_nothing_to_run(&check.content) {
        None
    } else {
        Some(!check.is_error)
    };
    tui::line(&tui::render_md(&check.content));

    let verifier = crate::verifier::Verifier::new(&cwd.to_string_lossy());
    let ctx = crate::verifier::VerificationContext {
        task_description: "Interactive workspace verification and operational audit".to_string(),
        task_type: Some(crate::rules::TaskType::CodeReview),
        changed_files: changed_files.clone(),
        tool_calls: vec![],
        evidence_gathered: vec![],
        tests_added: vec![],
        dependencies_changed: vec![],
        tests_passed,
    };

    let report = verifier.verify(&ctx);
    // The report is Markdown — render it instead of echoing raw ##/**/`.
    let report_str = crate::verifier::Verifier::format_report(&report);
    tui::line(&tui::render_md(&report_str));
    tui::line(&tui::dim(
        "  tip: @rules:<id> or /rules inspects specific constraints",
    ));
    true
}

// check_work found no project checks to run (or every checker was missing):
// no verdict, rather than a false "tests passed".
pub(super) fn verify_check_had_nothing_to_run(report: &str) -> bool {
    report.contains("no build/test/lint commands detected") || report.contains("nothing ran")
}

#[cfg(test)]
mod review_tests {
    use super::*;

    #[test]
    fn review_targets_parse() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            ReviewRequest::parse(&a(&["--base", "origin/main", "auth", "paths"])).unwrap(),
            ReviewRequest {
                base: Some("origin/main".into()),
                staged: false,
                focus: "auth paths".into()
            }
        );
        assert!(ReviewRequest::parse(&a(&["--base=main"]))
            .unwrap()
            .base
            .is_some());
        assert!(ReviewRequest::parse(&a(&["--base"])).is_err());
        assert!(ReviewRequest::parse(&a(&["--staged", "--base", "x"])).is_err());
        assert!(ReviewRequest::parse(&a(&["--bsae", "x"]))
            .unwrap_err()
            .contains("did you mean --base"));
    }

    #[test]
    fn findings_are_read_from_the_answer() {
        let text = "Looks fine overall.\n\
                    - [blocking] src/auth.rs:42 — token compared with ==, timing leak\n\
                    * [nit] README.md — typo in the title\n\
                    - [minor] no tests for the new flag\n\
                    - [later] not a severity\n";
        let f = parse_findings(text);
        assert_eq!(f.len(), 3);
        assert_eq!(f[0].severity, "blocking");
        assert_eq!(f[0].path.as_deref(), Some("src/auth.rs"));
        assert_eq!(f[0].line, Some(42));
        assert_eq!(f[1].path.as_deref(), Some("README.md"));
        assert_eq!(f[1].line, None);
        assert_eq!(f[2].path, None);
        assert_eq!(f[2].message, "no tests for the new flag");
        assert!(parse_findings("No findings.").is_empty());
    }
}
