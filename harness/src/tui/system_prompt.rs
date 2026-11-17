//! `/system-prompt` panes. Colours come from the theme, so a later design
//! pass changes this view with the rest of the screen.

use std::io::{self, IsTerminal};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyCode, KeyEvent, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use super::{Overlay, OverlayEvent};
use crate::system_prompt::{
    apply_hunks, diff_hunks, estimate_tokens, redact_text, Assembly, Hunk, ESTIMATE,
};

pub(crate) enum ShowAction {
    Close,
    Edit,
    Suggest,
}

struct Painted {
    lines: Vec<String>,
    /// Scroll offset of each labeled section, in `lines`.
    starts: Vec<(&'static str, usize)>,
}

pub(crate) fn show_view(asm: &Assembly) -> ShowAction {
    let overlay = Overlay::open();
    if !overlay.is_live() {
        let painted = paint_show(asm, 80);
        for line in &painted.lines {
            println!("{line}");
        }
        return ShowAction::Close;
    }
    let mut scroll = 0usize;
    let mut section = 0usize;
    loop {
        let (cols, rows) = overlay.size();
        let painted = paint_show(asm, cols as usize);
        let body_rows = rows.saturating_sub(2) as usize;
        let max_scroll = painted.lines.len().saturating_sub(body_rows.max(1));
        scroll = scroll.min(max_scroll);
        let mut frame = painted
            .lines
            .iter()
            .skip(scroll)
            .take(body_rows)
            .cloned()
            .collect::<Vec<_>>();
        while frame.len() < body_rows {
            frame.push(String::new());
        }
        frame.push(super::rule());
        frame.push(show_hint(cols as usize));
        overlay.draw(&frame);
        match overlay.read_event() {
            OverlayEvent::Key(k) => match k.code {
                KeyCode::Char('q') | KeyCode::Esc => return ShowAction::Close,
                KeyCode::Char('e') => return ShowAction::Edit,
                KeyCode::Char('s') => return ShowAction::Suggest,
                KeyCode::Char('j') | KeyCode::Down => scroll = scroll.saturating_add(1),
                KeyCode::Char('k') | KeyCode::Up => scroll = scroll.saturating_sub(1),
                KeyCode::Char('g') | KeyCode::Home => scroll = 0,
                KeyCode::Char('G') | KeyCode::End => scroll = max_scroll,
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    scroll = scroll.saturating_add(body_rows.max(1)).min(max_scroll);
                }
                KeyCode::PageUp => scroll = scroll.saturating_sub(body_rows.max(1)),
                KeyCode::Char('n') if !painted.starts.is_empty() => {
                    section = (section + 1).min(painted.starts.len() - 1);
                    scroll = painted.starts[section].1;
                }
                KeyCode::Char('p') if !painted.starts.is_empty() => {
                    section = section.saturating_sub(1);
                    scroll = painted.starts[section].1;
                }
                KeyCode::Char('u') => {
                    if let Some((i, (_, at))) = painted
                        .starts
                        .iter()
                        .enumerate()
                        .find(|(_, (layer, _))| *layer == "user override")
                    {
                        section = i;
                        scroll = *at;
                    }
                }
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    return ShowAction::Close;
                }
                _ => {}
            },
            OverlayEvent::Resize => {}
            OverlayEvent::Closed => return ShowAction::Close,
        }
    }
}

pub(crate) fn confirm_save(scope: &str, path: &str, old: &str, new: &str) -> Result<bool, String> {
    let hunks = diff_hunks(old, new);
    let overlay = Overlay::open();
    let title = format!("save {scope} override  {path}");
    if !overlay.is_live() {
        for line in diff_frame(&title, &hunks, None, 80) {
            println!("{line}");
        }
        return Err("no terminal to confirm the save".into());
    }
    loop {
        let (cols, rows) = overlay.size();
        let lines = diff_frame(&title, &hunks, None, cols as usize);
        draw_scroll(&overlay, &lines, rows, " y save    n discard");
        match overlay.read_event() {
            OverlayEvent::Key(k) => match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(true),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                    return Ok(false);
                }
                _ => {}
            },
            OverlayEvent::Closed => return Ok(false),
            OverlayEvent::Resize => {}
        }
    }
}

/// Per-hunk review. `Ok(None)` is cancel. Nothing is accepted until `a`.
pub(crate) fn review_hunks(
    path: &str,
    old: &str,
    proposed: &str,
) -> Result<Option<String>, String> {
    let hunks = diff_hunks(old, proposed);
    if hunks.is_empty() {
        return Ok(Some(old.to_string()));
    }
    let overlay = Overlay::open();
    if !overlay.is_live() {
        let title = format!("suggestion  {path}");
        for line in diff_frame(&title, &hunks, None, 80) {
            println!("{line}");
        }
        return Err("no terminal to accept hunks".into());
    }
    let mut accepted = vec![false; hunks.len()];
    let mut cursor = 0usize;
    loop {
        let (cols, rows) = overlay.size();
        let title = format!("suggestion  {path}  hunk {}/{}", cursor + 1, hunks.len());
        let lines = diff_frame(&title, &hunks, Some((cursor, &accepted)), cols as usize);
        draw_scroll(
            &overlay,
            &lines,
            rows,
            " a accept   r reject   n/p hunk   y write accepted   q cancel",
        );
        match overlay.read_event() {
            OverlayEvent::Key(k) => match k.code {
                KeyCode::Char('a') => {
                    accepted[cursor] = true;
                    cursor = (cursor + 1).min(hunks.len() - 1);
                }
                KeyCode::Char('r') => {
                    accepted[cursor] = false;
                    cursor = (cursor + 1).min(hunks.len() - 1);
                }
                KeyCode::Char('n') | KeyCode::Down => {
                    cursor = (cursor + 1).min(hunks.len() - 1);
                }
                KeyCode::Char('p') | KeyCode::Up => cursor = cursor.saturating_sub(1),
                KeyCode::Char('y') => return Ok(Some(apply_hunks(old, &hunks, &accepted))),
                KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                _ => {}
            },
            OverlayEvent::Closed => return Ok(None),
            OverlayEvent::Resize => {}
        }
    }
}

pub(crate) fn edit_text(current: &str) -> Result<Option<String>, String> {
    if let Some(text) = external_edit(current)? {
        return Ok(Some(text));
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("set $VISUAL or $EDITOR to edit the override".into());
    }
    Ok(edit_in_tui(current))
}

fn external_edit(current: &str) -> Result<Option<String>, String> {
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|s| !s.trim().is_empty())
        });
    let Some(editor) = editor else {
        return Ok(None);
    };
    let path = std::env::temp_dir().join(format!("bwn-system-prompt-{}.md", std::process::id()));
    std::fs::write(&path, current).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    let was_raw = super::is_raw();
    if was_raw {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
    }
    let mut parts = editor.split_whitespace();
    let cmd = parts.next().unwrap_or("vi");
    let started = std::process::Command::new(cmd)
        .args(parts)
        .arg(&path)
        .status();
    if was_raw {
        let _ = enable_raw_mode();
        let _ = execute!(io::stdout(), EnableBracketedPaste);
    }
    let content = std::fs::read_to_string(&path).unwrap_or_else(|_| current.to_string());
    let _ = std::fs::remove_file(&path);
    match started {
        Ok(_) => Ok(Some(content)),
        Err(_) => Ok(None),
    }
}

fn edit_in_tui(current: &str) -> Option<String> {
    let mut ed = Editor::from_text(current);
    let overlay = Overlay::open();
    if !overlay.is_live() {
        return None;
    }
    loop {
        let (cols, rows) = overlay.size();
        let lines = ed.paint(cols as usize);
        draw_scroll(&overlay, &lines, rows, " ctrl-s save    esc cancel");
        match overlay.read_event() {
            OverlayEvent::Key(k) => match ed.key(k) {
                EditStep::Save => return Some(ed.text()),
                EditStep::Cancel => return None,
                EditStep::Redraw => {}
            },
            OverlayEvent::Closed => return None,
            OverlayEvent::Resize => {}
        }
    }
}

fn draw_scroll(overlay: &Overlay, lines: &[String], rows: u16, footer: &str) {
    let body_rows = rows.saturating_sub(2) as usize;
    let mut frame: Vec<String> = lines.iter().take(body_rows).cloned().collect();
    while frame.len() < body_rows {
        frame.push(String::new());
    }
    let cols = overlay.size().0 as usize;
    frame.push(super::rule());
    frame.push(clip(&super::dim(&format!(" {footer}")), cols));
    overlay.draw(&frame);
}

fn paint_show(asm: &Assembly, width: usize) -> Painted {
    let width = width.max(20);
    let mut lines = Vec::new();
    let mut starts = Vec::new();
    let total = asm.system_tokens();
    lines.push(clip(
        &super::accent(&format!(
            " system prompt   {}   ~{total} tokens {ESTIMATE}",
            asm.mode.label()
        )),
        width,
    ));
    if asm.compact() {
        lines.push(clip(
            &super::dim(" compact prompt: this context window leaves out the long sections"),
            width,
        ));
    }
    lines.push(super::rule());
    for section in &asm.sections {
        starts.push((section.layer, lines.len()));
        let tokens = estimate_tokens(&section.text);
        let marker = if section.in_system { "" } else { "  not sent" };
        lines.push(clip(
            &format!(
                "{} {}{}",
                super::accent(&format!(" {} ", section.layer)),
                super::dim(&section.source),
                super::dim(&format!("  ~{tokens} est{marker}"))
            ),
            width,
        ));
        let shown = redact_text(&section.text);
        let rendered = super::render_md_width(&shown, width.saturating_sub(2));
        for raw in rendered.split('\n') {
            if super::strip_ansi(raw).is_empty() {
                lines.push(String::new());
                continue;
            }
            for row in super::wrap_ansi_line(raw, width.saturating_sub(2)) {
                lines.push(paint_redacted(&format!("  {row}")));
            }
        }
        lines.push(String::new());
    }
    Painted { lines, starts }
}

fn diff_frame(
    title: &str,
    hunks: &[Hunk],
    focus: Option<(usize, &[bool])>,
    width: usize,
) -> Vec<String> {
    let width = width.max(20);
    let mut lines = vec![
        clip(&super::accent(&format!(" {title}")), width),
        clip(
            &super::dim(" token counts on the prompt are estimates; this is the override diff"),
            width,
        ),
        super::rule(),
    ];
    if hunks.is_empty() {
        lines.push(super::dim("  no changes"));
        return lines;
    }
    for (i, hunk) in hunks.iter().enumerate() {
        let state = match focus {
            Some((cursor, accepted)) => {
                let mark = if accepted[i] {
                    super::green(" accept")
                } else {
                    super::dim(" pending")
                };
                let here = if cursor == i {
                    super::accent(" >")
                } else {
                    "  ".to_string()
                };
                format!("{here} hunk {}  line {}{mark}", i + 1, hunk.old_line)
            }
            None => format!("  hunk {}  line {}", i + 1, hunk.old_line),
        };
        lines.push(clip(&state, width));
        for row in &hunk.removed {
            let shown = paint_redacted(&redact_text(row));
            for piece in wrap(&format!("-{shown}"), width.saturating_sub(2)) {
                lines.push(format!("  {}", super::diff_del_span(&piece)));
            }
        }
        for row in &hunk.added {
            let shown = paint_redacted(&redact_text(row));
            for piece in wrap(&format!("+{shown}"), width.saturating_sub(2)) {
                lines.push(format!("  {}", super::diff_add_span(&piece)));
            }
        }
        lines.push(String::new());
    }
    lines
}

fn paint_redacted(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(at) = rest.find("[redacted]") {
        out.push_str(&rest[..at]);
        out.push_str(&super::red("[redacted]"));
        rest = &rest[at + "[redacted]".len()..];
    }
    out.push_str(rest);
    out
}

fn wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    // Callers pass plain text. A break prefers the last space so a word is
    // not cut in the middle of the pane.
    let mut rest = super::strip_ansi(s);
    if rest.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    while !rest.is_empty() {
        if super::str_width(&rest) <= width {
            out.push(rest);
            break;
        }
        let mut end = 0usize;
        let mut w = 0usize;
        let mut last_space = None;
        for (i, ch) in rest.char_indices() {
            let cw = super::str_width(&ch.to_string()).max(1);
            if w + cw > width && end > 0 {
                break;
            }
            end = i + ch.len_utf8();
            w += cw;
            if ch == ' ' {
                last_space = Some(end);
            }
        }
        let cut = last_space
            .filter(|at| *at > 0 && *at < rest.len())
            .unwrap_or(end);
        let cut = cut.max(rest.chars().next().map(|c| c.len_utf8()).unwrap_or(1));
        let (line, next) = rest.split_at(cut.min(rest.len()));
        let line = line.trim_end().to_string();
        let next = next.trim_start().to_string();
        out.push(line);
        rest = next;
    }
    out
}

/// Hint under the show pane. `n/p section` is there when the line fits.
fn show_hint(cols: usize) -> String {
    let full = " j/k scroll   n/p section   e edit   s suggest   u your layer   q close";
    let short = " j/k scroll   e edit   s suggest   u your layer   q close";
    let text = if super::str_width(full) <= cols {
        full
    } else {
        short
    };
    clip(&super::dim(text), cols)
}

fn clip(s: &str, cols: usize) -> String {
    // The overlay cuts anything past the pane with no marker. A header has
    // to finish inside `cols`, ellipsis included, or the last word is sliced
    // (`bundled.` with the closer gone).
    if cols == 0 {
        return String::new();
    }
    if super::str_width(&super::strip_ansi(s)) <= cols {
        return s.to_string();
    }
    let ellipsis = '…';
    let limit = cols.saturating_sub(super::char_width(ellipsis));
    let mut kept = String::new();
    let mut w = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            kept.push(c);
            for n in chars.by_ref() {
                kept.push(n);
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        let cw = super::char_width(c);
        if w + cw > limit {
            break;
        }
        kept.push(c);
        w += cw;
    }
    kept.push(ellipsis);
    kept
}

// ── in-TUI editor ─────────────────────────────────────────────────────────────

struct Editor {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

enum EditStep {
    Redraw,
    Save,
    Cancel,
}

impl Editor {
    fn from_text(text: &str) -> Self {
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Self {
            lines,
            row: 0,
            col: 0,
        }
    }

    fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn key(&mut self, key: KeyEvent) -> EditStep {
        match key.code {
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => EditStep::Save,
            KeyCode::Esc => EditStep::Cancel,
            KeyCode::Enter => {
                let line = self.lines[self.row].clone();
                let (left, right) = split_at_char(&line, self.col);
                self.lines[self.row] = left;
                self.row += 1;
                self.lines.insert(self.row, right);
                self.col = 0;
                EditStep::Redraw
            }
            KeyCode::Backspace => {
                if self.col > 0 {
                    let line = self.lines[self.row].clone();
                    let (mut left, right) = split_at_char(&line, self.col);
                    left.pop();
                    self.col -= 1;
                    self.lines[self.row] = format!("{left}{right}");
                } else if self.row > 0 {
                    let taken = self.lines.remove(self.row);
                    self.row -= 1;
                    self.col = self.lines[self.row].chars().count();
                    self.lines[self.row].push_str(&taken);
                }
                EditStep::Redraw
            }
            KeyCode::Left => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.lines[self.row].chars().count();
                }
                EditStep::Redraw
            }
            KeyCode::Right => {
                if self.col < self.lines[self.row].chars().count() {
                    self.col += 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
                EditStep::Redraw
            }
            KeyCode::Up if self.row > 0 => {
                self.row -= 1;
                self.clamp_col();
                EditStep::Redraw
            }
            KeyCode::Down if self.row + 1 < self.lines.len() => {
                self.row += 1;
                self.clamp_col();
                EditStep::Redraw
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let line = self.lines[self.row].clone();
                let (mut left, right) = split_at_char(&line, self.col);
                left.push(c);
                self.col += 1;
                self.lines[self.row] = format!("{left}{right}");
                EditStep::Redraw
            }
            _ => EditStep::Redraw,
        }
    }

    fn clamp_col(&mut self) {
        let n = self.lines[self.row].chars().count();
        if self.col > n {
            self.col = n;
        }
    }

    fn paint(&self, width: usize) -> Vec<String> {
        let mut lines = vec![
            super::accent(" edit your layer"),
            super::dim(" the built-in prompt stays as it is; this file is appended"),
            super::rule(),
        ];
        for (i, line) in self.lines.iter().enumerate() {
            let shown = if i == self.row {
                cursor_line(line, self.col)
            } else {
                line.clone()
            };
            lines.push(format!("  {shown}"));
        }
        lines.push(String::new());
        lines.push(super::dim(" preview"));
        let rendered = super::render_md_width(&self.text(), width.saturating_sub(2).max(4));
        for row in rendered.split('\n') {
            lines.push(format!("  {row}"));
        }
        lines
    }
}

fn cursor_line(line: &str, col: usize) -> String {
    let chars: Vec<char> = line.chars().collect();
    let col = col.min(chars.len());
    let before: String = chars[..col].iter().collect();
    let at = chars
        .get(col)
        .copied()
        .map(|c| c.to_string())
        .unwrap_or_else(|| " ".into());
    let after: String = chars.get(col + 1..).unwrap_or(&[]).iter().collect();
    format!("{before}{}{after}", super::accent(&at))
}

fn split_at_char(s: &str, chars: usize) -> (String, String) {
    let mut left = String::new();
    for (count, ch) in s.chars().enumerate() {
        if count == chars {
            let rest = s[left.len()..].to_string();
            return (left, rest);
        }
        left.push(ch);
    }
    (left, String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::PromptMode;
    use crate::system_prompt::Section;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn the_fallback_editor_inserts_and_breaks_lines() {
        let mut ed = Editor::from_text("one");
        assert!(matches!(ed.key(key(KeyCode::Char('!'))), EditStep::Redraw));
        assert_eq!(ed.text(), "!one");
        assert!(matches!(ed.key(key(KeyCode::Enter)), EditStep::Redraw));
        assert_eq!(ed.text(), "!\none");
        assert!(matches!(ed.key(key(KeyCode::Backspace)), EditStep::Redraw));
        assert_eq!(ed.text(), "!one");
        assert!(matches!(
            ed.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            EditStep::Save
        ));
    }

    #[test]
    fn show_renders_markdown_in_the_layer() {
        let asm = Assembly {
            mode: PromptMode::Build,
            context_tokens: 200_000,
            sections: vec![Section {
                layer: "user override",
                source: "user".into(),
                text: "# Rules\n\n- use **tabs**\n".into(),
                in_system: true,
            }],
        };
        let plain = super::super::strip_ansi(&paint_show(&asm, 80).lines.join("\n"));
        assert!(plain.contains("Rules"), "{plain}");
        assert!(plain.contains("•"), "{plain}");
        assert!(plain.contains("tabs"), "{plain}");
        assert!(!plain.contains("# Rules"), "{plain}");
        assert!(!plain.contains("**"), "{plain}");
    }

    #[test]
    fn edit_preview_renders_the_buffer() {
        let ed = Editor::from_text("# Hello\n\n**bold** and `code`\n");
        let plain = super::super::strip_ansi(&ed.paint(80).join("\n"));
        let preview = plain.split("preview").nth(1).expect("preview");
        assert!(preview.contains("Hello"), "{preview}");
        assert!(!preview.contains("# Hello"), "{preview}");
        assert!(
            preview.contains("bold") && !preview.contains("**"),
            "{preview}"
        );
        assert!(
            preview.contains("code") && !preview.contains('`'),
            "{preview}"
        );
    }

    #[test]
    fn show_lines_name_the_layer_and_redact_secrets() {
        let secret = "sk-abcdefghijklmnopqrstuvwxyz123456";
        let asm = Assembly {
            mode: PromptMode::Build,
            context_tokens: 200_000,
            sections: vec![Section {
                layer: "user override",
                source: "/tmp/system.md".into(),
                text: format!("keep\n{secret}\n"),
                in_system: true,
            }],
        };
        let painted = paint_show(&asm, 80);
        let plain = super::super::strip_ansi(&painted.lines.join("\n"));
        assert!(plain.contains("user override"));
        assert!(plain.contains("/tmp/system.md"));
        assert!(plain.contains("[redacted]"));
        assert!(!plain.contains(secret), "{plain}");
        assert!(plain.contains(ESTIMATE));
        assert_eq!(painted.starts[0].0, "user override");
    }

    /// The skills header used to run past 80 columns and the terminal cut it
    /// at `rust-cli (bundled.` — the closer and everything after it gone.
    #[test]
    fn skills_header_truncates_inside_80_columns() {
        let source = "self-knowledge (bundled), codebase-repair (bundled), rust-cli (bundled), tool-use (bundled), git (bundled), git-release (bundled)";
        let asm = Assembly {
            mode: PromptMode::Build,
            context_tokens: 200_000,
            sections: vec![Section {
                layer: "skills",
                source: source.into(),
                text: "• /self-knowledge — look it up\n".into(),
                in_system: true,
            }],
        };
        let broken =
            "skills  self-knowledge (bundled), codebase-repair (bundled), rust-cli (bundled.";
        for width in [80usize, 100, 120] {
            let painted = paint_show(&asm, width);
            let header = painted
                .lines
                .iter()
                .find(|l| super::super::strip_ansi(l).contains("skills"))
                .expect("skills header");
            let plain = super::super::strip_ansi(header);
            assert!(
                super::super::str_width(&plain) <= width,
                "{width}: {} cols, {plain}",
                super::super::str_width(&plain)
            );
            assert!(
                plain.ends_with('…'),
                "{width}: a header past the pane ends on an ellipsis, got {plain}"
            );
            assert!(
                !plain.contains(broken) && !plain.ends_with("bundled."),
                "{width}: {plain}"
            );
        }
        let at_80 = super::super::strip_ansi(
            paint_show(&asm, 80)
                .lines
                .iter()
                .find(|l| super::super::strip_ansi(l).contains("skills"))
                .unwrap(),
        );
        assert!(at_80.contains("self-knowledge (bundled)"));
        assert!(at_80.contains("codebase-repair (bundled)"));
        assert_eq!(super::super::str_width(&at_80), 80);

        let hint = super::super::strip_ansi(&show_hint(80));
        assert!(hint.contains("n/p section"), "{hint}");
        assert!(super::super::str_width(&hint) <= 80, "{hint}");
        let narrow = super::super::strip_ansi(&show_hint(40));
        assert!(!narrow.contains("n/p section"), "{narrow}");
        assert!(super::super::str_width(&narrow) <= 40, "{narrow}");
    }
}
