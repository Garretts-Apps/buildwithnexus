//! The REPL's slash commands: permissions, diffs and commits, context and cost, undo and rewind, help, and the rest.

use super::*;

// ── slash-command handlers ───────────────────────────────────────────────

pub(super) fn handle_compact(provider: &Provider, transcript: &mut Vec<provider::Msg>) {
    if transcript.is_empty() {
        tui::line(&tui::dim("  nothing to compact (empty transcript)"));
        return;
    }
    let before = transcript.len();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    hooks::pre_compact("manual", "", &cwd);
    let taken = std::mem::take(transcript);
    *transcript = agent::compact_msgs(provider, taken);
    usage::forget_last();
    let after = transcript.len();
    tui::line(&tui::green(&format!(
        "  ✓ compacted: {before} → {after} messages"
    )));
}

pub(super) fn handle_workflows() {
    let snaps = workflow::snapshots();
    if snaps.is_empty() {
        tui::line(&tui::dim(
            "  no workflows yet — /schedule or /loop to create one",
        ));
        return;
    }
    tui::line(&tui::accent("  background workflows"));
    tui::line(&tui::rule());
    for s in &snaps {
        let status_color = match s.status_str.as_str() {
            "running" => tui::blue(&s.status_str),
            "done" => tui::green(&s.status_str),
            "failed" => tui::red(&s.status_str),
            _ => tui::dim(&s.status_str),
        };
        let elapsed = s
            .elapsed_secs
            .map(|e| format!(" [{e}s]"))
            .unwrap_or_default();
        let iter_label = if s.iteration > 1 {
            format!(" ×{}", s.iteration)
        } else {
            String::new()
        };
        tui::line(&format!(
            "  #{:<3}  {}{}  [{}]  {}{}",
            s.id,
            status_color,
            elapsed,
            s.kind_str,
            tui::dim(&s.task),
            iter_label
        ));
        if let Some(why) = &s.reason {
            tui::line(&tui::dim(&format!(
                "        {}",
                tui::sanitize_terminal(why)
            )));
        }
    }
    tui::line(&tui::rule());
    tui::line(&tui::dim(
        "  c<id> cancel  ·  i<id> inspect output  ·  Enter dismiss",
    ));
    let action = tui::ask("  action: ").unwrap_or_default();
    let action = action.trim();
    if let Some(rest) = action.strip_prefix('c') {
        if let Ok(id) = rest.trim().parse::<usize>() {
            if workflow::cancel(id) {
                tui::line(&tui::yellow(&format!("  cancelled workflow #{id}")));
            } else {
                tui::line(&tui::dim(&format!(
                    "  workflow #{id} not found or already finished"
                )));
            }
        }
    } else if let Some(rest) = action.strip_prefix('i') {
        if let Ok(id) = rest.trim().parse::<usize>() {
            // The run's --json events, as the lines they stand for.
            let lines: Vec<String> = workflow::output(id)
                .iter()
                .filter_map(|l| workflow::readable(l))
                .collect();
            if lines.is_empty() {
                tui::line(&tui::dim(&format!(
                    "  no output captured for workflow #{id}"
                )));
            } else {
                tui::line(&tui::accent(&format!("  workflow #{id} output:")));
                // Child stderr is raw tool and model output.
                for l in lines.iter().take(100) {
                    tui::line(&format!("    {}", tui::dim(&tui::sanitize_terminal(l))));
                }
                if lines.len() > 100 {
                    tui::line(&tui::dim(&format!(
                        "  … ({} more lines)",
                        lines.len() - 100
                    )));
                }
            }
        }
    }
}

// Detect intent to switch agent mode from natural language input.
// Only catches unambiguous switch phrases — not ordinary task verbs like "plan this".
pub(super) fn detect_mode_switch(t: &str) -> Option<Mode> {
    let l = t.trim().to_lowercase();
    let l = l.trim_end_matches(['!', '.', '?']).trim();

    let verb_prefixes: &[&str] = &[
        "switch to ",
        "switch mode to ",
        "change to ",
        "change mode to ",
        "go to ",
        "set mode to ",
        "set mode ",
    ];
    for prefix in verb_prefixes {
        if let Some(rest) = l.strip_prefix(prefix) {
            let rest = rest.trim().trim_end_matches("mode").trim();
            match rest {
                "plan" | "planning" => return Some(Mode::Plan),
                "build" | "building" | "code" => return Some(Mode::Build),
                "brainstorm" | "brainstorming" => return Some(Mode::Brainstorm),
                _ => {}
            }
        }
    }
    // "use X mode" — the word "mode" makes the intent unambiguous.
    if let Some(rest) = l.strip_prefix("use ") {
        if let Some(name) = rest.trim().strip_suffix(" mode") {
            match name.trim() {
                "plan" | "planning" => return Some(Mode::Plan),
                "build" | "building" | "code" => return Some(Mode::Build),
                "brainstorm" | "brainstorming" => return Some(Mode::Brainstorm),
                _ => {}
            }
        }
    }
    // Bare "X mode" when that's the entire input (2 words or fewer).
    if t.split_whitespace().count() <= 2 {
        let bare = l.trim_end_matches("mode").trim();
        match bare {
            "plan" | "planning" => return Some(Mode::Plan),
            "build" | "building" => return Some(Mode::Build),
            "brainstorm" | "brainstorming" => return Some(Mode::Brainstorm),
            _ => {}
        }
    }
    None
}

// Detect intent to switch permission mode from natural language input.
pub(super) fn detect_permission_switch(t: &str) -> Option<&'static str> {
    let l = t.trim().to_lowercase();
    let l = l.trim_end_matches(['!', '.', '?']).trim();

    let verb_prefixes: &[&str] = &[
        "switch to ",
        "change to ",
        "change permission to ",
        "set permission to ",
        "set permission ",
        "use ",
    ];
    for prefix in verb_prefixes {
        if let Some(rest) = l.strip_prefix(prefix) {
            let rest = rest
                .trim()
                .trim_end_matches("mode")
                .trim()
                .trim_end_matches("permission")
                .trim();
            match rest {
                "ask" | "confirm" => return Some("ask"),
                "accept edits" | "accept-edits" | "acceptedits" => return Some("accept-edits"),
                "auto" | "yolo" | "approve all" => return Some("auto"),
                "readonly" | "read only" | "read-only" | "safe" => return Some("readonly"),
                _ => {}
            }
        }
    }
    // Bare "use readonly", "use ask" — short and unambiguous.
    if t.split_whitespace().count() <= 3 {
        match l.trim() {
            "readonly" | "read-only" | "read only" => return Some("readonly"),
            "auto permission" | "auto mode" => return Some("auto"),
            "ask permission" | "ask mode" => return Some("ask"),
            _ => {}
        }
    }
    None
}

// Writes only the changed keys into the user settings file. Saving the merged
// settings would copy a project's own settings into the user's global file.
pub(super) fn save_user_settings(changes: &[(&str, Option<serde_json::Value>)]) {
    if let Err(e) = config::save_user_settings(changes) {
        tui::line(&tui::yellow(&format!("  ⚠ not saved: {e}")));
    }
}

// How far a permission change reaches: a switch typed in the conversation
// (or `/permissions <mode>`) lasts for this session; only an explicit
// "save as default" writes the user settings file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum PermScope {
    Session,
    Default,
}

// Apply a permission name to the session, and save it as the default only
// when asked to.
pub(super) fn apply_permission(perm: &mut Permission, ps: &str, scope: PermScope) {
    let new = match agent::parse_permission(ps) {
        Ok(p) => p,
        Err(e) => {
            tui::line(&tui::red(&format!("  {e}")));
            return;
        }
    };
    *perm = new;
    let name = agent::permission_name(new);
    hooks::set_permission_mode(name);
    tui::set_permission_mode(permission_label(perm));
    match scope {
        PermScope::Session => tui::line(&tui::green(&format!(
            "  ✓ permission: {name} for this session — /permissions to make it the default"
        ))),
        PermScope::Default => {
            save_user_settings(&[("permission", Some(name.into()))]);
            tui::line(&tui::green(&format!(
                "  ✓ permission: {name} — saved as the default for every project"
            )));
        }
    }
}

pub(super) fn permission_label(perm: &Permission) -> &'static str {
    agent::permission_name(*perm)
}

// `/permissions <arg>`: a mode for this session, `default <mode>` to save
// it, or the saved-approval commands.
pub(super) fn handle_permissions_arg(perm: &mut Permission, cwd: &std::path::Path, arg: &str) {
    // The numbers are the picker's rows, 1-3 as in 0.14.
    let mode = |a: &str| match a {
        "1" => Some("ask"),
        "2" => Some("auto"),
        "3" => Some("readonly"),
        "4" => Some("accept-edits"),
        other => agent::parse_permission(other)
            .ok()
            .map(agent::permission_name),
    };
    if let Some(rest) = arg.strip_prefix("default") {
        match mode(rest.trim()) {
            Some(m) => apply_permission(perm, m, PermScope::Default),
            None => tui::line(&tui::red(
                "  usage: /permissions default <ask|accept-edits|auto|readonly>",
            )),
        }
        return;
    }
    match arg {
        "reset" => handle_permissions_reset(cwd),
        "list" => print_saved_approvals(cwd),
        other if other.starts_with("remove ") => {
            handle_permissions_remove(cwd, &other["remove ".len()..])
        }
        other => match mode(other) {
            Some(m) => apply_permission(perm, m, PermScope::Session),
            None => tui::line(&tui::red(&format!(
                "  unknown permission '{other}' — try: ask, accept-edits, auto, readonly, default <mode>, list, remove <entry>, reset"
            ))),
        },
    }
}

// `/permissions reset`: forget every "always allow" answer given in this
// project (the per-project map in the user settings file).
pub(super) fn handle_permissions_reset(cwd: &std::path::Path) {
    agent::clear_session_allowed(cwd);
    let n = config::reset_project_allowed(cwd);
    if n == 0 {
        tui::line(&tui::dim("  no \"always allow\" entries for this project"));
    } else {
        tui::line(&tui::green(&format!(
            "  ✓ cleared {n} \"always allow\" entr{} for {}",
            if n == 1 { "y" } else { "ies" },
            tui::sanitize_terminal(&cwd.display().to_string())
        )));
    }
}

// What `s` and `a` answers allow in this project, and how to take them back;
// then the allow, ask and deny rules in force, each with its file.
pub(super) fn print_saved_approvals(cwd: &std::path::Path) {
    let rules = config::policy_rules(cwd);
    if !rules.is_empty() {
        tui::line("  rules (deny > ask > allow > mode):");
        for r in &rules {
            let what = if r.network {
                format!("network.{}", r.effect.as_str())
            } else {
                r.effect.as_str().to_string()
            };
            tui::line(&format!(
                "    {what:<13} {}  {}",
                tui::sanitize_terminal(&r.rule),
                tui::dim(&format!("— {}", r.source))
            ));
        }
    }
    let always = config::load_project_allowed(cwd);
    let session = agent::session_allowed(cwd);
    if always.is_empty() && session.is_empty() {
        tui::line(&tui::dim(
            "  no saved approvals for this project (answer s or a at an approval to add one)",
        ));
        return;
    }
    for (title, keys) in [
        ("always allowed in this project", &always),
        ("allowed for this session", &session),
    ] {
        if keys.is_empty() {
            continue;
        }
        tui::line(&format!("  {title}:"));
        for k in keys {
            tui::line(&format!("    {}", tui::sanitize_terminal(k)));
        }
    }
    tui::line(&tui::dim(
        "  /permissions remove <entry> forgets one · /permissions reset forgets them all",
    ));
}

// `/permissions remove <entry>`: forget one saved approval, both the
// project's "always" entry and this session's.
pub(super) fn handle_permissions_remove(cwd: &std::path::Path, key: &str) {
    let key = key.trim().trim_matches('`');
    let always = config::remove_project_allowed(cwd, key);
    let session = agent::remove_session_allowed(cwd, key);
    let shown = tui::sanitize_terminal(key);
    if always || session {
        tui::line(&tui::green(&format!("  ✓ removed: {shown}")));
    } else {
        tui::line(&tui::yellow(&format!(
            "  no saved approval '{shown}' — /permissions list shows them"
        )));
    }
}

pub(super) fn handle_permissions(perm: &mut Permission, cwd: &std::path::Path) {
    if let Some(n) = agent::ignored_approvals_notice(cwd) {
        report::notice(&format!("  {n}"));
    }
    print_saved_approvals(cwd);
    let current = permission_label(perm);
    // 0.14's rows keep their places; accept-edits is added last, so a row
    // picked from memory never lands on a looser mode.
    let modes = [
        (
            "ask",
            "Confirm before each file write or command (recommended)",
        ),
        ("auto", "Auto-approve all safe tool operations (yolo)"),
        ("readonly", "Never write files or run mutating commands"),
        (
            "accept-edits",
            "Apply file edits in this project without asking; commands and network still ask",
        ),
    ];
    let items: Vec<tui::SelectItem> = modes
        .iter()
        .map(|(label, detail)| tui::SelectItem {
            label: (*label).into(),
            detail: (*detail).into(),
        })
        .collect();
    let title = format!("Select Tool Permission Mode (Current: {current})");
    let Some(&(mode, _)) = tui::select_item(&title, &items).and_then(|i| modes.get(i)) else {
        return;
    };
    let scopes = [
        tui::SelectItem {
            label: "this session".into(),
            detail: "Back to the saved default next time".into(),
        },
        tui::SelectItem {
            label: "save as default".into(),
            detail: "Every new session, in every project".into(),
        },
    ];
    match tui::select_item(&format!("Use {mode} for"), &scopes) {
        Some(0) => apply_permission(perm, mode, PermScope::Session),
        Some(1) => apply_permission(perm, mode, PermScope::Default),
        _ => {}
    }
}

// `/sandbox`: bare or `status` reports; a mode switches the session and
// persists to settings.json, like /permissions.
pub(super) fn handle_sandbox(arg: &str) {
    match arg {
        "" | "status" => {
            for l in sandbox::status_lines() {
                tui::line(&format!("  {l}"));
            }
        }
        other => match sandbox::Mode::parse(other) {
            Some(mode) => {
                sandbox::set_mode(mode);
                save_user_settings(&[("sandbox", Some(mode.as_str().into()))]);
                tui::line(&tui::green(&format!("  ✓ sandbox: {}", mode.as_str())));
                for l in sandbox::status_lines().into_iter().skip(1) {
                    tui::line(&tui::dim(&format!("  {l}")));
                }
            }
            None => tui::line(&tui::red(&format!(
                "  unknown sandbox mode '{other}' — try: off, auto, require, status"
            ))),
        },
    }
}

pub(super) fn handle_mouse(arg: Option<&str>) {
    let cmd = arg.unwrap_or("").trim();
    match cmd {
        "on" | "enable" => {
            tui::set_mouse_capture(true);
            tui::line(&tui::green(
                "  ✓ mouse: on — wheel scroll and drag-to-copy are enabled",
            ));
        }
        "off" | "disable" => {
            tui::set_mouse_capture(false);
            tui::line(&tui::green(
                "  ✓ mouse: off — terminal-native selection restored; use PgUp/PgDn to scroll",
            ));
        }
        "" | "toggle" => {
            let new_state = !tui::mouse_capture_enabled();
            tui::set_mouse_capture(new_state);
            if new_state {
                tui::line(&tui::green(
                    "  ✓ mouse: on — wheel scroll and drag-to-copy are enabled",
                ));
            } else {
                tui::line(&tui::green(
                    "  ✓ mouse: off — terminal-native selection restored; use PgUp/PgDn to scroll",
                ));
            }
        }
        "status" => {
            let state = if tui::mouse_capture_enabled() {
                "on"
            } else {
                "off"
            };
            tui::line(&tui::accent("  /mouse — mouse wheel scrolling"));
            tui::line(&format!("  Current: {}", tui::bold(state)));
            tui::line(&tui::dim(
                "  Default is on: wheel scrolls transcript and drag copies selected transcript text. /mouse off restores terminal-native selection.",
            ));
        }
        other => tui::line(&tui::red(&format!(
            "  unknown mouse setting '{other}' — try: on, off, toggle, status"
        ))),
    }
}

// Git for bwn's own commands (/commit, /diff), never for the model. A
// repository's config can name programs git runs (fsmonitor, an external
// diff, textconv, filters), so unless every repository-level key is known
// to be inert the person is asked first.
pub(super) fn git_may_run(
    cwd: &std::path::Path,
    ask: &mut dyn FnMut(&str) -> Option<String>,
) -> bool {
    if tools::skips_prompt_safely("git status", cwd) {
        return true;
    }
    let a = ask(
        "  this repository's git config can run programs (hooks, filters, an external diff) — run git here anyway? [y/N]: ",
    );
    matches!(a.as_deref().map(str::trim), Some("y" | "Y" | "yes" | "YES"))
}

// `@diff` and `@status` run git as /diff does: in a repository whose config
// can name programs the person is asked first, and without a terminal to
// ask, nothing is attached and the notice says why.
pub(super) fn git_attachment_may_run(word: &str, cwd: &std::path::Path) -> bool {
    if tools::skips_prompt_safely("git status", cwd) {
        return true;
    }
    if report::is_json() || !std::io::stdin().is_terminal() {
        report::notice(&format!(
            "  {word} not attached: this repository's git config can run programs (hooks, filters, an external diff)"
        ));
        return false;
    }
    git_may_run(cwd, &mut |q| tui::ask(q))
}

pub(super) fn git_text(cwd: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let o = std::process::Command::new("git")
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !o.status.success() {
        return Err(String::from_utf8_lossy(&o.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

// `/commit`: the model drafts the message, bwn shows it, and nothing is
// committed until the person answers c.
pub(super) fn handle_commit(provider: &Provider, cwd: &std::path::Path) {
    commit_flow(
        cwd,
        |stat, diff| agent::draft_commit_message(provider, stat, diff),
        &mut |q| tui::ask(q),
    );
}

// Returns the new commit's one-line summary when a commit was made.
pub(super) fn commit_flow(
    cwd: &std::path::Path,
    draft: impl FnOnce(&str, &str) -> Result<String, String>,
    ask: &mut dyn FnMut(&str) -> Option<String>,
) -> Option<String> {
    if !git_may_run(cwd, ask) {
        tui::line(&tui::dim("  cancelled — nothing committed"));
        return None;
    }
    let stat = match git_text(cwd, &["diff", "--staged", "--stat", "--no-ext-diff"]) {
        Ok(s) => s,
        Err(e) => {
            tui::line(&tui::red(&format!(
                "  /commit needs a git repository: {}",
                tui::sanitize_terminal(&e)
            )));
            return None;
        }
    };
    if stat.trim().is_empty() {
        tui::line(&tui::yellow(
            "  nothing is staged — `git add <files>` first, then /commit",
        ));
        return None;
    }
    let diff =
        git_text(cwd, &["diff", "--staged", "--no-ext-diff", "--no-textconv"]).unwrap_or_default();
    let mut msg = match draft(&stat, &diff) {
        Ok(m) => m,
        Err(e) => {
            tui::line(&tui::red(&format!(
                "  could not draft a commit message: {}",
                tui::sanitize_terminal(&e)
            )));
            return None;
        }
    };
    loop {
        tui::line(&tui::accent("  proposed commit message:"));
        // The draft is model text.
        for l in tui::sanitize_terminal(&msg).lines() {
            tui::line(&format!("    {l}"));
        }
        let answer = ask("  [c]ommit · [e]dit · [n]o: ");
        match answer.as_deref().map(str::trim) {
            Some("c" | "C" | "commit") => break,
            Some("e" | "E" | "edit") => {
                if let Some(new) = ask("  new message (Enter keeps the proposed one): ") {
                    if !new.trim().is_empty() {
                        msg = new.trim().to_string();
                    }
                }
            }
            _ => {
                tui::line(&tui::dim("  not committed — the changes stay staged"));
                return None;
            }
        }
    }
    match git_commit(cwd, &msg) {
        Ok(summary) => {
            checkpoint::note_commit(cwd);
            tui::line(&tui::green(&format!(
                "  ✓ committed {}",
                tui::sanitize_terminal(&summary)
            )));
            Some(summary)
        }
        Err(e) => {
            tui::line(&tui::red(&format!(
                "  git commit failed: {}",
                tui::sanitize_terminal(&e)
            )));
            None
        }
    }
}

pub(super) fn git_commit(cwd: &std::path::Path, msg: &str) -> Result<String, String> {
    use std::io::Write;
    let mut child = std::process::Command::new("git")
        .args(["-c", "core.fsmonitor=false", "commit", "-q", "-F", "-"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(msg.as_bytes());
    }
    let o = child
        .wait_with_output()
        .map_err(|e| format!("git commit: {e}"))?;
    if !o.status.success() {
        let mut out = String::from_utf8_lossy(&o.stderr).trim().to_string();
        if out.is_empty() {
            out = String::from_utf8_lossy(&o.stdout).trim().to_string();
        }
        return Err(out);
    }
    Ok(git_text(cwd, &["log", "-1", "--oneline", "--no-decorate"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default())
}

// One changed path in the working tree, as /diff lists it.
#[derive(Debug, PartialEq)]
pub(super) struct ChangedFile {
    /// Relative to the repository root; a new folder ends with '/'.
    pub(super) path: String,
    /// git's two status letters ("M ", " M", "??", "A ", " D", "R ").
    pub(super) status: String,
    pub(super) added: usize,
    pub(super) removed: usize,
}

impl ChangedFile {
    pub(super) fn kind(&self) -> &'static str {
        match self.status.as_str() {
            "??" if self.path.ends_with('/') => "new folder",
            "??" => "new",
            s if s.contains('D') => "deleted",
            s if s.contains('R') => "renamed",
            s if s.contains('A') => "added",
            _ => "modified",
        }
    }
}

// `git status --porcelain=v1 -z` entries as (status, path); a rename's
// source path is skipped.
pub(super) fn parse_porcelain(z: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut parts = z.split('\0');
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        let (xy, path) = (&entry[..2], &entry[3..]);
        if xy.contains(['R', 'C']) {
            parts.next();
        }
        out.push((xy.to_string(), path.to_string()));
    }
    out
}

// `git diff --numstat -z` (added, removed) by path; binary files count 0.
pub(super) fn parse_numstat(z: &str) -> std::collections::HashMap<String, (usize, usize)> {
    let mut out = std::collections::HashMap::new();
    for rec in z.split('\0') {
        let mut f = rec.splitn(3, '\t');
        let (Some(a), Some(r), Some(path)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        out.insert(
            path.trim_start_matches('\n').to_string(),
            (a.parse().unwrap_or(0), r.parse().unwrap_or(0)),
        );
    }
    out
}

// Lines in a new file, or in every file under a new folder (bounded).
pub(super) fn new_lines(path: &std::path::Path) -> usize {
    let count = |p: &std::path::Path| {
        std::fs::read(p)
            .ok()
            .filter(|b| b.len() <= MAX_DIFF_BYTES)
            .map(|b| b.iter().filter(|c| **c == b'\n').count())
            .unwrap_or(0)
    };
    if !path.is_dir() {
        return count(path);
    }
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    let mut seen = 0;
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            seen += 1;
            if seen > 2000 {
                return total;
            }
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                total += count(&p);
            }
        }
    }
    total
}

pub(super) const MAX_DIFF_BYTES: usize = 512 * 1024;

pub(super) fn changed_files(cwd: &std::path::Path) -> Result<(PathBuf, Vec<ChangedFile>), String> {
    let top = PathBuf::from(git_text(cwd, &["rev-parse", "--show-toplevel"])?.trim());
    let status = git_text(
        cwd,
        &["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
    )?;
    // Against HEAD when there is one: staged and unstaged changes together.
    let numstat = git_text(
        cwd,
        &[
            "diff",
            "HEAD",
            "--numstat",
            "-z",
            "--no-ext-diff",
            "--no-textconv",
        ],
    )
    .or_else(|_| {
        git_text(
            cwd,
            &["diff", "--cached", "--numstat", "-z", "--no-ext-diff"],
        )
    })
    .unwrap_or_default();
    let counts = parse_numstat(&numstat);
    let files = parse_porcelain(&status)
        .into_iter()
        .map(|(status, path)| {
            let (added, removed) = if status == "??" {
                (new_lines(&top.join(&path)), 0)
            } else {
                counts.get(&path).copied().unwrap_or((0, 0))
            };
            ChangedFile {
                path,
                status,
                added,
                removed,
            }
        })
        .collect();
    Ok((top, files))
}

pub(super) fn diff_summary(files: &[ChangedFile]) -> String {
    let added: usize = files.iter().map(|f| f.added).sum();
    let removed: usize = files.iter().map(|f| f.removed).sum();
    format!(
        "{} file{} changed, {added} insertion{}(+), {removed} deletion{}(-)",
        files.len(),
        crate::report::plural(files.len()),
        crate::report::plural(added),
        crate::report::plural(removed),
    )
}

// /diff: every changed and new file with its line counts and one summary;
// picking a file shows its diff. `/diff turn` shows what the last agent
// turn changed.
pub(super) fn handle_diff(cwd: &std::path::Path, arg: &str) {
    let mut ask = |q: &str| tui::ask(q);
    if !git_may_run(cwd, &mut ask) {
        tui::line(&tui::dim("  cancelled"));
        return;
    }
    if arg.trim() == "turn" {
        diff_last_turn(cwd);
        return;
    }
    let (top, files) = match changed_files(cwd) {
        Ok(v) => v,
        Err(e) => {
            tui::line(&tui::red(&format!(
                "  /diff needs a git repository: {}",
                tui::sanitize_terminal(&e)
            )));
            return;
        }
    };
    if files.is_empty() {
        tui::line(&tui::dim("  no changes — the working tree matches HEAD"));
        return;
    }
    // File names come from the checkout.
    for f in &files {
        tui::line(&format!(
            "  {:<10} {}  {}",
            tui::dim(f.kind()),
            tui::sanitize_terminal(&f.path),
            tui::dim(&format!("+{} -{}", f.added, f.removed))
        ));
    }
    tui::line(&tui::dim(&format!("  {}", diff_summary(&files))));
    // The picker draws over the rows above the composer; keep the list in view.
    if tui::is_raw() {
        for _ in 0..files.len() + 2 {
            tui::line("");
        }
    }
    let items: Vec<tui::SelectItem> = files
        .iter()
        .map(|f| tui::SelectItem {
            label: f.path.clone(),
            detail: format!("{} +{} -{}", f.kind(), f.added, f.removed),
        })
        .collect();
    if let Some(i) = tui::select_item("Show the diff of", &items) {
        show_file_diff(&top, &files[i]);
    }
}

pub(super) fn read_capped(p: &std::path::Path) -> Option<String> {
    std::fs::metadata(p)
        .ok()
        .filter(|m| m.len() as usize <= MAX_DIFF_BYTES)?;
    std::fs::read_to_string(p).ok()
}

pub(super) fn show_file_diff(top: &std::path::Path, f: &ChangedFile) {
    let path = top.join(&f.path);
    if f.status == "??" && path.is_dir() {
        let mut stack = vec![path];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Some(new) = read_capped(&p) {
                    let shown = p.strip_prefix(top).unwrap_or(&p).display().to_string();
                    report::diff(&shown, "", &new);
                }
            }
        }
        return;
    }
    let old = if f.status == "??" || f.status.contains('A') {
        Some(String::new())
    } else {
        git_text(top, &["show", &format!("HEAD:{}", f.path)])
            .ok()
            .filter(|t| t.len() <= MAX_DIFF_BYTES)
    };
    let new = if f.status.contains('D') {
        Some(String::new())
    } else {
        read_capped(&path)
    };
    match (old, new) {
        (Some(old), Some(new)) => report::diff(&f.path, &old, &new),
        _ => tui::line(&tui::dim(&format!(
            "  {} is too large or not text to show here — git diff -- {}",
            tui::sanitize_terminal(&f.path),
            tui::sanitize_terminal(&f.path)
        ))),
    }
}

// What the last agent turn changed: each checkpointed file against its
// state before the turn, and the files shell commands changed.
pub(super) fn diff_last_turn(cwd: &std::path::Path) {
    let Some(last) = checkpoint::last_turn(cwd) else {
        tui::line(&tui::dim("  no agent turn recorded in this folder"));
        return;
    };
    tui::line(&tui::dim(&format!(
        "  the last turn, '{}' ({}):",
        tui::sanitize_terminal(&last.turn.task),
        session::ago(last.turn.started_ms)
    )));
    // Oldest checkpoint per file holds its contents before the turn.
    let mut firsts: Vec<&checkpoint::Checkpoint> = Vec::new();
    for cp in last.checkpoints.iter().rev() {
        if !firsts.iter().any(|f| f.path == cp.path) {
            firsts.push(cp);
        }
    }
    for cp in &firsts {
        let shown = checkpoint::shown_path(&cp.path, cwd);
        let before = if cp.existed {
            Some(cp.content.clone())
        } else {
            Some(String::new())
        };
        match (
            before.filter(|_| cp.snapshotted),
            read_capped(&cp.path).or_else(|| (!cp.path.exists()).then(String::new)),
        ) {
            (Some(old), Some(new)) => report::diff(&shown, &old, &new),
            _ => tui::line(&tui::dim(&format!(
                "  {} — no snapshot to compare (too large or not text)",
                tui::sanitize_terminal(&shown)
            ))),
        }
    }
    if !last.turn.untracked.is_empty() {
        tui::line(&tui::dim("  changed by shell commands (against HEAD):"));
        for rel in &last.turn.untracked {
            tui::line(&tui::dim(&format!("    - {}", tui::sanitize_terminal(rel))));
        }
    }
    if firsts.is_empty() && last.turn.untracked.is_empty() {
        tui::line(&tui::dim("  it changed no files"));
    }
}

/// Where the next request's tokens go, estimated at four characters a token
/// (images by their encoded size, as compaction counts them).
#[derive(Debug, Default, PartialEq)]
pub(super) struct ContextBreakdown {
    pub(super) system: usize,
    pub(super) tools: usize,
    pub(super) mcp_tools: usize,
    pub(super) conversation: usize,
    pub(super) images: usize,
}

impl ContextBreakdown {
    pub(super) fn total(&self) -> usize {
        self.system + self.tools + self.mcp_tools + self.conversation + self.images
    }
}

pub(super) fn context_breakdown(
    msgs: &[provider::Msg],
    tools: &[tools::ToolDef],
) -> ContextBreakdown {
    let mut b = ContextBreakdown::default();
    for m in msgs {
        match m {
            provider::Msg::System(s) => b.system += s.len() / 4,
            provider::Msg::User(s) => b.conversation += s.len() / 4,
            provider::Msg::UserImages { text, images } => {
                b.conversation += text.len() / 4;
                b.images += images.iter().map(|(_, d)| d.len() / 3).sum::<usize>() / 4;
            }
            provider::Msg::Assistant { text, calls } => {
                b.conversation += (text.len()
                    + calls
                        .iter()
                        .map(|c| c.name.len() + c.input.to_string().len())
                        .sum::<usize>())
                    / 4;
            }
            provider::Msg::Tool(results) => {
                b.conversation += results.iter().map(|r| r.content.len()).sum::<usize>() / 4;
                b.images += results
                    .iter()
                    .flat_map(|r| &r.images)
                    .map(|(_, d)| d.len() / 3)
                    .sum::<usize>()
                    / 4;
            }
        }
    }
    for t in tools {
        let size = (t.name.len() + t.description.len() + t.schema.to_string().len()) / 4;
        if mcp::is_mcp_tool(t.name) {
            b.mcp_tools += size;
        } else {
            b.tools += size;
        }
    }
    b
}

// Tokens the next request would carry, as /context counts them.
pub(super) fn context_in_use(transcript: &[provider::Msg], total: usize) -> usize {
    let measured = if transcript.is_empty() {
        None
    } else {
        usage::last_context_tokens()
    };
    measured.unwrap_or_else(|| {
        let tools = tools::defs_for_context(true, total);
        context_breakdown(transcript, &tools).total()
    })
}

// Whether `p`'s window is bwn's guess for an endpoint that did not say: a
// local server or a custom endpoint, where the guess is 8k. A hosted
// preset's window is the provider's documented one.
pub(super) fn window_guessed(p: &Provider) -> bool {
    !provider::window_is_known(p)
        && (is_loopback_url(&p.base_url)
            || active_preset()
                .is_some_and(|id| id == "custom" || config::preset(&id).is_some_and(|pr| pr.local)))
}

// For /context and /teamwork: whether the window is a guess, and what the
// compact tool set it leads to leaves out. Empty when neither applies.
pub(super) fn window_notes(p: &Provider) -> Vec<String> {
    let window = provider::short_tokens(p.context_tokens);
    let mut out = Vec::new();
    if window_guessed(p) {
        out.push(format!(
            "this endpoint did not report its context window, so bwn assumes {window} tokens"
        ));
    }
    if tools::compact_surface(p.context_tokens) {
        out.push(format!(
            "at {window} tokens the model gets the compact tool set: no helpers, todo list \
             or screenshots"
        ));
    }
    if !out.is_empty() {
        out.push(
            "set \"context_tokens\" in settings.json to the model's real window \
             (a window of 32,768 tokens or more offers every tool)"
                .into(),
        );
    }
    out
}

pub(super) fn handle_context(transcript: &[provider::Msg], p: &Provider) {
    let total = p.context_tokens;
    let tools = tools::defs_for_context(true, total);
    let b = context_breakdown(transcript, &tools);
    let estimate = b.total();
    // The server's own count for the last request beats the chars/4 guess,
    // but only while the transcript it measured is still the live one.
    let measured = if transcript.is_empty() {
        None
    } else {
        usage::last_context_tokens()
    };
    let used = measured.unwrap_or(estimate);
    tui::context_meter(used, total);
    let pct = (used * 100).checked_div(total).unwrap_or(0);
    tui::line(&format!(
        "  context: {} of {} tokens ({pct}%)",
        provider::short_tokens(used),
        provider::short_tokens(total)
    ));
    let rows = [
        ("system prompt", b.system),
        ("tools", b.tools),
        ("MCP tools", b.mcp_tools),
        ("conversation", b.conversation),
        ("images", b.images),
    ];
    for (name, n) in rows {
        tui::line(&tui::dim(&format!(
            "    {name:<14} {:>7}",
            provider::short_tokens(n)
        )));
    }
    tui::line(&tui::dim(&match measured {
        Some(_) => format!(
            "  total measured from the last request; rows estimated at 4 characters a token · {} messages",
            transcript.len()
        ),
        None => format!(
            "  estimated at 4 characters a token — no usage reported yet · {} messages",
            transcript.len()
        ),
    }));
    if pct >= 80 {
        tui::line(&tui::yellow(
            "  nearly full — /compact summarizes the conversation to make room",
        ));
    }
    for note in window_notes(p) {
        tui::line(&tui::yellow(&format!("  {note}")));
    }
}

pub(super) fn handle_cost(provider: &Provider) {
    tui::line(&tui::accent("  session usage"));
    for l in usage::render(&usage::snapshot(), &provider.model) {
        tui::line(&tui::dim(&l));
    }
}

// `/effort` shows the level; `/effort <level>` applies it to the live
// provider and persists it to settings.json.
pub(super) fn handle_effort(provider: &mut Provider, arg: &str) {
    let arg = arg.trim();
    if arg.is_empty() {
        tui::line(&tui::dim(&format!(
            "  effort: {} — /effort off|low|medium|high",
            provider.effort
        )));
        return;
    }
    match config::Effort::parse(arg) {
        Some(level) => {
            provider.effort = level;
            save_user_settings(&[("reasoning_effort", Some(level.as_str().into()))]);
            tui::line(&tui::green(&format!(
                "  ✓ effort → {level} (saved to settings)"
            )));
        }
        None => tui::line(&tui::red(&format!(
            "  unknown effort '{arg}' — try: off, low, medium, high"
        ))),
    }
}

pub(super) const CHECKPOINT_ROWS: usize = 15;

// /checkpoints: newest first, grouped by the task that made them, with age
// and paths relative to this folder.
pub(super) fn handle_checkpoints(cwd: &std::path::Path) {
    let pruned = checkpoint::prune_once(cwd);
    if pruned > 0 {
        tui::line(&tui::dim(&format!(
            "  pruned {pruned} old checkpoints — this folder keeps the newest {}",
            checkpoint::KEEP_CHECKPOINTS
        )));
    }
    let items = checkpoint::list(cwd);
    if items.is_empty() {
        tui::line(&tui::dim("  no checkpoints for this directory"));
        return;
    }
    tui::line(&tui::dim(
        "  checkpoints in this folder, newest first — /undo <id> restores one, /undo the last task",
    ));
    let mut group: Option<(Option<&str>, String)> = None;
    for cp in items.iter().take(CHECKPOINT_ROWS) {
        let task = cp.task.as_deref();
        let age = session::ago(cp.created_ms);
        if group.as_ref() != Some(&(task, age.clone())) {
            // Task text is the person's prompt; paths are wherever the model wrote.
            tui::line(&format!(
                "  {}  {}",
                tui::bold(&age),
                tui::sanitize_terminal(task.unwrap_or("(task not recorded)"))
            ));
            group = Some((task, age));
        }
        tui::line(&format!(
            "    {}  {:<11} {}",
            tui::dim(&tui::sanitize_terminal(&cp.id)),
            tui::sanitize_terminal(&cp.action),
            tui::sanitize_terminal(&checkpoint::shown_path(&cp.path, cwd))
        ));
    }
    if items.len() > CHECKPOINT_ROWS {
        tui::line(&tui::dim(&format!(
            "  … {} older — this folder keeps the newest {}",
            items.len() - CHECKPOINT_ROWS,
            checkpoint::KEEP_CHECKPOINTS
        )));
    }
}

// The repository's instruction files steer the model: the first launch
// with this folder and content asks whether to use them, and they are used
// only on a yes (No is the default, as for every trust question). After a
// yes they are named in a dim line. The question takes whole answers, so a
// task typed meanwhile is never read as one: it is kept for the input box.
pub(super) fn review_repo_instructions(cwd: &std::path::Path) {
    let Some(repo) = config::repo_instructions(cwd) else {
        return;
    };
    // File names come from the checkout.
    let notice = tui::sanitize_terminal(&repo.notice()).into_owned();
    if repo.acknowledged(cwd) {
        tui::line(&tui::dim(&format!("  {notice}")));
        return;
    }
    // Answered no on the trust screen this launch.
    if config::repo_instructions_declined() {
        tui::line(&tui::dim(&format!(
            "  ✗ not using {notice} this session — asked again next time"
        )));
        return;
    }
    // Prompts piped in: the first one is not an answer to this.
    if !std::io::stdin().is_terminal() {
        tui::line(&tui::dim(&format!("  {notice} (not reviewed)")));
        return;
    }
    tui::line(&tui::yellow(&format!(
        "  {notice} — the model follows these files from the checkout"
    )));
    let mut held: Vec<String> = Vec::new();
    let used = loop {
        let Some(answer) = tui::ask("  use them? y yes · n no · r review [N]: ") else {
            break false;
        };
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => break true,
            "" | "n" | "no" => break false,
            "r" | "review" => show_instruction_files(&repo.files),
            _ => {
                held.push(answer);
                tui::line(&tui::dim(
                    "  (answer y or n first — your text is kept for the input box)",
                ));
            }
        }
    };
    if used {
        repo.acknowledge(cwd);
        tui::line(&tui::dim(&format!(
            "  ✓ using {notice} — asked again when they change"
        )));
    } else {
        config::decline_repo_instructions();
        tui::line(&tui::dim(&format!(
            "  ✗ not using {notice} this session — asked again next time"
        )));
    }
    if !held.is_empty() {
        tui::prefill_composer(&held.join("\n"));
    }
}

pub(crate) fn show_instruction_files(files: &[config::InstructionFile]) {
    for f in files {
        tui::line(&tui::bold(&format!(
            "  ── {} ──",
            tui::sanitize_terminal(&f.label)
        )));
        for l in tui::sanitize_terminal(&f.content).lines() {
            tui::line(&format!("    {l}"));
        }
        if f.truncated {
            tui::line(&tui::dim(
                "    … cut here: the model is sent the text above",
            ));
        }
    }
}

// A prompt of this run that /rewind can go back to: where its messages
// start in the transcript, when its turn began (checkpoints after it are its
// changes and later ones), the text sent, and the text as typed (before
// attached files were added), which goes back in the input box.
pub(super) struct RewindPoint {
    pub(super) index: usize,
    pub(super) started_ms: u128,
    pub(super) prompt: String,
    pub(super) typed: String,
}

// Drops the point's prompt and everything after it from the conversation.
// The prompt is found at or after its recorded index (a turn may put the
// system prompt first); a hook may have appended context to it. False when
// it is no longer there (compacted away), leaving the transcript alone.
pub(super) fn rewind_transcript(transcript: &mut Vec<provider::Msg>, point: &RewindPoint) -> bool {
    let start = point.index.min(transcript.len());
    let found = transcript[start..].iter().position(|m| match m {
        provider::Msg::User(t) | provider::Msg::UserImages { text: t, .. } => {
            t.starts_with(&point.prompt)
        }
        _ => false,
    });
    match found {
        Some(i) => {
            transcript.truncate(start + i);
            // Nothing but a system prompt left: an empty conversation.
            if transcript
                .iter()
                .all(|m| matches!(m, provider::Msg::System(_)))
            {
                transcript.clear();
            }
            true
        }
        None => false,
    }
}

// /rewind: pick an earlier prompt of this run and go back to just before it
// — the files its turn and later turns changed, the conversation from it on,
// or both.
pub(super) fn handle_rewind(
    transcript: &mut Vec<provider::Msg>,
    points: &mut Vec<RewindPoint>,
    sid: &str,
    cwd: &std::path::Path,
    model: &str,
) {
    if points.is_empty() {
        tui::line(&tui::dim(
            "  nothing to rewind in this session yet — /undo restores files from earlier turns",
        ));
        return;
    }
    let items: Vec<tui::SelectItem> = points
        .iter()
        .rev()
        .map(|p| {
            let first: String = p
                .prompt
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(70)
                .collect();
            let files = checkpoint::list(cwd)
                .iter()
                .filter(|c| c.created_ms >= p.started_ms)
                .count();
            tui::SelectItem {
                label: first,
                detail: format!(
                    "{} · {files} file change{} since",
                    session::ago(p.started_ms),
                    crate::report::plural(files)
                ),
            }
        })
        .collect();
    let Some(pick) = tui::select_item("Rewind to before", &items) else {
        return;
    };
    let at = points.len() - 1 - pick;
    let what = [
        tui::SelectItem {
            label: "Code and conversation".into(),
            detail: "restore the files and drop this prompt and everything after".into(),
        },
        tui::SelectItem {
            label: "Conversation only".into(),
            detail: "drop this prompt and everything after; files stay".into(),
        },
        tui::SelectItem {
            label: "Code only".into(),
            detail: "restore the files changed since; the conversation stays".into(),
        },
    ];
    let Some(choice) = tui::select_item("Rewind what", &what) else {
        return;
    };
    let (code, conversation) = (choice != 1, choice != 2);
    if code {
        let mut overwrite = |p: &std::path::Path| confirm_overwrite(cwd, p);
        match checkpoint::undo_all_since(cwd, points[at].started_ms, &mut overwrite) {
            Ok(undone) => {
                tui::line(&tui::green(&format!(
                    "  ✓ restored {} file{}:",
                    undone.restored.len(),
                    crate::report::plural(undone.restored.len())
                )));
                show_undone(cwd, &undone);
            }
            Err(e) if e.starts_with("no checkpoints") => {
                tui::line(&tui::dim("  no file changes since then"));
            }
            Err(e) => {
                tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
                return;
            }
        }
    }
    if conversation {
        let before = transcript.len();
        if rewind_transcript(transcript, &points[at]) {
            if transcript.is_empty() {
                let _ = session::remove(sid);
            } else {
                session::save(sid, cwd, model, transcript);
            }
            tui::line(&tui::green(&format!(
                "  ✓ conversation rewound — dropped {} message{}",
                before - transcript.len(),
                if before - transcript.len() == 1 {
                    ""
                } else {
                    "s"
                }
            )));
            tui::prefill_composer(&points[at].typed);
            points.truncate(at);
            tui::line(&tui::dim(
                "  your prompt is back in the input box — edit it and press Enter",
            ));
        } else {
            tui::line(&tui::yellow(
                "  that prompt is no longer in the conversation (it was compacted) — files only",
            ));
        }
    }
}

// The question /undo asks before it overwrites a file changed since the
// agent's edit. No (or Enter, Esc) keeps the file as it is.
pub(super) fn confirm_overwrite(cwd: &std::path::Path, path: &std::path::Path) -> bool {
    let shown = path.strip_prefix(cwd).unwrap_or(path).display().to_string();
    let q = format!(
        "  {} changed after the agent edited it — overwrite your changes? [y/N]: ",
        tui::sanitize_terminal(&shown)
    );
    matches!(
        tui::ask(&q).as_deref().map(str::trim),
        Some("y" | "Y" | "yes" | "YES")
    )
}

pub(super) fn show_undone(cwd: &std::path::Path, undone: &checkpoint::Undone) {
    for c in &undone.restored {
        tui::line(&format!(
            "    - {} ({})",
            tui::sanitize_terminal(&checkpoint::shown_path(&c.path, cwd)),
            tui::sanitize_terminal(&c.action)
        ));
    }
    for p in &undone.kept {
        let shown = p.strip_prefix(cwd).unwrap_or(p).display().to_string();
        tui::line(&tui::yellow(&format!(
            "    - kept your version of {}",
            tui::sanitize_terminal(&shown)
        )));
    }
}

// Bare /undo: reverts the last agent turn in this folder as a unit — the
// recovery for a partial multi-file edit, where undoing one file would
// quietly leave the rest changed — and says what it cannot undo: commits,
// files changed by shell commands, a turn from an earlier run (asked first).
pub(super) fn undo_last_turn(
    cwd: &std::path::Path,
    overwrite: &mut dyn FnMut(&std::path::Path) -> bool,
) {
    let last = checkpoint::last_turn(cwd);
    let committed = checkpoint::committed_since_turn(cwd);
    for note in undo_preamble(last.as_ref(), committed) {
        tui::line(&tui::yellow(note));
    }
    let Some(last) = last else {
        return;
    };
    let shell_note = |lead: &str| {
        tui::line(&tui::yellow(&format!(
            "  {lead} changed files with shell commands, which checkpoints do not track — git diff shows them:"
        )));
        for p in last.turn.untracked.iter().take(8) {
            tui::line(&tui::dim(&format!("    - {}", tui::sanitize_terminal(p))));
        }
        if last.turn.untracked.len() > 8 {
            tui::line(&tui::dim(&format!(
                "    … and {} more",
                last.turn.untracked.len() - 8
            )));
        }
    };
    if last.checkpoints.is_empty() {
        if last.turn.untracked.is_empty() {
            tui::line(&tui::yellow(
                "  the last agent turn made no file changes — use /undo latest, /undo <id>, or /undo all",
            ));
        } else {
            shell_note("that turn");
        }
        return;
    }
    let files: std::collections::HashSet<&std::path::Path> =
        last.checkpoints.iter().map(|c| c.path.as_path()).collect();
    if !last.this_session {
        // A turn from an earlier run: say which before touching anything.
        let q = format!(
            "  undo the last task in this folder, '{}' ({}, {} file{})? [y/N]: ",
            tui::sanitize_terminal(&last.turn.task),
            session::ago(last.turn.started_ms),
            files.len(),
            crate::report::plural(files.len())
        );
        let go = tui::ask(&q).unwrap_or_default();
        if !matches!(go.trim(), "y" | "Y" | "yes" | "YES") {
            tui::line(&tui::dim("  cancelled — nothing restored."));
            return;
        }
    }
    match checkpoint::undo_last_turn(cwd, overwrite) {
        Ok(undone) => {
            let n = undone.restored.len();
            tui::line(&tui::green(&format!(
                "  ✓ undid the last agent turn — restored {n} file{}:",
                crate::report::plural(n)
            )));
            show_undone(cwd, &undone);
            if !last.turn.untracked.is_empty() {
                shell_note("that turn also");
            }
        }
        Err(e) => tui::line(&tui::yellow(&format!("  {}", tui::sanitize_terminal(&e)))),
    }
}

// What bare /undo says before restoring anything: that a commit (by /commit,
// or HEAD moved since the turn) is not undone, and that there is no turn to
// undo — a /commit with no agent turn before it still gets the first.
pub(super) fn undo_preamble(
    last: Option<&checkpoint::LastTurn>,
    committed: bool,
) -> Vec<&'static str> {
    let mut notes = Vec::new();
    if committed || last.is_some_and(|l| l.head_moved) {
        notes.push("  commits are not undone by /undo — git reset --soft HEAD~1 keeps the changes");
    }
    if last.is_none() {
        notes.push(
            "  no agent turn recorded in this folder — /checkpoints lists what can be restored",
        );
    }
    notes
}

pub(super) fn handle_undo(cwd: &std::path::Path, arg: &str) {
    let arg = arg.trim();
    let mut overwrite = |p: &std::path::Path| confirm_overwrite(cwd, p);
    if arg.is_empty() {
        undo_last_turn(cwd, &mut overwrite);
    } else if arg == "git" {
        // The one command here that can destroy work bwn didn't do: it
        // discards ALL unstaged changes, including the user's hand edits.
        // Every file write asks first — the most destructive command must too.
        tui::line(&tui::yellow(
            "  /undo git runs `git checkout -- .` — it discards ALL unstaged changes,",
        ));
        tui::line(&tui::yellow(
            "  including edits you made by hand outside bwn.",
        ));
        let go = tui::ask("  Discard all unstaged changes? [y/N]: ").unwrap_or_default();
        if !matches!(go.trim(), "y" | "Y" | "yes" | "YES") {
            tui::line(&tui::dim("  cancelled — nothing discarded."));
            return;
        }
        match checkpoint::git_rollback(cwd) {
            Ok(msg) => tui::line(&tui::green(&format!(
                "  ✓ git reset: {}",
                tui::sanitize_terminal(&msg)
            ))),
            Err(e) => tui::line(&tui::red(&format!(
                "  git reset error: {}",
                tui::sanitize_terminal(&e)
            ))),
        }
    } else if arg == "all" || arg == "session" {
        let since = checkpoint::now_ms().saturating_sub(24 * 3600 * 1000);
        // Say exactly what's about to be rewound before doing it.
        let pending: Vec<checkpoint::Checkpoint> = checkpoint::list(cwd)
            .into_iter()
            .filter(|c| c.created_ms >= since)
            .collect();
        if pending.is_empty() {
            tui::line(&tui::dim("  no checkpoints in the last 24 hours."));
            return;
        }
        tui::line(&tui::yellow(&format!(
            "  /undo all rewinds every checkpoint from the last 24 hours — {} restore{}:",
            pending.len(),
            crate::report::plural(pending.len())
        )));
        for c in pending.iter().take(8) {
            tui::line(&tui::dim(&format!(
                "    - {} ({})",
                tui::sanitize_terminal(&c.path.display().to_string()),
                tui::sanitize_terminal(&c.action)
            )));
        }
        if pending.len() > 8 {
            tui::line(&tui::dim(&format!("    … and {} more", pending.len() - 8)));
        }
        let go = tui::ask("  Rewind all of it? [y/N]: ").unwrap_or_default();
        if !matches!(go.trim(), "y" | "Y" | "yes" | "YES") {
            tui::line(&tui::dim("  cancelled — nothing restored."));
            return;
        }
        match checkpoint::undo_all_since(cwd, since, &mut overwrite) {
            Ok(undone) => {
                tui::line(&tui::green(&format!(
                    "  ✓ restored {} files across session:",
                    undone.restored.len()
                )));
                show_undone(cwd, &undone);
            }
            Err(e) => tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e)))),
        }
    } else if arg == "latest" {
        match checkpoint::undo_latest(cwd, &mut overwrite) {
            Ok(cp) => tui::line(&tui::green(&format!(
                "  ✓ restored latest {}",
                tui::sanitize_terminal(&cp.path.display().to_string())
            ))),
            Err(e) => tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e)))),
        }
    } else {
        match checkpoint::undo_by_id(cwd, arg, &mut overwrite) {
            Ok(cp) => tui::line(&tui::green(&format!(
                "  ✓ restored checkpoint {} ({})",
                cp.id,
                tui::sanitize_terminal(&cp.path.display().to_string())
            ))),
            Err(e) => tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e)))),
        }
    }
}

pub(super) fn handle_align(cwd: &std::path::Path) {
    tui::line(&tui::accent("  alignment interview"));
    tui::line(&tui::dim(
        "  a short alignment review before proceeding with complex changes",
    ));

    let q1 = tui::ask("  1. What is the primary operational risk? [1: Regression | 2: Data Loss | 3: Performance | 4: Security]: ").unwrap_or_default();
    let risk_label = match q1.trim() {
        "2" => "Data Loss",
        "3" => "Performance Degradation",
        "4" => "Security Vulnerability",
        _ => "System Regression",
    };

    let q2 = tui::ask("  2. What is the reversibility of this change? [1: Easy (flag/config) | 2: Moderate (revert) | 3: Hard (db/contract) | 4: Irreversible]: ").unwrap_or_default();
    let rev_label = match q2.trim() {
        "1" => "Easy (Feature Flag / Config)",
        "3" => "Hard (Database Migration / API Contract)",
        "4" => "Irreversible",
        _ => "Moderate (Code Revert)",
    };

    let q3 = tui::ask("  3. What is the target confidence threshold? [1: High (>90%) | 2: Medium (>75%) | 3: Exploratory]: ").unwrap_or_default();
    let conf_label = match q3.trim() {
        "1" => "High (>90%)",
        "3" => "Exploratory / Prototype",
        _ => "Medium (>75%)",
    };

    tui::line(&tui::green("  ✓ alignment recorded"));
    tui::line(&format!("    • Primary Risk: {}", tui::bold(risk_label)));
    tui::line(&format!("    • Reversibility: {}", tui::bold(rev_label)));
    tui::line(&format!(
        "    • Confidence Threshold: {}",
        tui::bold(conf_label)
    ));

    let mut kb = crate::knowledge::KnowledgeBase::new(&cwd.to_string_lossy());
    let id = format!("dec-{}", crate::checkpoint::now_ms());
    let entity = crate::knowledge::Entity {
        id: id.clone(),
        entity_type: crate::knowledge::EntityType::ArchitectureDecision,
        name: format!("Operational Alignment ({})", risk_label),
        path: None,
        description: Some(format!(
            "Risk: {}, Reversibility: {}, Confidence: {}",
            risk_label, rev_label, conf_label
        )),
        metadata: serde_json::json!({
            "risk": risk_label,
            "reversibility": rev_label,
            "confidence_target": conf_label,
            "timestamp": crate::checkpoint::now_ms()
        }),
        last_updated: "now".to_string(),
    };
    kb.add_entity(entity);
    if let Err(e) = kb.save() {
        tui::line(&tui::red(&format!("  {}", tui::sanitize_terminal(&e))));
        return;
    }
    tui::line(&tui::dim(
        "  Decision recorded into structured knowledge base (.buildwithnexus/knowledge/).",
    ));
}

// What delegation really is: the model hands a subtask to a helper with the
// task tool; helpers are the two built-in roles and any agent files.
pub(super) fn handle_teamwork(p: &Provider) {
    tui::line(&tui::accent(
        "  teamwork — helpers the model can delegate to",
    ));
    // The compact tool set has no task tool: say so first.
    if tools::compact_surface(p.context_tokens) {
        for note in window_notes(p) {
            tui::line(&tui::yellow(&format!("  {note}")));
        }
    }
    tui::line(&tui::dim(
        "  In BUILD the model can hand a self-contained subtask to a helper with the task tool \
         (spawn_subagent). The helper gets a fresh context, works, and reports back.",
    ));
    tui::line(&format!(
        "    • {} — the default: edits files and runs commands under your permission",
        tui::bold("engineer")
    ));
    tui::line(&format!(
        "    • {} — reads and investigates, cites paths",
        tui::bold("researcher")
    ));
    tui::line(&format!(
        "    • {} — `isolate: true` runs the helper in a git worktree on its own branch",
        tui::bold("isolation")
    ));
    tui::line(&format!(
        "    • {} — `read_only: true` (or an agent file's) lets it read and search, never change",
        tui::bold("read-only")
    ));
    let together = match agent::max_parallel_helpers() {
        1 => "one at a time (max_parallel_helpers is 1 — a local server is \
              given one helper at a time unless it reports more slots or you set it)"
            .to_string(),
        n => format!("up to {n} at the same time (max_parallel_helpers)"),
    };
    tui::line(&tui::dim(&format!(
        "  Read-only and isolated helpers from one reply run {together}, and each shows its \
         work when it finishes; Esc stops them all. Helpers that write in your folder run one \
         after another."
    )));
    tui::line(&tui::dim(&format!(
        "  Your own helpers: <name>.md files (name, description, tools, read_only in frontmatter; \
         instructions as the body) in {}/agents or ~/.claude/agents, and in a trusted project's \
         .buildwithnexus/agents or .claude/agents. /agents lists them.",
        config::home().display()
    )));
}

// `/add-dir <path>` adds a folder to work in; bare `/add-dir` lists them.
pub(super) fn handle_add_dir(arg: &str, cwd: &std::path::Path) {
    let arg = arg.trim();
    if arg.is_empty() {
        let dirs = workdirs::list();
        if dirs.is_empty() {
            tui::line(&tui::dim(
                "  no added folders — /add-dir <path> lets the agent read and change files in another folder",
            ));
        }
        for d in dirs {
            let shown = d.display().to_string();
            tui::line(&format!("  + {}", tui::sanitize_terminal(&shown)));
        }
        return;
    }
    match workdirs::add(arg, cwd) {
        Ok(workdirs::Added::New(dir)) => {
            let shown = dir.display().to_string();
            tui::line(&tui::green(&format!(
                "  ✓ added {} — the agent may read and change files there; sandboxed commands may write there",
                tui::sanitize_terminal(&shown)
            )));
            // The instruction files it brings, named before the next
            // message carries them to the model.
            for n in workdirs::notices(cwd)
                .into_iter()
                .filter(|n| n.starts_with("instructions") && n.contains(&shown))
            {
                report::notice(&format!("  {n}"));
            }
        }
        Ok(workdirs::Added::Covered(why)) => {
            tui::line(&tui::dim(&format!("  {}", tui::sanitize_terminal(&why))))
        }
        Err(e) => tui::line(&tui::red(&format!(
            "  /add-dir: {}",
            tui::sanitize_terminal(&e)
        ))),
    }
}

// `/agents`: the helpers the model can delegate to, then Agents.md.
pub(super) fn handle_agents() {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let defs = config::load_agent_defs(&cwd);
    if defs.is_empty() {
        tui::line(&tui::dim(&format!(
            "  no helper agents yet — add <name>.md to {}/agents or .buildwithnexus/agents (see /teamwork)",
            config::home().display()
        )));
    } else {
        tui::line(&tui::accent("  helper agents (task tool roles)"));
        for a in &defs {
            let mut tools = a
                .tools
                .as_ref()
                .map_or("all tools".to_string(), |t| t.join(", "));
            if a.read_only {
                tools.push_str(", read-only");
            }
            // Names, descriptions and paths come from files on disk.
            tui::line(&format!(
                "    • {} — {}",
                tui::bold(&tui::sanitize_terminal(&a.name)),
                tui::sanitize_terminal(&a.description)
            ));
            tui::line(&tui::dim(&format!(
                "      tools: {} · {}",
                tui::sanitize_terminal(&tools),
                tui::sanitize_terminal(&a.path.display().to_string())
            )));
        }
    }
    match config::load_agents() {
        Some(agents) => {
            // Agents.md is Markdown — render it instead of dumping #/**/- raw.
            let shown: Vec<&str> = agents.lines().take(80).collect();
            tui::line(&tui::render_md(&shown.join("\n")));
            let total = agents.lines().count();
            if total > 80 {
                tui::line(&tui::dim(&format!("  …(+{} more lines)", total - 80)));
            }
        }
        None => tui::line(&tui::dim("  no Agents.md found")),
    }
}

// One line per configured MCP server, after a bounded connection attempt.
// One MCP check per configured server: a real connect and handshake each,
// bounded by their timeouts.
pub(super) fn mcp_checks() -> Vec<DoctorCheck> {
    mcp::ensure_ready();
    let reports = mcp::report();
    if reports.is_empty() {
        return vec![DoctorCheck::note("mcp", "no servers configured")];
    }
    reports
        .into_iter()
        .map(|r| {
            let name = format!("mcp:{}", r.name);
            match r.status {
                mcp::Status::Connected => DoctorCheck::pass(
                    name,
                    format!(
                        "{} · {} tool{}",
                        r.transport,
                        r.tools.len(),
                        crate::report::plural(r.tools.len())
                    ),
                ),
                mcp::Status::Disabled => DoctorCheck::note(name, "disabled"),
                mcp::Status::Connecting => {
                    DoctorCheck::fail(name, "still connecting after the timeout")
                }
                mcp::Status::Failed(e) | mcp::Status::Invalid(e) => {
                    DoctorCheck::fail(name, e.chars().take(160).collect::<String>())
                }
                mcp::Status::NeedsAuth(why) => {
                    let hint = mcp_auth::login_hint(&r.name, &why);
                    DoctorCheck::fail(
                        name,
                        format!(
                            "needs login: {}",
                            hint.chars().take(150).collect::<String>()
                        ),
                    )
                }
            }
        })
        .collect()
}

// `/doctor` (and `/debug`): the same checks as `buildwithnexus doctor`,
// probing the model this session is using.
pub(super) fn handle_doctor_tui(live: &Provider) {
    tui::line(&tui::accent(&format!("  buildwithnexus {VERSION} doctor")));
    let checks = doctor_checks(&CliOptions::default(), Some(live));
    for c in &checks {
        tui::line(&c.line());
    }
    if let Some(summary) = doctor_summary_line(&checks) {
        tui::line(&tui::yellow(&summary));
    }
}

/// How to call a listed command that needs an argument, for when it is
/// typed bare.
pub(super) fn command_usage(cmd_name: &str) -> Option<&'static str> {
    match cmd_name {
        "btw" => Some("/btw <context>  e.g. /btw also update the tests"),
        _ => None,
    }
}

/// The mode a bare `/plan`, `/build` or `/brainstorm` switches to.
pub(super) fn bare_mode_command(t: &str) -> Option<Mode> {
    match t {
        "/plan" => Some(Mode::Plan),
        "/build" => Some(Mode::Build),
        "/brainstorm" => Some(Mode::Brainstorm),
        _ => None,
    }
}

/// Whether input that starts with `/` is really a path: its first word has
/// another `/` in it (`/home/me/shot.png what is this`) or names something
/// on disk. Command names never contain a second slash.
pub(super) fn starts_with_path(t: &str) -> bool {
    let first = t.split_whitespace().next().unwrap_or("");
    first.len() > 1
        && first.starts_with('/')
        && (first[1..].contains('/') || std::path::Path::new(first).exists())
}

/// `/cmd` alone or `/cmd <args>`: the trimmed arguments ("" when bare).
/// None for any other input, including `/cmdmore`.
pub(super) fn slash_args<'a>(t: &'a str, cmd: &str) -> Option<&'a str> {
    match t.strip_prefix(cmd)? {
        "" => Some(""),
        rest if rest.starts_with(char::is_whitespace) => Some(rest.trim()),
        _ => None,
    }
}

/// `/theme [dark|light|ansi|auto]`: switch the colour theme and save it as
/// the `theme` setting. Bare, it opens a picker.
pub(super) fn handle_theme(arg: &str) {
    let choice = if arg.is_empty() {
        let names = ["dark", "light", "ansi", "auto"];
        let details = [
            "for dark terminal backgrounds",
            "for light terminal backgrounds",
            "the terminal's own 16 colours",
            "follow the terminal's background colour",
        ];
        let items: Vec<tui::SelectItem> = names
            .iter()
            .zip(details)
            .map(|(n, d)| tui::SelectItem {
                label: n.to_string(),
                detail: d.to_string(),
            })
            .collect();
        let title = format!("Theme (now: {})", tui::theme_name());
        match tui::select_item(&title, &items) {
            Some(i) => names[i].to_string(),
            None => return,
        }
    } else {
        arg.to_ascii_lowercase()
    };
    match tui::set_theme(&choice) {
        Ok(now) => {
            let saved = config::save_user_settings(&[("theme", Some(choice.clone().into()))]);
            let shown = if choice == "auto" {
                format!("auto ({now})")
            } else {
                now.to_string()
            };
            match saved {
                Ok(()) => tui::line(&tui::green(&format!(
                    "  ✓ theme: {shown} — saved; new output uses it"
                ))),
                Err(e) => tui::line(&tui::yellow(&format!(
                    "  theme: {shown} for this session — not saved: {e}"
                ))),
            }
        }
        Err(e) => tui::line(&tui::red(&format!("  {e}"))),
    }
}

/// Before the session closes (Ctrl+D, a second Ctrl+C, /exit): background
/// workflows run only while bwn is open, so name the ones that would wait
/// and ask. True means quit. Input that has gone away quits without asking.
pub(super) fn confirm_quit() -> bool {
    let waiting = waiting_workflows(&workflow::snapshots());
    if waiting.is_empty() || tui::input_closed() {
        return true;
    }
    let answer = tui::ask(&quit_question(&waiting)).map(|a| a.trim().to_lowercase());
    matches!(answer.as_deref(), Some("y" | "yes"))
}

// The workflows that would not run once bwn closes.
pub(super) fn waiting_workflows(snaps: &[workflow::WorkflowSnapshot]) -> Vec<usize> {
    snaps
        .iter()
        .filter(|w| matches!(w.status_str.as_str(), "pending" | "running"))
        .map(|w| w.id)
        .collect()
}

pub(super) fn quit_question(ids: &[usize]) -> String {
    let list: Vec<String> = ids.iter().map(|id| format!("#{id}")).collect();
    let noun = if ids.len() == 1 {
        "workflow"
    } else {
        "workflows"
    };
    format!(
        "  {noun} {} will not run while bwn is closed — quit anyway? [y/N] ",
        list.join(", ")
    )
}

/// The page that says what is sent to a hosted model, what stays on this
/// machine, and where keys are kept.
pub(super) const DATA_DOCS: &str = "https://buildwithnexus.dev/docs/data";

pub(super) fn print_help() {
    for l in help_lines() {
        tui::line(&l);
    }
}

pub(super) const HELP_CUSTOM_ROWS: usize = 12;

// The /help text: every built-in command by section, then the keys and the
// answers to an approval, as an aligned table.
pub(super) fn help_lines() -> Vec<String> {
    // (command or key, its arguments and aliases, what it does)
    type Row = (String, String, String);
    let mut sections: Vec<(&str, Vec<Row>)> = Vec::new();
    for c in tui::COMMANDS {
        let aliases = c.aliases.join(", ");
        let args = match (aliases.is_empty(), c.args.is_empty()) {
            (true, _) => c.args.to_string(),
            (false, true) => format!("({aliases})"),
            (false, false) => format!("({aliases}) {}", c.args),
        };
        let row = (c.name.to_string(), args, c.desc.to_string());
        match sections.iter_mut().find(|(t, _)| *t == c.section) {
            Some((_, rows)) => rows.push(row),
            None => sections.push((c.section, vec![row])),
        }
    }
    let rows = |r: &[(&str, &'static str)]| -> Vec<Row> {
        r.iter()
            .map(|(k, d)| (k.to_string(), String::new(), d.to_string()))
            .collect()
    };
    // Command files and skills the popup also offers; a long list is cut
    // and /skills has the rest.
    let mut mine: Vec<Row> = Vec::new();
    let custom = config::load_custom_commands();
    for c in custom.iter().take(HELP_CUSTOM_ROWS) {
        mine.push((
            format!("/{}", c.name),
            String::new(),
            tui::custom_command_desc(c),
        ));
    }
    if custom.len() > HELP_CUSTOM_ROWS {
        let more = format!(
            "… {} more — /skills lists them all",
            custom.len() - HELP_CUSTOM_ROWS
        );
        mine.push((String::new(), String::new(), more));
    }
    if !mine.is_empty() {
        sections.push(("your commands and skills", mine));
    }
    sections.push((
        "keys",
        rows(&[
            ("Shift+Tab", "cycle PLAN → BUILD → BRAINSTORM"),
            ("Enter", "send · end a line with \\ to add another"),
            (
                "Esc",
                "stop the agent mid-turn · cancel a question or picker",
            ),
            ("Esc Esc", "on an empty line: rewind to an earlier prompt"),
            (
                "Ctrl+C",
                "stop the agent · clear the draft · twice on an empty line: quit",
            ),
            ("Ctrl+D", "quit (asks first if workflows are waiting)"),
            ("Ctrl+Q / Ctrl+X", "edit / drop the next queued message"),
            ("↑↓  Ctrl+R", "history · search history"),
            ("Tab", "complete commands and @paths"),
            ("1-9 ↑↓ Enter", "in a picker: move to a row, Enter picks it"),
        ]),
    ));
    sections.push((
        "answering an approval (allow?)",
        rows(&[
            ("y", "yes, this once"),
            ("n", "no"),
            ("s", "allow it for the rest of this session"),
            ("a", "always allow it in this project"),
            ("d <reason>", "deny and tell the agent why"),
            ("Esc", "deny and stop the turn"),
        ]),
    ));

    let cmd_w = sections
        .iter()
        .flat_map(|(_, rows)| rows.iter())
        .map(|(cmd, _, _)| cmd.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = vec![
        tui::bold(&tui::accent("  buildwithnexus — commands and keys")),
        tui::dim("  type a task or a question; /command runs a command; Shift+Tab changes mode"),
    ];
    for (title, rows) in &sections {
        out.push(String::new());
        out.push(tui::dim(&format!("  {title}")));
        for (cmd, args, desc) in rows {
            let pad = " ".repeat(cmd_w.saturating_sub(cmd.chars().count()));
            let args_part = if args.is_empty() {
                String::new()
            } else {
                format!("  {}", tui::dim(args))
            };
            out.push(format!("    {}{pad}  {desc}{args_part}", tui::bold(cmd)));
        }
    }
    out.push(String::new());
    out.push(tui::dim("  input"));
    for l in [
        "    !<cmd> shell command · @<path> attach file/image/video · @diff @kb: @symbol:",
        "    ^V paste image/text · ^G $EDITOR · ←→ ^A ^E move · ^W ^U ^K kill · ^Y yank",
        "    PgUp/PgDn scroll · --plain (or TERM=dumb) for line mode without screen control",
    ] {
        out.push(tui::dim(l));
    }
    out.push(String::new());
    out.push(tui::dim(&format!(
        "  what leaves your machine, and where keys are kept: {DATA_DOCS}"
    )));
    out.push(String::new());
    out
}

#[cfg(test)]
mod terminal_ui_tests {
    use super::*;

    #[test]
    fn quitting_names_the_workflows_that_would_wait() {
        assert_eq!(
            quit_question(&[1]),
            "  workflow #1 will not run while bwn is closed — quit anyway? [y/N] "
        );
        assert!(quit_question(&[2, 5]).contains("workflows #2, #5 will not run"));
        // Built from snapshots, not the live queue: other tests schedule
        // workflows in this process while this one runs.
        let snap = |id: usize, status: &str| workflow::WorkflowSnapshot {
            id,
            task: "cargo test".into(),
            kind_str: "once".into(),
            status_str: status.into(),
            iteration: 0,
            elapsed_secs: None,
            output_lines: 0,
            reason: None,
        };
        assert!(waiting_workflows(&[]).is_empty());
        assert_eq!(
            waiting_workflows(&[
                snap(1, "done"),
                snap(2, "pending"),
                snap(3, "running"),
                snap(4, "cancelled"),
            ]),
            [2, 3]
        );
    }

    // The REPL's source, from `fn repl(` to the end of that function.
    fn repl_source() -> &'static str {
        let src = include_str!("repl.rs");
        let start = src.find("\npub(super) fn repl(").expect("fn repl");
        let body = &src[start..];
        &body[..body[1..].find("\n}\n").expect("end of repl") + 3]
    }

    // A listed command typed bare does something: the REPL matches it
    // alone (an arm, slash_args), it switches the mode, or it needs an
    // argument and its usage is printed.
    fn handled(cmd: &str) -> bool {
        repl_source().contains(&format!("\"{cmd}\""))
            || bare_mode_command(cmd).is_some()
            || command_usage(&cmd[1..]).is_some()
    }

    // Whether `word` appears in `text` on its own: not inside a longer
    // command, option or word.
    fn mentions(text: &str, word: &str) -> bool {
        let part = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
        text.match_indices(word)
            .any(|(i, _)| !text[..i].ends_with(part) && !text[i + word.len()..].starts_with(part))
    }

    // The source of `fn <name>(` up to the end of that function.
    fn fn_source(name: &str) -> &'static str {
        // The crate root and the modules split out of it.
        static SRC: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let src = SRC.get_or_init(|| {
            [
                include_str!("lib.rs"),
                include_str!("repl.rs"),
                include_str!("commands.rs"),
            ]
            .join("\n")
        });
        let start = ["fn", "pub fn", "pub(super) fn"]
            .iter()
            .find_map(|v| src.find(&format!("\n{v} {name}(")))
            .unwrap_or_else(|| panic!("fn {name}"));
        let body = &src[start..];
        &body[..body[1..].find("\n}\n").expect("end of fn") + 3]
    }

    // Every `/command` the REPL matches on: match arms, strip_prefix,
    // slash_args and `t == "/x"` tests.
    fn repl_commands() -> Vec<String> {
        let src = repl_source();
        let mut out: Vec<String> = Vec::new();
        for (i, _) in src.match_indices("\"/") {
            let name: String = std::iter::once('/')
                .chain(
                    src[i + 2..]
                        .chars()
                        .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'),
                )
                .collect();
            let next = src[i + 1 + name.len()..].chars().next();
            if name.len() > 1 && matches!(next, Some('"' | ' ')) && !out.contains(&name) {
                out.push(name);
            }
        }
        out
    }

    // The string literals in the patterns of a function's match arms
    // (`"a" | "b" =>`, `Some("a" | "b") =>`), empty ones left out.
    fn arm_literals(src: &str) -> Vec<String> {
        src.lines()
            .filter_map(|l| l.split_once("=>").map(|(pat, _)| pat))
            .flat_map(|pat| {
                pat.split('"')
                    .skip(1)
                    .step_by(2)
                    .filter(|w| !w.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn every_command_the_repl_takes_is_in_help_the_popup_and_completion() {
        let cmds = repl_commands();
        assert!(cmds.len() > 50, "{cmds:?}");
        let help = tui::strip_ansi(&help_lines().join("\n"));
        let usage = usage_text();
        let popup = tui::builtin_slash_commands();
        for cmd in &cmds {
            assert!(mentions(&help, cmd), "/help does not list {cmd}");
            assert!(mentions(&usage, cmd), "--help does not list {cmd}");
            assert!(
                popup.contains(&cmd.as_str()),
                "the / popup does not list {cmd}"
            );
            assert!(
                tui::completions_for(cmd).contains(cmd),
                "Tab does not complete {cmd}"
            );
        }
        // The page on what is sent where is one step from either help.
        assert!(help.contains(DATA_DOCS), "{help}");
        assert!(usage.contains(DATA_DOCS), "{usage}");
    }

    #[test]
    fn help_says_bwn_is_the_same_command() {
        let usage = usage_text();
        let first = usage
            .lines()
            .find(|l| l.contains("interactive session"))
            .unwrap();
        assert!(first.contains("(also `bwn`)"), "{first}");
        // The description column stays aligned with the rows around it.
        let col = |l: &str| {
            l.find("interactive session")
                .or_else(|| l.find("execute a task"))
        };
        let run = usage
            .lines()
            .find(|l| l.contains("execute a task"))
            .unwrap();
        assert_eq!(col(first), col(run), "{usage}");
    }

    #[test]
    fn permission_modes_and_subcommands_are_listed_and_complete() {
        let help = tui::strip_ansi(&help_lines().join("\n"));
        let usage = usage_text();
        let perms = tui::find_command("/permissions").unwrap();
        // Every word /permissions takes: the modes and the approval commands.
        let taken: Vec<String> = arm_literals(fn_source("handle_permissions_arg"))
            .into_iter()
            .map(|w| w.trim().to_string())
            .filter(|w| w.chars().all(|c| c.is_ascii_lowercase() || c == '-'))
            .collect();
        assert!(taken.len() >= 3, "{taken:?}");
        for word in taken.iter().map(String::as_str).chain([
            "ask",
            "accept-edits",
            "auto",
            "readonly",
            "default",
        ]) {
            assert!(perms.subs.contains(&word), "/permissions {word} not listed");
            assert!(mentions(perms.args, word), "/help: /permissions {word}");
            assert!(
                tui::completions_for("/permissions ").contains(&word.to_string()),
                "Tab: /permissions {word}"
            );
        }
        // Every permission mode by its name, and plan's reading, in --help.
        for p in [
            Permission::Ask,
            Permission::AcceptEdits,
            Permission::Auto,
            Permission::ReadOnly,
        ] {
            let name = agent::permission_name(p);
            assert!(mentions(&usage, name), "--help: {name}");
            assert!(mentions(&help, name), "/help: {name}");
        }
        assert!(usage.contains("plan reads as"), "{usage}");
        // Subcommands of other commands complete too.
        assert_eq!(tui::completions_for("/diff t"), ["turn"]);
        assert!(tui::completions_for("/review --").contains(&"--staged".to_string()));
        assert!(tui::completions_for("/mcp ").contains(&"reload".to_string()));
    }

    #[test]
    fn every_cli_command_option_and_subcommand_is_in_help() {
        let usage = usage_text();
        // The commands run() dispatches, their flag spellings included.
        let run_src = fn_source("run");
        let dispatch = &run_src[run_src.find("    match cmd {").expect("dispatch")..];
        let commands = arm_literals(dispatch);
        assert!(commands.len() > 25, "{commands:?}");
        for word in &commands {
            assert!(mentions(&usage, word), "--help does not list `{word}`");
        }
        // Every option parse_cli_options takes, and its "did you mean" list.
        let parse_src = fn_source("parse_cli_options");
        let mut flags: Vec<String> = arm_literals(parse_src)
            .into_iter()
            .filter(|w| w.starts_with('-'))
            .collect();
        for (i, _) in parse_src.match_indices("arg == \"") {
            let flag = &parse_src[i + 8..];
            flags.push(flag[..flag.find('"').unwrap()].to_string());
        }
        assert!(flags.len() > 12, "{flags:?}");
        for flag in flags
            .iter()
            .map(String::as_str)
            .chain(CLI_OPTIONS.iter().copied())
        {
            assert!(mentions(&usage, flag), "--help does not list {flag}");
            if flag.starts_with("--") && flag != "--" {
                assert!(CLI_OPTIONS.contains(&flag), "{flag} is not in CLI_OPTIONS");
            }
        }
        // Subcommands and their options.
        for word in arm_literals(fn_source("sessions_command")) {
            assert!(mentions(&usage, &word), "--help: sessions {word}");
        }
        for word in [
            "--print",
            "--agents-md",
            "--check",
            "--base",
            "--staged",
            "add",
            "remove",
            "reload",
        ] {
            assert!(mentions(&usage, word), "--help: {word}");
        }
    }

    #[test]
    fn every_listed_command_and_tip_has_a_handler() {
        for cmd in tui::builtin_slash_commands() {
            assert!(
                handled(cmd),
                "{cmd} is in the command list but no REPL arm handles it"
            );
        }
        for tip in STARTUP_TIPS {
            for word in tip.split_whitespace().filter(|w| w.starts_with('/')) {
                let cmd = word.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
                assert!(
                    handled(cmd) && tui::builtin_slash_commands().contains(&cmd),
                    "tip names {cmd}, which is not a command: {tip}"
                );
            }
        }
        // Bare mode commands switch the mode; with a task they run it.
        assert!(matches!(bare_mode_command("/plan"), Some(Mode::Plan)));
        assert!(matches!(bare_mode_command("/build"), Some(Mode::Build)));
        assert!(matches!(
            bare_mode_command("/brainstorm"),
            Some(Mode::Brainstorm)
        ));
        assert!(bare_mode_command("/plan add tests").is_none());
        assert_eq!(slash_args("/loop", "/loop"), Some(""));
        assert_eq!(
            slash_args("/loop  5m cargo test ", "/loop"),
            Some("5m cargo test")
        );
        assert_eq!(slash_args("/loops", "/loop"), None);
    }

    #[test]
    fn a_message_starting_with_an_absolute_path_is_not_a_command() {
        assert!(starts_with_path(
            "/home/me/shot.png what is in this screenshot"
        ));
        assert!(starts_with_path("/home/me/my\\ shot.png what is this"));
        assert!(starts_with_path("/tmp"));
        assert!(!starts_with_path("/help"));
        assert!(!starts_with_path("/frobnicate now"));
        assert!(!starts_with_path("/"));
    }
}
