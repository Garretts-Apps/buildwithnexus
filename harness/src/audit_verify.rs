//! bwn-audit-verify: check a bwn decision log and list what it holds
//! (roadmap 2.5, AC-1).
//!
//! Usage: `bwn-audit-verify <log.jsonl> [--head <hex>]`
//!
//! Given only the log, it lists every decision with its time, actor, kind,
//! decision and subject, and checks the chain: each line's `prev` must be
//! the SHA-256 of the previous line's bytes, and the first line's must be
//! the genesis value for the session the file is named after. A deleted,
//! edited, inserted or reordered line breaks that. `--head` is a chain head
//! recorded elsewhere (the OS keyring, a commit trailer): the log must
//! reach it, which also catches a log rewritten with a consistent chain or
//! cut short.
//!
//! This program shares no code with the writer (`src/audit.rs`) on purpose.
//! Exit status: 0 the chain holds, 1 it is broken, 2 usage or I/O error.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut path = None;
    let mut head = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--head" => head = it.next().cloned(),
            "-h" | "--help" => {
                println!("usage: bwn-audit-verify <log.jsonl> [--head <hex>]");
                return ExitCode::SUCCESS;
            }
            _ if path.is_none() => path = Some(a.clone()),
            _ => {
                eprintln!("bwn-audit-verify: unexpected argument {a}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(path) = path else {
        eprintln!("usage: bwn-audit-verify <log.jsonl> [--head <hex>]");
        return ExitCode::from(2);
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("bwn-audit-verify: {path}: {e}");
            return ExitCode::from(2);
        }
    };
    let stem = std::path::Path::new(&path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let report = verify(&bytes, &stem, head.as_deref());
    for row in &report.rows {
        println!("{row}");
    }
    match &report.broken {
        None => {
            println!(
                "ok: {} decisions, chain intact, head {}",
                report.rows.len(),
                report.head
            );
            ExitCode::SUCCESS
        }
        Some(why) => {
            println!("BROKEN: {why}");
            ExitCode::from(1)
        }
    }
}

struct Report {
    rows: Vec<String>,
    head: String,
    broken: Option<String>,
}

fn verify(bytes: &[u8], session: &str, want_head: Option<&str>) -> Report {
    let mut rows = Vec::new();
    let mut expect = digest_hex(format!("bwn-audit-genesis:{session}").as_bytes());
    let mut expect_seq: u64 = 1;
    // Lines as written: split on \n only, keeping each line's exact bytes.
    let text = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if !text.is_empty() {
        for (i, line) in text.split(|b| *b == b'\n').enumerate() {
            let n = i + 1;
            let v: serde_json::Value = match serde_json::from_slice(line) {
                Ok(v) => v,
                Err(e) => {
                    return Report {
                        rows,
                        head: expect,
                        broken: Some(format!("line {n} is not JSON ({e})")),
                    }
                }
            };
            let field = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
            let prev = field("prev");
            if prev != expect {
                return Report {
                    rows,
                    head: expect,
                    broken: Some(format!(
                        "line {n}: its prev does not match the line before it (a line was removed, edited, inserted or moved here)"
                    )),
                };
            }
            if v.get("seq").and_then(|s| s.as_u64()) != Some(expect_seq) {
                return Report {
                    rows,
                    head: expect,
                    broken: Some(format!("line {n}: sequence number is not {expect_seq}")),
                };
            }
            rows.push(format!(
                "{}  {:<6} {:<9} {:<6} {}  {}",
                field("time"),
                field("actor"),
                field("kind"),
                field("decision"),
                field("subject"),
                field("detail")
            ));
            expect = digest_hex(line);
            expect_seq += 1;
        }
    }
    let broken = match want_head {
        Some(h) if !h.eq_ignore_ascii_case(&expect) => Some(format!(
            "the log ends at head {expect}, not at the recorded head {h} (it was cut short or rewritten)"
        )),
        _ => None,
    };
    Report {
        rows,
        head: expect,
        broken,
    }
}

fn digest_hex(b: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, b)
        .as_ref()
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}
