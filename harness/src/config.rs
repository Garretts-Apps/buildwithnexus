// Provider presets, persisted settings, API-key store, memory, and skills.
// Everything here is flat data + direct file IO.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub enum Protocol {
    Anthropic,
    OpenAi,
    /// Ollama's native /api/chat endpoint. Unlike the OpenAI-compat /v1
    /// endpoint it accepts `options.num_ctx` — without it Ollama silently
    /// truncates prompts to the server-default window — and lets us reset
    /// `repeat_penalty` (Ollama's 1.1 default corrupts tool-call JSON).
    OllamaNative,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Protocol::Anthropic => write!(f, "Anthropic"),
            Protocol::OpenAi => write!(f, "OpenAI"),
            Protocol::OllamaNative => write!(f, "Ollama"),
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
        protocol: Protocol::OllamaNative,
        // Host root, not …/v1: the native API lives at /api/chat. The
        // provider falls back to {base}/v1 OpenAI-compat on older servers.
        base_url: "http://localhost:11434",
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
    // Any OpenAI-compatible /v1 server: vLLM, TGI, LiteLLM, a corporate
    // gateway… The key is optional (CUSTOM_API_KEY) because most self-hosted
    // servers don't need one; build_provider refuses to send a configured key
    // to a non-HTTPS, non-loopback URL.
    Preset {
        id: "custom",
        label: "OpenAI-compatible endpoint",
        protocol: Protocol::OpenAi,
        base_url: "http://localhost:8000/v1",
        env_key: "",
        default_model: "local-model",
        local: true,
    },
];

/// Optional key for the `custom` preset — not wired through `env_key` so the
/// key stays optional (env_key drives the "must be set" checks).
pub const CUSTOM_KEY: &str = "CUSTOM_API_KEY";

pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

/// Reasoning depth requested from the model. `Off` (the default) sends no
/// thinking/reasoning parameters at all, so the wire shape is unchanged from
/// before the setting existed; the other levels map per protocol in
/// `provider` (Anthropic thinking, OpenAI `reasoning_effort` on reasoning
/// models only, Ollama `think`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Effort {
    #[default]
    Off,
    Low,
    Medium,
    High,
}

impl Effort {
    pub const LEVELS: [&'static str; 4] = ["off", "low", "medium", "high"];

    pub fn parse(s: &str) -> Option<Effort> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "" => Some(Effort::Off),
            "low" => Some(Effort::Low),
            "medium" | "med" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Off => "off",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    pub provider: String,
    pub model: String,
    pub permission: String,
    /// Reasoning level: "off" (default), "low", "medium", or "high" — see
    /// [`Effort`]. `--effort` and `/effort` override and persist it.
    // Stored as `reasoning_effort`: the pre-0.13 `effort` key was never read and
    // every saved settings file carries its old default ("low"), so honoring it
    // would switch reasoning on for existing users. Stale `effort` keys are ignored.
    #[serde(default = "default_effort", rename = "reasoning_effort")]
    pub effort: String,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Sampling temperature override; None → per-protocol default (0.2 on
    /// OpenAI-style APIs; Anthropic uses the server default).
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Response token cap override; None → per-protocol default (4096 on
    /// OpenAI-style APIs, 8192 on Anthropic).
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Context-window override in tokens; None → per-provider default. On
    /// the Ollama preset this also sets `options.num_ctx` directly and
    /// skips /api/show detection.
    #[serde(default)]
    pub context_tokens: Option<u32>,
    /// Session spend ceiling in USD (estimated from the price table); the
    /// agent loop stops before the next model request once it's exceeded.
    /// None or <= 0 → no limit. `--max-budget-usd` overrides it per run.
    #[serde(default)]
    pub max_budget_usd: Option<f64>,
    /// npm auto-update policy: "off" (no check, no notices), "notify"
    /// (daily check, startup notice, never installs — the default), or
    /// "install" (daily check + silent `npm install -g`, notice on next
    /// launch). BWN_NO_AUTO_UPDATE=1 caps "install" back to "notify".
    #[serde(default = "default_auto_update")]
    pub auto_update: String,
    /// Shell binaries that auto-approve in Ask mode. Empty = use built-in defaults.
    #[serde(default)]
    pub allowed_commands: Vec<String>,
    /// Tools/binaries approved with "always allow", keyed by canonical project
    /// directory — an `a` answer in one project never silences the gate in
    /// another. Lives in the user settings file only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub project_allowed: BTreeMap<String, Vec<String>>,
    /// How many background workflows may run at once (default 2).
    #[serde(default = "default_max_concurrent_workflows")]
    pub max_concurrent_workflows: usize,
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub plugins: BTreeMap<String, serde_json::Value>,
    /// Project instruction file names looked up in every directory from the
    /// git root down to the cwd; the first match per directory wins. Default
    /// `["AGENTS.md", "CLAUDE.md"]`; `[]` disables instruction loading.
    #[serde(default = "default_instruction_files")]
    pub instruction_files: Vec<String>,
    /// Extra skill roots (each holding `<name>/SKILL.md` folders or flat
    /// `<name>.md` files) scanned in addition to the built-in locations.
    /// `~/` is expanded; relative paths resolve against the project cwd.
    #[serde(default)]
    pub skill_dirs: Vec<String>,
    /// OS-level sandbox for shell commands: "off" (default), "auto" (confine
    /// when bwrap/sandbox-exec works, else run unsandboxed with a notice), or
    /// "require" (refuse to run commands without a backend). See sandbox.rs.
    #[serde(default = "default_sandbox")]
    pub sandbox: String,
    /// Whether sandboxed commands may reach the network (default true).
    #[serde(default = "default_true")]
    pub sandbox_network: bool,
    /// Inline images in the transcript: "auto" (default; pixel-perfect on
    /// kitty/Ghostty, half-block art elsewhere), "kitty" (force the graphics
    /// protocol), "blocks" (always half-block art), or "off".
    #[serde(default = "default_auto")]
    pub images: String,
    /// Desktop notification when a long turn finishes: "auto" (default;
    /// only while the terminal window is unfocused), "always", or "off".
    #[serde(default = "default_auto")]
    pub notify: String,
}

fn default_auto() -> String {
    "auto".into()
}

fn default_sandbox() -> String {
    "off".into()
}

fn default_true() -> bool {
    true
}

fn default_effort() -> String {
    Effort::Off.as_str().into()
}

fn default_instruction_files() -> Vec<String> {
    vec!["AGENTS.md".into(), "CLAUDE.md".into()]
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            provider: "anthropic".into(),
            model: String::new(),
            permission: "ask".into(),
            effort: default_effort(),
            base_url: None,
            temperature: None,
            max_tokens: None,
            context_tokens: None,
            max_budget_usd: None,
            auto_update: default_auto_update(),
            allowed_commands: Vec::new(),
            project_allowed: BTreeMap::new(),
            max_concurrent_workflows: default_max_concurrent_workflows(),
            mcp_servers: BTreeMap::new(),
            plugins: BTreeMap::new(),
            instruction_files: default_instruction_files(),
            skill_dirs: Vec::new(),
            sandbox: default_sandbox(),
            images: default_auto(),
            notify: default_auto(),
            sandbox_network: true,
        }
    }
}

fn default_max_concurrent_workflows() -> usize {
    2
}

// Only unambiguously read-only binaries auto-approve in Ask mode by default.
// Anything that can mutate files, run arbitrary code, or reach the network
// (git, npm, curl, docker, patch, …) must prompt; users who want more can add
// their own entries via `allowed_commands` in settings.
fn default_auto_update() -> String {
    "notify".into()
}

fn default_allowed_commands() -> Vec<String> {
    [
        "ls", "cat", "head", "tail", "grep", "rg", "pwd", "echo", "which", "wc", "du", "df",
        "sort", "uniq", "diff",
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

/// Canonical key for a project directory in `project_allowed`.
pub fn project_key(cwd: &std::path::Path) -> String {
    cwd.canonicalize()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Tools approved with "always allow" for this project only.
pub fn load_project_allowed(cwd: &std::path::Path) -> Vec<String> {
    load_settings()
        .and_then(|s| s.project_allowed.get(&project_key(cwd)).cloned())
        .unwrap_or_default()
}

/// Persist an "always allow" answer for `tool` scoped to this project.
pub fn add_project_allowed(cwd: &std::path::Path, tool: &str) {
    if tool.is_empty() {
        return;
    }
    let Some(mut s) = load_settings() else {
        return;
    };
    let list = s.project_allowed.entry(project_key(cwd)).or_default();
    if !list.iter().any(|t| t == tool) {
        list.push(tool.to_string());
        save_settings(&s);
    }
}

/// Drop every per-project "always allow" entry for this project. Returns how
/// many entries were cleared.
pub fn reset_project_allowed(cwd: &std::path::Path) -> usize {
    let Some(mut s) = load_settings() else {
        return 0;
    };
    match s.project_allowed.remove(&project_key(cwd)) {
        Some(list) => {
            save_settings(&s);
            list.len()
        }
        None => 0,
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

pub fn settings_path() -> PathBuf {
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
    write_atomic(&history_path(), &body, true);
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
    write_atomic(&memory_path(), content, false);
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
// `Agents.md` (mixed case, harness-specific) defines roles/capabilities the
// model can adopt. It is distinct from the cross-harness project instruction
// files `AGENTS.md` / `CLAUDE.md` handled by `load_instructions` below, which
// carry repository conventions. Skills are markdown instructions loaded on
// demand — either flat `<name>.md` files or `<name>/SKILL.md` folders.

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

/// Load custom user system prompt from project-local `.buildwithnexus/system.md`
/// or global `~/.buildwithnexus/system.md`.
pub fn load_system_prompt() -> Option<String> {
    if let Ok(cwd) = std::env::current_dir() {
        let proj = cwd.join(".buildwithnexus").join("system.md");
        if let Ok(t) = fs::read_to_string(&proj) {
            if !t.trim().is_empty() {
                return Some(t.trim().to_string());
            }
        }
    }
    let global = home().join("system.md");
    fs::read_to_string(&global)
        .ok()
        .filter(|t| !t.trim().is_empty())
        .map(|t| t.trim().to_string())
}

// ── project instruction files (AGENTS.md / CLAUDE.md) ─────────────────────────
// The cross-harness "how to work in this repo" files: build/test commands,
// conventions, do-nots. Discovery order (most general first):
//   1. ~/.buildwithnexus/AGENTS.md
//   2. every directory from the git root (or filesystem root) down to the cwd:
//      the first `instruction_files` name present (AGENTS.md, else CLAUDE.md),
//      then `.buildwithnexus/AGENTS.md`.
// Names are matched against the exact directory listing so a case-insensitive
// filesystem never mistakes the roles file `Agents.md` for `AGENTS.md`.

/// Per-file cap; longer files are cut with a visible marker.
pub const INSTRUCTION_FILE_CAP: usize = 32 * 1024;
/// Cap across all loaded instruction files; later files are omitted.
pub const INSTRUCTION_TOTAL_CAP: usize = 96 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct InstructionFile {
    pub path: PathBuf,
    /// Display name: relative to the git root when there is one.
    pub label: String,
    pub content: String,
    pub truncated: bool,
}

/// Nearest ancestor of `start` (inclusive) that contains a `.git` entry.
pub fn find_git_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|d| d.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Exact-case names in `dir`; empty when unreadable.
fn dir_names(dir: &Path) -> HashSet<String> {
    fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn user_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

fn tilde(path: &Path) -> String {
    if let Some(h) = user_home() {
        if let Ok(rest) = path.strip_prefix(&h) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

fn cut_at_char_boundary(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Instruction files for `cwd`, honouring the `instruction_files` setting.
pub fn load_instructions(cwd: &Path) -> Vec<InstructionFile> {
    let names = load_settings_from_dir(cwd)
        .map(|s| s.instruction_files)
        .unwrap_or_else(default_instruction_files);
    load_instructions_with(cwd, &names)
}

/// `names` are tried in order in each directory; the first present wins.
/// An empty list disables instruction loading entirely.
pub fn load_instructions_with(cwd: &Path, names: &[String]) -> Vec<InstructionFile> {
    let mut out = Vec::new();
    if names.is_empty() {
        return out;
    }
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let root = find_git_root(&cwd);

    let mut candidates: Vec<(PathBuf, String)> = Vec::new();
    let h = home();
    if dir_names(&h).contains("AGENTS.md") {
        let p = h.join("AGENTS.md");
        let label = tilde(&p);
        candidates.push((p, label));
    }
    let label_for = |p: &Path| -> String {
        match &root {
            Some(r) => p
                .strip_prefix(r)
                .map(|rel| rel.display().to_string())
                .unwrap_or_else(|_| p.display().to_string()),
            None => p.display().to_string(),
        }
    };
    let mut chain: Vec<&Path> = cwd.ancestors().collect();
    chain.reverse();
    for dir in chain {
        if root.as_deref().is_some_and(|r| !dir.starts_with(r)) {
            continue;
        }
        let listing = dir_names(dir);
        if let Some(n) = names.iter().find(|n| listing.contains(n.as_str())) {
            let p = dir.join(n);
            let label = label_for(&p);
            candidates.push((p, label));
        }
        let dot = dir.join(".buildwithnexus");
        if dir_names(&dot).contains("AGENTS.md") {
            let p = dot.join("AGENTS.md");
            let label = label_for(&p);
            candidates.push((p, label));
        }
    }

    let mut total = 0usize;
    for (path, label) in candidates {
        let Some(raw) = fs::read_to_string(&path)
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
        else {
            continue;
        };
        let remaining = INSTRUCTION_TOTAL_CAP.saturating_sub(total);
        let (content, truncated) = if remaining == 0 {
            (
                format!(
                    "[… omitted: {} KiB total instruction cap reached — read {} directly]",
                    INSTRUCTION_TOTAL_CAP / 1024,
                    path.display()
                ),
                true,
            )
        } else if raw.len() > INSTRUCTION_FILE_CAP.min(remaining) {
            let cap = INSTRUCTION_FILE_CAP.min(remaining);
            (
                format!(
                    "{}\n\n[… truncated at {} KiB — read {} for the rest]",
                    cut_at_char_boundary(&raw, cap),
                    cap / 1024,
                    path.display()
                ),
                true,
            )
        } else {
            (raw, false)
        };
        total += content.len();
        out.push(InstructionFile {
            path,
            label,
            content,
            truncated,
        });
    }
    out
}

/// System-prompt section for the loaded files, or None when there are none.
pub fn instructions_prompt(files: &[InstructionFile]) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let mut s = String::from(
        "[Project instructions — AGENTS.md / CLAUDE.md]\n\
         Repository instructions, most general first; later files are more specific and take precedence. Follow them.\n",
    );
    for f in files {
        s.push_str(&format!("\n--- {} ---\n{}\n", f.path.display(), f.content));
    }
    Some(s)
}

/// One-line startup notice, e.g. `instructions: AGENTS.md, src/AGENTS.md`.
pub fn instructions_notice(files: &[InstructionFile]) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let names = files
        .iter()
        .map(|f| {
            if f.truncated {
                format!("{} (truncated)", f.label)
            } else {
                f.label.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("instructions: {names}"))
}

/// Starter written by `/init` when the cwd has no AGENTS.md.
pub const STARTER_AGENTS_MD: &str = "\
# AGENTS.md

Instructions for AI coding agents working in this repository.

## Build & test

- Build: `<command>`
- Test: `<command>`
- Lint / format: `<command>`

## Conventions

- <language, style, and directory layout rules>
- <how commits and pull requests are written>

## Do not

- <files or directories that must not be edited>
- <commands that must not be run>
";

/// Create a starter AGENTS.md in `cwd`; refuses to overwrite an existing one.
pub fn create_starter_agents_md(cwd: &Path) -> Result<PathBuf, String> {
    let p = cwd.join("AGENTS.md");
    if p.exists() {
        return Err(format!("{} already exists", p.display()));
    }
    fs::write(&p, STARTER_AGENTS_MD).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(p)
}

// ── skills ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillSource {
    Bundled,
    /// `~/.buildwithnexus/skills/`
    User,
    /// `./.buildwithnexus/skills/`
    Project,
    /// `~/.claude/skills/` or `./.claude/skills/`
    Claude,
    /// `~/.agents/skills/` or `./.agents/skills/`
    Agents,
    /// A `skill_dirs` entry from settings.
    Custom,
}

impl SkillSource {
    pub fn label(self) -> &'static str {
        match self {
            SkillSource::Bundled => "bundled",
            SkillSource::User => "user",
            SkillSource::Project => "project",
            SkillSource::Claude => "claude",
            SkillSource::Agents => "agents",
            SkillSource::Custom => "custom",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    pub name: String,
    /// From `description:` frontmatter (or the first prose line of a flat
    /// file). None for a SKILL.md folder that declares none.
    pub description: Option<String>,
    /// Markdown body with any frontmatter stripped.
    pub content: String,
    pub source: SkillSource,
    /// Folder of a `<name>/SKILL.md` skill; None for flat files and bundled.
    pub dir: Option<PathBuf>,
}

impl Skill {
    pub fn description_or_default(&self) -> &str {
        self.description.as_deref().unwrap_or("(no description)")
    }

    /// Full text handed to the model. Folder skills lead with their directory
    /// so referenced files (scripts/, references/, …) are addressable with
    /// the ordinary file tools.
    pub fn loaded_text(&self) -> String {
        match &self.dir {
            Some(d) => format!(
                "Skill directory: {}\n\
                 (Files this skill references, e.g. scripts/ or references/, live under that path — read them with read_file.)\n\n{}",
                d.display(),
                self.content
            ),
            None => self.content.clone(),
        }
    }
}

fn unquote(v: &str) -> String {
    let b = v.as_bytes();
    if b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"' {
        return v[1..v.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\");
    }
    if b.len() >= 2 && b[0] == b'\'' && b[b.len() - 1] == b'\'' {
        return v[1..v.len() - 1].replace("''", "'");
    }
    v.to_string()
}

/// Minimal YAML frontmatter: `---` … `---` with `key: value` scalars
/// (quoted or bare) and `|` / `>` block scalars. Unknown keys are kept in
/// the map and ignored by callers. Returns the fields and the body after the
/// closing fence; text without a complete fence is all body.
pub fn parse_frontmatter(text: &str) -> (BTreeMap<String, String>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return (BTreeMap::new(), text);
    };
    if first.trim_end() != "---" {
        return (BTreeMap::new(), text);
    }
    let mut pos = first.len();
    let mut map = BTreeMap::new();
    let mut block: Option<(String, char)> = None;
    let mut closed = false;
    for line in lines {
        pos += line.len();
        let raw = line.trim_end_matches(['\r', '\n']);
        let t = raw.trim();
        if t == "---" || t == "..." {
            closed = true;
            break;
        }
        if let Some((key, style)) = &block {
            if raw.starts_with([' ', '\t']) || t.is_empty() {
                if !t.is_empty() {
                    let entry: &mut String = map.entry(key.clone()).or_default();
                    if !entry.is_empty() {
                        entry.push(if *style == '|' { '\n' } else { ' ' });
                    }
                    entry.push_str(t);
                }
                continue;
            }
            block = None;
        }
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let Some((k, v)) = t.split_once(':') else {
            continue;
        };
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        let v = v.trim();
        let style = v.chars().next();
        if matches!(style, Some('|') | Some('>')) && v[1..].trim_matches(['-', '+']).is_empty() {
            block = Some((k.to_string(), style.unwrap_or('|')));
            map.insert(k.to_string(), String::new());
            continue;
        }
        map.insert(k.to_string(), unquote(v));
    }
    if !closed {
        return (BTreeMap::new(), text);
    }
    (map, &text[pos..])
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

fn skill_from_text(
    default_name: &str,
    text: &str,
    source: SkillSource,
    dir: Option<PathBuf>,
) -> Option<Skill> {
    let (fm, body) = parse_frontmatter(text);
    let name = fm
        .get("name")
        .map(|n| n.trim().trim_start_matches('/').to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| default_name.to_string());
    let content = body.trim().to_string();
    let description = match fm.get("description").map(|d| d.trim()) {
        Some(d) if !d.is_empty() => Some(d.to_string()),
        // Flat files never had frontmatter; keep the prose heuristic for them.
        _ if dir.is_none() && !content.is_empty() => Some(skill_description(&content)),
        _ => None,
    };
    if content.is_empty() && description.is_none() {
        return None;
    }
    Some(Skill {
        name,
        description,
        content,
        source,
        dir,
    })
}

// Later sources win on a name collision.
fn push_skill(out: &mut Vec<Skill>, skill: Skill) {
    out.retain(|s| s.name != skill.name);
    out.push(skill);
}

// Flat `<name>.md` files first, then `<name>/SKILL.md` folders, so a folder
// beats a flat file of the same name in the same root.
fn scan_skill_root(dir: &Path, source: SkillSource, out: &mut Vec<Skill>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    let mut folders = Vec::new();
    for path in entries {
        let stem = path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if stem.is_empty() || stem.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if path.join("SKILL.md").is_file() {
                folders.push(path);
            }
        } else if path.extension().is_some_and(|x| x == "md") {
            if let Ok(text) = fs::read_to_string(&path) {
                if let Some(s) = skill_from_text(&stem, &text, source, None) {
                    push_skill(out, s);
                }
            }
        }
    }
    for folder in folders {
        let name = folder
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Ok(text) = fs::read_to_string(folder.join("SKILL.md")) {
            if let Some(s) = skill_from_text(&name, &text, source, Some(folder)) {
                push_skill(out, s);
            }
        }
    }
}

/// Skill roots in precedence order (lowest first): user-level `.agents`,
/// `.claude`, `~/.buildwithnexus/skills`, then `skill_dirs` from settings,
/// then the project-level `.agents`, `.claude`, `.buildwithnexus/skills`.
fn skill_roots(cwd: &Path) -> Vec<(PathBuf, SkillSource)> {
    let mut roots = Vec::new();
    if let Some(u) = user_home() {
        roots.push((u.join(".agents").join("skills"), SkillSource::Agents));
        roots.push((u.join(".claude").join("skills"), SkillSource::Claude));
    }
    roots.push((skills_dir(), SkillSource::User));
    if let Some(s) = load_settings_from_dir(cwd) {
        for d in s.skill_dirs {
            let d = d.trim();
            if d.is_empty() {
                continue;
            }
            let p = match d.strip_prefix("~/") {
                Some(rest) => match user_home() {
                    Some(u) => u.join(rest),
                    None => continue,
                },
                None => cwd.join(d),
            };
            roots.push((p, SkillSource::Custom));
        }
    }
    roots.push((cwd.join(".agents").join("skills"), SkillSource::Agents));
    roots.push((cwd.join(".claude").join("skills"), SkillSource::Claude));
    roots.push((
        cwd.join(".buildwithnexus").join("skills"),
        SkillSource::Project,
    ));
    roots
}

/// All skills visible from `cwd`: bundled, then every root from `skill_roots`;
/// a later source replaces an earlier one of the same name.
pub fn discover_skills(cwd: &Path) -> Vec<Skill> {
    let mut out = Vec::new();
    for (name, content) in bundled_skills() {
        if let Some(s) = skill_from_text(name, content, SkillSource::Bundled, None) {
            push_skill(&mut out, s);
        }
    }
    for (dir, source) in skill_roots(cwd) {
        scan_skill_root(&dir, source, &mut out);
    }
    out
}

/// Warnings worth one dim line: SKILL.md folders without a description.
pub fn skill_warnings(skills: &[Skill]) -> Vec<String> {
    skills
        .iter()
        .filter(|s| s.description.is_none())
        .filter_map(|s| {
            s.dir.as_ref().map(|d| {
                format!(
                    "skill {}: no `description:` in {} frontmatter",
                    s.name,
                    d.join("SKILL.md").display()
                )
            })
        })
        .collect()
}

/// `skill_warnings` filtered to ones not yet returned in this process.
pub fn skill_warnings_once(skills: &[Skill]) -> Vec<String> {
    static SEEN: std::sync::Mutex<Option<HashSet<String>>> = std::sync::Mutex::new(None);
    let mut lock = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let seen = lock.get_or_insert_with(HashSet::new);
    skill_warnings(skills)
        .into_iter()
        .filter(|w| seen.insert(w.clone()))
        .collect()
}

/// Returns (name, description) pairs for all skills — never the bodies, so the
/// system prompt stays small and the model load_skill's what it needs.
pub fn load_skill_descriptions(cwd: &Path) -> Vec<(String, String)> {
    discover_skills(cwd)
        .into_iter()
        .map(|s| {
            let desc = s.description_or_default().to_string();
            (s.name, desc)
        })
        .collect()
}

/// Dim startup lines: which instruction files loaded, plus skill warnings.
pub fn startup_context_notices(cwd: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(n) = instructions_notice(&load_instructions(cwd)) {
        out.push(n);
    }
    out.extend(skill_warnings_once(&discover_skills(cwd)));
    out
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
        ("git", include_str!("bundled_skills/git.md")),
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
        // The letsbeheroes collection: process discipline (how to work),
        // complementing the domain skills above (what to work on).
        (
            "letsbeheroes",
            include_str!("bundled_skills/letsbeheroes/letsbeheroes.md"),
        ),
        (
            "hero-brainstorm",
            include_str!("bundled_skills/letsbeheroes/hero-brainstorm.md"),
        ),
        (
            "hero-plan",
            include_str!("bundled_skills/letsbeheroes/hero-plan.md"),
        ),
        (
            "hero-execute",
            include_str!("bundled_skills/letsbeheroes/hero-execute.md"),
        ),
        (
            "hero-debug",
            include_str!("bundled_skills/letsbeheroes/hero-debug.md"),
        ),
        (
            "hero-ship",
            include_str!("bundled_skills/letsbeheroes/hero-ship.md"),
        ),
        (
            "hero-wait",
            include_str!("bundled_skills/letsbeheroes/hero-wait.md"),
        ),
        (
            "hero-subagents",
            include_str!("bundled_skills/letsbeheroes/hero-subagents.md"),
        ),
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
    // Every discovered skill (bundled, user, project, .claude, .agents) is a
    // slash command too; an explicit commands/ entry of the same name wins.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for skill in discover_skills(&cwd) {
        if !seen.contains(&skill.name) {
            out.push(CustomCommand {
                content: skill.loaded_text(),
                name: skill.name,
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
                let unix_like = matches!(
                    ext.as_str(),
                    "sh" | "bash" | "py" | "python" | "rs" | "rust"
                );
                // PowerShell and cmd scripts only have an interpreter on Windows.
                let windows_only = cfg!(windows) && matches!(ext.as_str(), "ps1" | "cmd" | "bat");
                if unix_like || windows_only {
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
        "hooks/SubagentStop",
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
    load_settings_diag().settings
}

/// Loads settings from global ~/.buildwithnexus/config.json, settings.json, settings.local.json,
/// and project .buildwithnexus/settings.json, settings.local.json, merging them in hierarchy order.
pub fn load_settings_from_dir(workdir: &std::path::Path) -> Option<Settings> {
    load_settings_from_dir_diag(workdir).settings
}

/// A settings file that exists on disk but was ignored, and why — surfaced at
/// startup and in `doctor` so a typo never silently drops configuration.
pub struct SettingsIssue {
    pub source: String,
    pub error: String,
}

pub struct SettingsLoad {
    pub settings: Option<Settings>,
    pub issues: Vec<SettingsIssue>,
    /// At least one settings file exists on disk — distinguishes "broken
    /// config" (never clobber it) from a true first run (offer onboarding).
    pub any_present: bool,
}

pub fn load_settings_diag() -> SettingsLoad {
    load_settings_from_dir_diag(&std::env::current_dir().unwrap_or_else(|_| home()))
}

pub fn load_settings_from_dir_diag(workdir: &std::path::Path) -> SettingsLoad {
    let dot = workdir.join(".buildwithnexus");
    let paths = [
        home().join("config.json"), // legacy base
        settings_path(),
        home().join("settings.local.json"),
        dot.join("settings.json"),
        dot.join("settings.local.json"),
    ];

    let mut sources = Vec::new();
    let mut issues = Vec::new();
    let mut any_present = false;
    for p in &paths {
        let Ok(text) = fs::read_to_string(p) else {
            continue;
        };
        any_present = true;
        // serde_json's Display includes line and column — keep it verbatim.
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(val) if val.is_object() => sources.push(val),
            Ok(_) => issues.push(SettingsIssue {
                source: p.display().to_string(),
                error: "top level must be a JSON object — file ignored".into(),
            }),
            Err(e) => issues.push(SettingsIssue {
                source: p.display().to_string(),
                error: format!("{e} — file ignored"),
            }),
        }
    }

    if sources.is_empty() {
        return SettingsLoad {
            settings: None,
            issues,
            any_present,
        };
    }

    let mut merged = sources.remove(0);
    for source in sources {
        merge_json_values(&mut merged, source);
    }

    match serde_json::from_value(merged) {
        Ok(s) => SettingsLoad {
            settings: Some(s),
            issues,
            any_present,
        },
        Err(e) => {
            issues.push(SettingsIssue {
                source: "merged settings".into(),
                error: format!(
                    "{e} — check the value types in the files listed by `buildwithnexus doctor`"
                ),
            });
            SettingsLoad {
                settings: None,
                issues,
                any_present,
            }
        }
    }
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
        write_atomic(&settings_path(), &text, true);
    }
}

/// Edits the user settings file in place as raw JSON, so keys the `Settings`
/// struct doesn't model (hooks, comments) survive. A missing file starts as
/// `{}`; a malformed one is refused rather than overwritten.
pub fn update_settings_json(
    f: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> Result<(), String> {
    ensure_home();
    let path = settings_path();
    let mut obj = match fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => {
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(serde_json::Value::Object(m)) => m,
                Ok(_) => {
                    return Err(format!(
                        "{}: top level must be a JSON object",
                        path.display()
                    ))
                }
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
        _ => serde_json::Map::new(),
    };
    f(&mut obj);
    let text =
        serde_json::to_string_pretty(&serde_json::Value::Object(obj)).map_err(|e| e.to_string())?;
    if write_atomic(&path, &text, true) {
        Ok(())
    } else {
        Err(format!("could not write {}", path.display()))
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
    write_atomic(&keys_path(), &body, true);
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

// Atomic write for user data (settings, keys, memory, history): temp file in
// the same directory, then rename — a crash mid-save can never truncate the
// file it replaces. With `restricted`, permissions are tightened on the TEMP
// file, so a secrets file is never visible at its real name with default
// (world-readable) permissions, even for an instant.
fn write_atomic(path: &std::path::Path, contents: &str, restricted: bool) -> bool {
    let mut name = match path.file_name() {
        Some(n) => n.to_os_string(),
        None => return false,
    };
    name.push(format!(".tmp-{}", std::process::id()));
    let tmp = path.with_file_name(name);
    if fs::write(&tmp, contents).is_err() {
        return false;
    }
    if restricted {
        restrict(&tmp);
    }
    if fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(&tmp);
        return false;
    }
    true
}

#[cfg(unix)]
fn restrict(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path) {
        let mode = if meta.is_dir() { 0o700 } else { 0o600 };
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
}

// Windows has no mode bits; the equivalent of 0600/0700 is an ACL that drops
// inheritance and grants only the current user. Done by shelling out to the
// built-in `icacls` rather than pulling in a Windows API crate. Directories
// are only tightened once per process — `ensure_home` runs on every save and
// a process spawn per call would be wasteful.
#[cfg(windows)]
fn restrict(path: &std::path::Path) {
    use std::process::{Command, Stdio};
    let is_dir = fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
    if is_dir {
        static DIR_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if DIR_DONE.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
    }
    let user = std::env::var("USERNAME").ok();
    let args = icacls_args(path, is_dir, user.as_deref());
    let outcome = Command::new("icacls")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    let problem = match outcome {
        Ok(o) if o.status.success() => return,
        Ok(o) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
        Err(e) => e.to_string(),
    };
    // Never abort a save over permissions; the file is still written.
    eprintln!(
        "{}",
        crate::tui::dim(&format!(
            "  ⚠ could not restrict {} to the current user (icacls): {problem}",
            path.display()
        ))
    );
}

#[cfg(not(any(unix, windows)))]
fn restrict(_path: &std::path::Path) {}

// `icacls <path> /inheritance:r /grant:r <user>:(perms)` — strip inherited
// ACEs and replace the explicit ones with a single grant to `user`. Without
// `USERNAME`, the well-known OWNER RIGHTS SID (`*S-1-3-4`) grants whoever
// owns the file, i.e. the account that just wrote it. Files get read+write;
// directories get full control that inherits (OI)(CI) so files created in
// them are usable at all — a bare (R,W) on a folder would leave new children
// with no inherited ACEs.
#[cfg_attr(not(windows), allow(dead_code))]
fn icacls_args(path: &std::path::Path, is_dir: bool, user: Option<&str>) -> Vec<String> {
    let who = match user.map(str::trim).filter(|u| !u.is_empty()) {
        Some(u) => u.to_string(),
        None => "*S-1-3-4".to_string(),
    };
    let perms = if is_dir { "(OI)(CI)F" } else { "(R,W)" };
    vec![
        path.to_string_lossy().into_owned(),
        "/inheritance:r".to_string(),
        "/grant:r".to_string(),
        format!("{who}:{perms}"),
    ]
}

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
    fn settings_diag_reports_broken_files() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unique_home();
        let _ = fs::remove_dir_all(&h);
        fs::create_dir_all(&h).unwrap();
        std::env::set_var("NEXUS_HOME", &h);
        let work = h.join("proj");
        fs::create_dir_all(&work).unwrap();

        // No files anywhere: a true first run — nothing present, no issues.
        let l = load_settings_from_dir_diag(&work);
        assert!(l.settings.is_none());
        assert!(l.issues.is_empty());
        assert!(!l.any_present);

        // Syntax error: file is present, ignored, and the issue names it.
        fs::write(h.join("settings.json"), "{ \"provider\": \"openai\", }").unwrap();
        let l = load_settings_from_dir_diag(&work);
        assert!(l.settings.is_none());
        assert!(l.any_present);
        assert_eq!(l.issues.len(), 1);
        assert!(l.issues[0].source.contains("settings.json"));
        assert!(l.issues[0].error.contains("line"));

        // Valid JSON, wrong shape: array top level is ignored with a clear reason.
        fs::write(h.join("settings.json"), "[1,2,3]").unwrap();
        let l = load_settings_from_dir_diag(&work);
        assert!(l.settings.is_none() && l.any_present);
        assert!(l.issues[0].error.contains("JSON object"));

        // Valid file + wrong field type: the merged deserialize fails loudly
        // instead of silently dropping all configuration.
        fs::write(
            h.join("settings.json"),
            r#"{"provider":"openai","model":"gpt-4o","permission":"ask","auto_update":true}"#,
        )
        .unwrap();
        let l = load_settings_from_dir_diag(&work);
        assert!(l.settings.is_none() && l.any_present);
        assert!(l.issues.iter().any(|i| i.source == "merged settings"));

        // Fixed file loads cleanly with zero issues.
        fs::write(
            h.join("settings.json"),
            r#"{"provider":"openai","model":"gpt-4o","permission":"ask"}"#,
        )
        .unwrap();
        let l = load_settings_from_dir_diag(&work);
        assert!(l.settings.is_some());
        assert!(l.issues.is_empty());

        // A broken project-local file is reported but doesn't take down the
        // valid global settings.
        fs::create_dir_all(work.join(".buildwithnexus")).unwrap();
        fs::write(work.join(".buildwithnexus/settings.json"), "{oops").unwrap();
        let l = load_settings_from_dir_diag(&work);
        assert!(l.settings.is_some());
        assert_eq!(l.issues.len(), 1);

        std::env::remove_var("NEXUS_HOME");
        let _ = fs::remove_dir_all(&h);
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
        assert!(preset("ollama").unwrap().protocol == Protocol::OllamaNative);
        assert!(preset("ollama").unwrap().local);
        assert!(preset("lmstudio").unwrap().protocol == Protocol::OpenAi);
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
    fn bundled_skills_include_letsbeheroes_collection() {
        let skills = bundled_skills();
        let names: Vec<_> = skills.iter().map(|(n, _)| *n).collect();
        for n in [
            "letsbeheroes",
            "hero-brainstorm",
            "hero-plan",
            "hero-execute",
            "hero-debug",
            "hero-ship",
            "hero-wait",
            "hero-subagents",
        ] {
            assert!(names.contains(&n), "missing skill {n}");
        }
        // Every member the charter references must actually be registered.
        let charter = skills
            .iter()
            .find(|(n, _)| *n == "letsbeheroes")
            .map(|(_, c)| *c)
            .unwrap();
        for n in &names {
            if let Some(member) = n.strip_prefix("hero-") {
                assert!(
                    charter.contains(&format!("/hero-{member}")),
                    "charter doesn't mention /hero-{member}"
                );
            }
        }
        // No duplicate names across the whole corpus.
        let mut uniq = std::collections::HashSet::new();
        for n in &names {
            assert!(uniq.insert(n), "duplicate skill name {n}");
        }
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
        // Newer knobs default to None on old settings files.
        assert!(s.context_tokens.is_none());
    }

    #[test]
    fn effort_parses_levels_and_defaults_to_off() {
        assert_eq!(Effort::parse("off"), Some(Effort::Off));
        assert_eq!(Effort::parse(" Low "), Some(Effort::Low));
        assert_eq!(Effort::parse("MEDIUM"), Some(Effort::Medium));
        assert_eq!(Effort::parse("high"), Some(Effort::High));
        assert_eq!(Effort::parse("max"), None);
        assert_eq!(Effort::default(), Effort::Off);
        assert_eq!(Effort::High.to_string(), "high");
        for l in Effort::LEVELS {
            assert_eq!(Effort::parse(l).unwrap().as_str(), l);
        }
        // Settings default to "off" so a fresh install sends no thinking params;
        // a file without the key gets the same.
        assert_eq!(Settings::default().effort, "off");
        let s: Settings =
            serde_json::from_str(r#"{"provider":"openai","model":"gpt-4o","permission":"ask"}"#)
                .unwrap();
        assert_eq!(s.effort, "off");
        assert!(s.max_budget_usd.is_none());
    }

    #[test]
    fn settings_max_budget_usd_roundtrip() {
        let s = Settings {
            max_budget_usd: Some(2.5),
            ..Default::default()
        };
        let text = serde_json::to_string(&s).unwrap();
        assert!(text.contains("\"max_budget_usd\":2.5"));
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(back.max_budget_usd, Some(2.5));
    }

    #[test]
    fn settings_context_tokens_roundtrip() {
        let s = Settings {
            context_tokens: Some(16_384),
            ..Default::default()
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(back.context_tokens, Some(16_384));
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
    fn config_saves_are_atomic_restricted_and_leave_no_temp() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unique_home();
        let _ = fs::remove_dir_all(&h);
        std::env::set_var("NEXUS_HOME", &h);

        save_key("ATOMKEY", "secret-value");
        save_settings(&Settings::default());
        save_memory("remember this");

        assert_eq!(load_key("ATOMKEY").as_deref(), Some("secret-value"));
        assert!(load_settings_from_dir_diag(&h).settings.is_some());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for f in [".env.keys", "settings.json"] {
                let mode = fs::metadata(h.join(f)).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{f} must be owner-only, got {mode:o}");
            }
        }

        // Atomic saves must not strand temp files next to the real ones.
        let stray: Vec<_> = fs::read_dir(&h)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(stray.is_empty(), "stray temp files: {stray:?}");

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
    fn project_allowed_is_scoped_per_project_and_resettable() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unique_home();
        let _ = fs::remove_dir_all(&h);
        fs::create_dir_all(&h).unwrap();
        std::env::set_var("NEXUS_HOME", &h);
        save_settings(&Settings::default());

        let a = h.join("proj-a");
        let b = h.join("proj-b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        add_project_allowed(&a, "write_file");
        add_project_allowed(&a, "write_file"); // idempotent
        add_project_allowed(&a, "npm");

        assert_eq!(load_project_allowed(&a), vec!["write_file", "npm"]);
        assert!(load_project_allowed(&b).is_empty(), "scope must not leak");
        // The legacy global list is untouched.
        assert!(load_settings().unwrap().allowed_commands.is_empty());
        // Keys are canonical so `proj-a/.` resolves to the same entry.
        assert_eq!(load_project_allowed(&a.join(".")).len(), 2);

        assert_eq!(reset_project_allowed(&a), 2);
        assert!(load_project_allowed(&a).is_empty());
        assert_eq!(reset_project_allowed(&a), 0);
        // An empty map is omitted from the file entirely.
        let text = fs::read_to_string(h.join("settings.json")).unwrap();
        assert!(!text.contains("project_allowed"), "{text}");

        std::env::remove_var("NEXUS_HOME");
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn max_concurrent_workflows_defaults_to_two() {
        let base = r#""provider":"ollama","model":"llama3.2","permission":"ask""#;
        let s: Settings = serde_json::from_str(&format!("{{{base}}}")).unwrap();
        assert_eq!(s.max_concurrent_workflows, 2);
        assert!(s.project_allowed.is_empty());
        let s: Settings =
            serde_json::from_str(&format!("{{{base},\"max_concurrent_workflows\":4}}")).unwrap();
        assert_eq!(s.max_concurrent_workflows, 4);
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

        fs::write(h.join("settings.json"), r#"{"provider": "openai", "model": "gpt-4o", "reasoning_effort": "low", "allowed_commands": ["git status"]}"#).unwrap();
        fs::write(
            h.join("settings.local.json"),
            r#"{"reasoning_effort": "medium"}"#,
        )
        .unwrap();
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

    #[test]
    fn icacls_args_grant_only_current_user() {
        let p = std::path::Path::new(r"C:\Users\me\.buildwithnexus\.env.keys");
        let a = icacls_args(p, false, Some("me"));
        assert_eq!(
            a,
            vec![
                r"C:\Users\me\.buildwithnexus\.env.keys",
                "/inheritance:r",
                "/grant:r",
                "me:(R,W)"
            ]
        );
        // Directories: full control, inherited by new children.
        let d = icacls_args(p.parent().unwrap(), true, Some("me"));
        assert_eq!(d[3], "me:(OI)(CI)F");
        // No USERNAME (or a blank one): fall back to the OWNER RIGHTS SID.
        assert_eq!(icacls_args(p, false, None)[3], "*S-1-3-4:(R,W)");
        assert_eq!(icacls_args(p, false, Some("  "))[3], "*S-1-3-4:(R,W)");
    }

    fn unique_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("bwn-{tag}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn instructions_walk_git_root_to_cwd_with_claude_fallback() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unique_home();
        let _ = fs::remove_dir_all(&h);
        fs::create_dir_all(&h).unwrap();
        std::env::set_var("NEXUS_HOME", &h);
        write(&h.join("AGENTS.md"), "global rules");
        // The roles file must never be mistaken for an instruction file.
        write(&h.join("Agents.md"), "## Engineer\nrole text");

        let outer = unique_dir("instr");
        write(&outer.join("AGENTS.md"), "ABOVE THE GIT ROOT");
        let root = outer.join("repo");
        fs::create_dir_all(root.join(".git")).unwrap();
        write(&root.join("AGENTS.md"), "root agents");
        write(&root.join("CLAUDE.md"), "root claude (shadowed)");
        write(&root.join("sub").join("CLAUDE.md"), "sub claude");
        let leaf = root.join("sub").join("leaf");
        write(&leaf.join("AGENTS.md"), "leaf agents");
        write(&leaf.join(".buildwithnexus").join("AGENTS.md"), "leaf dot");
        write(&leaf.join(".buildwithnexus").join("Agents.md"), "## Roles");
        write(&leaf.join("deeper").join("AGENTS.md"), "below cwd");

        let files = load_instructions_with(&leaf, &default_instruction_files());
        let labels: Vec<&str> = files.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels.len(), 5, "{labels:?}");
        assert!(labels[0].ends_with("AGENTS.md") && files[0].content == "global rules");
        assert_eq!(
            &labels[1..],
            [
                "AGENTS.md",
                "sub/CLAUDE.md",
                "sub/leaf/AGENTS.md",
                "sub/leaf/.buildwithnexus/AGENTS.md"
            ]
        );
        let bodies: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        assert!(!bodies.contains(&"ABOVE THE GIT ROOT"));
        assert!(!bodies.contains(&"root claude (shadowed)"));
        assert!(!bodies.iter().any(|b| b.contains("## Roles")));
        assert!(files.iter().all(|f| !f.truncated));

        let notice = instructions_notice(&files).unwrap();
        assert!(notice.starts_with("instructions: "));
        assert!(notice.ends_with(
            "AGENTS.md, sub/CLAUDE.md, sub/leaf/AGENTS.md, sub/leaf/.buildwithnexus/AGENTS.md"
        ));
        let prompt = instructions_prompt(&files).unwrap();
        assert!(prompt.starts_with("[Project instructions"));
        let a = prompt.find("global rules").unwrap();
        let b = prompt.find("root agents").unwrap();
        let c = prompt.find("leaf dot").unwrap();
        assert!(a < b && b < c);
        assert!(prompt.contains(&format!("--- {} ---", leaf.join("AGENTS.md").display())));
        assert!(instructions_prompt(&[]).is_none());
        assert!(instructions_notice(&[]).is_none());

        std::env::remove_var("NEXUS_HOME");
        let _ = fs::remove_dir_all(&h);
        let _ = fs::remove_dir_all(&outer);
    }

    #[test]
    fn instructions_without_git_walk_from_filesystem_root() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unique_home();
        let _ = fs::remove_dir_all(&h);
        fs::create_dir_all(&h).unwrap();
        std::env::set_var("NEXUS_HOME", &h);
        let d = unique_dir("nogit");
        let cwd = d.join("a").join("b");
        write(&d.join("a").join("AGENTS.md"), "parent");
        write(&cwd.join("AGENTS.md"), "child");
        let files = load_instructions_with(&cwd, &default_instruction_files());
        let bodies: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(bodies, ["parent", "child"]);
        // No git root: labels are absolute paths.
        assert!(files[1].path.is_absolute());
        assert_eq!(files[1].label, files[1].path.display().to_string());
        std::env::remove_var("NEXUS_HOME");
        let _ = fs::remove_dir_all(&h);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn instructions_respect_settings_names_and_empty_disables() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unique_home();
        let _ = fs::remove_dir_all(&h);
        fs::create_dir_all(&h).unwrap();
        std::env::set_var("NEXUS_HOME", &h);
        write(&h.join("AGENTS.md"), "global");
        let root = unique_dir("names");
        fs::create_dir_all(root.join(".git")).unwrap();
        write(&root.join("AGENTS.md"), "agents");
        write(&root.join("GEMINI.md"), "gemini");

        let only_gemini = load_instructions_with(&root, &["GEMINI.md".to_string()]);
        let bodies: Vec<&str> = only_gemini.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(bodies, ["global", "gemini"]);
        assert!(load_instructions_with(&root, &[]).is_empty());

        // The settings key drives the default loader; `[]` switches it off.
        write(
            &h.join("settings.json"),
            r#"{"provider":"openai","model":"gpt-4o","permission":"ask","instruction_files":["GEMINI.md","AGENTS.md"]}"#,
        );
        let via_settings = load_instructions(&root);
        let bodies: Vec<&str> = via_settings.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(bodies, ["global", "gemini"]);
        write(
            &root.join(".buildwithnexus").join("settings.json"),
            r#"{"instruction_files":[]}"#,
        );
        assert!(load_instructions(&root).is_empty());

        std::env::remove_var("NEXUS_HOME");
        let _ = fs::remove_dir_all(&h);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn instructions_enforce_per_file_and_total_caps() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unique_home();
        let _ = fs::remove_dir_all(&h);
        fs::create_dir_all(&h).unwrap();
        std::env::set_var("NEXUS_HOME", &h);
        let root = unique_dir("caps");
        fs::create_dir_all(root.join(".git")).unwrap();
        let big = "é".repeat(20 * 1024); // 40 KiB of 2-byte chars
        write(&root.join("AGENTS.md"), &big);
        write(&root.join("a").join("AGENTS.md"), &big);
        write(&root.join("a").join("b").join("AGENTS.md"), &big);
        let leaf = root.join("a").join("b").join("c");
        write(&leaf.join("AGENTS.md"), "small but late");

        let files = load_instructions_with(&leaf, &default_instruction_files());
        assert_eq!(files.len(), 4);
        for (i, f) in files[..3].iter().enumerate() {
            assert!(f.truncated);
            assert!(f.content.contains("[… truncated at "));
            let body = f.content.split("\n\n[… truncated").next().unwrap();
            assert!(body.len() <= INSTRUCTION_FILE_CAP);
            assert!(body.chars().all(|c| c == 'é'));
            if i < 2 {
                // Full per-file cap, cut on a char boundary.
                assert!(f.content.contains("[… truncated at 32 KiB"));
                assert!(body.len() > INSTRUCTION_FILE_CAP - 4);
            }
        }
        assert!(files[3].truncated);
        assert!(files[3]
            .content
            .contains("omitted: 96 KiB total instruction cap"));
        assert!(!files[3].content.contains("small but late"));
        let total: usize = files.iter().map(|f| f.content.len()).sum();
        assert!(total <= INSTRUCTION_TOTAL_CAP + 4 * 200);
        let notice = instructions_notice(&files).unwrap();
        assert!(notice.contains("AGENTS.md (truncated)"));

        std::env::remove_var("NEXUS_HOME");
        let _ = fs::remove_dir_all(&h);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn frontmatter_parses_quoted_unquoted_crlf_and_missing() {
        let (fm, body) = parse_frontmatter(
            "---\nname: my-skill\ndescription: \"Use when: quoting \\\"things\\\"\"\nunknown: 3\n---\n\n# Body\ntext",
        );
        assert_eq!(fm["name"], "my-skill");
        assert_eq!(fm["description"], "Use when: quoting \"things\"");
        assert_eq!(fm["unknown"], "3");
        assert_eq!(body.trim(), "# Body\ntext");

        let (fm, body) =
            parse_frontmatter("\u{feff}---\r\nname: 'it''s'\r\n# comment\r\n---\r\nbody\r\n");
        assert_eq!(fm["name"], "it's");
        assert!(!fm.contains_key("description"));
        assert_eq!(body.trim(), "body");

        let (fm, body) = parse_frontmatter("# No frontmatter\nplain");
        assert!(fm.is_empty());
        assert_eq!(body, "# No frontmatter\nplain");

        // An unclosed fence is not frontmatter — everything stays body.
        let (fm, body) = parse_frontmatter("---\nname: x\nstill body");
        assert!(fm.is_empty());
        assert_eq!(body, "---\nname: x\nstill body");

        let (fm, _) = parse_frontmatter("---\n---\nempty");
        assert!(fm.is_empty());
    }

    #[test]
    fn frontmatter_block_scalars_fold_or_keep_lines() {
        let (fm, body) = parse_frontmatter(
            "---\ndescription: >-\n  first line\n  second line\nname: |\n  a\n  b\nafter: 1\n---\nbody",
        );
        assert_eq!(fm["description"], "first line second line");
        assert_eq!(fm["name"], "a\nb");
        assert_eq!(fm["after"], "1");
        assert_eq!(body, "body");
    }

    #[test]
    fn skill_precedence_folders_beat_flat_and_project_beats_user() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unique_home();
        let _ = fs::remove_dir_all(&h);
        fs::create_dir_all(&h).unwrap();
        std::env::set_var("NEXUS_HOME", &h);
        let old_home = std::env::var_os("HOME");
        let user = unique_dir("userhome");
        std::env::set_var("HOME", &user);
        let proj = unique_dir("proj");

        // user root: flat file and a folder of the same name → folder wins.
        write(&h.join("skills").join("foo.md"), "Flat foo.\nmore");
        write(
            &h.join("skills").join("foo").join("SKILL.md"),
            "---\nname: foo\ndescription: Folder foo\n---\nFolder body",
        );
        // project flat file beats the user folder.
        write(
            &proj.join(".buildwithnexus").join("skills").join("foo.md"),
            "Project foo.",
        );
        // ~/.claude overrides a bundled name; folder name is the fallback name.
        write(
            &user
                .join(".claude")
                .join("skills")
                .join("git")
                .join("SKILL.md"),
            "---\ndescription: \"Custom git\"\n---\nbody",
        );
        // ./.agents: frontmatter name wins over the folder name; no description.
        write(
            &proj
                .join(".agents")
                .join("skills")
                .join("some-dir")
                .join("SKILL.md"),
            "---\nname: renamed\n---\nno desc body",
        );
        // skill_dirs entry from settings, relative to the project.
        write(
            &h.join("settings.json"),
            r#"{"provider":"openai","model":"m","permission":"ask","skill_dirs":["extra"]}"#,
        );
        write(
            &proj.join("extra").join("bar").join("SKILL.md"),
            "---\ndescription: Bar\n---\nbar",
        );
        // hidden and non-skill entries are ignored
        write(
            &proj
                .join(".claude")
                .join("skills")
                .join(".hidden")
                .join("SKILL.md"),
            "x",
        );
        write(
            &proj
                .join(".claude")
                .join("skills")
                .join("nope")
                .join("README.md"),
            "x",
        );

        let skills = discover_skills(&proj);
        let find = |n: &str| skills.iter().find(|s| s.name == n).cloned();

        let foo = find("foo").unwrap();
        assert_eq!(foo.source, SkillSource::Project);
        assert_eq!(foo.content, "Project foo.");
        assert_eq!(foo.description.as_deref(), Some("Project foo."));
        assert!(foo.dir.is_none());
        assert_eq!(skills.iter().filter(|s| s.name == "foo").count(), 1);

        let git = find("git").unwrap();
        assert_eq!(git.source, SkillSource::Claude);
        assert_eq!(git.description.as_deref(), Some("Custom git"));
        assert_eq!(
            git.dir.as_deref(),
            Some(user.join(".claude/skills/git").as_path())
        );
        assert!(git.loaded_text().starts_with("Skill directory: "));
        assert!(git.loaded_text().ends_with("\n\nbody"));

        let renamed = find("renamed").unwrap();
        assert_eq!(renamed.source, SkillSource::Agents);
        assert!(renamed.description.is_none());
        assert_eq!(renamed.description_or_default(), "(no description)");
        assert!(find("some-dir").is_none());

        let bar = find("bar").unwrap();
        assert_eq!(bar.source, SkillSource::Custom);
        assert!(find(".hidden").is_none() && find("nope").is_none());
        assert!(find("rust-cli").is_some_and(|s| s.source == SkillSource::Bundled));

        let warns = skill_warnings(&skills);
        assert_eq!(warns.len(), 1, "{warns:?}");
        assert!(warns[0].contains("renamed") && warns[0].contains("SKILL.md"));
        assert_eq!(skill_warnings_once(&skills).len(), 1);
        assert!(skill_warnings_once(&skills).is_empty());

        let descs = load_skill_descriptions(&proj);
        assert!(descs.contains(&("renamed".to_string(), "(no description)".to_string())));
        assert!(descs.contains(&("git".to_string(), "Custom git".to_string())));

        // A user-root folder skill also becomes a slash command.
        write(
            &h.join("skills").join("zed").join("SKILL.md"),
            "---\ndescription: Z\n---\nzed body",
        );
        std::env::set_current_dir(&proj).unwrap();
        let cmds = load_custom_commands();
        let zed = cmds.iter().find(|c| c.name == "zed").unwrap();
        assert!(zed.script.is_none() && zed.content.contains("Skill directory: "));
        assert!(zed.content.ends_with("zed body"));

        match old_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        std::env::remove_var("NEXUS_HOME");
        let _ = fs::remove_dir_all(&h);
        let _ = fs::remove_dir_all(&user);
        let _ = std::env::set_current_dir(std::env::temp_dir());
        let _ = fs::remove_dir_all(&proj);
    }

    #[test]
    fn starter_agents_md_is_created_once() {
        let d = unique_dir("starter");
        let p = create_starter_agents_md(&d).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("## Build & test") && text.contains("## Do not"));
        assert!(create_starter_agents_md(&d).is_err());
        let files = load_instructions_with(&d, &default_instruction_files());
        assert!(files.iter().any(|f| f.path == p));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn settings_instruction_files_and_skill_dirs_roundtrip() {
        let s: Settings =
            serde_json::from_str(r#"{"provider":"openai","model":"gpt-4o","permission":"ask"}"#)
                .unwrap();
        assert_eq!(s.instruction_files, ["AGENTS.md", "CLAUDE.md"]);
        assert!(s.skill_dirs.is_empty());
        let s: Settings = serde_json::from_str(
            r#"{"provider":"openai","model":"m","permission":"ask","instruction_files":[],"skill_dirs":["~/my-skills"]}"#,
        )
        .unwrap();
        assert!(s.instruction_files.is_empty());
        assert_eq!(s.skill_dirs, ["~/my-skills"]);
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(back.instruction_files.is_empty());
        assert_eq!(back.skill_dirs, ["~/my-skills"]);
    }
}
