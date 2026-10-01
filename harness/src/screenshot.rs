// screenshot_url: a page served on this machine, as the model would see it in
// a browser. A local headless Chrome, Chromium or Edge renders it with a
// throwaway profile. Every request to another host goes to a proxy that
// refuses it and keeps the host's name, so the page cannot reach the network
// (IP literals included) and neither can Chrome's own services. The tool and
// the permission gate allow loopback addresses only, unless settings allow
// the host; that host is then the one reached directly.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const DEFAULT_SIZE: (u32, u32) = (1280, 800);
/// Virtual time the page's scripts get before the capture.
const SETTLE_MS: u32 = 3000;

/// Whether `authority` (`host`, `host:port`, `[::1]:port`) names this
/// machine: localhost, a `.localhost` name, 127.0.0.0/8 or ::1.
pub fn is_loopback_host(authority: &str) -> bool {
    let a = authority.trim().to_ascii_lowercase();
    let host = if let Some(rest) = a.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else if a.matches(':').count() > 1 {
        a.as_str() // a bare IPv6 address
    } else {
        a.split(':').next().unwrap_or("")
    };
    let host = host.trim_end_matches('.');
    host == "localhost"
        || host.ends_with(".localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Why screenshot_url will not open `host`.
pub fn off_loopback_refusal(host: &str) -> String {
    format!(
        "screenshot_url opens pages on this machine only (localhost, 127.0.0.1, [::1]); {host} is \
         not one. A server listening on 0.0.0.0 answers on 127.0.0.1. To allow {host}, add it to \
         \"network\": {{\"allow\": [...]}} in settings."
    )
}

// Browser names on PATH, most specific first.
const CHROME_NAMES: &[&str] = &[
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
    "chrome",
    "microsoft-edge",
    "microsoft-edge-stable",
    "msedge",
];

/// A local Chrome, Chromium or Edge: `BWN_CHROME` when set, else the usual
/// names on PATH, the usual install folders, and the Chromium a Playwright
/// install keeps (`PLAYWRIGHT_BROWSERS_PATH` or its default cache).
pub fn find_chrome() -> Option<PathBuf> {
    find_chrome_with(
        &|k| std::env::var_os(k),
        cfg!(windows),
        cfg!(target_os = "macos"),
    )
}

fn find_chrome_with(
    env: &dyn Fn(&str) -> Option<OsString>,
    windows: bool,
    macos: bool,
) -> Option<PathBuf> {
    if let Some(p) = env("BWN_CHROME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let path = env("PATH").unwrap_or_default();
    let pathext = env("PATHEXT");
    CHROME_NAMES
        .iter()
        .find_map(|n| crate::tools::find_in_path(n, &path, pathext.as_deref(), windows))
        .or_else(|| {
            install_paths(env, windows, macos)
                .into_iter()
                .find(|p| p.is_file())
        })
}

// Where installers put the browser, then Playwright's builds.
fn install_paths(
    env: &dyn Fn(&str) -> Option<OsString>,
    windows: bool,
    macos: bool,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let home = env("HOME").map(PathBuf::from);
    if windows {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(base) = env(var).map(PathBuf::from) {
                out.push(base.join("Google/Chrome/Application/chrome.exe"));
                out.push(base.join("Microsoft/Edge/Application/msedge.exe"));
                out.push(base.join("Chromium/Application/chrome.exe"));
            }
        }
    } else if macos {
        let mut roots = vec![PathBuf::from("/Applications")];
        roots.extend(home.as_ref().map(|h| h.join("Applications")));
        for root in roots {
            out.push(root.join("Google Chrome.app/Contents/MacOS/Google Chrome"));
            out.push(root.join("Chromium.app/Contents/MacOS/Chromium"));
            out.push(root.join("Microsoft Edge.app/Contents/MacOS/Microsoft Edge"));
        }
    } else {
        out.push(PathBuf::from("/opt/google/chrome/chrome"));
        out.push(PathBuf::from("/snap/bin/chromium"));
    }
    let playwright = env("PLAYWRIGHT_BROWSERS_PATH")
        .filter(|v| !v.is_empty() && v != "0")
        .map(PathBuf::from)
        .or_else(|| {
            if windows {
                env("LOCALAPPDATA").map(|b| PathBuf::from(b).join("ms-playwright"))
            } else if macos {
                home.as_ref()
                    .map(|h| h.join("Library/Caches/ms-playwright"))
            } else {
                env("XDG_CACHE_HOME")
                    .map(PathBuf::from)
                    .or_else(|| home.as_ref().map(|h| h.join(".cache")))
                    .map(|c| c.join("ms-playwright"))
            }
        });
    if let Some(dir) = playwright {
        out.extend(playwright_builds(&dir, windows, macos));
    }
    out
}

// The browsers in a Playwright cache, newest first; the headless shell (no
// sign-in or update services) before full Chromium.
fn playwright_builds(dir: &Path, windows: bool, macos: bool) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut builds: Vec<(bool, u64, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let (shell, rev) = if let Some(rev) = name.strip_prefix("chromium_headless_shell-") {
                (true, rev.to_string())
            } else {
                (false, name.strip_prefix("chromium-")?.to_string())
            };
            Some((shell, rev.parse().ok()?, e.path()))
        })
        .collect();
    builds.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let exes: &[&str] = if windows {
        &[
            "chrome-win/headless_shell.exe",
            "chrome-headless-shell-win64/chrome-headless-shell.exe",
            "chrome-win/chrome.exe",
            "chrome-win64/chrome.exe",
        ]
    } else if macos {
        &[
            "chrome-mac/headless_shell",
            "chrome-headless-shell-mac-arm64/chrome-headless-shell",
            "chrome-headless-shell-mac-x64/chrome-headless-shell",
            "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
            "chrome-mac-arm64/Chromium.app/Contents/MacOS/Chromium",
        ]
    } else {
        &[
            "chrome-linux/headless_shell",
            "chrome-headless-shell-linux64/chrome-headless-shell",
            "chrome-linux/chrome",
            "chrome-linux64/chrome",
        ]
    };
    builds
        .into_iter()
        .flat_map(|(_, _, d)| exes.iter().map(move |e| d.join(e)))
        .collect()
}

/// The browser's command line for one screenshot of `url` into `png`.
/// Requests to hosts other than loopback and `direct` go to the proxy on
/// `proxy_port`, which refuses them. Chrome refuses to start as root with
/// its sandbox (containers, CI), so `no_sandbox` turns it off there.
pub fn chrome_args(
    url: &str,
    (width, height): (u32, u32),
    profile: &Path,
    png: &Path,
    proxy_port: u16,
    direct: Option<&str>,
    no_sandbox: bool,
) -> Vec<OsString> {
    let mut a: Vec<OsString> = [
        "--headless",
        "--disable-gpu",
        "--hide-scrollbars",
        "--mute-audio",
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
        "--metrics-recording-only",
        "--no-pings",
        "--dns-prefetch-disable",
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
        "--disable-features=Translate,MediaRouter,OptimizationHints,NetworkTimeServiceQuerying",
        "--disable-field-trial-config",
        // No keychain or keyring prompt for the throwaway profile.
        "--use-mock-keychain",
        "--password-store=basic",
        "--force-color-profile=srgb",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    if no_sandbox {
        a.push("--no-sandbox".into());
    }
    // Chrome's implicit bypass also sends link-local addresses (169.254/16,
    // where cloud metadata answers, and fe80::/10) around any proxy.
    // `<-loopback>` drops those rules; loopback is then named to stay direct.
    a.push(format!("--proxy-server=http://127.0.0.1:{proxy_port}").into());
    let mut bypass = String::from(DIRECT_LOOPBACK);
    if let Some(host) = direct {
        bypass.push(';');
        bypass.push_str(host);
    }
    a.push(format!("--proxy-bypass-list={bypass}").into());
    a.push(format!("--window-size={width},{height}").into());
    a.push(format!("--virtual-time-budget={SETTLE_MS}").into());
    a.push(flag_path("--user-data-dir=", profile));
    a.push(flag_path("--screenshot=", png));
    a.push(url.into());
    a
}

const DIRECT_LOOPBACK: &str = "<-loopback>;localhost;*.localhost;127.0.0.1/8;[::1]";

fn flag_path(flag: &str, p: &Path) -> OsString {
    let mut s = OsString::from(flag);
    s.push(p.as_os_str());
    s
}

// Hosts Chrome itself calls (sign-in, network time, updates). They are
// refused like the rest but are not the page's, so they go unreported.
const CHROME_SERVICE_HOSTS: &[&str] = &[
    "accounts.google.com",
    "clients2.google.com",
    "www.google.com",
    "update.googleapis.com",
    "optimizationguide-pa.googleapis.com",
    "safebrowsing.googleapis.com",
    "content-autofill.googleapis.com",
];

/// A proxy that refuses every request and keeps the host each one named.
pub struct Blackhole {
    pub port: u16,
    seen: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Blackhole {
    pub fn start() -> std::io::Result<Blackhole> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (log, done) = (Arc::clone(&seen), Arc::clone(&stop));
        let thread = std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Some(host) = refuse(stream) {
                            if let Ok(mut l) = log.lock() {
                                if !l.contains(&host) {
                                    l.push(host);
                                }
                            }
                        }
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        Ok(Blackhole {
            port,
            seen,
            stop,
            thread: Some(thread),
        })
    }

    /// The hosts the page asked for, in the order first asked, without
    /// Chrome's own services.
    pub fn hosts(&self) -> Vec<String> {
        self.seen
            .lock()
            .map(|l| l.clone())
            .unwrap_or_default()
            .into_iter()
            .filter(|h| !CHROME_SERVICE_HOSTS.contains(&h.as_str()))
            .collect()
    }
}

impl Drop for Blackhole {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// Answers one proxied request with 403 and returns the host it named
// (`CONNECT host:443` or `GET http://host/…`).
fn refuse(stream: std::net::TcpStream) -> Option<String> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let mut first = String::new();
    let _ = BufReader::new(&stream).read_line(&mut first);
    let _ = (&stream)
        .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let target = first.split_whitespace().nth(1)?;
    let host = if first.starts_with("CONNECT ") {
        url::Url::parse(&format!("https://{target}")).ok()?
    } else {
        url::Url::parse(target).ok()?
    };
    host.host_str()
        .map(|h| h.trim_matches(['[', ']']).to_string())
}

/// The browser's name for the result line (`headless_shell`, `chrome`).
pub fn browser_name(p: &Path) -> String {
    p.file_stem()
        .unwrap_or(OsStr::new("browser"))
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_are_this_machine_only() {
        for h in [
            "localhost",
            "localhost:3000",
            "LOCALHOST.:80",
            "app.localhost:5173",
            "127.0.0.1",
            "127.1.2.3:8080",
            "[::1]:3000",
            "::1",
        ] {
            assert!(is_loopback_host(h), "{h}");
        }
        for h in [
            "example.com",
            "localhost.example.com",
            "0.0.0.0:8000",
            "10.0.0.5:3000",
            "[::ffff:8.8.8.8]",
            "169.254.169.254",
            "",
        ] {
            assert!(!is_loopback_host(h), "{h}");
        }
    }

    #[test]
    fn the_browser_never_reaches_other_hosts_and_uses_a_throwaway_profile() {
        let a = chrome_args(
            "http://localhost:3000/",
            (1280, 800),
            Path::new("/t/profile"),
            Path::new("/t/shot.png"),
            4242,
            None,
            false,
        );
        let a: Vec<String> = a.iter().map(|s| s.to_string_lossy().into_owned()).collect();
        for want in [
            "--headless",
            "--proxy-server=http://127.0.0.1:4242",
            "--user-data-dir=/t/profile",
            "--screenshot=/t/shot.png",
            "--window-size=1280,800",
            "--disable-background-networking",
            "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
            "--use-mock-keychain",
            "--password-store=basic",
        ] {
            assert!(a.iter().any(|x| x == want), "{want} missing: {a:?}");
        }
        assert_eq!(a.last().unwrap(), "http://localhost:3000/");
        assert!(a.contains(&format!("--proxy-bypass-list={DIRECT_LOOPBACK}")));
        assert!(!a.iter().any(|x| x == "--no-sandbox"));
        // A host settings allow is reached directly; root turns the sandbox off.
        let a = chrome_args(
            "https://docs.example.com/",
            (800, 600),
            Path::new("/p"),
            Path::new("/s.png"),
            1,
            Some("docs.example.com"),
            true,
        );
        let a: Vec<String> = a.iter().map(|s| s.to_string_lossy().into_owned()).collect();
        assert!(a.contains(&format!(
            "--proxy-bypass-list={DIRECT_LOOPBACK};docs.example.com"
        )));
        assert!(a.contains(&"--no-sandbox".to_string()));
    }

    #[test]
    fn the_blackhole_refuses_and_names_each_host() {
        let hole = Blackhole::start().unwrap();
        for req in [
            "CONNECT cdn.example.org:443 HTTP/1.1\r\nHost: cdn.example.org\r\n\r\n",
            "GET http://93.184.216.34/c.png HTTP/1.1\r\n\r\n",
            "GET http://clients2.google.com/time HTTP/1.1\r\n\r\n",
            "CONNECT cdn.example.org:443 HTTP/1.1\r\n\r\n",
        ] {
            let mut s = std::net::TcpStream::connect(("127.0.0.1", hole.port)).unwrap();
            s.write_all(req.as_bytes()).unwrap();
            let mut answer = String::new();
            let _ = std::io::Read::read_to_string(&mut s, &mut answer);
            assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
        }
        assert_eq!(hole.hosts(), ["cdn.example.org", "93.184.216.34"]);
    }

    #[test]
    fn chrome_is_found_by_setting_path_install_folder_or_playwright() {
        let root = std::env::temp_dir().join(format!("bwn-find-chrome-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let pw = root.join("pw");
        for rel in [
            "chromium-1100/chrome-linux/chrome",
            "chromium-1194/chrome-linux/chrome",
            "chromium_headless_shell-1194/chrome-linux/headless_shell",
            "chromium-1194/chrome-win/chrome.exe",
        ] {
            let p = pw.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
        }
        let vars = |pairs: Vec<(&'static str, OsString)>| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
        };
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        // The setting wins, as given.
        let env = vars(vec![("BWN_CHROME", "/custom/chrome".into())]);
        assert_eq!(
            find_chrome_with(&env, false, false),
            Some(PathBuf::from("/custom/chrome"))
        );
        // Playwright: the newest headless shell first.
        let env = vars(vec![
            ("PATH", empty.clone().into()),
            ("PLAYWRIGHT_BROWSERS_PATH", pw.clone().into()),
        ]);
        assert_eq!(
            find_chrome_with(&env, false, false),
            Some(pw.join("chromium_headless_shell-1194/chrome-linux/headless_shell"))
        );
        // Windows: the install folders, then Playwright's chrome-win build.
        let env = vars(vec![
            ("PATH", empty.clone().into()),
            ("ProgramFiles", r"C:\Program Files".into()),
            ("LOCALAPPDATA", root.clone().into()),
            ("PLAYWRIGHT_BROWSERS_PATH", pw.clone().into()),
        ]);
        let paths = install_paths(&env, true, false);
        assert!(
            paths[0].starts_with(r"C:\Program Files")
                && paths[0].ends_with("Google/Chrome/Application/chrome.exe"),
            "{paths:?}"
        );
        assert_eq!(
            find_chrome_with(&env, true, false),
            Some(pw.join("chromium-1194/chrome-win/chrome.exe"))
        );
        // A browser on PATH comes before any of them.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let bin = root.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let chromium = bin.join("chromium");
            std::fs::write(&chromium, "").unwrap();
            std::fs::set_permissions(&chromium, std::fs::Permissions::from_mode(0o755)).unwrap();
            let env = vars(vec![
                ("PATH", bin.clone().into()),
                ("PLAYWRIGHT_BROWSERS_PATH", pw.clone().into()),
            ]);
            assert_eq!(find_chrome_with(&env, false, false), Some(chromium));
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
