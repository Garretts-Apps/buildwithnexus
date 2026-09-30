// Self-update, inside the binary (the npm wrapper is deliberately inert: no
// network, no scripts — so update checking lives here). Policy comes from the
// `auto_update` setting: "off" (no check, no notices), "notify" (daily
// registry check, one-line startup notice, never installs — the default),
// "install" (daily check + silent `npm install -g` of patch releases within
// the running minor; a new minor or major is only announced), or
// "install-any" (installs any newer release, what "install" did before 0.15).
// BWN_NO_AUTO_UPDATE=1 caps both install policies back to "notify".
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
        .map(|p| installed_via_npm(&p, &crate::config::home()))
        .unwrap_or(false);
    resolve_policy(setting, env_cap, npm)
}

fn resolve_policy(setting: &str, env_cap: bool, npm_install: bool) -> &'static str {
    match setting {
        "off" => "off",
        "install" if !env_cap && npm_install => "install",
        "install-any" if !env_cap && npm_install => "install-any",
        _ => "notify",
    }
}

// The npm launcher runs the binary it downloaded into `<home>/bin/<version>`
// (0.15+), or one from a `node_modules` tree (the per-platform package or,
// before 0.15, the wrapper's own `bin/`); cargo installs live in
// `~/.cargo/bin` and source builds in `target/`, neither of which npm can
// replace.
// current_exe() resolves symlinks while home() does not, so a symlinked home
// is compared resolved too.
fn installed_via_npm(exe: &std::path::Path, home: &std::path::Path) -> bool {
    let bin = home.join("bin");
    exe.starts_with(&bin)
        || std::fs::canonicalize(&bin).is_ok_and(|b| {
            std::fs::canonicalize(exe)
                .unwrap_or_else(|_| exe.to_path_buf())
                .starts_with(b)
        })
        || exe
            .components()
            .any(|c| c.as_os_str().to_str() == Some("node_modules"))
}

// One-line startup notice when a background update landed (or a newer version
// was seen that this policy does not install). Consumes the notice so it
// prints once.
pub fn startup_notice(policy: &str) -> Option<String> {
    let policy = effective_policy(policy);
    let (notice, version) = pending_notice(policy, &read_state(), crate::VERSION)?;
    write_state(&[("noticeShownFor", serde_json::json!(version))]);
    Some(notice)
}

// The notice for `state`, and the version it is about.
fn pending_notice(policy: &str, st: &serde_json::Value, current: &str) -> Option<(String, String)> {
    if policy == "off" {
        return None;
    }
    let shown = st["noticeShownFor"].as_str();
    let updated = st["updatedTo"].as_str().unwrap_or("");
    if !updated.is_empty() && newer(updated, current) && shown != Some(updated) {
        return Some((
            format!("  ✓ updated to v{updated} in the background — restart to use it"),
            updated.to_string(),
        ));
    }
    let latest = st["latestSeen"].as_str().unwrap_or("");
    if latest.is_empty() || !newer(latest, current) || shown == Some(latest) {
        return None;
    }
    let notice = match policy {
        "notify" => format!(
            "  ⬆ v{latest} is available — npm install -g {PKG}@latest (or set auto_update: \"install\")"
        ),
        // Installed in the background; its own notice follows.
        _ if installs(policy, latest, current) => return None,
        _ => format!(
            "  ⬆ v{latest} is available — npm install -g {PKG}@{latest} (auto_update: \"install\" only installs patch releases)"
        ),
    };
    Some((notice, latest.to_string()))
}

// Whether `policy` installs `latest` over `current`: "install" only within
// the same major.minor, so a release that may change behavior never lands
// unannounced; "install-any" installs anything newer.
fn installs(policy: &str, latest: &str, current: &str) -> bool {
    let minor = |v: &str| -> Vec<u64> {
        v.split('.')
            .take(2)
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    match policy {
        "install" => newer(latest, current) && minor(latest) == minor(current),
        "install-any" => newer(latest, current),
        _ => false,
    }
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
        if !installs(policy, latest, crate::VERSION) {
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
    use super::{
        effective_policy, installed_via_npm, installs, newer, pending_notice, resolve_policy,
    };
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn policy_resolution() {
        assert_eq!(resolve_policy("off", false, true), "off");
        assert_eq!(resolve_policy("notify", false, true), "notify");
        assert_eq!(resolve_policy("install", false, true), "install");
        assert_eq!(resolve_policy("bogus", false, true), "notify");
        // BWN_NO_AUTO_UPDATE=1 caps install back to notify.
        assert_eq!(resolve_policy("install", true, true), "notify");
        assert_eq!(resolve_policy("install-any", false, true), "install-any");
        assert_eq!(resolve_policy("install-any", true, true), "notify");
        assert_eq!(resolve_policy("install-any", false, false), "notify");
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
    fn install_stays_within_the_running_minor() {
        assert!(installs("install", "0.15.3", "0.15.1"));
        assert!(!installs("install", "0.15.1", "0.15.1"));
        // A new minor or major is only announced.
        assert!(!installs("install", "0.16.0", "0.15.1"));
        assert!(!installs("install", "1.0.0", "0.15.1"));
        assert!(!installs("install", "2.0.1", "1.4.0"));
        assert!(installs("install", "1.4.2", "1.4.0"));
        // "install-any" is the pre-0.15 "install": any newer release.
        assert!(installs("install-any", "0.16.0", "0.15.1"));
        assert!(installs("install-any", "1.0.0", "0.15.1"));
        assert!(!installs("install-any", "0.15.1", "0.15.1"));
        assert!(!installs("notify", "0.15.3", "0.15.1"));
        assert!(!installs("off", "0.15.3", "0.15.1"));
    }

    #[test]
    fn npm_install_detection() {
        let home = Path::new("/home/u/.buildwithnexus");
        assert!(installed_via_npm(
            Path::new("/usr/lib/node_modules/buildwithnexus-linux-x64/bin/buildwithnexus"),
            home
        ));
        assert!(installed_via_npm(
            Path::new(
                "/home/u/.nvm/versions/node/v22/lib/node_modules/buildwithnexus/bin/buildwithnexus"
            ),
            home
        ));
        // Where the 0.15 launcher downloads it.
        assert!(installed_via_npm(
            Path::new("/home/u/.buildwithnexus/bin/0.15.0/buildwithnexus"),
            home
        ));
        assert!(!installed_via_npm(
            Path::new("/home/u/.cargo/bin/buildwithnexus"),
            home
        ));
        assert!(!installed_via_npm(
            Path::new("/src/bwn/target/release/buildwithnexus"),
            home
        ));
    }

    // current_exe() resolves symlinks (on Linux it reads /proc/self/exe), so
    // with a symlinked home it names the real path while home() keeps the
    // link. Before 0.15 the node_modules check did not care.
    #[cfg(unix)]
    #[test]
    fn npm_install_detection_through_a_symlinked_home() {
        let root = std::env::temp_dir().join(format!("bwn-update-link-{}", std::process::id()));
        let real = root.join("real");
        let dir = real.join(".buildwithnexus/bin/0.15.0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("buildwithnexus"), "").unwrap();
        std::os::unix::fs::symlink(&real, root.join("link")).unwrap();
        let exe = std::fs::canonicalize(dir.join("buildwithnexus")).unwrap();
        let found = installed_via_npm(&exe, &root.join("link/.buildwithnexus"));
        let _ = std::fs::remove_dir_all(&root);
        assert!(found, "{} under a symlinked home", exe.display());
    }

    #[test]
    fn a_new_minor_is_announced_with_its_command_under_install() {
        let seen = json!({"latestSeen": "0.16.0"});
        let (text, v) = pending_notice("install", &seen, "0.15.1").unwrap();
        assert_eq!(v, "0.16.0");
        assert!(
            text.contains("npm install -g buildwithnexus@0.16.0"),
            "{text}"
        );
        // Shown once.
        let shown = json!({"latestSeen": "0.16.0", "noticeShownFor": "0.16.0"});
        assert_eq!(pending_notice("install", &shown, "0.15.1"), None);
        // A patch is installed in the background, so nothing to announce yet.
        let patch = json!({"latestSeen": "0.15.2"});
        assert_eq!(pending_notice("install", &patch, "0.15.1"), None);
        assert_eq!(pending_notice("install-any", &seen, "0.15.1"), None);
        let done = json!({"latestSeen": "0.15.2", "updatedTo": "0.15.2"});
        assert!(pending_notice("install", &done, "0.15.1")
            .unwrap()
            .0
            .contains("updated to v0.15.2"));
        assert!(pending_notice("notify", &seen, "0.15.1")
            .unwrap()
            .0
            .contains("@latest"));
        assert_eq!(pending_notice("off", &seen, "0.15.1"), None);
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
