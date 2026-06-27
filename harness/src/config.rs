// Provider presets, persisted settings, API-key store, memory, and skills.
// Everything here is flat data + direct file IO.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    Anthropic,
    OpenAi,
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
    Preset { id: "anthropic", label: "Anthropic (Claude)", protocol: Protocol::Anthropic,
        base_url: "https://api.anthropic.com", env_key: "ANTHROPIC_API_KEY",
        default_model: "claude-sonnet-4-6", local: false },
    Preset { id: "openai", label: "OpenAI", protocol: Protocol::OpenAi,
        base_url: "https://api.openai.com/v1", env_key: "OPENAI_API_KEY",
        default_model: "gpt-4o", local: false },
    Preset { id: "openrouter", label: "OpenRouter", protocol: Protocol::OpenAi,
        base_url: "https://openrouter.ai/api/v1", env_key: "OPENROUTER_API_KEY",
        default_model: "anthropic/claude-3.7-sonnet", local: false },
    Preset { id: "groq", label: "Groq", protocol: Protocol::OpenAi,
        base_url: "https://api.groq.com/openai/v1", env_key: "GROQ_API_KEY",
        default_model: "llama-3.3-70b-versatile", local: false },
    Preset { id: "huggingface", label: "Hugging Face", protocol: Protocol::OpenAi,
        base_url: "https://router.huggingface.co/v1", env_key: "HF_TOKEN",
        default_model: "meta-llama/Llama-3.3-70B-Instruct", local: false },
    Preset { id: "ollama", label: "Ollama (local)", protocol: Protocol::OpenAi,
        base_url: "http://localhost:11434/v1", env_key: "",
        default_model: "llama3.2", local: true },
    Preset { id: "llamacpp", label: "llama.cpp server (local)", protocol: Protocol::OpenAi,
        base_url: "http://localhost:8080/v1", env_key: "",
        default_model: "local-model", local: true },
    Preset { id: "lmstudio", label: "LM Studio (local)", protocol: Protocol::OpenAi,
        base_url: "http://localhost:1234/v1", env_key: "",
        default_model: "local-model", local: true },
];

pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

#[derive(Serialize, Deserialize)]
pub struct Settings {
    pub provider: String,
    pub model: String,
    pub permission: String,
    #[serde(default)]
    pub base_url: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { provider: "anthropic".into(), model: String::new(),
            permission: "ask".into(), base_url: None }
    }
}

pub fn home() -> PathBuf {
    if let Ok(h) = std::env::var("NEXUS_HOME") {
        return PathBuf::from(h);
    }
    let base = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join(".buildwithnexus")
}

fn settings_path() -> PathBuf { home().join("config.json") }
fn keys_path() -> PathBuf { home().join(".env.keys") }
pub fn history_path() -> PathBuf { home().join("history") }
pub fn memory_path() -> PathBuf { home().join("memory.md") }
fn agents_path() -> PathBuf { home().join("Agents.md") }
fn skills_dir() -> PathBuf { home().join("skills") }
fn commands_dir() -> PathBuf { home().join("commands") }
fn hooks_dir() -> PathBuf { home().join("hooks") }

pub fn load_history() -> Vec<String> {
    fs::read_to_string(history_path())
        .map(|t| t.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

pub fn save_history(entries: &[String]) {
    const MAX: usize = 1000;
    ensure_home();
    let tail = if entries.len() > MAX { &entries[entries.len() - MAX..] } else { entries };
    let body: String = tail.iter().map(|e| format!("{}\n", e.replace('\n', " "))).collect();
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
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
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
        if !t.trim().is_empty() { return Some(t.trim().to_string()); }
    }
    let global = agents_path();
    fs::read_to_string(&global).ok().filter(|t| !t.trim().is_empty()).map(|t| t.trim().to_string())
}

// Returns (name, content) pairs for all skill files.
pub fn load_skills() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for dir in [skills_dir(), std::env::current_dir().ok().map(|d| d.join(".buildwithnexus/skills")).unwrap_or_default()] {
        if let Ok(rd) = fs::read_dir(&dir) {
            for e in rd.flatten() {
                let path = e.path();
                if path.extension().is_some_and(|x| x == "md") {
                    if let Ok(content) = fs::read_to_string(&path) {
                        let name = path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        if !name.is_empty() && !content.trim().is_empty() {
                            out.push((name, content.trim().to_string()));
                        }
                    }
                }
            }
        }
    }
    out
}

// ── custom slash commands ─────────────────────────────────────────────────────
pub struct CustomCommand {
    pub name: String,     // without leading /
    pub content: String,  // markdown instructions injected as context
    pub script: Option<PathBuf>, // optional shell/py script to run
}

pub fn load_custom_commands() -> Vec<CustomCommand> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(commands_dir()) {
        for e in rd.flatten() {
            let path = e.path();
            let ext = path.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
            let stem = path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if stem.is_empty() || stem.starts_with('.') { continue; }
            match ext.as_str() {
                "md" => {
                    if let Ok(content) = fs::read_to_string(&path) {
                        out.push(CustomCommand { name: stem, content: content.trim().to_string(), script: None });
                    }
                }
                "sh" | "py" | "bash" => {
                    out.push(CustomCommand { name: stem, content: String::new(), script: Some(path) });
                }
                _ => {}
            }
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
                if matches!(ext.as_str(), "sh" | "bash" | "py" | "python") {
                    scripts.push(p);
                }
            }
        }
    }
    scripts
}

pub fn ensure_home() {
    let _ = fs::create_dir_all(home());
    restrict(&home());
}

pub fn load_settings() -> Option<Settings> {
    let text = fs::read_to_string(settings_path()).ok()?;
    serde_json::from_str(&text).ok()
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
    read_keys_file().get(name).filter(|v| !v.trim().is_empty()).cloned()
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
    let tail: String = key.chars().rev().take(reveal).collect::<Vec<_>>().into_iter().rev().collect();
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
    use super::*;
    use super::TEST_ENV_LOCK as ENV_LOCK;

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
        let s = Settings { provider: "ollama".into(), model: "llama3.2".into(),
            permission: "auto".into(), base_url: Some("http://x".into()) };
        let text = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(back.provider, "ollama");
        assert_eq!(back.base_url.as_deref(), Some("http://x"));
    }

    #[test]
    fn settings_tolerates_missing_base_url() {
        let s: Settings = serde_json::from_str(
            r#"{"provider":"openai","model":"gpt-4o","permission":"ask"}"#).unwrap();
        assert!(s.base_url.is_none());
    }

    #[test]
    fn keys_file_parsing_and_env_precedence() {
        let _g = ENV_LOCK.lock().unwrap();
        let h = unique_home();
        let _ = fs::remove_dir_all(&h);
        fs::create_dir_all(&h).unwrap();
        std::env::set_var("NEXUS_HOME", &h);
        std::env::remove_var("TESTKEY_A");
        std::env::remove_var("TESTKEY_B");

        fs::write(h.join(".env.keys"),
            "TESTKEY_A=from_file\nb=garbage_no_eq_handled\n=leadingeq\nTESTKEY_B=second\n").unwrap();

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
        let _g = ENV_LOCK.lock().unwrap();
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
        let _g = ENV_LOCK.lock().unwrap();
        let h = unique_home();
        let _ = fs::remove_dir_all(&h);
        fs::create_dir_all(&h).unwrap();
        std::env::set_var("NEXUS_HOME", &h);
        assert!(load_settings().is_none());
        let s = Settings { provider: "groq".into(), model: String::new(),
            permission: "ask".into(), base_url: None };
        save_settings(&s);
        assert_eq!(load_settings().unwrap().provider, "groq");
        std::env::remove_var("NEXUS_HOME");
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn memory_roundtrip() {
        let _g = ENV_LOCK.lock().unwrap();
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
}
