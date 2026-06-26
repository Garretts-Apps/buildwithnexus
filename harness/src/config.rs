// Provider presets, persisted settings, and the API-key store.
// Everything here is flat data + direct file IO — no abstraction the call sites
// don't actually need (compression-oriented).

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    Anthropic,
    OpenAi,
}

// A model endpoint the harness knows how to talk to out of the box. Two wire
// protocols cover every one of these — the rest is just base_url/key/model data.
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    pub protocol: Protocol,
    pub base_url: &'static str,
    pub env_key: &'static str, // key name in env / .env.keys; "" = local, no key
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
    pub provider: String, // preset id
    pub model: String,
    pub permission: String, // "ask" | "auto" | "readonly"
    #[serde(default)]
    pub base_url: Option<String>, // override for self-hosted endpoints
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

fn settings_path() -> PathBuf {
    home().join("config.json")
}

fn keys_path() -> PathBuf {
    home().join(".env.keys")
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

// Keys live one-per-line as NAME=VALUE, 0600. Env vars win over the file so CI
// and one-off overrides Just Work.
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
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Tests here mutate process-global env (NEXUS_HOME and key vars); serialize
    // them so parallel runs don't clobber each other.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn unique_home() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("bwn-cfg-{id}"))
    }

    // ── mask ────────────────────────────────────────────────────────────────
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
        // Even a very long key reveals at most 4 chars each side.
        let key = "A".repeat(200);
        let m = mask(&key);
        let head = m.split('…').next().unwrap();
        assert!(head.len() <= 4);
    }

    // ── preset ──────────────────────────────────────────────────────────────
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

    // ── settings default ────────────────────────────────────────────────────
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

    // ── keys file parsing + precedence ──────────────────────────────────────
    #[test]
    fn keys_file_parsing_and_env_precedence() {
        let _g = ENV_LOCK.lock().unwrap();
        let h = unique_home();
        let _ = fs::remove_dir_all(&h);
        fs::create_dir_all(&h).unwrap();
        std::env::set_var("NEXUS_HOME", &h);
        std::env::remove_var("TESTKEY_A");
        std::env::remove_var("TESTKEY_B");

        // Lines with no '=' and a leading '=' are ignored; valid pairs kept.
        fs::write(h.join(".env.keys"),
            "TESTKEY_A=from_file\nb=garbage_no_eq_handled\n=leadingeq\nTESTKEY_B=second\n").unwrap();

        let map = read_keys_file();
        assert_eq!(map.get("TESTKEY_A").map(String::as_str), Some("from_file"));
        assert_eq!(map.get("TESTKEY_B").map(String::as_str), Some("second"));
        assert!(!map.contains_key(""));

        // File value when env unset.
        assert_eq!(load_key("TESTKEY_A").as_deref(), Some("from_file"));
        // Env overrides file.
        std::env::set_var("TESTKEY_A", "from_env");
        assert_eq!(load_key("TESTKEY_A").as_deref(), Some("from_env"));
        // Blank env is ignored, file used.
        std::env::set_var("TESTKEY_A", "   ");
        assert_eq!(load_key("TESTKEY_A").as_deref(), Some("from_file"));
        // Empty name → None.
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
        // Overwrite preserves other keys.
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
}
