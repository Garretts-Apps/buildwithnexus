//! The system prompt, assembled only when `/system-prompt` or
//! `bwn system-prompt` asks for it. The pieces are the strings the turn
//! already sends; this module adds labels, estimates, redaction and the
//! plain-text override files.

use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use crate::agent::{effective_pieces, PromptMode, COMPACT_CONTEXT};

/// Where an override is stored. The built-in prompt is never written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    User,
    Project,
}

impl Scope {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
        }
    }
}

/// Same estimator the session meter uses: four bytes, called characters, to a token.
pub(crate) const ESTIMATE: &str = "estimated at 4 characters a token";

pub(crate) fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}

pub(crate) struct Section {
    pub layer: &'static str,
    pub source: String,
    /// The text that is sent, before display redaction.
    pub text: String,
    /// False for sections that ride beside the system message (tool
    /// definitions) or that are present on disk but not sent.
    pub in_system: bool,
}

pub(crate) struct Assembly {
    pub mode: PromptMode,
    pub context_tokens: usize,
    pub sections: Vec<Section>,
}

impl Assembly {
    pub(crate) fn compact(&self) -> bool {
        self.context_tokens <= COMPACT_CONTEXT
    }

    pub(crate) fn system_text(&self) -> String {
        let parts: Vec<&str> = self
            .sections
            .iter()
            .filter(|s| s.in_system)
            .map(|s| s.text.as_str())
            .collect();
        format!("{}\n\n", parts.join("\n\n"))
    }

    pub(crate) fn system_tokens(&self) -> usize {
        self.sections
            .iter()
            .filter(|s| s.in_system)
            .map(|s| estimate_tokens(&s.text))
            .sum()
    }
}

/// What a turn would send, plus tool definitions and an untrusted project
/// override that is on disk but not included.
pub(crate) fn assemble(cwd: &Path, mode: PromptMode, context_tokens: usize) -> Assembly {
    let mut sections: Vec<Section> = effective_pieces(cwd, mode, context_tokens)
        .into_iter()
        .map(|p| Section {
            layer: p.layer,
            source: p.source,
            text: p.text,
            in_system: true,
        })
        .collect();
    if let Some(text) = untrusted_project(cwd) {
        let path = override_path(Scope::Project, cwd);
        sections.push(Section {
            layer: "project override",
            source: format!(
                "{} (on disk, not in the prompt until this folder is trusted)",
                path.display()
            ),
            text: format!("[Project System Prompt — .buildwithnexus/system.md]\n{text}"),
            in_system: false,
        });
    }
    sections.push(tool_section(context_tokens));
    Assembly {
        mode,
        context_tokens,
        sections,
    }
}

fn untrusted_project(cwd: &Path) -> Option<String> {
    let text = crate::config::project_system_md(cwd)?;
    let trusted =
        crate::hooks::project_file_trusted(cwd, crate::config::PROJECT_SYSTEM_PROMPT, &text);
    if trusted {
        None
    } else {
        Some(text)
    }
}

fn tool_section(context_tokens: usize) -> Section {
    let defs = crate::tools::defs_for_context(true, context_tokens);
    let mut text = String::new();
    for d in &defs {
        let desc = d.description.replace('\n', " ");
        text.push_str(&format!("- {} - {}\n", d.name, desc.trim()));
    }
    Section {
        layer: "tool definitions",
        source: "offered with the request, not inside the system text".into(),
        text,
        in_system: false,
    }
}

pub(crate) fn redact_text(text: &str) -> String {
    crate::provider::redact(text)
}

/// The window `/system-prompt` plans with when no session provider is loaded.
/// Settings win; otherwise the full prompt, so a show does not probe a server.
pub(crate) fn context_tokens_from_settings() -> usize {
    crate::config::load_settings()
        .and_then(|s| s.context_tokens)
        .map(|n| n as usize)
        .filter(|n| *n > 0)
        .unwrap_or(200_000)
}

pub(crate) fn show_json(cwd: &Path, mode: PromptMode, context_tokens: usize) -> String {
    let asm = assemble(cwd, mode, context_tokens);
    let sections: Vec<serde_json::Value> = asm
        .sections
        .iter()
        .map(|s| {
            serde_json::json!({
                "layer": s.layer,
                "source": s.source,
                "tokens": estimate_tokens(&s.text),
                "estimated": true,
                "in_system": s.in_system,
                "text": redact_text(&s.text),
            })
        })
        .collect();
    let body = serde_json::json!({
        "mode": mode.name(),
        "context_tokens": context_tokens,
        "compact": asm.compact(),
        "estimate": ESTIMATE,
        "tokens": asm.system_tokens(),
        "prompt": redact_text(&asm.system_text()),
        "sections": sections,
        "overrides": {
            "user": override_path(Scope::User, cwd).display().to_string(),
            "project": override_path(Scope::Project, cwd).display().to_string(),
        },
    });
    format!("{body}\n")
}

pub(crate) fn override_path(scope: Scope, cwd: &Path) -> PathBuf {
    match scope {
        Scope::User => crate::config::home().join("system.md"),
        Scope::Project => cwd.join(".buildwithnexus").join("system.md"),
    }
}

pub(crate) fn read_override(scope: Scope, cwd: &Path) -> Result<String, String> {
    let path = override_path(scope, cwd);
    if fs::symlink_metadata(&path)
        .ok()
        .is_some_and(|m| m.file_type().is_symlink())
    {
        return Err(format!(
            "refusing to read {}: it is a symlink",
            path.display()
        ));
    }
    if !path.is_file() {
        return Ok(String::new());
    }
    match scope {
        Scope::User => {
            fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))
        }
        Scope::Project => Ok(crate::config::read_project_file(&path, cwd).unwrap_or_default()),
    }
}

/// Write the override, or remove it when the text is blank.
pub(crate) fn save_override(scope: Scope, cwd: &Path, text: &str) -> Result<PathBuf, String> {
    let path = override_path(scope, cwd);
    let text = normalize(text);
    if text.is_empty() {
        if fs::symlink_metadata(&path).is_ok() {
            fs::remove_file(&path).map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
        }
        return Ok(path);
    }
    match scope {
        Scope::User => {
            crate::config::ensure_home();
            atomic_write(&path, &text)?;
        }
        Scope::Project => {
            crate::config::write_project_file(&path, cwd, text.as_bytes())?;
        }
    }
    Ok(path)
}

pub(crate) fn reset_override(scope: Scope, cwd: &Path) -> Result<PathBuf, String> {
    let path = override_path(scope, cwd);
    if fs::symlink_metadata(&path).is_err() {
        return Err(format!("no override at {}", path.display()));
    }
    fs::remove_file(&path).map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
    Ok(path)
}

fn normalize(text: &str) -> String {
    let t = text.trim_end_matches(['\n', '\r']);
    if t.trim().is_empty() {
        String::new()
    } else {
        format!("{t}\n")
    }
}

fn atomic_write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    if fs::symlink_metadata(path)
        .ok()
        .is_some_and(|m| m.file_type().is_symlink())
    {
        return Err(format!(
            "refusing to write {}: it is a symlink",
            path.display()
        ));
    }
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".bwn-tmp-{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("cannot write {}: {e}", path.display())
    })
}

// ── diff ──────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hunk {
    /// 1-based line in the old text. An insertion names the line it sits before.
    pub old_line: usize,
    pub removed: Vec<String>,
    pub added: Vec<String>,
}

pub(crate) fn diff_hunks(old: &str, new: &str) -> Vec<Hunk> {
    let a = split_lines(old);
    let b = split_lines(new);
    if a.len().saturating_mul(b.len()) > 250_000 {
        return vec![Hunk {
            old_line: 1,
            removed: a,
            added: b,
        }];
    }
    group_hunks(&a, &b, &edit_ops(&a, &b))
}

pub(crate) fn apply_hunks(old: &str, hunks: &[Hunk], accept: &[bool]) -> String {
    let lines = split_lines(old);
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    for (n, hunk) in hunks.iter().enumerate() {
        let start = hunk.old_line.saturating_sub(1).min(lines.len());
        if start > i {
            out.extend(lines[i..start].iter().cloned());
        }
        if accept.get(n).copied().unwrap_or(false) {
            out.extend(hunk.added.iter().cloned());
        } else {
            out.extend(hunk.removed.iter().cloned());
        }
        i = start + hunk.removed.len();
    }
    if i < lines.len() {
        out.extend(lines[i..].iter().cloned());
    }
    out.join("\n")
}

fn split_lines(s: &str) -> Vec<String> {
    s.split('\n').map(str::to_string).collect()
}

enum Op {
    Keep,
    Del(usize),
    Ins(usize),
}

fn edit_ops(a: &[String], b: &[String]) -> Vec<Op> {
    let n = a.len();
    let m = b.len();
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut ops = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(Op::Keep);
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(Op::Del(i));
            i += 1;
        } else {
            ops.push(Op::Ins(j));
            j += 1;
        }
    }
    while i < n {
        ops.push(Op::Del(i));
        i += 1;
    }
    while j < m {
        ops.push(Op::Ins(j));
        j += 1;
    }
    ops
}

fn group_hunks(a: &[String], b: &[String], ops: &[Op]) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut old_i = 0usize;
    let mut at = 0usize;
    while at < ops.len() {
        if matches!(ops[at], Op::Keep) {
            old_i += 1;
            at += 1;
            continue;
        }
        let start = old_i;
        let mut removed = Vec::new();
        let mut added = Vec::new();
        while at < ops.len() {
            match ops[at] {
                Op::Keep => break,
                Op::Del(i) => {
                    removed.push(a[i].clone());
                    old_i += 1;
                    at += 1;
                }
                Op::Ins(j) => {
                    added.push(b[j].clone());
                    at += 1;
                }
            }
        }
        hunks.push(Hunk {
            old_line: start + 1,
            removed,
            added,
        });
    }
    hunks
}

pub(crate) fn suggestion_prompt(current: &str, goal: &str) -> String {
    let goal = if goal.trim().is_empty() {
        "make this layer clearer and shorter without dropping instructions that are already here"
    } else {
        goal.trim()
    };
    let current = if current.trim().is_empty() {
        "(empty)"
    } else {
        current
    };
    format!(
        "Revise one layer of a coding agent's system prompt: the user's own \
         text, which is appended after the built-in prompt. Do not rewrite \
         the built-in prompt. Return only the revised layer, as plain text, \
         with no code fence and no commentary.\n\n\
         Goal: {goal}\n\n\
         Current layer:\n{current}\n"
    )
}

pub(crate) fn parse_suggestion(reply: &str) -> Result<String, String> {
    let text = strip_fence(reply.trim());
    if text.trim().is_empty() {
        return Err("the model returned an empty suggestion".into());
    }
    Ok(text.to_string())
}

pub(crate) fn suggest_with(
    current: &str,
    goal: &str,
    ask: impl FnOnce(&str) -> Result<String, String>,
) -> Result<String, String> {
    let reply = ask(&suggestion_prompt(current, goal))?;
    parse_suggestion(&reply)
}

fn strip_fence(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    let rest = rest
        .strip_prefix("markdown")
        .or_else(|| rest.strip_prefix("md"))
        .unwrap_or(rest);
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    rest.strip_suffix("```")
        .map(|t| t.trim_end_matches('\n'))
        .unwrap_or(s)
}

// ── command line ──────────────────────────────────────────────────────────────

enum Sub {
    Show,
    Edit,
    Suggest,
    Reset,
    Path,
}

struct Parsed {
    json: bool,
    scope: Option<Scope>,
    mode: Option<PromptMode>,
    sub: Sub,
    goal: String,
}

fn parse_args(args: &[String], json: bool) -> Result<Parsed, String> {
    let mut parsed = Parsed {
        json,
        scope: None,
        mode: None,
        sub: Sub::Show,
        goal: String::new(),
    };
    let mut words: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => parsed.json = true,
            "--user" => set_scope(&mut parsed, Scope::User)?,
            "--project" => set_scope(&mut parsed, Scope::Project)?,
            "--mode" => {
                let value = it
                    .next()
                    .ok_or("--mode requires build, plan, brainstorm or chat")?;
                parsed.mode = Some(PromptMode::parse(value)?);
            }
            other if other.starts_with("--mode=") => {
                let value = other.trim_start_matches("--mode=");
                parsed.mode = Some(PromptMode::parse(value)?);
            }
            "-h" | "--help" => return Err(usage()),
            other if other.starts_with('-') => {
                return Err(format!("unknown option {other}\n{}", usage()));
            }
            other => words.push(other.to_string()),
        }
    }
    if let Some(first) = words.first() {
        parsed.sub = match first.as_str() {
            "show" => Sub::Show,
            "edit" => Sub::Edit,
            "suggest" => Sub::Suggest,
            "reset" => Sub::Reset,
            "path" => Sub::Path,
            other => {
                return Err(format!(
                    "unknown system-prompt command '{other}'\n{}",
                    usage()
                ))
            }
        };
        if matches!(parsed.sub, Sub::Suggest) {
            parsed.goal = words[1..].join(" ");
        } else if words.len() > 1 {
            return Err(format!("unexpected argument '{}'\n{}", words[1], usage()));
        }
    }
    if parsed.json && !matches!(parsed.sub, Sub::Show) {
        return Err("--json is for system-prompt show".into());
    }
    Ok(parsed)
}

fn set_scope(parsed: &mut Parsed, scope: Scope) -> Result<(), String> {
    if parsed.scope.is_some() {
        return Err("pass only one of --user or --project".into());
    }
    parsed.scope = Some(scope);
    Ok(())
}

fn usage() -> String {
    "\
system-prompt show [--json] [--mode build|plan|brainstorm|chat]
system-prompt edit [--user|--project]
system-prompt suggest [--user|--project] [goal]
system-prompt reset [--user|--project]
system-prompt path [--user|--project]"
        .into()
}

pub(crate) fn cli(
    args: &[String],
    json: bool,
    ask_model: impl Fn(&str, &str) -> Result<String, String>,
) -> i32 {
    match run(args, json, ask_model) {
        Ok(()) => 0,
        Err(e) if e == "cancelled" => 0,
        Err(e) => {
            let usage_err = e.contains('\n')
                || e.starts_with("unknown")
                || e.starts_with("unexpected")
                || e.starts_with("pass ")
                || e.starts_with("--");
            eprintln!("buildwithnexus system-prompt: {e}");
            if usage_err {
                2
            } else {
                1
            }
        }
    }
}

fn run(
    args: &[String],
    json: bool,
    ask_model: impl Fn(&str, &str) -> Result<String, String>,
) -> Result<(), String> {
    let parsed = parse_args(args, json)?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    match parsed.sub {
        Sub::Show => {
            let mode = parsed.mode.unwrap_or(PromptMode::Build);
            let tokens = context_tokens_from_settings();
            if parsed.json {
                print!("{}", show_json(&cwd, mode, tokens));
                return Ok(());
            }
            loop {
                let asm = assemble(&cwd, mode, context_tokens_from_settings());
                match crate::tui::system_prompt::show_view(&asm) {
                    crate::tui::system_prompt::ShowAction::Close => return Ok(()),
                    crate::tui::system_prompt::ShowAction::Edit => {
                        edit_flow(&cwd, need_scope(parsed.scope)?)?;
                    }
                    crate::tui::system_prompt::ShowAction::Suggest => {
                        let goal = crate::tui::ask("  goal (empty is fine): ").unwrap_or_default();
                        suggest_flow(&cwd, need_scope(parsed.scope)?, goal.trim(), &ask_model)?;
                    }
                }
            }
        }
        Sub::Path => {
            print_paths(&cwd, parsed.scope);
            Ok(())
        }
        Sub::Edit => {
            let scope = need_scope(parsed.scope)?;
            edit_flow(&cwd, scope)
        }
        Sub::Reset => {
            let scope = need_scope(parsed.scope)?;
            let path = reset_override(scope, &cwd)?;
            println!("removed {}", path.display());
            Ok(())
        }
        Sub::Suggest => {
            let scope = need_scope(parsed.scope)?;
            suggest_flow(&cwd, scope, &parsed.goal, ask_model)
        }
    }
}

fn print_paths(cwd: &Path, scope: Option<Scope>) {
    let one = |s: Scope| println!("{}", override_path(s, cwd).display());
    match scope {
        Some(s) => one(s),
        None => {
            println!("user     {}", override_path(Scope::User, cwd).display());
            println!("project  {}", override_path(Scope::Project, cwd).display());
        }
    }
}

fn need_scope(scope: Option<Scope>) -> Result<Scope, String> {
    if let Some(s) = scope {
        return Ok(s);
    }
    if !std::io::stdin().is_terminal() {
        return Err("pass --user or --project".into());
    }
    match crate::tui::ask_key(
        "  where?  u user  ·  p project  ·  q cancel  ",
        &['u', 'p', 'q'],
    ) {
        Some('u') => Ok(Scope::User),
        Some('p') => Ok(Scope::Project),
        _ => Err("cancelled".into()),
    }
}

fn edit_flow(cwd: &Path, scope: Scope) -> Result<(), String> {
    let current = read_override(scope, cwd)?;
    let Some(edited) = crate::tui::system_prompt::edit_text(&current)? else {
        println!("not saved");
        return Ok(());
    };
    let edited = normalize(&edited);
    let current = normalize(&current);
    if edited == current {
        println!("unchanged");
        return Ok(());
    }
    let path = override_path(scope, cwd);
    if !crate::tui::system_prompt::confirm_save(
        scope.name(),
        &path.display().to_string(),
        &current,
        &edited,
    )? {
        println!("not saved");
        return Ok(());
    }
    let saved = save_override(scope, cwd, &edited)?;
    println!("saved {}", saved.display());
    if scope == Scope::Project && untrusted_project(cwd).is_some() {
        println!("it stays out of the prompt until this folder is trusted (bwn trust --print)");
    }
    Ok(())
}

fn suggest_flow(
    cwd: &Path,
    scope: Scope,
    goal: &str,
    ask_model: impl Fn(&str, &str) -> Result<String, String>,
) -> Result<(), String> {
    let current = normalize(&read_override(scope, cwd)?);
    let proposed = suggest_with(&current, goal, |prompt| ask_model(goal, prompt))?;
    let proposed = normalize(&proposed);
    if proposed == current {
        println!("the suggestion matches the current layer");
        return Ok(());
    }
    let path = override_path(scope, cwd);
    let Some(next) =
        crate::tui::system_prompt::review_hunks(&path.display().to_string(), &current, &proposed)?
    else {
        println!("nothing applied");
        return Ok(());
    };
    if normalize(&next) == current {
        println!("nothing applied");
        return Ok(());
    }
    let saved = save_override(scope, cwd, &next)?;
    println!("saved {}", saved.display());
    if scope == Scope::Project && untrusted_project(cwd).is_some() {
        println!("it stays out of the prompt until this folder is trusted (bwn trust --print)");
    }
    Ok(())
}

/// `/system-prompt` inside a session. `arg` is everything after the command.
pub(crate) fn slash(
    context_tokens: usize,
    cwd: &Path,
    mode: PromptMode,
    arg: &str,
    ask_model: impl Fn(&str, &str) -> Result<String, String>,
) {
    let mut words = arg.split_whitespace();
    let sub = words.next().unwrap_or("show");
    let rest = words.collect::<Vec<_>>().join(" ");
    let run = match sub {
        "show" | "" => {
            let asm = assemble(cwd, mode, context_tokens);
            match crate::tui::system_prompt::show_view(&asm) {
                crate::tui::system_prompt::ShowAction::Edit => {
                    need_scope(None).and_then(|scope| edit_flow(cwd, scope))
                }
                crate::tui::system_prompt::ShowAction::Suggest => {
                    need_scope(None).and_then(|scope| {
                        let goal = crate::tui::ask("  goal (empty is fine): ").unwrap_or_default();
                        suggest_flow(cwd, scope, goal.trim(), &ask_model)
                    })
                }
                crate::tui::system_prompt::ShowAction::Close => Ok(()),
            }
        }
        "edit" => {
            let scope = slash_scope(&rest);
            edit_flow(cwd, scope)
        }
        "suggest" => {
            let (scope, goal) = slash_suggest(&rest);
            suggest_flow(cwd, scope, &goal, ask_model)
        }
        "reset" => {
            let scope = slash_scope(&rest);
            reset_override(scope, cwd).map(|p| {
                crate::tui::line(&crate::tui::green(&format!("  removed {}", p.display())));
            })
        }
        "path" => {
            print_paths(cwd, None);
            Ok(())
        }
        other => Err(format!(
            "unknown /system-prompt command '{other}' - show, edit, suggest, reset, path"
        )),
    };
    if let Err(e) = run {
        if e != "cancelled" {
            crate::tui::line(&crate::tui::red(&format!(
                "  {}",
                crate::tui::sanitize_terminal(&e)
            )));
        }
    }
}

fn slash_scope(rest: &str) -> Scope {
    match rest.trim() {
        "project" | "--project" => Scope::Project,
        "user" | "--user" => Scope::User,
        _ => {
            if rest.trim().is_empty() {
                need_scope(None).unwrap_or(Scope::User)
            } else {
                Scope::User
            }
        }
    }
}

fn slash_suggest(rest: &str) -> (Scope, String) {
    let rest = rest.trim();
    if let Some(goal) = rest.strip_prefix("--project") {
        return (Scope::Project, goal.trim().to_string());
    }
    if let Some(goal) = rest.strip_prefix("project ") {
        return (Scope::Project, goal.trim().to_string());
    }
    if let Some(goal) = rest.strip_prefix("--user") {
        return (Scope::User, goal.trim().to_string());
    }
    if rest == "project" {
        return (Scope::Project, String::new());
    }
    if rest == "user" {
        return (Scope::User, String::new());
    }
    if rest.is_empty() {
        return (need_scope(None).unwrap_or(Scope::User), String::new());
    }
    (Scope::User, rest.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "bwn-sp-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&base);
        let home = base.join("home");
        let proj = base.join("proj");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&proj).unwrap();
        (home, proj)
    }

    #[test]
    fn layers_follow_the_order_the_prompt_is_sent() {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (home, proj) = scratch("order");
        std::env::set_var("NEXUS_HOME", &home);
        fs::write(proj.join("AGENTS.md"), "use tabs\n").unwrap();
        fs::write(home.join("memory.md"), "remembers tabs\n").unwrap();
        fs::write(home.join("Agents.md"), "role: scribe\n").unwrap();
        fs::write(home.join("system.md"), "USER-LAYER be terse\n").unwrap();

        let asm = assemble(&proj, PromptMode::Build, 200_000);
        let names: Vec<&str> = asm
            .sections
            .iter()
            .filter(|s| s.in_system)
            .map(|s| s.layer)
            .collect();
        let pos = |n: &str| {
            names
                .iter()
                .position(|l| *l == n)
                .unwrap_or_else(|| panic!("{n} missing from {names:?}"))
        };
        assert!(pos("mode") < pos("built-in base"));
        assert!(pos("built-in base") < pos("environment"));
        assert!(pos("environment") < pos("built-in tools"));
        assert!(pos("instructions") < pos("memory"));
        assert!(pos("memory") < pos("agent definitions"));
        assert!(pos("agent definitions") < pos("user override"));
        assert!(pos("user override") < pos("operational rules"));
        assert!(pos("operational rules") < pos("skills"));
        let instructions = asm
            .sections
            .iter()
            .find(|s| s.layer == "instructions")
            .unwrap();
        assert!(
            instructions.source.contains("AGENTS.md"),
            "{}",
            instructions.source
        );
        assert!(asm.system_text().contains("USER-LAYER be terse"));
        assert!(asm.system_text().contains("use tabs"));
        assert_eq!(
            asm.system_text(),
            crate::agent::effective_system(&proj, PromptMode::Build, 200_000)
        );
        assert!(asm
            .sections
            .iter()
            .any(|s| s.layer == "tool definitions" && !s.in_system));
        assert!(asm.system_tokens() > 0);
        assert!(!asm.compact());

        let compact = assemble(&proj, PromptMode::Build, 1_024);
        assert!(compact.compact());
        assert!(compact
            .sections
            .iter()
            .all(|s| s.layer != "operational rules"));
        assert!(compact.sections.iter().all(|s| s.layer != "skills"));
        assert!(compact.sections.iter().any(|s| s.layer == "user override"));

        std::env::remove_var("NEXUS_HOME");
        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn project_override_follows_the_user_until_replace_or_reset() {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (home, proj) = scratch("prec");
        std::env::set_var("NEXUS_HOME", &home);
        fs::create_dir_all(proj.join(".buildwithnexus")).unwrap();
        save_override(Scope::User, &proj, "USER-LAYER").unwrap();
        save_override(Scope::Project, &proj, "PROJECT-LAYER").unwrap();

        let text = assemble(&proj, PromptMode::Build, 200_000).system_text();
        assert!(text.contains("USER-LAYER"), "{text}");
        assert!(!text.contains("PROJECT-LAYER"), "{text}");
        let held = assemble(&proj, PromptMode::Build, 200_000)
            .sections
            .into_iter()
            .find(|s| s.layer == "project override" && !s.in_system);
        assert!(
            held.is_some(),
            "an untrusted project file is shown and not sent"
        );

        let pending = crate::config::untrusted_project_files(&proj);
        crate::hooks::store_trust(&proj, &pending);
        let text = assemble(&proj, PromptMode::Build, 200_000).system_text();
        let user_at = text.find("USER-LAYER").unwrap();
        let proj_at = text.find("PROJECT-LAYER").unwrap();
        assert!(user_at < proj_at);

        fs::write(
            home.join("settings.json"),
            r#"{"provider":"openai","model":"m","permission":"ask","project_system_prompt":"replace"}"#,
        )
        .unwrap();
        let text = assemble(&proj, PromptMode::Build, 200_000).system_text();
        assert!(
            text.contains("PROJECT-LAYER") && !text.contains("USER-LAYER"),
            "{text}"
        );

        let removed = reset_override(Scope::Project, &proj).unwrap();
        assert!(!removed.exists());
        let text = assemble(&proj, PromptMode::Build, 200_000).system_text();
        assert!(!text.contains("PROJECT-LAYER"), "{text}");
        assert!(text.contains("USER-LAYER"), "{text}");
        reset_override(Scope::User, &proj).unwrap();
        let text = assemble(&proj, PromptMode::Build, 200_000).system_text();
        assert!(!text.contains("USER-LAYER"), "{text}");
        assert!(reset_override(Scope::User, &proj).is_err());

        std::env::remove_var("NEXUS_HOME");
        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn secrets_in_the_shown_prompt_are_redacted() {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (home, proj) = scratch("redact");
        std::env::set_var("NEXUS_HOME", &home);
        let secret = "sk-abcdefghijklmnopqrstuvwxyz123456";
        save_override(Scope::User, &proj, &format!("keep this\nkey {secret}\n")).unwrap();
        let json = show_json(&proj, PromptMode::Plan, 200_000);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["mode"], "plan");
        assert_eq!(v["estimate"], ESTIMATE);
        assert!(v["tokens"].as_u64().unwrap() > 0);
        assert_eq!(v["sections"][0]["layer"], "mode");
        assert_eq!(v["sections"][0]["estimated"], true);
        let shown = json_text(&v);
        assert!(!shown.contains(secret), "{shown}");
        assert!(shown.contains("[redacted]"));
        assert!(shown.contains("keep this"));
        let user = v["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["layer"] == "user override")
            .unwrap();
        assert!(user["source"].as_str().unwrap().ends_with("system.md"));
        assert!(user["in_system"] == true);

        std::env::remove_var("NEXUS_HOME");
        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    fn json_text(v: &serde_json::Value) -> String {
        v["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["text"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn suggestion_hunks_apply_only_where_accepted() {
        let old = "keep this\nchange me\nalso keep\n";
        let reply = suggest_with(old, "shorter", |prompt| {
            assert!(prompt.contains("shorter"));
            assert!(prompt.contains("change me"));
            Ok("```markdown\nkeep this\nchanged\nalso keep\nextra\n```".into())
        })
        .unwrap();
        assert!(reply.contains("changed"));
        assert!(!reply.contains("```"));
        let hunks = diff_hunks(old, &reply);
        assert!(hunks.len() >= 2, "{hunks:?}");
        let all = vec![true; hunks.len()];
        assert_eq!(apply_hunks(old, &hunks, &all), reply);
        let none = vec![false; hunks.len()];
        assert_eq!(apply_hunks(old, &hunks, &none), old);
        let mut one = vec![false; hunks.len()];
        one[0] = true;
        let mixed = apply_hunks(old, &hunks, &one);
        assert!(mixed.contains("changed") || mixed.contains("extra"));
        assert_ne!(mixed, reply);
        assert_ne!(mixed, old);
        assert!(parse_suggestion("").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_override_is_not_written() {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (home, proj) = scratch("link");
        std::env::set_var("NEXUS_HOME", &home);
        let target = proj.join("elsewhere.md");
        fs::write(&target, "nope\n").unwrap();
        let link = home.join("system.md");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = save_override(Scope::User, &proj, "new text").unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "nope\n");
        std::env::remove_var("NEXUS_HOME");
        let _ = fs::remove_dir_all(home.parent().unwrap());
    }
}
