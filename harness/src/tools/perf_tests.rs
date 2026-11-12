// Tests that hold the fast paths of grep_files and read_file against the
// straightforward implementations they replaced: the output must be the
// same, byte for byte.

use super::*;
use std::path::PathBuf;

pub(super) fn tempdir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("bwn-perf-{tag}-{}-{id}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

// A small deterministic generator, so the random corpora are the same on
// every run.
pub(super) struct Rng(pub(super) u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    pub(super) fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

// ── grep_files ──────────────────────────────────────────────────────────

/// `grep_files` as it was before the whole-buffer search: every file read
/// with `read_to_string`, every line lowercased and tested with `contains`.
fn reference_grep(input: &Value, cwd: &Path) -> Outcome {
    let pattern = input["pattern"].as_str().unwrap_or("");
    if pattern.is_empty() {
        return err("pattern is required");
    }
    let file_pattern = input["file_pattern"]
        .as_str()
        .or_else(|| input["include"].as_str())
        .unwrap_or("*");
    let case_sensitive = input["case_sensitive"].as_bool().unwrap_or(false);
    let needle = if case_sensitive {
        pattern.to_string()
    } else {
        pattern.to_lowercase()
    };
    let limit = search_limit(input);
    let mut files = Vec::new();
    let mut seen = 0;
    for root in search_roots(input, cwd) {
        collect_files(&root, &mut files, &mut seen);
    }
    files.sort();
    let mut matches = Vec::new();
    let mut total = 0usize;
    for path in files {
        let rel = display_path(&path, cwd);
        let basename_matches = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| simple_glob_match(file_pattern, name));
        if !simple_glob_match(file_pattern, &rel) && !basename_matches {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for (idx, line) in text.lines().enumerate() {
            let hit = if case_sensitive {
                line.contains(needle.as_str())
            } else {
                line.to_lowercase().contains(&needle)
            };
            if hit {
                total += 1;
                if matches.len() < limit {
                    matches.push(format!(
                        "{}:{}:{}",
                        rel,
                        idx + 1,
                        clip_line(line.trim_end(), MAX_MATCH_LINE)
                    ));
                }
            }
        }
    }
    if matches.is_empty() {
        ok(format!("no matches for {pattern}"))
    } else {
        let shown = matches.len();
        let body = matches.join("\n");
        let out = if total > shown {
            format!("{total} matches, showing {shown} (raise `max` or narrow the pattern)\n{body}")
        } else {
            body
        };
        ok(truncate(out, MAX_OUT))
    }
}

fn assert_same_grep(dir: &Path, input: &Value) {
    let want = reference_grep(input, dir);
    let got = run("grep_files", input, dir);
    assert_eq!(got.content, want.content, "input {input}");
    assert_eq!(got.is_error, want.is_error, "input {input}");
}

const GREP_PATTERNS: &[&str] = &[
    "needle",
    "Needle",
    "NEEDLE",
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
];

fn each_grep_input(mut check: impl FnMut(Value)) {
    for pattern in GREP_PATTERNS {
        for case_sensitive in [false, true] {
            for max in [None, Some(1), Some(3), Some(500)] {
                let mut input = json!({"pattern": pattern, "case_sensitive": case_sensitive});
                if let Some(max) = max {
                    input["max"] = json!(max);
                }
                check(input);
            }
        }
    }
}

#[test]
fn grep_whole_buffer_search_prints_what_the_line_search_printed() {
    let d = tempdir("grep");
    let files: &[(&str, &[u8])] = &[
        ("a/plain.rs", b"fn main() {\n    let needle = 1;\n    // Needle NEEDLE\n}\n"),
        ("a/crlf.txt", b"start\r\nNeedle one\r\n\r\nneedle two   \r\nend\r\n"),
        ("a/bare_cr.txt", b"one\rneedle\rtwo\nthree needle\r"),
        ("a/tail\r.txt", b"x tail\r\ny tail\rz\ntail\r"),
        ("a/no_newline.txt", b"first\nlast needle"),
        ("a/only.txt", b"needle"),
        ("a/empty.txt", b""),
        ("a/blank_lines.txt", b"\n\n\nneedle\n\n"),
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
        ("c/invalid.bin", b"needle here\n\xff\xfe broken\nneedle again\n"),
        ("c/invalid_tail.bin", b"needle\n\xc3"),
        ("c/invalid_no_hit.bin", b"\xff\xff\xff"),
        ("c/data.json", b"{\"k\": \"needle\", \"edge\": 0}\n"),
        ("needle.txt", b"the file name matches but only this line: NEEDLE\n"),
    ];
    for (name, bytes) in files {
        let path = d.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
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
    // Matches at the edges of the 64 KB read buffer, and a file that needs
    // the buffer to grow.
    let chunk = 64 * 1024;
    for (name, len) in [("at", chunk), ("under", chunk - 1), ("over", chunk + 1)] {
        let mut text = "edge\n".to_string();
        text.push_str(&"p".repeat(len - text.len() - "needle".len()));
        text.push_str("needle");
        assert_eq!(text.len(), len);
        fs::write(d.join(format!("c/buffer_{name}.txt")), &text).unwrap();
    }
    let mut big = String::new();
    for i in 0..9000 {
        big.push_str(&format!(
            "line {i} of a file past one buffer: Needle {i}\r\n"
        ));
    }
    fs::write(d.join("c/big.log"), &big).unwrap();

    each_grep_input(|input| assert_same_grep(&d, &input));
    for file_pattern in [
        "*", "", "*.txt", "*.TXT", "a/*", "b/*.md", "needle", "c/*.bin", "nope",
    ] {
        for pattern in ["needle", "k", "end", "é"] {
            assert_same_grep(
                &d,
                &json!({"pattern": pattern, "file_pattern": file_pattern}),
            );
            assert_same_grep(&d, &json!({"pattern": pattern, "include": file_pattern}));
        }
    }
    for root in ["a", "b", "c/big.log", "missing"] {
        assert_same_grep(&d, &json!({"pattern": "needle", "root": root}));
        assert_same_grep(
            &d,
            &json!({"pattern": "Needle", "path": root, "case_sensitive": true}),
        );
    }
    assert_same_grep(&d, &json!({"pattern": ""}));
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn grep_matches_the_line_search_on_generated_text() {
    // Small alphabets make matches, near-matches and overlaps common.
    let pieces: &[&str] = &[
        "a", "b", "A", "B", "k", "K", "i", "I", "ab", "ba", "\n", "\n", "\r\n", "\r", " ", "é",
        "É", "\u{212A}", "\u{130}", "ß", "σ", "Σ", "日", "\t",
    ];
    let needles: &[&str] = &[
        "a", "ab", "aba", "abab", "ba", "k", "ki", "ik", "i", "b a", "é", "éa", "aé", "É", "ka",
        "KA", "Ab", "σ", "日", "a\r", " ", "bb", "aaa", "ß", "ss", "i\u{307}",
    ];
    let mut rng = Rng(7);
    let d = tempdir("grep-gen");
    for round in 0..40 {
        let _ = fs::remove_dir_all(d.join("t"));
        fs::create_dir_all(d.join("t")).unwrap();
        for f in 0..12 {
            let mut text = String::new();
            for _ in 0..rng.below(120) {
                text.push_str(pieces[rng.below(pieces.len())]);
            }
            let mut bytes = text.into_bytes();
            // Now and then a file that is not UTF-8.
            if rng.below(10) == 0 {
                let at = rng.below(bytes.len() + 1);
                bytes.insert(at, 0xff);
            }
            fs::write(d.join(format!("t/f{f}.txt")), bytes).unwrap();
        }
        for needle in needles {
            for case_sensitive in [false, true] {
                let max = [1, 2, 7, 100][round % 4];
                assert_same_grep(
                    &d,
                    &json!({"pattern": needle, "case_sensitive": case_sensitive, "max": max}),
                );
            }
        }
    }
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn grep_only_two_non_ascii_characters_lowercase_into_ascii() {
    // The byte search compares ASCII letters in either case and nothing
    // else; that equals Unicode lowercasing unless some non-ASCII character
    // lowercases to text with ASCII in it. This pins the list of those.
    let found: Vec<char> = (0x80u32..=0x10FFFF)
        .filter_map(char::from_u32)
        .filter(|c| c.to_lowercase().any(|l| l.is_ascii()))
        .collect();
    assert_eq!(found, grep::LOWERCASE_INTO_ASCII);
    // And lowercasing a string is char by char apart from final sigma, which
    // stays non-ASCII, so ASCII never appears from a combination.
    assert_eq!("ΟΔΟΣ".to_lowercase(), "οδος");
}

#[test]
fn grep_counts_every_match_past_the_limit_and_numbers_lines_from_one() {
    let mut text = String::new();
    for i in 1..=1000 {
        text.push_str(if i % 10 == 0 {
            "Hit here\n"
        } else {
            "nothing\n"
        });
    }
    let hits = grep::search_bytes("hit", false, text.as_bytes(), 3);
    assert_eq!(hits.total, 100);
    assert_eq!(
        hits.lines,
        ["f:10:Hit here", "f:20:Hit here", "f:30:Hit here"]
    );
    // Several matches on one line are one result.
    let hits = grep::search_bytes("ab", true, b"ab ab ab\nxx\nab", 10);
    assert_eq!(hits.lines, ["f:1:ab ab ab", "f:3:ab"]);
    // A needle longer than the text, and one that only fits at the very end.
    assert_eq!(grep::search_bytes("abcd", true, b"abc", 10).total, 0);
    assert_eq!(
        grep::search_bytes("bc", true, b"abc", 10).lines,
        ["f:1:abc"]
    );
    // The anchor byte sits in the last 32 bytes and in the first.
    let pad = "-".repeat(100);
    let hay = format!("Q{pad}\n{pad}Q");
    assert_eq!(grep::search_bytes("q", false, hay.as_bytes(), 10).total, 2);
}

#[test]
fn grep_skips_a_file_over_the_text_tool_limit() {
    // A sparse file one byte over the limit: its first line would match.
    let d = tempdir("grep-big");
    let path = d.join("huge.log");
    fs::write(&path, "needle at the top\n").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(MAX_TEXT_FILE + 1)
        .unwrap();
    fs::write(d.join("small.txt"), "needle in a small file\n").unwrap();
    let r = run("grep_files", &json!({"pattern": "needle"}), &d);
    assert_eq!(r.content, "small.txt:1:needle in a small file");
    let _ = fs::remove_dir_all(&d);
}

#[cfg(unix)]
#[test]
fn walk_sensitivity_by_folder_agrees_with_resolving_every_file() {
    let d = tempdir("walk-sens");
    for (name, text) in [
        ("src/main.rs", "x"),
        ("src/.env", "x"),
        ("src/.env.local", "x"),
        ("src/id_rsa", "x"),
        ("src/cert.pem", "x"),
        (".ssh/config", "x"),
        (".aws/credentials", "x"),
        ("keys/plain.txt", "x"),
        ("notes.md", "x"),
    ] {
        let path = d.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    // A folder link onto a credential folder, and a file link onto a key.
    std::os::unix::fs::symlink(d.join(".ssh"), d.join("linked")).unwrap();
    std::os::unix::fs::symlink(d.join("src/id_rsa"), d.join("keys/innocent.txt")).unwrap();
    let mut checked = 0;
    for dir in ["src", ".ssh", ".aws", "keys", "linked", "."] {
        let dir = d.join(dir);
        let real = fs::canonicalize(&dir).unwrap();
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            if entry.file_type().unwrap().is_symlink() {
                continue;
            }
            let path = entry.path();
            assert_eq!(
                is_sensitive_entry(&path, &real),
                is_sensitive(&path),
                "{}",
                path.display()
            );
            checked += 1;
        }
    }
    assert!(checked >= 12, "{checked}");
    assert!(is_sensitive_entry(
        &d.join("linked/config"),
        &fs::canonicalize(d.join("linked")).unwrap()
    ));
    // The walk hides the same files either way, links included.
    let mut files = Vec::new();
    collect_files(&d, &mut files, &mut 0);
    let mut names: Vec<String> = files.iter().map(|p| display_path(p, &d)).collect();
    names.sort();
    assert_eq!(names, ["keys/plain.txt", "notes.md", "src/main.rs"]);
    let _ = fs::remove_dir_all(&d);
}
