//! Inline images: kitty graphics, Sixel and half-block previews sized to the terminal.

use super::*;

// ── inline images ────────────────────────────────────────────────────────────
// Two tiers. Terminals that render kitty Unicode placeholders (kitty,
// Ghostty, WezTerm with placeholder support) get the real pixels: the PNG
// is uploaded once and the transcript carries placeholder rows that scroll,
// wrap and repaint like text (see graphics.rs). Everywhere else the image
// is drawn as truecolor half-block cells sized to the terminal, which needs
// ffmpeg for the decode. Both paths are gated by the `images` setting.

pub(super) const IMAGE_EXTS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

fn images_mode() -> u8 {
    screen::with(|s| s.flags.images_mode)
}

pub(super) fn images_enabled() -> bool {
    images_mode() != 0
}

pub(super) fn use_placeholders() -> bool {
    match images_mode() {
        2 => true,
        3 | 4 | 0 => false,
        _ => crate::graphics::placeholders_supported(),
    }
}

// Sixel previews: the `images` setting picks them explicitly, or `auto`
// follows BWN_IMAGES and the startup probe.
pub(super) fn use_sixel() -> bool {
    match images_mode() {
        4 => true,
        2 | 3 | 0 => false,
        _ => crate::sixel::supported(),
    }
}

// ── inline images that follow the terminal size ───────────────────────────────
// An attached image is one transcript line holding `IMG_MARK` and an id.
// Wrapping expands it to as many rows as the image needs at the current
// terminal size, so a resize re-fits it. Block art renders straight into
// those rows. A Sixel image leaves them as `IMG_MARK id:row` placeholders
// and render_output draws the pixels over them.
pub(super) const IMG_MARK: &str = "\u{F8FF}bwn-img:";

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum ImageTier {
    Blocks,
    Sixel,
}

pub(super) struct InlineImage {
    pub(super) path: std::path::PathBuf,
    pub(super) native: (u32, u32),
    pub(super) tier: ImageTier,
    // The terminal size and cell size the renderings below were made for.
    pub(super) made_for: (usize, usize, (u32, u32)),
    pub(super) rows: usize,
    pub(super) blocks: Vec<String>,
    pub(super) sixel: String,
    // Decoded pixels (width, height, rgb) and cell height behind `sixel`, kept
    // so an image partly scrolled out of view can be drawn cropped.
    pub(super) pixels: (u32, u32, Vec<u8>),
    pub(super) cell_h: u32,
    pub(super) crop: Option<((usize, usize), String)>,
}

impl InlineImage {
    // Re-render for a new terminal size; a no-op when nothing changed.
    pub(super) fn ensure(&mut self, term_w: usize, term_h: usize) {
        let cell = crate::sixel::cell_pixels();
        if self.made_for == (term_w, term_h, cell) && self.rows > 0 {
            return;
        }
        self.made_for = (term_w, term_h, cell);
        let (max_cols, max_rows) = image_cell_budget_for(term_w, term_h, 4);
        if self.tier == ImageTier::Sixel {
            let (cw, ch) = (cell.0.max(1), cell.1.max(1));
            let (nw, nh) = (self.native.0.max(1), self.native.1.max(1));
            let scale = (f64::from(max_cols as u32 * cw) / f64::from(nw))
                .min(f64::from(max_rows as u32 * ch) / f64::from(nh))
                .min(1.0);
            let w = ((f64::from(nw) * scale).round() as u32).max(1);
            let h = ((f64::from(nh) * scale).round() as u32).max(1);
            if let Some(rgb) = crate::sixel::decode(&self.path, w, h) {
                self.sixel = crate::sixel::encode(&rgb, w as usize, h as usize);
                self.rows = h.div_ceil(ch) as usize;
                self.pixels = (w, h, rgb);
                self.cell_h = ch;
                self.crop = None;
                return;
            }
            self.tier = ImageTier::Blocks;
        }
        let px_rows = ((max_rows * 2) as u32).max(2);
        self.blocks = crate::media::decode_thumbnail(&self.path, max_cols as u32, px_rows)
            .map(|(w, h, rgb)| image_preview_cells(&rgb, w, h))
            .unwrap_or_default();
        self.rows = self.blocks.len().max(1);
    }
}

impl InlineImage {
    // Sixel for image rows `first..first + count`: the whole image, or the
    // visible slice of one partly scrolled out of view.
    pub(super) fn sixel_rows(&mut self, first: usize, count: usize) -> String {
        if first == 0 && count >= self.rows {
            return self.sixel.clone();
        }
        if let Some((key, data)) = &self.crop {
            if *key == (first, count) {
                return data.clone();
            }
        }
        let (w, h, rgb) = (&self.pixels.0, &self.pixels.1, &self.pixels.2);
        let (w, h) = (*w as usize, *h as usize);
        let ch = self.cell_h.max(1) as usize;
        let top = (first * ch).min(h);
        let bottom = ((first + count) * ch).min(h);
        if bottom <= top || rgb.len() < w * h * 3 {
            return String::new();
        }
        let data = crate::sixel::encode(&rgb[top * w * 3..bottom * w * 3], w, bottom - top);
        self.crop = Some(((first, count), data.clone()));
        data
    }
}

pub(super) fn inline_images() -> &'static Mutex<Vec<InlineImage>> {
    static IMAGES: OnceLock<Mutex<Vec<InlineImage>>> = OnceLock::new();
    IMAGES.get_or_init(|| Mutex::new(Vec::new()))
}

// Something other than render_output may have drawn over the output rows (a
// popup, a clear, a resize): the next frame redraws every Sixel image
// instead of keeping the ones that did not move.
pub(super) fn invalidate_inline_pixels() {
    screen::with(|s| s.images.dirty = true);
}

// Rows for an image marker line at the current size, or None when `line`
// is not a registered image (a row placeholder, or an unknown id).
pub(super) fn inline_image_rows(line: &str, width: usize) -> Option<Vec<String>> {
    let id: usize = line.strip_prefix(IMG_MARK)?.parse().ok()?;
    let height = term_size().1 as usize;
    let mut images = inline_images().lock().ok()?;
    let img = images.get_mut(id)?;
    img.ensure(width, height);
    Some(match img.tier {
        ImageTier::Blocks if img.blocks.is_empty() => vec![String::new()],
        ImageTier::Blocks => img.blocks.clone(),
        ImageTier::Sixel => (0..img.rows)
            .map(|r| format!("{IMG_MARK}{id}:{r}"))
            .collect(),
    })
}

// A Sixel row placeholder: (image id, row within the image).
pub(super) fn sixel_row(line: &str) -> Option<(usize, usize)> {
    let (id, row) = line.strip_prefix(IMG_MARK)?.split_once(':')?;
    Some((id.parse().ok()?, row.parse().ok()?))
}

// The visible part of each Sixel image: (id, first screen row, first image
// row, rows). An image partly scrolled out of view is drawn cropped.
pub(super) fn sixel_placements(visible: &[&String], rows: usize) -> Vec<Placement> {
    let mut out = Vec::new();
    let mut screen_row = 0;
    while screen_row < visible.len().min(rows) {
        let Some((id, first)) = sixel_row(visible[screen_row]) else {
            screen_row += 1;
            continue;
        };
        let mut n = 1;
        while screen_row + n < rows
            && visible
                .get(screen_row + n)
                .and_then(|l| sixel_row(l))
                .is_some_and(|(i, r)| i == id && r == first + n)
        {
            n += 1;
        }
        out.push((id, screen_row, first, n));
        screen_row += n;
    }
    out
}

pub(super) type Placement = (usize, usize, usize, usize);

// Cell budget for an inline image: a thumbnail, not a full-screen picture.
// At most half the width (80 columns) and a third of the rows above the
// composer (16 rows), so the conversation stays in view; the image keeps its
// aspect ratio inside that box.
pub(super) fn image_cell_budget() -> (usize, usize) {
    let (w, h) = term_size();
    image_cell_budget_for(w as usize, h as usize, reserved_rows() as usize)
}

pub(super) fn image_cell_budget_for(
    width: usize,
    height: usize,
    reserved: usize,
) -> (usize, usize) {
    let cols = (width.saturating_sub(4) / 2).clamp(8, 80);
    let rows = (height.saturating_sub(reserved + 4) / 3).clamp(4, 16);
    (cols, rows)
}

pub(super) fn free_images() {
    let sent = screen::with(|s| {
        s.images.dirty = true;
        std::mem::replace(&mut s.images.sent, false)
    });
    if sent {
        let _ = write!(io::stdout(), "{}", crate::graphics::delete_all());
        flush();
    }
}

/// Show `path` (an image, or a video's first frame via ffmpeg) inline in
/// the transcript. With `once`, a path already shown this session is
/// skipped (a pasted screenshot previews at paste time and must not repeat
/// on submit). Returns whether anything was drawn.
pub fn show_image_file(path: &std::path::Path, once: bool) -> bool {
    if !images_enabled() || no_color() {
        return false;
    }
    let seen = !screen::with(|s| s.images.previewed.insert(path.to_path_buf()));
    if seen && once {
        return true;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string());
    let name = sanitize_terminal(&name).into_owned();
    let link = file_link(&path.display().to_string(), &name);
    let (max_cols, max_rows) = image_cell_budget();
    if ALT_SCREEN.load(Ordering::Relaxed) && use_placeholders() {
        if let Some((png, iw, ih)) = crate::media::png_for_terminal(path, 1600) {
            let cell = crate::graphics::cell_pixels();
            let (cols, rows) = crate::graphics::fit_cells(iw, ih, max_cols, max_rows, cell);
            let id = screen::with(|s| {
                let id = s.images.next_id;
                s.images.next_id = id.wrapping_add(1);
                id
            });
            let _ = write!(
                io::stdout(),
                "{}",
                crate::graphics::transmit(&png, id, cols, rows)
            );
            flush();
            screen::with(|s| s.images.sent = true);
            let mut rows_out = vec![format!(
                "  {} {} {}",
                dim("⎘"),
                link,
                dim(&format!("· {iw}×{ih}"))
            )];
            rows_out.extend(crate::graphics::placeholder_rows(id, cols, rows, 2));
            line(&rows_out.join("\n"));
            return true;
        }
    }
    // Real pixels over Sixel where the terminal has it, else block art. In
    // the TUI the image is one marker line that re-renders at every terminal
    // size; outside it, the image is printed once at the current size.
    let sixel = use_sixel() && crate::media::ffmpeg_available();
    if sixel || (ALT_SCREEN.load(Ordering::Relaxed) && truecolor()) {
        if let Some(native) = crate::media::probe_dims(path) {
            let mut img = InlineImage {
                path: path.to_path_buf(),
                native,
                tier: if sixel {
                    ImageTier::Sixel
                } else {
                    ImageTier::Blocks
                },
                made_for: (0, 0, (0, 0)),
                rows: 0,
                blocks: Vec::new(),
                sixel: String::new(),
                pixels: (0, 0, Vec::new()),
                cell_h: 0,
                crop: None,
            };
            let (w, h) = term_size();
            img.ensure(w as usize, h as usize);
            let header = format!(
                "  {} {} {}",
                dim("⎘"),
                link,
                dim(&format!("· {}×{}", native.0, native.1))
            );
            let drawable = match img.tier {
                ImageTier::Sixel => !img.sixel.is_empty(),
                ImageTier::Blocks => !img.blocks.is_empty(),
            };
            if drawable && ALT_SCREEN.load(Ordering::Relaxed) {
                let id = inline_images().lock().ok().map(|mut imgs| {
                    imgs.push(img);
                    imgs.len() - 1
                });
                if let Some(id) = id {
                    line(&format!("{header}\n{IMG_MARK}{id}"));
                    return true;
                }
            } else if drawable && img.tier == ImageTier::Sixel {
                line(&header);
                print!("  {}{}", img.sixel, if is_raw() { "\r\n" } else { "\n" });
                flush();
                return true;
            }
        }
    }
    // Half-block fallback: one text row shows two pixel rows.
    if !truecolor() {
        return false;
    }
    let px_rows = ((max_rows * 2) as u32).max(2);
    if let Some((tw, th, rgb)) = crate::media::decode_thumbnail(path, max_cols as u32, px_rows) {
        let rows = image_preview_cells(&rgb, tw, th);
        if !rows.is_empty() {
            let mut out = vec![format!("  {} {}", dim("⎘"), link)];
            out.extend(rows);
            line(&out.join("\n"));
            return true;
        }
    }
    false
}

/// `@path` token for the composer, quoted when the path needs it (shlex
/// splits the prompt at submit time).
pub fn attachment_token(path: &std::path::Path) -> String {
    let p = path.display().to_string();
    if p.chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\'')
    {
        format!("@\"{}\" ", p.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        format!("@{p} ")
    }
}

/// A pasted (or drag-and-dropped) single file path to an image or video:
/// quotes, `file://`, shell escapes and `~` stripped; must exist. Anything
/// else — prose, multi-line text, non-media files — is None.
pub fn pasted_media_path(text: &str) -> Option<std::path::PathBuf> {
    let t = text.trim();
    if t.is_empty() || t.len() > 4096 || t.contains('\n') || t.contains('\r') {
        return None;
    }
    let mut t = t.to_string();
    for q in ['"', '\''] {
        if t.len() >= 2 && t.starts_with(q) && t.ends_with(q) {
            t = t[1..t.len() - 1].to_string();
        }
    }
    if let Some(rest) = t.strip_prefix("file://") {
        t = rest.to_string();
    }
    // Shell-escaped spaces from a drag-and-drop (`My\ Shot.png`).
    if t.contains("\\ ") {
        t = t.replace("\\ ", " ");
    }
    if let Some(rest) = t.strip_prefix("~/") {
        if let Some(home) = crate::config::user_home() {
            t = home.join(rest).display().to_string();
        }
    }
    let path = std::path::PathBuf::from(&t);
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if !IMAGE_EXTS.contains(&ext.as_str()) && !crate::media::VIDEO_EXTS.contains(&ext.as_str()) {
        return None;
    }
    path.is_file().then_some(path)
}

// ── inline image preview ─────────────────────────────────────────────────────
// Renders RGB pixels as half-block cells: each character shows two vertically
// stacked pixels ('▀' with fg = top pixel, bg = bottom pixel). Works in every
// truecolor terminal — no kitty/iTerm2 graphics protocol needed — and the
// result is ordinary transcript lines, so scrolling, wrapping, and repaints
// all behave. Returns no lines when color is off or the buffer is malformed.
pub fn image_preview(rgb: &[u8], w: u32, h: u32) -> Vec<String> {
    if no_color() || !truecolor() {
        return Vec::new();
    }
    image_preview_cells(rgb, w, h)
}

// Pure renderer (no terminal-capability gate) — unit-testable.
pub(super) fn image_preview_cells(rgb: &[u8], w: u32, h: u32) -> Vec<String> {
    let (w, h) = (w as usize, h as usize);
    if w == 0 || h == 0 || rgb.len() < w * h * 3 {
        return Vec::new();
    }
    let px = |x: usize, y: usize| {
        let i = (y * w + x) * 3;
        (rgb[i], rgb[i + 1], rgb[i + 2])
    };
    let mut out = Vec::with_capacity(h.div_ceil(2));
    for row in 0..h.div_ceil(2) {
        let mut line = String::with_capacity(w * 24 + 16);
        line.push_str("  ");
        for x in 0..w {
            let (tr, tg, tb) = px(x, row * 2);
            if row * 2 + 1 < h {
                let (br, bg, bb) = px(x, row * 2 + 1);
                line.push_str(&format!("\x1b[38;2;{tr};{tg};{tb};48;2;{br};{bg};{bb}m▀"));
            } else {
                line.push_str(&format!("\x1b[38;2;{tr};{tg};{tb}m▀"));
            }
        }
        line.push_str(&reset_all());
        line.push_str(&reset_bg());
        out.push(line);
    }
    out
}

// Terminal width for layout done outside this module (diff gutters).
pub fn term_width() -> usize {
    term_size().0 as usize
}
