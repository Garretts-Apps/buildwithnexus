// `read_file`'s view of a text file: at most MAX_READ bytes of it, or of the
// requested lines, with the note that says how much there was.
//
// A small file is read whole. A large one is read in fixed-size chunks that
// are checked for UTF-8, counted and dropped, so showing the first 100 KB of
// a 60 MB log does not hold 60 MB; a line range stops reading at its last
// line. The text returned is what reading the whole file and then cutting
// it down returns.

use super::*;

// Files up to this size take the simple path: read whole, then cut.
const STREAM_OVER: u64 = 2 * MAX_READ as u64;
const CHUNK: usize = 128 * 1024;

/// The text `read_file` shows for `path`, and whether the file was longer
/// than MAX_TEXT_FILE. The editor's copy is used when it has one.
pub(super) fn read_file_view(
    path: &Path,
    range: Option<(usize, usize)>,
) -> std::io::Result<(String, bool)> {
    if let Some(Ok(text)) = EDITOR_FILES.get().and_then(|e| e.read(path)) {
        return Ok((
            truncate_read(apply_line_range(text, range), MAX_READ),
            false,
        ));
    }
    let (file, len) = open_regular(path)?;
    if len <= STREAM_OVER {
        let (text, cut) = read_prefix(file, len, MAX_TEXT_FILE)?;
        return Ok((truncate_read(apply_line_range(text, range), MAX_READ), cut));
    }
    stream_view(file, len, range, MAX_TEXT_FILE, MAX_READ, CHUNK)
}

// What is shown, built up piece by piece: the first `max` bytes (and a few
// more, to find the character boundary) are kept, the rest only counted.
struct Shown {
    max: usize,
    kept: Vec<u8>,
    len: usize,
    newlines: usize,
    last: u8,
}

impl Shown {
    fn push(&mut self, bytes: &[u8]) {
        let Some(&last) = bytes.last() else {
            return;
        };
        let room = (self.max + 4).saturating_sub(self.kept.len());
        self.kept.extend_from_slice(&bytes[..room.min(bytes.len())]);
        self.len += bytes.len();
        self.newlines += bytes.iter().filter(|&&b| b == b'\n').count();
        self.last = last;
    }

    // `truncate_read` of everything pushed.
    fn finish(mut self) -> String {
        if self.len <= self.max {
            return String::from_utf8_lossy(&self.kept).into_owned();
        }
        let mut at = self.max;
        while at > 0 && self.kept[at] & 0xC0 == 0x80 {
            at -= 1;
        }
        self.kept.truncate(at);
        let total_lines = self.newlines + usize::from(self.last != b'\n');
        read_truncation(&String::from_utf8_lossy(&self.kept), self.len, total_lines)
    }
}

// `apply_line_range` over a stream: lines `start..=end` as `str::lines`
// splits them, joined with `\n`.
struct Lines {
    start: usize,
    end: usize,
    /// The line the next byte belongs to.
    line_no: usize,
    /// Some of the current line has been seen.
    started: bool,
    /// A line has already been shown, so the next one follows a `\n`.
    any: bool,
    /// The current line so far ends in a `\r` not yet shown: it is dropped
    /// if a `\n` follows.
    pending_cr: bool,
    done: bool,
}

impl Lines {
    fn feed(&mut self, mut rest: &[u8], shown: &mut Shown) {
        while !rest.is_empty() && !self.done {
            if self.line_no < self.start {
                // Whole chunks before the range are only counted.
                let count = rest.iter().filter(|&&b| b == b'\n').count();
                if count < self.start - self.line_no {
                    self.line_no += count;
                    return;
                }
                while self.line_no < self.start {
                    let Some(i) = rest.iter().position(|&b| b == b'\n') else {
                        return;
                    };
                    rest = &rest[i + 1..];
                    self.line_no += 1;
                }
                continue;
            }
            let newline = rest.iter().position(|&b| b == b'\n');
            let seg = &rest[..newline.unwrap_or(rest.len())];
            if !self.started {
                if self.any {
                    shown.push(b"\n");
                }
                self.started = true;
                self.any = true;
            }
            if self.pending_cr {
                self.pending_cr = false;
                if !seg.is_empty() {
                    shown.push(b"\r");
                }
            }
            match newline {
                Some(i) => {
                    shown.push(seg.strip_suffix(b"\r").unwrap_or(seg));
                    rest = &rest[i + 1..];
                    self.started = false;
                    self.line_no += 1;
                    self.done = self.line_no > self.end;
                }
                None => {
                    match seg.strip_suffix(b"\r") {
                        Some(body) => {
                            shown.push(body);
                            self.pending_cr = true;
                        }
                        None => shown.push(seg),
                    }
                    return;
                }
            }
        }
    }
}

fn read_some(file: &mut fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        match std::io::Read::read(file, buf) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            other => return other,
        }
    }
}

fn not_utf8() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "stream did not contain valid UTF-8",
    )
}

/// `truncate_read(apply_line_range(text, range), max)` for the first `cap`
/// bytes of `file`, without holding them, and whether the file is longer
/// than `cap`. Text that is not UTF-8 is an InvalidData error, as it is for
/// a file read whole; with a range, only what is read up to the range's
/// last line is checked.
pub(super) fn stream_view(
    mut file: fs::File,
    len: u64,
    range: Option<(usize, usize)>,
    cap: u64,
    max: usize,
    chunk: usize,
) -> std::io::Result<(String, bool)> {
    let mut shown = Shown {
        max,
        kept: Vec::with_capacity(max + 4),
        len: 0,
        newlines: 0,
        last: 0,
    };
    let mut lines = range.map(|(start, end)| Lines {
        start,
        end,
        line_no: 1,
        started: false,
        any: false,
        pending_cr: false,
        done: false,
    });
    // Takes text in; true once a line range has all its lines.
    let mut feed = |bytes: &[u8], shown: &mut Shown| match &mut lines {
        Some(lines) => {
            lines.feed(bytes, shown);
            lines.done
        }
        None => {
            shown.push(bytes);
            false
        }
    };
    let mut buf = vec![0u8; chunk.max(4)];
    // The bytes of a character split across two chunks.
    let (mut carry, mut carried) = ([0u8; 4], 0usize);
    let mut consumed = 0u64;
    let (mut cut, mut stopped_early) = (false, false);
    while !stopped_early {
        let room = (cap - consumed).min(buf.len() as u64) as usize;
        if room == 0 {
            // At the cap: one more byte tells a longer file from one that
            // ends here.
            cut = read_some(&mut file, &mut buf[..1])? > 0;
            break;
        }
        let n = read_some(&mut file, &mut buf[..room])?;
        if n == 0 {
            break;
        }
        consumed += n as u64;
        let mut bytes = &buf[..n];
        while carried > 0 && !bytes.is_empty() {
            carry[carried] = bytes[0];
            carried += 1;
            bytes = &bytes[1..];
            match std::str::from_utf8(&carry[..carried]) {
                Ok(_) => {
                    stopped_early = feed(&carry[..carried], &mut shown);
                    carried = 0;
                }
                Err(e) if e.error_len().is_some() || carried == carry.len() => {
                    return Err(not_utf8());
                }
                Err(_) => {}
            }
        }
        if carried > 0 || stopped_early {
            continue;
        }
        let valid = match std::str::from_utf8(bytes) {
            Ok(_) => bytes.len(),
            Err(e) if e.error_len().is_some() => return Err(not_utf8()),
            Err(e) => e.valid_up_to(),
        };
        stopped_early = feed(&bytes[..valid], &mut shown);
        carried = bytes.len() - valid;
        carry[..carried].copy_from_slice(&bytes[valid..]);
    }
    if stopped_early {
        // The rest was not read; the size says whether it passes the cap.
        cut = len > cap;
    } else if carried > 0 && !cut {
        // The file ends inside a character. (At the cap that character is
        // dropped instead, as when the first `cap` bytes are read whole.)
        return Err(not_utf8());
    }
    if lines.is_some_and(|l| l.pending_cr) {
        shown.push(b"\r");
    }
    Ok((shown.finish(), cut))
}
