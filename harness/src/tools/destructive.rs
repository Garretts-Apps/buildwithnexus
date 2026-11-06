//! Commands that would destroy data outside a project, such as deleting the home folder or formatting a drive; they ask even in auto mode.

use super::*;

// ── destructive commands ─────────────────────────────────────────────────

// Commands so destructive they require confirmation in every mode.
pub fn catastrophic(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    let nospace: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    if nospace.contains("rm-rf/")          // rm -rf of an absolute path
        || nospace.contains("rm-fr/")
        || nospace.contains(":(){:|:&};:") // fork bomb
        || nospace.contains("mkfs")
        || nospace.contains("of=/dev/")    // dd onto a device
        || nospace.contains(">/dev/sd")
        || nospace.contains(">/dev/nvme")
        || nospace.contains("chmod-r777/")
    {
        return true;
    }
    // rm with recursive+force flags (joined `-rf`/`-fr` or split `-r -f`)
    // aimed at a root-ish target: `/abs`, `~`, `~/`, `*`, `.`, `..`.
    let toks: Vec<&str> = lower.split_whitespace().collect();
    for (i, t) in toks.iter().enumerate() {
        if *t != "rm" {
            continue;
        }
        let (mut recursive, mut force) = (false, false);
        for arg in &toks[i + 1..] {
            if let Some(flags) = arg.strip_prefix('-').filter(|f| !f.starts_with('-')) {
                recursive |= flags.contains('r');
                force |= flags.contains('f');
            } else if *arg == "--recursive" {
                recursive = true;
            } else if *arg == "--force" {
                force = true;
            }
        }
        if !(recursive && force) {
            continue;
        }
        for arg in &toks[i + 1..] {
            if arg.starts_with('-') {
                continue;
            }
            // Quotes and `$HOME`/`${HOME}` spellings folded away first, and a
            // trailing `/*`, `/.*` or `/.` (`~/*`, `"$HOME"/*`, `./*`).
            let target = normalize_target(arg);
            if matches!(target.as_str(), "~" | "*" | "." | "..")
                || target.starts_with(['/', '\\'])
                || is_drive_path(&arg.replace(['"', '\''], ""))
            {
                return true;
            }
        }
    }
    find_deletes_root_or_home(&toks) || windows_destructive(&lower)
}

// A shell word folded to the form the destructive-command checks compare:
// quotes dropped, `$HOME`, `${HOME}` and the Windows profile variables read as
// `~`, then trailing path separators and `/*`, `/.*`, `/.` removed (they all
// name the same folder's contents). A target that was `/`-rooted and is empty
// afterwards is `/`. Expects lowercase input.
pub(super) fn normalize_target(word: &str) -> String {
    let mut t: String = word.chars().filter(|c| !matches!(c, '"' | '\'')).collect();
    for var in [
        "${home}",
        "$home",
        "$env:userprofile",
        "$userprofile",
        "%userprofile%",
    ] {
        if let Some(rest) = t.strip_prefix(var) {
            if rest.is_empty() || rest.starts_with(['/', '\\']) {
                t = format!("~{rest}");
                break;
            }
        }
    }
    let rooted = t.starts_with('/');
    loop {
        let before = t.len();
        t = t.trim_end_matches(['/', '\\']).to_string();
        for tail in ["/*", "/.*", "/."] {
            if let Some(head) = t.strip_suffix(tail) {
                t = head.to_string();
            }
        }
        if t.len() == before {
            break;
        }
    }
    if t.is_empty() && rooted {
        t.push('/');
    }
    t
}

// `find <home or root> … -delete` and `find <home or root> … -exec rm …`: a
// recursive delete by another name. Only the folders `rm -rf` is also stopped
// on count (`/`, `~`, `$HOME`); `find . -name '*.tmp' -delete` and
// `find ./build -delete` are ordinary work.
pub(super) fn find_deletes_root_or_home(toks: &[&str]) -> bool {
    for (i, tok) in toks.iter().enumerate() {
        if *tok != "find" && !tok.ends_with("/find") {
            continue;
        }
        // The command ends at a shell separator.
        let args: Vec<&str> = toks[i + 1..]
            .iter()
            .copied()
            .take_while(|t| !matches!(*t, "|" | "||" | "&&" | "&" | ";"))
            .collect();
        // Start points: the words before the first expression (`-name`, `(`, `!`).
        let starts = args
            .iter()
            .copied()
            .skip_while(|t| matches!(*t, "-h" | "-l" | "-p"))
            .take_while(|t| !t.starts_with(['-', '(', '!']));
        let mut whole = false;
        for start in starts {
            let t = normalize_target(start);
            whole |= t == "/" || t == "~";
        }
        if !whole {
            continue;
        }
        for (j, a) in args.iter().enumerate() {
            if *a == "-delete" {
                return true;
            }
            if matches!(*a, "-exec" | "-execdir" | "-ok" | "-okdir") {
                let prog = args.get(j + 1).map_or("", |p| p.trim_matches(['"', '\'']));
                let name = prog.rsplit('/').next().unwrap_or(prog);
                if matches!(name, "rm" | "rmdir" | "unlink" | "shred") {
                    return true;
                }
            }
        }
    }
    false
}

// Windows commands that destroy data or weaken the system whatever their
// target: cmd's recursive or unconfirmed deletes, PowerShell's recursive
// forced Remove-Item under any alias, disk, boot and registry tools,
// ownership or ACL changes on system paths, execution-policy changes, and
// encoded PowerShell, whose script no check here can read. Matched on every
// word, so a call wrapped in `cmd /c` or `powershell -Command` counts too.
pub(super) fn windows_destructive(lower: &str) -> bool {
    let toks: Vec<&str> = lower
        .split(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    ';' | '|' | '&' | '(' | ')' | '{' | '}' | '"' | '\'' | '`' | ','
                )
        })
        .filter(|t| !t.is_empty())
        .collect();
    for (i, tok) in toks.iter().enumerate() {
        // cmd reads `rd/s/q` as `rd /s /q`.
        let (name, glued) = match tok.split_once('/') {
            Some((n, g)) if matches!(n, "rd" | "rmdir" | "del" | "erase" | "cipher") => {
                (n.to_string(), g)
            }
            _ => (normalized_bin(tok), ""),
        };
        let rest = &toks[i + 1..];
        let switch = |want: &dyn Fn(&str) -> bool| {
            glued.split('/').any(&want)
                || rest
                    .iter()
                    .filter_map(|t| t.strip_prefix('/'))
                    .any(|t| t.split('/').any(&want))
        };
        // PowerShell parameters take any unambiguous prefix and `:value`.
        let param = |full: &str, min: usize| {
            rest.iter().any(|t| {
                let p = t.strip_prefix('-').unwrap_or("");
                let p = p.split(':').next().unwrap_or(p);
                p.len() >= min && full.starts_with(p)
            })
        };
        let remove_item = matches!(name.as_str(), "remove-item" | "ri");
        let hit = match name.as_str() {
            "rd" | "rmdir" if switch(&|s| s == "s") => true,
            "del" | "erase" if switch(&|s| s == "s" || s == "q") => true,
            "cipher" => switch(&|s| s.starts_with('w')),
            "format" => rest.iter().any(|t| is_drive_root(t)),
            "diskpart" | "bcdedit" | "set-executionpolicy" | "format-volume" | "clear-disk" => true,
            "reg" => rest.first() == Some(&"delete"),
            "takeown" | "icacls" => rest.iter().any(|t| is_windows_system_path(t)),
            "powershell" | "pwsh" => rest.iter().any(|t| {
                let p = t.strip_prefix(['-', '/']).unwrap_or("");
                let p = p.split(':').next().unwrap_or(p);
                p == "ec" || (!p.is_empty() && "encodedcommand".starts_with(p))
            }),
            _ => false,
        };
        // Remove-Item and its aliases. `-fo` is not a Unix rm flag, so next
        // to it `-r` is PowerShell's -Recurse.
        let hit = hit
            || (remove_item || matches!(name.as_str(), "rm" | "rd" | "rmdir" | "del" | "erase"))
                && param("recurse", 1)
                && param("force", 2);
        if hit {
            return true;
        }
    }
    false
}

// An absolute Windows path: `c:\...` or `c:/...`.
pub(super) fn is_drive_path(t: &str) -> bool {
    let b = t.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/')
}

// `c:`, `c:\` or `c:/`.
pub(super) fn is_drive_root(t: &str) -> bool {
    let b = t.as_bytes();
    b.len() >= 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && t[2..].chars().all(|c| matches!(c, '\\' | '/'))
}

// A drive root or the Windows, Program Files or Users tree, lowercased.
pub(super) fn is_windows_system_path(t: &str) -> bool {
    if [
        "%systemroot%",
        "%windir%",
        "%programfiles",
        "$env:systemroot",
        "$env:windir",
    ]
    .iter()
    .any(|v| t.contains(v))
        || t.starts_with("$env:programfiles")
    {
        return true;
    }
    let t = t.replace('\\', "/");
    let t = t
        .strip_prefix("//?/")
        .or_else(|| t.strip_prefix("//./"))
        .unwrap_or(&t);
    let b = t.as_bytes();
    if b.len() < 2 || !b[0].is_ascii_alphabetic() || b[1] != b':' {
        return false;
    }
    let r = t[2..].trim_matches('/');
    r.is_empty()
        || r == "*"
        || r == "users"
        || ["windows", "program"].iter().any(|d| r.starts_with(d))
}
