// read_file's chunked read held against reading the file whole: the text
// shown must be the same, byte for byte.

use super::perf_tests::{tempdir, Rng};
use super::*;

/// `read_file`'s text as it was before the chunked read: the first `cap`
/// bytes read whole, then cut down to the lines and to `max`.
fn reference_view(
    path: &Path,
    range: Option<(usize, usize)>,
    cap: u64,
    max: usize,
) -> std::io::Result<(String, bool)> {
    let (file, len) = open_regular(path)?;
    let (text, cut) = read_prefix(file, len, cap)?;
    Ok((truncate_read(apply_line_range(text, range), max), cut))
}

fn streamed(
    path: &Path,
    range: Option<(usize, usize)>,
    cap: u64,
    max: usize,
    chunk: usize,
) -> std::io::Result<(String, bool)> {
    let (file, _) = open_regular(path)?;
    read_view::stream_view(file, range, cap, max, chunk)
}

const RANGES: &[Option<(usize, usize)>] = &[
    None,
    Some((1, 1)),
    Some((1, 3)),
    Some((2, 2)),
    Some((2, usize::MAX)),
    Some((3, 7)),
    Some((5, 5)),
    Some((1, usize::MAX)),
    Some((40, 60)),
    Some((1000, 2000)),
];

fn assert_same_view(path: &Path, what: &str) {
    for &range in RANGES {
        for cap in [1u64, 2, 3, 10, 33, 100, 1 << 20] {
            for max in [1usize, 8, 20, 50, 4000] {
                let want = reference_view(path, range, cap, max).map_err(|e| e.kind());
                for chunk in [4usize, 5, 7, 16, 64, 4096] {
                    let got = streamed(path, range, cap, max, chunk).map_err(|e| e.kind());
                    assert_eq!(
                        got, want,
                        "{what:?} range {range:?} cap {cap} max {max} chunk {chunk}"
                    );
                }
            }
        }
    }
}

#[test]
fn read_in_chunks_shows_what_reading_whole_showed() {
    let d = tempdir("read");
    let path = d.join("f.txt");
    let texts: &[&str] = &[
        "",
        "\n",
        "\n\n\n",
        "one",
        "one\n",
        "one\ntwo",
        "one\r\ntwo\r\nthree\r\n",
        "bare\rcr\r\nline\r",
        "a\r\r\nb\r\r\r\n\rc",
        "\r",
        "\r\n",
        "x\r",
        "é\nüber\n日本語のテキスト\n🙂🙂🙂\nend",
        "🙂",
        "aé🙂日aé🙂日aé🙂日aé🙂日aé🙂日aé🙂日",
        "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n",
        "\n\nthird\n\n\nsixth",
    ];
    for text in texts {
        fs::write(&path, text).unwrap();
        assert_same_view(&path, text);
    }
    // Generated text: short lines, long lines, every line ending.
    let pieces: &[&str] = &[
        "a", "bc", "\n", "\n", "\r\n", "\r", "é", "日本", "🙂", " ", "word ", "\n\n",
    ];
    let mut rng = Rng(11);
    for _ in 0..40 {
        let mut text = String::new();
        for _ in 0..rng.below(150) {
            text.push_str(pieces[rng.below(pieces.len())]);
        }
        fs::write(&path, &text).unwrap();
        assert_same_view(&path, &text);
    }
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn read_in_chunks_refuses_text_that_is_not_utf8_like_reading_whole() {
    let d = tempdir("read-bad");
    let path = d.join("f.bin");
    let base = "line one\nline twö\nline three 🙂\n";
    let bads: &[&[u8]] = &[
        b"\xff",
        b"\xc3",
        b"\xe6\x97",
        b"\xf0\x9f\x99",
        b"\xc3\x28",
        b"\x80",
    ];
    for bad in bads {
        for at in 0..=base.len() {
            if !base.is_char_boundary(at) {
                continue;
            }
            let mut bytes = base.as_bytes().to_vec();
            bytes.splice(at..at, bad.iter().copied());
            fs::write(&path, &bytes).unwrap();
            // Without a range every byte up to the cap is checked.
            for cap in [5u64, 12, 20, 1000] {
                let want = reference_view(&path, None, cap, 16).map_err(|e| e.kind());
                for chunk in [4usize, 5, 7, 64] {
                    let got = streamed(&path, None, cap, 16, chunk).map_err(|e| e.kind());
                    assert_eq!(got, want, "bad {bad:?} at {at} cap {cap} chunk {chunk}");
                }
            }
            // A range is refused or shown as the whole read decides,
            // wherever the bad bytes are from its lines.
            for range in [(1, 1), (2, 2), (3, 3), (2, 9), (7, 9)] {
                for cap in [12u64, 20, 1000] {
                    let want = reference_view(&path, Some(range), cap, 16).map_err(|e| e.kind());
                    for chunk in [4usize, 5, 64] {
                        let got =
                            streamed(&path, Some(range), cap, 16, chunk).map_err(|e| e.kind());
                        assert_eq!(got, want, "bad {bad:?} at {at} {range:?} {cap} {chunk}");
                    }
                }
            }
            // A range that takes in the whole file reads it all, too.
            let want = reference_view(&path, Some((1, 9)), 1000, 16).map_err(|e| e.kind());
            assert_eq!(want, Err(std::io::ErrorKind::InvalidData));
            for chunk in [4usize, 7, 64] {
                let got = streamed(&path, Some((1, 9)), 1000, 16, chunk).map_err(|e| e.kind());
                assert_eq!(got, want, "bad {bad:?} at {at} chunk {chunk}");
            }
        }
    }
    // Bad bytes before the requested lines fail the read, and so do bad
    // bytes after them, as when the file was read whole.
    let mut bytes = b"\xff\n".to_vec();
    bytes.extend_from_slice(base.as_bytes());
    fs::write(&path, &bytes).unwrap();
    assert!(streamed(&path, Some((2, 2)), 1000, 100, 4).is_err());
    let mut bytes = base.as_bytes().to_vec();
    bytes.extend_from_slice(b"padding padding\n\xff\n");
    fs::write(&path, &bytes).unwrap();
    assert!(streamed(&path, Some((1, 2)), 1000, 100, 8).is_err());
    assert_eq!(
        streamed(&path, Some((1, 2)), 40, 100, 8).unwrap(),
        ("line one\nline twö".to_string(), true)
    );
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn read_file_of_a_large_file_keeps_its_notes_and_line_ranges() {
    let d = tempdir("read-large");
    // Over the size read whole, with CRLF endings and multibyte text, so
    // the 100 KB cut and the chunk edges both land inside characters.
    let mut text = String::new();
    let mut n = 0;
    while text.len() < 700 * 1024 {
        n += 1;
        text.push_str(&format!(
            "line {n}: grüße 日本語 🙂 {}\r\n",
            "é".repeat(n % 37)
        ));
    }
    text.push_str("the last line has no newline");
    fs::write(d.join("big.txt"), &text).unwrap();
    let whole = run("read_file", &json!({"path": "big.txt"}), &d);
    assert!(!whole.is_error);
    assert_eq!(whole.content, truncate_read(text.clone(), MAX_READ));
    let marker = format!(
        "…[truncated: content is {} bytes / {} lines total; showing the first {} lines — pass start_line/end_line to read the rest in ranges]",
        text.len(),
        n + 1,
        whole.content.lines().count() - 1
    );
    assert!(whole.content.ends_with(&marker), "{marker}");
    for (start, end) in [
        (1, 1),
        (1, 40),
        (n - 2, n + 1),
        (n + 1, n + 1),
        (n + 5, n + 9),
        (10, 0),
    ] {
        let input = json!({"path": "big.txt", "start_line": start, "end_line": end});
        let got = run("read_file", &input, &d);
        let want = truncate_read(apply_line_range(text.clone(), line_range(&input)), MAX_READ);
        assert_eq!(got.content, want, "lines {start}-{end}");
    }
    let from = run(
        "read_file",
        &json!({"path": "big.txt", "start_line": n - 1}),
        &d,
    );
    assert_eq!(
        from.content,
        format!(
            "line {}: grüße 日本語 🙂 {}\nline {n}: grüße 日本語 🙂 {}\nthe last line has no newline",
            n - 1,
            "é".repeat((n - 1) % 37),
            "é".repeat(n % 37)
        )
    );
    // A range wider than the cut gets the same marker as before.
    let wide = run(
        "read_file",
        &json!({"path": "big.txt", "start_line": 2, "end_line": n}),
        &d,
    );
    let want = truncate_read(apply_line_range(text.clone(), Some((2, n))), MAX_READ);
    assert_eq!(wide.content, want);
    assert!(wide.content.contains("…[truncated: content is "));
    // Not UTF-8 anywhere in a large file: the same refusal as a small one.
    let mut bytes = text.clone().into_bytes();
    bytes.extend_from_slice(b"\n\xff\xfe\n");
    fs::write(d.join("big.bin"), &bytes).unwrap();
    fs::write(d.join("small.bin"), b"ok\n\xff\xfe\n").unwrap();
    let big = run("read_file", &json!({"path": "big.bin"}), &d);
    let small = run("read_file", &json!({"path": "small.bin"}), &d);
    assert!(big.is_error && small.is_error);
    assert_eq!(
        big.content.replace("big.bin", "F"),
        small.content.replace("small.bin", "F")
    );
    assert!(
        big.content.contains("contents are not valid UTF-8"),
        "{}",
        big.content
    );
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn read_file_past_the_text_limit_says_so_with_and_without_a_range() {
    // A sparse file one byte over the limit, text at the top.
    let d = tempdir("read-huge");
    let path = d.join("huge.log");
    fs::write(&path, "first\nsecond\nthird\n").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(MAX_TEXT_FILE + 1)
        .unwrap();
    let note = format!(
        "\n…[{} is larger than {}: only that much was read, so line numbers past it are not available here; use run_command (`sed -n 'A,Bp'`, `tail`) for the rest]",
        path.display(),
        human_bytes(MAX_TEXT_FILE)
    );
    let lines = run(
        "read_file",
        &json!({"path": "huge.log", "start_line": 2, "end_line": 3}),
        &d,
    );
    assert_eq!(lines.content, format!("second\nthird{note}"));
    let whole = run("read_file", &json!({"path": "huge.log"}), &d);
    assert!(whole.content.starts_with("first\nsecond\nthird\n\0\0"));
    assert!(whole.content.ends_with(&note));
    let marker = format!(
        "…[truncated: content is {MAX_TEXT_FILE} bytes / 4 lines total; showing the first 4 lines"
    );
    assert!(whole.content.contains(&marker), "{marker}");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn read_file_still_takes_the_editors_copy_of_a_file() {
    // The editor hook is process-wide and set once: this one answers only
    // for its own marker file, so every other test reads the disk.
    struct Unsaved;
    const MARK: &str = "bwn-perf-editor-copy.txt";
    impl EditorFiles for Unsaved {
        fn read(&self, path: &Path) -> Option<Result<String, String>> {
            path.ends_with(MARK)
                .then(|| Ok("unsaved one\nunsaved two\nunsaved three".to_string()))
        }
        fn write(&self, _: &Path, _: &str) -> Option<Result<(), String>> {
            None
        }
    }
    set_editor_files(Box::new(Unsaved));
    let d = tempdir("read-editor");
    // On disk the file is large enough for the chunked read.
    fs::write(d.join(MARK), "on disk\n".repeat(100_000)).unwrap();
    if EDITOR_FILES
        .get()
        .and_then(|e| e.read(&d.join(MARK)))
        .is_none()
    {
        return; // another hook was installed first
    }
    let whole = run("read_file", &json!({"path": MARK}), &d);
    assert_eq!(whole.content, "unsaved one\nunsaved two\nunsaved three");
    let lines = run(
        "read_file",
        &json!({"path": MARK, "start_line": 2, "end_line": 2}),
        &d,
    );
    assert_eq!(lines.content, "unsaved two");
    let _ = fs::remove_dir_all(&d);
}

#[cfg(unix)]
#[test]
fn read_file_still_refuses_a_device() {
    let r = run(
        "read_file",
        &json!({"path": "/dev/zero"}),
        &std::env::temp_dir(),
    );
    assert!(r.is_error);
    assert!(r.content.contains("not a regular file"), "{}", r.content);
}
