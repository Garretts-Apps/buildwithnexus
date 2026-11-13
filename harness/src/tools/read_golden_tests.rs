// `read_file` output held against what the implementation before the
// chunked read printed: `read_golden.txt` was written by this same file run
// on origin/main 83190bc (set BWN_WRITE_READ_GOLDEN to a path to write it
// again). Each line is one call: its input, whether it was an error, and
// the length and FNV-1a hash of the text sent to the model.
//
// Self-contained on purpose (only `run` and std), so it compiles on the old
// tree unchanged.
#![cfg(unix)]

use super::run;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bwn-golden-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::canonicalize(&dir).unwrap()
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

// Lines of mixed width, so the 100 KB cut and every 128 KB chunk edge land
// inside multi-byte characters somewhere in the set.
fn text(target: usize, ending: &str, pad: usize) -> String {
    let mut out = "x".repeat(pad);
    let mut n = 0;
    while out.len() < target {
        n += 1;
        out.push_str(&format!(
            "line {n}: grüße 日本語 🙂 {}{ending}",
            "é".repeat(n % 41)
        ));
    }
    out
}

fn corpus(d: &Path) -> Vec<String> {
    let kb = 1024;
    let mut files: Vec<(String, Vec<u8>)> = vec![
        ("empty.txt".into(), Vec::new()),
        ("one.txt".into(), b"one".to_vec()),
        ("small.txt".into(), b"a\nb\nc\nd\ne\nf\ng\n".to_vec()),
        ("crlf.txt".into(), b"a\r\nb\r\n\r\nd\r\ne".to_vec()),
        ("bare_cr.txt".into(), b"a\rb\r\nc\r".to_vec()),
        ("blank.txt".into(), b"\n\n\n".to_vec()),
        ("uni.txt".into(), "é\nüber\n日本語\n🙂".as_bytes().to_vec()),
        ("bad_small.bin".into(), b"ok\n\xff\xfe\nmore\n".to_vec()),
        ("bad_tail.bin".into(), b"ok\nline\n\xc3".to_vec()),
        ("nul.txt".into(), b"a\0b\nc\n".to_vec()),
    ];
    for (name, size, ending) in [
        ("under_max.txt", 90 * kb, "\n"),
        ("over_max.txt", 150 * kb, "\n"),
        ("over_max_crlf.txt", 150 * kb, "\r\n"),
        ("at_stream.txt", 200 * kb, "\n"),
        ("large.txt", 700 * kb, "\n"),
        ("large_crlf.txt", 700 * kb, "\r\n"),
        ("large_cr.txt", 300 * kb, "\r"),
    ] {
        for pad in 0..4 {
            let mut t = text(size, ending, pad);
            if pad == 1 {
                t.push_str("the last line has no newline");
            }
            if pad == 2 {
                t.push('\r');
            }
            files.push((format!("p{pad}_{name}"), t.into_bytes()));
        }
    }
    // One line longer than everything shown, and a file of one newline per
    // byte.
    files.push(("one_line.txt".into(), "é".repeat(200 * kb).into_bytes()));
    files.push(("newlines.txt".into(), vec![b'\n'; 300 * kb]));
    // Exactly the sizes around the two thresholds.
    for size in [
        100 * kb - 1,
        100 * kb,
        100 * kb + 1,
        200 * kb - 1,
        200 * kb,
        200 * kb + 1,
    ] {
        let mut t = text(size, "\n", 0).into_bytes();
        t.truncate(size);
        while std::str::from_utf8(&t).is_err() {
            t.pop();
        }
        t.resize(size, b'z');
        files.push((format!("exact_{size}.txt"), t));
    }
    // Bad bytes at the start, across a chunk edge, in the middle, at the
    // very end, and a character cut short by the end of the file.
    let base = text(700 * kb, "\n", 0).into_bytes();
    for (name, at, bad) in [
        ("bad_start.bin", 0usize, &b"\xff"[..]),
        ("bad_early.bin", 5000, b"\xff\xfe"),
        ("bad_edge.bin", 128 * kb - 1, b"\xc3\x28"),
        ("bad_mid.bin", 400 * kb, b"\x80"),
        ("bad_end.bin", base.len(), b"\xff"),
        ("bad_cut_short.bin", base.len(), b"\xf0\x9f\x99"),
    ] {
        let mut at = at;
        while at < base.len() && base[at] & 0xC0 == 0x80 {
            at += 1;
        }
        let mut bytes = base.clone();
        bytes.splice(at..at, bad.iter().copied());
        files.push((name.into(), bytes));
    }
    let mut names = Vec::new();
    for (name, bytes) in &files {
        fs::write(d.join(name), bytes).unwrap();
        names.push(name.clone());
    }
    // A sparse file one byte over the 64 MB the text tools stop at.
    fs::write(d.join("huge.log"), "first\nsecond\nthird\n").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(d.join("huge.log"))
        .unwrap()
        .set_len(64 * 1024 * 1024 + 1)
        .unwrap();
    names.push("huge.log".into());
    // Not regular files, and not there.
    let fifo = std::ffi::CString::new(d.join("pipe").to_string_lossy().as_bytes()).unwrap();
    // SAFETY: a valid NUL-terminated path; mkfifo touches nothing else.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    fs::create_dir(d.join("folder")).unwrap();
    std::os::unix::fs::symlink(d.join("p1_large.txt"), d.join("link.txt")).unwrap();
    std::os::unix::fs::symlink(d.join("pipe"), d.join("link_to_pipe")).unwrap();
    for name in [
        "pipe",
        "link_to_pipe",
        "folder",
        "link.txt",
        "missing.txt",
        "/dev/zero",
        "/dev/null",
    ] {
        names.push(name.into());
    }
    names
}

fn transcript() -> String {
    let d = tempdir("read");
    let names = corpus(&d);
    let here = d.to_string_lossy().into_owned();
    let ranges: &[(Option<u64>, Option<u64>)] = &[
        (None, None),
        (Some(1), Some(1)),
        (Some(1), Some(40)),
        (Some(0), Some(0)),
        (Some(2), None),
        (None, Some(3)),
        (Some(5), Some(2)),
        (Some(3), Some(3)),
        (Some(700), Some(2600)),
        (Some(2000), None),
        (Some(3990), Some(4010)),
        (Some(1_000_000), Some(1_000_005)),
        (Some(299_990), None),
    ];
    let mut out = String::new();
    for name in &names {
        for &(start, end) in ranges {
            let mut input = json!({"path": name});
            if let Some(start) = start {
                input["start_line"] = json!(start);
            }
            if let Some(end) = end {
                input["end_line"] = json!(end);
            }
            let r = run("read_file", &input, &d);
            let text = r.content.replace(&here, "<D>");
            out.push_str(&format!(
                "{input}\t{}\t{}\t{:016x}\n",
                r.is_error,
                text.len(),
                fnv(text.as_bytes())
            ));
        }
    }
    let _ = fs::remove_dir_all(&d);
    out
}

#[test]
fn read_file_sends_the_model_what_it_sent_before_the_rewrite() {
    let got = transcript();
    if let Ok(path) = std::env::var("BWN_WRITE_READ_GOLDEN") {
        fs::write(path, &got).unwrap();
        return;
    }
    let want: &str = include_str!("read_golden.txt");
    for (g, w) in got.lines().zip(want.lines()) {
        assert_eq!(g, w);
    }
    assert_eq!(got.lines().count(), want.lines().count());
}
