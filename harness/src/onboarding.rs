// First-run walkthrough. Guides the user from nothing to a working provider:
// pick remote-or-local, drop in a key if needed, choose a model and a trust level.

use crate::config::{self, Settings, PRESETS};
use crate::{local, provider, tui};

pub fn run() -> Option<Settings> {
    config::scaffold_home();
    tui::clear();
    tui::line(&tui::accent("  buildwithnexus"));
    tui::line(&tui::dim("  a hilariously fast, agentic AI CLI — remote or local models"));

    // Warn the user if the home dir landed on a Windows mount — they should
    // set NEXUS_HOME to a native Linux path (e.g. ~/. buildwithnexus in WSL).
    let home = config::home();
    if crate::tools::is_wsl() {
        let h = home.to_string_lossy();
        if h.starts_with("/mnt/") {
            tui::line("");
            tui::line(&tui::yellow("  ⚠ WSL2: home directory is on a Windows mount."));
            tui::line(&tui::dim(&format!("    {}", h)));
            tui::line(&tui::dim("    Set NEXUS_HOME to a Linux path to avoid cross-OS I/O:"));
            tui::line(&tui::dim("    export NEXUS_HOME=$HOME/.buildwithnexus"));
        } else {
            tui::line(&tui::dim("  (WSL2 detected — Windows drive mounts are guarded)"));
        }
    }
    tui::line("");
    tui::line("  Let's get you set up. Pick a model provider:");
    tui::line("");

    for (i, p) in PRESETS.iter().enumerate() {
        let tag = if p.local { tui::green("local") } else { tui::blue("remote") };
        tui::line(&format!("  {}  {:<26} {}", tui::bold(&(i + 1).to_string()), p.label, tag));
    }
    tui::line("");

    let pick = loop {
        let ans = tui::ask("  provider number: ")?;
        if let Ok(n) = ans.trim().parse::<usize>() {
            if n >= 1 && n <= PRESETS.len() {
                break &PRESETS[n - 1];
            }
        }
        tui::line(&tui::red("  enter a number from the list"));
    };

    // Local endpoints can move (custom host/port); offer an override.
    let mut base_url = None;
    if pick.local {
        if let Some(u) = tui::ask(&format!("  endpoint [{}]: ", pick.base_url)) {
            if !u.trim().is_empty() {
                base_url = Some(u.trim().to_string());
            }
        }
    }

    // For local providers, auto-detect what's actually installed so the model
    // prompt offers real choices: Ollama's API for Ollama, GGUF files on disk
    // for llama.cpp / LM Studio.
    let detected: Vec<String> = if pick.local {
        let found = if pick.id == "ollama" {
            provider::ollama_models(base_url.as_deref().unwrap_or(pick.base_url))
        } else {
            local::scan_gguf()
        };
        tui::line("");
        if found.is_empty() {
            tui::line(&tui::yellow("  no local models detected."));
            tui::line(&tui::dim("    • Ollama: run  ollama pull qwen2.5:3b"));
            tui::line(&tui::dim(&format!("    • llama.cpp / LM Studio: drop a .gguf into {}", local::models_dir().display())));
            tui::line(&tui::dim("      (or your LM Studio models folder), then start the server"));
            tui::line(&tui::dim("    you can also just type a model name below, then re-run init once it's available"));
        } else {
            tui::line(&tui::dim("  detected local models:"));
            for (i, m) in found.iter().take(20).enumerate() {
                tui::line(&format!("    {}  {}", tui::bold(&(i + 1).to_string()), m));
            }
        }
        found
    } else {
        Vec::new()
    };

    // Key, only if the provider needs one and we don't already have it.
    if !pick.env_key.is_empty() && config::load_key(pick.env_key).is_none() {
        tui::line("");
        tui::line(&tui::dim(&format!("  {} needs an API key (stored 0600 in ~/.buildwithnexus/.env.keys)", pick.label)));
        let key = tui::ask(&format!("  {}: ", pick.env_key))?;
        if !key.trim().is_empty() {
            config::save_key(pick.env_key, key.trim());
        }
    } else if !pick.env_key.is_empty() {
        let shown = config::load_key(pick.env_key).map(|k| config::mask(&k)).unwrap_or_default();
        tui::line(&tui::green(&format!("  ✓ {} already set ({})", pick.env_key, shown)));
    }

    // Model: pick a detected one by number (or name), else type/accept the default.
    let model = if !detected.is_empty() {
        let def = &detected[0];
        match tui::ask(&format!("  model # or name [{def}]: ")).as_deref().map(str::trim) {
            None | Some("") => def.clone(),
            Some(s) => match s.parse::<usize>() {
                Ok(n) if n >= 1 && n <= detected.len() => detected[n - 1].clone(),
                _ => s.to_string(),
            },
        }
    } else {
        match tui::ask(&format!("  model [{}]: ", pick.default_model)) {
            Some(m) if !m.trim().is_empty() => m.trim().to_string(),
            _ => pick.default_model.to_string(),
        }
    };

    tui::line("");
    tui::line("  Tool permissions:");
    tui::line(&format!("    {}  ask before every file write / command  {}", tui::bold("1"), tui::dim("(recommended)")));
    tui::line(&format!("    {}  auto-approve everything                {}", tui::bold("2"), tui::dim("(yolo)")));
    tui::line(&format!("    {}  read-only — never modify anything", tui::bold("3")));
    let permission = match tui::ask("  choice [1]: ").as_deref().map(str::trim) {
        Some("2") => "auto",
        Some("3") => "readonly",
        _ => "ask",
    }
    .to_string();

    let settings = Settings { provider: pick.id.to_string(), model, permission, base_url, allowed_commands: Vec::new() };
    config::save_settings(&settings);
    tui::line("");
    tui::line(&tui::green("  ✓ ready"));
    Some(settings)
}
