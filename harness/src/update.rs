// Self-update, inside the binary (the npm wrapper is deliberately inert: no
// network, no scripts — so update checking lives here). Policy comes from the
// `auto_update` setting: "off" (no check, no notices), "notify" (daily
// registry check, one-line startup notice, never installs — the default), or
// "install" (daily check + silent `npm install -g`, notice on next launch).
// BWN_NO_AUTO_UPDATE=1 caps "install" back to "notify" for back-compat.
// Everything here is best-effort and must never affect the session.

use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PKG: &str = "buildwithnexus";
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

fn state_path() -> std::path::PathBuf {
    crate::config::home().join("update-state.json")
}

fn read_state() -> serde_json::Value {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

fn write_state(patch: &[(&str, serde_json::Value)]) {
    let mut v = read_state();
    if let Some(obj) = v.as_object_mut() {
        for (k, val) in patch {
            obj.insert(k.to_string(), val.clone());
        }
    }
    let _ = std::fs::create_dir_all(crate::config::home());
    let _ = std::fs::write(state_path(), v.to_string());
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// a strictly newer than b, numeric semver segments only; pre-releases never win.
pub fn newer(a: &str, b: &str) -> bool {
    if a.contains('-') {
        return false;
    }
    let parse = |s: &str| -> Vec<u64> {
        s.split('-')
            .next()
            .unwrap_or("")
            .split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (pa, pb) = (parse(a), parse(b));
    for i in 0..3 {
        let (x, y) = (
            pa.get(i).copied().unwrap_or(0),
            pb.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

// Effective policy: the `auto_update` setting, with BWN_NO_AUTO_UPDATE=1
// capping "install" back to "notify", unknown values treated as "notify", and
// "install" only honored for npm installs — `npm install -g` cannot update a
// cargo or source build, so those are capped to "notify" as well.
fn effective_policy(setting: &str) -> &'static str {
    let env_cap = std::env::var("BWN_NO_AUTO_UPDATE").is_ok_and(|v| v == "1");
    let npm = std::env::current_exe()
        .map(|p| installed_via_npm(&p))
        .unwrap_or(false);
    resolve_policy(setting, env_cap, npm)
}

fn resolve_policy(setting: &str, env_cap: bool, npm_install: bool) -> &'static str {
    match setting {
        "off" => "off",
        "install" if !env_cap && npm_install => "install",
        _ => "notify",
    }
}

// The npm launcher always resolves the binary from a `node_modules` tree (the
// per-platform package or the wrapper's own `bin/`); cargo installs live in
// `~/.cargo/bin` and source builds in `target/`, neither of which npm can
// replace.
fn installed_via_npm(exe: &std::path::Path) -> bool {
    exe.components()
        .any(|c| c.as_os_str().to_str() == Some("node_modules"))
}

// One-line startup notice when a background update landed (or a newer version
// was seen and installs are off). Consumes the notice so it prints once.
pub fn startup_notice(policy: &str) -> Option<String> {
    let policy = effective_policy(policy);
    if policy == "off" {
        return None;
    }
    let st = read_state();
    let updated = st["updatedTo"].as_str().unwrap_or("");
    if !updated.is_empty()
        && newer(updated, crate::VERSION)
        && st["noticeShownFor"].as_str() != Some(updated)
    {
        write_state(&[("noticeShownFor", serde_json::json!(updated))]);
        return Some(format!(
            "  ✓ updated to v{updated} in the background — restart to use it"
        ));
    }
    let latest = st["latestSeen"].as_str().unwrap_or("");
    if policy != "install"
        && !latest.is_empty()
        && newer(latest, crate::VERSION)
        && st["noticeShownFor"].as_str() != Some(latest)
    {
        write_state(&[("noticeShownFor", serde_json::json!(latest))]);
        return Some(format!(
            "  ⬆ v{latest} is available — npm install -g {PKG}@latest (or set auto_update: \"install\")"
        ));
    }
    None
}

// Fire-and-forget daily check. Never blocks startup; all failures are silent.
pub fn spawn_check(policy: &str) {
    let policy = effective_policy(policy);
    if policy == "off" {
        return;
    }
    let last = read_state()["lastCheck"].as_u64().unwrap_or(0);
    if now_secs().saturating_sub(last) < CHECK_INTERVAL_SECS {
        return;
    }
    let install = policy == "install";
    std::thread::spawn(move || {
        write_state(&[("lastCheck", serde_json::json!(now_secs()))]);
        let Ok(resp) = ureq::get(&format!("https://registry.npmjs.org/{PKG}/latest"))
            .timeout(Duration::from_secs(10))
            .call()
        else {
            return;
        };
        let Ok(body) = resp.into_json::<serde_json::Value>() else {
            return;
        };
        let Some(latest) = body["version"].as_str() else {
            return;
        };
        write_state(&[("latestSeen", serde_json::json!(latest))]);
        if !newer(latest, crate::VERSION) || !install {
            return;
        }
        let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
        let ok = Command::new(npm)
            .args([
                "install",
                "-g",
                &format!("{PKG}@{latest}"),
                "--no-fund",
                "--no-audit",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            write_state(&[("updatedTo", serde_json::json!(latest))]);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{effective_policy, installed_via_npm, newer, resolve_policy};
    use std::path::Path;

    #[test]
    fn policy_resolution() {
        assert_eq!(resolve_policy("off", false, true), "off");
        assert_eq!(resolve_policy("notify", false, true), "notify");
        assert_eq!(resolve_policy("install", false, true), "install");
        assert_eq!(resolve_policy("bogus", false, true), "notify");
        // BWN_NO_AUTO_UPDATE=1 caps install back to notify.
        assert_eq!(resolve_policy("install", true, true), "notify");
        assert_eq!(resolve_policy("off", true, true), "off");
        // cargo / source installs are never auto-updated.
        assert_eq!(resolve_policy("install", false, false), "notify");
        assert_eq!(resolve_policy("off", false, false), "off");
        // The test binary itself lives under target/, so "install" never
        // resolves to "install" from the real entry point here.
        std::env::remove_var("BWN_NO_AUTO_UPDATE");
        assert_ne!(effective_policy("install"), "install");
    }

    #[test]
    fn npm_install_detection() {
        assert!(installed_via_npm(Path::new(
            "/usr/lib/node_modules/buildwithnexus-linux-x64/bin/buildwithnexus"
        )));
        assert!(installed_via_npm(Path::new(
            "/home/u/.nvm/versions/node/v22/lib/node_modules/buildwithnexus/bin/buildwithnexus"
        )));
        assert!(!installed_via_npm(Path::new(
            "/home/u/.cargo/bin/buildwithnexus"
        )));
        assert!(!installed_via_npm(Path::new(
            "/src/bwn/target/release/buildwithnexus"
        )));
    }

    #[test]
    fn version_comparison() {
        assert!(newer("0.12.1", "0.12.0"));
        assert!(newer("0.13.0", "0.12.9"));
        assert!(newer("1.0.0", "0.99.99"));
        assert!(!newer("0.12.0", "0.12.0"));
        assert!(!newer("0.11.9", "0.12.0"));
        // Pre-releases never auto-install.
        assert!(!newer("0.13.0-beta.1", "0.12.0"));
        // Missing segments count as zero.
        assert!(newer("0.12.1", "0.12"));
        assert!(!newer("0.12", "0.12.0"));
    }
}
