//! Integrity seals on the stores bwn trusts (roadmap 2.5; analysis I3).
//!
//! The trust store (`trusted.json`), the startup-check store (`posture/`)
//! and the rules folder decide what bwn lets a session do, and they are
//! plain files that any process running as the user outside the sandbox can
//! rewrite. Each gets a seal: an HMAC-SHA256 of its contents under a key
//! that lives in the OS keyring (audit.rs), kept in `seals.json`.
//!
//! - A store bwn writes (trust, startup check) is sealed when written and
//!   checked when read. One rewritten behind bwn's back reads as not there:
//!   nothing trusted, nothing accepted, so the person is asked again.
//! - The rules folder is the user's own to edit, so it isn't sealed on
//!   write: its seal changes only when the person accepts the startup
//!   check, which names the folder when its contents changed since then.
//!
//! Without a keyring the key is a 0600 file in bwn's own folder, which a
//! process outside the sandbox can read too; the startup check says which.
//! The first run after an upgrade seals what is there; after that a store
//! with no seal counts as rewritten, so deleting `seals.json` doesn't pass.

use std::collections::BTreeMap;
use std::path::PathBuf;

const KEY_ACCOUNT: &str = "store-seal-key";

/// What a store's contents say against its seal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The seal matches.
    Sealed,
    /// No key can be kept on this machine, so nothing can be checked.
    Unsealed,
    /// Changed outside bwn since it was sealed, or its seal was removed.
    Rewritten,
}

fn seals_path() -> PathBuf {
    crate::config::home().join("seals.json")
}

fn key_file() -> PathBuf {
    crate::config::home().join("seal.key")
}

/// Where the seal key is kept, for the startup check.
pub fn key_place() -> &'static str {
    match crate::audit::keyring() {
        Some(k) => k.label(),
        None => "a file in bwn's own folder (no keyring here)",
    }
}

// The key, made on first use. Some(key, made_now).
fn key() -> Option<(Vec<u8>, bool)> {
    let decode = |h: &str| -> Option<Vec<u8>> {
        let h = h.trim();
        (h.len() == 64)
            .then(|| {
                (0..32)
                    .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).ok())
                    .collect::<Option<Vec<u8>>>()
            })
            .flatten()
    };
    let ring = crate::audit::keyring();
    let existing = match &ring {
        Some(k) => crate::audit::keyring_get(k, KEY_ACCOUNT),
        None => std::fs::read_to_string(key_file()).ok(),
    };
    if let Some(k) = existing.as_deref().and_then(decode) {
        return Some((k, false));
    }
    // A key file that's there (perhaps made since the read above) is read
    // again, and one that doesn't read as a key is not replaced: making a
    // new one would make every sealed store read as rewritten.
    if ring.is_none() && key_file().exists() {
        return std::fs::read_to_string(key_file())
            .ok()
            .as_deref()
            .and_then(decode)
            .map(|k| (k, false));
    }
    let mut fresh = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut fresh).ok()?;
    let hex: String = fresh.iter().map(|b| format!("{b:02x}")).collect();
    let saved = match &ring {
        Some(k) => crate::audit::keyring_set(k, KEY_ACCOUNT, &hex),
        None => {
            crate::config::ensure_home();
            // Made whole beside it, then linked into place, which fails if
            // a key is there already: two bwn starting at once (or a reader
            // catching a half-written file) must not each make a key, or
            // the stores the first sealed read as rewritten. The one that
            // loses uses the key that won.
            let tmp =
                key_file().with_extension(format!("key.{}.{}", std::process::id(), &hex[..8]));
            let linked = write_private(&tmp, &hex) && std::fs::hard_link(&tmp, key_file()).is_ok();
            let _ = std::fs::remove_file(&tmp);
            if !linked {
                return std::fs::read_to_string(key_file())
                    .ok()
                    .as_deref()
                    .and_then(decode)
                    .map(|k| (k, false));
            }
            true
        }
    };
    if !saved {
        return None;
    }
    // The first key on this machine (an upgrade): seal the stores as they
    // are, so from here on one without a seal counts as rewritten.
    seal_existing(&fresh);
    Some((fresh.to_vec(), true))
}

fn seal_existing(key: &[u8]) {
    let home = crate::config::home();
    let mut seals = read_seals();
    if let Ok(b) = std::fs::read(home.join("trusted.json")) {
        seals.insert("trusted.json".into(), mac(key, "trusted.json", &b));
    }
    if let Ok(rd) = std::fs::read_dir(home.join("posture")) {
        for e in rd.flatten() {
            let name = format!("posture/{}", e.file_name().to_string_lossy());
            if let Ok(b) = std::fs::read(e.path()) {
                let m = mac(key, &name, &b);
                seals.insert(name, m);
            }
        }
    }
    let rules = rules_bytes(&home.join("rules"));
    if !rules.is_empty() {
        seals.insert("rules".into(), mac(key, "rules", &rules));
    }
    if let Ok(text) = serde_json::to_string_pretty(&seals) {
        crate::config::ensure_home();
        let _ = write_private(&seals_path(), &text);
    }
}

fn write_private(p: &std::path::Path, text: &str) -> bool {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(p)
            .and_then(|mut f| f.write_all(text.as_bytes()))
            .is_ok()
    }
    #[cfg(not(unix))]
    {
        std::fs::write(p, text).is_ok()
    }
}

fn mac(key: &[u8], name: &str, bytes: &[u8]) -> String {
    let k = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    let mut ctx = ring::hmac::Context::with_key(&k);
    ctx.update(name.as_bytes());
    ctx.update(&[0]);
    ctx.update(bytes);
    ctx.sign()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn read_seals() -> BTreeMap<String, String> {
    std::fs::read_to_string(seals_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Seal `name` as holding `bytes` now (bwn just wrote it, or the person
/// accepted it).
pub fn seal(name: &str, bytes: &[u8]) {
    let Some((key, _)) = key() else { return };
    let mut seals = read_seals();
    seals.insert(name.to_string(), mac(&key, name, bytes));
    if let Ok(text) = serde_json::to_string_pretty(&seals) {
        crate::config::ensure_home();
        let _ = write_private(&seals_path(), &text);
    }
}

/// What `bytes` (the store's contents now) say against `name`'s seal.
pub fn check(name: &str, bytes: &[u8]) -> Verdict {
    let Some((key, made_now)) = key() else {
        // No way to keep a key at all: nothing to check against.
        return Verdict::Unsealed;
    };
    let _ = made_now;
    // bwn seals everything it writes, and every store there was when the
    // key was made: one with no seal was put there by something else.
    match read_seals().get(name) {
        Some(m) if *m == mac(&key, name, bytes) => Verdict::Sealed,
        _ => Verdict::Rewritten,
    }
}

/// Check `name`. True when its seal matches. A rewritten store is recorded in the decision log and said
/// once on stderr.
pub fn trusted(name: &str, bytes: &[u8]) -> bool {
    match check(name, bytes) {
        Verdict::Sealed => true,
        Verdict::Unsealed => {
            seal(name, bytes);
            true
        }
        Verdict::Rewritten => {
            crate::audit::record(crate::audit::Entry {
                kind: crate::audit::Kind::Exception,
                actor: "policy",
                decision: "deny",
                subject: name,
                detail: "changed outside bwn since it was sealed; read as empty",
            });
            static TOLD: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
            let mut told = TOLD.lock().unwrap_or_else(|e| e.into_inner());
            if !told.iter().any(|t| t == name) {
                told.push(name.to_string());
                eprintln!(
                    "buildwithnexus: warning: {name} changed outside bwn since bwn last wrote it; bwn ignores it and asks again"
                );
            }
            false
        }
    }
}

/// The rules folder's contents as one digest input: each `.json` file's
/// name and bytes, in name order.
pub fn rules_bytes(dir: &std::path::Path) -> Vec<u8> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        out.extend(
            f.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
                .as_bytes(),
        );
        out.push(0);
        out.extend(std::fs::read(&f).unwrap_or_default());
        out.push(0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // I3 (roadmap 2.5): a store sealed by bwn and rewritten behind its back
    // reads as rewritten; its seal deleted, too; unchanged, sealed.
    // Many threads making the key at once all end up with one key, so a
    // store sealed by one checks as sealed for every other.
    #[test]
    fn the_seal_key_is_made_once_under_a_race() {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("bwn-seal-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("NEXUS_HOME", &home);
        let keys: Vec<Vec<u8>> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..16).map(|_| s.spawn(|| key().map(|(k, _)| k))).collect();
            hs.into_iter().filter_map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(keys.len(), 16);
        assert!(
            keys.iter().all(|k| *k == keys[0]),
            "more than one key was made"
        );
        std::env::remove_var("NEXUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn invariant_i3_a_store_rewritten_outside_bwn_is_caught() {
        let _g = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("bwn-seal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("NEXUS_HOME", &home);
        std::env::set_var("BWN_TEST_KEYRING", home.join("keyring"));
        // An upgrade: the store is there before any key; making the key
        // seals it as found.
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("trusted.json"), "{}").unwrap();
        assert!(trusted("trusted.json", b"{}"));
        assert_eq!(check("trusted.json", b"{}"), Verdict::Sealed);
        // A store that appears with no seal was not written by bwn.
        assert_eq!(check("posture/forged.txt", b"fp"), Verdict::Rewritten);
        // bwn writes, and seals.
        seal("trusted.json", b"{\"a\":1}");
        assert_eq!(check("trusted.json", b"{\"a\":1}"), Verdict::Sealed);
        // Rewritten behind bwn's back.
        assert_eq!(check("trusted.json", b"{\"evil\":1}"), Verdict::Rewritten);
        assert!(!trusted("trusted.json", b"{\"evil\":1}"));
        // The seals file deleted: still caught, the key says seals existed.
        let _ = std::fs::remove_file(seals_path());
        assert_eq!(check("trusted.json", b"{\"a\":1}"), Verdict::Rewritten);
        std::env::remove_var("BWN_TEST_KEYRING");
        std::env::remove_var("NEXUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }
}
