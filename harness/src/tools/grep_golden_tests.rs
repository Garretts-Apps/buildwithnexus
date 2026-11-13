// `grep_files` output held against what the implementation before the
// whole-buffer search printed: `grep_golden.txt` was written by this same
// file run on origin/main 83190bc (set BWN_WRITE_GREP_GOLDEN to a path to
// write it again). Each line is one call: its input, whether it was an
// error, and the length and FNV-1a hash of the text sent to the model.
//
// Self-contained on purpose (only `run` and std), so it compiles on the old
// tree unchanged.
#![cfg(unix)]

use super::run;
use serde_json::{json, Value};
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

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) % n as u64) as usize
    }
}

fn corpus(d: &Path) {
    let files: &[(&str, &[u8])] = &[
        ("a/plain.rs", b"fn main() {\n    let needle = 1;\n    // Needle NEEDLE nEeDlE\n}\n"),
        ("a/crlf.txt", b"start\r\nNeedle one\r\n\r\nneedle two   \r\nend\r\n"),
        ("a/bare_cr.txt", b"one\rneedle\rtwo\nthree needle\r"),
        ("a/tail\r.txt", b"x tail\r\ny tail\rz\ntail\r"),
        ("a/no_newline.txt", b"first\nlast needle"),
        ("a/only.txt", b"needle"),
        ("a/empty.txt", b""),
        ("a/blank_lines.txt", b"\n\n\nneedle\n\n"),
        ("a/trailing_space.txt", b"needle \t \nneedle\x0b\x0c\n \tneedle\n"),
        (
            "b/unicode.md",
            "Ünïcode Needle\nÉcole é\nSTRASSE straße Straße\nΣίσυφος ΟΔΟΣ σ ς\n日本語 needle 日本\n"
                .as_bytes(),
        ),
        (
            "b/folds.txt",
            "\u{212A}elvin 300\u{212A}\n\u{130}stanbul D\u{130}YARBAKIR\nKELVIN kelvin Istanbul\nıi\n"
                .as_bytes(),
        ),
        ("b/kelvin_only.txt", "temp: 4 \u{212A}\nno match here\n".as_bytes()),
        ("b/bom.txt", "\u{feff}needle after bom\n".as_bytes()),
        ("b/nul.txt", b"needle\0needle\nx\0y\n"),
        ("b/nbsp.txt", "needle\u{a0}\nneedle\u{2028}more needle\u{85}\n".as_bytes()),
        ("c/invalid.bin", b"needle here\n\xff\xfe broken\nneedle again\n"),
        ("c/invalid_tail.bin", b"needle\n\xc3"),
        ("c/invalid_no_hit.bin", b"\xff\xff\xff"),
        ("c/binary.dat", b"\x00\x01\x02needle\x03\x04\n\x7fELF needle\n"),
        ("c/data.json", b"{\"k\": \"needle\", \"edge\": 0}\n"),
        ("needle.txt", b"the file name matches but only this line: NEEDLE\n"),
        (".env", b"SECRET=needle\n"),
        ("keys/server.pem", b"needle key\n"),
        (".gitignore", b"ignored/\n*.skip\n"),
        ("ignored/x.txt", b"needle ignored\n"),
        ("a/y.skip", b"needle skipped\n"),
        (".hidden/h.txt", b"needle hidden\n"),
        ("node_modules/m/index.js", b"needle in a dependency\n"),
    ];
    for (name, bytes) in files {
        let path = d.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    std::os::unix::fs::symlink(d.join("a/plain.rs"), d.join("link_to_plain.rs")).unwrap();
    std::os::unix::fs::symlink(d.join("b"), d.join("link_to_b")).unwrap();
    std::os::unix::fs::symlink(d.join("keys"), d.join("link_to_keys")).unwrap();
    std::os::unix::fs::symlink(d.join(".env"), d.join("a/env_link.txt")).unwrap();
    // Very long lines: over the clip length, with matches at both ends, in
    // the middle, and a multibyte character across the clip point.
    let mut long = String::new();
    long.push_str(&format!("needle{}edge\n", "x".repeat(5000)));
    long.push_str(&format!("{}needle\n", "y".repeat(70_000)));
    long.push_str(&format!(
        "{}é{}NEEDLE long\n",
        "z".repeat(499),
        "w".repeat(600)
    ));
    long.push_str(&"long line ".repeat(20_000));
    fs::write(d.join("a/long.txt"), &long).unwrap();
    // Matches at the edges of 32-byte, 64 KB and 128 KB blocks.
    for len in [
        31usize, 32, 33, 63, 64, 65, 65535, 65536, 65537, 131072, 131073,
    ] {
        let mut text = "edge\n".to_string();
        text.push_str(&"p".repeat(len.saturating_sub(text.len() + "needle".len())));
        text.push_str("Needle");
        fs::write(d.join(format!("c/buffer_{len}.txt")), &text).unwrap();
        let mut text = "q".repeat(len - 3);
        text.push_str("nEEdle\nq\n");
        fs::write(d.join(format!("c/straddle_{len}.txt")), &text).unwrap();
    }
    let mut big = String::new();
    for i in 0..9000 {
        big.push_str(&format!(
            "line {i} of a file past one buffer: Needle {i}\r\n"
        ));
    }
    fs::write(d.join("c/big.log"), &big).unwrap();
    // Generated text over small alphabets, so matches and near-matches of
    // short patterns are common.
    let alphabets: [&[&str]; 3] = [
        &["a", "b", "A", "B", "\n", " "],
        &[
            "k", "K", "i", "I", "\u{212A}", "\u{130}", "ı", "\n", "\r\n", "é", "É",
        ],
        &[
            "ne", "ed", "le", "NE", "ED", "LE", "\n", "\r", "σ", "Σ", "ς", "ß", "ẞ",
        ],
    ];
    let mut rng = Rng(7);
    for (a, alphabet) in alphabets.iter().enumerate() {
        for f in 0..8 {
            let mut text = String::new();
            for _ in 0..rng.below(4000) {
                text.push_str(alphabet[rng.below(alphabet.len())]);
            }
            fs::write(d.join(format!("gen/g{a}_{f}.txt")), &text).unwrap_or_else(|_| {
                fs::create_dir_all(d.join("gen")).unwrap();
                fs::write(d.join(format!("gen/g{a}_{f}.txt")), &text).unwrap();
            });
        }
    }
}

const PATTERNS: &[&str] = &[
    "needle",
    "Needle",
    "NEEDLE",
    "nEeDlE",
    "k",
    "K",
    "i",
    "I",
    "kelvin",
    "istanbul",
    "é",
    "É",
    "straße",
    "STRASSE",
    "ß",
    "ẞ",
    "σ",
    "Σ",
    "ς",
    "日本",
    "ı",
    "\u{130}",
    "\u{212A}",
    "end",
    "start",
    "x",
    " ",
    "\t",
    "\r",
    "\n",
    "a\nb",
    "tail\r",
    "(",
    "fn main()",
    "zzz-not-there",
    "edge",
    "0",
    "::",
    "long",
    "\u{feff}",
    "\0",
    "ab",
    "AB",
    "ba b",
    "ki",
    "IK",
    "needle ",
    "ednele",
    "leed",
    "\u{a0}",
    "\u{2028}",
    "p",
    "pneedle",
    "qneedle",
    "SECRET",
    "ignored",
    "hidden",
    "dependency",
    "skipped",
    "key",
    ".*",
    "need.e",
    "^fn",
    "[a-z]",
];

fn inputs() -> Vec<Value> {
    let mut all = Vec::new();
    for pattern in PATTERNS {
        for case_sensitive in [None, Some(false), Some(true)] {
            for max in [None, Some(0), Some(1), Some(3), Some(500), Some(100_000)] {
                let mut input = json!({"pattern": pattern});
                if let Some(c) = case_sensitive {
                    input["case_sensitive"] = json!(c);
                }
                if let Some(max) = max {
                    input["max"] = json!(max);
                }
                all.push(input);
            }
        }
    }
    for file_pattern in [
        "*",
        "",
        "*.txt",
        "*.TXT",
        "a/*",
        "b/*.md",
        "needle",
        "c/*.bin",
        "nope",
        "**/*.txt",
        "*.{txt,md}",
        "plain.rs",
        "a/plain.rs",
        "?/*.rs",
        "[ab]/*",
        ".env",
        "*.pem",
        "g*_1.txt",
    ] {
        for pattern in ["needle", "k", "end", "é", "ab"] {
            all.push(json!({"pattern": pattern, "file_pattern": file_pattern}));
            all.push(json!({"pattern": pattern, "include": file_pattern, "case_sensitive": true}));
        }
    }
    for root in [
        "a",
        "b",
        "c/big.log",
        "missing",
        ".",
        "",
        "a/",
        "./b",
        "link_to_b",
        "link_to_keys",
        "keys",
        ".hidden",
        "ignored",
        "node_modules",
        "gen",
        "a/../b",
        ".env",
    ] {
        for key in ["root", "path", "dir", "directory"] {
            all.push(json!({"pattern": "needle", key: root}));
            all.push(json!({"pattern": "Needle", key: root, "case_sensitive": true, "max": 2}));
        }
    }
    all.push(json!({"pattern": ""}));
    all.push(json!({}));
    all.push(json!({"pattern": 7}));
    all.push(json!({"pattern": "needle", "max": "3"}));
    all.push(json!({"pattern": "needle", "case_sensitive": "true"}));
    all
}

fn transcript() -> String {
    let d = tempdir("grep");
    corpus(&d);
    let here = d.to_string_lossy().into_owned();
    let mut out = String::new();
    let mut record = |input: &Value, dir: &Path| {
        let r = run("grep_files", input, dir);
        let text = r.content.replace(&here, "<D>");
        out.push_str(&format!(
            "{input}\t{}\t{}\t{:016x}\n",
            r.is_error,
            text.len(),
            fnv(text.as_bytes())
        ));
    };
    for input in inputs() {
        record(&input, &d);
    }
    // A file one byte over the 64 MB the text tools stop at (sparse: NULs
    // after its first line).
    let huge = d.join("huge");
    fs::create_dir_all(&huge).unwrap();
    fs::write(huge.join("huge.log"), "needle at the top\n").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(huge.join("huge.log"))
        .unwrap()
        .set_len(64 * 1024 * 1024 + 1)
        .unwrap();
    fs::write(huge.join("small.txt"), "needle in a small file\n").unwrap();
    record(&json!({"pattern": "needle", "root": "huge"}), &d);
    record(
        &json!({"pattern": "NEEDLE", "root": "huge", "case_sensitive": true}),
        &d,
    );
    let _ = fs::remove_dir_all(&d);
    out
}

#[test]
fn grep_files_sends_the_model_what_it_sent_before_the_rewrite() {
    let got = transcript();
    if let Ok(path) = std::env::var("BWN_WRITE_GREP_GOLDEN") {
        fs::write(path, &got).unwrap();
        return;
    }
    let want = include_str!("grep_golden.txt");
    for (g, w) in got.lines().zip(want.lines()) {
        assert_eq!(g, w);
    }
    assert_eq!(got.lines().count(), want.lines().count());
}
