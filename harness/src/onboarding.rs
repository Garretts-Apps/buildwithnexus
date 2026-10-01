// First-run walkthrough. Guides the user from nothing to a working provider:
// pick remote-or-local, drop in a key if needed, choose a model and a trust
// level. Nothing is saved until the chosen model has answered, so a finished
// setup always works; Esc (or the end of input) leaves without saving.

use crate::config::{self, Preset, Settings, PRESETS};
use crate::provider::{self, Provider};
use crate::{local, tui};

/// Detected models a list shows before '+N more — type a name'.
pub(crate) const MODEL_LIST_MAX: usize = 20;

/// Why a setup step ended without a value.
enum Exit {
    /// Pick another provider.
    Back,
    /// Esc, Ctrl+C or the end of input: leave without saving anything.
    Stop,
}

fn ask(prompt: &str) -> Result<String, Exit> {
    tui::ask(prompt)
        .map(|s| s.trim().to_string())
        .ok_or(Exit::Stop)
}

pub fn run() -> Option<Settings> {
    tui::clear();
    tui::line(&tui::accent("  buildwithnexus"));
    tui::line(&tui::dim(
        "  a hilariously fast, agentic AI CLI — remote or local models",
    ));
    tui::line("");
    wsl_notice();
    tui::line("");
    let checked = provider::ollama_models_checked(default_ollama_url());
    let ollama_models = checked.clone().unwrap_or_default();
    tui::line(&tui::accent("  Local model check"));
    let found = local_check_line(checked.as_deref());
    tui::line(&if ollama_models.is_empty() {
        tui::dim(&found)
    } else {
        tui::green(&found)
    });
    loop {
        let pick = pick_provider(!ollama_models.is_empty())?;
        match configure(pick, &ollama_models) {
            Ok(settings) => {
                config::scaffold_home();
                return Some(settings);
            }
            Err(Exit::Back) => continue,
            Err(Exit::Stop) => return None,
        }
    }
}

// What the first screen says about Ollama: not answering, answering with no
// models (a different fix), or the models found.
fn local_check_line(found: Option<&[String]>) -> String {
    match found {
        None => "  no Ollama answering on this machine; local models need a running server (Ollama, LM Studio, llama.cpp).".into(),
        Some([]) => "  Ollama is running but has no models — pull one with  ollama pull <model>".into(),
        Some(models) => format!(
            "  found Ollama with {} model{}",
            models.len(),
            if models.len() == 1 { "" } else { "s" }
        ),
    }
}

// Warn the user if the home dir landed on a Windows mount — they should
// set NEXUS_HOME to a native Linux path (e.g. ~/.buildwithnexus in WSL).
fn wsl_notice() {
    if !crate::tools::is_wsl() {
        return;
    }
    let home = config::home();
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

fn default_ollama_url() -> &'static str {
    config::preset("ollama").map_or("http://localhost:11434", |p| p.base_url)
}

// Local presets first, then remote ones; the shown number indexes this list.
fn display_order() -> Vec<&'static Preset> {
    PRESETS
        .iter()
        .filter(|p| p.local)
        .chain(PRESETS.iter().filter(|p| !p.local))
        .collect()
}

fn pick_provider(ollama_found: bool) -> Option<&'static Preset> {
    let shown = display_order();
    tui::line("");
    tui::line("  Pick a model provider:");
    tui::line("");
    tui::line(&tui::dim("  Local"));
    for (i, p) in shown.iter().enumerate() {
        if i > 0 && p.local != shown[i - 1].local {
            tui::line(&tui::dim("  Remote"));
        }
        let tag = match (p.local, p.id) {
            (true, "ollama") if ollama_found => tui::green("local · found, recommended"),
            (true, _) => tui::green("local"),
            (false, _) => tui::blue("remote"),
        };
        tui::line(&format!(
            "  {}  {:<26} {}",
            tui::bold(&(i + 1).to_string()),
            p.label,
            tag
        ));
    }
    tui::line("");
    loop {
        let ans = tui::ask("  provider number or name: ")?;
        let ans = ans.trim();
        // 0 was Ollama's own row before it was folded into the list.
        if ans == "0" && ollama_found {
            return config::preset("ollama");
        }
        if let Ok(n) = ans.parse::<usize>() {
            if n >= 1 && n <= shown.len() {
                return Some(shown[n - 1]);
            }
        }
        if let Some(p) = shown
            .iter()
            .find(|p| p.id.eq_ignore_ascii_case(ans) || p.label.eq_ignore_ascii_case(ans))
        {
            return Some(p);
        }
        tui::line(&tui::red("  enter a number or provider name from the list"));
    }
}

// Every step for one provider, ending in a probe that has to succeed before
// anything is written.
fn configure(pick: &'static Preset, ollama_found: &[String]) -> Result<Settings, Exit> {
    let mut base_url = None;
    if pick.local {
        let u = ask(&format!("  endpoint [{}]: ", pick.base_url))?;
        if !u.is_empty() {
            base_url = Some(u);
        }
    }
    // A gateway that wants a key says so before the model is chosen.
    let mut wants_key = !pick.local;
    let detected = if pick.local {
        detect_models(pick, &mut base_url, ollama_found, &mut wants_key)?
    } else {
        Vec::new()
    };
    let name = key_name(pick);
    let url_now = |b: &Option<String>| b.clone().unwrap_or_else(|| pick.base_url.to_string());
    let stored_for = |url: &str| {
        if pick.id == "custom" {
            config::load_custom_key(url)
        } else {
            config::load_key(name)
        }
    };
    let mut stored = stored_for(&url_now(&base_url));
    // A key typed here is saved only once the model has answered with it.
    let mut key = if wants_key {
        choose_key(pick, stored.as_deref(), &url_now(&base_url))?
    } else {
        None
    };
    let mut key_origin = config::endpoint_origin(&url_now(&base_url));
    // A gateway that lists its models only to a caller with its key: list
    // them now that there is one, so the default is a model it serves.
    let mut detected = detected;
    if pick.id == "custom" && detected.is_empty() {
        let url = url_now(&base_url);
        if let Some(k) = key.as_deref().or(stored.as_deref()) {
            if plain_http_key_refusal(&url).is_none() {
                detected = provider::openai_models_keyed(&url, Some(k)).unwrap_or_default();
                if !detected.is_empty() {
                    list_models(&detected);
                }
            }
        }
    }
    let mut model = choose_model(pick, &detected)?;

    loop {
        // A custom endpoint's key belongs to its address: one typed for
        // another address is not sent to this one.
        let origin = config::endpoint_origin(&url_now(&base_url));
        if pick.id == "custom" && origin != key_origin {
            key = None;
            stored = stored_for(&url_now(&base_url));
            key_origin = origin;
        }
        let settings = Settings {
            provider: pick.id.to_string(),
            model: model.clone(),
            base_url: base_url.clone(),
            ..Default::default()
        };
        let url = base_url.as_deref().unwrap_or(pick.base_url);
        tui::line(&tui::dim(&format!(
            "  checking {} at {}…",
            tui::sanitize_terminal(&model),
            tui::sanitize_terminal(&host_of(url))
        )));
        let fail = match check(&settings, key.as_deref()) {
            Ok(served) => {
                if let Some(m) = served {
                    model = m;
                }
                break;
            }
            Err(f) => f,
        };
        match fail {
            // The llama.cpp and LM Studio presets send no key.
            Fail::KeyRejected(code) if name.is_empty() => {
                tui::line(&tui::yellow(&format!(
                    "  the server wants an API key (HTTP {code}) — pick 'OpenAI-compatible endpoint' to give it one"
                )));
                retry(&mut base_url, pick)?;
            }
            Fail::KeyRejected(code) => {
                if let Some(why) = plain_http_key_refusal(url).filter(|_| pick.id == "custom") {
                    tui::line(&tui::yellow(&format!(
                        "  the endpoint wants an API key (HTTP {code}) — {why}"
                    )));
                    retry(&mut base_url, pick)?;
                    continue;
                }
                if key.is_none() && config::key_from_env(name) {
                    tui::line(&tui::red(&format!(
                        "  {name} from your environment was rejected (HTTP {code}) — fix or unset it, then run setup again"
                    )));
                    return Err(Exit::Stop);
                }
                if key.is_none() && stored.is_none() {
                    tui::line(&tui::yellow(&format!(
                        "  the endpoint wants an API key (HTTP {code}) — paste it, or Esc to stop"
                    )));
                } else {
                    tui::line(&tui::red(&format!(
                        "  the key was rejected (HTTP {code}) — paste it again, or Esc to stop"
                    )));
                }
                key = if pick.id == "custom" {
                    custom_key(&url_now(&base_url))?
                } else {
                    Some(ask_key(pick)?)
                };
            }
            Fail::ModelMissing => {
                model = replace_model(pick, url, &model)?;
            }
            Fail::Unreachable => {
                tui::line(&tui::yellow(&format!(
                    "  nothing answered at {}",
                    tui::sanitize_terminal(url)
                )));
                retry(&mut base_url, pick)?;
            }
            Fail::Other(e) => {
                tui::line(&tui::red(&format!("  ✗ {}", tui::sanitize_terminal(&e))));
                retry(&mut base_url, pick)?;
            }
        }
    }

    let permission = choose_permission()?;
    let url = url_now(&base_url);
    let slot = key_slot(pick, &url);
    if let Some(k) = &key {
        if pick.id == "custom" {
            config::save_custom_key(&url, k);
        } else {
            config::save_key(&slot, k);
        }
    }
    if let Some(k) = key.as_deref().or(stored.as_deref()) {
        config::record_key_check(&slot, k, true);
    }
    tui::line("");
    match save_setup(pick.id, model.clone(), permission, base_url) {
        Ok(settings) => {
            tui::line(&tui::green(&format!(
                "  ✓ ready — {} on {}",
                tui::sanitize_terminal(&model),
                tui::sanitize_terminal(&provider_label(pick.id, &url))
            )));
            Ok(settings)
        }
        Err(e) => {
            tui::line(&tui::red(&format!("  ✗ settings not saved: {e}")));
            Err(Exit::Stop)
        }
    }
}

// After a failed check: Enter tries again, a URL replaces the address, `b`
// goes back to the provider list, Esc stops.
fn retry(base_url: &mut Option<String>, pick: &Preset) -> Result<(), Exit> {
    let prompt = if pick.local {
        "  Enter to try again · a new address · b for another provider · Esc to stop: "
    } else {
        "  Enter to try again · b for another provider · Esc to stop: "
    };
    let ans = ask(prompt)?;
    if ans.eq_ignore_ascii_case("b") || ans.eq_ignore_ascii_case("back") {
        return Err(Exit::Back);
    }
    if pick.local && ans.contains("://") {
        *base_url = Some(ans);
    }
    Ok(())
}

// For local presets: list what the server has, or say exactly why there is
// nothing to list (no server, an empty server, a server that wants a key).
fn detect_models(
    pick: &Preset,
    base_url: &mut Option<String>,
    ollama_found: &[String],
    wants_key: &mut bool,
) -> Result<Vec<String>, Exit> {
    loop {
        let url = base_url
            .clone()
            .unwrap_or_else(|| pick.base_url.to_string());
        let shown_url = tui::sanitize_terminal(&url).into_owned();
        tui::line("");
        if pick.id == "ollama" {
            let found = if url == default_ollama_url() && !ollama_found.is_empty() {
                ollama_found.to_vec()
            } else {
                provider::ollama_models(&url)
            };
            if !found.is_empty() {
                list_models(&found);
                return Ok(found);
            }
            let starter = pick.default_model;
            let root = url.trim_end_matches('/').trim_end_matches("/v1");
            if http_status(&format!("{root}/api/tags")).is_none() {
                tui::line(&tui::yellow(&format!("  nothing answered at {shown_url}")));
                if crate::tools::find_on_path("ollama").is_none() {
                    tui::line(&tui::dim(
                        "    Ollama is not installed — https://ollama.com",
                    ));
                } else {
                    tui::line(&tui::dim(
                        "    Ollama is installed; start it with  ollama serve",
                    ));
                }
            } else {
                tui::line(&tui::yellow(&format!(
                    "  Ollama is running but has no models — ollama pull {starter}"
                )));
                let pull = ask(&format!("  pull {starter} now? [y/N]: "))?;
                if matches!(pull.to_ascii_lowercase().as_str(), "y" | "yes") && pull_model(starter)
                {
                    continue;
                }
            }
        } else {
            let found = provider::openai_models(&url);
            if !found.is_empty() {
                list_models(&found);
                return Ok(found);
            }
            match http_status(&format!("{}/models", url.trim_end_matches('/'))) {
                None => {
                    tui::line(&tui::yellow(&format!("  nothing answered at {shown_url}")));
                    for hint in start_hints(pick.id) {
                        tui::line(&tui::dim(&format!("    {hint}")));
                    }
                }
                // A gateway that wants a key: the key question comes next.
                Some(401 | 403) if pick.id == "custom" => {
                    tui::line(&tui::dim("  the endpoint answers and wants an API key"));
                    *wants_key = true;
                    return Ok(Vec::new());
                }
                Some(401 | 403) => tui::line(&tui::yellow(&format!(
                    "  {shown_url} wants an API key — pick 'OpenAI-compatible endpoint' to give it one"
                ))),
                Some(_) if pick.id == "lmstudio" => tui::line(&tui::yellow(
                    "  LM Studio is running but has no model loaded — load one in LM Studio, or run  lms load <model>",
                )),
                // It answers but lists nothing: the model name typed next is
                // checked against it.
                Some(_) => return Ok(Vec::new()),
            }
        }
        retry(base_url, pick)?;
    }
}

// What to start for each local server kind when nothing answered.
fn start_hints(id: &str) -> Vec<String> {
    match id {
        "llamacpp" => {
            let mut hints = Vec::new();
            if crate::tools::find_on_path("llama-server").is_none() {
                hints.push(
                    "llama-server is not installed — https://github.com/ggml-org/llama.cpp".into(),
                );
            }
            let ggufs = local::scan_gguf();
            match ggufs.first() {
                Some(g) => hints.push(format!(
                    "start it:  llama-server -m {} --port 8080  ({} GGUF file{} in {})",
                    tui::sanitize_terminal(g),
                    ggufs.len(),
                    if ggufs.len() == 1 { "" } else { "s" },
                    local::models_dir().display()
                )),
                None => hints.push("start it:  llama-server -m <model.gguf> --port 8080".into()),
            }
            hints
        }
        "lmstudio" => vec!["start LM Studio's local server (or run  lms server start)".into()],
        _ => vec!["check the address, and that the server is running".into()],
    }
}

// Status of a GET, or None when nothing answered at all.
fn http_status(url: &str) -> Option<u16> {
    match crate::net::shared()
        .get(url)
        .timeout(std::time::Duration::from_secs(2))
        .call()
    {
        Ok(r) => Some(r.status()),
        Err(ureq::Error::Status(code, _)) => Some(code),
        Err(_) => None,
    }
}

fn pull_model(model: &str) -> bool {
    let status = std::process::Command::new("ollama")
        .arg("pull")
        .arg(model)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();
    match status {
        Ok(s) if s.success() => true,
        Ok(_) => {
            tui::line(&tui::red(&format!("  ollama pull {model} failed")));
            false
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tui::line(&tui::yellow(
                "  Ollama is not installed — https://ollama.com (or run the pull where Ollama runs)",
            ));
            false
        }
        Err(e) => {
            tui::line(&tui::red(&format!("  could not run ollama: {e}")));
            false
        }
    }
}

fn list_models(found: &[String]) {
    tui::line(&tui::dim("  detected models:"));
    for (i, m) in found.iter().take(MODEL_LIST_MAX).enumerate() {
        // Names come from whatever answers on the local port.
        tui::line(&format!(
            "    {}  {}",
            tui::bold(&(i + 1).to_string()),
            tui::sanitize_terminal(m)
        ));
    }
    if found.len() > MODEL_LIST_MAX {
        tui::line(&tui::dim(&format!(
            "    {}",
            more_line(found.len() - MODEL_LIST_MAX)
        )));
    }
}

/// The last row of a cut model list.
pub(crate) fn more_line(hidden: usize) -> String {
    format!("+{hidden} more — type a name")
}

/// The key variable a preset uses: CUSTOM_API_KEY for the custom endpoint,
/// none for the keyless local servers.
pub(crate) fn key_name(p: &Preset) -> &'static str {
    if p.id == "custom" {
        config::CUSTOM_KEY
    } else {
        p.env_key
    }
}

/// Where a preset's key is saved: its variable, or for the custom endpoint
/// one entry per endpoint (`config::custom_key_name`).
pub(crate) fn key_slot(p: &Preset, base_url: &str) -> String {
    if p.id == "custom" {
        config::custom_key_name(base_url)
    } else {
        p.env_key.to_string()
    }
}

/// What a key question was answered with.
#[derive(Debug, PartialEq)]
pub(crate) enum KeyAnswer {
    Key(String),
    /// Enter on an empty line.
    Empty,
    /// Esc, Ctrl+C, the end of input, or a `/command`.
    Cancel,
}

// Longest answer taken as a key; the longest real tokens (signed JWTs) are
// a couple of thousand characters.
const MAX_KEY_LEN: usize = 4096;

/// Why `s` (trimmed, not empty) cannot be an API key, or None when it could
/// be one: a question, a `/command`, a menu number or a provider name typed
/// at the key question is refused here instead of being sent as the key.
pub(crate) fn not_a_key(s: &str) -> Option<&'static str> {
    if s.starts_with('/') {
        Some("it is a command")
    } else if s.chars().any(char::is_whitespace) {
        Some("it has spaces")
    } else if s.chars().all(|c| c.is_ascii_digit()) {
        Some("it is a number, like a menu choice")
    } else if s.len() > MAX_KEY_LEN {
        Some("it is far longer than any key")
    } else if s.chars().any(|c| !c.is_ascii_graphic()) {
        Some("it has characters keys do not use")
    } else if PRESETS.iter().any(|p| p.id.eq_ignore_ascii_case(s)) {
        Some("it is a provider name")
    } else {
        None
    }
}

/// The key question of setup, /login and /model: masked, and asked again
/// until the answer could be a key, so nothing else typed there is sent to
/// the provider. A `/command` closes the question: in a session it runs
/// next, during setup it stops setup.
pub(crate) fn read_key(prompt: &str) -> KeyAnswer {
    loop {
        let Some(k) = tui::ask_secret(prompt) else {
            return KeyAnswer::Cancel;
        };
        let k = k.trim();
        if k.is_empty() {
            return KeyAnswer::Empty;
        }
        if k.starts_with('/') {
            leave_for_command(k);
            return KeyAnswer::Cancel;
        }
        match not_a_key(k) {
            None => return KeyAnswer::Key(k.to_string()),
            Some(why) => tui::line(&tui::yellow(&format!(
                "  that does not look like a key ({why}) — not sent. You are still at the key question: paste the key, or Esc to cancel"
            ))),
        }
    }
}

// A `/command` typed at a key question: the question closes with nothing
// sent or saved. In a session the command runs next; setup has no commands.
fn leave_for_command(cmd: &str) {
    if tui::is_raw() {
        tui::line(&tui::dim("  key question closed — nothing sent or saved"));
        tui::queue_message(cmd);
    } else {
        tui::line(&tui::dim(&format!(
            "  {} is a command, not a key — stopped here, nothing sent or saved",
            tui::sanitize_terminal(cmd)
        )));
    }
}

/// Before asking for a key: None when a key may be sent to `url`, or the
/// reason it cannot, with what to do instead. A key never travels over
/// plain http to another machine.
pub(crate) fn plain_http_key_refusal(url: &str) -> Option<String> {
    // The same test build_provider_with_key applies when the key is sent.
    if url.starts_with("https://") || crate::is_loopback_url(url) {
        return None;
    }
    let host = host_of(url);
    let port = host.rsplit_once(':').map_or("80", |(_, p)| p);
    Some(format!(
        "{} is plain http on another machine, so no API key is sent there. Use https://, \
         or reach it through a loopback tunnel (ssh -L {port}:localhost:{port} <that machine>, \
         then http://127.0.0.1:{port}/v1), or run the server without a key",
        tui::sanitize_terminal(&host)
    ))
}

/// The key for a custom endpoint that has none saved, asked for that
/// endpoint alone. A key an earlier version saved for no endpoint in
/// particular is offered first. Over plain http to another machine no key
/// is asked: it could not be sent.
pub(crate) fn ask_custom_key(url: &str) -> KeyAnswer {
    if let Some(why) = plain_http_key_refusal(url) {
        tui::line(&tui::dim(&format!("  {why}")));
        return KeyAnswer::Empty;
    }
    let host = tui::sanitize_terminal(&host_of(url)).into_owned();
    if let Some(old) = config::unbound_custom_key() {
        tui::line(&tui::dim(&format!(
            "  a CUSTOM_API_KEY saved by an earlier version ({}) is not tied to an endpoint",
            config::mask(&old)
        )));
        let Some(a) = tui::ask(&format!("  send it to {host}? [y/N]: ")) else {
            return KeyAnswer::Cancel;
        };
        if matches!(a.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            return KeyAnswer::Key(old);
        }
    }
    read_key("  API key for this endpoint (Enter for none): ")
}

// A key typed now (Some) or the stored one (None).
fn choose_key(pick: &Preset, stored: Option<&str>, url: &str) -> Result<Option<String>, Exit> {
    let name = key_name(pick);
    if name.is_empty() {
        return Ok(None);
    }
    if let Some(k) = stored {
        let from = if config::key_from_env(name) {
            ", from the environment"
        } else {
            ""
        };
        tui::line(&tui::green(&format!(
            "  ✓ {name} already set ({}{from})",
            config::mask(k)
        )));
        return Ok(None);
    }
    if pick.id == "custom" {
        return custom_key(url);
    }
    tui::line("");
    tui::line(&tui::dim(&format!(
        "  {} needs an API key; it is checked before it is saved (0600 in ~/.buildwithnexus/.env.keys)",
        pick.label
    )));
    ask_key(pick).map(Some)
}

// Setup's custom-endpoint key: Some(key), None for no key; Esc stops.
fn custom_key(url: &str) -> Result<Option<String>, Exit> {
    match ask_custom_key(url) {
        KeyAnswer::Key(k) => Ok(Some(k)),
        KeyAnswer::Empty => Ok(None),
        KeyAnswer::Cancel => Err(Exit::Stop),
    }
}

// Asks until a key is pasted; an empty answer is refused, `b` goes back.
fn ask_key(pick: &Preset) -> Result<String, Exit> {
    let name = key_name(pick);
    loop {
        match read_key(&format!("  {name}: ")) {
            KeyAnswer::Cancel => return Err(Exit::Stop),
            KeyAnswer::Key(k) if k.eq_ignore_ascii_case("b") => return Err(Exit::Back),
            KeyAnswer::Key(k) => return Ok(k),
            KeyAnswer::Empty => tui::line(&tui::yellow(
                "  a key is required for this provider — or pick a local one (b)",
            )),
        }
    }
}

fn choose_model(pick: &Preset, detected: &[String]) -> Result<String, Exit> {
    if let Some(def) = detected.first() {
        let s = ask(&format!(
            "  model # or name [{}]: ",
            tui::sanitize_terminal(def)
        ))?;
        if s.is_empty() {
            return Ok(def.clone());
        }
        return Ok(match s.parse::<usize>() {
            Ok(n) if n >= 1 && n <= detected.len() => detected[n - 1].clone(),
            _ => s,
        });
    }
    let s = ask(&format!("  model [{}]: ", pick.default_model))?;
    Ok(if s.is_empty() {
        pick.default_model.to_string()
    } else {
        s
    })
}

// The server answered but not for `model`: name what it does serve.
fn replace_model(pick: &Preset, url: &str, model: &str) -> Result<String, Exit> {
    let served = if pick.id == "ollama" {
        provider::ollama_models(url)
    } else {
        provider::openai_models(url)
    };
    let model = tui::sanitize_terminal(model);
    if pick.id == "ollama" {
        tui::line(&tui::yellow(&format!(
            "  '{model}' isn't pulled on this Ollama — ollama pull {model}"
        )));
    } else {
        tui::line(&tui::yellow(&format!(
            "  '{model}' isn't served at {}",
            tui::sanitize_terminal(&host_of(url))
        )));
    }
    if !served.is_empty() {
        list_models(&served);
    }
    choose_model(pick, &served)
}

fn choose_permission() -> Result<String, Exit> {
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
    // Numbered as in /permissions.
    tui::line(&format!(
        "    {}  accept edits — file edits in this project without asking; commands still ask",
        tui::bold("4")
    ));
    loop {
        match permission_choice(&ask("  choice [1]: ")?) {
            Some(p) => return Ok(p.to_string()),
            None => tui::line(&tui::yellow("  choose 1, 2, 3 or 4 (Enter for 1)")),
        }
    }
}

// A setup permission answer: a row number or the mode's name.
fn permission_choice(answer: &str) -> Option<&'static str> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "ask" => Some("ask"),
        "2" | "auto" => Some("auto"),
        "3" | "readonly" | "read-only" => Some("readonly"),
        "4" | "accept-edits" | "accept edits" => Some("accept-edits"),
        _ => None,
    }
}

/// How a check of a provider failed, so the next step can say what to do.
#[derive(Debug, PartialEq)]
pub(crate) enum Fail {
    /// The provider refused the key (HTTP 401 or 403).
    KeyRejected(u16),
    /// The server answered, but not for this model.
    ModelMissing,
    /// Nothing answered at the address.
    Unreachable,
    /// Anything else, in the provider's words.
    Other(String),
}

impl Fail {
    pub(crate) fn from_error(e: &str) -> Fail {
        let lower = e.to_ascii_lowercase();
        // "HTTP 401: …" from the request path, or "(HTTP 401)" in friendlier
        // wording around it.
        let status = lower
            .find("http ")
            .and_then(|i| lower.get(i + 5..i + 8))
            .and_then(|c| c.parse::<u16>().ok());
        match status {
            Some(c @ (401 | 403)) => Fail::KeyRejected(c),
            _ if lower.contains("key was rejected") => Fail::KeyRejected(401),
            Some(404) => Fail::ModelMissing,
            Some(_)
                if lower.contains("model")
                    && (lower.contains("not found") || lower.contains("does not exist")) =>
            {
                Fail::ModelMissing
            }
            // A certificate failure carries its own fix on a second line.
            None if lower.starts_with("nothing is answering")
                || (lower.starts_with("connection failed") && !e.contains('\n')) =>
            {
                Fail::Unreachable
            }
            _ => Fail::Other(e.to_string()),
        }
    }
}

/// Proves the model in `settings` answers, using `key` in place of the saved
/// key when given. Ollama is checked by its installed list (a probe could
/// cold-load a large model for minutes); everything else by a one-token
/// request. Ok(Some(name)) when the server only answers under another name.
pub(crate) fn check(settings: &Settings, key: Option<&str>) -> Result<Option<String>, Fail> {
    let p = crate::build_provider_with_key(settings, key).map_err(Fail::Other)?;
    if settings.provider == "ollama" {
        let root = p.base_url.trim_end_matches('/').trim_end_matches("/v1");
        if http_status(&format!("{root}/api/tags")).is_none() {
            return Err(Fail::Unreachable);
        }
        let have = provider::ollama_models(&p.base_url)
            .iter()
            .any(|m| *m == p.model || m.split(':').next() == Some(p.model.as_str()));
        return if have {
            Ok(None)
        } else {
            Err(Fail::ModelMissing)
        };
    }
    provider::validate(&p).map_err(|e| Fail::from_error(&e))
}

/// `/login` and `buildwithnexus login`: a new key for the provider in
/// `settings`, checked against its model and endpoint, and saved only once it
/// is accepted. Returns the provider built with the new key.
pub(crate) fn login(settings: &Settings) -> Option<Provider> {
    let Some(preset) = config::preset(&settings.provider) else {
        tui::line(&tui::red(&format!(
            "  {}",
            crate::unknown_provider_msg(&settings.provider)
        )));
        return None;
    };
    let name = key_name(preset);
    let url = settings.base_url.as_deref().unwrap_or(preset.base_url);
    let slot = key_slot(preset, url);
    let model = if settings.model.is_empty() {
        preset.default_model
    } else {
        settings.model.as_str()
    };
    if name.is_empty() {
        tui::line(&tui::dim(&format!(
            "  {} needs no key — /model switches to a provider that does",
            preset.label
        )));
        return None;
    }
    // A custom endpoint's key is for that endpoint alone.
    let label = if preset.id == "custom" {
        tui::sanitize_terminal(&config::endpoint_origin(url)).into_owned()
    } else {
        preset.label.to_string()
    };
    tui::line(&tui::dim(&format!(
        "  a new {name} for {label}: it is checked with {} before it is saved · Esc cancels",
        tui::sanitize_terminal(model)
    )));
    loop {
        let key = match read_key(&format!("  {name}: ")) {
            KeyAnswer::Key(k) => k,
            KeyAnswer::Empty => {
                tui::line(&tui::yellow(
                    "  a key is required — paste it, or Esc to cancel",
                ));
                continue;
            }
            KeyAnswer::Cancel => {
                tui::line(&tui::dim("  /login cancelled — nothing saved"));
                return None;
            }
        };
        let key = key.as_str();
        match check(settings, Some(key)) {
            Ok(served) => {
                if preset.id == "custom" {
                    config::save_custom_key(url, key);
                } else {
                    config::save_key(&slot, key);
                }
                config::record_key_check(&slot, key, true);
                tui::line(&tui::green(&format!("  ✓ {name} saved — {label} answered")));
                if config::key_from_env(name) {
                    tui::line(&tui::yellow(&format!(
                        "  {name} is also set in your environment, which wins at the next launch — unset it to keep using this key"
                    )));
                }
                let mut s = settings.clone();
                if let Some(m) = served {
                    s.model = m;
                }
                return crate::build_provider_with_key(&s, Some(key)).ok();
            }
            Err(Fail::KeyRejected(code)) => tui::line(&tui::red(&format!(
                "  ✗ rejected (HTTP {code}) — not saved. Paste another key, or Esc to cancel"
            ))),
            Err(f) => {
                tui::line(&tui::red(&format!(
                    "  ✗ {} — not saved",
                    tui::sanitize_terminal(&f.message())
                )));
                return None;
            }
        }
    }
}

impl Fail {
    fn message(&self) -> String {
        match self {
            Fail::KeyRejected(code) => format!("the key was rejected (HTTP {code})"),
            Fail::ModelMissing => "the model isn't served there".into(),
            Fail::Unreachable => "nothing answered".into(),
            Fail::Other(e) => e.clone(),
        }
    }
}

/// "LM Studio (localhost:1234)": the preset's name and the host it talks to,
/// for every line that says what is answering.
pub(crate) fn provider_label(preset_id: &str, base_url: &str) -> String {
    let name = match config::preset(preset_id) {
        Some(p) if p.id == "custom" => "custom endpoint",
        Some(p) => p.label.split(" (").next().unwrap_or(p.label),
        None => preset_id,
    };
    format!("{name} ({})", tui::sanitize_terminal(&host_of(base_url)))
}

// host[:port] of a URL, without scheme, path or credentials: the host the
// request goes to, as the key for it is filed (`config::endpoint_origin`).
fn host_of(url: &str) -> String {
    let origin = config::endpoint_origin(url);
    match origin.split_once("://") {
        Some((_, host)) => host.to_string(),
        None => origin,
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
    let mut changes = vec![
        ("provider", Some(settings.provider.as_str().into())),
        ("model", Some(settings.model.as_str().into())),
        ("permission", Some(settings.permission.as_str().into())),
        ("base_url", settings.base_url.as_deref().map(Into::into)),
    ];
    // /model returns to this provider at the address chosen here.
    let before = settings.endpoints.clone();
    match &settings.base_url {
        Some(u) => settings.endpoints.insert(provider.to_string(), u.clone()),
        None => settings.endpoints.remove(provider),
    };
    if settings.endpoints != before {
        changes.push(("endpoints", serde_json::to_value(&settings.endpoints).ok()));
    }
    config::save_user_settings(&changes)?;
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_could_be_a_key_is_sent_as_one() {
        for typed in [
            "what does this project do?",
            "/init",
            "/model",
            "5",
            "12",
            "anthropic",
            "OpenAI",
            "hello, what can you do?",
            "sk-ant café",
            &"k".repeat(5000),
        ] {
            assert!(not_a_key(typed).is_some(), "{typed}");
        }
        for key in [
            "sk-ant-api03-abc_DEF-123",
            "sk-proj-Zx9",
            "gsk_0123456789",
            "hf_abcdef",
            "lm-studio-token",
        ] {
            assert_eq!(not_a_key(key), None, "{key}");
        }
        assert_eq!(not_a_key("hello there"), Some("it has spaces"));
    }

    #[test]
    fn setup_offers_every_permission_mode_and_refuses_the_rest() {
        assert_eq!(permission_choice(""), Some("ask"));
        assert_eq!(permission_choice("2"), Some("auto"));
        assert_eq!(permission_choice("3"), Some("readonly"));
        assert_eq!(permission_choice("4"), Some("accept-edits"));
        assert_eq!(permission_choice(" Accept-Edits "), Some("accept-edits"));
        assert_eq!(permission_choice("5"), None);
        assert_eq!(permission_choice("yolo"), None);
    }

    #[test]
    fn no_key_is_asked_for_plain_http_to_another_machine() {
        let why = plain_http_key_refusal("http://192.168.50.10:1234/v1").unwrap();
        assert!(
            why.contains("ssh -L 1234:localhost:1234")
                && why.contains("http://127.0.0.1:1234/v1")
                && why.contains("https://"),
            "{why}"
        );
        assert_eq!(plain_http_key_refusal("https://gw.example.com/v1"), None);
        assert_eq!(plain_http_key_refusal("http://127.0.0.1:8080/v1"), None);
        assert_eq!(plain_http_key_refusal("http://localhost:1234/v1"), None);
    }

    #[test]
    fn the_first_screen_tells_an_empty_ollama_from_a_missing_one() {
        assert!(local_check_line(None).contains("no Ollama answering"));
        let empty = local_check_line(Some(&[]));
        assert!(empty.contains("running but has no models"), "{empty}");
        let two = local_check_line(Some(&["a".to_string(), "b".to_string()]));
        assert!(two.contains("found Ollama with 2 models"), "{two}");
    }

    #[test]
    fn the_host_named_is_the_one_the_request_goes_to() {
        assert_eq!(
            host_of("http://a.example.com\\@b.example.com/v1"),
            "a.example.com"
        );
        assert_eq!(host_of("http://u:p@localhost:1234/v1"), "localhost:1234");
        assert_eq!(
            provider_label("custom", "https://gw.example.com/v1"),
            "custom endpoint (gw.example.com)"
        );
    }

    #[test]
    fn probe_errors_say_what_to_do_next() {
        let f = Fail::from_error;
        assert_eq!(
            f(r#"HTTP 401: {"error":{"message":"invalid api key"}}"#),
            Fail::KeyRejected(401)
        );
        assert_eq!(f("HTTP 403: forbidden"), Fail::KeyRejected(403));
        // Friendlier wording around the status still reads as a bad key.
        assert_eq!(
            f("the API key was rejected by the provider — /login to replace it"),
            Fail::KeyRejected(401)
        );
        assert_eq!(f(r#"HTTP 404: {"error":"nope"}"#), Fail::ModelMissing);
        assert_eq!(
            f("HTTP 400: The model `qwen` does not exist"),
            Fail::ModelMissing
        );
        assert_eq!(
            f("nothing is answering at http://127.0.0.1:19111 — is the model server running?"),
            Fail::Unreachable
        );
        assert_eq!(
            f("connection failed: http://10.0.0.9:8000/v1/chat/completions: Connection refused"),
            Fail::Unreachable
        );
        // A certificate failure keeps the fix it carries on its second line.
        assert!(matches!(
            f("connection failed: invalid peer certificate: UnknownIssuer\n  set SSL_CERT_FILE"),
            Fail::Other(_)
        ));
        assert!(matches!(f("HTTP 500: boom"), Fail::Other(_)));
    }

    #[test]
    fn labels_name_the_preset_and_the_host_it_talks_to() {
        assert_eq!(
            provider_label("lmstudio", "http://localhost:1234/v1"),
            "LM Studio (localhost:1234)"
        );
        assert_eq!(
            provider_label("ollama", "http://192.168.50.10:11434"),
            "Ollama (192.168.50.10:11434)"
        );
        assert_eq!(
            provider_label("anthropic", "https://api.anthropic.com"),
            "Anthropic (api.anthropic.com)"
        );
        // A custom endpoint is never called "OpenAI", and credentials in the
        // URL never reach the screen.
        assert_eq!(
            provider_label("custom", "https://user:secret@gw.corp:8443/v1"),
            "custom endpoint (gw.corp:8443)"
        );
        assert_eq!(more_line(28), "+28 more — type a name");
        assert_eq!(
            key_name(config::preset("custom").unwrap()),
            "CUSTOM_API_KEY"
        );
        assert_eq!(key_name(config::preset("ollama").unwrap()), "");
    }

    #[test]
    fn the_provider_list_shows_each_preset_once() {
        let shown = display_order();
        assert_eq!(shown.len(), PRESETS.len());
        assert_eq!(shown.iter().filter(|p| p.id == "ollama").count(), 1);
        // Local servers first, then the hosted ones.
        let first_remote = shown.iter().position(|p| !p.local).unwrap();
        assert!(shown[first_remote..].iter().all(|p| !p.local));
    }

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
        assert!(v.get("endpoints").is_none());

        // An address chosen in setup is where /model returns to later.
        let s = save_setup(
            "ollama",
            "m".into(),
            "ask".into(),
            Some("http://gpu-box:11434".into()),
        )
        .unwrap();
        assert_eq!(s.endpoints["ollama"], "http://gpu-box:11434");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(h.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(v["endpoints"]["ollama"], "http://gpu-box:11434");

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
