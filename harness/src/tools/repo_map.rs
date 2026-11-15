//! A compact map of files and top-level symbols. The model asks for it with
//! the `repo_map` tool. It is not inserted into the prompt.

use std::path::{Path, PathBuf};

/// `files` are absolute or cwd-relative paths. `focus` boosts paths and
/// symbols that contain its words. Output is capped at `max_chars`.
pub(super) fn render(cwd: &Path, files: &[PathBuf], focus: &str, max_chars: usize) -> String {
    let focus_words: Vec<String> = focus
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .filter(|w| w.len() >= 2)
        .map(|w| w.to_ascii_lowercase())
        .collect();
    let mut ranked: Vec<(i32, String, Vec<String>)> = files
        .iter()
        .filter_map(|path| {
            let rel = path
                .strip_prefix(cwd)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            let symbols = symbols_of(path);
            if symbols.is_empty() && !focus_words.is_empty() {
                // A file with no recognised symbols still shows up when the
                // focus names it.
                if !focus_words
                    .iter()
                    .any(|w| rel.to_ascii_lowercase().contains(w))
                {
                    return None;
                }
            }
            let mut score = symbols.len() as i32;
            let rel_l = rel.to_ascii_lowercase();
            for w in &focus_words {
                if rel_l.contains(w) {
                    score += 50;
                }
                if symbols.iter().any(|s| s.to_ascii_lowercase().contains(w)) {
                    score += 20;
                }
            }
            Some((score, rel, symbols))
        })
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    let mut out = format!(
        "repo map: {} source file{}",
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    );
    if !focus_words.is_empty() {
        out.push_str(&format!(" (focus: {focus})"));
    }
    out.push('\n');
    if ranked.is_empty() {
        out.push_str("no symbols matched\n");
        return out;
    }
    for (_, rel, symbols) in ranked {
        let block = format!("{rel}\n");
        if out.len() + block.len() > max_chars {
            out.push_str("…\n");
            break;
        }
        out.push_str(&block);
        for sym in symbols.iter().take(12) {
            let line = format!("  {sym}\n");
            if out.len() + line.len() > max_chars {
                out.push_str("…\n");
                return out;
            }
            out.push_str(&line);
        }
    }
    out
}

fn symbols_of(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut out = Vec::new();
    for line in text.lines().take(4000) {
        let t = line.trim();
        if t.is_empty() || t.starts_with("//") || t.starts_with('#') || t.starts_with('*') {
            continue;
        }
        if let Some(sym) = symbol_line(&ext, t) {
            if !out.contains(&sym) {
                out.push(sym);
            }
            if out.len() == 24 {
                break;
            }
        }
    }
    out
}

fn symbol_line(ext: &str, line: &str) -> Option<String> {
    let hit = match ext {
        "rs" => rust_symbol(line),
        "py" => starts(line, &["def ", "async def ", "class "]),
        "go" => starts(line, &["func ", "type "]),
        "js" | "jsx" | "mjs" | "ts" | "tsx" => starts(
            line,
            &[
                "function ",
                "async function ",
                "export function ",
                "export async function ",
                "export default function ",
                "class ",
                "export class ",
                "export interface ",
                "export type ",
                "interface ",
                "type ",
            ],
        ),
        "java" | "kt" | "scala" | "cs" => starts(
            line,
            &[
                "class ",
                "interface ",
                "enum ",
                "fun ",
                "public class ",
                "public interface ",
                "def ",
            ],
        ),
        "c" | "h" | "cc" | "cpp" | "hpp" => c_symbol(line),
        "rb" => starts(line, &["def ", "class ", "module "]),
        "php" => starts(line, &["function ", "class ", "interface "]),
        "sh" => starts(line, &["function "]),
        "swift" => starts(line, &["func ", "class ", "struct ", "enum ", "protocol "]),
        _ => false,
    };
    if !hit {
        return None;
    }
    let trimmed = line.trim();
    Some(if trimmed.chars().count() > 120 {
        trimmed.chars().take(117).collect::<String>() + "..."
    } else {
        trimmed.to_string()
    })
}

fn starts(line: &str, prefixes: &[&str]) -> bool {
    let l = line.trim_start();
    prefixes.iter().any(|p| l.starts_with(p))
}

fn rust_symbol(line: &str) -> bool {
    let l = line.trim_start();
    let l = l
        .strip_prefix("pub(crate) ")
        .or_else(|| l.strip_prefix("pub(super) "))
        .or_else(|| l.strip_prefix("pub "))
        .unwrap_or(l);
    let l = l.strip_prefix("async ").unwrap_or(l);
    [
        "fn ", "struct ", "enum ", "trait ", "type ", "mod ", "const ", "static ",
    ]
    .iter()
    .any(|p| l.starts_with(p))
}

fn c_symbol(line: &str) -> bool {
    let l = line.trim_start();
    (l.contains('(') && l.ends_with('{') || l.ends_with(')'))
        && !l.starts_with("if ")
        && !l.starts_with("for ")
        && !l.starts_with("while ")
        && !l.starts_with("switch ")
        && !l.starts_with('#')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_ranks_the_named_file_first_and_keeps_signatures() {
        let dir = std::env::temp_dir().join(format!("bwn-repomap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/parser.rs"),
            "pub fn parse_line() {}\nstruct Ast {}\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/other.rs"), "fn unrelated() {}\n").unwrap();
        let files = vec![dir.join("src/other.rs"), dir.join("src/parser.rs")];
        let text = render(&dir, &files, "parser", 4000);
        let parser = text.find("src/parser.rs").unwrap();
        let other = text.find("src/other.rs").unwrap();
        assert!(parser < other, "{text}");
        assert!(text.contains("pub fn parse_line"), "{text}");
        assert!(text.contains("struct Ast"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_map_stops_at_max_chars() {
        let dir = std::env::temp_dir().join(format!("bwn-repomap-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.join("b.rs"), "fn b() {}\n").unwrap();
        let files = vec![dir.join("a.rs"), dir.join("b.rs")];
        let text = render(&dir, &files, "", 40);
        assert!(text.chars().count() <= 80, "{text}");
        assert!(text.contains('…'), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
