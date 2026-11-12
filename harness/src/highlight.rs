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

/// The escapes each token class is wrapped in, looked up once per block
/// (or line) rather than once per token.
struct Paints(tui::SpanPaint);

impl Paints {
    fn new() -> Self {
        Paints(tui::span_paint())
    }
    fn span(&self, out: &mut String, on: &str, s: &str) {
        out.push_str(on);
        out.push_str(s);
        out.push_str(&self.0.reset);
    }
    fn kw(&self, out: &mut String, s: &str) {
        self.span(out, &self.0.accent, s);
    }
    fn ty(&self, out: &mut String, s: &str) {
        self.span(out, &self.0.info, s);
    }
    fn string(&self, out: &mut String, s: &str) {
        self.span(out, &self.0.success, s);
    }
    fn number(&self, out: &mut String, s: &str) {
        self.span(out, &self.0.warning, s);
    }
    fn comment(&self, out: &mut String, s: &str) {
        self.span(out, &self.0.muted, s);
    }
    fn func(&self, out: &mut String, s: &str) {
        self.span(out, &self.0.info, s);
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}
fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

// The character starting at byte `i` (a character boundary, `i < len`).
fn char_at(s: &str, i: usize) -> char {
    let b = s.as_bytes()[i];
    if b < 0x80 {
        b as char
    } else {
        s[i..].chars().next().unwrap_or('\u{fffd}')
    }
}

// Three `q` in a row, as a pattern to search for.
struct Triple {
    buf: [u8; 12],
    len: usize,
}

impl Triple {
    fn new(q: char) -> Self {
        let mut buf = [0u8; 12];
        let n = q.len_utf8();
        for k in 0..3 {
            q.encode_utf8(&mut buf[k * n..]);
        }
        Triple { buf, len: 3 * n }
    }
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

// Byte offset of the first run of three `q` in `s`.
fn find_triple(s: &str, q: char) -> Option<usize> {
    s.find(Triple::new(q).as_str())
}

/// Highlight one line. Diff-style fences (`diff`, `patch`) are handled by
/// [`highlight_block`] before the lexer; everything else lands here.
pub fn highlight_line(lang: &Lang, line: &str, st: &mut State) -> String {
    let mut out = String::with_capacity(line.len() * 2);
    lex_line(lang, line, st, &Paints::new(), &mut out);
    out
}

// One pass over `line` by byte offset: plain text is copied through in
// slices, each token is pushed between its colour and the reset. Nothing is
// allocated but `out`.
fn lex_line(lang: &Lang, mut line: &str, st: &mut State, p: &Paints, out: &mut String) {
    // Continue a multi-line construct. What follows its end is lexed as a
    // line of its own: "start of line" below means the start of that rest.
    loop {
        if st.in_block_comment {
            let end = lang.block_comment.map(|b| b.1).unwrap_or("*/");
            let Some(pos) = line.find(end) else {
                p.comment(out, line);
                return;
            };
            let cut = pos + end.len();
            p.comment(out, &line[..cut]);
            st.in_block_comment = false;
            line = &line[cut..];
        } else if let Some(kind) = st.in_string {
            let found = match kind {
                Str::Triple(q) => find_triple(line, q).map(|pos| pos + 3 * q.len_utf8()),
                Str::Backtick => line.find('`').map(|pos| pos + 1),
            };
            let Some(cut) = found else {
                p.string(out, line);
                return;
            };
            p.string(out, &line[..cut]);
            st.in_string = None;
            line = &line[cut..];
        } else {
            break;
        }
    }
    let bytes = line.as_bytes();
    let n = bytes.len();
    let is_json = std::ptr::eq(lang, &JSON);
    let is_yaml = std::ptr::eq(lang, &YAML);
    let is_toml = std::ptr::eq(lang, &TOML);
    let is_html = std::ptr::eq(lang, &HTML);
    // Plain text not yet copied to `out` starts here.
    let mut plain = 0usize;
    let mut i = 0usize;
    while i < n {
        let c = char_at(line, i);
        let cl = c.len_utf8();
        let rest = &line[i..];
        // Comments.
        if lang.line_comment.iter().any(|lc| rest.starts_with(lc)) {
            out.push_str(&line[plain..i]);
            p.comment(out, rest);
            return;
        }
        if let Some((open, close)) = lang.block_comment {
            if let Some(body) = rest.strip_prefix(open) {
                out.push_str(&line[plain..i]);
                if let Some(pos) = body.find(close) {
                    let cut = open.len() + pos + close.len();
                    p.comment(out, &rest[..cut]);
                    i += cut;
                    plain = i;
                    continue;
                }
                p.comment(out, rest);
                st.in_block_comment = true;
                return;
            }
        }
        // Strings.
        if lang.quotes.contains(&c) || (lang.backtick_strings && c == '`') {
            // Python-style triple quotes and JS template literals may span lines.
            if c != '`' && rest.starts_with(Triple::new(c).as_str()) {
                out.push_str(&line[plain..i]);
                let body = &rest[3 * cl..];
                if let Some(pos) = find_triple(body, c) {
                    let taken = 3 * cl + pos + 3 * cl;
                    p.string(out, &rest[..taken]);
                    i += taken;
                    plain = i;
                } else {
                    p.string(out, rest);
                    st.in_string = Some(Str::Triple(c));
                    return;
                }
                continue;
            }
            // The closing quote: the first one not preceded by a backslash
            // (a backslash hides whatever one character follows it).
            let mut closed = None;
            let mut chars = rest[cl..].char_indices();
            while let Some((off, ch)) = chars.next() {
                if ch == '\\' {
                    chars.next();
                } else if ch == c {
                    closed = Some(cl + off + cl);
                    break;
                }
            }
            if let Some(len) = closed {
                out.push_str(&line[plain..i]);
                let s = &rest[..len];
                // JSON/YAML object keys read better in the type colour.
                let is_key =
                    is_json && rest[len..].chars().find(|ch| !ch.is_whitespace()) == Some(':');
                if is_key {
                    p.ty(out, s);
                } else {
                    p.string(out, s);
                }
                i += len;
                plain = i;
            } else if c == '`' {
                out.push_str(&line[plain..i]);
                p.string(out, rest);
                st.in_string = Some(Str::Backtick);
                return;
            } else {
                // Unterminated single-line string (or an apostrophe in
                // prose): leave the quote plain and carry on.
                i += cl;
            }
            continue;
        }
        // Shell/Ruby variables.
        if lang.dollar_vars && c == '$' {
            let next = rest[1..].chars().next();
            if next.is_some_and(|d| is_ident(d) || d == '{') {
                let len = if next == Some('{') {
                    rest.find('}').map_or(rest.len(), |pos| pos + 1)
                } else {
                    1 + ident_len(&rest[1..])
                };
                out.push_str(&line[plain..i]);
                p.ty(out, &rest[..len]);
                i += len;
                plain = i;
                continue;
            }
        }
        // Numbers.
        if c.is_ascii_digit() && !line[..i].chars().next_back().is_some_and(is_ident) {
            let mut j = i + 1;
            while j < n
                && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'.' || bytes[j] == b'_')
            {
                j += 1;
            }
            out.push_str(&line[plain..i]);
            p.number(out, &line[i..j]);
            i = j;
            plain = i;
            continue;
        }
        // Identifiers, keywords, types, calls. `#include`-style words keep
        // their sigil so C preprocessor lines match the keyword table.
        if is_ident_start(c) || (c == '#' && rest[1..].chars().next().is_some_and(is_ident_start)) {
            let mut j = i + cl;
            for ch in line[j..].chars() {
                let len = ch.len_utf8();
                if is_ident(ch) || (ch == '?' && j + len == n) {
                    j += len;
                } else {
                    break;
                }
            }
            let word = &line[i..j];
            let next_nonspace = || line[j..].chars().find(|ch| !ch.is_whitespace());
            #[derive(Clone, Copy)]
            enum Class {
                Kw,
                Ty,
                Func,
                Plain,
            }
            let class = if lang.keywords.contains(&word) {
                Class::Kw
            } else if lang.types.contains(&word) {
                Class::Ty
            } else if is_yaml || is_toml {
                // `key:` / `key =` at the start of a YAML/TOML line.
                let lead = line[..i].trim();
                let is_key = lead.is_empty() || lead == "-";
                let sep = if is_yaml { ':' } else { '=' };
                if is_key && next_nonspace() == Some(sep) {
                    Class::Ty
                } else {
                    Class::Plain
                }
            } else if next_nonspace() == Some('(') {
                Class::Func
            } else if lang.capital_types
                && c.is_uppercase()
                && word.chars().any(|ch| ch.is_lowercase())
            {
                Class::Ty
            } else {
                Class::Plain
            };
            if !matches!(class, Class::Plain) {
                out.push_str(&line[plain..i]);
                match class {
                    Class::Kw => p.kw(out, word),
                    Class::Ty => p.ty(out, word),
                    _ => p.func(out, word),
                }
                plain = j;
            }
            i = j;
            continue;
        }
        // HTML tags: `<name` and `</name`.
        if is_html && c == '<' {
            let mut j = i + 1;
            if j < n && bytes[j] == b'/' {
                j += 1;
            }
            let start = j;
            for ch in line[j..].chars() {
                if is_ident(ch) || ch == '-' || ch == ':' {
                    j += ch.len_utf8();
                } else {
                    break;
                }
            }
            if j > start {
                out.push_str(&line[plain..i]);
                p.kw(out, &line[i..j]);
                i = j;
                plain = i;
                continue;
            }
        }
        i += cl;
    }
    out.push_str(&line[plain..]);
}

// Bytes of the run of identifier characters at the start of `s`.
fn ident_len(s: &str) -> usize {
    s.chars()
        .take_while(|c| is_ident(*c))
        .map(char::len_utf8)
        .sum()
}

/// Highlight a whole fenced block. Returns the lines unchanged when the tag
/// is unknown or colour is disabled.
pub fn highlight_block(tag: &str, lines: &[String]) -> Vec<String> {
    if tui::no_color() {
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
    let paints = Paints::new();
    lines
        .iter()
        .map(|l| {
            let mut out = String::with_capacity(l.len() * 2);
            lex_line(lang, l, &mut st, &paints, &mut out);
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert_eq!(crate::tui::strip_ansi(a), *b);
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

    // ── output snapshot ──────────────────────────────────────────────────────
    // The lexer's output, pinned: a hash per input of what every language's
    // lexer makes of it. The inputs are frozen excerpts of this repository
    // (tests/fixtures/highlight) and the edge cases below. Colours are
    // replaced by role tags first, so the hashes do not depend on the theme
    // or the terminal's colour depth.

    const SNAP_LANGS: &[&str] = &[
        "rust", "c", "js", "py", "go", "java", "sh", "rb", "sql", "json", "yaml", "toml", "css",
        "html", "diff", "nope",
    ];

    fn fnv1a(h: &mut u64, bytes: &[u8]) {
        for b in bytes {
            *h ^= u64::from(*b);
            *h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    // `ESC[<colour>m` … reset becomes `\x02<role>` … `\x03`.
    fn normalise(s: &str) -> String {
        let split = |painted: String| -> (String, String) {
            let (a, b) = painted.split_once('\u{1}').expect("marker");
            (a.to_string(), b.to_string())
        };
        let (_, reset) = split(kw("\u{1}"));
        let (bold_on, bold_off) = split(tui::bold("\u{1}"));
        let mut roles = [
            (split(kw("\u{1}")).0, "k"),
            (split(ty("\u{1}")).0, "t"),
            (split(string("\u{1}")).0, "s"),
            (split(number("\u{1}")).0, "n"),
            (split(comment("\u{1}")).0, "c"),
            (split(tui::red("\u{1}")).0, "r"),
            (bold_on, "b"),
        ];
        roles.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
        for (i, (a, _)) in roles.iter().enumerate() {
            assert!(!a.is_empty() && *a != reset, "colour is on");
            for (b, _) in &roles[i + 1..] {
                assert_ne!(a, b, "two roles share a colour: the snapshot cannot tell");
            }
        }
        let mut out = s.replace(&reset, "\u{3}").replace(&bold_off, "\u{4}");
        for (prefix, tag) in &roles {
            out = out.replace(prefix, &format!("\u{2}{tag}"));
        }
        assert!(!out.contains('\x1b'), "an unknown escape: {out:?}");
        out
    }

    fn snapshot(tags: &[&str], text: &str) -> u64 {
        let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for tag in tags {
            let out = highlight_block(tag, &lines);
            assert_eq!(out.len(), lines.len());
            for (o, l) in out.iter().zip(&lines) {
                assert_eq!(tui::strip_ansi(o), *l, "{tag}: text changed");
                fnv1a(&mut h, normalise(o).as_bytes());
                fnv1a(&mut h, b"\n");
            }
        }
        h
    }

    fn edge_cases() -> Vec<(&'static str, String)> {
        let s = |x: &str| x.to_string();
        vec![
            ("empty", s("")),
            ("blank_lines", s("\n\n   \n\t\n")),
            (
                "unterminated_string",
                s("let s = \"abc\nx = 'it's\nlet t = \"abc\\\nu = 'a\\'"),
            ),
            (
                "backtick_open",
                s("a = `x\ny ${z}\n` + 1; b = `one` + `two\nstill"),
            ),
            (
                "block_comment_open",
                s("a /* b\nc\nd */ e /* f */ g /* h\n*/ */ i\n/* never closed"),
            ),
            (
                "html_comment",
                s("<!-- x\n y --> <div a='1'> <!-- z --> </a-b:c> < 3 <1> <é>"),
            ),
            (
                "triple_quotes",
                s("\"\"\"doc\nmore\n\"\"\" x = 1\n'''a''' + \"\"\"b\nc\"\"\"\"\"\"\n''''''"),
            ),
            (
                "non_ascii",
                s("let naïve = \"héllo wörld\"; // ünïcode 日本語 🎉\n变量 = 1\n\
                   Ünï(x) Élan ÉLAN é1 ٣ 🎉(1)\n'é' \"日本\" `🎉`"),
            ),
            (
                "numbers",
                s("0x1F 1_000 1.5e3 a1 1a _1 x.0 1..2 9é 1٣ .5 -3 3."),
            ),
            (
                "shell_vars",
                s("echo $HOME ${X ${Y}z $ $1 $é \"$A\" ${unclosed\n$_a$b ${} $"),
            ),
            (
                "sigils",
                s("#include <x.h>\n#!sh\n# c\n#é #1 ##a\nfoo? bar?\nbaz?"),
            ),
            (
                "yaml_toml_keys",
                s("- key: v\nkey : v\n  nested: \"x\": y\n~\na = 1\n[t]\na.b = \"c\"\n- - k: v\nk= 1\nk:v"),
            ),
            (
                "json_keys",
                s("{\"k\" : 1, \"s\": \"a\\\"b\", \"u\": \"é\" :}\n\"k\"\u{a0}: 1 \"j\"\u{3000}:\n\"last\""),
            ),
            (
                "wide_spaces",
                s("f\u{2003}(x) g\u{a0}(y) h\t(z) k\u{3000}: 1"),
            ),
            (
                "escapes",
                s("\"a\\\\\" \"b\\\"\" 'c\\'' \"é\\é\" \"abc\\"),
            ),
            (
                "css",
                s("@media !important a { color: #fff } /* c */ important @import 'x'"),
            ),
            ("lone", s("\"\n'\n`\n$\n#\n<\n/\n/*\n*/\n-\n--\n//\n\\\n?")),
            (
                "capitals",
                s("Foo FOO Fo F fOO Élan ÉLAN Ünï Self String Vec<Option<T>>"),
            ),
            (
                "rustisms",
                s("r#\"raw\"# b'x' <'a> 'static fn f<'a>(x: &'a str) -> u8 { 1u8 as f64 }"),
            ),
            (
                "comment_starts",
                s("a//b http://x\na--b --x\na#b\n/ / /**/ /***/ /*/ x */"),
            ),
            (
                "calls",
                s("f(1) g (2) h\t(3) if (x) T(y) t::u(z) é(1) f?(x) f?"),
            ),
            (
                "long_ascii",
                "fn f(a: u32) -> Vec<String> { g(\"s\", 1.0) } ".repeat(300),
            ),
            (
                "long_non_ascii",
                "naïve(\"日本語\", 🎉) + Ünï { 42 } ".repeat(200),
            ),
            ("long_word", "x".repeat(50_000)),
            ("long_string_open", format!("\"{}", "ab ".repeat(2_000))),
        ]
    }

    const FIXTURES: &[(&str, &str, &str)] = &[
        (
            "sample.rs",
            "rust",
            include_str!("../tests/fixtures/highlight/sample.rs.txt"),
        ),
        (
            "sample.py",
            "python",
            include_str!("../tests/fixtures/highlight/sample.py"),
        ),
        (
            "sample.js",
            "js",
            include_str!("../tests/fixtures/highlight/sample.js"),
        ),
        (
            "sample.sh",
            "bash",
            include_str!("../tests/fixtures/highlight/sample.sh"),
        ),
        (
            "sample.ps1",
            "powershell",
            include_str!("../tests/fixtures/highlight/sample.ps1"),
        ),
        (
            "sample.toml",
            "toml",
            include_str!("../tests/fixtures/highlight/sample.toml"),
        ),
        (
            "sample.yml",
            "yaml",
            include_str!("../tests/fixtures/highlight/sample.yml"),
        ),
        (
            "sample.json",
            "json",
            include_str!("../tests/fixtures/highlight/sample.json"),
        ),
    ];

    // (input, hash in its own language, hash across every language).
    const FIXTURE_HASHES: &[(&str, u64, u64)] = &[
        ("sample.rs", 0x30e081ccdce308f5, 0xad386793790ad61e),
        ("sample.py", 0x885367042fd47f87, 0x58d610659d48343f),
        ("sample.js", 0x604073b1d516b86e, 0x7371a04c210e91db),
        ("sample.sh", 0xce093bdf6c731d92, 0xe068fe2df0694d7f),
        ("sample.ps1", 0xce07d0f9b3a6bd8d, 0xb3ee57ba51c70ac4),
        ("sample.toml", 0x8e89ff06ff99153a, 0x8d6dbfa433983720),
        ("sample.yml", 0xb9083f9674e29e2e, 0xcb7b7b7b35a0de34),
        ("sample.json", 0xeed2d63dd0f96557, 0x44e98200f4df31a0),
    ];
    const EDGE_HASHES: &[(&str, u64)] = &[
        ("empty", 0x674bec8e372766a5),
        ("blank_lines", 0xf089fad8b7900ef5),
        ("unterminated_string", 0x2deb239d857cb101),
        ("backtick_open", 0x4fcce5bfc3dd6822),
        ("block_comment_open", 0xc8769c991f982299),
        ("html_comment", 0x84db2a3e04d67e2c),
        ("triple_quotes", 0x330500bf7a6e0546),
        ("non_ascii", 0x29844091eec1c142),
        ("numbers", 0x3d73d8f6e1376357),
        ("shell_vars", 0xcf95a313c70eeaa9),
        ("sigils", 0x6106ca66e5aede46),
        ("yaml_toml_keys", 0x2b73d64e66d1387a),
        ("json_keys", 0xa15a1d1ea395d6a5),
        ("wide_spaces", 0x279db310f80acf1d),
        ("escapes", 0xd6b779b1cc529bb5),
        ("css", 0x3e9f3e0614178b57),
        ("lone", 0xce42e8fbd102e23b),
        ("capitals", 0x41bc416ffe100ddc),
        ("rustisms", 0x974f7c43e9e13001),
        ("comment_starts", 0x7f65dd3d6b0f929b),
        ("calls", 0xf261f0e543c78269),
        ("long_ascii", 0xba22e0369901f56d),
        ("long_non_ascii", 0x0f1b4f1e7fcb742d),
        ("long_word", 0x52c9a2d56430eea5),
        ("long_string_open", 0x1d40d6ca3dcd9c25),
    ];

    #[test]
    fn output_matches_the_snapshot() {
        std::env::remove_var("NO_COLOR");
        let mut got_fixtures = Vec::new();
        for (name, tag, text) in FIXTURES {
            got_fixtures.push((*name, snapshot(&[tag], text), snapshot(SNAP_LANGS, text)));
        }
        let mut got_edges = Vec::new();
        for (name, text) in edge_cases() {
            got_edges.push((name, snapshot(SNAP_LANGS, &text)));
        }
        // To regenerate after an intended change: copy these from the
        // failure message into the two tables above.
        let mut listing = String::from("FIXTURE_HASHES:\n");
        for (n, a, b) in &got_fixtures {
            listing.push_str(&format!("        (\"{n}\", 0x{a:016x}, 0x{b:016x}),\n"));
        }
        listing.push_str("EDGE_HASHES:\n");
        for (n, a) in &got_edges {
            listing.push_str(&format!("        (\"{n}\", 0x{a:016x}),\n"));
        }
        assert!(
            got_fixtures == FIXTURE_HASHES && got_edges == EDGE_HASHES,
            "highlighter output changed:\n{listing}"
        );
    }
}
