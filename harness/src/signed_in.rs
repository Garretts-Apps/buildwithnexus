//! A browser with the user's signed-in session (roadmap 5.9).
//!
//! `"signed_in_browser": {"profile": "~/.config/google-chrome", "hosts":
//! ["github.com"]}` in the user's own settings lets the model read pages on
//! those hosts as the user. It can't use the session anywhere else, and it
//! can't act:
//!
//! - **A copy of the profile.** Its cookies are copied into a throwaway
//!   folder, so nothing the browser does writes back to the user's own.
//! - **Only the listed hosts.** Every other request, a bare IP address
//!   included, goes to a proxy that refuses it (`screenshot::Blackhole`),
//!   so a page's planted image, fetch or redirect can't take the cookies to
//!   another site.
//! - **No scripts, read only.** Scripts are off (the copy's content setting
//!   for JavaScript is "block") and the page is only read
//!   (`--dump-dom`); nothing is clicked or typed, so a planted instruction
//!   can't submit a form.
//! - **Someone else's text.** What it reads is labelled untrusted (5.1).
//!
//! A project file's `signed_in_browser` waits for trust, since it names a
//! profile to read.

use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The configured profile folder and hosts, from the settings.
pub fn configured() -> Option<(PathBuf, Vec<String>)> {
    let v = crate::config::load_settings()?.signed_in_browser?;
    let profile = v["profile"].as_str()?.trim();
    let profile = match (profile.strip_prefix("~/"), crate::config::user_home()) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(profile),
    };
    let hosts: Vec<String> = v["hosts"]
        .as_array()?
        .iter()
        .filter_map(|h| h.as_str())
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| {
            !h.is_empty()
                && h.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))
        })
        .collect();
    (!hosts.is_empty()).then_some((profile, hosts))
}

/// The `host[:port]` of `url` when it is one of `hosts`, else why not.
pub fn allowed_authority(url: &str, hosts: &[String]) -> Result<String, String> {
    let u = url::Url::parse(url.trim()).map_err(|_| format!("`{url}` isn't a URL"))?;
    if !matches!(u.scheme(), "https" | "http") || !u.username().is_empty() || u.password().is_some()
    {
        return Err(format!("`{url}`: only http(s) URLs without a user name"));
    }
    let host = u
        .host_str()
        .ok_or_else(|| format!("`{url}` names no host"))?
        .to_ascii_lowercase();
    let authority = match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.clone(),
    };
    if hosts.iter().any(|h| *h == authority || *h == host) {
        Ok(authority)
    } else {
        Err(format!(
            "{host} isn't one of the hosts signed_in_browser may reach ({})",
            hosts.join(", ")
        ))
    }
}

/// The browser's command line: read `url` as text with scripts off, from
/// `profile` (the copy), with every host but `hosts` sent to the refusing
/// proxy on `proxy_port`.
pub fn chrome_args(
    url: &str,
    profile: &Path,
    proxy_port: u16,
    hosts: &[String],
    no_sandbox: bool,
) -> Vec<OsString> {
    let mut a: Vec<OsString> = [
        "--headless",
        "--disable-gpu",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-extensions",
        "--disable-background-networking",
        "--disable-component-update",
        "--disable-sync",
        "--disable-default-apps",
        "--disable-domain-reliability",
        "--disable-client-side-phishing-detection",
        "--disable-breakpad",
        "--no-pings",
        "--dns-prefetch-disable",
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
        "--disable-features=Translate,MediaRouter,OptimizationHints,NetworkTimeServiceQuerying",
        "--password-store=basic",
        "--use-mock-keychain",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    if no_sandbox {
        a.push("--no-sandbox".into());
    }
    a.push(format!("--proxy-server=http://127.0.0.1:{proxy_port}").into());
    // `<-loopback>` drops Chrome's implicit bypass (loopback and link-local),
    // so only the listed hosts go direct.
    let mut bypass = String::from("<-loopback>");
    for h in hosts {
        bypass.push(';');
        bypass.push_str(h);
    }
    a.push(format!("--proxy-bypass-list={bypass}").into());
    let mut dir = OsString::from("--user-data-dir=");
    dir.push(profile);
    a.push(dir);
    a.push("--dump-dom".into());
    a.push(url.into());
    a
}

// Folders of a profile that hold no session, only caches.
const SKIP: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "Service Worker",
    "ShaderCache",
    "GrShaderCache",
    "Crashpad",
    "component_crx_cache",
    "optimization_guide_model_store",
];

/// Copy `src` into `dst`, leaving caches out and never following a link;
/// at most `limit` bytes.
pub fn copy_profile(src: &Path, dst: &Path, limit: u64) -> Result<u64, String> {
    let mut copied = 0u64;
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((from, to)) = stack.pop() {
        std::fs::create_dir_all(&to).map_err(|e| format!("{}: {e}", to.display()))?;
        let entries = std::fs::read_dir(&from).map_err(|e| format!("{}: {e}", from.display()))?;
        for e in entries.flatten() {
            let name = e.file_name();
            let Ok(t) = e.file_type() else { continue };
            if t.is_symlink()
                || SKIP.iter().any(|s| name == *s)
                || name.to_string_lossy().starts_with("Singleton")
            {
                continue;
            }
            if t.is_dir() {
                stack.push((e.path(), to.join(&name)));
            } else if t.is_file() {
                copied += std::fs::copy(e.path(), to.join(&name)).unwrap_or(0);
                if copied > limit {
                    return Err(format!(
                        "the profile is over {} MB without its caches",
                        limit >> 20
                    ));
                }
            }
        }
    }
    Ok(copied)
}

/// Turn scripts off in the profile copy at `profile`: its default content
/// setting for JavaScript becomes "block". (Chrome's `scriptEnabled=false`
/// switch would also stop `--dump-dom` from printing the page.)
pub fn block_scripts(profile: &Path) -> Result<(), String> {
    let prefs = profile.join("Default").join("Preferences");
    let mut v: Value = std::fs::read_to_string(&prefs)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    for key in [
        "default_content_setting_values",
        "managed_default_content_settings",
    ] {
        if !v["profile"].is_object() {
            v["profile"] = serde_json::json!({});
        }
        if !v["profile"][key].is_object() {
            v["profile"][key] = serde_json::json!({});
        }
        v["profile"][key]["javascript"] = 2.into();
    }
    std::fs::create_dir_all(prefs.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(&prefs, v.to_string()).map_err(|e| format!("{}: {e}", prefs.display()))
}

/// The tool: read `input.url` as the signed-in user.
pub fn tool(input: &Value) -> Result<String, String> {
    let (profile, hosts) = configured()
        .ok_or("signed_in_browser isn't set up: name a browser profile and the hosts it may reach in your settings")?;
    let url = input["url"].as_str().unwrap_or("").trim();
    allowed_authority(url, &hosts)?;
    if !profile.is_dir() {
        return Err(format!("the profile {} isn't a folder", profile.display()));
    }
    let browser = crate::screenshot::find_chrome().ok_or("no Chrome, Chromium or Edge found")?;
    let dir = std::env::temp_dir().join(format!(
        "bwn-signed-in-{}-{}",
        std::process::id(),
        crate::knowledge::chrono_now_iso().replace([':', '.'], "-")
    ));
    let copy = dir.join("profile");
    let result = (|| {
        copy_profile(&profile, &copy, 512 << 20)?;
        block_scripts(&copy)?;
        let hole = crate::screenshot::Blackhole::start()
            .map_err(|e| format!("cannot start the network guard: {e}"))?;
        let mut cmd = std::process::Command::new(&browser);
        cmd.args(chrome_args(
            url,
            &copy,
            hole.port,
            &hosts,
            crate::tools::running_as_root_pub(),
        ));
        crate::sandbox::scrub_credentials(&mut cmd);
        let out = cmd
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("cannot start the browser: {e}"))?;
        let refused = hole.hosts();
        let mut text = crate::tools::strip_html_pub(&String::from_utf8_lossy(&out.stdout));
        if !refused.is_empty() {
            text.push_str(&format!(
                "\n\n[signed_in_browser refused requests to: {}]",
                refused.join(", ")
            ));
        }
        Ok(text)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts() -> Vec<String> {
        vec!["github.com".into(), "localhost:8080".into()]
    }

    // Roadmap 5.9: only the listed hosts, and never with a user name.
    #[test]
    fn invariant_browser_reaches_only_the_listed_hosts() {
        for ok in ["https://github.com/o/r", "http://localhost:8080/x"] {
            assert!(allowed_authority(ok, &hosts()).is_ok(), "{ok}");
        }
        for bad in [
            "https://evil.example/",
            "https://github.com.evil.example/",
            "http://localhost:9999/",
            "http://127.0.0.1:8080/",
            "https://user:pw@github.com/",
            "file:///etc/passwd",
            "javascript:alert(1)",
        ] {
            assert!(allowed_authority(bad, &hosts()).is_err(), "{bad}");
        }
    }

    // Scripts off, read only, the copy as the profile, and every host but
    // the listed ones through the refusing proxy, loopback included.
    #[test]
    fn invariant_browser_runs_without_scripts_from_a_copy_behind_the_guard() {
        let args: Vec<String> = chrome_args(
            "https://github.com/",
            Path::new("/tmp/copy"),
            4711,
            &hosts(),
            false,
        )
        .into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
        for want in [
            "--dump-dom",
            "--user-data-dir=/tmp/copy",
            "--proxy-server=http://127.0.0.1:4711",
            "--proxy-bypass-list=<-loopback>;github.com;localhost:8080",
        ] {
            assert!(args.iter().any(|a| a == want), "{want} missing: {args:?}");
        }
        assert_eq!(args.last().map(String::as_str), Some("https://github.com/"));
    }

    #[test]
    fn the_profile_copy_leaves_caches_and_links_out() {
        let base = std::env::temp_dir().join(format!("bwn-prof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("src");
        std::fs::create_dir_all(src.join("Default/Cache")).unwrap();
        std::fs::write(src.join("Default/Cookies"), "c").unwrap();
        std::fs::write(src.join("Default/Cache/big"), "x").unwrap();
        std::fs::write(src.join("SingletonLock"), "l").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", src.join("Default/link")).unwrap();
        copy_profile(&src, &base.join("dst"), 1 << 20).unwrap();
        block_scripts(&base.join("dst")).unwrap();
        let prefs: Value = serde_json::from_str(
            &std::fs::read_to_string(base.join("dst/Default/Preferences")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            prefs["profile"]["default_content_setting_values"]["javascript"],
            2
        );
        assert!(base.join("dst/Default/Cookies").is_file());
        assert!(!base.join("dst/Default/Cache").exists());
        assert!(!base.join("dst/SingletonLock").exists());
        assert!(!base.join("dst/Default/link").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
