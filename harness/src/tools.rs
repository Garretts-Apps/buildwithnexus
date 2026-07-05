// The tool surface the agent can call. Definitions are data; execution is a
// single `match` — no registry, no dyn dispatch (Casey: don't build the plugin
// system before there are plugins).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::checkpoint;

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value, // JSON Schema for the input object
}

pub struct Outcome {
    pub content: String,
    pub is_error: bool,
    pub finished: bool, // the `finish` tool ends the loop
}

fn ok(s: impl Into<String>) -> Outcome {
    Outcome {
        content: s.into(),
        is_error: false,
        finished: false,
    }
}
fn err(s: impl Into<String>) -> Outcome {
    Outcome {
        content: s.into(),
        is_error: true,
        finished: false,
    }
}

const MAX_READ: usize = 100 * 1024;
const MAX_OUT: usize = 16 * 1024;
const MAX_SEARCH_FILES: usize = 10_000;
const DEFAULT_SEARCH_LIMIT: usize = 100;

// Exposed only for the criterion perf suite (see provider::bench).
#[doc(hidden)]
pub mod bench {
    use std::path::{Path, PathBuf};
    pub fn normalize(p: &Path) -> PathBuf {
        super::normalize(p)
    }
    pub fn truncate(s: String, max: usize) -> String {
        super::truncate(s, max)
    }
}

pub fn defs(include_subagent: bool) -> Vec<ToolDef> {
    let mut v = vec![
        ToolDef { name: "read_file", description: "Read a UTF-8 text file and return its contents. Optional start_line/end_line return a bounded line range. Works anywhere on the filesystem and expands `~`.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"start_line":{"type":"integer","minimum":1},"end_line":{"type":"integer","minimum":1}},"required":["path"]}) },
        ToolDef { name: "read_many_files", description: "Read several UTF-8 text files at once. Use for comparing related files without repeated round trips.",
            schema: json!({"type":"object","properties":{"paths":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":20},"max_bytes_per_file":{"type":"integer","minimum":1000,"maximum":100000}},"required":["paths"]}) },
        ToolDef { name: "list_dir", description: "List entries in a directory. Works anywhere on the filesystem and expands `~`.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}) },
        ToolDef { name: "list_tree", description: "Recursively list a bounded directory tree, skipping heavy dependency/build directories. Use before guessing paths.",
            schema: json!({"type":"object","properties":{"path":{"type":"string","default":"."},"max_depth":{"type":"integer","minimum":1,"maximum":8},"max_entries":{"type":"integer","minimum":1,"maximum":1000}}}) },
        ToolDef { name: "file_info", description: "Return metadata for a file or directory: type, size, readonly flag, and modified time when available.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}) },
        ToolDef { name: "find_paths", description: "Recursively find files and/or directories by a simple case-insensitive glob pattern such as `nexus`, `*project*`, or `*.rs`. Use kind=`dir` when the user asks for a folder. Expands `~` and skips heavy build/dependency directories.",
            schema: json!({"type":"object","properties":{"root":{"type":"string","default":"."},"pattern":{"type":"string"},"kind":{"type":"string","enum":["any","file","dir"],"default":"any"},"max":{"type":"integer","minimum":1,"maximum":500}},"required":["pattern"]}) },
        ToolDef { name: "find_files", description: "Recursively find files by a simple case-insensitive glob pattern such as `*.rs`, `src/*test*`, or `*project*`. Expands `~` and skips heavy build/dependency directories.",
            schema: json!({"type":"object","properties":{"root":{"type":"string","default":"."},"pattern":{"type":"string"},"max":{"type":"integer","minimum":1,"maximum":500}},"required":["pattern"]}) },
        ToolDef { name: "grep_files", description: "Search text files for a literal pattern and return path:line matches. Expands `~`; optional file_pattern limits searched files, e.g. `*.rs`.",
            schema: json!({"type":"object","properties":{"root":{"type":"string","default":"."},"pattern":{"type":"string"},"file_pattern":{"type":"string"},"case_sensitive":{"type":"boolean","default":false},"max":{"type":"integer","minimum":1,"maximum":500}},"required":["pattern"]}) },
        ToolDef { name: "write_file", description: "Create or overwrite a file with the given contents.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}) },
        ToolDef { name: "edit_file", description: "Replace the unique occurrence of `old` with `new` in a file.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"old":{"type":"string"},"new":{"type":"string"}},"required":["path","old","new"]}) },
        ToolDef { name: "multi_edit", description: "Apply several unique text replacements to one file in order. Fails without writing if any old text is missing or ambiguous.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"edits":{"type":"array","items":{"type":"object","properties":{"old":{"type":"string"},"new":{"type":"string"}},"required":["old","new"]},"minItems":1,"maxItems":50}},"required":["path","edits"]}) },
        ToolDef { name: "apply_patch", description: "Apply a unified diff patch to the current repository using git apply. Use for multi-file edits when exact patch context is known.",
            schema: json!({"type":"object","properties":{"patch":{"type":"string"}},"required":["patch"]}) },
        ToolDef { name: "create_dir", description: "Create a directory and any missing parents.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}) },
        ToolDef { name: "move_path", description: "Rename or move a file or directory.",
            schema: json!({"type":"object","properties":{"from":{"type":"string"},"to":{"type":"string"}},"required":["from","to"]}) },
        ToolDef { name: "remove_path", description: "Remove a file or empty directory. Does not recursively remove non-empty directories.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}) },
        ToolDef { name: "run_command", description: "Run a shell command (grep, find, git, cargo, etc.) and return its output. Use this for searching, building, or any shell operation.",
            schema: json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}) },
        ToolDef { name: "todo_write", description: "Replace the current task todo list with structured items. Use to track multi-step work.",
            schema: json!({"type":"object","properties":{"items":{"type":"array","items":{"type":"object","properties":{"task":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed"]}},"required":["task","status"]},"minItems":0,"maxItems":30}},"required":["items"]}) },
        ToolDef { name: "todo_read", description: "Read the current task todo list.",
            schema: json!({"type":"object","properties":{}}) },
        ToolDef { name: "create_docx", description: "Create a simple .docx document from a title and markdown-like body text. Supports headings, bullets, numbered items, and paragraphs.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"title":{"type":"string"},"body":{"type":"string"}},"required":["path","title","body"]}) },
        ToolDef { name: "finish", description: "Signal the task is complete with a short summary for the user.",
            schema: json!({"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"]}) },
        ToolDef { name: "save_memory", description: "Save a short note to persistent memory so it's available in future sessions. Use for preferences, recurring facts, or things the user says to remember.",
            schema: json!({"type":"object","properties":{"note":{"type":"string","description":"Short fact or preference to remember (one line)"}},"required":["note"]}) },
        ToolDef { name: "fetch_url", description: "Fetch the content of a URL via HTTP GET. Returns the response body as text. Use for reading documentation, API responses, changelogs, or any web content.",
            schema: json!({"type":"object","properties":{"url":{"type":"string","description":"HTTP or HTTPS URL to fetch"}},"required":["url"]}) },
        ToolDef { name: "web_search", description: "Search the web via DuckDuckGo and return relevant excerpts. Use for current events, documentation lookup, or any question that benefits from live web results.",
            schema: json!({"type":"object","properties":{"query":{"type":"string","description":"Search query"}},"required":["query"]}) },
        ToolDef { name: "list_skills", description: "List available skill names and short descriptions. Use before load_skill when choosing task-specific instructions.",
            schema: json!({"type":"object","properties":{}}) },
        ToolDef { name: "load_skill", description: "Load the full instructions for one named skill. Use only when that skill is relevant to the task.",
            schema: json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}) },
    ];
    if include_subagent {
        v.push(ToolDef {
            name: "spawn_subagent",
            description: "Delegate a self-contained sub-task to a fresh agent with its own context window. Set isolate=true to run it in an isolated git worktree. Returns the subagent's summary.",
            schema: json!({"type":"object","properties":{
                "task":{"type":"string"},
                "role":{"type":"string","enum":["engineer","researcher"]},
                "isolate":{"type":"boolean"}
            },"required":["task"]}),
        });
    }
    v
}

// Mutating tools pass through the permission gate; reads never do.
pub fn is_mutating(name: &str) -> bool {
    matches!(
        name,
        "write_file"
            | "edit_file"
            | "multi_edit"
            | "apply_patch"
            | "create_dir"
            | "move_path"
            | "remove_path"
            | "run_command"
            | "todo_write"
            | "create_docx"
    )
}

// Commands that are unambiguously read-only (grep, find, cat, etc.) — allowed
// even in ReadOnly permission mode despite run_command being generically mutating.
pub fn is_readonly_command(cmd: &str) -> bool {
    let lower = cmd.trim().to_lowercase();
    let first = lower.split_whitespace().next().unwrap_or("");
    let base = first.rsplit('/').next().unwrap_or(first);
    matches!(
        base,
        "grep"
            | "egrep"
            | "fgrep"
            | "rg"
            | "find"
            | "cat"
            | "ls"
            | "head"
            | "tail"
            | "wc"
            | "sort"
            | "uniq"
            | "diff"
            | "tree"
            | "stat"
            | "file"
            | "jq"
            | "sed"
    ) || lower.starts_with("git log")
        || lower.starts_with("git status")
        || lower.starts_with("git diff")
        || lower.starts_with("git show")
        || lower.starts_with("git branch")
        || lower.starts_with("git tag")
        || lower.starts_with("git remote")
}

// A one-line, human-readable preview of what a call will do (shown at the gate).
pub fn preview(name: &str, input: &Value) -> String {
    match name {
        "write_file" => format!("write {}", input["path"].as_str().unwrap_or("?")),
        "edit_file" => format!("edit {}", input["path"].as_str().unwrap_or("?")),
        "multi_edit" => format!("multi-edit {}", input["path"].as_str().unwrap_or("?")),
        "apply_patch" => "apply patch".to_string(),
        "create_dir" => format!("mkdir {}", input["path"].as_str().unwrap_or("?")),
        "move_path" => format!(
            "move {} -> {}",
            input["from"].as_str().unwrap_or("?"),
            input["to"].as_str().unwrap_or("?")
        ),
        "remove_path" => format!("remove {}", input["path"].as_str().unwrap_or("?")),
        "create_docx" => format!("create docx {}", input["path"].as_str().unwrap_or("?")),
        "run_command" => format!("run: {}", input["command"].as_str().unwrap_or("?")),
        "find_paths" => format!("find paths: {}", input["pattern"].as_str().unwrap_or("?")),
        "find_files" => format!("find files: {}", input["pattern"].as_str().unwrap_or("?")),
        "grep_files" => format!("grep: {}", input["pattern"].as_str().unwrap_or("?")),
        "spawn_subagent" => format!("subagent: {}", input["task"].as_str().unwrap_or("?")),
        "fetch_url" => format!("GET {}", input["url"].as_str().unwrap_or("?")),
        "list_skills" => "list skills".to_string(),
        "load_skill" => format!("load skill: {}", input["name"].as_str().unwrap_or("?")),
        _ => name.to_string(),
    }
}

fn resolve(cwd: &Path, p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    if p == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

// Marker placed on a tool call whose JSON arguments failed to parse, so the
// agent can feed the model a clear error instead of running with empty fields.
pub const INVALID_ARGS: &str = "__bwn_invalid_args__";

// The filesystem path a call touches (for confinement checks), resolved against
// cwd. None for non-path tools.
pub fn touched_path(name: &str, input: &Value, cwd: &Path) -> Option<PathBuf> {
    match name {
        "read_file" | "list_dir" | "list_tree" | "file_info" | "write_file" | "edit_file"
        | "multi_edit" | "create_dir" | "remove_path" | "create_docx" => {
            Some(resolve(cwd, input["path"].as_str().unwrap_or("")))
        }
        "move_path" => Some(resolve(cwd, input["from"].as_str().unwrap_or(""))),
        "find_paths" | "find_files" | "grep_files" => {
            Some(resolve(cwd, input["root"].as_str().unwrap_or(".")))
        }
        _ => None,
    }
}

// Lexically fold `.`/`..` without touching the filesystem (works for paths that
// don't exist yet, e.g. write targets).
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// True if the path resolves outside the working directory.
pub fn escapes_cwd(p: &Path, cwd: &Path) -> bool {
    let base = cwd.canonicalize().unwrap_or_else(|_| normalize(cwd));
    !normalize(p).starts_with(&base)
}

// Paths that should never be read/written without explicit confirmation, even in
// auto mode — credential stores and key material are the prime exfil targets.
pub fn is_sensitive(p: &Path) -> bool {
    let s = normalize(p).to_string_lossy().to_lowercase();
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    s.contains("/.ssh/")
        || s.contains("/.buildwithnexus/")
        || s.contains("/.aws/")
        || s.contains("/.gnupg/")
        || name == ".env.keys"
        || name == ".env"
        || name.starts_with(".env.")
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || name.ends_with(".pem")
}

// ── WSL2 filesystem boundary guard ───────────────────────────────────────────

/// True when running inside WSL2 (Windows Subsystem for Linux).
pub fn is_wsl() -> bool {
    // WSL sets WSL_DISTRO_NAME in every shell launched from Windows Terminal.
    if std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some() {
        return true;
    }
    // Fallback: /proc/version contains "Microsoft" or "WSL" on WSL kernels.
    std::fs::read_to_string("/proc/version")
        .map(|v| {
            let l = v.to_lowercase();
            l.contains("microsoft") || l.contains("wsl")
        })
        .unwrap_or(false)
}

/// True when `path` is on a Windows drive mount (e.g. /mnt/c/) inside WSL2.
/// Writes to these paths cross the WSL2 → Windows boundary and always need
/// confirmation so the agent can't silently mutate the host filesystem.
pub fn is_wsl_windows_mount(path: &Path) -> bool {
    if !is_wsl() {
        return false;
    }
    use std::path::Component;
    let normed = normalize(path);
    let comps = normed.components().collect::<Vec<_>>();
    // /mnt/<single-letter>/ → Windows drive mount
    if comps.len() >= 3 {
        if let (Component::RootDir, Component::Normal(mnt), Component::Normal(drive)) =
            (&comps[0], &comps[1], &comps[2])
        {
            return mnt.to_str() == Some("mnt")
                && drive
                    .to_str()
                    .map(|d| d.len() == 1 && d.is_ascii())
                    .unwrap_or(false);
        }
    }
    false
}

/// True when a shell command string references /mnt/<drive>/ paths in WSL2.
/// Catches `cp /mnt/c/foo .`, `rm /mnt/d/bar`, etc.
pub fn command_touches_wsl_mount(cmd: &str) -> bool {
    if !is_wsl() {
        return false;
    }
    // Simple heuristic: look for /mnt/<single-letter>/ token in the command.
    cmd.split_whitespace().any(|tok| {
        tok.starts_with("/mnt/")
            && tok.len() > 6
            && tok
                .as_bytes()
                .get(5)
                .map(|b| b.is_ascii_alphabetic())
                .unwrap_or(false)
    })
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        }
    }
    out
}

// Strip HTML for search result display. Removes script/style blocks,
// HTML tags, and collapses whitespace. Keeps meaningful text content.
fn strip_html(html: &str) -> String {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len / 3);
    let mut i = 0;
    let mut in_tag = false;
    // Tracks whether we're inside a script or style block (suppresses content).
    let mut skip_depth = 0u8;

    while i < len {
        // Check for <script or <style open tags — enter suppression mode.
        if !in_tag && skip_depth == 0 && bytes[i] == b'<' {
            let rest = &bytes[i..];
            let is_skip = |tag: &[u8]| {
                rest.len() > tag.len()
                    && rest[..tag.len()].eq_ignore_ascii_case(tag)
                    && (rest
                        .get(tag.len())
                        .is_none_or(|b| !b.is_ascii_alphanumeric()))
            };
            if is_skip(b"<script") || is_skip(b"<style") {
                skip_depth = 1;
                in_tag = true;
                i += 1;
                continue;
            }
        }
        if bytes[i] == b'<' {
            if skip_depth > 0 {
                // Inside suppressed block: look for </script> or </style>.
                let rest = &bytes[i..];
                let ends =
                    |t: &[u8]| rest.len() >= t.len() && rest[..t.len()].eq_ignore_ascii_case(t);
                if ends(b"</script>") || ends(b"</style>") {
                    skip_depth = 0;
                    // Skip past the closing tag.
                    while i < len && bytes[i] != b'>' {
                        i += 1;
                    }
                    i += 1;
                    continue;
                }
            }
            in_tag = true;
            i += 1;
            continue;
        }
        if bytes[i] == b'>' {
            in_tag = false;
            if skip_depth == 0 {
                out.push(' ');
            }
            i += 1;
            continue;
        }
        if !in_tag && skip_depth == 0 {
            out.push(bytes[i] as char);
        }
        i += 1;
    }

    // Collapse whitespace and decode common HTML entities.
    let decoded = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&nbsp;", " ")
        .replace("&#39;", "'");

    let mut result = String::new();
    let mut prev_ws = true;
    for c in decoded.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                result.push(' ');
            }
            prev_ws = true;
        } else {
            result.push(c);
            prev_ws = false;
        }
    }
    result.trim().to_string()
}

// Commands so destructive they require confirmation in every mode.
pub fn catastrophic(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    let nospace: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    nospace.contains("rm-rf/")          // rm -rf of an absolute path
        || nospace.contains("rm-fr/")
        || nospace.contains(":(){:|:&};:") // fork bomb
        || nospace.contains("mkfs")
        || nospace.contains("of=/dev/")    // dd onto a device
        || nospace.contains(">/dev/sd")
        || nospace.contains(">/dev/nvme")
        || nospace.contains("chmod-r777/")
}

fn truncate(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    // Back off to a UTF-8 char boundary — String::truncate / slicing panics if
    // `max` lands inside a multibyte glyph, which arbitrary file/command output
    // can easily produce.
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("\n…[truncated]");
    out
}

fn line_range(input: &Value) -> Option<(usize, usize)> {
    let start = input["start_line"].as_u64().map(|n| n.max(1) as usize);
    let end = input["end_line"].as_u64().map(|n| n.max(1) as usize);
    match (start, end) {
        (Some(s), Some(e)) => Some((s, e.max(s))),
        (Some(s), None) => Some((s, usize::MAX)),
        (None, Some(e)) => Some((1, e)),
        (None, None) => None,
    }
}

fn apply_line_range(text: &str, range: Option<(usize, usize)>) -> String {
    let Some((start, end)) = range else {
        return text.to_string();
    };
    text.lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let line_no = i + 1;
            if line_no >= start && line_no <= end {
                Some(line)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn skip_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".next"
            | ".turbo"
            | ".cache"
            | "vendor"
            | "__pycache__"
    )
}

fn collect_files(root: &Path, out: &mut Vec<PathBuf>, seen: &mut usize) {
    if *seen >= MAX_SEARCH_FILES {
        return;
    }
    let Ok(rd) = fs::read_dir(root) else {
        return;
    };
    for entry in rd.filter_map(|e| e.ok()) {
        if *seen >= MAX_SEARCH_FILES {
            break;
        }
        let path = entry.path();
        if path.is_dir() {
            if !skip_dir(&path) {
                collect_files(&path, out, seen);
            }
        } else if path.is_file() {
            *seen += 1;
            out.push(path);
        }
    }
}

fn collect_paths(root: &Path, out: &mut Vec<PathBuf>, seen: &mut usize, dirs: bool, files: bool) {
    if *seen >= MAX_SEARCH_FILES {
        return;
    }
    let Ok(rd) = fs::read_dir(root) else {
        return;
    };
    for entry in rd.filter_map(|e| e.ok()) {
        if *seen >= MAX_SEARCH_FILES {
            break;
        }
        let path = entry.path();
        if path.is_dir() {
            if dirs && !skip_dir(&path) {
                *seen += 1;
                out.push(path.clone());
            }
            if !skip_dir(&path) {
                collect_paths(&path, out, seen, dirs, files);
            }
        } else if files && path.is_file() {
            *seen += 1;
            out.push(path);
        }
    }
}

fn simple_glob_match(pattern: &str, text: &str) -> bool {
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

fn display_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn collect_tree(
    root: &Path,
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
        let suffix = if path.is_dir() { "/" } else { "" };
        out.push(format!("{}{}", display_path(&path, cwd), suffix));
        if path.is_dir() && !skip_dir(&path) {
            collect_tree(&path, cwd, depth + 1, max_depth, out, max_entries);
        }
    }
}

fn todo_store() -> &'static Mutex<Vec<(String, String)>> {
    static TODO: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();
    TODO.get_or_init(|| Mutex::new(Vec::new()))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn docx_paragraph(text: &str, style: Option<&str>) -> String {
    let style = style
        .map(|s| format!("<w:pPr><w:pStyle w:val=\"{s}\"/></w:pPr>"))
        .unwrap_or_default();
    format!(
        "<w:p>{style}<w:r><w:t>{}</w:t></w:r></w:p>",
        xml_escape(text)
    )
}

fn docx_document(title: &str, body: &str) -> String {
    let mut out = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
    );
    out.push_str(&docx_paragraph(title, Some("Title")));
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            out.push_str("<w:p/>");
        } else if let Some(h) = trimmed.strip_prefix("### ") {
            out.push_str(&docx_paragraph(h, Some("Heading3")));
        } else if let Some(h) = trimmed.strip_prefix("## ") {
            out.push_str(&docx_paragraph(h, Some("Heading2")));
        } else if let Some(h) = trimmed.strip_prefix("# ") {
            out.push_str(&docx_paragraph(h, Some("Heading1")));
        } else if let Some(item) = trimmed.strip_prefix("- ") {
            out.push_str(&docx_paragraph(&format!("• {item}"), None));
        } else {
            out.push_str(&docx_paragraph(trimmed, None));
        }
    }
    out.push_str(r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440"/></w:sectPr></w:body></w:document>"#);
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn write_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn write_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn stored_zip(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in files {
        let offset = out.len() as u32;
        let crc = crc32(data);
        write_u32(&mut out, 0x0403_4b50);
        write_u16(&mut out, 20);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u32(&mut out, crc);
        write_u32(&mut out, data.len() as u32);
        write_u32(&mut out, data.len() as u32);
        write_u16(&mut out, name.len() as u16);
        write_u16(&mut out, 0);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        write_u32(&mut central, 0x0201_4b50);
        write_u16(&mut central, 20);
        write_u16(&mut central, 20);
        write_u16(&mut central, 0);
        write_u16(&mut central, 0);
        write_u16(&mut central, 0);
        write_u16(&mut central, 0);
        write_u32(&mut central, crc);
        write_u32(&mut central, data.len() as u32);
        write_u32(&mut central, data.len() as u32);
        write_u16(&mut central, name.len() as u16);
        write_u16(&mut central, 0);
        write_u16(&mut central, 0);
        write_u16(&mut central, 0);
        write_u16(&mut central, 0);
        write_u32(&mut central, 0);
        write_u32(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = out.len() as u32;
    out.extend_from_slice(&central);
    write_u32(&mut out, 0x0605_4b50);
    write_u16(&mut out, 0);
    write_u16(&mut out, 0);
    write_u16(&mut out, files.len() as u16);
    write_u16(&mut out, files.len() as u16);
    write_u32(&mut out, central.len() as u32);
    write_u32(&mut out, central_offset);
    write_u16(&mut out, 0);
    out
}

fn docx_bytes(title: &str, body: &str) -> Vec<u8> {
    let content_types = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/></Types>"#.to_vec();
    let rels = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#.to_vec();
    let styles = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/></w:style><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/></w:style><w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/></w:style><w:style w:type="paragraph" w:styleId="Heading3"><w:name w:val="heading 3"/></w:style></w:styles>"#.to_vec();
    stored_zip(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", rels),
        ("word/document.xml", docx_document(title, body).into_bytes()),
        ("word/styles.xml", styles),
    ])
}

fn search_limit(input: &Value) -> usize {
    input["max"]
        .as_u64()
        .map(|n| n.clamp(1, 500) as usize)
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
}

pub fn run(name: &str, input: &Value, cwd: &Path) -> Outcome {
    match name {
        "read_file" => {
            let p = resolve(cwd, input["path"].as_str().unwrap_or(""));
            match fs::read_to_string(&p) {
                Ok(c) => ok(truncate(apply_line_range(&c, line_range(input)), MAX_READ)),
                Err(e) => err(format!(
                    "cannot read {}: {e}\nrecovery: do not invent another path or ask the user immediately. Use list_tree/find_paths/find_files/grep_files to locate likely files; for folders use find_paths kind=`dir`; for personal files try roots like `~`, `~/Documents`, `~/Desktop`, `~/Downloads`, `~/Projects`, and `~/repos`. If a broader search is needed, propose or call a read-only find/rg command.",
                    p.display()
                )),
            }
        }
        "read_many_files" => {
            let Some(paths) = input["paths"].as_array() else {
                return err("paths is required");
            };
            let max = input["max_bytes_per_file"]
                .as_u64()
                .unwrap_or(MAX_READ as u64)
                .clamp(1_000, MAX_READ as u64) as usize;
            let mut sections = Vec::new();
            for path in paths.iter().filter_map(|v| v.as_str()) {
                let p = resolve(cwd, path);
                match fs::read_to_string(&p) {
                    Ok(c) => sections.push(format!(
                        "--- {} ---\n{}",
                        display_path(&p, cwd),
                        truncate(c, max)
                    )),
                    Err(e) => sections.push(format!("--- {} ---\n[error] {e}", p.display())),
                }
            }
            ok(truncate(sections.join("\n\n"), MAX_READ))
        }
        "list_dir" => {
            let p = resolve(cwd, input["path"].as_str().unwrap_or("."));
            match fs::read_dir(&p) {
                Ok(rd) => {
                    let mut names: Vec<String> = rd
                        .filter_map(|e| e.ok())
                        .map(|e| {
                            let n = e.file_name().to_string_lossy().into_owned();
                            if e.path().is_dir() {
                                format!("{n}/")
                            } else {
                                n
                            }
                        })
                        .collect();
                    names.sort();
                    ok(names.join("\n"))
                }
                Err(e) => err(format!("cannot list {}: {e}", p.display())),
            }
        }
        "list_tree" => {
            let root = resolve(cwd, input["path"].as_str().unwrap_or("."));
            let max_depth = input["max_depth"].as_u64().unwrap_or(3).clamp(1, 8) as usize;
            let max_entries = input["max_entries"].as_u64().unwrap_or(200).clamp(1, 1000) as usize;
            let mut entries = Vec::new();
            collect_tree(&root, cwd, 1, max_depth, &mut entries, max_entries);
            if entries.is_empty() {
                ok(format!("no entries under {}", root.display()))
            } else {
                ok(entries.join("\n"))
            }
        }
        "file_info" => {
            let p = resolve(cwd, input["path"].as_str().unwrap_or(""));
            match fs::metadata(&p) {
                Ok(m) => {
                    let kind = if m.is_dir() {
                        "directory"
                    } else if m.is_file() {
                        "file"
                    } else {
                        "other"
                    };
                    let modified_ms = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis());
                    ok(json!({
                        "path": display_path(&p, cwd),
                        "kind": kind,
                        "len": m.len(),
                        "readonly": m.permissions().readonly(),
                        "modified_ms": modified_ms,
                    })
                    .to_string())
                }
                Err(e) => err(format!("cannot stat {}: {e}", p.display())),
            }
        }
        "find_paths" => {
            let root = resolve(cwd, input["root"].as_str().unwrap_or("."));
            let pattern = input["pattern"].as_str().unwrap_or("").trim();
            if pattern.is_empty() {
                return err("pattern is required");
            }
            let kind = input["kind"].as_str().unwrap_or("any");
            let include_dirs = kind != "file";
            let include_files = kind != "dir";
            let limit = search_limit(input);
            let mut paths = Vec::new();
            let mut seen = 0;
            collect_paths(&root, &mut paths, &mut seen, include_dirs, include_files);
            paths.sort();
            let matches: Vec<String> = paths
                .into_iter()
                .filter_map(|path| {
                    let rel = display_path(&path, cwd);
                    let name_match = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|name| simple_glob_match(pattern, name));
                    if simple_glob_match(pattern, &rel) || name_match {
                        Some(rel)
                    } else {
                        None
                    }
                })
                .take(limit)
                .collect();
            if matches.is_empty() {
                ok(format!("no paths matched {pattern}"))
            } else {
                ok(matches.join("\n"))
            }
        }
        "find_files" => {
            let root = resolve(cwd, input["root"].as_str().unwrap_or("."));
            let pattern = input["pattern"].as_str().unwrap_or("").trim();
            if pattern.is_empty() {
                return err("pattern is required");
            }
            let limit = search_limit(input);
            let mut files = Vec::new();
            let mut seen = 0;
            collect_files(&root, &mut files, &mut seen);
            files.sort();
            let matches: Vec<String> = files
                .into_iter()
                .filter_map(|path| {
                    let rel = display_path(&path, cwd);
                    let name_match = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|name| simple_glob_match(pattern, name));
                    if simple_glob_match(pattern, &rel) || name_match {
                        Some(rel)
                    } else {
                        None
                    }
                })
                .take(limit)
                .collect();
            if matches.is_empty() {
                ok(format!("no files matched {pattern}"))
            } else {
                ok(matches.join("\n"))
            }
        }
        "grep_files" => {
            let root = resolve(cwd, input["root"].as_str().unwrap_or("."));
            let pattern = input["pattern"].as_str().unwrap_or("");
            if pattern.is_empty() {
                return err("pattern is required");
            }
            let file_pattern = input["file_pattern"].as_str().unwrap_or("*");
            let case_sensitive = input["case_sensitive"].as_bool().unwrap_or(false);
            let needle = if case_sensitive {
                pattern.to_string()
            } else {
                pattern.to_lowercase()
            };
            let limit = search_limit(input);
            let mut files = Vec::new();
            let mut seen = 0;
            collect_files(&root, &mut files, &mut seen);
            files.sort();
            let mut matches = Vec::new();
            for path in files {
                if matches.len() >= limit {
                    break;
                }
                let rel = display_path(&path, cwd);
                let basename_matches = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| simple_glob_match(file_pattern, name));
                if !simple_glob_match(file_pattern, &rel) && !basename_matches {
                    continue;
                }
                let Ok(text) = fs::read_to_string(&path) else {
                    continue;
                };
                for (idx, line) in text.lines().enumerate() {
                    let haystack = if case_sensitive {
                        line.to_string()
                    } else {
                        line.to_lowercase()
                    };
                    if haystack.contains(&needle) {
                        matches.push(format!("{}:{}:{}", rel, idx + 1, line.trim_end()));
                        if matches.len() >= limit {
                            break;
                        }
                    }
                }
            }
            if matches.is_empty() {
                ok(format!("no matches for {pattern}"))
            } else {
                ok(matches.join("\n"))
            }
        }
        "write_file" => {
            let p = resolve(cwd, input["path"].as_str().unwrap_or(""));
            let content = input["content"].as_str().unwrap_or("");
            if let Some(dir) = p.parent() {
                let _ = fs::create_dir_all(dir);
            }
            checkpoint::record(cwd, &p, "write_file");
            match fs::write(&p, content) {
                Ok(_) => ok(format!("wrote {} ({} bytes)", p.display(), content.len())),
                Err(e) => err(format!("cannot write {}: {e}", p.display())),
            }
        }
        "edit_file" => {
            let p = resolve(cwd, input["path"].as_str().unwrap_or(""));
            let old = input["old"].as_str().unwrap_or("");
            let new = input["new"].as_str().unwrap_or("");
            let body = match fs::read_to_string(&p) {
                Ok(b) => b,
                Err(e) => return err(format!("cannot read {}: {e}", p.display())),
            };
            if old.is_empty() {
                return err("`old` text not found in file");
            }
            // One pass to locate, a partial pass to confirm uniqueness — instead
            // of matches().count() (materializes all) plus a separate replace.
            let Some(first) = body.find(old) else {
                return err("`old` text not found in file");
            };
            if body[first + old.len()..].contains(old) {
                return err("`old` text is not unique — add surrounding context");
            }
            checkpoint::record(cwd, &p, "edit_file");
            match fs::write(&p, body.replacen(old, new, 1)) {
                Ok(_) => ok(format!("edited {}", p.display())),
                Err(e) => err(format!("cannot write {}: {e}", p.display())),
            }
        }
        "multi_edit" => {
            let p = resolve(cwd, input["path"].as_str().unwrap_or(""));
            let Some(edits) = input["edits"].as_array() else {
                return err("edits is required");
            };
            let mut body = match fs::read_to_string(&p) {
                Ok(b) => b,
                Err(e) => return err(format!("cannot read {}: {e}", p.display())),
            };
            for edit in edits {
                let old = edit["old"].as_str().unwrap_or("");
                if old.is_empty() {
                    return err("old text cannot be empty");
                }
                let count = body.matches(old).count();
                if count == 0 {
                    return err("old text not found");
                }
                if count > 1 {
                    return err("old text is not unique — add surrounding context");
                }
                let new = edit["new"].as_str().unwrap_or("");
                body = body.replacen(old, new, 1);
            }
            checkpoint::record(cwd, &p, "multi_edit");
            match fs::write(&p, body) {
                Ok(_) => ok(format!(
                    "edited {} with {} replacements",
                    p.display(),
                    edits.len()
                )),
                Err(e) => err(format!("cannot write {}: {e}", p.display())),
            }
        }
        "apply_patch" => {
            let patch = input["patch"].as_str().unwrap_or("");
            if patch.trim().is_empty() {
                return err("patch is required");
            }
            let mut child = match Command::new("git")
                .args(["apply", "--whitespace=nowarn", "-"])
                .current_dir(cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => return err(format!("failed to spawn git apply: {e}")),
            };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = std::io::Write::write_all(&mut stdin, patch.as_bytes());
            }
            match child.wait_with_output() {
                Ok(o) if o.status.success() => ok("patch applied"),
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    err(format!("patch failed: {stderr}"))
                }
                Err(e) => err(format!("failed to wait for git apply: {e}")),
            }
        }
        "create_dir" => {
            let p = resolve(cwd, input["path"].as_str().unwrap_or(""));
            match fs::create_dir_all(&p) {
                Ok(_) => ok(format!("created {}", p.display())),
                Err(e) => err(format!("cannot create {}: {e}", p.display())),
            }
        }
        "move_path" => {
            let from = resolve(cwd, input["from"].as_str().unwrap_or(""));
            let to = resolve(cwd, input["to"].as_str().unwrap_or(""));
            if let Some(parent) = to.parent() {
                let _ = fs::create_dir_all(parent);
            }
            checkpoint::record(cwd, &from, "move_path");
            match fs::rename(&from, &to) {
                Ok(_) => ok(format!("moved {} -> {}", from.display(), to.display())),
                Err(e) => err(format!(
                    "cannot move {} -> {}: {e}",
                    from.display(),
                    to.display()
                )),
            }
        }
        "remove_path" => {
            let p = resolve(cwd, input["path"].as_str().unwrap_or(""));
            checkpoint::record(cwd, &p, "remove_path");
            let res = if p.is_dir() {
                fs::remove_dir(&p)
            } else {
                fs::remove_file(&p)
            };
            match res {
                Ok(_) => ok(format!("removed {}", p.display())),
                Err(e) => err(format!("cannot remove {}: {e}", p.display())),
            }
        }
        "run_command" => {
            let cmd = input["command"].as_str().unwrap_or("");
            let output = if cfg!(windows) {
                Command::new("cmd")
                    .args(["/C", cmd])
                    .current_dir(cwd)
                    .output()
            } else {
                Command::new("sh")
                    .args(["-c", cmd])
                    .current_dir(cwd)
                    .output()
            };
            match output {
                Ok(o) => {
                    let mut s = String::new();
                    s.push_str(&String::from_utf8_lossy(&o.stdout));
                    let e = String::from_utf8_lossy(&o.stderr);
                    if !e.trim().is_empty() {
                        s.push_str("\n[stderr]\n");
                        s.push_str(&e);
                    }
                    let code = o.status.code().unwrap_or(-1);
                    s.push_str(&format!("\n[exit {code}]"));
                    Outcome {
                        content: truncate(s, MAX_OUT),
                        is_error: !o.status.success(),
                        finished: false,
                    }
                }
                Err(e) => err(format!("failed to spawn: {e}")),
            }
        }
        "todo_write" => {
            let Some(items) = input["items"].as_array() else {
                return err("items is required");
            };
            let mut next = Vec::new();
            for item in items {
                let task = item["task"].as_str().unwrap_or("").trim();
                let status = item["status"].as_str().unwrap_or("pending");
                if !task.is_empty() {
                    next.push((task.to_string(), status.to_string()));
                }
            }
            match todo_store().lock() {
                Ok(mut todos) => {
                    *todos = next;
                    ok(format!("stored {} todo item(s)", todos.len()))
                }
                Err(_) => err("todo store unavailable"),
            }
        }
        "todo_read" => match todo_store().lock() {
            Ok(todos) if todos.is_empty() => ok("no todo items"),
            Ok(todos) => ok(todos
                .iter()
                .enumerate()
                .map(|(i, (task, status))| format!("{}. [{status}] {task}", i + 1))
                .collect::<Vec<_>>()
                .join("\n")),
            Err(_) => err("todo store unavailable"),
        },
        "create_docx" => {
            let p = resolve(cwd, input["path"].as_str().unwrap_or(""));
            let title = input["title"].as_str().unwrap_or("Document");
            let body = input["body"].as_str().unwrap_or("");
            if let Some(dir) = p.parent() {
                let _ = fs::create_dir_all(dir);
            }
            checkpoint::record(cwd, &p, "create_docx");
            let bytes = docx_bytes(title, body);
            match fs::write(&p, &bytes) {
                Ok(_) => ok(format!("created {} ({} bytes)", p.display(), bytes.len())),
                Err(e) => err(format!("cannot write {}: {e}", p.display())),
            }
        }
        "fetch_url" => {
            let url = input["url"].as_str().unwrap_or("");
            if url.is_empty() {
                return err("url is required");
            }
            match ureq::get(url).call() {
                Ok(resp) => match resp.into_string() {
                    Ok(body) => ok(truncate(body, MAX_OUT)),
                    Err(e) => err(format!("failed to read response body: {e}")),
                },
                Err(e) => err(format!("fetch failed: {e}")),
            }
        }
        "web_search" => {
            let query = input["query"].as_str().unwrap_or("").trim();
            if query.is_empty() {
                return err("query is required");
            }
            let encoded = url_encode(query);
            let search_url = format!("https://html.duckduckgo.com/html/?q={encoded}");
            match ureq::get(&search_url)
                .set("User-Agent", "Mozilla/5.0 (compatible; buildwithnexus/1.0)")
                .call()
            {
                Ok(resp) => match resp.into_string() {
                    Ok(html) => ok(truncate(strip_html(&html), MAX_OUT)),
                    Err(e) => err(format!("search response error: {e}")),
                },
                Err(e) => err(format!("web search failed: {e}")),
            }
        }
        "list_skills" => {
            let rows = crate::config::load_skills()
                .into_iter()
                .map(|(name, content)| {
                    let first = content.lines().next().unwrap_or("").trim().to_string();
                    format!("{name}: {first}")
                })
                .collect::<Vec<_>>();
            if rows.is_empty() {
                ok("no skills available")
            } else {
                ok(rows.join("\n"))
            }
        }
        "load_skill" => {
            let wanted = input["name"]
                .as_str()
                .unwrap_or("")
                .trim()
                .trim_start_matches('/');
            if wanted.is_empty() {
                return err("name is required");
            }
            for (name, content) in crate::config::load_skills() {
                if name == wanted {
                    crate::trace::record_visible(
                        "skill",
                        format!("loaded {name}"),
                        json!({"name": name, "bytes": content.len(), "preview": crate::trace::preview(&content, 600)}),
                    );
                    return ok(format!("# Skill: {name}\n{content}"));
                }
            }
            err(format!("skill not found: {wanted}"))
        }
        "finish" => Outcome {
            content: input["summary"].as_str().unwrap_or("done").to_string(),
            is_error: false,
            finished: true,
        },
        other => err(format!("unknown tool: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ── truncate ────────────────────────────────────────────────────────────
    #[test]
    fn truncate_shorter_than_max_unchanged() {
        assert_eq!(truncate("hello".into(), 100), "hello");
    }

    #[test]
    fn truncate_appends_marker() {
        let out = truncate("abcdefghij".into(), 5);
        assert!(out.starts_with("abcde"));
        assert!(out.contains("[truncated]"));
    }

    #[test]
    fn truncate_backs_off_to_char_boundary() {
        // "é" is two bytes; cutting at an odd byte must not panic and must yield
        // valid UTF-8.
        let s = "a".to_string() + &"é".repeat(50); // 1 + 100 bytes
        let out = truncate(s, 4); // 4 lands mid-glyph
        assert!(out.is_char_boundary(out.find('\n').unwrap_or(out.len())));
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn truncate_emoji_boundary() {
        let s = "🎉".repeat(10); // 4 bytes each
        let out = truncate(s, 5); // mid-emoji
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn truncate_exactly_max_unchanged() {
        assert_eq!(truncate("abcde".into(), 5), "abcde");
    }

    // ── is_sensitive ────────────────────────────────────────────────────────
    #[test]
    fn sensitive_paths() {
        for p in [
            "/home/u/.ssh/id_rsa",
            "/home/u/.aws/credentials",
            "/home/u/.gnupg/secring.gpg",
            "/home/u/.buildwithnexus/.env.keys",
            "/proj/.env",
            "/proj/.env.local",
            "/proj/server.pem",
            "/home/u/id_ed25519",
        ] {
            assert!(is_sensitive(Path::new(p)), "{p} should be sensitive");
        }
    }

    #[test]
    fn non_sensitive_paths() {
        for p in [
            "/proj/src/main.rs",
            "/proj/README.md",
            "/proj/environment.txt",
        ] {
            assert!(!is_sensitive(Path::new(p)), "{p} should not be sensitive");
        }
    }

    #[test]
    fn sensitive_is_case_insensitive() {
        assert!(is_sensitive(Path::new("/home/U/.SSH/known_hosts")));
    }

    // ── escapes_cwd ─────────────────────────────────────────────────────────
    #[test]
    fn escapes_cwd_detects_parent_traversal() {
        let cwd = PathBuf::from("/proj/work");
        assert!(escapes_cwd(Path::new("/proj/work/../../etc/passwd"), &cwd));
        assert!(escapes_cwd(Path::new("/etc/passwd"), &cwd));
    }

    #[test]
    fn escapes_cwd_allows_inside() {
        let cwd = PathBuf::from("/proj/work");
        // Use a non-existent dir so canonicalize falls back to lexical normalize.
        assert!(!escapes_cwd(Path::new("/proj/work/src/a.rs"), &cwd));
        assert!(!escapes_cwd(Path::new("/proj/work/a/../b.rs"), &cwd));
    }

    // ── catastrophic ────────────────────────────────────────────────────────
    #[test]
    fn catastrophic_commands() {
        for c in [
            "rm -rf /",
            "rm   -rf   /",
            "rm -fr /home",
            ":(){ :|:& };:",
            "mkfs.ext4 /dev/sda1",
            "dd if=/dev/zero of=/dev/sda",
            "echo x > /dev/sda",
            "chmod -R 777 /",
        ] {
            assert!(catastrophic(c), "{c} should be catastrophic");
        }
    }

    #[test]
    fn non_catastrophic_commands() {
        for c in [
            "ls -la",
            "rm -rf ./build",
            "cargo test",
            "git status",
            "rm file.txt",
        ] {
            assert!(!catastrophic(c), "{c} should be allowed");
        }
    }

    // ── normalize ───────────────────────────────────────────────────────────
    #[test]
    fn normalize_folds_dot_segments() {
        assert_eq!(normalize(Path::new("a/./b/../c")), PathBuf::from("a/c"));
        assert_eq!(normalize(Path::new("./x")), PathBuf::from("x"));
    }

    // ── is_mutating / touched_path / preview ────────────────────────────────
    #[test]
    fn mutating_classification() {
        assert!(is_mutating("write_file"));
        assert!(is_mutating("edit_file"));
        assert!(is_mutating("run_command"));
        assert!(!is_mutating("read_file"));
        assert!(!is_mutating("list_dir"));
        assert!(!is_mutating("finish"));
    }

    #[test]
    fn touched_path_only_for_file_tools() {
        let cwd = Path::new("/proj");
        assert!(touched_path("read_file", &json!({"path": "a.rs"}), cwd).is_some());
        assert!(touched_path("run_command", &json!({"command": "ls"}), cwd).is_none());
        assert!(touched_path("finish", &json!({}), cwd).is_none());
    }

    #[test]
    fn touched_path_resolves_relative_to_cwd() {
        let cwd = Path::new("/proj");
        assert_eq!(
            touched_path("read_file", &json!({"path": "src/a.rs"}), cwd),
            Some(PathBuf::from("/proj/src/a.rs"))
        );
    }

    #[test]
    fn preview_strings() {
        assert_eq!(preview("write_file", &json!({"path": "a"})), "write a");
        assert_eq!(preview("run_command", &json!({"command": "ls"})), "run: ls");
        assert_eq!(preview("read_file", &json!({})), "read_file");
    }

    #[test]
    fn defs_includes_subagent_only_when_requested() {
        assert!(!defs(false).iter().any(|d| d.name == "spawn_subagent"));
        assert!(defs(true).iter().any(|d| d.name == "spawn_subagent"));
    }

    // ── run: filesystem tools against a tempdir ─────────────────────────────
    fn tempdir() -> PathBuf {
        // Unique-enough path without external deps or Date/random (which the
        // harness forbids): use the test thread name + an atomic counter.
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let tn = std::thread::current()
            .name()
            .unwrap_or("t")
            .replace("::", "_");
        let dir = std::env::temp_dir().join(format!("bwn-test-{tn}-{id}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn run_write_then_read_roundtrip() {
        let d = tempdir();
        let w = run("write_file", &json!({"path": "f.txt", "content": "hi"}), &d);
        assert!(!w.is_error);
        let r = run("read_file", &json!({"path": "f.txt"}), &d);
        assert_eq!(r.content, "hi");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_write_creates_parent_dirs() {
        let d = tempdir();
        let w = run(
            "write_file",
            &json!({"path": "a/b/c.txt", "content": "x"}),
            &d,
        );
        assert!(!w.is_error);
        assert!(d.join("a/b/c.txt").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_read_missing_file_errors() {
        let d = tempdir();
        let r = run("read_file", &json!({"path": "nope.txt"}), &d);
        assert!(r.is_error);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_read_file_line_range() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.txt", "content": "one\ntwo\nthree\nfour"}),
            &d,
        );
        let r = run(
            "read_file",
            &json!({"path": "f.txt", "start_line": 2, "end_line": 3}),
            &d,
        );
        assert_eq!(r.content, "two\nthree");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_find_files_matches_glob_and_skips_heavy_dirs() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "src/main.rs", "content": ""}),
            &d,
        );
        run(
            "write_file",
            &json!({"path": "target/debug/skip.rs", "content": ""}),
            &d,
        );
        let r = run("find_files", &json!({"root": ".", "pattern": "*.rs"}), &d);
        assert!(r.content.contains("src/main.rs"), "{}", r.content);
        assert!(!r.content.contains("target/debug/skip.rs"), "{}", r.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_find_files_matches_vague_case_insensitive_names() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "notes/MyProjectsFile.MD", "content": "alpha"}),
            &d,
        );
        let r = run(
            "find_files",
            &json!({"root": ".", "pattern": "projects"}),
            &d,
        );
        assert!(
            r.content.contains("notes/MyProjectsFile.MD"),
            "{}",
            r.content
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_find_paths_can_find_directories() {
        let d = tempdir();
        run("create_dir", &json!({"path": "Projects/Nexus"}), &d);
        run(
            "write_file",
            &json!({"path": "Projects/Nexus/README.md", "content": "alpha"}),
            &d,
        );
        let r = run(
            "find_paths",
            &json!({"root": ".", "pattern": "nexus", "kind": "dir"}),
            &d,
        );
        assert!(r.content.contains("Projects/Nexus"), "{}", r.content);
        assert!(!r.content.contains("README.md"), "{}", r.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn resolve_expands_home_shorthand() {
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(resolve(Path::new("/tmp/work"), "~"), PathBuf::from(&home));
            assert_eq!(
                resolve(Path::new("/tmp/work"), "~/Documents"),
                PathBuf::from(home).join("Documents")
            );
        }
    }

    #[test]
    fn run_grep_files_returns_literal_path_line_matches() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "src/lib.rs", "content": "alpha\nNeedle\nbeta"}),
            &d,
        );
        run(
            "write_file",
            &json!({"path": "README.md", "content": "Needle"}),
            &d,
        );
        let r = run(
            "grep_files",
            &json!({"root": ".", "pattern": "needle", "file_pattern": "*.rs"}),
            &d,
        );
        assert!(r.content.contains("src/lib.rs:2:Needle"), "{}", r.content);
        assert!(!r.content.contains("README.md"), "{}", r.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_edit_unique_match() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.txt", "content": "alpha beta gamma"}),
            &d,
        );
        let e = run(
            "edit_file",
            &json!({"path": "f.txt", "old": "beta", "new": "BETA"}),
            &d,
        );
        assert!(!e.is_error);
        let r = run("read_file", &json!({"path": "f.txt"}), &d);
        assert_eq!(r.content, "alpha BETA gamma");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_edit_non_unique_match_errors() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.txt", "content": "x x"}),
            &d,
        );
        let e = run(
            "edit_file",
            &json!({"path": "f.txt", "old": "x", "new": "y"}),
            &d,
        );
        assert!(e.is_error);
        assert!(e.content.contains("not unique"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_edit_not_found_errors() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.txt", "content": "abc"}),
            &d,
        );
        let e = run(
            "edit_file",
            &json!({"path": "f.txt", "old": "zzz", "new": "y"}),
            &d,
        );
        assert!(e.is_error);
        assert!(e.content.contains("not found"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_edit_empty_old_errors() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.txt", "content": "abc"}),
            &d,
        );
        let e = run(
            "edit_file",
            &json!({"path": "f.txt", "old": "", "new": "y"}),
            &d,
        );
        assert!(e.is_error);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_list_dir_sorted() {
        let d = tempdir();
        run("write_file", &json!({"path": "b.txt", "content": ""}), &d);
        run("write_file", &json!({"path": "a.txt", "content": ""}), &d);
        let r = run("list_dir", &json!({"path": "."}), &d);
        let lines: Vec<&str> = r.content.lines().collect();
        assert_eq!(lines, vec!["a.txt", "b.txt"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_command_captures_output_and_exit() {
        let d = tempdir();
        let r = run("run_command", &json!({"command": "echo hello"}), &d);
        assert!(!r.is_error);
        assert!(r.content.contains("hello"));
        assert!(r.content.contains("[exit 0]"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_command_nonzero_is_error() {
        let d = tempdir();
        let r = run("run_command", &json!({"command": "exit 3"}), &d);
        assert!(r.is_error);
        assert!(r.content.contains("[exit 3]"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_finish_sets_finished_flag() {
        let d = tempdir();
        let r = run("finish", &json!({"summary": "all done"}), &d);
        assert!(r.finished);
        assert_eq!(r.content, "all done");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_unknown_tool_errors() {
        let d = tempdir();
        let r = run("bogus", &json!({}), &d);
        assert!(r.is_error);
        let _ = fs::remove_dir_all(&d);
    }
}
