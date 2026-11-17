//! IDE-style tree and diff panes.
//!
//! Hidden until F2 (tree) or F3 (diff). While hidden, a frame only loads one
//! atomic: no directory walk, no diff, no git status. Directories are listed
//! when they are opened, through the same `.gitignore` filter as the file
//! tools (`tools::visible_entries`). Diffs are cached per path and dropped
//! when that file is written.

use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Mutex, MutexGuard};

const TREE_DEFAULT: u16 = 28;
const DIFF_DEFAULT: u16 = 52;
const WIDE: u16 = 120;

static ARMED: AtomicBool = AtomicBool::new(false);
static TREE_ON: AtomicBool = AtomicBool::new(false);
static DIFF_ON: AtomicBool = AtomicBool::new(false);
static TREE_W: AtomicU16 = AtomicU16::new(TREE_DEFAULT);
static DIFF_W: AtomicU16 = AtomicU16::new(DIFF_DEFAULT);
static FOCUS: AtomicU16 = AtomicU16::new(0); // 0 chat, 1 tree, 2 diff
static JUMP: AtomicBool = AtomicBool::new(false);
static GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Chat = 0,
    Tree = 1,
    Diff = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    None,
    Read,
    Modified,
    Created,
    Deleted,
}

impl Mark {
    fn ch(self) -> char {
        match self {
            Mark::None => ' ',
            Mark::Read => 'r',
            Mark::Modified => 'm',
            Mark::Created => '+',
            Mark::Deleted => 'd',
        }
    }
}

#[derive(Clone, Debug)]
struct Entry {
    name: String,
    dir: bool,
}

#[derive(Clone, Debug)]
struct Row {
    kind: u8,
    text: String,
    hunk: u16,
}

#[derive(Clone, Debug)]
struct Hunk {
    accepted: bool,
}

#[derive(Clone, Debug)]
struct Diff {
    rows: Vec<Row>,
    hunks: Vec<Hunk>,
    side_by_side: bool,
    vs_git: bool,
    ext: String,
}

struct State {
    root: PathBuf,
    expanded: Vec<String>,
    children: HashMap<String, Vec<Entry>>,
    cursor: usize,
    scroll: usize,
    diff_scroll: usize,
    hunk_cursor: usize,
    jump: String,
    marks: HashMap<String, Mark>,
    git: HashMap<String, String>,
    git_loaded: bool,
    baseline: HashMap<String, String>,
    diff: Option<(String, Diff)>,
    pending_path: String,
    pending_tool: String,
    pending_field: String,
}

impl State {
    fn new() -> Self {
        Self {
            root: PathBuf::new(),
            expanded: vec![String::new()],
            children: HashMap::new(),
            cursor: 0,
            scroll: 0,
            diff_scroll: 0,
            hunk_cursor: 0,
            jump: String::new(),
            marks: HashMap::new(),
            git: HashMap::new(),
            git_loaded: false,
            baseline: HashMap::new(),
            diff: None,
            pending_path: String::new(),
            pending_tool: String::new(),
            pending_field: String::new(),
        }
    }
}

fn state() -> MutexGuard<'static, State> {
    static STATE: std::sync::OnceLock<Mutex<State>> = std::sync::OnceLock::new();
    STATE
        .get_or_init(|| Mutex::new(State::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn bump() {
    GEN.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn armed() -> bool {
    ARMED.load(Ordering::Relaxed)
}

#[inline]
pub fn visible() -> bool {
    TREE_ON.load(Ordering::Relaxed) || DIFF_ON.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneLayout {
    pub wide: bool,
    pub tree: Option<(u16, u16)>,
    pub chat: Option<(u16, u16)>,
    pub diff: Option<(u16, u16)>,
    pub tab: &'static str,
}

/// Where the panes sit. Narrow terminals show one tab; 120+ shows them together.
pub fn layout_panes(
    term_w: u16,
    tree: bool,
    diff: bool,
    focus: Focus,
    tree_w: u16,
    diff_w: u16,
) -> PaneLayout {
    let tab = match focus {
        Focus::Chat => "chat",
        Focus::Tree => "tree",
        Focus::Diff => "diff",
    };
    if !tree && !diff {
        return PaneLayout {
            wide: term_w >= WIDE,
            tree: None,
            chat: Some((0, term_w)),
            diff: None,
            tab,
        };
    }
    if term_w < WIDE {
        let show_tree = focus == Focus::Tree || (focus == Focus::Chat && tree && !diff);
        let show_diff = focus == Focus::Diff || (!show_tree && diff && focus != Focus::Chat);
        return PaneLayout {
            wide: false,
            tree: show_tree.then_some((0, term_w)),
            chat: (focus == Focus::Chat).then_some((0, term_w)),
            diff: show_diff.then_some((0, term_w)),
            tab,
        };
    }
    let left = if tree {
        tree_w.clamp(16, term_w / 3)
    } else {
        0
    };
    let right = if diff {
        diff_w.clamp(24, term_w / 2)
    } else {
        0
    };
    let mid = term_w.saturating_sub(left).saturating_sub(right).max(20);
    let left = term_w.saturating_sub(mid).saturating_sub(right);
    PaneLayout {
        wide: true,
        tree: tree.then_some((0, left)),
        chat: Some((left, mid)),
        diff: diff.then_some((left + mid, term_w.saturating_sub(left + mid))),
        tab,
    }
}

/// Transcript origin and width. `(0, term_w)` when the workspace is hidden.
pub fn content_box(term_w: u16) -> (u16, u16) {
    if !visible() {
        return (0, term_w);
    }
    let panes = current_layout(term_w);
    match panes.chat {
        Some((x, w)) if panes.wide => (x, w),
        Some((x, w)) => (x, w),
        None => (0, term_w),
    }
}

pub fn covers_output(term_w: u16) -> bool {
    if !visible() || term_w >= WIDE {
        return false;
    }
    current_layout(term_w).chat.is_none()
}

fn focus() -> Focus {
    match FOCUS.load(Ordering::Relaxed) {
        1 => Focus::Tree,
        2 => Focus::Diff,
        _ => Focus::Chat,
    }
}

fn current_layout(term_w: u16) -> PaneLayout {
    layout_panes(
        term_w,
        TREE_ON.load(Ordering::Relaxed),
        DIFF_ON.load(Ordering::Relaxed),
        focus(),
        TREE_W.load(Ordering::Relaxed),
        DIFF_W.load(Ordering::Relaxed),
    )
}

fn arm(root: PathBuf) {
    ARMED.store(true, Ordering::Relaxed);
    let mut g = state();
    if g.root.as_os_str().is_empty() {
        g.root = root;
    }
}

pub fn open_diff(path: &str) {
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    arm(root);
    DIFF_ON.store(true, Ordering::Relaxed);
    FOCUS.store(Focus::Diff as u16, Ordering::Relaxed);
    {
        let mut g = state();
        show_path(&mut g, path, false);
    }
    bump();
}

pub fn on_code(code: crossterm::event::KeyCode, alt: bool) -> bool {
    if !visible()
        && !matches!(
            code,
            crossterm::event::KeyCode::F(2) | crossterm::event::KeyCode::F(3)
        )
    {
        return false;
    }
    match code {
        crossterm::event::KeyCode::F(2) => {
            let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            arm(root);
            let on = !TREE_ON.load(Ordering::Relaxed);
            TREE_ON.store(on, Ordering::Relaxed);
            if on {
                FOCUS.store(Focus::Tree as u16, Ordering::Relaxed);
                let mut g = state();
                ensure_dir(&mut g, "");
                if !g.git_loaded {
                    g.git = git_status(&g.root);
                    g.git_loaded = true;
                }
            } else if focus() == Focus::Tree {
                FOCUS.store(Focus::Chat as u16, Ordering::Relaxed);
            }
            bump();
            true
        }
        crossterm::event::KeyCode::F(3) => {
            let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            arm(root);
            let on = !DIFF_ON.load(Ordering::Relaxed);
            DIFF_ON.store(on, Ordering::Relaxed);
            FOCUS.store(
                if on {
                    Focus::Diff as u16
                } else {
                    Focus::Chat as u16
                },
                Ordering::Relaxed,
            );
            bump();
            true
        }
        crossterm::event::KeyCode::F(4) => {
            if !TREE_ON.load(Ordering::Relaxed) {
                return false;
            }
            let on = !JUMP.swap(true, Ordering::Relaxed);
            if !on {
                JUMP.store(false, Ordering::Relaxed);
                state().jump.clear();
            }
            bump();
            true
        }
        crossterm::event::KeyCode::F(6) => {
            if !visible() {
                return false;
            }
            let next = match focus() {
                Focus::Chat => Focus::Tree,
                Focus::Tree => Focus::Diff,
                Focus::Diff => Focus::Chat,
            };
            FOCUS.store(next as u16, Ordering::Relaxed);
            if next == Focus::Tree {
                TREE_ON.store(true, Ordering::Relaxed);
            }
            if next == Focus::Diff {
                DIFF_ON.store(true, Ordering::Relaxed);
            }
            bump();
            true
        }
        crossterm::event::KeyCode::Char('[') if alt && TREE_ON.load(Ordering::Relaxed) => {
            let w = TREE_W.load(Ordering::Relaxed).saturating_sub(2).max(16);
            TREE_W.store(w, Ordering::Relaxed);
            bump();
            true
        }
        crossterm::event::KeyCode::Char(']') if alt && TREE_ON.load(Ordering::Relaxed) => {
            let w = TREE_W.load(Ordering::Relaxed).saturating_add(2).min(48);
            TREE_W.store(w, Ordering::Relaxed);
            bump();
            true
        }
        crossterm::event::KeyCode::Up if focus() == Focus::Tree => {
            let mut g = state();
            g.cursor = g.cursor.saturating_sub(1);
            bump();
            true
        }
        crossterm::event::KeyCode::Down if focus() == Focus::Tree => {
            let mut g = state();
            let n = flat(&g).len();
            if n > 0 {
                g.cursor = (g.cursor + 1).min(n - 1);
            }
            bump();
            true
        }
        crossterm::event::KeyCode::Enter
            if focus() == Focus::Tree && !JUMP.load(Ordering::Relaxed) =>
        {
            let mut g = state();
            activate(&mut g);
            bump();
            true
        }
        crossterm::event::KeyCode::Left if focus() == Focus::Tree => {
            let mut g = state();
            collapse(&mut g);
            bump();
            true
        }
        crossterm::event::KeyCode::Right if focus() == Focus::Tree => {
            let mut g = state();
            expand(&mut g);
            bump();
            true
        }
        crossterm::event::KeyCode::Up if focus() == Focus::Diff => {
            let mut g = state();
            g.diff_scroll = g.diff_scroll.saturating_sub(1);
            bump();
            true
        }
        crossterm::event::KeyCode::Down if focus() == Focus::Diff => {
            let mut g = state();
            g.diff_scroll = g.diff_scroll.saturating_add(1);
            bump();
            true
        }
        crossterm::event::KeyCode::Char('n') if focus() == Focus::Diff => {
            let mut g = state();
            step_hunk(&mut g, 1);
            bump();
            true
        }
        crossterm::event::KeyCode::Char('p') if focus() == Focus::Diff => {
            let mut g = state();
            step_hunk(&mut g, -1);
            bump();
            true
        }
        crossterm::event::KeyCode::Char('x') if focus() == Focus::Diff => {
            let mut g = state();
            toggle_hunk(&mut g);
            bump();
            true
        }
        crossterm::event::KeyCode::Char('v') if focus() == Focus::Diff => {
            let mut g = state();
            if let Some((_, d)) = g.diff.as_mut() {
                d.side_by_side = !d.side_by_side;
                if d.side_by_side {
                    let w = DIFF_W.load(Ordering::Relaxed).max(68);
                    DIFF_W.store(w, Ordering::Relaxed);
                }
            }
            bump();
            true
        }
        crossterm::event::KeyCode::Char('g') if focus() == Focus::Diff => {
            let mut g = state();
            let flipped = g.diff.as_ref().map(|(path, d)| (path.clone(), !d.vs_git));
            if let Some((path, vs_git)) = flipped {
                show_path(&mut g, &path, vs_git);
            }
            bump();
            true
        }
        crossterm::event::KeyCode::Esc if JUMP.load(Ordering::Relaxed) => {
            JUMP.store(false, Ordering::Relaxed);
            state().jump.clear();
            bump();
            true
        }
        crossterm::event::KeyCode::Enter if JUMP.load(Ordering::Relaxed) => {
            let mut g = state();
            let q = g.jump.clone();
            let root = g.root.clone();
            let hit = fuzzy_walk(&root, &q, 1).into_iter().next();
            JUMP.store(false, Ordering::Relaxed);
            g.jump.clear();
            if let Some(path) = hit {
                DIFF_ON.store(true, Ordering::Relaxed);
                FOCUS.store(Focus::Diff as u16, Ordering::Relaxed);
                show_path(&mut g, &path, false);
            }
            bump();
            true
        }
        crossterm::event::KeyCode::Backspace if JUMP.load(Ordering::Relaxed) => {
            state().jump.pop();
            bump();
            true
        }
        crossterm::event::KeyCode::Char(c)
            if JUMP.load(Ordering::Relaxed) && !c.is_control() && !alt =>
        {
            state().jump.push(c);
            bump();
            true
        }
        _ => false,
    }
}

pub fn before_tool(name: &str, input: &Value, cwd: &Path) {
    if !armed() {
        return;
    }
    let Some(path) = tool_path(name, input, cwd) else {
        return;
    };
    let mut g = state();
    let key = rel(&g.root, &path);
    if let std::collections::hash_map::Entry::Vacant(slot) = g.baseline.entry(key) {
        let text = fs::read_to_string(&path).unwrap_or_default();
        if text.len() <= 1_000_000 {
            slot.insert(text);
        }
    }
}

pub fn after_tool(name: &str, input: &Value, cwd: &Path, is_error: bool) {
    if !armed() || is_error {
        return;
    }
    let Some(path) = tool_path(name, input, cwd) else {
        return;
    };
    let mut g = state();
    let key = rel(&g.root, &path);
    let mark = if crate::tools::read_tracking_path(name, input, cwd).is_some() {
        Mark::Read
    } else if !path.exists() {
        Mark::Deleted
    } else if g.baseline.get(&key).is_some_and(|s| s.is_empty()) {
        Mark::Created
    } else {
        Mark::Modified
    };
    if !matches!(mark, Mark::Read) || !g.marks.contains_key(&key) {
        g.marks.insert(key.clone(), mark);
    }
    if let Some((shown, _)) = &g.diff {
        if shown == &key {
            g.diff = None;
        }
    }
    g.git_loaded = false;
    bump();
}

pub fn note_pending(name: &str, input: &Value, cwd: &Path) {
    if !armed() {
        return;
    }
    let (field, old, new) = match edit_pair(name, input) {
        Some(v) => v,
        None => return,
    };
    let Some(path) = tool_path(name, input, cwd) else {
        return;
    };
    let mut g = state();
    let key = rel(&g.root, &path);
    let (hunks, rows) = hunks_of(&old, &new);
    g.pending_path = key.clone();
    g.pending_tool = name.to_string();
    g.pending_field = field.to_string();
    g.hunk_cursor = 0;
    g.diff = Some((
        key,
        Diff {
            rows,
            hunks,
            side_by_side: false,
            vs_git: false,
            ext: ext_of(&path),
        },
    ));
    bump();
}

pub fn rewrite(name: &str, input: &Value, cwd: &Path) -> Option<Value> {
    if !armed() {
        return None;
    }
    let g = state();
    let (path, diff) = g.diff.as_ref()?;
    if g.pending_path != *path || g.pending_tool != name {
        return None;
    }
    if diff.hunks.iter().all(|h| h.accepted) {
        return None;
    }
    let field = g.pending_field.clone();
    let old = edit_pair(name, input)?.1;
    let text = materialize(&old, &diff.rows, &diff.hunks);
    let _ = cwd;
    let mut copy = input.clone();
    if let Some(obj) = copy.as_object_mut() {
        obj.insert(field, Value::String(text));
    }
    Some(copy)
}

pub fn append_overlay(out: &mut Vec<u8>, term_w: u16, rows: u16, force: bool) {
    if !visible() || term_w == 0 || rows == 0 {
        return;
    }
    let gen = GEN.load(Ordering::Relaxed);
    let key = (gen, term_w, rows, focus() as u16);
    static LAST: Mutex<(u64, u16, u16, u16, Vec<u8>)> = Mutex::new((0, 0, 0, 0, Vec::new()));
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if !force && last.0 == key.0 && last.1 == key.1 && last.2 == key.2 && last.3 == key.3 {
        return;
    }
    let bytes = paint(term_w, rows);
    out.extend_from_slice(&bytes);
    *last = (key.0, key.1, key.2, key.3, bytes);
}

fn paint(term_w: u16, rows: u16) -> Vec<u8> {
    let panes = current_layout(term_w);
    let g = state();
    let mut out = Vec::new();
    if let Some((col, w)) = panes.tree {
        for (i, line) in tree_lines(&g, w as usize, rows as usize)
            .into_iter()
            .enumerate()
        {
            let shown = if i == 0 {
                crate::tui::underline(&line)
            } else if line.contains('⌞') {
                crate::tui::bold(&line)
            } else {
                line
            };
            let _ = std::io::Write::write_fmt(
                &mut out,
                format_args!("\x1b[{};{}H{}", i + 1, col + 1, shown),
            );
        }
    }
    if let Some((col, w)) = panes.diff {
        let side = term_w >= WIDE && w >= 64;
        for (i, line) in diff_lines(&g, w as usize, rows as usize, side)
            .into_iter()
            .enumerate()
        {
            let shown = color_diff_line(&line, &g);
            let shown = if i == 0 {
                crate::tui::underline(&shown)
            } else {
                shown
            };
            let _ = std::io::Write::write_fmt(
                &mut out,
                format_args!(
                    "\x1b[{};{}H{}",
                    i + 1,
                    col + 1,
                    fit_ansi(&shown, w as usize)
                ),
            );
        }
    }
    out
}

fn tree_lines(g: &State, width: usize, rows: usize) -> Vec<String> {
    let flat = flat(g);
    let mut start = g.scroll.min(flat.len().saturating_sub(rows));
    if g.cursor >= start + rows.saturating_sub(1) {
        start = g.cursor + 1 - rows;
    }
    let mut lines = Vec::new();
    let title = if JUMP.load(Ordering::Relaxed) {
        phrase(width, &[&format!("jump {}", g.jump), "jump"])
    } else {
        phrase(width, &["files", "tree"])
    };
    lines.push(fit(&title, width));
    if JUMP.load(Ordering::Relaxed) && !g.jump.is_empty() {
        for hit in fuzzy_walk(&g.root, &g.jump, rows.saturating_sub(1)) {
            lines.push(fit(&format!("· {hit}"), width));
        }
    } else {
        for (i, (depth, rel, ent)) in flat
            .iter()
            .enumerate()
            .skip(start)
            .take(rows.saturating_sub(1))
        {
            let rail = if i == g.cursor { "⌞" } else { "·" };
            let mark = g.marks.get(rel).copied().unwrap_or(Mark::None).ch();
            let git = g.git.get(rel).map(|s| s.as_str()).unwrap_or("  ");
            let slash = if ent.dir { "/" } else { "" };
            let indent = " ".repeat((*depth).min(6));
            lines.push(fit(
                &format!("{rail}{mark}{git} {indent}{}{slash}", ent.name),
                width,
            ));
        }
    }
    while lines.len() < rows {
        lines.push(fit("", width));
    }
    lines.truncate(rows);
    lines
}

fn diff_lines(g: &State, width: usize, rows: usize, wide_side: bool) -> Vec<String> {
    let mut lines = Vec::new();
    let Some((path, diff)) = &g.diff else {
        lines.push(fit(&phrase(width, &["diff", ""]), width));
        while lines.len() < rows {
            lines.push(fit("", width));
        }
        return lines;
    };
    let adds = diff.rows.iter().filter(|r| r.kind == b'+').count();
    let dels = diff.rows.iter().filter(|r| r.kind == b'-').count();
    let name = path.rsplit('/').next().unwrap_or(path);
    let mode = if diff.vs_git {
        "git"
    } else if wide_side && diff.side_by_side {
        "side"
    } else {
        "unified"
    };
    let full = format!("{name}  +{adds} -{dels}  {mode}");
    let short = format!("{name}  +{adds} -{dels}");
    // An unchanged Markdown file is a page, not a diff of every line.
    if is_markdown_ext(&diff.ext) && diff.hunks.is_empty() {
        lines.push(fit(&phrase(width, &[name, "page"]), width));
        let src = diff
            .rows
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let body = crate::tui::render_md_width(&src, width);
        let start = g
            .diff_scroll
            .min(body.lines().count().saturating_sub(rows.saturating_sub(1)));
        for line in body.lines().skip(start).take(rows.saturating_sub(1)) {
            lines.push(pad_cells(line, width));
        }
        while lines.len() < rows {
            lines.push(fit("", width));
        }
        lines.truncate(rows);
        return lines;
    }
    lines.push(fit(&phrase(width, &[&full, &short, name]), width));
    let side = diff.side_by_side && (wide_side || width >= 64);
    let body: Vec<String> = if side {
        side_rows(diff, width)
    } else {
        numbered_rows(diff, g.hunk_cursor)
    };
    let start = g
        .diff_scroll
        .min(body.len().saturating_sub(rows.saturating_sub(1)));
    for line in body.into_iter().skip(start).take(rows.saturating_sub(1)) {
        lines.push(fit(&line, width));
    }
    while lines.len() < rows {
        lines.push(fit("", width));
    }
    lines.truncate(rows);
    lines
}

fn side_rows(diff: &Diff, width: usize) -> Vec<String> {
    let half = width / 2;
    let mut rows = Vec::new();
    let mut i = 0;
    while i < diff.rows.len() {
        let row = &diff.rows[i];
        if row.kind == b'-' {
            let add = diff
                .rows
                .get(i + 1)
                .filter(|r| r.kind == b'+' && r.hunk == row.hunk);
            let right = add.map(|r| r.text.as_str()).unwrap_or("");
            rows.push(format!(
                "{:<half$}{:<half$}",
                clip(&format!("-{}", row.text), half),
                clip(&format!("+{right}"), half),
                half = half
            ));
            if add.is_some() {
                i += 2;
                continue;
            }
        } else if row.kind == b'+' {
            rows.push(format!(
                "{:<half$}{:<half$}",
                "",
                clip(&format!("+{}", row.text), half),
                half = half
            ));
        } else {
            rows.push(format!(
                "{:<half$}{:<half$}",
                clip(&format!(" {}", row.text), half),
                clip(&format!(" {}", row.text), half),
                half = half
            ));
        }
        i += 1;
    }
    rows
}

fn phrase(width: usize, options: &[&str]) -> String {
    options
        .iter()
        .copied()
        .find(|s| s.chars().count() <= width)
        .unwrap_or("")
        .to_string()
}

fn numbered_rows(diff: &Diff, cursor: usize) -> Vec<String> {
    let mut old_n = 1usize;
    let mut new_n = 1usize;
    let mut out = Vec::new();
    for (i, row) in diff.rows.iter().enumerate() {
        let (ol, nl) = match row.kind {
            b'-' => {
                let o = old_n;
                old_n += 1;
                (Some(o), None)
            }
            b'+' => {
                let n = new_n;
                new_n += 1;
                (None, Some(n))
            }
            _ => {
                let o = old_n;
                let n = new_n;
                old_n += 1;
                new_n += 1;
                (Some(o), Some(n))
            }
        };
        let rail = if i == cursor { '⌞' } else { '·' };
        let flag = diff
            .hunks
            .get(row.hunk as usize)
            .map(|h| if h.accepted { ' ' } else { 'x' })
            .unwrap_or(' ');
        let ol = ol
            .map(|n| format!("{n:>4}"))
            .unwrap_or_else(|| "    ".into());
        let nl = nl
            .map(|n| format!("{n:>4}"))
            .unwrap_or_else(|| "    ".into());
        out.push(format!(
            "{rail}{ol} {nl} {flag}{}{}",
            row.kind as char, row.text
        ));
    }
    out
}

fn color_diff_line(line: &str, g: &State) -> String {
    let chars: Vec<char> = line.chars().collect();
    // Numbered row: rail, 4, space, 4, space, flag, sign, text.
    const SIGN: usize = 12;
    let (head, sign, tail) =
        if chars.len() > SIGN && matches!(chars[0], '·' | '⌞') && matches!(chars[SIGN], '+' | '-')
        {
            let head: String = chars.iter().take(SIGN).collect();
            let tail: String = chars.iter().skip(SIGN + 1).collect();
            let sign = if chars[SIGN] == '+' {
                crate::tui::green("+")
            } else {
                crate::tui::red("-")
            };
            (head, sign, tail)
        } else {
            return line.to_string();
        };
    let ext = g.diff.as_ref().map(|(_, d)| d.ext.as_str()).unwrap_or("");
    let code = if let Some(lang) = crate::highlight::lang_for(ext) {
        let mut st = crate::highlight::State::default();
        crate::highlight::highlight_line(lang, &tail, &mut st)
    } else {
        tail
    };
    format!("{head}{sign}{code}")
}

fn ensure_dir(g: &mut State, rel_dir: &str) {
    if g.children.contains_key(rel_dir) {
        return;
    }
    let abs = if rel_dir.is_empty() {
        g.root.clone()
    } else {
        g.root.join(rel_dir)
    };
    let entries = crate::tools::visible_entries(&g.root, &abs)
        .into_iter()
        .map(|(name, dir)| Entry { name, dir })
        .collect();
    g.children.insert(rel_dir.to_string(), entries);
}

fn flat(g: &State) -> Vec<(usize, String, Entry)> {
    let mut out = Vec::new();
    fn walk(g: &State, dir: &str, depth: usize, out: &mut Vec<(usize, String, Entry)>) {
        let Some(kids) = g.children.get(dir) else {
            return;
        };
        for ent in kids {
            let rel = if dir.is_empty() {
                ent.name.clone()
            } else {
                format!("{dir}/{}", ent.name)
            };
            out.push((depth, rel.clone(), ent.clone()));
            if ent.dir && g.expanded.iter().any(|e| e == &rel) {
                walk(g, &rel, depth + 1, out);
            }
        }
    }
    walk(g, "", 0, &mut out);
    out
}

fn activate(g: &mut State) {
    let rows = flat(g);
    let Some((_, rel, ent)) = rows.get(g.cursor) else {
        return;
    };
    if ent.dir {
        if let Some(i) = g.expanded.iter().position(|e| e == rel) {
            g.expanded.remove(i);
        } else {
            g.expanded.push(rel.clone());
            ensure_dir(g, rel);
        }
    } else {
        let rel = rel.clone();
        show_path(g, &rel, false);
        DIFF_ON.store(true, Ordering::Relaxed);
        FOCUS.store(Focus::Diff as u16, Ordering::Relaxed);
    }
}

fn expand(g: &mut State) {
    let rows = flat(g);
    let Some((_, rel, ent)) = rows.get(g.cursor) else {
        return;
    };
    if ent.dir && !g.expanded.iter().any(|e| e == rel) {
        g.expanded.push(rel.clone());
        ensure_dir(g, rel);
    }
}

fn collapse(g: &mut State) {
    let rows = flat(g);
    if let Some((_, rel, ent)) = rows.get(g.cursor) {
        if ent.dir {
            g.expanded.retain(|e| e != rel);
        }
    }
}

fn show_path(g: &mut State, rel_path: &str, vs_git: bool) {
    let abs = g.root.join(rel_path);
    let current = fs::read_to_string(&abs).unwrap_or_default();
    let (old, new) = if vs_git {
        (git_show(&g.root, rel_path).unwrap_or_default(), current)
    } else if let Some(base) = g.baseline.get(rel_path) {
        (base.clone(), current)
    } else {
        (current.clone(), current)
    };
    let (hunks, rows) = hunks_of(&old, &new);
    g.diff_scroll = 0;
    g.hunk_cursor = 0;
    g.diff = Some((
        rel_path.to_string(),
        Diff {
            rows,
            hunks,
            side_by_side: false,
            vs_git,
            ext: ext_of(Path::new(rel_path)),
        },
    ));
}

fn step_hunk(g: &mut State, dir: isize) {
    let Some((_, diff)) = &g.diff else {
        return;
    };
    if diff.hunks.is_empty() {
        return;
    }
    let cur = diff.rows.get(g.hunk_cursor).map(|r| r.hunk).unwrap_or(0);
    let next = if dir < 0 {
        cur.saturating_sub(1)
    } else {
        (cur + 1).min(diff.hunks.len().saturating_sub(1) as u16)
    };
    if let Some(i) = diff.rows.iter().position(|r| r.hunk == next) {
        g.hunk_cursor = i;
        g.diff_scroll = i.saturating_sub(2);
    }
}

fn toggle_hunk(g: &mut State) {
    let idx = g
        .diff
        .as_ref()
        .and_then(|(_, d)| d.rows.get(g.hunk_cursor))
        .map(|r| r.hunk as usize);
    if let Some(i) = idx {
        if let Some((_, d)) = g.diff.as_mut() {
            if let Some(h) = d.hunks.get_mut(i) {
                h.accepted = !h.accepted;
            }
        }
    }
}

fn tool_path(name: &str, input: &Value, cwd: &Path) -> Option<PathBuf> {
    crate::tools::edit_tracking_path(name, input, cwd)
        .or_else(|| crate::tools::read_tracking_path(name, input, cwd))
}

fn edit_pair(name: &str, input: &Value) -> Option<(&'static str, String, String)> {
    let old = input["old"]
        .as_str()
        .or_else(|| input["oldString"].as_str())
        .or_else(|| input["old_str"].as_str());
    let new = input["new"]
        .as_str()
        .or_else(|| input["newString"].as_str())
        .or_else(|| input["new_str"].as_str());
    if let (Some(o), Some(n)) = (old, new) {
        let field = if input.get("new").is_some() {
            "new"
        } else if input.get("newString").is_some() {
            "newString"
        } else {
            "new_str"
        };
        return Some((field, o.to_string(), n.to_string()));
    }
    if matches!(name, "write" | "write_file" | "create") {
        let content = input["content"].as_str().unwrap_or("");
        return Some(("content", String::new(), content.to_string()));
    }
    None
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn is_markdown_ext(ext: &str) -> bool {
    ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown")
}

fn pad_cells(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let visible = crate::tui::str_width(&crate::tui::strip_ansi(s));
    if visible > width {
        return crate::tui::clip_cells(s, width);
    }
    let mut out = s.to_string();
    out.extend(std::iter::repeat_n(' ', width - visible));
    out
}

fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_string()
}

fn git_status(root: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(done) = Command::new("git")
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=normal"])
        .current_dir(root)
        .output()
    else {
        return out;
    };
    if !done.status.success() {
        return out;
    }
    let text = String::from_utf8_lossy(&done.stdout);
    let mut parts = text.split('\0');
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        let (xy, path) = (&entry[..2], &entry[3..]);
        if xy.contains(['R', 'C']) {
            parts.next();
        }
        out.insert(path.replace('\\', "/"), xy.to_string());
    }
    out
}

fn git_show(root: &Path, rel_path: &str) -> Option<String> {
    let done = Command::new("git")
        .args(["show", &format!("HEAD:{rel_path}")])
        .current_dir(root)
        .output()
        .ok()?;
    if done.status.success() {
        Some(String::from_utf8_lossy(&done.stdout).into_owned())
    } else {
        None
    }
}

/// Names matching `query`, listing a directory only when the walk reaches it.
/// Stops after `limit` hits or 8_000 directories, so a jump does not promise
/// a full-tree index.
pub fn fuzzy_walk(root: &Path, query: &str, limit: usize) -> Vec<String> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() || limit == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut stack = vec![String::new()];
    let mut dirs = 0usize;
    while let Some(dir) = stack.pop() {
        if dirs >= 8_000 || out.len() >= limit {
            break;
        }
        dirs += 1;
        let abs = if dir.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&dir)
        };
        for (name, is_dir) in crate::tools::visible_entries(root, &abs) {
            let rel = if dir.is_empty() {
                name.clone()
            } else {
                format!("{dir}/{name}")
            };
            if is_dir {
                stack.push(rel);
            } else if name.to_ascii_lowercase().contains(&q) {
                out.push(rel);
                if out.len() >= limit {
                    break;
                }
            }
        }
    }
    out
}

#[derive(Clone, Copy)]
enum Op {
    Keep,
    Del,
    Ins,
}

fn hunks_of(old: &str, new: &str) -> (Vec<Hunk>, Vec<Row>) {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let ops = if a.len() > 800 || b.len() > 800 {
        span_ops(&a, &b)
    } else {
        lcs_ops(&a, &b)
    };
    group(&a, &b, &ops)
}

fn lcs_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let n = a.len();
    let m = b.len();
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    let idx = |i: usize, j: usize| i * (m + 1) + j;
    for i in 1..=n {
        for j in 1..=m {
            dp[idx(i, j)] = if a[i - 1] == b[j - 1] {
                dp[idx(i - 1, j - 1)] + 1
            } else {
                dp[idx(i - 1, j)].max(dp[idx(i, j - 1)])
            };
        }
    }
    let mut ops = Vec::new();
    let mut i = n;
    let mut j = m;
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a[i - 1] == b[j - 1] {
            ops.push(Op::Keep);
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[idx(i, j - 1)] >= dp[idx(i.saturating_sub(1), j)]) {
            ops.push(Op::Ins);
            j -= 1;
        } else {
            ops.push(Op::Del);
            i -= 1;
        }
    }
    ops.reverse();
    ops
}

fn span_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let mut pre = 0usize;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0usize;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf]
    {
        suf += 1;
    }
    let mut ops = vec![Op::Keep; pre];
    ops.extend(std::iter::repeat_n(Op::Del, a.len() - pre - suf));
    ops.extend(std::iter::repeat_n(Op::Ins, b.len() - pre - suf));
    ops.extend(std::iter::repeat_n(Op::Keep, suf));
    ops
}

fn group(a: &[&str], b: &[&str], ops: &[Op]) -> (Vec<Hunk>, Vec<Row>) {
    let mut hunks = Vec::new();
    let mut rows = Vec::new();
    let mut ia = 0usize;
    let mut ib = 0usize;
    let mut i = 0usize;
    while i < ops.len() {
        if matches!(ops[i], Op::Keep) {
            rows.push(Row {
                kind: b' ',
                text: a.get(ia).unwrap_or(&"").to_string(),
                hunk: u16::MAX,
            });
            ia += 1;
            ib += 1;
            i += 1;
            continue;
        }
        let start = rows.len().saturating_sub(3);
        let hunk = hunks.len() as u16;
        hunks.push(Hunk { accepted: true });
        // Context immediately above the change belongs to this hunk.
        for row in rows.iter_mut().skip(start) {
            if row.hunk == u16::MAX {
                row.hunk = hunk;
            }
        }
        while i < ops.len() && !matches!(ops[i], Op::Keep) {
            match ops[i] {
                Op::Del => {
                    rows.push(Row {
                        kind: b'-',
                        text: a.get(ia).unwrap_or(&"").to_string(),
                        hunk,
                    });
                    ia += 1;
                }
                Op::Ins => {
                    rows.push(Row {
                        kind: b'+',
                        text: b.get(ib).unwrap_or(&"").to_string(),
                        hunk,
                    });
                    ib += 1;
                }
                Op::Keep => break,
            }
            i += 1;
        }
        let mut ctx = 0;
        while ctx < 3 && i < ops.len() && matches!(ops[i], Op::Keep) {
            rows.push(Row {
                kind: b' ',
                text: a.get(ia).unwrap_or(&"").to_string(),
                hunk,
            });
            ia += 1;
            ib += 1;
            i += 1;
            ctx += 1;
        }
    }
    (hunks, rows)
}

fn materialize(old: &str, rows: &[Row], hunks: &[Hunk]) -> String {
    let mut out = String::new();
    for row in rows {
        let accept = hunks
            .get(row.hunk as usize)
            .map(|h| h.accepted)
            .unwrap_or(true);
        let keep = match row.kind {
            b'+' => accept,
            b'-' => !accept,
            _ => true,
        };
        if keep {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&row.text);
        }
    }
    if old.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    let _ = old;
    out
}

fn fit(s: &str, width: usize) -> String {
    fit_ansi(s, width)
}

fn fit_ansi(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let plain = strip_ansi(s);
    if plain.chars().count() == width {
        return s.to_string();
    }
    let mut out: String = plain.chars().take(width).collect();
    if plain.chars().count() > width && width >= 1 {
        out = plain.chars().take(width - 1).collect();
        out.push('…');
    }
    while out.chars().count() < width {
        out.push(' ');
    }
    // Keep a leading ANSI prefix from the original when the plain text fit.
    if s.starts_with('\u{1b}') && plain.chars().count() <= width {
        return s.to_string();
    }
    out
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn clip(s: &str, width: usize) -> String {
    let mut out: String = s.chars().take(width).collect();
    while out.chars().count() < width {
        out.push(' ');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_80_is_one_pane() {
        let tree = layout_panes(80, true, true, Focus::Tree, 28, 52);
        assert!(!tree.wide);
        assert_eq!(tree.tree, Some((0, 80)));
        assert!(tree.chat.is_none());
        assert!(tree.diff.is_none());
        let diff = layout_panes(80, true, true, Focus::Diff, 28, 52);
        assert_eq!(diff.diff, Some((0, 80)));
        assert!(diff.tree.is_none());
        let chat = layout_panes(80, true, true, Focus::Chat, 28, 52);
        assert_eq!(chat.chat, Some((0, 80)));
        assert!(chat.tree.is_none());
    }

    #[test]
    fn layout_120_is_three_columns() {
        let panes = layout_panes(120, true, true, Focus::Chat, 28, 52);
        assert!(panes.wide);
        let (tx, tw) = panes.tree.unwrap();
        let (cx, cw) = panes.chat.unwrap();
        let (dx, dw) = panes.diff.unwrap();
        assert_eq!(tx, 0);
        assert!((16..=40).contains(&tw), "{tw}");
        assert_eq!(cx, tw);
        assert!(cw >= 20, "{cw}");
        assert_eq!(dx, cx + cw);
        assert_eq!(dx + dw, 120);
    }

    #[test]
    fn hidden_content_box_is_the_full_width() {
        TREE_ON.store(false, Ordering::Relaxed);
        DIFF_ON.store(false, Ordering::Relaxed);
        assert_eq!(content_box(100), (0, 100));
        assert!(!covers_output(80));
    }

    #[test]
    fn two_hunks_and_rejecting_one() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nB\nc\nd\nE\n";
        let (hunks, rows) = hunks_of(old, new);
        assert!(hunks.len() >= 2, "{}", hunks.len());
        assert!(rows.iter().any(|r| r.text == "B" && r.kind == b'+'));
        let mut hunks = hunks;
        hunks[1].accepted = false;
        let got = materialize(old, &rows, &hunks);
        assert!(got.contains('B'), "{got}");
        assert!(got.contains('e') && !got.contains('E'), "{got}");
    }

    #[test]
    fn markdown_file_with_no_edits_is_a_rendered_page() {
        let (hunks, rows) = hunks_of("# Title\n\n- item\n", "# Title\n\n- item\n");
        assert!(hunks.is_empty());
        let diff = Diff {
            rows,
            hunks,
            side_by_side: false,
            vs_git: false,
            ext: "md".into(),
        };
        let mut g = State::new();
        g.diff = Some(("notes.md".into(), diff));
        let lines = diff_lines(&g, 40, 16, false);
        let plain = crate::tui::strip_ansi(&lines.join("\n"));
        assert!(plain.contains("Title"), "{plain}");
        assert!(plain.contains("•"), "{plain}");
        assert!(!plain.contains("# Title"), "{plain}");
        assert!(!plain.contains("+0"), "{plain}");
    }

    #[test]
    fn identical_files_have_no_hunks() {
        let (hunks, rows) = hunks_of("a\nb\n", "a\nb\n");
        assert!(hunks.is_empty());
        assert!(rows.iter().all(|r| r.kind == b' '));
    }

    #[test]
    fn large_file_still_diffs_the_changed_span() {
        let old: String = (0..2_000).map(|i| format!("l{i}\n")).collect();
        let mut lines: Vec<_> = (0..2_000).map(|i| format!("l{i}")).collect();
        lines[10] = "changed".into();
        let new = lines.join("\n");
        let (hunks, rows) = hunks_of(&old, &new);
        assert_eq!(hunks.len(), 1);
        assert!(rows.iter().any(|r| r.text == "changed"));
    }

    #[test]
    fn list_one_directory_respects_gitignore() {
        let root = std::env::temp_dir().join(format!("bwn-tree-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("keep")).unwrap();
        fs::create_dir_all(root.join("skip")).unwrap();
        fs::write(root.join(".gitignore"), "skip/\n*.log\n").unwrap();
        fs::write(root.join("keep/a.rs"), "fn a() {}\n").unwrap();
        fs::write(root.join("noise.log"), "x").unwrap();
        fs::write(root.join("skip/secret.rs"), "x").unwrap();
        let names: Vec<_> = crate::tools::visible_entries(&root, &root)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert!(names.iter().any(|n| n == "keep"), "{names:?}");
        assert!(!names.iter().any(|n| n == "skip"), "{names:?}");
        assert!(!names.iter().any(|n| n == "noise.log"), "{names:?}");
        let _ = fs::remove_dir_all(&root);
    }

    /// Local measurement, not CI: `cargo test --release --lib measure_large_tree -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn measure_large_tree() {
        let root = std::env::temp_dir().join("bwn-100k");
        if !root.join("d099").join("f999.txt").exists() {
            for d in 0..100 {
                let dir = root.join(format!("d{d:03}"));
                fs::create_dir_all(&dir).unwrap();
                for f in 0..1000 {
                    fs::write(dir.join(format!("f{f:03}.txt")), "x").unwrap();
                }
            }
            fs::write(root.join(".gitignore"), "d050/\n").unwrap();
            let mut body = String::new();
            for i in 0..1000 {
                body.push_str(&format!("line {i}\n"));
            }
            fs::write(root.join("d000").join("big.txt"), &body).unwrap();
        }
        let t0 = std::time::Instant::now();
        let top = crate::tools::visible_entries(&root, &root);
        let open_us = t0.elapsed().as_micros();
        assert!(top.len() > 50, "{}", top.len());
        assert!(!top.iter().any(|(n, _)| n == "d050"));
        let t1 = std::time::Instant::now();
        let hits = fuzzy_walk(&root, "f999", 5);
        let jump_us = t1.elapsed().as_micros();
        let old = fs::read_to_string(root.join("d000").join("big.txt")).unwrap();
        let mut neu = old.clone();
        neu = neu.replacen("line 10\n", "line TEN\n", 1);
        let t2 = std::time::Instant::now();
        let (hunks, rows) = hunks_of(&old, &neu);
        let diff_us = t2.elapsed().as_micros();
        let t3 = std::time::Instant::now();
        let _lines = {
            let mut g = State::new();
            g.root = root.clone();
            g.children.insert(
                String::new(),
                top.into_iter()
                    .map(|(name, dir)| Entry { name, dir })
                    .collect(),
            );
            tree_lines(&g, 28, 40)
        };
        let paint_us = t3.elapsed().as_micros();
        eprintln!(
            "MEASURE tree_open_us={open_us} fuzzy_us={jump_us} hits={} diff_us={diff_us} hunks={} rows={} paint_us={paint_us}",
            hits.len(),
            hunks.len(),
            rows.len()
        );
    }

    #[test]
    fn tree_render_is_only_the_visible_rows() {
        let mut g = State::new();
        g.root = PathBuf::from(".");
        g.children.insert(
            String::new(),
            (0..500)
                .map(|i| Entry {
                    name: format!("f{i}.rs"),
                    dir: false,
                })
                .collect(),
        );
        let lines = tree_lines(&g, 40, 10);
        assert_eq!(lines.len(), 10);
        assert!(lines.iter().all(|l| l.chars().count() <= 40));
    }
}
