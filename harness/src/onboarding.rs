// First-run walkthrough. Guides the user from nothing to a working provider:
// pick remote-or-local, drop in a key if needed, choose a model and a trust level.

use crate::config::{self, Settings, PRESETS};
use crate::{local, provider, tui};

pub fn run() -> Option<Settings> {
    config::scaffold_home();
    tui::clear();
    tui::line(&tui::accent("  buildwithnexus"));
    tui::line(&tui::dim(
        "  a hilariously fast, agentic AI CLI — remote or local models",
    ));
    tui::line("");
    crate::check_and_offer_install_dependencies(true);

    // Warn the user if the home dir landed on a Windows mount — they should
    // set NEXUS_HOME to a native Linux path (e.g. ~/. buildwithnexus in WSL).
    let home = config::home();
    if crate::tools::is_wsl() {
        let h = home.to_string_lossy();
        if h.starts_with("/mnt/") {
            tui::line("");
            tui::line(&tui::yellow(
                "  ⚠ WSL2: home directory is on a Windows mount.",
            ));
            tui::line(&tui::dim(&format!("    {}", h)));
            tui::line(&tui::dim(
                "    Set NEXUS_HOME to a Linux path to avoid cross-OS I/O:",
            ));
            tui::line(&tui::dim("    export NEXUS_HOME=$HOME/.buildwithnexus"));
        } else {
            tui::line(&tui::dim(
                "  (WSL2 detected — Windows drive mounts are guarded)",
            ));
        }
    }
    tui::line("");
    let default_ollama_url = config::PRESETS
        .iter()
        .find(|p| p.id == "ollama")
        .map(|p| p.base_url)
        .unwrap_or("http://localhost:11434/v1");
    let ollama_models = provider::ollama_models(default_ollama_url);
    tui::line(&tui::accent("  Local model check"));
    if ollama_models.is_empty() {
        tui::line(&tui::dim(
            "  Ollama is reachable only after it is installed, running, and has a model pulled.",
        ));
    } else {
        tui::line(&tui::green(&format!(
            "  found Ollama with {} model{}",
            ollama_models.len(),
            if ollama_models.len() == 1 { "" } else { "s" }
        )));
    }
    tui::line("");
    tui::line("  Pick a model provider:");
    tui::line("");

    if !ollama_models.is_empty() {
        tui::line(&format!(
            "  {}  {:<26} {}",
            tui::bold("0"),
            "Ollama",
            tui::green("recommended local")
        ));
    }
    let mut display_presets = Vec::new();
    for (i, p) in PRESETS.iter().enumerate().filter(|(_, p)| p.local) {
        display_presets.push((i, p));
    }
    for (i, p) in PRESETS.iter().enumerate().filter(|(_, p)| !p.local) {
        display_presets.push((i, p));
    }

    tui::line(&tui::dim("  Local"));
    let mut disp_num = 1;
    for &(_i, p) in &display_presets {
        if !p.local {
            break;
        }
        tui::line(&format!(
            "  {}  {:<26} {}",
            tui::bold(&disp_num.to_string()),
            p.label,
            tui::green("local")
        ));
        disp_num += 1;
    }
    tui::line(&tui::dim("  Remote"));
    for &(_i, p) in &display_presets {
        if p.local {
            continue;
        }
        tui::line(&format!(
            "  {}  {:<26} {}",
            tui::bold(&disp_num.to_string()),
            p.label,
            tui::blue("remote")
        ));
        disp_num += 1;
    }
    tui::line("");

    let pick = loop {
        let ans = tui::ask("  provider number or name: ")?;
        let ans = ans.trim();
        if ans == "0" && !ollama_models.is_empty() {
            if let Some(ollama_preset) = PRESETS.iter().find(|p| p.id == "ollama") {
                break ollama_preset;
            }
            tui::line(&tui::red("  ollama preset is not configured in PRESETS"));
            continue;
        }
        if let Ok(n) = ans.parse::<usize>() {
            if n >= 1 && n <= display_presets.len() {
                break display_presets[n - 1].1;
            }
        }
        if let Some(&(_, p)) = display_presets
            .iter()
            .find(|&&(_, p)| p.id.eq_ignore_ascii_case(ans) || p.label.eq_ignore_ascii_case(ans))
        {
            break p;
        }
        tui::line(&tui::red("  enter a number or provider name from the list"));
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
        let found = if pick.id == "ollama"
            && base_url.as_deref().unwrap_or(pick.base_url) == default_ollama_url
        {
            ollama_models.clone()
        } else if pick.id == "ollama" {
            provider::ollama_models(base_url.as_deref().unwrap_or(pick.base_url))
        } else {
            // Ask the running server first; GGUF files on disk are only a
            // hint for a server that is not up yet.
            let served = provider::openai_models(base_url.as_deref().unwrap_or(pick.base_url));
            if served.is_empty() {
                local::scan_gguf()
            } else {
                served
            }
        };
        tui::line("");
        if found.is_empty() {
            tui::line(&tui::yellow("  no local models detected."));
            tui::line(&tui::dim("    • Ollama: run  ollama pull qwen2.5:3b"));
            tui::line(&tui::dim(&format!(
                "    • llama.cpp / LM Studio: drop a .gguf into {}",
                local::models_dir().display()
            )));
            tui::line(&tui::dim(
                "      (or your LM Studio models folder), then start the server",
            ));
            tui::line(&tui::dim("    you can also just type a model name below, then re-run init once it's available"));
            if pick.id == "ollama" {
                let pull = tui::ask("  pull qwen2.5:3b now? [y/N]: ").unwrap_or_default();
                if matches!(pull.trim(), "y" | "Y" | "yes" | "YES") {
                    let _ = std::process::Command::new("ollama")
                        .arg("pull")
                        .arg("qwen2.5:3b")
                        .stdout(std::process::Stdio::inherit())
                        .stderr(std::process::Stdio::inherit())
                        .status();
                }
            }
        } else {
            tui::line(&tui::dim("  detected local models:"));
            for (i, m) in found.iter().take(20).enumerate() {
                // Names come from whatever answers on the local port.
                tui::line(&format!(
                    "    {}  {}",
                    tui::bold(&(i + 1).to_string()),
                    tui::sanitize_terminal(m)
                ));
            }
        }
        found
    } else {
        Vec::new()
    };

    // Key, only if the provider needs one and we don't already have it.
    if !pick.env_key.is_empty() && config::load_key(pick.env_key).is_none() {
        tui::line("");
        tui::line(&tui::dim(&format!(
            "  {} needs an API key (stored 0600 in ~/.buildwithnexus/.env.keys)",
            pick.label
        )));
        let key = tui::ask(&format!("  {}: ", pick.env_key))?;
        if !key.trim().is_empty() {
            config::save_key(pick.env_key, key.trim());
        }
    } else if !pick.env_key.is_empty() {
        let shown = config::load_key(pick.env_key)
            .map(|k| config::mask(&k))
            .unwrap_or_default();
        tui::line(&tui::green(&format!(
            "  ✓ {} already set ({})",
            pick.env_key, shown
        )));
    }

    // Model: pick a detected one by number (or name), else type/accept the default.
    let model = if !detected.is_empty() {
        let def = &detected[0];
        match tui::ask(&format!("  model # or name [{def}]: "))
            .as_deref()
            .map(str::trim)
        {
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
    tui::line(&format!(
        "    {}  ask before every file write / command  {}",
        tui::bold("1"),
        tui::dim("(recommended)")
    ));
    tui::line(&format!(
        "    {}  auto-approve everything                {}",
        tui::bold("2"),
        tui::dim("(yolo)")
    ));
    tui::line(&format!(
        "    {}  read-only — never modify anything",
        tui::bold("3")
    ));
    let permission = match tui::ask("  choice [1]: ").as_deref().map(str::trim) {
        Some("2") => "auto",
        Some("3") => "readonly",
        _ => "ask",
    }
    .to_string();

    let saved = save_setup(pick.id, model, permission, base_url);
    tui::line("");
    match saved {
        Ok(settings) => {
            tui::line(&tui::green("  ✓ ready"));
            Some(settings)
        }
        Err(e) => {
            tui::line(&tui::red(&format!("  ✗ settings not saved: {e}")));
            None
        }
    }
}

// Only the four answered keys change: allowed_commands, project_allowed,
// budget, hooks and anything else already in the user file are kept.
fn save_setup(
    provider: &str,
    model: String,
    permission: String,
    base_url: Option<String>,
) -> Result<Settings, String> {
    let mut settings = config::load_user_settings().unwrap_or_default();
    settings.provider = provider.to_string();
    settings.model = model;
    settings.permission = permission;
    settings.base_url = base_url;
    config::save_user_settings(&[
        ("provider", Some(settings.provider.as_str().into())),
        ("model", Some(settings.model.as_str().into())),
        ("permission", Some(settings.permission.as_str().into())),
        ("base_url", settings.base_url.as_deref().map(Into::into)),
    ])?;
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_keeps_existing_user_settings() {
        let _g = config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let h = std::env::temp_dir().join(format!("bwn-onboard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        std::fs::create_dir_all(&h).unwrap();
        std::env::set_var("NEXUS_HOME", &h);
        std::fs::write(
            h.join("settings.json"),
            r#"{"provider":"custom","model":"m","permission":"ask","base_url":"http://old/v1","allowed_commands":["make"],
                "max_budget_usd":5.0,"auto_update":"off","hooks":{"Stop":[]},"custom_key":1}"#,
        )
        .unwrap();

        let s = save_setup("openai", "gpt-4o".into(), "readonly".into(), None).unwrap();
        assert_eq!(s.allowed_commands, ["make"]);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(h.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(v["provider"], "openai");
        assert_eq!(v["model"], "gpt-4o");
        assert_eq!(v["permission"], "readonly");
        assert!(v.get("base_url").is_none());
        assert_eq!(v["allowed_commands"][0], "make");
        assert_eq!(v["max_budget_usd"], 5.0);
        assert_eq!(v["auto_update"], "off");
        assert!(v["hooks"]["Stop"].is_array());
        assert_eq!(v["custom_key"], 1);

        // A malformed file is reported, never overwritten.
        std::fs::write(h.join("settings.json"), "{oops").unwrap();
        assert!(save_setup("openai", "m".into(), "ask".into(), None).is_err());
        assert_eq!(
            std::fs::read_to_string(h.join("settings.json")).unwrap(),
            "{oops"
        );

        std::env::remove_var("NEXUS_HOME");
        let _ = std::fs::remove_dir_all(&h);
    }
}
