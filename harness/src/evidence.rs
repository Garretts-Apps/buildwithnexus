//! An evidence record for each change bwn commits (roadmap 3.7; analysis
//! C4, AC-2).
//!
//! When a turn's change is committed (roadmap 3.8: once its checks passed),
//! bwn writes what it knew about it: each file's blob as committed, the
//! checks that ran and their verdict, the verifier's status, and the head
//! of the decision log. The record is canonical JSON, kept in bwn's folder
//! under its SHA-256, and the commit names it in a `Bwn-Evidence:` trailer.
//! `buildwithnexus evidence verify <commit>` finds the record, checks its
//! hash, and checks that the commit holds exactly the files the record
//! describes, so a record edited afterwards, or a commit amended with other
//! content, doesn't verify.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The commit trailer naming the record.
pub const TRAILER: &str = "Bwn-Evidence";

/// What the turn established, set by the agent before the commit.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    /// The project's checks: passed, or None when none ran.
    pub checks_passed: Option<bool>,
    /// Their report, shortened.
    pub checks_report: String,
    /// The verifier's status.
    pub verifier: String,
    /// The quality policy that applied (roadmap 5.6;
    /// `tools::applied_policy`).
    pub policy: Value,
}

static FACTS: std::sync::Mutex<Option<Facts>> = std::sync::Mutex::new(None);

/// Note what the turn established, for the record of its commit.
pub fn note(facts: Facts) {
    *FACTS.lock().unwrap_or_else(|e| e.into_inner()) = Some(facts);
}

fn dir() -> PathBuf {
    crate::config::home().join("evidence")
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Write the record for `files` (path, staged blob or "-" when deleted)
/// and return its hash, for the trailer.
pub fn record(files: &[(String, String)]) -> Option<String> {
    let facts = FACTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default();
    let rec = json!({
        "version": 1,
        "time": crate::knowledge::chrono_now_iso(),
        "files": files.iter().map(|(p, b)| json!({"path": p, "blob": b})).collect::<Vec<_>>(),
        "checks": {
            "passed": facts.checks_passed,
            "report": facts.checks_report.chars().take(4000).collect::<String>(),
        },
        "verifier": facts.verifier,
        "policy": facts.policy,
        "decision_log": crate::audit::head().map(|(a, b, n)| json!([a, b, n])),
        // The SBOM as committed, when the change carries one.
        "sbom": files.iter().find(|(p, _)| p == "bom.json").map(|(_, b)| b),
    });
    let text = serde_json::to_string(&rec).ok()?;
    let hash = sha256_hex(text.as_bytes());
    crate::config::ensure_home();
    std::fs::create_dir_all(dir()).ok()?;
    std::fs::write(dir().join(format!("{hash}.json")), &text).ok()?;
    Some(hash)
}

fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let o = std::process::Command::new("git")
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !o.status.success() {
        return Err(String::from_utf8_lossy(&o.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Check `commit` against its evidence record. Ok is a one-line summary.
pub fn verify(cwd: &Path, commit: &str) -> Result<String, String> {
    let body = git(cwd, &["log", "-1", "--format=%B", commit])?;
    let hash = body
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{TRAILER}:")))
        .map(str::trim)
        .ok_or_else(|| format!("{commit} names no evidence record (no {TRAILER} trailer)"))?
        .to_string();
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{TRAILER} `{hash}` isn't a record's hash"));
    }
    let path = dir().join(format!("{hash}.json"));
    let text = std::fs::read(&path).map_err(|e| format!("no record at {}: {e}", path.display()))?;
    if sha256_hex(&text) != hash {
        return Err("the record was changed after the commit named it".into());
    }
    let rec: Value =
        serde_json::from_slice(&text).map_err(|e| format!("the record isn't JSON: {e}"))?;
    let files = rec["files"].as_array().ok_or("the record lists no files")?;
    for f in files {
        let p = f["path"].as_str().unwrap_or("");
        let want = f["blob"].as_str().unwrap_or("");
        let got = git(
            cwd,
            &["rev-parse", "--verify", "-q", &format!("{commit}:{p}")],
        )
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "-".into());
        if got != want {
            return Err(format!(
                "{p} in {commit} isn't the file the record describes"
            ));
        }
    }
    if rec["checks"]["passed"].as_bool() == Some(false) {
        return Err("the record says the checks failed".into());
    }
    let checks = match rec["checks"]["passed"].as_bool() {
        Some(true) => "checks passed",
        _ => "no checks ran",
    };
    let policy = match rec["policy"]["digest"].as_str() {
        Some(d) => format!(
            ", policy {} ({}: {})",
            &d[..12.min(d.len())],
            rec["policy"]["source"].as_str().unwrap_or("?"),
            rec["policy"]["checks"]
                .as_array()
                .map(|a| {
                    let names: Vec<&str> = a.iter().filter_map(|c| c["name"].as_str()).collect();
                    if names.is_empty() {
                        "no checks".to_string()
                    } else {
                        names.join(", ")
                    }
                })
                .unwrap_or_else(|| "no checks".into())
        ),
        None => String::new(),
    };
    Ok(format!(
        "ok: {commit} matches evidence {} ({} file{}, {checks}, verifier {}{policy})",
        &hash[..12],
        files.len(),
        if files.len() == 1 { "" } else { "s" },
        rec["verifier"].as_str().unwrap_or("not run"),
    ))
}

/// `buildwithnexus evidence verify <commit>`.
pub fn cli(args: &[String]) -> i32 {
    match args {
        [v, commit] if v == "verify" => {
            let cwd = std::env::current_dir().unwrap_or_default();
            match verify(&cwd, commit) {
                Ok(line) => {
                    println!("{line}");
                    0
                }
                Err(why) => {
                    println!("NOT VERIFIED: {why}");
                    1
                }
            }
        }
        _ => {
            eprintln!("usage: buildwithnexus evidence verify <commit>");
            2
        }
    }
}
