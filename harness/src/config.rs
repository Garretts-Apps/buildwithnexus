// Provider presets, persisted settings, API-key store, memory, and skills.
// Everything here is flat data + direct file IO.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub enum Protocol {
    Anthropic,
    OpenAi,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Protocol::Anthropic => write!(f, "Anthropic"),
            Protocol::OpenAi => write!(f, "OpenAI"),
        }
    }
}

pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    pub protocol: Protocol,
    pub base_url: &'static str,
    pub env_key: &'static str,
    pub default_model: &'static str,
    pub local: bool,
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "anthropic",
        label: "Anthropic (Claude)",
        protocol: Protocol::Anthropic,
        base_url: "https://api.anthropic.com",
        env_key: "ANTHROPIC_API_KEY",
        default_model: "claude-sonnet-4-6",
        local: false,
    },
    Preset {
        id: "openai",
        label: "OpenAI",
        protocol: Protocol::OpenAi,
        base_url: "https://api.openai.com/v1",
        env_key: "OPENAI_API_KEY",
        default_model: "gpt-4o",
        local: false,
    },
    Preset {
        id: "openrouter",
        label: "OpenRouter",
        protocol: Protocol::OpenAi,
        base_url: "https://openrouter.ai/api/v1",
        env_key: "OPENROUTER_API_KEY",
        default_model: "anthropic/claude-3.7-sonnet",
        local: false,
    },
    Preset {
        id: "groq",
        label: "Groq",
        protocol: Protocol::OpenAi,
        base_url: "https://api.groq.com/openai/v1",
        env_key: "GROQ_API_KEY",
        default_model: "llama-3.3-70b-versatile",
        local: false,
    },
    Preset {
        id: "huggingface",
        label: "Hugging Face",
        protocol: Protocol::OpenAi,
        base_url: "https://router.huggingface.co/v1",
        env_key: "HF_TOKEN",
        default_model: "meta-llama/Llama-3.3-70B-Instruct",
        local: false,
    },
    Preset {
        id: "ollama",
        label: "Ollama (local)",
        protocol: Protocol::OpenAi,
        base_url: "http://localhost:11434/v1",
        env_key: "",
        default_model: "llama3.2",
        local: true,
    },
    Preset {
        id: "llamacpp",
        label: "llama.cpp server (local)",
        protocol: Protocol::OpenAi,
        base_url: "http://localhost:8080/v1",
        env_key: "",
        default_model: "local-model",
        local: true,
    },
    Preset {
        id: "lmstudio",
        label: "LM Studio (local)",
        protocol: Protocol::OpenAi,
        base_url: "http://localhost:1234/v1",
        env_key: "",
        default_model: "local-model",
        local: true,
    },
];

pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

#[derive(Serialize, Deserialize)]
pub struct Settings {
    pub provider: String,
    pub model: String,
    pub permission: String,
    #[serde(default = "default_effort")]
    pub effort: String,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Shell binaries that auto-approve in Ask mode. Empty = use built-in defaults.
    #[serde(default)]
    pub allowed_commands: Vec<String>,
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub plugins: BTreeMap<String, serde_json::Value>,
}

fn default_effort() -> String {
    "low".into()
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            provider: "anthropic".into(),
            model: String::new(),
            permission: "ask".into(),
            effort: "low".into(),
            base_url: None,
            allowed_commands: Vec::new(),
            mcp_servers: BTreeMap::new(),
            plugins: BTreeMap::new(),
        }
    }
}

fn default_allowed_commands() -> Vec<String> {
    [
        "git", "cargo", "npm", "npx", "node", "python3", "pip3", "make", "cmake", "bun", "deno",
        "rg", "grep", "find", "ls", "cat", "curl", "wget", "jq", "gh", "rustc", "go", "ruby",
        "java", "yarn", "pnpm", "docker", "kubectl", "sed", "awk", "sort", "uniq", "wc", "head",
        "tail", "diff", "patch", "tar", "zip", "unzip", "rsync", "mvn", "gradle",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Returns the user's allowed-command list, falling back to built-in defaults.
pub fn load_allowed_commands() -> Vec<String> {
    match load_settings() {
        Some(s) if !s.allowed_commands.is_empty() => s.allowed_commands,
        _ => default_allowed_commands(),
    }
}

pub fn home() -> PathBuf {
    if let Ok(h) = std::env::var("NEXUS_HOME") {
        return PathBuf::from(h);
    }
    let base = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join(".buildwithnexus")
}

fn settings_path() -> PathBuf {
    home().join("settings.json")
}
fn keys_path() -> PathBuf {
    home().join(".env.keys")
}
pub fn history_path() -> PathBuf {
    home().join("history")
}
pub fn memory_path() -> PathBuf {
    home().join("memory.md")
}
fn agents_path() -> PathBuf {
    home().join("Agents.md")
}
fn skills_dir() -> PathBuf {
    home().join("skills")
}
fn commands_dir() -> PathBuf {
    home().join("commands")
}
fn hooks_dir() -> PathBuf {
    home().join("hooks")
}

pub fn load_history() -> Vec<String> {
    fs::read_to_string(history_path())
        .map(|t| {
            t.lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub fn save_history(entries: &[String]) {
    const MAX: usize = 1000;
    ensure_home();
    let tail = if entries.len() > MAX {
        &entries[entries.len() - MAX..]
    } else {
        entries
    };
    let body: String = tail
        .iter()
        .map(|e| format!("{}\n", e.replace('\n', " ")))
        .collect();
    let p = history_path();
    if fs::write(&p, body).is_ok() {
        restrict(&p);
    }
}

// ── memory ────────────────────────────────────────────────────────────────────
// memory.md persists facts the model saves across sessions. On startup it's
// injected into the system context so the model "remembers" previous sessions.

pub fn load_memory() -> Option<String> {
    let text = fs::read_to_string(memory_path()).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn save_memory(content: &str) {
    ensure_home();
    let p = memory_path();
    let _ = fs::write(&p, content);
}

pub fn append_memory(entry: &str) {
    ensure_home();
    let existing = load_memory().unwrap_or_default();
    let new = if existing.is_empty() {
        format!("- {entry}\n")
    } else {
        format!("{existing}\n- {entry}\n")
    };
    save_memory(&new);
}

// ── agents + skills ───────────────────────────────────────────────────────────
// Agents.md defines roles/capabilities the model can invoke. Skills are
// individual markdown files that describe custom behaviors.

pub fn load_agents() -> Option<String> {
    // Project-local Agents.md takes precedence over the home one.
    let cwd = std::env::current_dir().ok()?;
    let proj = cwd.join(".buildwithnexus").join("Agents.md");
    if let Ok(t) = fs::read_to_string(&proj) {
        if !t.trim().is_empty() {
            return Some(t.trim().to_string());
        }
    }
    let global = agents_path();
    fs::read_to_string(&global)
        .ok()
        .filter(|t| !t.trim().is_empty())
        .map(|t| t.trim().to_string())
}

// Returns (name, content) pairs for all skill files.
pub fn load_skills() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (name, content) in bundled_skills() {
        seen.insert(name.to_string());
        out.push((name.to_string(), content.trim().to_string()));
    }
    for dir in [
        skills_dir(),
        std::env::current_dir()
            .ok()
            .map(|d| d.join(".buildwithnexus/skills"))
            .unwrap_or_default(),
    ] {
        if let Ok(rd) = fs::read_dir(&dir) {
            for e in rd.flatten() {
                let path = e.path();
                if path.extension().is_some_and(|x| x == "md") {
                    if let Ok(content) = fs::read_to_string(&path) {
                        let name = path
                            .file_stem()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        if !name.is_empty() && !content.trim().is_empty() {
                            if seen.contains(&name) {
                                out.retain(|(existing, _)| existing != &name);
                            }
                            seen.insert(name.clone());
                            out.push((name, content.trim().to_string()));
                        }
                    }
                }
            }
        }
    }
    out
}

/// Extract a short description from a skill's markdown content.
/// Looks for the first non-heading, non-empty line (typically "Use this skill for/when...").
/// Falls back to the first heading text if no description line is found.
pub fn skill_description(content: &str) -> String {
    let mut title = String::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            if title.is_empty() {
                title = trimmed.trim_start_matches('#').trim().to_string();
            }
            continue;
        }
        // First non-heading, non-empty line is the description.
        return trimmed.to_string();
    }
    title
}

/// Returns (name, short_description) pairs for all skills.
/// Only extracts the description line — never loads the full skill body into the
/// model's context. Bad/small models benefit from this because the system prompt
/// stays small and they can selectively load_skill the ones they need.
pub fn load_skill_descriptions() -> Vec<(String, String)> {
    load_skills()
        .into_iter()
        .map(|(name, content)| {
            let desc = skill_description(&content);
            (name, desc)
        })
        .collect()
}

pub fn bundled_skills() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "self-knowledge",
            include_str!("bundled_skills/self-knowledge.md"),
        ),
        (
            "codebase-repair",
            include_str!("bundled_skills/codebase-repair.md"),
        ),
        ("rust-cli", include_str!("bundled_skills/rust-cli.md")),
        ("tool-use", include_str!("bundled_skills/tool-use.md")),
        ("git-release", include_str!("bundled_skills/git-release.md")),
        (
            "spec-writing",
            include_str!("bundled_skills/spec-writing.md"),
        ),
        (
            "document-generation",
            include_str!("bundled_skills/document-generation.md"),
        ),
        (
            "test-engineering",
            include_str!("bundled_skills/test-engineering.md"),
        ),
        ("code-review", include_str!("bundled_skills/code-review.md")),
        (
            "security-review",
            include_str!("bundled_skills/security-review.md"),
        ),
        (
            "release-notes",
            include_str!("bundled_skills/release-notes.md"),
        ),
        ("research", include_str!("bundled_skills/research.md")),
        (
            "data-analysis",
            include_str!("bundled_skills/data-analysis.md"),
        ),
        ("frontend-ux", include_str!("bundled_skills/frontend-ux.md")),
        ("static-app", include_str!("bundled_skills/static-app.md")),
    ]
}

// ── custom slash commands ─────────────────────────────────────────────────────
pub struct CustomCommand {
    pub name: String,            // without leading /
    pub content: String,         // markdown instructions injected as context
    pub script: Option<PathBuf>, // optional shell/py script to run
}

pub fn load_custom_commands() -> Vec<CustomCommand> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    if let Ok(rd) = fs::read_dir(commands_dir()) {
        for e in rd.flatten() {
            let path = e.path();
            let ext = path
                .extension()
                .map(|x| x.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let stem = path
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if stem.is_empty() || stem.starts_with('.') {
                continue;
            }
            match ext.as_str() {
                "md" => {
                    if let Ok(content) = fs::read_to_string(&path) {
                        seen.insert(stem.clone());
                        out.push(CustomCommand {
                            name: stem,
                            content: content.trim().to_string(),
                            script: None,
                        });
                    }
                }
                "sh" | "py" | "bash" => {
                    seen.insert(stem.clone());
                    out.push(CustomCommand {
                        name: stem,
                        content: String::new(),
                        script: Some(path),
                    });
                }
                _ => {}
            }
        }
    }
    for (name, content) in bundled_skills() {
        if !seen.contains(name) {
            out.push(CustomCommand {
                name: name.to_string(),
                content: content.trim().to_string(),
                script: None,
            });
        }
    }
    out
}

// ── hooks directory ───────────────────────────────────────────────────────────
// Auto-discovers scripts in ~/.buildwithnexus/hooks/<Event>/ so users can drop
// a .sh or .py file there without editing settings.json.
pub fn discover_hook_scripts(event: &str) -> Vec<PathBuf> {
    let dir = hooks_dir().join(event);
    let mut scripts = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            if let Some(ext) = p.extension().map(|x| x.to_string_lossy().to_lowercase()) {
                if matches!(
                    ext.as_str(),
                    "sh" | "bash" | "py" | "python" | "rs" | "rust"
                ) {
                    scripts.push(p);
                }
            }
        }
    }
    scripts
}

pub fn ensure_home() {
    let h = home();
    if let Err(e) = fs::create_dir_all(&h) {
        // Surface the error immediately — on WSL this often means $HOME is
        // pointing at a Windows path or the directory is read-only.
        eprintln!(
            "buildwithnexus: cannot create home directory {}: {e}",
            h.display()
        );
        eprintln!("  Tip: set NEXUS_HOME to a writable path, e.g. export NEXUS_HOME=$HOME/.buildwithnexus");
        return;
    }
    restrict(&h);
}

/// Create the full directory skeleton and starter files on first use.
/// Safe to call repeatedly — all operations are idempotent.
pub fn scaffold_home() {
    ensure_home();
    let h = home();

    // Sub-directories (created silently; errors ignored — missing dirs are
    // handled gracefully everywhere they are used).
    for sub in &[
        "skills",
        "commands",
        "checkpoints",
        "hooks/PreToolUse",
        "hooks/PostToolUse",
        "hooks/SessionStart",
        "hooks/SessionEnd",
        "hooks/UserPromptSubmit",
        "hooks/PrePrompt",
        "hooks/PostResponse",
        "hooks/OnError",
        "hooks/Stop",
    ] {
        let _ = fs::create_dir_all(h.join(sub));
    }

    // Starter Agents.md only if it doesn't exist yet.
    let agents_md = h.join("Agents.md");
    if !agents_md.exists() {
        let _ = fs::write(
            &agents_md,
            "\
# Agents

Define custom agent roles here. Each section becomes available to the model
so it can adopt specialised personas or delegate sub-tasks.

## Skill Use Policy
Before doing substantial work, inspect the available skills and deliberately use
the most relevant skill instructions. Bundled skills are callable as slash
commands, for example /self-knowledge, /tool-use, /codebase-repair,
/rust-cli, /spec-writing, /document-generation, /test-engineering,
/code-review, /security-review, /release-notes, /research, /data-analysis,
/frontend-ux, and /static-app.

When a user names a skill or uses a skill slash command, treat that skill as
active context for the task. When no skill is named, choose the closest relevant
skill yourself and follow it. Use /trace to inspect evidence of loaded skills,
tool calls, hooks, and subagents.

For browser games, canvas demos, standalone prototypes, and simple websites,
load and follow /static-app. Build an actual runnable artifact rather than
replying with code in markdown.

## Engineer
A senior full-stack engineer. Reads before writing. Prefers small, verifiable
edits. Uses the finish tool when the task is done.

## Researcher
A meticulous research engineer. Investigates the codebase with read_file and
list_dir before drawing conclusions. Cites file paths. Never modifies files
unless explicitly asked.

## Reviewer
A careful code reviewer. Looks for correctness bugs, security issues, and
unnecessary complexity. Produces a concise numbered list of findings.
",
        );
    }
}

pub fn load_settings() -> Option<Settings> {
    load_settings_from_dir(&std::env::current_dir().unwrap_or_else(|_| home()))
}

/// Loads settings from global ~/.buildwithnexus/config.json, settings.json, settings.local.json,
/// and project .buildwithnexus/settings.json, settings.local.json, merging them in hierarchy order.
pub fn load_settings_from_dir(workdir: &std::path::Path) -> Option<Settings> {
    let mut sources = Vec::new();

    // 1. Global config.json (legacy base)
    if let Ok(text) = fs::read_to_string(home().join("config.json")) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
            if val.is_object() {
                sources.push(val);
            }
        }
    }

    // 2. Global settings.json
    if let Ok(text) = fs::read_to_string(settings_path()) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
            if val.is_object() {
                sources.push(val);
            }
        }
    }

    // 3. Global settings.local.json
    if let Ok(text) = fs::read_to_string(home().join("settings.local.json")) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
            if val.is_object() {
                sources.push(val);
            }
        }
    }

    // 4. Project settings.json
    let proj_path = workdir.join(".buildwithnexus").join("settings.json");
    if let Ok(text) = fs::read_to_string(&proj_path) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
            if val.is_object() {
                sources.push(val);
            }
        }
    }

    // 5. Project settings.local.json
    let local_path = workdir.join(".buildwithnexus").join("settings.local.json");
    if let Ok(text) = fs::read_to_string(&local_path) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
            if val.is_object() {
                sources.push(val);
            }
        }
    }

    if sources.is_empty() {
        return None;
    }

    let mut merged = sources.remove(0);
    for source in sources {
        merge_json_values(&mut merged, source);
    }

    serde_json::from_value(merged).ok()
}

fn merge_json_values(target: &mut serde_json::Value, source: serde_json::Value) {
    match (target, source) {
        (serde_json::Value::Object(ref mut target_map), serde_json::Value::Object(source_map)) => {
            for (k, v) in source_map {
                if k == "allowed_commands" {
                    if let (
                        Some(serde_json::Value::Array(ref mut target_arr)),
                        serde_json::Value::Array(source_arr),
                    ) = (target_map.get_mut(&k), v.clone())
                    {
                        for item in source_arr {
                            if !target_arr.contains(&item) {
                                target_arr.push(item);
                            }
                        }
                        continue;
                    }
                }
                if let Some(target_val) = target_map.get_mut(&k) {
                    if target_val.is_object() && v.is_object() {
                        merge_json_values(target_val, v);
                        continue;
                    }
                }
                target_map.insert(k, v);
            }
        }
        (target, source) => {
            *target = source;
        }
    }
}

pub fn save_settings(s: &Settings) {
    ensure_home();
    if let Ok(text) = serde_json::to_string_pretty(s) {
        let p = settings_path();
        if fs::write(&p, text).is_ok() {
            restrict(&p);
        }
    }
}

fn read_keys_file() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Ok(text) = fs::read_to_string(keys_path()) {
        for line in text.lines() {
            if let Some(eq) = line.find('=') {
                if eq > 0 {
                    map.insert(line[..eq].to_string(), line[eq + 1..].to_string());
                }
            }
        }
    }
    map
}

pub fn load_key(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    if let Ok(v) = std::env::var(name) {
        if !v.trim().is_empty() {
            return Some(v);
        }
    }
    read_keys_file()
        .get(name)
        .filter(|v| !v.trim().is_empty())
        .cloned()
}

pub fn save_key(name: &str, value: &str) {
    ensure_home();
    let mut map = read_keys_file();
    map.insert(name.to_string(), value.to_string());
    let body: String = map.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    let p = keys_path();
    if fs::write(&p, body).is_ok() {
        restrict(&p);
    }
}

pub fn mask(key: &str) -> String {
    let n = key.chars().count();
    if n <= 8 {
        return "***".into();
    }
    let reveal = (n / 10).clamp(2, 4);
    let head: String = key.chars().take(reveal).collect();
    let tail: String = key
        .chars()
        .rev()
        .take(reveal)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}…{tail}")
}

#[cfg(unix)]
fn restrict(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path) {
        let mode = if meta.is_dir() { 0o700 } else { 0o600 };
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
}

#[cfg(not(unix))]
fn restrict(_path: &std::path::Path) {}

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::TEST_ENV_LOCK as ENV_LOCK;
    use super::*;

    fn unique_home() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("bwn-cfg-{id}"))
    }

    #[test]
    fn mask_short_keys_fully_hidden() {
        assert_eq!(mask("short"), "***");
        assert_eq!(mask("12345678"), "***");
        assert_eq!(mask(""), "***");
    }

    #[test]
    fn mask_reveals_head_and_tail() {
        let m = mask("sk-abcdefghijklmnopqrstuvwxyz");
        assert!(m.contains('…'));
        assert!(m.starts_with("sk"));
        assert!(m.ends_with("yz"));
    }

    #[test]
    fn mask_never_leaks_more_than_clamp() {
        let key = "A".repeat(200);
        let m = mask(&key);
        let head = m.split('…').next().unwrap();
        assert!(head.len() <= 4);
    }

    #[test]
    fn preset_lookup() {
        assert!(preset("anthropic").unwrap().protocol == Protocol::Anthropic);
        assert!(preset("ollama").unwrap().protocol == Protocol::OpenAi);
        assert!(preset("ollama").unwrap().local);
        assert!(preset("nonexistent").is_none());
    }

    #[test]
    fn all_presets_have_distinct_ids() {
        for (i, a) in PRESETS.iter().enumerate() {
            for b in &PRESETS[i + 1..] {
                assert_ne!(a.id, b.id);
            }
        }
    }

    #[test]
    fn bundled_skills_include_static_app() {
        let names = bundled_skills()
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"static-app"));
        assert!(names.contains(&"frontend-ux"));
    }

    #[test]
    fn remote_presets_use_https() {
        for p in PRESETS.iter().filter(|p| !p.local) {
            assert!(p.base_url.starts_with("https://"), "{} not https", p.id);
            assert!(!p.env_key.is_empty(), "{} missing env_key", p.id);
        }
    }

    #[test]
    fn settings_default_is_ask() {
        let s = Settings::default();
        assert_eq!(s.permission, "ask");
        assert_eq!(s.provider, "anthropic");
        assert!(s.base_url.is_none());
    }

    #[test]
    fn settings_roundtrip_json() {
        let s = Settings {
            provider: "ollama".into(),
            model: "llama3.2".into(),
            permission: "auto".into(),
            effort: "high".into(),
            base_url: Some("http://x".into()),
            allowed_commands: Vec::new(),
            ..Default::default()
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(back.provider, "ollama");
        assert_eq!(back.effort, "high");
        assert_eq!(back.base_url.as_deref(), Some("http://x"));
    }

    #[test]
    fn settings_tolerates_missing_base_url() {
        let s: Settings =
            serde_json::from_str(r#"{"provider":"openai","model":"gpt-4o","permission":"ask"}"#)
                .unwrap();
        assert!(s.base_url.is_none());
    }

    #[test]
    fn keys_file_parsing_and_env_precedence() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unique_home();
        let _ = fs::remove_dir_all(&h);
        fs::create_dir_all(&h).unwrap();
        std::env::set_var("NEXUS_HOME", &h);
        std::env::remove_var("TESTKEY_A");
        std::env::remove_var("TESTKEY_B");

        fs::write(
            h.join(".env.keys"),
            "TESTKEY_A=from_file\nb=garbage_no_eq_handled\n=leadingeq\nTESTKEY_B=second\n",
        )
        .unwrap();

        let map = read_keys_file();
        assert_eq!(map.get("TESTKEY_A").map(String::as_str), Some("from_file"));
        assert_eq!(map.get("TESTKEY_B").map(String::as_str), Some("second"));
        assert!(!map.contains_key(""));

        assert_eq!(load_key("TESTKEY_A").as_deref(), Some("from_file"));
        std::env::set_var("TESTKEY_A", "from_env");
        assert_eq!(load_key("TESTKEY_A").as_deref(), Some("from_env"));
        std::env::set_var("TESTKEY_A", "   ");
        assert_eq!(load_key("TESTKEY_A").as_deref(), Some("from_file"));
        assert!(load_key("").is_none());

        std::env::remove_var("TESTKEY_A");
        std::env::remove_var("NEXUS_HOME");
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn save_and_load_key_roundtrip() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unique_home();
        let _ = fs::remove_dir_all(&h);
        std::env::set_var("NEXUS_HOME", &h);
        std::env::remove_var("RTKEY");

        save_key("RTKEY", "secret-value");
        assert_eq!(load_key("RTKEY").as_deref(), Some("secret-value"));
        save_key("OTHER", "x");
        save_key("RTKEY", "updated");
        assert_eq!(load_key("RTKEY").as_deref(), Some("updated"));
        assert_eq!(load_key("OTHER").as_deref(), Some("x"));

        std::env::remove_var("NEXUS_HOME");
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn load_settings_none_when_absent() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unique_home();
        let _ = fs::remove_dir_all(&h);
        fs::create_dir_all(&h).unwrap();
        std::env::set_var("NEXUS_HOME", &h);
        assert!(load_settings().is_none());
        let s = Settings {
            provider: "groq".into(),
            model: String::new(),
            permission: "ask".into(),
            effort: "low".into(),
            base_url: None,
            allowed_commands: Vec::new(),
            ..Default::default()
        };
        save_settings(&s);
        assert_eq!(load_settings().unwrap().provider, "groq");
        std::env::remove_var("NEXUS_HOME");
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn memory_roundtrip() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unique_home();
        let _ = fs::remove_dir_all(&h);
        std::env::set_var("NEXUS_HOME", &h);

        assert!(load_memory().is_none());
        save_memory("- prefers Rust\n- dislikes Java");
        let m = load_memory().unwrap();
        assert!(m.contains("prefers Rust"));
        append_memory("uses dark theme");
        let m2 = load_memory().unwrap();
        assert!(m2.contains("dark theme"));

        std::env::remove_var("NEXUS_HOME");
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn test_load_settings_from_dir_hierarchy_and_merging() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unique_home();
        let _ = fs::remove_dir_all(&h);
        fs::create_dir_all(&h).unwrap();
        std::env::set_var("NEXUS_HOME", &h);

        let proj = std::env::temp_dir().join(format!("bwn-cfg-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&proj);
        fs::create_dir_all(proj.join(".buildwithnexus")).unwrap();

        fs::write(h.join("settings.json"), r#"{"provider": "openai", "model": "gpt-4o", "effort": "low", "allowed_commands": ["git status"]}"#).unwrap();
        fs::write(h.join("settings.local.json"), r#"{"effort": "medium"}"#).unwrap();
        fs::write(
            proj.join(".buildwithnexus").join("settings.json"),
            r#"{"model": "gpt-4o-mini", "allowed_commands": ["cargo check"]}"#,
        )
        .unwrap();
        fs::write(
            proj.join(".buildwithnexus").join("settings.local.json"),
            r#"{"permission": "readonly", "allowed_commands": ["git status", "cargo test"]}"#,
        )
        .unwrap();

        let s = load_settings_from_dir(&proj).unwrap();
        assert_eq!(s.provider, "openai");
        assert_eq!(s.model, "gpt-4o-mini");
        assert_eq!(s.effort, "medium");
        assert_eq!(s.permission, "readonly");
        assert_eq!(
            s.allowed_commands,
            vec!["git status", "cargo check", "cargo test"]
        );

        std::env::remove_var("NEXUS_HOME");
        let _ = fs::remove_dir_all(&h);
        let _ = fs::remove_dir_all(&proj);
    }
}
