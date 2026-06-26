// First-run walkthrough. Guides the user from nothing to a working provider:
// pick remote-or-local, drop in a key if needed, choose a model and a trust level.

use crate::config::{self, Settings, PRESETS};
use crate::tui;

pub fn run() -> Option<Settings> {
    config::ensure_home();
    tui::clear();
    tui::line(&tui::accent("  buildwithnexus"));
    tui::line(&tui::dim("  a hilariously fast, agentic AI CLI — remote or local models"));
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

    let model = match tui::ask(&format!("  model [{}]: ", pick.default_model)) {
        Some(m) if !m.trim().is_empty() => m.trim().to_string(),
        _ => pick.default_model.to_string(),
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

    let settings = Settings { provider: pick.id.to_string(), model, permission, base_url };
    config::save_settings(&settings);
    tui::line("");
    tui::line(&tui::green("  ✓ ready"));
    Some(settings)
}
