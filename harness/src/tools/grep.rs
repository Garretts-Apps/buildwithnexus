// `grep_files`: literal search of file contents under a folder.
//
// Each file is read into one reused buffer and searched as a whole; line
// starts and line numbers are worked out only for hits. The output is what a
// line-by-line `str::lines` + `contains` search prints (`search_lines` below
// is that search, and still handles the patterns the byte search cannot).

use super::*;

/// How a pattern is searched for.
enum Mode {
    /// Bytes compared as they are (case-sensitive).
    Exact,
    /// ASCII pattern, letters compared in either case.
    AsciiFold,
    /// Line by line with Unicode lowercasing: a non-ASCII pattern searched
    /// without regard to case, or a pattern holding a line break.
    Lines,
}

struct Search {
    /// The pattern, lowercased unless the search is case-sensitive.
    needle: String,
    case_sensitive: bool,
    mode: Mode,
    /// Index of the needle byte the scan looks for first.
    anchor: usize,
    /// The needle holds `k` or `i`, which two non-ASCII characters also
    /// lowercase to (see `lowercases_into_ascii`).
    folds_from_non_ascii: bool,
}

// The only non-ASCII characters whose lowercase form contains ASCII: İ
// (U+0130, lowercases to `i` + U+0307) and the Kelvin sign (U+212A, to `k`).
// A file holding either is searched line by line when the pattern has an
// `i` or a `k`, since a byte comparison would miss those lines.
pub(super) const LOWERCASE_INTO_ASCII: [char; 2] = ['\u{130}', '\u{212A}'];

fn lowercases_into_ascii(hay: &[u8]) -> bool {
    !hay.is_ascii()
        && LOWERCASE_INTO_ASCII.iter().any(|c| {
            let mut utf8 = [0u8; 4];
            let seq = c.encode_utf8(&mut utf8).as_bytes();
            hay.windows(seq.len()).any(|w| w == seq)
        })
}

// A rough "how common is this byte in source and prose" score, so the scan
// can look for the needle's least common byte and skip most of the text.
fn commonness(b: u8) -> u8 {
    match b.to_ascii_lowercase() {
        b' ' | b'e' | b't' | b'a' | b'o' | b'i' | b'n' | b's' | b'r' => 9,
        b'l' | b'c' | b'd' | b'u' | b'p' | b'm' | b'f' | b'h' | b'_' => 7,
        b'a'..=b'z' => 5,
        b'(' | b')' | b',' | b'.' | b';' | b':' | b'=' | b'/' | b'-' | b'"' | b'0' | b'1' => 6,
        b'0'..=b'9' => 4,
        b'\t' => 6,
        _ => 2,
    }
}

impl Search {
    fn new(pattern: &str, case_sensitive: bool) -> Self {
        let needle = if case_sensitive {
            pattern.to_string()
        } else {
            pattern.to_lowercase()
        };
        let mode = if pattern.contains(['\n', '\r']) {
            Mode::Lines
        } else if case_sensitive {
            Mode::Exact
        } else if pattern.is_ascii() {
            Mode::AsciiFold
        } else {
            Mode::Lines
        };
        let anchor = needle
            .bytes()
            .enumerate()
            .min_by_key(|&(_, b)| commonness(b))
            .map_or(0, |(i, _)| i);
        let folds_from_non_ascii = needle.contains(['k', 'i']);
        Search {
            needle,
            case_sensitive,
            mode,
            anchor,
            folds_from_non_ascii,
        }
    }

    /// The first match at or after `from`.
    fn find(&self, hay: &[u8], from: usize) -> Option<usize> {
        let needle = self.needle.as_bytes();
        let fold = matches!(self.mode, Mode::AsciiFold);
        let want = needle[self.anchor];
        // `b | 0x20` maps both cases of a letter onto its lowercase byte and
        // no other byte onto it.
        let mask = if fold && want.is_ascii_lowercase() {
            0x20
        } else {
            0
        };
        // `at` walks the positions of the anchor byte; `end` is its last
        // position that leaves room for the whole needle.
        let end = hay.len().checked_sub(needle.len())? + self.anchor;
        let mut at = from + self.anchor;
        while at <= end {
            // Skip 32 bytes at a time while none of them is the anchor; the
            // compiler turns the inner loop into vector compares.
            while at + 32 <= hay.len() {
                let mut any = false;
                for &b in &hay[at..at + 32] {
                    any |= (b | mask) == want;
                }
                if any {
                    break;
                }
                at += 32;
            }
            at += hay.get(at..)?.iter().position(|&b| (b | mask) == want)?;
            if at > end {
                return None;
            }
            let start = at - self.anchor;
            let window = &hay[start..start + needle.len()];
            let hit = if fold {
                window.eq_ignore_ascii_case(needle)
            } else {
                window == needle
            };
            if hit {
                return Some(start);
            }
            at += 1;
        }
        None
    }
}

pub(super) struct Hits {
    pub(super) limit: usize,
    pub(super) lines: Vec<String>,
    /// Every matching line, including those past `limit`.
    pub(super) total: usize,
}

impl Hits {
    fn add(&mut self, rel: &str, line_no: usize, line: &str) {
        // Counted past the limit so the cap header can report how much was
        // left out.
        self.total += 1;
        if self.lines.len() < self.limit {
            self.lines.push(format!(
                "{}:{}:{}",
                rel,
                line_no,
                clip_line(line.trim_end(), MAX_MATCH_LINE)
            ));
        }
    }
}

// The line-by-line search: every line of `text` that contains the needle.
fn search_lines(search: &Search, text: &str, rel: &str, hits: &mut Hits) {
    for (idx, line) in text.lines().enumerate() {
        let hit = if search.case_sensitive {
            line.contains(search.needle.as_str())
        } else {
            line.to_lowercase().contains(&search.needle)
        };
        if hit {
            hits.add(rel, idx + 1, line);
        }
    }
}

// The whole-buffer search. Prints what `search_lines` prints for the same
// file; a file that is not UTF-8 is skipped, as there.
fn search_buffer(search: &Search, hay: &[u8], rel: &dyn Fn() -> String, hits: &mut Hits) {
    let by_lines = match search.mode {
        Mode::Lines => true,
        Mode::AsciiFold => search.folds_from_non_ascii && lowercases_into_ascii(hay),
        Mode::Exact => false,
    };
    if by_lines {
        if let Ok(text) = std::str::from_utf8(hay) {
            search_lines(search, text, &rel(), hits);
        }
        return;
    }
    let Some(mut hit) = search.find(hay, 0) else {
        return;
    };
    // Only a file with a candidate match is checked for UTF-8.
    let Ok(text) = std::str::from_utf8(hay) else {
        return;
    };
    let rel = rel();
    // `line_no` is the number of the line that starts at `counted`.
    let (mut line_no, mut counted) = (1usize, 0usize);
    loop {
        let start = hay[..hit]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
        let end = hay[hit..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(hay.len(), |i| hit + i);
        if hits.lines.len() < hits.limit {
            line_no += hay[counted..start].iter().filter(|&&b| b == b'\n').count();
            counted = start;
            // `str::lines` drops the `\n` and one `\r` before it.
            let mut line = &text[start..end];
            if end < hay.len() {
                line = line.strip_suffix('\r').unwrap_or(line);
            }
            hits.add(&rel, line_no, line);
        } else {
            hits.total += 1;
        }
        // One result per line: carry on from the next line.
        match search.find(hay, end + 1) {
            Some(next) => hit = next,
            None => return,
        }
    }
}

// Reads `path` whole into `buf` (reused across files) and returns its length,
// or None when it cannot be read. There is no size limit, as there was none
// when each file was read with `read_to_string`.
fn read_into(path: &Path, buf: &mut Vec<u8>) -> Option<usize> {
    use std::io::Read;
    let mut file = fs::File::open(path).ok()?;
    let mut filled = 0usize;
    loop {
        if filled == buf.len() {
            buf.resize((filled * 2).max(64 * 1024), 0);
        }
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    Some(filled)
}

/// One file's matches, by the whole-buffer search. For the tests that hold
/// it against the line-by-line search.
#[cfg(test)]
pub(super) fn search_bytes(pattern: &str, case_sensitive: bool, hay: &[u8], limit: usize) -> Hits {
    let mut hits = Hits {
        limit,
        lines: Vec::new(),
        total: 0,
    };
    let search = Search::new(pattern, case_sensitive);
    search_buffer(&search, hay, &|| "f".to_string(), &mut hits);
    hits
}

/// `grep_files`: search file contents under a folder.
pub(super) fn run_grep(input: &Value, cwd: &Path) -> Outcome {
    let pattern = input["pattern"].as_str().unwrap_or("");
    if pattern.is_empty() {
        return err("pattern is required");
    }
    let file_pattern = input["file_pattern"]
        .as_str()
        .or_else(|| input["include"].as_str())
        .unwrap_or("*");
    let every_file = file_pattern.is_empty() || file_pattern == "*";
    let case_sensitive = input["case_sensitive"].as_bool().unwrap_or(false);
    let search = Search::new(pattern, case_sensitive);
    let mut files = Vec::new();
    let mut seen = 0;
    for root in search_roots(input, cwd) {
        collect_files(&root, &mut files, &mut seen);
    }
    // Results are listed in path order.
    files.sort_unstable();
    let mut hits = Hits {
        limit: search_limit(input),
        lines: Vec::new(),
        total: 0,
    };
    let mut buf = Vec::new();
    for path in files {
        if !every_file {
            let basename_matches = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| simple_glob_match(file_pattern, name));
            if !basename_matches && !simple_glob_match(file_pattern, &display_path(&path, cwd)) {
                continue;
            }
        }
        let Some(len) = read_into(&path, &mut buf) else {
            continue;
        };
        search_buffer(
            &search,
            &buf[..len],
            &|| display_path(&path, cwd),
            &mut hits,
        );
        // One very large file does not keep its buffer for the rest.
        if buf.len() as u64 > MAX_TEXT_FILE {
            buf = Vec::new();
        }
    }
    if hits.lines.is_empty() {
        ok(format!("no matches for {pattern}"))
    } else {
        let (total, shown) = (hits.total, hits.lines.len());
        let body = hits.lines.join("\n");
        let out = if total > shown {
            format!("{total} matches, showing {shown} (raise `max` or narrow the pattern)\n{body}")
        } else {
            body
        };
        ok(truncate(out, MAX_OUT))
    }
}
