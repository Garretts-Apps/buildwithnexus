//! Syntax highlighting for fenced code blocks — a small hand-rolled lexer
//! per language family, no grammar files, no dependencies. It runs once per
//! block when the closing fence lands (never per streamed token), so the
//! cost is a single pass over the block. Unknown languages come back
//! untouched.

use crate::tui;

/// One language's lexical shape. Everything not listed falls through as
/// plain text, so an incomplete keyword list degrades to "less colour",
/// never to wrong colour.
pub struct Lang {
    line_comment: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    keywords: &'static [&'static str],
    types: &'static [&'static str],
    quotes: &'static [char],
    /// Identifiers starting with an uppercase letter are types (Rust, Go,
    /// Java, TS…); false for Python/shell where that's just a name.
    capital_types: bool,
    /// `$name` and `${name}` are variables (shell, PowerShell, Makefile).
    dollar_vars: bool,
    /// Backticks open strings (JS template literals, Go raw strings).
    backtick_strings: bool,
}

const RUST_KW: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while",
];
const RUST_TY: &[&str] = &[
    "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "str", "u8", "u16",
    "u32", "u64", "u128", "usize", "String", "Vec", "Option", "Result", "Box", "Some", "None",
    "Ok", "Err",
];
static RUST: Lang = Lang {
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    keywords: RUST_KW,
    types: RUST_TY,
    quotes: &['"'],
    capital_types: true,
    dollar_vars: false,
    backtick_strings: false,
};

const C_KW: &[&str] = &[
    "auto",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "constexpr",
    "continue",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "explicit",
    "extern",
    "false",
    "for",
    "friend",
    "goto",
    "if",
    "inline",
    "namespace",
    "new",
    "noexcept",
    "nullptr",
    "operator",
    "override",
    "private",
    "protected",
    "public",
    "register",
    "return",
    "sizeof",
    "static",
    "struct",
    "switch",
    "template",
    "this",
    "throw",
    "true",
    "try",
    "typedef",
    "typename",
    "union",
    "using",
    "virtual",
    "volatile",
    "while",
    "NULL",
    "#include",
    "#define",
    "#if",
    "#ifdef",
    "#ifndef",
    "#else",
    "#endif",
    "#pragma",
];
const C_TY: &[&str] = &[
    "bool", "char", "double", "float", "int", "long", "short", "signed", "unsigned", "void",
    "size_t", "int8_t", "int16_t", "int32_t", "int64_t", "uint8_t", "uint16_t", "uint32_t",
    "uint64_t", "string", "vector", "map",
];
static C: Lang = Lang {
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    keywords: C_KW,
    types: C_TY,
    quotes: &['"', '\''],
    capital_types: true,
    dollar_vars: false,
    backtick_strings: false,
};

const JS_KW: &[&str] = &[
    "abstract",
    "as",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "from",
    "function",
    "get",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "of",
    "private",
    "protected",
    "public",
    "readonly",
    "return",
    "set",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "type",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "with",
    "yield",
    "keyof",
    "declare",
    "namespace",
    "satisfies",
];
const JS_TY: &[&str] = &[
    "any", "boolean", "never", "number", "object", "string", "symbol", "unknown", "bigint",
    "Promise", "Array", "Record", "Map", "Set", "Date", "Error", "JSON", "Math", "console",
];
static JS: Lang = Lang {
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    keywords: JS_KW,
    types: JS_TY,
    quotes: &['"', '\''],
    capital_types: true,
    dollar_vars: false,
    backtick_strings: true,
};

const PY_KW: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield", "self", "match", "case",
];
const PY_TY: &[&str] = &[
    "int",
    "float",
    "str",
    "bytes",
    "bool",
    "list",
    "dict",
    "set",
    "tuple",
    "object",
    "print",
    "len",
    "range",
    "enumerate",
    "zip",
    "isinstance",
    "super",
    "open",
    "type",
];
static PYTHON: Lang = Lang {
    line_comment: &["#"],
    block_comment: None,
    keywords: PY_KW,
    types: PY_TY,
    quotes: &['"', '\''],
    capital_types: false,
    dollar_vars: false,
    backtick_strings: false,
};

const GO_KW: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "map",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "type",
    "var",
    "nil",
    "true",
    "false",
    "iota",
];
const GO_TY: &[&str] = &[
    "bool", "byte", "error", "float32", "float64", "int", "int8", "int16", "int32", "int64",
    "rune", "string", "uint", "uint8", "uint16", "uint32", "uint64", "uintptr", "any", "make",
    "new", "len", "cap", "append", "panic", "recover",
];
static GO: Lang = Lang {
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    keywords: GO_KW,
    types: GO_TY,
    quotes: &['"', '\''],
    capital_types: true,
    dollar_vars: false,
    backtick_strings: true,
};

const JAVA_KW: &[&str] = &[
    "abstract",
    "assert",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "else",
    "enum",
    "extends",
    "final",
    "finally",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "interface",
    "native",
    "new",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "try",
    "volatile",
    "while",
    "true",
    "false",
    "null",
    "var",
    "record",
    "sealed",
    "permits",
    "yield",
    "fun",
    "val",
    "override",
    "data",
    "object",
    "when",
    "is",
    "in",
    "let",
    "guard",
    "struct",
    "protocol",
    "extension",
    "mutating",
];
const JAVA_TY: &[&str] = &[
    "boolean", "byte", "char", "double", "float", "int", "long", "short", "void", "String", "Int",
    "Long", "Double", "Bool", "Unit", "Any", "List", "Map", "Set", "Array",
];
static JAVA: Lang = Lang {
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    keywords: JAVA_KW,
    types: JAVA_TY,
    quotes: &['"', '\''],
    capital_types: true,
    dollar_vars: false,
    backtick_strings: false,
};

const SH_KW: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "in", "function", "select", "return", "exit", "export", "local", "readonly", "declare", "set",
    "unset", "shift", "source", "alias", "echo", "cd", "sudo", "true", "false", "break",
    "continue",
];
static SHELL: Lang = Lang {
    line_comment: &["#"],
    block_comment: None,
    keywords: SH_KW,
    types: &[],
    quotes: &['"', '\''],
    capital_types: false,
    dollar_vars: true,
    backtick_strings: true,
};

const RUBY_KW: &[&str] = &[
    "alias",
    "and",
    "begin",
    "break",
    "case",
    "class",
    "def",
    "defined?",
    "do",
    "else",
    "elsif",
    "end",
    "ensure",
    "false",
    "for",
    "if",
    "in",
    "module",
    "next",
    "nil",
    "not",
    "or",
    "redo",
    "rescue",
    "retry",
    "return",
    "self",
    "super",
    "then",
    "true",
    "undef",
    "unless",
    "until",
    "when",
    "while",
    "yield",
    "require",
    "attr_accessor",
    "puts",
];
static RUBY: Lang = Lang {
    line_comment: &["#"],
    block_comment: None,
    keywords: RUBY_KW,
    types: &[],
    quotes: &['"', '\''],
    capital_types: true,
    dollar_vars: true,
    backtick_strings: true,
};

const SQL_KW: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "CREATE",
    "TABLE",
    "DROP",
    "ALTER",
    "ADD",
    "COLUMN",
    "INDEX",
    "VIEW",
    "JOIN",
    "LEFT",
    "RIGHT",
    "INNER",
    "OUTER",
    "FULL",
    "ON",
    "AS",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "EXISTS",
    "GROUP",
    "BY",
    "ORDER",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "UNION",
    "ALL",
    "DISTINCT",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "PRIMARY",
    "KEY",
    "FOREIGN",
    "REFERENCES",
    "DEFAULT",
    "UNIQUE",
    "CONSTRAINT",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "WITH",
    "RETURNING",
    "TRUE",
    "FALSE",
    "ASC",
    "DESC",
    "select",
    "from",
    "where",
    "insert",
    "into",
    "values",
    "update",
    "set",
    "delete",
    "create",
    "table",
    "drop",
    "alter",
    "join",
    "left",
    "inner",
    "on",
    "as",
    "and",
    "or",
    "not",
    "null",
    "is",
    "in",
    "group",
    "by",
    "order",
    "limit",
    "with",
    "returning",
    "primary",
    "key",
    "default",
    "unique",
];
const SQL_TY: &[&str] = &[
    "INT",
    "INTEGER",
    "BIGINT",
    "SMALLINT",
    "TEXT",
    "VARCHAR",
    "CHAR",
    "BOOLEAN",
    "DATE",
    "TIMESTAMP",
    "TIMESTAMPTZ",
    "SERIAL",
    "UUID",
    "JSON",
    "JSONB",
    "NUMERIC",
    "DECIMAL",
    "REAL",
    "FLOAT",
    "BYTEA",
    "int",
    "integer",
    "text",
    "varchar",
    "boolean",
    "timestamp",
    "serial",
    "uuid",
    "jsonb",
];
static SQL: Lang = Lang {
    line_comment: &["--"],
    block_comment: Some(("/*", "*/")),
    keywords: SQL_KW,
    types: SQL_TY,
    quotes: &['\''],
    capital_types: false,
    dollar_vars: false,
    backtick_strings: false,
};

const JSON_KW: &[&str] = &["true", "false", "null"];
static JSON: Lang = Lang {
    line_comment: &[],
    block_comment: None,
    keywords: JSON_KW,
    types: &[],
    quotes: &['"'],
    capital_types: false,
    dollar_vars: false,
    backtick_strings: false,
};

const YAML_KW: &[&str] = &["true", "false", "null", "yes", "no", "on", "off", "~"];
static YAML: Lang = Lang {
    line_comment: &["#"],
    block_comment: None,
    keywords: YAML_KW,
    types: &[],
    quotes: &['"', '\''],
    capital_types: false,
    dollar_vars: false,
    backtick_strings: false,
};

static CSS: Lang = Lang {
    line_comment: &[],
    block_comment: Some(("/*", "*/")),
    keywords: &[
        "important",
        "@media",
        "@import",
        "@keyframes",
        "@font-face",
        "!important",
    ],
    types: &[],
    quotes: &['"', '\''],
    capital_types: false,
    dollar_vars: false,
    backtick_strings: false,
};

static HTML: Lang = Lang {
    line_comment: &[],
    block_comment: Some(("<!--", "-->")),
    keywords: &[],
    types: &[],
    quotes: &['"', '\''],
    capital_types: false,
    dollar_vars: false,
    backtick_strings: false,
};

static TOML: Lang = Lang {
    line_comment: &["#"],
    block_comment: None,
    keywords: &["true", "false"],
    types: &[],
    quotes: &['"', '\''],
    capital_types: false,
    dollar_vars: false,
    backtick_strings: false,
};

/// Resolve a fence tag (`rust`, `TypeScript`, `sh`, …) to a lexer.
pub fn lang_for(tag: &str) -> Option<&'static Lang> {
    let t = tag.trim().to_ascii_lowercase();
    let t = t.split([' ', ':', ',']).next().unwrap_or("");
    Some(match t {
        "rust" | "rs" => &RUST,
        "c" | "cpp" | "c++" | "cc" | "h" | "hpp" | "cxx" | "objc" | "csharp" | "cs" => &C,
        "js" | "javascript" | "jsx" | "ts" | "typescript" | "tsx" | "mjs" | "cjs" => &JS,
        "py" | "python" | "python3" => &PYTHON,
        "go" | "golang" => &GO,
        "java" | "kotlin" | "kt" | "swift" | "scala" | "dart" | "groovy" => &JAVA,
        "sh" | "bash" | "zsh" | "shell" | "console" | "fish" | "dockerfile" | "docker"
        | "makefile" | "make" | "powershell" | "ps1" => &SHELL,
        "rb" | "ruby" | "php" | "perl" | "elixir" | "ex" | "exs" | "lua" => &RUBY,
        "sql" | "postgres" | "postgresql" | "mysql" | "sqlite" | "plsql" => &SQL,
        "json" | "jsonc" | "json5" => &JSON,
        "yaml" | "yml" => &YAML,
        "toml" | "ini" | "cfg" | "conf" | "env" | "properties" => &TOML,
        "css" | "scss" | "less" => &CSS,
        "html" | "xml" | "svg" | "vue" | "svelte" | "astro" => &HTML,
        _ => return None,
    })
}

/// Lexer state carried between lines of one block (an open `/* … */` or a
/// still-open backtick/triple-quote string).
#[derive(Default)]
pub struct State {
    in_block_comment: bool,
    in_string: Option<Str>,
}

#[derive(Clone, Copy, PartialEq)]
enum Str {
    Triple(char),
    Backtick,
}

fn kw(s: &str) -> String {
    tui::accent(s)
}
fn ty(s: &str) -> String {
    tui::cyan(s)
}
fn string(s: &str) -> String {
    tui::green(s)
}
fn number(s: &str) -> String {
    tui::yellow(s)
}
fn comment(s: &str) -> String {
    tui::dim(s)
}
fn func(s: &str) -> String {
    tui::blue(s)
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}
fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Highlight one line. Diff-style fences (`diff`, `patch`) are handled by
/// [`highlight_block`] before the lexer; everything else lands here.
pub fn highlight_line(lang: &Lang, line: &str, st: &mut State) -> String {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(line.len() * 2);
    let mut i = 0;
    // Fast path: continue a multi-line construct.
    if st.in_block_comment {
        let end = lang.block_comment.map(|b| b.1).unwrap_or("*/");
        if let Some(pos) = line.find(end) {
            let cut = pos + end.len();
            out.push_str(&comment(&line[..cut]));
            st.in_block_comment = false;
            let rest = highlight_line(lang, &line[cut..], st);
            out.push_str(&rest);
        } else {
            out.push_str(&comment(line));
        }
        return out;
    }
    if let Some(kind) = st.in_string {
        let close = match kind {
            Str::Triple(q) => std::iter::repeat_n(q, 3).collect::<String>(),
            Str::Backtick => "`".to_string(),
        };
        if let Some(pos) = line.find(&close) {
            let cut = pos + close.len();
            out.push_str(&string(&line[..cut]));
            st.in_string = None;
            out.push_str(&highlight_line(lang, &line[cut..], st));
        } else {
            out.push_str(&string(line));
        }
        return out;
    }
    let at = |i: usize, pat: &str| -> bool {
        let pc: Vec<char> = pat.chars().collect();
        i + pc.len() <= n && chars[i..i + pc.len()] == pc[..]
    };
    let mut plain = String::new();
    let flush = |plain: &mut String, out: &mut String| {
        if !plain.is_empty() {
            out.push_str(plain);
            plain.clear();
        }
    };
    while i < n {
        let c = chars[i];
        // Comments.
        if lang
            .line_comment
            .iter()
            .any(|lc| lc.starts_with(c) && at(i, lc))
        {
            flush(&mut plain, &mut out);
            let rest: String = chars[i..].iter().collect();
            out.push_str(&comment(&rest));
            return out;
        }
        if let Some((open, close)) = lang.block_comment {
            if open.starts_with(c) && at(i, open) {
                flush(&mut plain, &mut out);
                let rest: String = chars[i..].iter().collect();
                if let Some(pos) = rest[open.len()..].find(close) {
                    let cut = open.len() + pos + close.len();
                    out.push_str(&comment(&rest[..cut]));
                    i += rest[..cut].chars().count();
                    continue;
                }
                out.push_str(&comment(&rest));
                st.in_block_comment = true;
                return out;
            }
        }
        // Strings.
        if lang.quotes.contains(&c) || (lang.backtick_strings && c == '`') {
            flush(&mut plain, &mut out);
            // Python-style triple quotes and JS template literals may span lines.
            if c != '`' && at(i, &std::iter::repeat_n(c, 3).collect::<String>()) {
                let rest: String = chars[i + 3..].iter().collect();
                let close: String = std::iter::repeat_n(c, 3).collect();
                if let Some(pos) = rest.find(&close) {
                    let taken = 3 + rest[..pos].chars().count() + 3;
                    let s: String = chars[i..i + taken].iter().collect();
                    out.push_str(&string(&s));
                    i += taken;
                } else {
                    let s: String = chars[i..].iter().collect();
                    out.push_str(&string(&s));
                    st.in_string = Some(Str::Triple(c));
                    return out;
                }
                continue;
            }
            let mut j = i + 1;
            let mut closed = false;
            while j < n {
                if chars[j] == '\\' {
                    j += 2;
                    continue;
                }
                if chars[j] == c {
                    closed = true;
                    break;
                }
                j += 1;
            }
            if closed {
                let s: String = chars[i..=j].iter().collect();
                // JSON/YAML object keys read better in the type colour.
                let is_key = std::ptr::eq(lang, &JSON)
                    && chars[j + 1..].iter().find(|ch| !ch.is_whitespace()) == Some(&':');
                out.push_str(&if is_key { ty(&s) } else { string(&s) });
                i = j + 1;
            } else if c == '`' {
                let s: String = chars[i..].iter().collect();
                out.push_str(&string(&s));
                st.in_string = Some(Str::Backtick);
                return out;
            } else {
                // Unterminated single-line string (or an apostrophe in
                // prose): leave the quote plain and carry on.
                plain.push(c);
                i += 1;
            }
            continue;
        }
        // Shell/Ruby variables.
        if lang.dollar_vars
            && c == '$'
            && i + 1 < n
            && (is_ident(chars[i + 1]) || chars[i + 1] == '{')
        {
            flush(&mut plain, &mut out);
            let mut j = i + 1;
            if chars[j] == '{' {
                while j < n && chars[j] != '}' {
                    j += 1;
                }
                j = (j + 1).min(n);
            } else {
                while j < n && is_ident(chars[j]) {
                    j += 1;
                }
            }
            let s: String = chars[i..j].iter().collect();
            out.push_str(&ty(&s));
            i = j;
            continue;
        }
        // Numbers.
        if c.is_ascii_digit() && (i == 0 || !is_ident(chars[i - 1])) {
            flush(&mut plain, &mut out);
            let mut j = i + 1;
            while j < n && (chars[j].is_ascii_alphanumeric() || chars[j] == '.' || chars[j] == '_')
            {
                j += 1;
            }
            let s: String = chars[i..j].iter().collect();
            out.push_str(&number(&s));
            i = j;
            continue;
        }
        // Identifiers, keywords, types, calls. `#include`-style words keep
        // their sigil so C preprocessor lines match the keyword table.
        if is_ident_start(c) || (c == '#' && i + 1 < n && is_ident_start(chars[i + 1])) {
            let mut j = i + 1;
            while j < n && (is_ident(chars[j]) || chars[j] == '?' && j + 1 == n) {
                j += 1;
            }
            let word: String = chars[i..j].iter().collect();
            let next_nonspace = chars[j..].iter().find(|ch| !ch.is_whitespace()).copied();
            let styled = if lang.keywords.contains(&word.as_str()) {
                Some(kw(&word))
            } else if lang.types.contains(&word.as_str()) {
                Some(ty(&word))
            } else if std::ptr::eq(lang, &YAML) || std::ptr::eq(lang, &TOML) {
                // `key:` / `key =` at the start of a YAML/TOML line.
                let lead: String = chars[..i].iter().collect();
                let is_key = lead.trim().is_empty() || lead.trim() == "-";
                let sep = if std::ptr::eq(lang, &YAML) { ':' } else { '=' };
                if is_key && next_nonspace == Some(sep) {
                    Some(ty(&word))
                } else {
                    None
                }
            } else if next_nonspace == Some('(') {
                Some(func(&word))
            } else if lang.capital_types
                && c.is_uppercase()
                && word.chars().any(|ch| ch.is_lowercase())
            {
                Some(ty(&word))
            } else {
                None
            };
            match styled {
                Some(s) => {
                    flush(&mut plain, &mut out);
                    out.push_str(&s);
                }
                None => plain.push_str(&word),
            }
            i = j;
            continue;
        }
        // HTML tags: `<name` and `</name`.
        if std::ptr::eq(lang, &HTML) && c == '<' {
            let mut j = i + 1;
            if j < n && chars[j] == '/' {
                j += 1;
            }
            let start = j;
            while j < n && (is_ident(chars[j]) || chars[j] == '-' || chars[j] == ':') {
                j += 1;
            }
            if j > start {
                flush(&mut plain, &mut out);
                let s: String = chars[i..j].iter().collect();
                out.push_str(&kw(&s));
                i = j;
                continue;
            }
        }
        plain.push(c);
        i += 1;
    }
    flush(&mut plain, &mut out);
    out
}

/// Highlight a whole fenced block. Returns the lines unchanged when the tag
/// is unknown or colour is disabled.
pub fn highlight_block(tag: &str, lines: &[String]) -> Vec<String> {
    if tui::color_disabled() {
        return lines.to_vec();
    }
    let t = tag.trim().to_ascii_lowercase();
    if t == "diff" || t == "patch" {
        return lines
            .iter()
            .map(|l| {
                if l.starts_with("+++") || l.starts_with("---") {
                    tui::bold(l)
                } else if l.starts_with('+') {
                    tui::green(l)
                } else if l.starts_with('-') {
                    tui::red(l)
                } else if l.starts_with("@@") {
                    tui::cyan(l)
                } else {
                    l.clone()
                }
            })
            .collect();
    }
    let Some(lang) = lang_for(tag) else {
        return lines.to_vec();
    };
    let mut st = State::default();
    lines
        .iter()
        .map(|l| highlight_line(lang, l, &mut st))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> String {
        // Strip SGR to compare content; presence of escapes is asserted separately.
        let mut out = String::new();
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c == '\x1b' {
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn content_is_preserved_exactly() {
        std::env::remove_var("NO_COLOR");
        let src = [
            "fn main() { let x: u32 = 42; // answer",
            "    println!(\"hi {}\", x); /* start",
            "    still comment */ let s = \"a \\\" b\";",
        ];
        let lines: Vec<String> = src.iter().map(|s| s.to_string()).collect();
        let out = highlight_block("rust", &lines);
        for (a, b) in out.iter().zip(src.iter()) {
            assert_eq!(plain(a), *b);
        }
        assert!(out[0].contains('\x1b'));
    }

    #[test]
    fn block_comment_spans_lines() {
        let mut st = State::default();
        let a = highlight_line(&RUST, "let a = 1; /* open", &mut st);
        assert!(st.in_block_comment);
        assert!(a.ends_with(&comment("/* open")));
        let b = highlight_line(&RUST, "still */ let b = 2;", &mut st);
        assert!(!st.in_block_comment);
        assert!(b.starts_with(&comment("still */")));
        assert!(b.contains(&kw("let")));
    }

    #[test]
    fn classifies_tokens() {
        let mut st = State::default();
        let l = highlight_line(&PYTHON, "def f(x): return \"s\" + str(3) # c", &mut st);
        assert!(l.contains(&kw("def")));
        assert!(l.contains(&func("f")));
        assert!(l.contains(&string("\"s\"")));
        assert!(l.contains(&number("3")));
        assert!(l.ends_with(&comment("# c")));
        let j = highlight_line(&JSON, "{\"k\": \"v\", \"n\": null}", &mut State::default());
        assert!(j.contains(&ty("\"k\"")));
        assert!(j.contains(&string("\"v\"")));
        assert!(j.contains(&kw("null")));
        let s = highlight_line(&SHELL, "echo \"$HOME\" ${X}y", &mut State::default());
        assert!(s.contains(&kw("echo")));
        assert!(s.contains(&ty("${X}")));
        let h = highlight_line(&HTML, "<div class=\"a\">", &mut State::default());
        assert!(h.starts_with(&kw("<div")));
        let y = highlight_line(&YAML, "name: bwn # x", &mut State::default());
        assert!(y.starts_with(&ty("name")));
    }

    #[test]
    fn unknown_language_and_diff() {
        let lines = vec![
            "-old".to_string(),
            "+new".to_string(),
            "@@ -1 +1 @@".to_string(),
        ];
        let d = highlight_block("diff", &lines);
        assert_eq!(d[0], tui::red("-old"));
        assert_eq!(d[1], tui::green("+new"));
        assert_eq!(highlight_block("brainfuck", &lines), lines);
        assert!(lang_for("TypeScript").is_some());
        assert!(lang_for("ts {1,3}").is_some());
    }
}
