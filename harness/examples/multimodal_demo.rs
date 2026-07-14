// Scripted multimodal session for docs screenshots: a pasted clipboard
// screenshot and an ffmpeg-parsed video attachment, rendered through the
// production TUI pipeline. Run under a PTY and capture (see tui_demo).

use buildwithnexus::{report, tui};

// Synthetic "app screenshot" RGB buffer for the inline-preview demo, drawn at
// 64x36 — the same cap decode_thumbnail applies to real attachments. Window
// dots, header, sidebar nav, text lines, a primary button, and the offending
// red dropdown overlapping the header.
fn fake_screenshot() -> (u32, u32, Vec<u8>) {
    let (w, h) = (64usize, 36usize);
    let mut rgb = vec![0u8; w * h * 3];
    let mut fill = |x0: usize, y0: usize, x1: usize, y1: usize, c: (u8, u8, u8)| {
        for y in y0..y1 {
            for x in x0..x1 {
                let i = (y * w + x) * 3;
                rgb[i] = c.0;
                rgb[i + 1] = c.1;
                rgb[i + 2] = c.2;
            }
        }
    };
    fill(0, 0, w, h, (0x1a, 0x1b, 0x26)); // app background
    fill(0, 0, w, 6, (0x3d, 0x59, 0xa1)); // header bar
    fill(2, 2, 4, 4, (0xf7, 0x76, 0x8e)); // window dots
    fill(5, 2, 7, 4, (0xe0, 0xaf, 0x68));
    fill(8, 2, 10, 4, (0x9e, 0xce, 0x6a));
    fill(14, 2, 34, 4, (0x56, 0x6b, 0xb8)); // title
    fill(0, 6, 14, h, (0x24, 0x28, 0x3b)); // sidebar
    for (i, y) in [9usize, 13, 17, 21].into_iter().enumerate() {
        let c = if i == 1 {
            (0x7a, 0xa2, 0xf7) // active nav item
        } else {
            (0x41, 0x48, 0x68)
        };
        fill(2, y, 12, y + 2, c);
    }
    for (i, tw) in [41usize, 58, 52, 33, 55, 47].into_iter().enumerate() {
        let y = 8 + i * 4;
        fill(17, y, tw, y + 2, (0x56, 0x5f, 0x89)); // content text lines
    }
    fill(17, 31, 29, 34, (0x7a, 0xa2, 0xf7)); // primary button
    fill(36, 3, 62, 16, (0xf7, 0x76, 0x8e)); // the dropdown over the header
    fill(37, 4, 61, 15, (0x2a, 0x1f, 0x2c));
    for (i, y) in [6usize, 9, 12].into_iter().enumerate() {
        let c = if i == 1 {
            (0x3b, 0x42, 0x61) // hovered menu row
        } else {
            (0x56, 0x5f, 0x89)
        };
        fill(39, y, 58, y + 2, c);
    }
    (w as u32, h as u32, rgb)
}

fn main() {
    tui::enter_alt(true);
    tui::set_permission_mode("ask");
    tui::show_banner(
        "anthropic",
        "claude-sonnet-4-5",
        "BUILD",
        "/home/garrett/acme-app",
    );
    tui::line("");

    // ── clipboard screenshot paste (Ctrl+V) ─────────────────────────────
    tui::line(&tui::dim("  ⎘ clipboard image → /tmp/bwn-paste-4021-1.png"));
    tui::line(&format!(
        "{} {}",
        tui::accent("›"),
        "why does the dropdown overlap the header? @/tmp/bwn-paste-4021-1.png"
    ));
    tui::line(&tui::dim("  ⎘ attached 1 image"));
    let (tw, th, rgb) = fake_screenshot();
    let preview = tui::image_preview(&rgb, tw, th);
    if !preview.is_empty() {
        tui::line(&preview.join("\n"));
    }
    tui::line("");
    tui::line(&tui::render_md(
        "Looking at your screenshot: the dropdown renders under a header with \
         `position: sticky` but a **lower z-index** — the menu's `z-index: 10` loses to the \
         header's `50`. Bump the dropdown to `60` or portal it out of the header's stacking \
         context.",
    ));
    tui::line("");

    // ── video attachment via ffmpeg ─────────────────────────────────────
    tui::line(&format!(
        "{} {}",
        tui::accent("›"),
        "the flicker happens mid-animation — see @demo.mp4"
    ));
    tui::line(&tui::dim("  ⎘ parsing video demo.mp4 with ffmpeg…"));
    tui::line(&tui::dim("  ⎘ attached 8 images"));
    tui::line(&tui::dim(
        "    [video: demo.mp4 — 6.4s, 1280x720, 30 fps; 8 frames sampled evenly across the clip, in order]",
    ));
    tui::line("");
    tui::line(&tui::render_md(
        "Between frames 4 and 5 (~2.8s) the sidebar unmounts for one frame — that's the \
         flicker. The `key` on `<Sidebar>` changes when the route updates, forcing a full \
         remount instead of a re-render. Keep the key stable and the animation holds.",
    ));

    tui::line("");

    // ── docx generation ─────────────────────────────────────────────────
    tui::line(&format!(
        "{} {}",
        tui::accent("›"),
        "write the findings up as a report I can share"
    ));
    tui::line("");
    report::tool_call(
        "create_docx",
        "create docx ui-findings.docx",
        &serde_json::json!({"path": "ui-findings.docx"}),
    );
    report::finish(
        "Wrote `ui-findings.docx` — headings, the annotated screenshot, and the fix as a \
         styled table. The path above is clickable and opens in Word.",
    );

    // Composer stays live for the capture.
    while tui::ask_task(&format!("{} ", tui::accent("›"))).is_some() {}
    tui::leave_alt();
}
