//! The raw-mode line editor: its internals, built-in commands, Tab completion and the live autocomplete popup.

use super::*;

// ── raw-mode editor internals ────────────────────────────────────────────────
pub(super) enum RawLine {
    Submit(String, bool), // text, continue (multiline)?
    CycleMode(Vec<char>, usize),
    Rewind, // Esc Esc on an empty composer
}

pub(super) fn viewport(buf: &[char], cursor: usize, avail: usize, scroll: usize) -> (usize, usize) {
    let avail = avail.max(1);
    let mut s = scroll;
    if cursor < s {
        s = cursor;
    } else {
        let mut cur_width: usize = buf[s..cursor].iter().copied().map(char_width).sum();
        while cur_width >= avail && s < cursor {
            cur_width -= char_width(buf[s]);
            s += 1;
        }
    }
    (s, buf[s..cursor].iter().copied().map(char_width).sum())
}

pub(super) fn redraw(
    prompt: &str,
    start: (u16, u16),
    buf: &[char],
    cursor: usize,
    scroll: &mut usize,
) {
    if ALT_SCREEN.load(Ordering::Relaxed) {
        update_open_input(prompt, buf, cursor);
        render_composer(prompt, buf, cursor, scroll);
        return;
    }
    let width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
    let avail = width.saturating_sub(start.0).max(8) as usize;
    let (s, _col) = viewport(buf, cursor, avail, *scroll);
    *scroll = s;
    let end = (s + avail).min(buf.len());
    let shown = composer_glyphs(&buf[s..end]);
    let col_width = buf[s..cursor.min(buf.len())]
        .iter()
        .copied()
        .map(char_width)
        .sum::<usize>();
    let mut out = io::stdout();
    let _ = queue!(
        out,
        MoveTo(start.0, start.1),
        Clear(ClearType::UntilNewLine)
    );
    let _ = write!(out, "{shown}");
    let _ = queue!(
        out,
        MoveTo(start.0.saturating_add(col_width as u16), start.1)
    );
    let _ = out.flush();
}

pub(super) fn prev_word(buf: &[char], mut i: usize) -> usize {
    while i > 0 && buf[i - 1].is_whitespace() {
        i -= 1;
    }
    while i > 0 && !buf[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}
pub(super) fn next_word(buf: &[char], mut i: usize) -> usize {
    let n = buf.len();
    while i < n && buf[i].is_whitespace() {
        i += 1;
    }
    while i < n && !buf[i].is_whitespace() {
        i += 1;
    }
    i
}

pub(super) fn end_word(buf: &[char], mut i: usize) -> usize {
    let n = buf.len();
    if i < n && !buf[i].is_whitespace() {
        i += 1;
    }
    while i < n && buf[i].is_whitespace() {
        i += 1;
    }
    while i + 1 < n && !buf[i + 1].is_whitespace() {
        i += 1;
    }
    i.min(n)
}

pub(super) fn edit_in_editor(current: &str) -> Option<String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let path = std::env::temp_dir().join(format!("bwn-prompt-{}.txt", std::process::id()));
    std::fs::write(&path, current).ok()?;
    let was_raw = is_raw();
    if was_raw {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
    }
    let mut parts = editor.split_whitespace();
    let cmd = parts.next().unwrap_or("vi");
    let _ = std::process::Command::new(cmd)
        .args(parts)
        .arg(&path)
        .status();
    if was_raw {
        let _ = enable_raw_mode();
        let _ = execute!(io::stdout(), EnableBracketedPaste);
    }
    let content = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    content.map(|c| c.trim_end_matches(['\n', '\r']).to_string())
}

// ── built-in commands ────────────────────────────────────────────────────────
/// A slash command the REPL handles itself: what /help, `--help`, the popup
/// and Tab completion show for it. A test in lib.rs holds this list to the
/// REPL's own match, both ways.
pub struct SlashCommand {
    pub name: &'static str,
    /// Other spellings the REPL takes.
    pub aliases: &'static [&'static str],
    /// Its arguments, as /help shows them.
    pub args: &'static str,
    pub desc: &'static str,
    /// The words Tab completes after it.
    pub subs: &'static [&'static str],
    /// The /help section it is listed under.
    pub section: &'static str,
}

pub(super) const fn cmd(
    section: &'static str,
    name: &'static str,
    aliases: &'static [&'static str],
    args: &'static str,
    desc: &'static str,
    subs: &'static [&'static str],
) -> SlashCommand {
    SlashCommand {
        name,
        aliases,
        args,
        desc,
        subs,
        section,
    }
}

pub(super) const MODES: &str = "modes and permissions";
pub(super) const WORK: &str = "context & git";
pub(super) const TALK: &str = "conversation";
pub(super) const AUTO: &str = "automation";
pub(super) const PROJECT: &str = "project";
pub(super) const SETUP: &str = "setup & screen";

/// Every built-in command, in /help order.
pub const COMMANDS: &[SlashCommand] = &[
    cmd(
        MODES,
        "/plan",
        &[],
        "[task]",
        "switch to PLAN, or plan this task",
        &[],
    ),
    cmd(
        MODES,
        "/build",
        &[],
        "[task]",
        "switch to BUILD, or do this task",
        &[],
    ),
    cmd(
        MODES,
        "/brainstorm",
        &[],
        "[task]",
        "switch to BRAINSTORM, or talk this through",
        &[],
    ),
    cmd(
        MODES,
        "/mode",
        &[],
        "[plan|build|brainstorm]",
        "show or switch mode",
        &["plan", "build", "brainstorm"],
    ),
    cmd(
        MODES,
        "/permissions",
        &[],
        "[ask|accept-edits|auto|readonly] · default <mode> · list · remove <entry> · reset",
        "permission mode and saved approvals",
        &[
            "ask",
            "accept-edits",
            "auto",
            "readonly",
            "default",
            "list",
            "remove",
            "reset",
        ],
    ),
    cmd(
        MODES,
        "/sandbox",
        &[],
        "[off|auto|require|status]",
        "OS sandbox for shell commands",
        &["off", "auto", "require", "status"],
    ),
    cmd(
        MODES,
        "/model",
        &[],
        "[name | <url> <model>]",
        "hot-swap the AI model mid-session",
        &[],
    ),
    cmd(
        MODES,
        "/effort",
        &[],
        "[off|low|medium|high]",
        "reasoning depth",
        &["off", "low", "medium", "high"],
    ),
    cmd(
        MODES,
        "/local",
        &[],
        "",
        "probe local servers and list GGUF models",
        &[],
    ),
    cmd(
        WORK,
        "/compact",
        &[],
        "",
        "compress context to free token budget",
        &[],
    ),
    cmd(WORK, "/context", &[], "", "show context window usage", &[]),
    cmd(
        WORK,
        "/cost",
        &[],
        "",
        "session tokens and estimated cost",
        &[],
    ),
    cmd(
        WORK,
        "/diff",
        &[],
        "[turn]",
        "changed files and their diffs; turn: the last turn's",
        &["turn"],
    ),
    cmd(
        WORK,
        "/review",
        &[],
        "[--base <ref>|--staged] [focus]",
        "read-only review of your changes",
        &["--base", "--staged"],
    ),
    cmd(
        WORK,
        "/commit",
        &[],
        "",
        "AI-drafted commit message, committed on c",
        &[],
    ),
    cmd(
        WORK,
        "/pr",
        &[],
        "",
        "AI-drafted PR title + description",
        &[],
    ),
    cmd(
        WORK,
        "/add-dir",
        &[],
        "[path]",
        "also work in another folder (list with no path)",
        &[],
    ),
    cmd(WORK, "/checkpoints", &[], "", "list edit checkpoints", &[]),
    cmd(
        WORK,
        "/undo",
        &[],
        "[latest|git|all|<id>]",
        "revert the last agent turn's edits",
        &["latest", "git", "all"],
    ),
    cmd(
        WORK,
        "/rewind",
        &[],
        "",
        "back to an earlier prompt (also Esc Esc)",
        &[],
    ),
    cmd(TALK, "/new", &[], "", "start a fresh session", &[]),
    cmd(
        TALK,
        "/resume",
        &[],
        "",
        "pick a saved session to resume",
        &[],
    ),
    cmd(TALK, "/rename", &[], "<name>", "name this session", &[]),
    cmd(
        TALK,
        "/export",
        &[],
        "[file]",
        "this conversation as Markdown",
        &[],
    ),
    cmd(
        TALK,
        "/copy",
        &[],
        "",
        "the last answer to the clipboard",
        &[],
    ),
    cmd(
        TALK,
        "/ask",
        &[],
        "<question>",
        "a side question, not added to the conversation",
        &[],
    ),
    cmd(
        TALK,
        "/btw",
        &[],
        "<note>",
        "add a note to your next message",
        &[],
    ),
    cmd(
        TALK,
        "/clear",
        &[],
        "",
        "clear the screen and start a fresh session",
        &[],
    ),
    cmd(
        AUTO,
        "/schedule",
        &[],
        "<delay> <task>",
        "run a task later (e.g. 5m cargo test)",
        &[],
    ),
    cmd(
        AUTO,
        "/loop",
        &[],
        "<interval> <task>",
        "run a task repeatedly (e.g. 30m)",
        &[],
    ),
    cmd(
        AUTO,
        "/workflows",
        &["/tasks"],
        "",
        "list and manage background workflows",
        &[],
    ),
    cmd(
        AUTO,
        "/teamwork",
        &["/teamwork-preview", "/swarm"],
        "",
        "how helper agents share work",
        &[],
    ),
    cmd(
        AUTO,
        "/grill-me",
        &["/align", "/interview"],
        "",
        "operational alignment interview",
        &[],
    ),
    cmd(
        PROJECT,
        "/memory",
        &[],
        "",
        "view and edit session memory",
        &[],
    ),
    cmd(
        PROJECT,
        "/skills",
        &[],
        "",
        "list skills and custom commands",
        &[],
    ),
    cmd(PROJECT, "/tools", &[], "", "browse callable tools", &[]),
    cmd(
        PROJECT,
        "/rules",
        &[],
        "",
        "inspect engineering rules and violations",
        &[],
    ),
    cmd(
        PROJECT,
        "/kb",
        &["/index"],
        "",
        "query or index project knowledge base",
        &[],
    ),
    cmd(
        PROJECT,
        "/verify",
        &["/audit"],
        "",
        "verify codebase against rules and tests",
        &[],
    ),
    cmd(
        PROJECT,
        "/agents",
        &[],
        "",
        "helper agents, then Agents.md",
        &[],
    ),
    cmd(
        PROJECT,
        "/mcp",
        &[],
        "[name|add|remove|login|logout|reload]",
        "MCP servers: list, <name>, add, remove, login, logout, reload",
        &["add", "remove", "login", "logout", "reload"],
    ),
    cmd(
        PROJECT,
        "/trace",
        &[],
        "[<id>]",
        "receipts: tool calls, hooks, skills, subagents",
        &[],
    ),
    cmd(
        PROJECT,
        "/config",
        &[],
        "",
        "configure hooks, memory, commands via AI",
        &[],
    ),
    cmd(
        SETUP,
        "/init",
        &[],
        "",
        "run setup, then offer to write AGENTS.md",
        &[],
    ),
    cmd(
        SETUP,
        "/login",
        &[],
        "",
        "replace the API key, checked before it is saved",
        &[],
    ),
    cmd(SETUP, "/doctor", &["/debug"], "", "diagnose setup", &[]),
    cmd(
        SETUP,
        "/voice",
        &[],
        "[<file>]",
        "audio transcription & voice input",
        &[],
    ),
    cmd(SETUP, "/vim", &[], "", "toggle Vim modal editing", &[]),
    cmd(
        SETUP,
        "/theme",
        &[],
        "[dark|light|ansi|auto]",
        "colour theme",
        &["dark", "light", "ansi", "auto"],
    ),
    cmd(
        SETUP,
        "/mouse",
        &["/scroll"],
        "[on|off|status]",
        "wheel scroll + drag-copy",
        &["on", "off", "status"],
    ),
    cmd(SETUP, "/help", &[], "", "show all commands and keys", &[]),
    cmd(SETUP, "/exit", &["/quit"], "", "exit the session", &[]),
];

/// The built-in command named `name`, by its name or an alias.
pub fn find_command(name: &str) -> Option<&'static SlashCommand> {
    COMMANDS
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name))
}

// Every built-in spelling, names and aliases, in /help order.
pub(super) fn builtin_names() -> impl Iterator<Item = &'static str> {
    COMMANDS
        .iter()
        .flat_map(|c| std::iter::once(c.name).chain(c.aliases.iter().copied()))
}

/// The built-in commands the popup lists (a test holds the REPL to them).
#[cfg(test)]
pub(crate) fn builtin_slash_commands() -> Vec<&'static str> {
    builtin_names().collect()
}

/// What Tab completion offers for `line` typed into the composer.
#[cfg(test)]
pub(crate) fn completions_for(line: &str) -> Vec<String> {
    let buf: Vec<char> = line.chars().collect();
    let (start, token) = token_at(&buf, buf.len());
    completions(&buf, start, &token)
}

// ── Tab completion ───────────────────────────────────────────────────────────
// Cached: the autocomplete popup consults this on every keystroke, and the
// skill/command set doesn't change within a session.
pub(super) fn load_slash_commands() -> Vec<String> {
    #[allow(clippy::type_complexity)]
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<(u64, Vec<String>)>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(None));
    let now = monotonic_ms();
    let mut lock = cache.lock().unwrap();
    if let Some((ts, cmds)) = &*lock {
        if now.saturating_sub(*ts) < 5_000 {
            return cmds.clone();
        }
    }
    let cmds = load_slash_commands_uncached();
    *lock = Some((now, cmds.clone()));
    cmds
}

pub(super) fn load_slash_commands_uncached() -> Vec<String> {
    let mut cmds: Vec<String> = builtin_names().map(str::to_string).collect();
    // Every command file the session loads (the user's, and a trusted
    // checkout's) and every skill.
    for c in crate::config::load_custom_commands() {
        let cmd = format!("/{}", c.name);
        if !cmds.contains(&cmd) {
            cmds.push(cmd);
        }
    }
    cmds
}

pub(super) fn token_at(buf: &[char], cursor: usize) -> (usize, String) {
    let mut start = cursor;
    while start > 0 && !buf[start - 1].is_whitespace() {
        start -= 1;
    }
    (start, buf[start..cursor].iter().collect())
}

// Replace the whole token, including text to the right of the cursor.
// Replacing only the prefix turns /he|lp into /helplp on completion.
pub(super) fn apply_completion(
    buf: &mut Vec<char>,
    cursor: &mut usize,
    candidate: &str,
    space: bool,
) {
    let (start, _) = token_at(buf, *cursor);
    let mut end = *cursor;
    while end < buf.len() && !buf[end].is_whitespace() {
        end += 1;
    }
    buf.splice(start..end, candidate.chars());
    *cursor = start + candidate.chars().count();
    if space && !candidate.ends_with('/') && !candidate.ends_with(':') {
        if !buf.get(*cursor).is_some_and(|c| c.is_whitespace()) {
            buf.insert(*cursor, ' ');
        }
        *cursor += 1;
    }
}

pub(super) fn common_prefix(items: &[String]) -> String {
    let mut iter = items.iter();
    let mut prefix: Vec<char> = match iter.next() {
        Some(s) => s.chars().collect(),
        None => return String::new(),
    };
    for s in iter {
        let sc: Vec<char> = s.chars().collect();
        let n = prefix
            .iter()
            .zip(sc.iter())
            .take_while(|(a, b)| a == b)
            .count();
        prefix.truncate(n);
    }
    prefix.into_iter().collect()
}

// ── live autocomplete popup ──────────────────────────────────────────────────
// As-you-type suggestions drawn just above the composer (alt-screen only):
// slash commands with one-line descriptions, sub-arguments, and @path
// mentions. ↑/↓ move the highlight, Tab/Enter accept, Esc dismisses.

pub(super) const POPUP_MAX_ROWS: usize = 8;

// One-line description shown next to each built-in command in the popup.
// User-defined commands and skills get an empty description here; see
// extra_command_desc for those.
pub(super) fn slash_command_desc(cmd: &str) -> &'static str {
    find_command(cmd).map_or("", |c| c.desc)
}

// Descriptions for the non-builtin popup entries: a skill's first line (via
// the skill loader) and "custom command" for scripts. Loaded once per
// session — the popup consults this on every keystroke, so no file reads
// after the first call.
pub(super) fn extra_command_desc(cmd: &str) -> String {
    static CACHE: std::sync::OnceLock<std::collections::HashMap<String, String>> =
        std::sync::OnceLock::new();
    let map = CACHE.get_or_init(|| {
        let mut map = std::collections::HashMap::new();
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        for (name, desc) in crate::config::load_skill_descriptions(&cwd) {
            if !desc.is_empty() {
                map.insert(format!("/{name}"), desc);
            }
        }
        for c in crate::config::load_custom_commands() {
            let key = format!("/{}", c.name);
            if c.script.is_some() {
                map.insert(key, "custom command".to_string());
            } else if let std::collections::hash_map::Entry::Vacant(slot) = map.entry(key) {
                slot.insert(custom_command_desc(&c));
            }
        }
        map
    });
    map.get(cmd).cloned().unwrap_or_default()
}

// A prompt command's popup text: its `description:` frontmatter, else the
// first prose line of its body (CustomCommand.description holds either; the
// body itself no longer carries the frontmatter).
pub(crate) fn custom_command_desc(c: &crate::config::CustomCommand) -> String {
    if c.description.trim().is_empty() {
        "custom command".to_string()
    } else {
        c.description.clone()
    }
}

// Popup description for any candidate: builtin text first, then the cached
// skill / custom-command description.
pub(super) fn popup_desc(cand: &str) -> String {
    let builtin = slash_command_desc(cand);
    if !builtin.is_empty() {
        return builtin.to_string();
    }
    if cand.starts_with('/') {
        extra_command_desc(cand)
    } else {
        String::new()
    }
}

// Candidates for the popup at the current cursor position. Only "interesting"
// tokens trigger it (slash commands, their sub-arguments, and @mentions) so
// ordinary prose never spawns a popup.
pub(super) fn popup_candidates(buf: &[char], cursor: usize) -> Vec<String> {
    let (tok_start, token) = token_at(buf, cursor);
    if token.is_empty() {
        return Vec::new();
    }
    let interesting = token.starts_with('/') || token.starts_with('@') || buf.first() == Some(&'/');
    if !interesting {
        return Vec::new();
    }
    completions(buf, tok_start, &token)
}

// Scroll window over the candidate list: (first_index, rows_shown), keeping
// the selection visible.
pub(super) fn popup_window(sel: usize, len: usize, max: usize) -> (usize, usize) {
    let show = len.min(max);
    if show == 0 {
        return (0, 0);
    }
    let first = sel.saturating_sub(show - 1).min(len - show);
    (first, show)
}

// Draw the popup over the bottom rows of the output region. Cursor position
// is saved/restored so the composer caret stays put; the caller repaints the
// transcript (render_output) once the popup shrinks or closes.
pub(super) fn render_suggestions(sug: &[String], sel: usize) {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return;
    }
    let (width, _) = term_size();
    let (first, show) = popup_window(sel, sug.len(), POPUP_MAX_ROWS);
    if show == 0 {
        return;
    }
    let pad = sug[first..first + show]
        .iter()
        .map(|c| str_width(c))
        .max()
        .unwrap_or(0);
    invalidate_inline_pixels();
    let mut out = io::stdout();
    let _ = execute!(out, SavePosition);
    // Sit just above the composer box's top border.
    let base = composer_top().saturating_sub(show as u16);
    for i in 0..show {
        let idx = first + i;
        let row = base + i as u16;
        let _ = queue!(out, MoveTo(0, row), Clear(ClearType::CurrentLine));
        let cand = &sug[idx];
        let padded = format!("{cand:<pad$}");
        // Skill and command descriptions are read from project files.
        let desc = popup_desc(cand);
        let desc = &*sanitize_terminal(&desc);
        let counter = if sug.len() > show && idx == sel {
            dim(&format!(" ({}/{})", sel + 1, sug.len()))
        } else {
            String::new()
        };
        let entry = if idx == sel {
            format!(
                "  {} {}  {}{}",
                accent("›"),
                bold(&padded),
                dim(desc),
                counter
            )
        } else {
            format!("    {padded}  {}", dim(desc))
        };
        let _ = write!(out, "{}", ellipsize_ansi_line(&entry, width as usize));
    }
    let _ = execute!(out, RestorePosition);
    let _ = out.flush();
}

pub(super) fn path_candidates(partial: &str, cwd: &std::path::Path) -> Vec<String> {
    if let Some(query) = partial.strip_prefix("kb:") {
        let kb = crate::knowledge::KnowledgeBase::new(&cwd.to_string_lossy());
        let mut out = Vec::new();
        for (id, entity) in &kb.entities {
            if id.to_lowercase().contains(&query.to_lowercase())
                || entity.name.to_lowercase().contains(&query.to_lowercase())
            {
                out.push(format!("kb:{id}"));
            }
        }
        out.sort();
        return out;
    }
    if let Some(query) = partial.strip_prefix("symbol:") {
        let kb = crate::knowledge::KnowledgeBase::new(&cwd.to_string_lossy());
        let mut out = Vec::new();
        for (id, entity) in &kb.entities {
            if matches!(
                entity.entity_type,
                crate::knowledge::EntityType::Function
                    | crate::knowledge::EntityType::Class
                    | crate::knowledge::EntityType::Interface
                    | crate::knowledge::EntityType::Module
            ) && (id.to_lowercase().contains(&query.to_lowercase())
                || entity.name.to_lowercase().contains(&query.to_lowercase()))
            {
                out.push(format!("symbol:{id}"));
            }
        }
        out.sort();
        return out;
    }
    if let Some(query) = partial.strip_prefix("rules:") {
        let mut engine = crate::rules::RuleEngine::load_defaults();
        let rules_dir = cwd.join(".buildwithnexus").join("rules");
        if let Ok(rd) = std::fs::read_dir(&rules_dir) {
            for e in rd.flatten() {
                if let Ok(loaded) =
                    crate::rules::RuleEngine::load_from_file(&e.path().to_string_lossy())
                {
                    for r in loaded.rules {
                        engine.add_rule(r);
                    }
                }
            }
        }
        let mut out = Vec::new();
        for rule in &engine.rules {
            if rule.id.to_lowercase().contains(&query.to_lowercase())
                || rule
                    .description
                    .to_lowercase()
                    .contains(&query.to_lowercase())
            {
                out.push(format!("rules:{}", rule.id));
            }
        }
        out.sort();
        return out;
    }
    let mut completion_partial = partial;
    let mut range_suffix = "";
    if let Some(idx) = completion_partial.rfind(':') {
        if completion_partial[idx + 1..]
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-')
        {
            range_suffix = &completion_partial[idx..];
            completion_partial = &completion_partial[..idx];
        }
    }
    let (base, dir, prefix) = match completion_partial.rfind('/') {
        Some(i) => (
            &completion_partial[..=i],
            cwd.join(&completion_partial[..=i]),
            &completion_partial[i + 1..],
        ),
        None => ("", cwd.to_path_buf(), completion_partial),
    };
    let mut out = Vec::new();
    if base.is_empty() {
        for special in [
            "diff", "status", "rules", "rules:", "kb:", "symbol:", "url:", "web:",
        ] {
            if special.starts_with(prefix) {
                out.push(special.to_string());
            }
        }
    }
    // Same rules as the file tools: what .gitignore or the skip list hides,
    // and sensitive files, are not offered by name either.
    for (name, is_dir) in crate::tools::visible_entries(cwd, &dir) {
        if name.starts_with(prefix) && !name.starts_with('.') {
            let mut full = format!("{base}{name}");
            if is_dir {
                full.push('/');
            } else {
                full.push_str(range_suffix);
            }
            out.push(full);
        }
    }
    out.sort();
    // A bare name (no folder typed) also finds files deeper in the tree:
    // @file_4999 offers src/mod49/sub9/file_4999.py. Same walk rules as
    // find_files, so ignored and sensitive files are never offered.
    if base.is_empty() && prefix.chars().count() >= 2 {
        for deep in crate::tools::rank_by_name(&project_files_cached(cwd), prefix, 20) {
            let full = format!("{deep}{range_suffix}");
            if !out.contains(&full) {
                out.push(full);
            }
        }
        // Folders added with /add-dir, offered by their full path.
        for root in crate::workdirs::list() {
            for deep in crate::tools::rank_by_name(&project_files_cached(&root), prefix, 20) {
                let full = format!("{}{range_suffix}", root.join(deep).display());
                if !out.contains(&full) {
                    out.push(full);
                }
            }
        }
    }
    out
}

// The project's file list for `@` completion. The popup asks on every
// keystroke, so the list is shared rather than copied, and once it is five
// seconds old it is rebuilt on a background thread while the old list keeps
// answering: typing never waits on a walk after the first one.
pub(super) fn project_files_cached(cwd: &std::path::Path) -> Arc<Vec<String>> {
    struct Entry {
        at: u64,
        dir: std::path::PathBuf,
        files: Arc<Vec<String>>,
        refreshing: bool,
    }
    // One entry per folder: the working folder and each added one.
    static CACHE: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
    let now = monotonic_ms();
    if let Ok(mut c) = CACHE.lock() {
        if let Some(e) = c.iter_mut().find(|e| e.dir == cwd) {
            if now.saturating_sub(e.at) >= 5_000 && !e.refreshing {
                e.refreshing = true;
                let dir = cwd.to_path_buf();
                std::thread::spawn(move || {
                    let files = Arc::new(crate::tools::project_files(&dir));
                    if let Ok(mut c) = CACHE.lock() {
                        if let Some(e) = c.iter_mut().find(|e| e.dir == dir) {
                            (e.at, e.files, e.refreshing) = (monotonic_ms(), files, false);
                        }
                    }
                });
            }
            return e.files.clone();
        }
    }
    let files = Arc::new(crate::tools::project_files(cwd));
    if let Ok(mut c) = CACHE.lock() {
        c.retain(|e| e.dir != cwd);
        c.push(Entry {
            at: now,
            dir: cwd.to_path_buf(),
            files: files.clone(),
            refreshing: false,
        });
    }
    files
}

// ↑/↓ history recall: nearest entry matching `prefix` (fish/zsh style —
// type "car", ↑ cycles only "car…" entries). `from=None` starts at the ends.
pub(super) fn hist_match(
    h: &[String],
    prefix: &str,
    from: Option<usize>,
    back: bool,
) -> Option<usize> {
    let hit = |i: &usize| prefix.is_empty() || h[*i].starts_with(prefix);
    if back {
        (0..from.unwrap_or(h.len())).rev().find(hit)
    } else {
        (from.map(|i| i + 1).unwrap_or(h.len())..h.len()).find(hit)
    }
}

pub(super) fn history_search(hist: &[String], query: &str, skip: usize) -> Option<String> {
    if query.is_empty() {
        return None;
    }
    hist.iter()
        .rev()
        .filter(|e| e.contains(query))
        .nth(skip)
        .cloned()
}

// Candidates come from file names, skill folders and rule ids in the
// checkout. One with escapes or bidi chars would be echoed raw by the popup
// and then by the composer once completed, so it is never offered.
pub(super) fn completions(buf: &[char], start: usize, token: &str) -> Vec<String> {
    drop_unsafe_candidates(completions_unfiltered(buf, start, token))
}

pub(super) fn drop_unsafe_candidates(mut cands: Vec<String>) -> Vec<String> {
    cands.retain(|c| matches!(sanitize_terminal(c), std::borrow::Cow::Borrowed(_)));
    cands
}

pub(super) fn completions_unfiltered(buf: &[char], start: usize, token: &str) -> Vec<String> {
    let at_line_start = buf[..start].iter().all(|c| c.is_whitespace());
    if at_line_start && token.starts_with('/') {
        let cmds = load_slash_commands();
        return cmds.into_iter().filter(|c| c.starts_with(token)).collect();
    }
    // Sub-argument completion: look at the command that precedes the current token.
    let prefix: String = buf[..start].iter().collect();
    if let Some(cmd) = find_command(prefix.trim()).filter(|c| !c.subs.is_empty()) {
        return cmd
            .subs
            .iter()
            .filter(|s| s.starts_with(token))
            .map(|s| s.to_string())
            .collect();
    }
    if let Some(partial) = token.strip_prefix('@') {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        return path_candidates(partial, &cwd)
            .into_iter()
            .map(|p| format!("@{p}"))
            .collect();
    }
    Vec::new()
}

// Ctrl+C on an empty composer: the first press says how to quit, and only a
// second one inside the window quits — so the press after "stop that turn"
// never closes the session and loses its approvals and undo marker.
pub(super) const QUIT_HINT: &str = "  press Ctrl+C again to quit (Ctrl+D quits now)";
pub(super) const QUIT_PRESS_WINDOW_MS: u64 = 2_000;

pub(super) fn ctrl_c_quits(armed_at: Option<u64>, now: u64) -> bool {
    armed_at.is_some_and(|t| now.saturating_sub(t) <= QUIT_PRESS_WINDOW_MS)
}

// Esc on an empty composer arms /rewind; a second Esc within this window,
// with no other key between, opens it.
pub(super) const ESC_ESC_WINDOW_MS: u64 = 1_000;

pub(super) fn esc_esc_rewinds(armed_at: Option<u64>, now: u64) -> bool {
    armed_at.is_some_and(|t| now.saturating_sub(t) <= ESC_ESC_WINDOW_MS)
}

// Raw mode for one read from a cooked terminal (setup, or /init after the
// session left the alternate screen), so Esc and Ctrl+C reach the prompt as
// keys instead of a SIGINT that ends the process.
pub(super) struct RawForRead(bool);

impl RawForRead {
    pub(super) fn on() -> Self {
        RawForRead(!is_raw() && enable_raw_mode().is_ok())
    }
}

impl Drop for RawForRead {
    fn drop(&mut self) {
        if self.0 {
            let _ = disable_raw_mode();
        }
    }
}

// Line editor for raw input outside the alternate screen (line mode, and
// prompts asked from a cooked terminal). It echoes what is typed and erases
// with backspace-space-backspace only: no cursor addressing and no other
// escape sequences, so it reads cleanly on TERM=dumb and to a screen reader.
// Keys follow the composer and prompt rules of read_line_raw_prefill.
pub(super) fn read_line_plain(prompt: &str, prefill: Vec<char>, composer: bool) -> Option<RawLine> {
    let _raw = RawForRead::on();
    let _keys = keyboard_lock();
    let mut out = io::stdout();
    let mut buf = prefill;
    let _ = write!(out, "{prompt}{}", buf.iter().collect::<String>());
    let _ = out.flush();
    let mut quit_armed_at: Option<u64> = None;
    let mut esc_armed_at: Option<u64> = None;
    let mut hist_idx: Option<usize> = None;
    let erase = |out: &mut io::Stdout, chars: &[char]| {
        let w: usize = chars.iter().copied().map(char_width).sum();
        let _ = write!(out, "{}", "\x08 \x08".repeat(w));
    };
    loop {
        let ev = match read() {
            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => k,
            Ok(Event::Paste(s)) => {
                let chars: Vec<char> = if composer {
                    clean_paste(&s).chars().collect()
                } else {
                    sanitize_paste(&s)
                };
                let _ = write!(out, "{}", composer_glyphs(&chars));
                buf.extend(chars);
                let _ = out.flush();
                continue;
            }
            Ok(_) => continue,
            Err(_) => {
                INPUT_CLOSED.store(true, Ordering::Relaxed);
                let _ = write!(out, "\r\n");
                let _ = out.flush();
                return None;
            }
        };
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        if !(ctrl && ev.code == KeyCode::Char('c')) {
            quit_armed_at = None;
        }
        let esc_armed = esc_armed_at.take();
        match ev.code {
            KeyCode::Esc | KeyCode::Char('c') if !composer && (ctrl || ev.code == KeyCode::Esc) => {
                let _ = write!(out, " {}\r\n", dim("cancelled"));
                let _ = out.flush();
                return None;
            }
            KeyCode::Char('d') if ctrl && buf.is_empty() => {
                let _ = write!(out, "\r\n");
                let _ = out.flush();
                return None;
            }
            KeyCode::Char('c') if ctrl => {
                if !buf.is_empty() {
                    erase(&mut out, &buf);
                    buf.clear();
                } else if ctrl_c_quits(quit_armed_at, monotonic_ms()) {
                    let _ = write!(out, "\r\n");
                    let _ = out.flush();
                    return None;
                } else {
                    quit_armed_at = Some(monotonic_ms());
                    let _ = write!(out, "\r\n{}\r\n{prompt}", yellow(QUIT_HINT));
                }
            }
            KeyCode::Esc if buf.is_empty() => {
                if esc_esc_rewinds(esc_armed, monotonic_ms()) {
                    let _ = write!(out, "\r\n");
                    let _ = out.flush();
                    return Some(RawLine::Rewind);
                }
                esc_armed_at = Some(monotonic_ms());
            }
            KeyCode::Esc => {
                erase(&mut out, &buf);
                buf.clear();
            }
            KeyCode::BackTab if composer => {
                let _ = write!(out, "\r\n");
                let _ = out.flush();
                let cursor = buf.len();
                return Some(RawLine::CycleMode(buf, cursor));
            }
            KeyCode::Enter => {
                let cont = composer && buf.last() == Some(&'\\');
                if cont {
                    buf.pop();
                }
                let _ = write!(out, "\r\n");
                let _ = out.flush();
                return Some(RawLine::Submit(buf.into_iter().collect(), cont));
            }
            KeyCode::Backspace => {
                if let Some(c) = buf.pop() {
                    erase(&mut out, &[c]);
                }
            }
            KeyCode::Char('u') if ctrl => {
                erase(&mut out, &buf);
                buf.clear();
            }
            KeyCode::Char('w') if ctrl => {
                let i = prev_word(&buf, buf.len());
                erase(&mut out, &buf[i..]);
                buf.truncate(i);
            }
            KeyCode::Up | KeyCode::Down if composer => {
                let entry = with_history(|h| {
                    let next = match (ev.code, hist_idx) {
                        (KeyCode::Up, None) => h.len().checked_sub(1),
                        (KeyCode::Up, Some(i)) => Some(i.saturating_sub(1)),
                        (_, Some(i)) if i + 1 < h.len() => Some(i + 1),
                        _ => None,
                    };
                    hist_idx = next;
                    next.map(|i| h[i].clone()).unwrap_or_default()
                });
                erase(&mut out, &buf);
                buf = clean_paste(&entry).chars().collect();
                let _ = write!(out, "{}", composer_glyphs(&buf));
            }
            KeyCode::Char(c) if !ctrl && !ev.modifiers.contains(KeyModifiers::ALT) => {
                buf.push(c);
                let _ = write!(out, "{c}");
            }
            _ => {}
        }
        let _ = out.flush();
    }
}

/// ↑/↓ through the prompt history. The first ↑ stashes the draft and
/// recalls only entries that start with it; ↓ past the newest match brings
/// the draft back instead of clearing the line. Editing a recalled entry
/// ends the recall.
#[derive(Default)]
struct HistoryRecall {
    idx: Option<usize>,
    draft: Vec<char>,
    prefix: String,
}

impl HistoryRecall {
    // Whether `buf` still shows the recalled entry; editing it ends recall.
    fn still_showing(&mut self, h: &[String], buf: &[char]) {
        if self
            .idx
            .is_some_and(|cur| buf != h[cur].chars().collect::<Vec<char>>().as_slice())
        {
            self.idx = None;
        }
    }

    fn up(&mut self, h: &[String], buf: &mut Vec<char>, cursor: &mut usize) {
        if h.is_empty() {
            return;
        }
        self.still_showing(h, buf);
        if self.idx.is_none() {
            self.draft = buf.clone();
            self.prefix = buf.iter().collect();
        }
        if let Some(idx) = hist_match(h, &self.prefix, self.idx, true) {
            self.idx = Some(idx);
            *buf = h[idx].chars().collect();
            *cursor = buf.len();
        }
    }

    fn down(&mut self, h: &[String], buf: &mut Vec<char>, cursor: &mut usize) {
        self.still_showing(h, buf);
        let Some(cur) = self.idx else {
            return;
        };
        match hist_match(h, &self.prefix, Some(cur), false) {
            Some(idx) => {
                self.idx = Some(idx);
                *buf = h[idx].chars().collect();
            }
            None => {
                self.idx = None;
                *buf = self.draft.clone();
            }
        }
        *cursor = buf.len();
    }
}

/// A key typed in vim NORMAL mode: motions, edits that fill the kill
/// buffer (and the clipboard over OSC 52), undo, and the ways into INSERT
/// and VISUAL.
fn vim_normal_key(
    c: char,
    buf: &mut Vec<char>,
    cursor: &mut usize,
    kill: &mut String,
    vim_state: &mut VimState,
    undo: &mut Vec<Vec<char>>,
) {
    match c {
        'i' => *vim_state = VimState::Insert,
        'a' => {
            if *cursor < buf.len() {
                *cursor += 1;
            }
            *vim_state = VimState::Insert;
        }
        'I' => {
            *cursor = 0;
            *vim_state = VimState::Insert;
        }
        'A' => {
            *cursor = buf.len();
            *vim_state = VimState::Insert;
        }
        'h' => *cursor = cursor.saturating_sub(1),
        'l' => {
            if *cursor < buf.len() {
                *cursor += 1;
            }
        }
        '0' | '^' => *cursor = 0,
        '$' => *cursor = buf.len(),
        'w' => *cursor = next_word(buf, *cursor),
        'b' => *cursor = prev_word(buf, *cursor),
        'e' => *cursor = end_word(buf, *cursor),
        's' => {
            if *cursor < buf.len() {
                *kill = buf[*cursor..=*cursor].iter().collect();
                buf.remove(*cursor);
            }
            *vim_state = VimState::Insert;
        }
        'o' => {
            buf.push('\n');
            *cursor = buf.len();
            *vim_state = VimState::Insert;
        }
        'O' => {
            buf.insert(0, '\n');
            *cursor = 0;
            *vim_state = VimState::Insert;
        }
        'x' => {
            if *cursor < buf.len() {
                *kill = buf[*cursor..=*cursor].iter().collect();
                buf.remove(*cursor);
                if !kill.is_empty() {
                    osc52_copy(kill);
                }
            }
        }
        'D' => {
            *kill = buf[*cursor..].iter().collect();
            buf.truncate(*cursor);
            if !kill.is_empty() {
                osc52_copy(kill);
            }
        }
        'C' => {
            *kill = buf[*cursor..].iter().collect();
            buf.truncate(*cursor);
            if !kill.is_empty() {
                osc52_copy(kill);
            }
            *vim_state = VimState::Insert;
        }
        'p' => {
            for ch in kill.chars() {
                if *cursor < buf.len() {
                    buf.insert(*cursor + 1, ch);
                    *cursor += 1;
                } else {
                    buf.push(ch);
                    *cursor = buf.len();
                }
            }
        }
        'u' => {
            if let Some(prev) = undo.pop() {
                *buf = prev;
                *cursor = (*cursor).min(buf.len());
            }
        }
        ':' => {
            buf.clear();
            buf.push('/');
            *cursor = 1;
            *vim_state = VimState::Insert;
        }
        // On an empty line `/` starts a command as typed, so
        // /vim (or any command) works from NORMAL mode.
        '/' if buf.is_empty() => {
            buf.push('/');
            *cursor = 1;
            *vim_state = VimState::Insert;
        }
        'v' => *vim_state = VimState::Visual(*cursor),
        _ => {}
    }
}

/// A key typed in vim VISUAL mode: motions extend the selection, and `d`,
/// `x` or `y` act on it and return to NORMAL.
fn vim_visual_key(
    c: char,
    buf: &mut Vec<char>,
    cursor: &mut usize,
    kill: &mut String,
    vim_state: &mut VimState,
) {
    let VimState::Visual(start_idx) = *vim_state else {
        unreachable!()
    };
    match c {
        'h' => *cursor = cursor.saturating_sub(1),
        'l' => {
            if *cursor < buf.len() {
                *cursor += 1;
            }
        }
        'w' => *cursor = next_word(buf, *cursor),
        'b' => *cursor = prev_word(buf, *cursor),
        'e' => *cursor = end_word(buf, *cursor),
        '0' | '^' => *cursor = 0,
        '$' => *cursor = buf.len(),
        'd' | 'x' => {
            let min_i = start_idx.min(*cursor);
            let max_i = start_idx.max(*cursor).min(buf.len().saturating_sub(1));
            if min_i <= max_i && max_i < buf.len() {
                *kill = buf[min_i..=max_i].iter().collect();
                buf.drain(min_i..=max_i);
                *cursor = min_i.min(buf.len());
                if !kill.is_empty() {
                    osc52_copy(kill);
                }
            }
            *vim_state = VimState::Normal;
        }
        'y' => {
            let min_i = start_idx.min(*cursor);
            let max_i = start_idx.max(*cursor).min(buf.len().saturating_sub(1));
            if min_i <= max_i && max_i < buf.len() {
                *kill = buf[min_i..=max_i].iter().collect();
                if !kill.is_empty() {
                    osc52_copy(kill);
                }
            }
            *vim_state = VimState::Normal;
        }
        _ => {}
    }
}

/// Ctrl+R: incremental search back through the prompt history, drawn on
/// the composer row. Returns the entry to send when Enter picks one;
/// otherwise `buf` and `cursor` hold the match to edit (Esc, Tab) or the
/// draft as it was (Ctrl+C, Ctrl+G, or Enter with no match).
fn reverse_search(buf: &mut Vec<char>, cursor: &mut usize, row: u16) -> Option<String> {
    let snapshot = (buf.clone(), *cursor);
    let mut query = String::new();
    let mut skip = 0usize;
    loop {
        let m = with_history(|h| history_search(h, &query, skip));
        {
            let mut out = io::stdout();
            if ALT_SCREEN.load(Ordering::Relaxed) {
                queue_composer_box(&mut out);
                let _ = write!(
                    out,
                    "{}{}",
                    dim(&format!("(reverse-i-search)`{query}`: ")),
                    m.as_deref().unwrap_or("")
                );
                queue_composer_right_border(&mut out);
            } else {
                let _ = queue!(out, MoveTo(0, row), Clear(ClearType::UntilNewLine));
                let _ = write!(
                    out,
                    "{}{}",
                    dim(&format!("(reverse-i-search)`{query}`: ")),
                    m.as_deref().unwrap_or("")
                );
            }
            let _ = out.flush();
        }
        let ev = match read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => k,
            Ok(_) => continue,
            Err(_) => {
                *buf = snapshot.0;
                *cursor = snapshot.1;
                break;
            }
        };
        let c = ev.modifiers.contains(KeyModifiers::CONTROL);
        match ev.code {
            KeyCode::Char('r') if c => {
                if m.is_some() {
                    skip += 1;
                }
            }
            KeyCode::Char('c') | KeyCode::Char('g') if c => {
                *buf = snapshot.0;
                *cursor = snapshot.1;
                break;
            }
            KeyCode::Char(ch) if !c => {
                query.push(ch);
                skip = 0;
            }
            KeyCode::Backspace => {
                query.pop();
                skip = 0;
            }
            KeyCode::Enter => {
                if m.is_some() {
                    return m;
                }
                *buf = snapshot.0;
                *cursor = snapshot.1;
                break;
            }
            KeyCode::Esc | KeyCode::Tab => {
                match m {
                    Some(e) => {
                        *buf = e.chars().collect();
                        *cursor = buf.len();
                    }
                    None => {
                        *buf = snapshot.0;
                        *cursor = snapshot.1;
                    }
                }
                break;
            }
            _ => {}
        }
    }
    None
}

// `composer` is the session's main input (ask_task): it cycles modes, shows
// the autocomplete popup, and quits on a second Ctrl+C. Anything else is a
// prompt (ask): Esc and Ctrl+C cancel it and return None.
pub(super) fn read_line_raw_prefill(
    prompt: &str,
    prefill: Vec<char>,
    prefill_cur: usize,
    composer: bool,
) -> Option<RawLine> {
    if !ALT_SCREEN.load(Ordering::Relaxed) {
        return read_line_plain(prompt, prefill, composer);
    }
    let mut start = (prompt_width(prompt) + COMPOSER_PAD, composer_row());
    let mut buf: Vec<char> = prefill;
    let mut cursor = prefill_cur.min(buf.len());
    let mut scroll = 0usize;
    // From here until this returns, the keyboard and the input box belong to
    // this prompt; `release!` hands them back before the answer is echoed.
    let mut owner = Some(InputOwner::open(prompt, &buf, cursor));
    macro_rules! release {
        () => {
            drop(owner.take())
        };
    }
    // A first Ctrl+C on an empty composer only arms quitting (see ctrl_c_quits).
    let mut quit_armed_at: Option<u64> = None;
    // A first Esc on an empty composer arms /rewind (see esc_esc_rewinds).
    let mut esc_armed_at: Option<u64> = None;
    redraw(prompt, start, &buf, cursor, &mut scroll);
    let mut recall = HistoryRecall::default();
    let mut kill = String::new();
    let mut vim_state = if is_vim_mode() {
        VimState::Normal
    } else {
        VimState::Insert
    };
    let mut vim_undo_stack: Vec<Vec<char>> = vec![buf.clone()];
    if is_vim_mode() {
        set_vim_state(0);
    }
    // Cursor: visible again (the working spinner hides it), shaped for the
    // editing mode — accent bar for insert, block for vim NORMAL.
    cursor_show();
    let mut cur_shape = if is_vim_mode() && vim_state == VimState::Normal {
        CursorShape::Block
    } else {
        CursorShape::Bar
    };
    set_cursor_shape(cur_shape);
    // Live autocomplete popup state (alt-screen only).
    let mut sug: Vec<String> = Vec::new();
    let mut sug_idx = 0usize;
    let mut sug_rows = 0usize; // rows currently drawn over the output region
    let mut sug_suppressed = false; // Esc hides the popup until the buffer changes
    let mut sug_dismissed_at: Vec<char> = Vec::new();

    macro_rules! reline {
        () => {{
            start = (prompt_width(prompt) + COMPOSER_PAD, composer_row());
            redraw(prompt, start, &buf, cursor, &mut scroll);
        }};
    }
    // When the last key came, and whether this wait was already announced.
    let mut idle_since = Instant::now();
    let mut idle_told = false;

    loop {
        // Refresh the autocomplete popup against the current buffer. Runs
        // before the blocking read so the popup tracks every edit (including
        // prefilled text on the first pass). Prompts get no popup: their
        // answers are words like `y`, and Esc there means cancel.
        if ALT_SCREEN.load(Ordering::Relaxed) {
            if sug_suppressed && buf != sug_dismissed_at {
                sug_suppressed = false;
            }
            let cands = if sug_suppressed || !composer {
                Vec::new()
            } else {
                popup_candidates(&buf, cursor)
            };
            if cands != sug {
                sug = cands;
                sug_idx = 0;
            }
            let show = sug.len().min(POPUP_MAX_ROWS);
            if show < sug_rows {
                // Popup shrank or closed: repaint the transcript rows it
                // covered, then restore the composer it clobbers.
                render_output();
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            if !sug.is_empty() {
                render_suggestions(&sug, sug_idx);
            }
            sug_rows = show;
        }
        // The composer has waited untouched: tell the idle notifier, once
        // per wait.
        if let Some((after, notify_idle)) = IDLE_NOTIFIER.get().filter(|_| composer && !idle_told) {
            loop {
                let left = after.saturating_sub(idle_since.elapsed());
                if left.is_zero() {
                    idle_told = true;
                    notify_idle();
                    reline!();
                    break;
                }
                if poll(left).unwrap_or(true) {
                    break;
                }
            }
        }
        let ev = match read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                idle_since = Instant::now();
                k
            }
            Ok(Event::Paste(s)) => {
                // Drag a screenshot onto the terminal (or paste its path):
                // the path becomes an @attachment token and the image shows
                // in the transcript immediately — no need to submit first.
                if let Some(p) = pasted_media_path(&s) {
                    for ch in attachment_token(&p).chars() {
                        buf.insert(cursor, ch);
                        cursor += 1;
                    }
                    show_image_file(&p, false);
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                    continue;
                }
                let chars = match collapse_paste(&s).filter(|_| composer) {
                    Some(token) => token.chars().collect(),
                    None if composer => clean_paste(&s).chars().collect(),
                    None => sanitize_paste(&s),
                };
                buf.splice(cursor..cursor, chars.iter().copied());
                cursor += chars.len();
                redraw(prompt, start, &buf, cursor, &mut scroll);
                continue;
            }
            Ok(Event::Mouse(m)) => {
                match m.kind {
                    MouseEventKind::ScrollUp => {
                        scroll_output(3);
                        redraw(prompt, start, &buf, cursor, &mut scroll);
                        continue;
                    }
                    MouseEventKind::ScrollDown => {
                        scroll_output(-3);
                        redraw(prompt, start, &buf, cursor, &mut scroll);
                        continue;
                    }
                    MouseEventKind::Down(MouseButton::Left) if in_output_region(m.row) => {
                        selection_start(m.row, m.column);
                        redraw(prompt, start, &buf, cursor, &mut scroll);
                        continue;
                    }
                    MouseEventKind::Drag(MouseButton::Left) if in_output_region(m.row) => {
                        selection_drag(m.row, m.column);
                        redraw(prompt, start, &buf, cursor, &mut scroll);
                        continue;
                    }
                    MouseEventKind::Up(MouseButton::Left) if in_output_region(m.row) => {
                        selection_finish(m.row, m.column);
                        redraw(prompt, start, &buf, cursor, &mut scroll);
                        continue;
                    }
                    _ => {}
                }
                // Left-click or drag on the input row moves the cursor to the clicked/dragged column.
                if (m.kind == MouseEventKind::Down(MouseButton::Left)
                    || m.kind == MouseEventKind::Drag(MouseButton::Left))
                    && m.row == start.1
                {
                    let col = m.column as usize;
                    if col >= start.0 as usize {
                        let target_col = col - start.0 as usize;
                        let mut current_col = 0;
                        let mut target_idx = scroll;
                        while target_idx < buf.len() {
                            let w = char_width(buf[target_idx]);
                            if current_col + w > target_col {
                                break;
                            }
                            current_col += w;
                            target_idx += 1;
                        }
                        cursor = target_idx.min(buf.len());
                        redraw(prompt, start, &buf, cursor, &mut scroll);
                    }
                }
                continue;
            }
            Ok(Event::Resize(_, _)) => {
                if ALT_SCREEN.load(Ordering::Relaxed) {
                    set_output_region();
                    render_output();
                    clear_composer();
                    start = (prompt_width(prompt) + COMPOSER_PAD, composer_row());
                    scroll = 0;
                }
                redraw(prompt, start, &buf, cursor, &mut scroll);
                continue;
            }
            Ok(Event::FocusGained) => {
                set_focused(true);
                continue;
            }
            Ok(Event::FocusLost) => {
                set_focused(false);
                continue;
            }
            Ok(_) => continue,
            Err(_) => {
                INPUT_CLOSED.store(true, Ordering::Relaxed);
                return None;
            }
        };
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let alt = ev.modifiers.contains(KeyModifiers::ALT);
        if !(ctrl && ev.code == KeyCode::Char('c')) {
            quit_armed_at = None;
        }
        let esc_armed = esc_armed_at.take();
        // A prompt is cancelled by Esc or Ctrl+C whatever was typed, and by
        // Ctrl+D on an empty line: the caller gets None and the box says so.
        let cancel = !composer
            && match ev.code {
                KeyCode::Esc => true,
                KeyCode::Char('c') => ctrl,
                KeyCode::Char('d') => ctrl && buf.is_empty(),
                _ => false,
            };
        if cancel {
            release!();
            echo_submitted(prompt, &dim("cancelled"));
            return None;
        }
        match ev.code {
            KeyCode::PageUp => {
                scroll_page_up();
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::PageDown => {
                scroll_page_down();
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Up if alt => {
                scroll_output(1);
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Down if alt => {
                scroll_output(-1);
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Home if alt => {
                scroll_output(isize::MAX / 4);
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::End if alt => {
                scroll_to_bottom();
                clear_composer();
                render_footer();
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            // Shift+Tab changes mode while retaining the draft. Ordinary
            // questions/approval prompts do not support mode changes.
            KeyCode::BackTab if composer => {
                release!();
                clear_composer();
                flush();
                return Some(RawLine::CycleMode(buf, cursor));
            }
            KeyCode::Tab if ev.modifiers.contains(KeyModifiers::SHIFT) && composer => {
                release!();
                clear_composer();
                flush();
                return Some(RawLine::CycleMode(buf, cursor));
            }
            KeyCode::BackTab => {}
            KeyCode::Tab if ev.modifiers.contains(KeyModifiers::SHIFT) => {}
            // Composer only (a prompt was cancelled above): Ctrl+C clears the
            // draft; on an empty draft it quits only when pressed twice.
            KeyCode::Char('c') if ctrl => {
                if !buf.is_empty() {
                    buf.clear();
                    cursor = 0;
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                } else if ctrl_c_quits(quit_armed_at, monotonic_ms()) {
                    release!();
                    clear_composer();
                    flush();
                    return None;
                } else {
                    quit_armed_at = Some(monotonic_ms());
                    line(&yellow(QUIT_HINT));
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                }
            }
            KeyCode::Char('d') if ctrl => {
                if buf.is_empty() {
                    release!();
                    clear_composer();
                    flush();
                    return None;
                }
            }
            KeyCode::Char('a') if ctrl => {
                cursor = 0;
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('e') if ctrl => {
                cursor = buf.len();
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('u') if ctrl => {
                kill = buf[..cursor].iter().collect();
                buf.drain(..cursor);
                cursor = 0;
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('k') if ctrl => {
                kill = buf[cursor..].iter().collect();
                buf.truncate(cursor);
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('w') if ctrl => {
                let i = prev_word(&buf, cursor);
                kill = buf[i..cursor].iter().collect();
                buf.drain(i..cursor);
                cursor = i;
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('y') if ctrl => {
                for c in kill.chars() {
                    buf.insert(cursor, c);
                    cursor += 1;
                }
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('v') if ctrl => {
                // Paste from the system clipboard: an image lands as a temp
                // png and inserts an @path attachment token; plain text
                // inserts at the cursor (for terminals that pass Ctrl+V
                // through instead of translating it to a Paste event).
                if let Some(img) = crate::media::clipboard_image_to_temp() {
                    for ch in attachment_token(&img).chars() {
                        buf.insert(cursor, ch);
                        cursor += 1;
                    }
                    // Show the screenshot right now — pixel-perfect on
                    // kitty/Ghostty, half-block art elsewhere — so you can
                    // see what the model will see before you send it.
                    if !show_image_file(&img, false) {
                        line(&dim(&format!("  ⎘ clipboard image → {}", img.display())));
                    }
                } else if let Some(text) = crate::media::clipboard_text() {
                    let chars: Vec<char> = if composer {
                        clean_paste(&text).chars().collect()
                    } else {
                        sanitize_paste(&text)
                    };
                    buf.splice(cursor..cursor, chars.iter().copied());
                    cursor += chars.len();
                }
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('l') if ctrl => reline!(),
            KeyCode::Char('g') if ctrl => {
                let cur: String = buf.iter().collect();
                if let Some(edited) = edit_in_editor(&cur) {
                    if edited.contains('\n') {
                        // A multi-line edit is submitted as-is: the composer is
                        // single-line, and flattening the newlines to spaces
                        // silently destroys the structure the user just wrote.
                        // ask_task joins submitted lines with '\n', so the
                        // text goes through untouched.
                        buf = edited.chars().collect();
                        cursor = buf.len();
                        reline!();
                        release!();
                        echo_submitted(prompt, &edited);
                        return Some(RawLine::Submit(edited, false));
                    }
                    buf = edited.chars().collect();
                    cursor = buf.len();
                }
                reline!();
            }
            KeyCode::Char('r') if ctrl => {
                if let Some(e) = reverse_search(&mut buf, &mut cursor, start.1) {
                    release!();
                    echo_submitted(prompt, &e);
                    return Some(RawLine::Submit(e, false));
                }
                reline!();
            }
            KeyCode::Char('b') if alt => {
                cursor = prev_word(&buf, cursor);
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char('f') if alt => {
                cursor = next_word(&buf, cursor);
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                if is_vim_mode() && vim_state == VimState::Normal {
                    vim_normal_key(
                        c,
                        &mut buf,
                        &mut cursor,
                        &mut kill,
                        &mut vim_state,
                        &mut vim_undo_stack,
                    );
                } else if is_vim_mode() && matches!(vim_state, VimState::Visual(_)) {
                    vim_visual_key(c, &mut buf, &mut cursor, &mut kill, &mut vim_state);
                } else {
                    if is_vim_mode() {
                        vim_undo_stack.push(buf.clone());
                        if vim_undo_stack.len() > 50 {
                            vim_undo_stack.remove(0);
                        }
                    }
                    buf.insert(cursor, c);
                    cursor += 1;
                }
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Backspace => {
                if cursor > 0 {
                    if is_vim_mode() && vim_state == VimState::Normal {
                        cursor -= 1;
                    } else {
                        buf.remove(cursor - 1);
                        cursor -= 1;
                    }
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                }
            }
            KeyCode::Delete => {
                if cursor < buf.len() {
                    buf.remove(cursor);
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                }
            }
            // Ctrl/Alt + arrows jump by word (matching Alt+B/F).
            KeyCode::Left if ctrl || alt => {
                cursor = prev_word(&buf, cursor);
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Right if ctrl || alt => {
                cursor = next_word(&buf, cursor);
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Left => {
                cursor = cursor.saturating_sub(1);
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Right => {
                if cursor < buf.len() {
                    cursor += 1;
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                }
            }
            KeyCode::Home => {
                cursor = 0;
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::End => {
                cursor = buf.len();
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Up => {
                if !sug.is_empty() {
                    sug_idx = if sug_idx == 0 {
                        sug.len() - 1
                    } else {
                        sug_idx - 1
                    };
                    continue; // loop top re-renders the popup
                }
                with_history(|h| recall.up(h, &mut buf, &mut cursor));
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Down => {
                if !sug.is_empty() {
                    sug_idx = (sug_idx + 1) % sug.len();
                    continue; // loop top re-renders the popup
                }
                with_history(|h| recall.down(h, &mut buf, &mut cursor));
                redraw(prompt, start, &buf, cursor, &mut scroll);
            }
            KeyCode::Enter => {
                // Accept the highlighted autocomplete entry first: a
                // line-start /command submits immediately; any other token
                // (sub-argument, @path) is inserted and editing continues.
                // A word already typed in full (`/mode build`, `/exit`) is
                // not a request to complete it: Enter submits as typed.
                let typed_in_full = {
                    let (_, token) = token_at(&buf, cursor);
                    sug.iter().any(|c| c.trim_end() == token)
                };
                if !sug.is_empty() && !typed_in_full {
                    let cand = sug[sug_idx].clone();
                    let (tok_start, token) = token_at(&buf, cursor);
                    let is_cmd = token.starts_with('/')
                        && buf[..tok_start].iter().all(|c| c.is_whitespace());
                    apply_completion(&mut buf, &mut cursor, &cand, !is_cmd);
                    if !is_cmd {
                        redraw(prompt, start, &buf, cursor, &mut scroll);
                        continue;
                    }
                    // Fall through to submit; echo_submitted repaints the
                    // rows the popup covered.
                }
                // Only a TRAILING backslash at end-of-line continues to the
                // next line; a backslash left of the cursor mid-line (e.g. in
                // a Windows path) must not trigger continuation.
                let cont = buf.last() == Some(&'\\');
                if cont {
                    buf.pop();
                    cursor = cursor.min(buf.len());
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                }
                let text: String = buf.iter().collect();
                release!();
                if !cont {
                    echo_submitted(prompt, &text);
                }
                return Some(RawLine::Submit(text, cont));
            }
            KeyCode::Tab => {
                if !sug.is_empty() {
                    let cand = sug[sug_idx].clone();
                    apply_completion(&mut buf, &mut cursor, &cand, true);
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                    continue;
                }
                let (tok_start, token) = token_at(&buf, cursor);
                let cands = completions(&buf, tok_start, &token);
                if cands.len() == 1 {
                    let cand = &cands[0];
                    apply_completion(&mut buf, &mut cursor, cand, true);
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                } else if cands.len() > 1 {
                    let common = common_prefix(&cands);
                    if common.chars().count() > token.chars().count() {
                        apply_completion(&mut buf, &mut cursor, &common, false);
                        redraw(prompt, start, &buf, cursor, &mut scroll);
                    } else {
                        clear_composer();
                        for c in &cands {
                            line(&format!("  {}", dim(c)));
                        }
                        flush();
                        reline!();
                    }
                }
            }
            KeyCode::Esc => {
                if !sug.is_empty() {
                    // Dismiss the popup only; it stays hidden until the
                    // buffer changes again.
                    sug_suppressed = true;
                    sug_dismissed_at = buf.clone();
                    continue; // loop top clears the popup rows
                }
                // Esc Esc on an empty composer (vim: in NORMAL) opens /rewind.
                if composer && buf.is_empty() && (!is_vim_mode() || vim_state == VimState::Normal) {
                    if esc_esc_rewinds(esc_armed, monotonic_ms()) {
                        release!();
                        clear_composer();
                        flush();
                        return Some(RawLine::Rewind);
                    }
                    esc_armed_at = Some(monotonic_ms());
                }
                if is_vim_mode() && vim_state != VimState::Normal {
                    vim_state = VimState::Normal;
                    cursor = cursor.saturating_sub(1);
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                } else if !is_vim_mode() {
                    buf.clear();
                    cursor = 0;
                    redraw(prompt, start, &buf, cursor, &mut scroll);
                }
            }
            _ => {}
        }
        if is_vim_mode() {
            let val = match vim_state {
                VimState::Normal => 0,
                VimState::Insert => 1,
                VimState::Visual(_) => 2,
            };
            set_vim_state(val);
        }
        // Cursor shape tracks the editing mode: accent bar for insert,
        // steady block for vim NORMAL, underline for VISUAL.
        let want = if !is_vim_mode() {
            CursorShape::Bar
        } else {
            match vim_state {
                VimState::Insert => CursorShape::Bar,
                VimState::Normal => CursorShape::Block,
                VimState::Visual(_) => CursorShape::Underline,
            }
        };
        if want != cur_shape {
            cur_shape = want;
            set_cursor_shape(want);
        }
    }
}
