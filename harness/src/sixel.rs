//! Real-pixel inline images for terminals that speak Sixel: Windows Terminal
//! 1.22+, WezTerm, foot, mlterm, xterm (-ti vt340) and others. The kitty
//! placeholder path (graphics.rs) covers kitty and Ghostty; this covers the
//! terminals that answer "4" (Sixel) to a Primary Device Attributes query.
//!
//! Support and the cell size in pixels come from one query at startup
//! (`probe`): Device Attributes plus XTWINOPS 16 ("report cell size"). The
//! image is decoded and quantized to at most 256 colours by ffmpeg, then
//! encoded here. Encoding is pure, so it is unit-testable without a terminal.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

// 0 = not probed, 1 = no Sixel, 2 = Sixel.
static SUPPORT: AtomicU8 = AtomicU8::new(0);
// Cell size reported by the terminal, packed as (w << 16) | h; 0 = unknown.
static CELL: AtomicU32 = AtomicU32::new(0);

/// Whether images should be drawn as Sixel. `BWN_IMAGES=sixel` forces it on;
/// `blocks`, `kitty` or `off` force it off; otherwise the startup probe decides.
pub fn supported() -> bool {
    match std::env::var("BWN_IMAGES")
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("sixel") => return true,
        Some("blocks") | Some("half") | Some("kitty") | Some("off") | Some("0") => return false,
        _ => {}
    }
    SUPPORT.load(Ordering::Relaxed) == 2
}

/// Pixel size of one cell: what the terminal reported, else the ioctl answer,
/// else the common 10x20.
pub fn cell_pixels() -> (u32, u32) {
    let c = CELL.load(Ordering::Relaxed);
    if c != 0 {
        return (c >> 16, c & 0xffff);
    }
    crate::graphics::cell_pixels()
}

/// Ask the terminal for Sixel support and its cell size. Runs once, in raw
/// mode, before anything else reads stdin. DA1 goes last: every terminal
/// answers it, so its reply marks the end of the responses.
#[cfg(unix)]
pub fn probe() {
    use std::io::Write;
    if SUPPORT.load(Ordering::Relaxed) != 0 {
        return;
    }
    SUPPORT.store(1, Ordering::Relaxed);
    let mut out = std::io::stdout();
    if out.write_all(b"\x1b[16t\x1b[14t\x1b[c").is_err() || out.flush().is_err() {
        return;
    }
    let mut buf = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(400);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd we own, a bounded timeout.
        let ready = unsafe { libc::poll(&mut pfd, 1, left.as_millis() as libc::c_int) };
        if ready <= 0 {
            break;
        }
        let mut chunk = [0u8; 256];
        // SAFETY: reads into a buffer we own, at most its length.
        let n = unsafe { libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
        if da1_complete(&buf) {
            break;
        }
    }
    record_probe(&buf);
}

// Store what the terminal said: Sixel support and the cell size, from the
// `16t` reply or else the `14t` window size divided by the grid.
#[cfg(any(unix, windows))]
fn record_probe(buf: &[u8]) {
    let (sixel, mut cell) = parse_probe(buf);
    // No cell-size answer: derive it from the window's pixel size.
    if cell.is_none() {
        if let (Some((ww, wh)), Ok((cols, rows))) =
            (parse_window_px(buf), crossterm::terminal::size())
        {
            if cols > 0 && rows > 0 {
                cell = Some((ww / u32::from(cols), wh / u32::from(rows)));
            }
        }
    }
    if sixel {
        SUPPORT.store(2, Ordering::Relaxed);
    }
    if let Some((w, h)) = cell {
        if w > 0 && h > 0 && w < 0xffff && h < 0xffff {
            CELL.store((w << 16) | h, Ordering::Relaxed);
        }
    }
}

/// Windows: the same query, read as console key events. VT input is turned
/// on only for the probe, so the terminal's reply arrives as characters, and
/// the console mode is restored before crossterm starts reading.
#[cfg(windows)]
pub fn probe() {
    use std::io::Write;
    if SUPPORT.load(Ordering::Relaxed) != 0 {
        return;
    }
    SUPPORT.store(1, Ordering::Relaxed);
    let buf = win::query(
        b"\x1b[16t\x1b[14t\x1b[c",
        std::time::Duration::from_millis(400),
    );
    let _ = std::io::stdout().flush();
    record_probe(&buf);
}

#[cfg(not(any(unix, windows)))]
pub fn probe() {}

#[cfg(windows)]
mod win {
    use std::io::Write;
    use std::time::{Duration, Instant};

    type Handle = *mut core::ffi::c_void;
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
    const KEY_EVENT: u16 = 0x0001;
    const WAIT_OBJECT_0: u32 = 0;

    // INPUT_RECORD: a u16 event type, padding, then a 16-byte union whose
    // KEY_EVENT_RECORD holds bKeyDown at 0 and the UTF-16 char at 10.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct InputRecord {
        event_type: u16,
        _pad: u16,
        event: [u8; 16],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> Handle;
        fn GetConsoleMode(h: Handle, mode: *mut u32) -> i32;
        fn SetConsoleMode(h: Handle, mode: u32) -> i32;
        fn WaitForSingleObject(h: Handle, ms: u32) -> u32;
        fn ReadConsoleInputW(h: Handle, buf: *mut InputRecord, len: u32, read: *mut u32) -> i32;
    }

    pub fn query(q: &[u8], timeout: Duration) -> Vec<u8> {
        let mut reply = Vec::new();
        // SAFETY: plain kernel32 calls on the process's own console handle,
        // with buffers owned here and lengths that match them.
        unsafe {
            let h = GetStdHandle(STD_INPUT_HANDLE);
            let mut mode = 0u32;
            if h.is_null() || GetConsoleMode(h, &mut mode) == 0 {
                return reply;
            }
            SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_INPUT);
            let mut out = std::io::stdout();
            if out.write_all(q).is_ok() && out.flush().is_ok() {
                let deadline = Instant::now() + timeout;
                let mut records = [InputRecord {
                    event_type: 0,
                    _pad: 0,
                    event: [0; 16],
                }; 64];
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero()
                        || WaitForSingleObject(h, left.as_millis() as u32) != WAIT_OBJECT_0
                    {
                        break;
                    }
                    let mut n = 0u32;
                    if ReadConsoleInputW(h, records.as_mut_ptr(), records.len() as u32, &mut n) == 0
                    {
                        break;
                    }
                    for r in &records[..n as usize] {
                        let down =
                            i32::from_le_bytes([r.event[0], r.event[1], r.event[2], r.event[3]]);
                        let ch = u16::from_le_bytes([r.event[10], r.event[11]]);
                        if r.event_type == KEY_EVENT && down != 0 && ch != 0 && ch < 0x80 {
                            reply.push(ch as u8);
                        }
                    }
                    if super::da1_complete(&reply) {
                        break;
                    }
                }
            }
            SetConsoleMode(h, mode);
        }
        reply
    }
}

fn da1_complete(buf: &[u8]) -> bool {
    let s = String::from_utf8_lossy(buf);
    s.find("\x1b[?").is_some_and(|i| s[i..].contains('c'))
}

/// Parse the replies to `CSI 16 t` (`CSI 6 ; h ; w t`) and DA1
/// (`CSI ? a ; b ; … c`). Sixel support is attribute 4 in the DA1 list.
pub fn parse_probe(buf: &[u8]) -> (bool, Option<(u32, u32)>) {
    let s = String::from_utf8_lossy(buf);
    let mut sixel = false;
    let mut cell = None;
    let mut rest = s.as_ref();
    while let Some(i) = rest.find("\x1b[") {
        rest = &rest[i + 2..];
        let end = rest
            .find(|c: char| c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let (params, fin) = (&rest[..end], rest[end..].chars().next());
        match fin {
            Some('c') if params.starts_with('?') => {
                sixel = params[1..].split(';').any(|p| p == "4");
            }
            Some('t') => {
                let v: Vec<u32> = params.split(';').filter_map(|p| p.parse().ok()).collect();
                if v.len() == 3 && v[0] == 6 {
                    cell = Some((v[2], v[1]));
                }
            }
            _ => {}
        }
    }
    (sixel, cell)
}

/// The reply to `CSI 14 t` (`CSI 4 ; height ; width t`): the text area in
/// pixels, as (width, height).
pub fn parse_window_px(buf: &[u8]) -> Option<(u32, u32)> {
    let s = String::from_utf8_lossy(buf);
    let i = s.find("\x1b[4;")?;
    let rest = &s[i + 4..];
    let v: Vec<u32> = rest[..rest.find('t')?]
        .split(';')
        .filter_map(|p| p.parse().ok())
        .collect();
    (v.len() == 2 && v[0] > 0 && v[1] > 0).then(|| (v[1], v[0]))
}

/// Decode `path` to exactly `w` x `h` RGB pixels quantized to at most 256
/// colours (ffmpeg's palettegen + paletteuse, Lanczos scaling).
pub fn decode(path: &Path, w: u32, h: u32) -> Option<Vec<u8>> {
    if !crate::media::ffmpeg_available() || w == 0 || h == 0 {
        return None;
    }
    let graph = format!(
        "[0:v]scale={w}:{h}:flags=lanczos,split[a][b];\
         [a]palettegen=max_colors=256:reserve_transparent=0:stats_mode=single[p];\
         [b][p]paletteuse=dither=sierra2_4a"
    );
    let out = Command::new("ffmpeg")
        .args(["-v", "error"])
        .args({
            let [w, f, url] = crate::media::ffmpeg_input(path);
            [w, f, "-i".into(), url]
        })
        .args([
            "-frames:v",
            "1",
            "-filter_complex",
            &graph,
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-",
        ])
        .output()
        .ok()?;
    (out.status.success() && out.stdout.len() == (w * h * 3) as usize).then_some(out.stdout)
}

/// Encode RGB pixels as a Sixel image. Colours beyond the first 256 distinct
/// ones fall back to the nearest palette entry (ffmpeg's quantizer keeps it
/// at or under 256, so this only matters for callers that skip it).
pub fn encode(rgb: &[u8], w: usize, h: usize) -> String {
    if w == 0 || h == 0 || rgb.len() < w * h * 3 {
        return String::new();
    }
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut index: HashMap<[u8; 3], u8> = HashMap::new();
    let mut pixels = Vec::with_capacity(w * h);
    for px in rgb[..w * h * 3].as_chunks::<3>().0 {
        let c = [px[0], px[1], px[2]];
        let i = match index.get(&c) {
            Some(&i) => i,
            None if palette.len() < 256 => {
                let i = palette.len() as u8;
                palette.push(c);
                index.insert(c, i);
                i
            }
            None => nearest(&palette, c),
        };
        pixels.push(i);
    }

    // P2=1: pixels left at 0 keep whatever is under them.
    let mut out = format!("\x1bP0;1;0q\"1;1;{w};{h}");
    for (i, c) in palette.iter().enumerate() {
        let pct = |v: u8| (u32::from(v) * 100 + 127) / 255;
        out.push_str(&format!("#{i};2;{};{};{}", pct(c[0]), pct(c[1]), pct(c[2])));
    }
    let mut bands: Vec<Vec<u8>> = vec![Vec::new(); palette.len()];
    for top in (0..h).step_by(6) {
        let mut used: Vec<usize> = Vec::new();
        for (dy, y) in (top..(top + 6).min(h)).enumerate() {
            for x in 0..w {
                let c = pixels[y * w + x] as usize;
                if bands[c].is_empty() {
                    bands[c] = vec![0u8; w];
                    used.push(c);
                }
                bands[c][x] |= 1 << dy;
            }
        }
        for (n, &c) in used.iter().enumerate() {
            if n > 0 {
                out.push('$');
            }
            out.push_str(&format!("#{c}"));
            push_run_length(&mut out, &bands[c]);
            bands[c] = Vec::new();
        }
        out.push('-');
    }
    out.push_str("\x1b\\");
    out
}

fn nearest(palette: &[[u8; 3]], c: [u8; 3]) -> u8 {
    let dist = |p: &[u8; 3]| {
        (0..3)
            .map(|k| {
                let d = i32::from(p[k]) - i32::from(c[k]);
                d * d
            })
            .sum::<i32>()
    };
    palette
        .iter()
        .enumerate()
        .min_by_key(|(_, p)| dist(p))
        .map(|(i, _)| i as u8)
        .unwrap_or(0)
}

// One colour's row of sixels, with `!n` repeats for runs longer than three.
fn push_run_length(out: &mut String, bits: &[u8]) {
    let mut i = 0;
    while i < bits.len() {
        let b = bits[i];
        let mut run = 1;
        while i + run < bits.len() && bits[i + run] == b {
            run += 1;
        }
        let ch = char::from(63 + b);
        if run > 3 {
            out.push_str(&format!("!{run}{ch}"));
        } else {
            for _ in 0..run {
                out.push(ch);
            }
        }
        i += run;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cell_size_and_sixel_attribute() {
        let reply = b"\x1b[6;20;10t\x1b[?61;4;6;7;14;21;22;23;24;28;32;42c";
        assert_eq!(parse_probe(reply), (true, Some((10, 20))));
        // A terminal without Sixel (no attribute 4), no cell-size answer.
        assert_eq!(parse_probe(b"\x1b[?62;22c"), (false, None));
        // "14" must not be read as "4".
        assert!(!parse_probe(b"\x1b[?62;14;22c").0);
        assert!(da1_complete(b"\x1b[6;20;10t\x1b[?1;2c"));
        // Window size in pixels, for terminals that skip the cell report.
        assert_eq!(
            parse_window_px(b"\x1b[4;1000;1920t\x1b[?62;4c"),
            Some((1920, 1000))
        );
        assert_eq!(parse_window_px(b"\x1b[?62;4c"), None);
        assert!(!da1_complete(b"\x1b[6;20;10t"));
    }

    #[test]
    fn encodes_a_two_colour_image() {
        // 4x7: top six rows red, last row blue.
        let mut rgb = Vec::new();
        for y in 0..7 {
            for _ in 0..4 {
                rgb.extend_from_slice(if y < 6 { &[255, 0, 0] } else { &[0, 0, 255] });
            }
        }
        let s = encode(&rgb, 4, 7);
        assert!(s.starts_with("\x1bP0;1;0q\"1;1;4;7"), "{s:?}");
        assert!(
            s.contains("#0;2;100;0;0") && s.contains("#1;2;0;0;100"),
            "{s:?}"
        );
        // Band 1: red on all six rows of 4 columns -> "!4~". Band 2: blue on
        // its first row -> 4 x '@'.
        assert!(s.contains("#0!4~-"), "{s:?}");
        assert!(s.contains("#1!4@-"), "{s:?}");
        assert!(s.ends_with("\x1b\\"));
        assert!(encode(&[1, 2], 1, 1).is_empty());
    }

    #[test]
    fn run_length_only_for_runs_over_three() {
        let mut s = String::new();
        push_run_length(&mut s, &[1, 1, 1, 2, 2, 2, 2]);
        assert_eq!(s, "@@@!4A");
    }
}
