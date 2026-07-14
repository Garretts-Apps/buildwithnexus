// The tool surface the agent can call. Definitions are data; execution is a
// single `match` — no registry, no dyn dispatch (Casey: don't build the plugin
// system before there are plugins).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::checkpoint;

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value, // JSON Schema for the input object
}

pub struct Outcome {
    pub content: String,
    pub is_error: bool,
    pub finished: bool, // the `finish` tool ends the loop
}

fn ok(s: impl Into<String>) -> Outcome {
    Outcome {
        content: s.into(),
        is_error: false,
        finished: false,
    }
}
fn err(s: impl Into<String>) -> Outcome {
    Outcome {
        content: s.into(),
        is_error: true,
        finished: false,
    }
}

const MAX_READ: usize = 100 * 1024;
const MAX_OUT: usize = 16 * 1024;
const MAX_SEARCH_FILES: usize = 10_000;
const DEFAULT_SEARCH_LIMIT: usize = 100;
// Head kept by head+tail truncation of command output (the rest of MAX_OUT
// goes to the tail, where failures and the `[exit N]` marker live).
const HEAD_KEEP: usize = 4 * 1024;
// Per-line cap for grep/list output so one minified line can't eat the budget.
const MAX_MATCH_LINE: usize = 500;
// Default deadline for run_command / python_tool; long-running processes
// should use start_server instead.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
// Deadline for a single MCP tools/call round trip.
const MCP_TIMEOUT: Duration = Duration::from_secs(30);

static UNDO_BACKUP: Mutex<Option<(PathBuf, String)>> = Mutex::new(None);

// Exposed only for the criterion perf suite (see provider::bench).
#[doc(hidden)]
pub mod bench {
    use std::path::{Path, PathBuf};
    pub fn normalize(p: &Path) -> PathBuf {
        super::normalize(p)
    }
    pub fn truncate(s: String, max: usize) -> String {
        super::truncate(s, max)
    }
}

pub fn defs(include_subagent: bool) -> Vec<ToolDef> {
    let mut v = vec![
        ToolDef { name: "bash", description: "Common coding-agent alias: run a shell command in the project environment. Prefer glob/grep/read/list for simple navigation.",
            schema: json!({"type":"object","properties":{"command":{"type":"string"},"description":{"type":"string"}},"required":["command"]}) },
        ToolDef { name: "read", description: "Common coding-agent alias: read a UTF-8 text file. Accepts path or filePath and expands `~`.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"filePath":{"type":"string"},"start_line":{"type":"integer","minimum":1},"end_line":{"type":"integer","minimum":1}}}) },
        ToolDef { name: "write", description: "Common coding-agent alias: create or overwrite a file. Accepts path or filePath.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"filePath":{"type":"string"},"content":{"type":"string"}},"required":["content"]}) },
        ToolDef { name: "edit", description: "Common coding-agent alias: replace a unique string in a file. Accepts old/new or oldString/newString.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"filePath":{"type":"string"},"old":{"type":"string"},"new":{"type":"string"},"oldString":{"type":"string"},"newString":{"type":"string"}}}) },
        ToolDef { name: "patch", description: "Common coding-agent alias: apply a unified diff patch to the current repository using git apply.",
            schema: json!({"type":"object","properties":{"patch":{"type":"string"}},"required":["patch"]}) },
        ToolDef { name: "glob", description: "Common coding-agent alias: find files and directories by glob/name pattern. Use for folder lookup too.",
            schema: json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string","default":"."},"root":{"type":"string"},"kind":{"type":"string","enum":["any","file","dir"],"default":"any"},"max":{"type":"integer","minimum":1,"maximum":500}},"required":["pattern"]}) },
        ToolDef { name: "grep", description: "Common coding-agent alias: search text files for a literal pattern. Optional include limits file names.",
            schema: json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string","default":"."},"root":{"type":"string"},"include":{"type":"string"},"file_pattern":{"type":"string"},"case_sensitive":{"type":"boolean","default":false},"max":{"type":"integer","minimum":1,"maximum":500}},"required":["pattern"]}) },
        ToolDef { name: "list", description: "Common coding-agent alias: list entries in a directory.",
            schema: json!({"type":"object","properties":{"path":{"type":"string","default":"."}}}) },
        ToolDef { name: "webfetch", description: "Common coding-agent alias: fetch a URL via HTTP GET.",
            schema: json!({"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}) },
        ToolDef { name: "websearch", description: "Common coding-agent alias: search the web and return relevant excerpts.",
            schema: json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}) },
        ToolDef { name: "todowrite", description: "Common coding-agent alias: replace the current task todo list.",
            schema: json!({"type":"object","properties":{"items":{"type":"array","items":{"type":"object","properties":{"task":{"type":"string"},"content":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed"]}}},"minItems":0,"maxItems":30}},"required":["items"]}) },
        ToolDef { name: "todoread", description: "Common coding-agent alias: read the current task todo list.",
            schema: json!({"type":"object","properties":{}}) },
        ToolDef { name: "skill", description: "Common coding-agent alias: load the full instructions for one named skill.",
            schema: json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}) },
        ToolDef { name: "question", description: "Ask the user a concise clarifying question when discovery is insufficient or user approval is needed.",
            schema: json!({"type":"object","properties":{"question":{"type":"string"},"default":{"type":"string"}},"required":["question"]}) },
        ToolDef { name: "AskUserQuestion", description: "Ask the user a structured clarifying question when discovery is insufficient or user approval is needed.",
            schema: json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string"},
                    "questions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {"type": "string"}
                            },
                            "required": ["question"]
                        }
                    },
                    "options": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": {"type": "string"},
                                "description": {"type": "string"}
                            },
                            "required": ["label"]
                        }
                    }
                }
            })
        },
        ToolDef { name: "exit_plan", description: "Submit the final actionable plan for the user's task after read-only inspection. The plan must describe task steps, not this tool.",
            schema: json!({"type":"object","properties":{"plan":{"type":"string","description":"Numbered task plan, e.g. 1. Inspect files\\n2. Apply changes\\n3. Verify"},"steps":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":12,"description":"Concrete task steps"}}}) },
        ToolDef { name: "ExitPlanMode", description: "Alias for exit_plan. Submit concrete task steps for approval before BUILD executes.",
            schema: json!({"type":"object","properties":{"plan":{"type":"string","description":"Numbered task plan for the user's task"},"steps":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":12,"description":"Concrete task steps"}}}) },
        ToolDef { name: "str_replace_editor", description: "A tool for viewing, creating, and editing files. Supported commands: `view`, `create`, `str_replace`, `insert`, `undo_edit`. The contents MUST be fully implemented code or text. DO NOT use placeholders.",
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["view", "create", "str_replace", "insert", "undo_edit"]},
                    "path": {"type": "string"},
                    "file_text": {"type": "string"},
                    "old_str": {"type": "string"},
                    "new_str": {"type": "string"},
                    "insert_line": {"type": "integer"},
                    "view_range": {"type": "array", "items": {"type": "integer"}}
                },
                "required": ["command", "path"]
            })
        },
        ToolDef { name: "text_editor_20241022", description: "A tool for viewing, creating, and editing files. Supported commands: `view`, `create`, `str_replace`, `insert`, `undo_edit`. The contents MUST be fully implemented code or text. DO NOT use placeholders.",
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["view", "create", "str_replace", "insert", "undo_edit"]},
                    "path": {"type": "string"},
                    "file_text": {"type": "string"},
                    "old_str": {"type": "string"},
                    "new_str": {"type": "string"},
                    "insert_line": {"type": "integer"},
                    "view_range": {"type": "array", "items": {"type": "integer"}}
                },
                "required": ["command", "path"]
            })
        },
        ToolDef { name: "text_editor_20250124", description: "A tool for viewing, creating, and editing files. Supported commands: `view`, `create`, `str_replace`, `insert`, `undo_edit`. The contents MUST be fully implemented code or text. DO NOT use placeholders.",
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["view", "create", "str_replace", "insert", "undo_edit"]},
                    "path": {"type": "string"},
                    "file_text": {"type": "string"},
                    "old_str": {"type": "string"},
                    "new_str": {"type": "string"},
                    "insert_line": {"type": "integer"},
                    "view_range": {"type": "array", "items": {"type": "integer"}}
                },
                "required": ["command", "path"]
            })
        },
        ToolDef { name: "Artifact", description: "Publish a structured document, webpage, SVG, diagram, or dataset as a self-contained local artifact. The contents MUST be fully implemented code or text. DO NOT use placeholders.",
            schema: json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "contents": {"type": "string"},
                    "type": {"type": "string", "enum": ["html", "markdown", "svg", "json", "text"]}
                },
                "required": ["contents"]
            })
        },
        ToolDef { name: "publish_artifact", description: "Publish a structured document, webpage, SVG, diagram, or dataset as a self-contained local artifact. The contents MUST be fully implemented code or text. DO NOT use placeholders.",
            schema: json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "contents": {"type": "string"},
                    "type": {"type": "string", "enum": ["html", "markdown", "svg", "json", "text"]}
                },
                "required": ["contents"]
            })
        },
        ToolDef { name: "list_python_tools", description: "List Python tool scripts in .buildwithnexus/tools and $NEXUS_HOME/tools. Python tools read JSON on stdin and print text or JSON.",
            schema: json!({"type":"object","properties":{}}) },
        ToolDef { name: "python_tool", description: "Run a Python tool script with JSON input on stdin. Use for specialized local tools that are easier to maintain outside Rust.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"input":{"type":"object"}},"required":["path"]}) },
        ToolDef { name: "read_file", description: "Read a UTF-8 text file and return its contents. Optional start_line/end_line return a bounded line range. Works anywhere on the filesystem and expands `~`.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"start_line":{"type":"integer","minimum":1},"end_line":{"type":"integer","minimum":1}},"required":["path"]}) },
        ToolDef { name: "read_many_files", description: "Read several UTF-8 text files at once. Use for comparing related files without repeated round trips.",
            schema: json!({"type":"object","properties":{"paths":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":20},"max_bytes_per_file":{"type":"integer","minimum":1000,"maximum":100000}},"required":["paths"]}) },
        ToolDef { name: "list_dir", description: "List entries in a directory. Works anywhere on the filesystem and expands `~`.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}) },
        ToolDef { name: "list_tree", description: "Recursively list a bounded directory tree, skipping heavy dependency/build directories. Use before guessing paths.",
            schema: json!({"type":"object","properties":{"path":{"type":"string","default":"."},"max_depth":{"type":"integer","minimum":1,"maximum":8},"max_entries":{"type":"integer","minimum":1,"maximum":1000}}}) },
        ToolDef { name: "file_info", description: "Return metadata for a file or directory: type, size, readonly flag, and modified time when available.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}) },
        ToolDef { name: "find_paths", description: "Recursively find files and/or directories by a simple case-insensitive glob pattern such as `nexus`, `*project*`, or `*.rs`. Use kind=`dir` when the user asks for a folder. Expands `~` and skips heavy build/dependency directories.",
            schema: json!({"type":"object","properties":{"root":{"type":"string","default":"."},"pattern":{"type":"string"},"kind":{"type":"string","enum":["any","file","dir"],"default":"any"},"max":{"type":"integer","minimum":1,"maximum":500}},"required":["pattern"]}) },
        ToolDef { name: "find_files", description: "Recursively find files by a simple case-insensitive glob pattern such as `*.rs`, `src/*test*`, or `*project*`. Expands `~` and skips heavy build/dependency directories.",
            schema: json!({"type":"object","properties":{"root":{"type":"string","default":"."},"pattern":{"type":"string"},"max":{"type":"integer","minimum":1,"maximum":500}},"required":["pattern"]}) },
        ToolDef { name: "grep_files", description: "Search text files for a literal pattern and return path:line matches. Expands `~`; optional file_pattern limits searched files, e.g. `*.rs`.",
            schema: json!({"type":"object","properties":{"root":{"type":"string","default":"."},"pattern":{"type":"string"},"file_pattern":{"type":"string"},"case_sensitive":{"type":"boolean","default":false},"max":{"type":"integer","minimum":1,"maximum":500}},"required":["pattern"]}) },
        ToolDef { name: "write_file", description: "Create or overwrite a file with the given contents.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}) },
        ToolDef { name: "edit_file", description: "Replace the unique occurrence of `old` with `new` in a file.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"old":{"type":"string"},"new":{"type":"string"}},"required":["path","old","new"]}) },
        ToolDef { name: "multi_edit", description: "Apply several unique text replacements to one file in order. Fails without writing if any old text is missing or ambiguous.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"edits":{"type":"array","items":{"type":"object","properties":{"old":{"type":"string"},"new":{"type":"string"}},"required":["old","new"]},"minItems":1,"maxItems":50}},"required":["path","edits"]}) },
        ToolDef { name: "apply_patch", description: "Apply a unified diff patch to the current repository using git apply. Use for multi-file edits when exact patch context is known.",
            schema: json!({"type":"object","properties":{"patch":{"type":"string"}},"required":["patch"]}) },
        ToolDef { name: "create_dir", description: "Create a directory and any missing parents.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}) },
        ToolDef { name: "move_path", description: "Rename or move a file or directory.",
            schema: json!({"type":"object","properties":{"from":{"type":"string"},"to":{"type":"string"}},"required":["from","to"]}) },
        ToolDef { name: "remove_path", description: "Remove a file or empty directory. Does not recursively remove non-empty directories.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}) },
        ToolDef { name: "run_command", description: "Run a shell command (grep, find, git, cargo, etc.) and return its output. Use this for searching, building, or any shell operation.",
            schema: json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}) },
        ToolDef { name: "todo_write", description: "Replace the current task todo list with structured items. Use to track multi-step work.",
            schema: json!({"type":"object","properties":{"items":{"type":"array","items":{"type":"object","properties":{"task":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed"]}},"required":["task","status"]},"minItems":0,"maxItems":30}},"required":["items"]}) },
        ToolDef { name: "todo_read", description: "Read the current task todo list.",
            schema: json!({"type":"object","properties":{}}) },
        ToolDef { name: "create_docx", description: "Create a simple .docx Word document from a title and markdown-like body text. MUST ONLY be used for .docx files. Do NOT use for code, HTML, or plain text.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"},"title":{"type":"string"},"body":{"type":"string"}},"required":["path","title","body"]}) },
        ToolDef { name: "finish", description: "Signal the task is complete with a short summary for the user.",
            schema: json!({"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"]}) },
        ToolDef { name: "save_memory", description: "Save a short note to persistent memory so it's available in future sessions. Use for preferences, recurring facts, or things the user says to remember.",
            schema: json!({"type":"object","properties":{"note":{"type":"string","description":"Short fact or preference to remember (one line)"}},"required":["note"]}) },
        ToolDef { name: "fetch_url", description: "Fetch the content of a URL via HTTP GET. Returns the response body as text. Use for reading documentation, API responses, changelogs, or any web content.",
            schema: json!({"type":"object","properties":{"url":{"type":"string","description":"HTTP or HTTPS URL to fetch"}},"required":["url"]}) },
        ToolDef { name: "web_search", description: "Search the web via DuckDuckGo and return relevant excerpts. Use for current events, documentation lookup, or any question that benefits from live web results.",
            schema: json!({"type":"object","properties":{"query":{"type":"string","description":"Search query"}},"required":["query"]}) },
        ToolDef { name: "headless_browser", description: "Fetch a web page and extract clean text content and links. Returns structured output with title, body text, and extracted links. Use for reading documentation, extracting data from web pages, or following links.",
            schema: json!({"type":"object","properties":{"url":{"type":"string","description":"URL to fetch and extract content from"},"extract_links":{"type":"boolean","description":"If true, also extract all links from the page"}},"required":["url"]}) },
        ToolDef { name: "start_server", description: "Start a long-running local development server in the background. Prefer this over run_command for npm dev/vite/next/cargo/python servers. Stores logs and a server record so list_servers/stop_server can manage it.",
            schema: json!({"type":"object","properties":{"name":{"type":"string"},"command":{"type":"string"},"cwd":{"type":"string"},"port":{"type":"integer","minimum":1,"maximum":65535}},"required":["command"]}) },
        ToolDef { name: "list_servers", description: "List background servers started by start_server, including status, cwd, port, command, and log path.",
            schema: json!({"type":"object","properties":{}}) },
        ToolDef { name: "stop_server", description: "Stop a background server previously started by start_server.",
            schema: json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}) },
        ToolDef { name: "read_server_log", description: "Read the tail of a background server log. Use after starting a server to verify readiness or diagnose failures.",
            schema: json!({"type":"object","properties":{"name":{"type":"string"},"lines":{"type":"integer","minimum":1,"maximum":500}},"required":["name"]}) },
        ToolDef { name: "wait_for_url", description: "Poll a local or remote URL until it responds with the expected status/text. Use after start_server before open_browser or finalizing web work.",
            schema: json!({"type":"object","properties":{"url":{"type":"string"},"timeout_seconds":{"type":"integer","minimum":1,"maximum":120},"expect_status":{"type":"integer","minimum":100,"maximum":599},"expect_text":{"type":"string"}},"required":["url"]}) },
        ToolDef { name: "open_browser", description: "Open a URL or local file in the user's default browser. Use after publishing an HTML artifact or starting a web server.",
            schema: json!({"type":"object","properties":{"url":{"type":"string"},"path":{"type":"string"}}}) },
        ToolDef { name: "list_skills", description: "List available skill names and short descriptions. Use before load_skill when choosing task-specific instructions.",
            schema: json!({"type":"object","properties":{}}) },
        ToolDef { name: "load_skill", description: "Load the full instructions for one named skill. Use only when that skill is relevant to the task.",
            schema: json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}) },
        ToolDef { name: "kb_query", description: "Query the project's local structured knowledge base (.buildwithnexus/knowledge/) for entities, relationships, and architectural decisions.",
            schema: json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}) },
        ToolDef { name: "kb_record", description: "Record an entity, relationship, or architectural decision (with risk, cost, and reversibility primitives) into the local knowledge base.",
            schema: json!({"type":"object","properties":{"name":{"type":"string"},"entity_type":{"type":"string","enum":["Function","Class","Module","Package","Service","Endpoint","DatabaseTable","Migration","ArchitectureDecision"]},"description":{"type":"string"},"path":{"type":"string"}},"required":["name","entity_type"]}) },
        ToolDef { name: "rule_check", description: "Evaluate the current project state and changed files against the project's engineering rules (.buildwithnexus/rules/).",
            schema: json!({"type":"object","properties":{"changed_files":{"type":"array","items":{"type":"string"}},"task_type":{"type":"string"}}}) },
        ToolDef { name: "verify", description: "Run the verification layer (rules, tests, static analysis, confidence calculation) to generate a structured quality and safety report.",
            schema: json!({"type":"object","properties":{"task_description":{"type":"string"},"changed_files":{"type":"array","items":{"type":"string"}}},"required":["task_description"]}) },
        ToolDef { name: "mcp_call", description: "Call a tool or query a resource on an enterprise Model Context Protocol (MCP) server configured in settings.json.",
            schema: json!({"type":"object","properties":{"server":{"type":"string"},"tool":{"type":"string"},"arguments":{"type":"object"}},"required":["server","tool"]}) },
    ];
    if include_subagent {
        v.push(ToolDef {
            name: "task",
            description:
                "Common coding-agent alias: delegate a self-contained sub-task to a fresh agent.",
            schema: json!({"type":"object","properties":{
                "task":{"type":"string"},
                "description":{"type":"string"},
                "role":{"type":"string","enum":["engineer","researcher"]},
                "isolate":{"type":"boolean"}
            }}),
        });
        v.push(ToolDef {
            name: "spawn_subagent",
            description: "Delegate a self-contained sub-task to a fresh agent with its own context window. Set isolate=true to run it in an isolated git worktree. Returns the subagent's summary.",
            schema: json!({"type":"object","properties":{
                "task":{"type":"string"},
                "role":{"type":"string","enum":["engineer","researcher"]},
                "isolate":{"type":"boolean"}
            },"required":["task"]}),
        });
    }
    v
}

// Models reporting at most this many context tokens get the compact tool set.
// Small open-weight models degrade sharply with large tool catalogs, so
// everything in local-model range (≤32k) gets the trimmed surface; remote
// frontier providers report much larger contexts and keep the full set.
const COMPACT_TOOLS_MAX_CONTEXT: usize = 32_768;

pub fn defs_for_context(include_subagent: bool, context_tokens: usize) -> Vec<ToolDef> {
    let all = defs(include_subagent);
    if context_tokens > COMPACT_TOOLS_MAX_CONTEXT {
        return all;
    }
    all.into_iter().filter(|d| compact_tool(d.name)).collect()
}

// The compact surface advertises one canonical tool per capability — read,
// write, edit, shell, glob, grep, list, artifact publishing, plus the finish
// control tool and python_tool for local extensions. The `question` tool is
// deliberately omitted: small models use it to stall (endlessly re-asking a
// clarifying question instead of building), so on the compact surface they must
// act on sensible defaults. Pure aliases (`read`/`write`/`edit`/`bash`,
// `publish_artifact`) still dispatch in `run` if a model insists, but
// advertising duplicates wastes prompt tokens and confuses weak models. Keep
// this list at 12 defs or fewer.
fn compact_tool(name: &str) -> bool {
    matches!(
        name,
        "read_file"
            | "write_file"
            | "edit_file"
            | "run_command"
            | "find_files"
            | "grep_files"
            | "list_dir"
            | "finish"
            | "Artifact"
            | "python_tool"
    )
}

pub fn defs_readonly() -> Vec<ToolDef> {
    defs(false)
        .into_iter()
        .filter(|d| {
            matches!(
                d.name,
                "bash"
                    | "read"
                    | "glob"
                    | "grep"
                    | "list"
                    | "webfetch"
                    | "websearch"
                    | "todoread"
                    | "skill"
                    | "question"
                    | "AskUserQuestion"
                    | "exit_plan"
                    | "ExitPlanMode"
                    | "read_file"
                    | "read_many_files"
                    | "list_dir"
                    | "list_tree"
                    | "file_info"
                    | "find_paths"
                    | "find_files"
                    | "grep_files"
                    | "run_command"
                    | "todo_read"
                    | "fetch_url"
                    | "web_search"
                    | "headless_browser"
                    | "list_servers"
                    | "read_server_log"
                    | "wait_for_url"
                    | "list_skills"
                    | "load_skill"
                    | "kb_query"
                    | "rule_check"
                    | "verify"
            )
        })
        .collect()
}

// Mutating tools pass through the permission gate; reads never do.
pub fn is_mutating(name: &str) -> bool {
    matches!(
        name,
        "write"
            | "write_file"
            | "edit"
            | "edit_file"
            | "multi_edit"
            | "patch"
            | "apply_patch"
            | "create_dir"
            | "move_path"
            | "remove_path"
            | "bash"
            | "run_command"
            | "start_server"
            | "stop_server"
            | "open_browser"
            | "python_tool"
            | "task"
            | "spawn_subagent"
            | "todowrite"
            | "todo_write"
            | "create_docx"
            | "Artifact"
            | "publish_artifact"
            | "kb_record"
            | "mcp_call"
    )
}

pub fn is_mutating_call(name: &str, input: &Value) -> bool {
    if matches!(
        name,
        "str_replace_editor" | "text_editor_20241022" | "text_editor_20250124"
    ) {
        let cmd = input["command"].as_str().unwrap_or("view");
        cmd != "view"
    } else {
        is_mutating(name)
    }
}

// Commands that are unambiguously read-only (grep, find, cat, etc.) — allowed
// even in ReadOnly permission mode despite run_command being generically mutating.
pub fn is_readonly_command(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    // Shell composition, redirection, and substitution can smuggle a mutating
    // tail behind a read-only first token (`cat x; rm -rf ~`), so any
    // metacharacter disqualifies the fast path. `|` also covers `||` and `&`
    // covers `&&`.
    if trimmed
        .chars()
        .any(|c| matches!(c, ';' | '|' | '&' | '>' | '<' | '`'))
        || trimmed.contains("$(")
    {
        return false;
    }
    let lower = trimmed.to_lowercase();
    let mut words = lower.split_whitespace();
    let first = words.next().unwrap_or("");
    let base = first.rsplit('/').next().unwrap_or(first);
    match base {
        // `find -delete`/`-exec` mutate; plain lookups are reads. Note that
        // `sed` is deliberately absent (sed -i edits in place), as are `xargs`
        // and `tee` (they exist to run/write things).
        "find" => !words.any(|w| matches!(w, "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir")),
        // Only known-readonly git subcommands; `git clean`, `git tag <name>`,
        // and branch delete/rename mutate.
        "git" => match words.next().unwrap_or("") {
            "status" | "log" | "diff" | "show" | "blame" => true,
            "remote" => words.all(|w| w == "-v" || w == "--verbose"),
            "branch" => !words.any(|w| {
                matches!(
                    w,
                    "-d" | "-D" | "-m" | "-M" | "--delete" | "--move" | "--force"
                )
            }),
            _ => false,
        },
        "grep" | "egrep" | "fgrep" | "rg" | "cat" | "ls" | "head" | "tail" | "wc" | "sort"
        | "uniq" | "diff" | "tree" | "stat" | "file" | "jq" => true,
        _ => false,
    }
}

// A one-line, human-readable preview of what a call will do (shown at the gate).
// One transcript line max: newlines/runs of whitespace collapse to single
// spaces and long arguments get an ellipsis, so a multi-line heredoc or a
// 300-char one-liner can't smear across the transcript (or the permission
// prompt, which reuses this).
const PREVIEW_MAX_CHARS: usize = 80;

fn clamp_preview(s: &str) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= PREVIEW_MAX_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(PREVIEW_MAX_CHARS - 1).collect();
    format!("{cut}…")
}

pub fn preview(name: &str, input: &Value) -> String {
    clamp_preview(&raw_preview(name, input))
}

fn raw_preview(name: &str, input: &Value) -> String {
    match name {
        "write" | "write_file" => format!("write {}", path_arg(input).unwrap_or("?")),
        "edit" | "edit_file" => format!("edit {}", path_arg(input).unwrap_or("?")),
        "multi_edit" => format!("multi-edit {}", input["path"].as_str().unwrap_or("?")),
        "patch" | "apply_patch" => "apply patch".to_string(),
        "create_dir" => format!("mkdir {}", input["path"].as_str().unwrap_or("?")),
        "move_path" => format!(
            "move {} -> {}",
            input["from"].as_str().unwrap_or("?"),
            input["to"].as_str().unwrap_or("?")
        ),
        "remove_path" => format!("remove {}", input["path"].as_str().unwrap_or("?")),
        "create_docx" => format!("create docx {}", input["path"].as_str().unwrap_or("?")),
        "bash" | "run_command" => format!("run: {}", input["command"].as_str().unwrap_or("?")),
        "glob" | "find_paths" => {
            format!("find paths: {}", input["pattern"].as_str().unwrap_or("?"))
        }
        "find_files" => format!("find files: {}", input["pattern"].as_str().unwrap_or("?")),
        "grep" | "grep_files" => format!("grep: {}", input["pattern"].as_str().unwrap_or("?")),
        "task" | "spawn_subagent" => {
            format!("subagent: {}", task_arg(input).unwrap_or("?"))
        }
        "exit_plan" | "ExitPlanMode" => "exit plan mode".to_string(),
        "webfetch" | "fetch_url" => format!("GET {}", input["url"].as_str().unwrap_or("?")),
        "websearch" | "web_search" => {
            format!("web search: {}", input["query"].as_str().unwrap_or("?"))
        }
        "start_server" => format!("start server: {}", input["command"].as_str().unwrap_or("?")),
        "list_servers" => "list servers".to_string(),
        "stop_server" => format!("stop server: {}", input["name"].as_str().unwrap_or("?")),
        "read_server_log" => format!("server log: {}", input["name"].as_str().unwrap_or("?")),
        "wait_for_url" => format!("wait for URL: {}", input["url"].as_str().unwrap_or("?")),
        "open_browser" => format!(
            "open browser: {}",
            input["url"]
                .as_str()
                .or_else(|| input["path"].as_str())
                .unwrap_or("?")
        ),
        "list_skills" => "list skills".to_string(),
        "skill" | "load_skill" => {
            format!("load skill: {}", input["name"].as_str().unwrap_or("?"))
        }
        "kb_query" => format!(
            "query knowledge base: {}",
            input["query"].as_str().unwrap_or("?")
        ),
        "kb_record" => format!(
            "record knowledge base entity: {}",
            input["name"].as_str().unwrap_or("?")
        ),
        "rule_check" => "evaluate engineering rules".to_string(),
        "verify" => format!(
            "verify task: {}",
            input["task_description"].as_str().unwrap_or("?")
        ),
        "mcp_call" => format!(
            "MCP call: {}/{}",
            input["server"].as_str().unwrap_or("?"),
            input["tool"].as_str().unwrap_or("?")
        ),
        "python_tool" => format!("python tool: {}", input["path"].as_str().unwrap_or("?")),
        "str_replace_editor" | "text_editor_20241022" | "text_editor_20250124" => {
            let cmd = input["command"].as_str().unwrap_or("view");
            format!("editor ({}) {}", cmd, path_arg(input).unwrap_or("?"))
        }
        "Artifact" | "publish_artifact" => {
            format!(
                "publish artifact: {}",
                input["title"].as_str().unwrap_or("untitled")
            )
        }
        _ => name.to_string(),
    }
}

fn path_arg(input: &Value) -> Option<&str> {
    let s = input["path"]
        .as_str()
        .or_else(|| input["filePath"].as_str())
        .or_else(|| input["file"].as_str())
        .or_else(|| input["target_file"].as_str())
        .or_else(|| input["targetFile"].as_str())
        .or_else(|| input["path_to_file"].as_str())?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn task_arg(input: &Value) -> Option<&str> {
    input["task"]
        .as_str()
        .or_else(|| input["description"].as_str())
}

fn resolve(cwd: &Path, p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    if p == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

// Marker placed on a tool call whose JSON arguments failed to parse, so the
// agent can feed the model a clear error instead of running with empty fields.
pub const INVALID_ARGS: &str = "__bwn_invalid_args__";

// The filesystem path a call touches (for confinement checks), resolved against
// cwd. None for non-path tools.
pub fn touched_path(name: &str, input: &Value, cwd: &Path) -> Option<PathBuf> {
    match name {
        "read"
        | "read_file"
        | "list"
        | "list_dir"
        | "list_tree"
        | "file_info"
        | "write"
        | "write_file"
        | "edit"
        | "edit_file"
        | "multi_edit"
        | "create_dir"
        | "remove_path"
        | "create_docx"
        | "python_tool"
        | "str_replace_editor"
        | "text_editor_20241022"
        | "text_editor_20250124" => Some(resolve(cwd, path_arg(input).unwrap_or(""))),
        "move_path" => Some(resolve(cwd, input["from"].as_str().unwrap_or(""))),
        "glob" | "find_paths" | "find_files" | "grep" | "grep_files" => {
            Some(resolve(cwd, root_arg(input)))
        }
        _ => None,
    }
}

pub fn command_arg_for<'a>(name: &str, input: &'a Value) -> Option<&'a str> {
    if matches!(name, "bash" | "run_command") {
        input["command"].as_str()
    } else {
        None
    }
}

pub fn read_tracking_path(name: &str, input: &Value, cwd: &Path) -> Option<PathBuf> {
    match name {
        "read" | "read_file" => path_arg(input).map(|p| resolve(cwd, p)),
        "str_replace_editor" | "text_editor_20241022" | "text_editor_20250124"
            if input["command"].as_str().unwrap_or("view") == "view" =>
        {
            path_arg(input).map(|p| resolve(cwd, p))
        }
        _ => None,
    }
}

pub fn edit_tracking_path(name: &str, input: &Value, cwd: &Path) -> Option<PathBuf> {
    match name {
        "write" | "write_file" | "edit" | "edit_file" | "multi_edit" => {
            path_arg(input).map(|p| resolve(cwd, p))
        }
        "str_replace_editor" | "text_editor_20241022" | "text_editor_20250124"
            if input["command"].as_str().unwrap_or("view") != "view" =>
        {
            path_arg(input).map(|p| resolve(cwd, p))
        }
        _ => None,
    }
}

fn root_arg(input: &Value) -> &str {
    let s = input["root"]
        .as_str()
        .or_else(|| input["path"].as_str())
        .or_else(|| input["dir"].as_str())
        .or_else(|| input["directory"].as_str())
        .unwrap_or(".")
        .trim();
    if s.is_empty() {
        "."
    } else {
        s
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn safe_name(name: &str) -> String {
    let mut out = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    if out.is_empty() {
        "server".to_string()
    } else {
        out
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

fn servers_dir() -> PathBuf {
    let dir = crate::config::home().join("servers");
    let _ = fs::create_dir_all(&dir);
    dir
}

fn server_logs_dir() -> PathBuf {
    let dir = crate::config::home().join("server-logs");
    let _ = fs::create_dir_all(&dir);
    dir
}

fn server_record_path(name: &str) -> PathBuf {
    servers_dir().join(format!("{}.json", safe_name(name)))
}

fn command_available(command: &str) -> bool {
    Command::new(command)
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn tmux_running(session: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", session])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn pid_running(pid: u64) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn server_is_running(record: &Value) -> bool {
    if let Some(session) = record["tmux_session"].as_str() {
        return tmux_running(session);
    }
    record["pid"].as_u64().map(pid_running).unwrap_or(false)
}

fn read_server_record(name: &str) -> Result<Value, String> {
    let path = server_record_path(name);
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("cannot read server record {}: {e}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|e| format!("invalid server record {}: {e}", path.display()))
}

fn write_server_record(name: &str, record: &Value) -> Result<(), String> {
    let path = server_record_path(name);
    let text = serde_json::to_string_pretty(record).map_err(|e| e.to_string())?;
    fs::write(&path, text)
        .map_err(|e| format!("cannot write server record {}: {e}", path.display()))
}

fn start_server(input: &Value, cwd: &Path) -> Outcome {
    let command = input["command"].as_str().unwrap_or("").trim();
    if command.is_empty() {
        return err("command is required");
    }
    let requested_name = input["name"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::trim)
        .unwrap_or_else(|| {
            if input["port"].as_u64().is_some() {
                "dev-server"
            } else {
                "server"
            }
        });
    let base_name = safe_name(requested_name);
    let name = if server_record_path(&base_name).exists() {
        format!("{}-{}", base_name, now_ms())
    } else {
        base_name
    };
    let run_cwd = input["cwd"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(|p| resolve(cwd, p))
        .unwrap_or_else(|| cwd.to_path_buf());
    let log_path = server_logs_dir().join(format!("{name}.log"));
    let port = input["port"].as_u64();

    let mut record = json!({
        "name": name,
        "command": command,
        "cwd": run_cwd.to_string_lossy(),
        "log": log_path.to_string_lossy(),
        "port": port,
        "started_ms": now_ms(),
    });

    if command_available("tmux") {
        let session = format!("bwn-{}", safe_name(&name));
        let shell = format!(
            "cd {} && {} 2>&1 | tee {}",
            shell_quote(&run_cwd.to_string_lossy()),
            command,
            shell_quote(&log_path.to_string_lossy())
        );
        match Command::new("tmux")
            .args(["new-session", "-d", "-s", &session])
            .arg(shell)
            .output()
        {
            Ok(out) if out.status.success() => {
                record["tmux_session"] = json!(session);
                if let Err(e) =
                    write_server_record(record["name"].as_str().unwrap_or("server"), &record)
                {
                    return err(e);
                }
                ok(format!(
                    "server '{}' started in tmux session '{}'\nlog: {}\n{}",
                    record["name"].as_str().unwrap_or("server"),
                    record["tmux_session"].as_str().unwrap_or(""),
                    log_path.display(),
                    port.map(|p| format!("url: http://127.0.0.1:{p}"))
                        .unwrap_or_default()
                ))
            }
            Ok(out) => err(format!(
                "tmux failed to start server: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )),
            Err(e) => err(format!("cannot start tmux: {e}")),
        }
    } else {
        let log = match fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            Ok(f) => f,
            Err(e) => {
                return err(format!(
                    "cannot open server log {}: {e}",
                    log_path.display()
                ))
            }
        };
        let err_log = match log.try_clone() {
            Ok(f) => f,
            Err(e) => return err(format!("cannot clone server log handle: {e}")),
        };
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", command]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-lc", command]);
            c
        };
        match cmd
            .current_dir(&run_cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err_log))
            .spawn()
        {
            Ok(child) => {
                record["pid"] = json!(child.id());
                if let Err(e) =
                    write_server_record(record["name"].as_str().unwrap_or("server"), &record)
                {
                    return err(e);
                }
                ok(format!(
                    "server '{}' started with pid {}\nlog: {}\n{}",
                    record["name"].as_str().unwrap_or("server"),
                    child.id(),
                    log_path.display(),
                    port.map(|p| format!("url: http://127.0.0.1:{p}"))
                        .unwrap_or_default()
                ))
            }
            Err(e) => err(format!("cannot start server: {e}")),
        }
    }
}

fn list_servers() -> Outcome {
    let mut rows = Vec::new();
    if let Ok(rd) = fs::read_dir(servers_dir()) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(record) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            rows.push(json!({
                "name": record["name"],
                "running": server_is_running(&record),
                "port": record["port"],
                "cwd": record["cwd"],
                "command": record["command"],
                "log": record["log"],
                "tmux_session": record["tmux_session"],
                "pid": record["pid"],
            }));
        }
    }
    ok(serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_string()))
}

fn stop_server(input: &Value) -> Outcome {
    let name = input["name"].as_str().unwrap_or("").trim();
    if name.is_empty() {
        return err("name is required");
    }
    let record = match read_server_record(name) {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let mut stopped = false;
    if let Some(session) = record["tmux_session"].as_str() {
        stopped = Command::new("tmux")
            .args(["kill-session", "-t", session])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    } else if let Some(pid) = record["pid"].as_u64() {
        #[cfg(unix)]
        {
            stopped = Command::new("kill")
                .arg(pid.to_string())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
        }
    }
    let _ = fs::remove_file(server_record_path(name));
    if stopped {
        ok(format!("server '{name}' stopped"))
    } else {
        ok(format!(
            "server '{name}' record removed; process was not running or could not be confirmed"
        ))
    }
}

fn read_server_log(input: &Value) -> Outcome {
    let name = input["name"].as_str().unwrap_or("").trim();
    if name.is_empty() {
        return err("name is required");
    }
    let record = match read_server_record(name) {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let Some(log) = record["log"].as_str() else {
        return err("server record has no log path");
    };
    let lines = input["lines"].as_u64().unwrap_or(120).clamp(1, 500) as usize;
    match fs::read_to_string(log) {
        Ok(text) => {
            let tail = text.lines().rev().take(lines).collect::<Vec<_>>();
            ok(tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
        }
        Err(e) => err(format!("cannot read server log {log}: {e}")),
    }
}

fn wait_for_url(input: &Value) -> Outcome {
    let url = input["url"].as_str().unwrap_or("").trim();
    if url.is_empty() {
        return err("url is required");
    }
    let timeout = input["timeout_seconds"]
        .as_u64()
        .unwrap_or(15)
        .clamp(1, 120);
    let expect_status = input["expect_status"].as_u64().map(|s| s as u16);
    let expect_text = input["expect_text"].as_str().filter(|s| !s.is_empty());
    let started = std::time::Instant::now();
    let deadline = started + Duration::from_secs(timeout);

    loop {
        let last_error = match ureq::get(url)
            .set("User-Agent", "buildwithnexus/1.0")
            .call()
        {
            Ok(resp) => {
                let status = resp.status();
                let body = resp.into_string().unwrap_or_default();
                let status_ok = expect_status
                    .map(|expected| status == expected)
                    .unwrap_or((200..300).contains(&status));
                let text_ok = expect_text
                    .map(|needle| body.contains(needle))
                    .unwrap_or(true);
                if status_ok && text_ok {
                    return ok(json!({
                        "url": url,
                        "status": status,
                        "elapsed_ms": started.elapsed().as_millis(),
                        "matched_text": expect_text.is_some(),
                        "response_excerpt": truncate(body, 1000),
                    })
                    .to_string());
                }
                format!(
                    "status {status}{}",
                    expect_text
                        .filter(|needle| !body.contains(*needle))
                        .map(|needle| format!(", missing expected text '{needle}'"))
                        .unwrap_or_default()
                )
            }
            Err(ureq::Error::Status(status, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                let status_ok = expect_status
                    .map(|expected| status == expected)
                    .unwrap_or(false);
                let text_ok = expect_text
                    .map(|needle| body.contains(needle))
                    .unwrap_or(true);
                if status_ok && text_ok {
                    return ok(json!({
                        "url": url,
                        "status": status,
                        "elapsed_ms": started.elapsed().as_millis(),
                        "matched_text": expect_text.is_some(),
                        "response_excerpt": truncate(body, 1000),
                    })
                    .to_string());
                }
                format!("HTTP {status}: {}", truncate(body, 300))
            }
            Err(e) => e.to_string(),
        };

        if std::time::Instant::now() >= deadline {
            return err(format!(
                "timed out after {timeout}s waiting for {url}: {last_error}"
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn open_browser(input: &Value, cwd: &Path) -> Outcome {
    let target = if let Some(url) = input["url"].as_str().filter(|s| !s.trim().is_empty()) {
        url.trim().to_string()
    } else if let Some(path) = input["path"].as_str().filter(|s| !s.trim().is_empty()) {
        resolve(cwd, path).to_string_lossy().into_owned()
    } else {
        return err("url or path is required");
    };
    let status = if cfg!(target_os = "macos") {
        Command::new("open").arg(&target).status()
    } else if cfg!(windows) {
        Command::new("cmd")
            .args(["/C", "start", "", &target])
            .status()
    } else {
        Command::new("xdg-open").arg(&target).status()
    };
    match status {
        Ok(s) if s.success() => ok(format!("opened in browser: {target}")),
        Ok(s) => err(format!("browser opener exited with status {s}: {target}")),
        Err(e) => err(format!("cannot open browser for {target}: {e}")),
    }
}

// Comment/line-anchored placeholder patterns. Bare `...`/`todo` are legal
// content (JS spread syntax, "Loading..." strings, todo apps) and must not
// trip this — only explicit stub markers count.
const PLACEHOLDER_MARKERS: [&str; 7] = [
    "// todo",
    "/* todo",
    "<!-- todo",
    "// placeholder",
    "your code here",
    "rest of the code",
    "code goes here",
];

// Case-insensitive scan for the earliest placeholder marker. Uses ASCII
// lowering so byte offsets in the lowered copy align with `contents`.
fn find_placeholder(contents: &str) -> Option<(usize, &'static str)> {
    let lower = contents.to_ascii_lowercase();
    PLACEHOLDER_MARKERS
        .iter()
        .filter_map(|m| lower.find(m).map(|pos| (pos, *m)))
        .min_by_key(|(pos, _)| *pos)
}

// ~80 chars of context around an offending byte range, clamped to char
// boundaries, so rejection messages can quote exactly what tripped the check.
fn snippet_around(contents: &str, pos: usize, len: usize) -> String {
    let mut start = pos.saturating_sub(40);
    while start > 0 && !contents.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (pos + len + 40).min(contents.len());
    while end < contents.len() && !contents.is_char_boundary(end) {
        end += 1;
    }
    contents[start..end].replace(['\n', '\r'], " ")
}

// First <script src=…> value that points at a local file. External http(s),
// protocol-relative, and data: URLs are fine; a bare `app.js` will not exist
// next to the published artifact.
fn first_local_script_src(contents: &str) -> Option<String> {
    let lower = contents.to_ascii_lowercase();
    let mut at = 0;
    while let Some(rel) = lower[at..].find("<script") {
        let tag_start = at + rel;
        let tag_end = lower[tag_start..]
            .find('>')
            .map(|e| tag_start + e)
            .unwrap_or(lower.len());
        if let Some(sp) = lower[tag_start..tag_end].find("src=") {
            let after = &contents[tag_start + sp + 4..tag_end];
            let val = if let Some(rest) = after.strip_prefix('"') {
                rest.split('"').next().unwrap_or("")
            } else if let Some(rest) = after.strip_prefix('\'') {
                rest.split('\'').next().unwrap_or("")
            } else {
                after
                    .split(|c: char| c.is_whitespace() || c == '>')
                    .next()
                    .unwrap_or("")
            };
            let val = val.trim();
            let val_lower = val.to_ascii_lowercase();
            if !val.is_empty()
                && !val_lower.starts_with("http://")
                && !val_lower.starts_with("https://")
                && !val_lower.starts_with("//")
                && !val_lower.starts_with("data:")
            {
                return Some(val.to_string());
            }
        }
        at = tag_end.max(tag_start + "<script".len());
    }
    None
}

// True if `val` is a local relative path (not an absolute URL or data: URI) —
// such a reference won't exist next to a published single-file artifact.
fn is_local_asset(val: &str) -> bool {
    let v = val.trim().to_ascii_lowercase();
    !v.is_empty()
        && !v.starts_with("http://")
        && !v.starts_with("https://")
        && !v.starts_with("//")
        && !v.starts_with("data:")
}

// Extract a (possibly quoted) attribute value from the text right after `attr=`.
fn attr_after(after: &str) -> String {
    let v = if let Some(rest) = after.strip_prefix('"') {
        rest.split('"').next().unwrap_or("")
    } else if let Some(rest) = after.strip_prefix('\'') {
        rest.split('\'').next().unwrap_or("")
    } else {
        after
            .split(|c: char| c.is_whitespace() || c == '>')
            .next()
            .unwrap_or("")
    };
    v.trim().to_string()
}

// A `<link rel="stylesheet" href="local.css">` pointing at a local file that
// won't exist next to the published artifact. Returns the local href, else None.
fn first_local_stylesheet_href(contents: &str) -> Option<String> {
    let lower = contents.to_ascii_lowercase();
    let mut at = 0;
    while let Some(rel) = lower[at..].find("<link") {
        let tag_start = at + rel;
        let tag_end = lower[tag_start..]
            .find('>')
            .map(|e| tag_start + e)
            .unwrap_or(lower.len());
        let tag_lower = &lower[tag_start..tag_end];
        if tag_lower.contains("stylesheet") {
            if let Some(hp) = tag_lower.find("href=") {
                let val = attr_after(&contents[tag_start + hp + "href=".len()..tag_end]);
                if is_local_asset(&val) {
                    return Some(val);
                }
            }
        }
        at = tag_end.max(tag_start + "<link".len());
    }
    None
}

// Rejection reasons for artifact contents. Every message quotes the offending
// snippet and the exact rule, plus what to change — cheap models can't fix
// what they can't see.
fn artifact_quality_error(contents: &str, kind: &str) -> Option<String> {
    if let Some((pos, marker)) = find_placeholder(contents) {
        return Some(format!(
            "artifact contains the placeholder marker '{marker}' near: \"…{}…\" — placeholders are rejected; replace it with the real, fully implemented content and resend the complete artifact",
            snippet_around(contents, pos, marker.len())
        ));
    }
    if kind == "html" {
        let trimmed_len = contents.trim().len();
        if trimmed_len < 300 {
            return Some(format!(
                "HTML artifact is too small to be a complete runnable app: {trimmed_len} chars, but the minimum is 300. It must be ONE self-contained file: do NOT link external files (no <link rel=stylesheet> or <script src=…>); put ALL CSS inside a <style> block and ALL JavaScript — the full working logic — inside a <script> block. Resend the complete document in `contents`."
            ));
        }
        if let Some(href) = first_local_stylesheet_href(contents) {
            return Some(format!(
                "HTML artifact links a local stylesheet via <link rel=\"stylesheet\" href=\"{href}\">, which will not exist next to the published file. Move those styles into an inline <style>…</style> block so the artifact is self-contained."
            ));
        }
        if let Some(src) = first_local_script_src(contents) {
            return Some(format!(
                "HTML artifact references a local script via <script src=\"{src}\">, which will not exist next to the published file. Inline that JavaScript in a <script>…</script> block so the artifact is self-contained."
            ));
        }
    }
    None
}

// The canvas-game heuristic is advisory only: a "game" title without a
// <canvas>/requestAnimationFrame loop is suspicious but not necessarily wrong
// (text games, DOM games), so warn instead of rejecting.
fn artifact_game_warning(title: &str, contents: &str, kind: &str) -> Option<String> {
    if kind != "html" || !title.to_ascii_lowercase().contains("game") {
        return None;
    }
    let lower = contents.to_ascii_lowercase();
    let mut missing = Vec::new();
    if !lower.contains("<canvas") {
        missing.push("a <canvas> element");
    }
    if !lower.contains("requestanimationframe") {
        missing.push("a requestAnimationFrame loop");
    }
    if missing.is_empty() {
        None
    } else {
        Some(format!(
            "warning: the title mentions a game but the HTML lacks {}; verify the artifact is actually playable",
            missing.join(" and ")
        ))
    }
}

// Browser auto-open bookkeeping: each artifact name opens at most once per
// session so republishing doesn't spawn a new tab every time.
fn mark_artifact_opened(name: &str) -> bool {
    static OPENED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let set = OPENED.get_or_init(|| Mutex::new(HashSet::new()));
    set.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.to_string())
}

// Lexically fold `.`/`..` without touching the filesystem (works for paths that
// don't exist yet, e.g. write targets).
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// Canonicalize the deepest existing ancestor and re-append the remainder, so
// symlinked prefixes (macOS `/tmp` → `/private/tmp`) compare consistently even
// for paths that don't exist yet (e.g. write targets).
fn canonicalize_lenient(p: &Path) -> PathBuf {
    let normed = normalize(p);
    if let Ok(c) = normed.canonicalize() {
        return c;
    }
    let mut rest = Vec::new();
    let mut cur = normed.as_path();
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            rest.push(name.to_os_string());
        }
        if let Ok(mut out) = parent.canonicalize() {
            for name in rest.iter().rev() {
                out.push(name);
            }
            return out;
        }
        cur = parent;
    }
    normed
}

// True if the path resolves outside the working directory. Both sides go
// through the same lenient canonicalization — canonicalizing only the base
// (as this used to) made cwd=/tmp/x become /private/tmp/x on macOS while the
// target stayed /tmp/x/f, misclassifying in both directions.
pub fn escapes_cwd(p: &Path, cwd: &Path) -> bool {
    let base = canonicalize_lenient(cwd);
    !canonicalize_lenient(p).starts_with(&base)
}

// The resolved out-of-cwd path for a mutating file-tool call, if any. The
// permission gate can route this through the same confirmation flow used for
// sensitive paths; `run` also refuses such writes outright so a target outside
// the working directory is never silently written. Reads stay unrestricted.
pub fn out_of_cwd_mutation(name: &str, input: &Value, cwd: &Path) -> Option<PathBuf> {
    if !is_mutating_call(name, input) {
        return None;
    }
    let candidate = match name {
        "write"
        | "write_file"
        | "edit"
        | "edit_file"
        | "multi_edit"
        | "create_dir"
        | "remove_path"
        | "create_docx"
        | "str_replace_editor"
        | "text_editor_20241022"
        | "text_editor_20250124" => touched_path(name, input, cwd)?,
        "move_path" => {
            let from = resolve(cwd, input["from"].as_str().unwrap_or(""));
            if escapes_cwd(&from, cwd) {
                return Some(from);
            }
            resolve(cwd, input["to"].as_str().unwrap_or(""))
        }
        _ => return None,
    };
    if escapes_cwd(&candidate, cwd) {
        Some(candidate)
    } else {
        None
    }
}

// Paths that should never be read/written without explicit confirmation, even in
// auto mode — credential stores and key material are the prime exfil targets.
pub fn is_sensitive(p: &Path) -> bool {
    let s = normalize(p).to_string_lossy().to_lowercase();
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    s.contains("/.ssh/")
        || s.contains("/.buildwithnexus/")
        || s.contains("/.aws/")
        || s.contains("/.gnupg/")
        || name == ".env.keys"
        || name == ".env"
        || name.starts_with(".env.")
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || name.ends_with(".pem")
}

// ── WSL2 filesystem boundary guard ───────────────────────────────────────────

/// True when running inside WSL2 (Windows Subsystem for Linux).
pub fn is_wsl() -> bool {
    // WSL sets WSL_DISTRO_NAME in every shell launched from Windows Terminal.
    if std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some() {
        return true;
    }
    // Fallback: /proc/version contains "Microsoft" or "WSL" on WSL kernels.
    std::fs::read_to_string("/proc/version")
        .map(|v| {
            let l = v.to_lowercase();
            l.contains("microsoft") || l.contains("wsl")
        })
        .unwrap_or(false)
}

/// True when `path` is on a Windows drive mount (e.g. /mnt/c/) inside WSL2.
/// Writes to these paths cross the WSL2 → Windows boundary and always need
/// confirmation so the agent can't silently mutate the host filesystem.
pub fn is_wsl_windows_mount(path: &Path) -> bool {
    if !is_wsl() {
        return false;
    }
    use std::path::Component;
    let normed = normalize(path);
    let comps = normed.components().collect::<Vec<_>>();
    // /mnt/<single-letter>/ → Windows drive mount
    if comps.len() >= 3 {
        if let (Component::RootDir, Component::Normal(mnt), Component::Normal(drive)) =
            (&comps[0], &comps[1], &comps[2])
        {
            return mnt.to_str() == Some("mnt")
                && drive
                    .to_str()
                    .map(|d| d.len() == 1 && d.is_ascii())
                    .unwrap_or(false);
        }
    }
    false
}

/// True when a shell command string references /mnt/<drive>/ paths in WSL2.
/// Catches `cp /mnt/c/foo .`, `rm /mnt/d/bar`, etc.
pub fn command_touches_wsl_mount(cmd: &str) -> bool {
    if !is_wsl() {
        return false;
    }
    // Simple heuristic: look for /mnt/<single-letter>/ token in the command.
    cmd.split_whitespace().any(|tok| {
        tok.starts_with("/mnt/")
            && tok.len() > 6
            && tok
                .as_bytes()
                .get(5)
                .map(|b| b.is_ascii_alphabetic())
                .unwrap_or(false)
    })
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        }
    }
    out
}

// Strip HTML for search result display. Removes script/style blocks,
// HTML tags, and collapses whitespace. Keeps meaningful text content.
fn strip_html(html: &str) -> String {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len / 3);
    let mut i = 0;
    let mut in_tag = false;
    // Tracks whether we're inside a script or style block (suppresses content).
    let mut skip_depth = 0u8;

    while i < len {
        // Check for <script or <style open tags — enter suppression mode.
        if !in_tag && skip_depth == 0 && bytes[i] == b'<' {
            let rest = &bytes[i..];
            let is_skip = |tag: &[u8]| {
                rest.len() > tag.len()
                    && rest[..tag.len()].eq_ignore_ascii_case(tag)
                    && (rest
                        .get(tag.len())
                        .is_none_or(|b| !b.is_ascii_alphanumeric()))
            };
            if is_skip(b"<script") || is_skip(b"<style") {
                skip_depth = 1;
                in_tag = true;
                i += 1;
                continue;
            }
        }
        if bytes[i] == b'<' {
            if skip_depth > 0 {
                // Inside suppressed block: look for </script> or </style>.
                let rest = &bytes[i..];
                let ends =
                    |t: &[u8]| rest.len() >= t.len() && rest[..t.len()].eq_ignore_ascii_case(t);
                if ends(b"</script>") || ends(b"</style>") {
                    skip_depth = 0;
                    // Skip past the closing tag.
                    while i < len && bytes[i] != b'>' {
                        i += 1;
                    }
                    i += 1;
                    continue;
                }
            }
            in_tag = true;
            i += 1;
            continue;
        }
        if bytes[i] == b'>' {
            in_tag = false;
            if skip_depth == 0 {
                out.push(' ');
            }
            i += 1;
            continue;
        }
        if !in_tag && skip_depth == 0 {
            out.push(bytes[i] as char);
        }
        i += 1;
    }

    // Collapse whitespace and decode HTML entities (named + numeric).
    let decoded = decode_html_entities(&out);

    let mut result = String::new();
    let mut prev_ws = true;
    for c in decoded.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                result.push(' ');
            }
            prev_ws = true;
        } else {
            result.push(c);
            prev_ws = false;
        }
    }
    result.trim().to_string()
}

// One parsed web-search result.
struct SearchHit {
    title: String,
    url: String,
    snippet: String,
}

// Percent-decode a URL-encoded string (also treats `+` as space). Invalid
// escapes are left as-is. Bytes are reassembled as UTF-8 lossily.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// Resolve a single entity body (the text between `&` and `;`) to its character:
// the common named entities plus decimal (`#8217`) and hex (`#x2019`) numeric
// character references. Returns None for anything unrecognized.
fn entity_char(ent: &str) -> Option<char> {
    match ent {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        "mdash" => Some('—'),
        "ndash" => Some('–'),
        "hellip" => Some('…'),
        "rsquo" => Some('’'),
        "lsquo" => Some('‘'),
        "rdquo" => Some('”'),
        "ldquo" => Some('“'),
        _ => {
            let num = ent.strip_prefix('#')?;
            let code = match num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse::<u32>().ok()?,
            };
            char::from_u32(code)
        }
    }
}

// Decode HTML entities in a single left-to-right pass — named entities and
// decimal/hex numeric character references. Single-pass so `&amp;lt;` decodes to
// the literal `&lt;` (not `<`), and an unrecognized `&…;` is left untouched.
fn decode_html_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        // `&` and `;` are ASCII, so `semi` is a valid char boundary; the ≤12
        // bound keeps a lone `&` in prose from swallowing a distant `;`.
        if let Some(semi) = tail.find(';') {
            if semi <= 12 {
                if let Some(ch) = entity_char(&tail[1..semi]) {
                    out.push(ch);
                    rest = &tail[semi + 1..];
                    continue;
                }
            }
        }
        out.push('&');
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

// Read attribute `attr` from the tag that opens at byte index `tag_start`,
// scanning only within that tag (up to its closing `>`).
fn tag_attr(html: &str, tag_start: usize, attr: &str) -> Option<String> {
    let end = html[tag_start..].find('>').map(|e| tag_start + e)?;
    let seg = &html[tag_start..end];
    let key = format!("{attr}=");
    let after = &seg[seg.find(&key)? + key.len()..];
    let quote = after.chars().next()?;
    if quote == '"' || quote == '\'' {
        let rest = &after[1..];
        rest.find(quote).map(|c| rest[..c].to_string())
    } else {
        let endv = after.find(char::is_whitespace).unwrap_or(after.len());
        Some(after[..endv].to_string())
    }
}

// DuckDuckGo Lite wraps result links in a redirect: `//duckduckgo.com/l/?uddg=
// <percent-encoded-target>&rut=…`. Recover the real destination; fall back to
// normalizing a protocol-relative href.
fn ddg_real_url(href: &str) -> String {
    let href = href.replace("&amp;", "&");
    if let Some(p) = href.find("uddg=") {
        let rest = &href[p + "uddg=".len()..];
        let end = rest.find('&').unwrap_or(rest.len());
        return percent_decode(&rest[..end]);
    }
    if let Some(stripped) = href.strip_prefix("//") {
        return format!("https://{stripped}");
    }
    href.to_string()
}

// Parse DuckDuckGo Lite result rows into structured hits. Returns empty if the
// markup doesn't match (the caller then falls back to a stripped-text blob).
fn parse_ddg_lite(html: &str) -> Vec<SearchHit> {
    let mut titles: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while let Some(rel) = html[i..].find("<a ") {
        let tag_start = i + rel;
        let Some(gt) = html[tag_start..].find('>').map(|e| tag_start + e) else {
            break;
        };
        if html[tag_start..gt].contains("result-link") {
            let url = ddg_real_url(&tag_attr(html, tag_start, "href").unwrap_or_default());
            let after = &html[gt + 1..];
            if let Some(close) = after.to_lowercase().find("</a") {
                let title = decode_html_entities(&strip_html(&after[..close]))
                    .trim()
                    .to_string();
                if !title.is_empty() {
                    titles.push((title, url));
                }
            }
        }
        i = gt + 1;
    }

    let mut snippets: Vec<String> = Vec::new();
    let mut j = 0;
    while let Some(rel) = html[j..].find("result-snippet") {
        let pos = j + rel;
        if let Some(gt) = html[pos..].find('>') {
            let start = pos + gt + 1;
            if let Some(lt) = html[start..].find('<') {
                snippets.push(
                    decode_html_entities(&strip_html(&html[start..start + lt]))
                        .trim()
                        .to_string(),
                );
            }
        }
        j = pos + "result-snippet".len();
    }

    titles
        .into_iter()
        .enumerate()
        .map(|(k, (title, url))| SearchHit {
            title,
            url,
            snippet: snippets.get(k).cloned().unwrap_or_default(),
        })
        .collect()
}

// Render parsed hits as compact, numbered `title / url / snippet` blocks that a
// small model can read, capped at `max` results.
fn format_search_hits(hits: &[SearchHit], max: usize) -> String {
    let mut out = String::new();
    for (n, h) in hits.iter().take(max).enumerate() {
        out.push_str(&format!("{}. {}\n", n + 1, h.title));
        if !h.url.is_empty() {
            out.push_str(&format!("   {}\n", h.url));
        }
        if !h.snippet.is_empty() {
            out.push_str(&format!("   {}\n", h.snippet));
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

// Commands so destructive they require confirmation in every mode.
pub fn catastrophic(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    let nospace: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    if nospace.contains("rm-rf/")          // rm -rf of an absolute path
        || nospace.contains("rm-fr/")
        || nospace.contains(":(){:|:&};:") // fork bomb
        || nospace.contains("mkfs")
        || nospace.contains("of=/dev/")    // dd onto a device
        || nospace.contains(">/dev/sd")
        || nospace.contains(">/dev/nvme")
        || nospace.contains("chmod-r777/")
    {
        return true;
    }
    // rm with recursive+force flags (joined `-rf`/`-fr` or split `-r -f`)
    // aimed at a root-ish target: `/abs`, `~`, `~/`, `*`, `.`, `..`.
    let toks: Vec<&str> = lower.split_whitespace().collect();
    for (i, t) in toks.iter().enumerate() {
        if *t != "rm" {
            continue;
        }
        let (mut recursive, mut force) = (false, false);
        for arg in &toks[i + 1..] {
            if let Some(flags) = arg.strip_prefix('-').filter(|f| !f.starts_with('-')) {
                recursive |= flags.contains('r');
                force |= flags.contains('f');
            } else if *arg == "--recursive" {
                recursive = true;
            } else if *arg == "--force" {
                force = true;
            }
        }
        if !(recursive && force) {
            continue;
        }
        for arg in &toks[i + 1..] {
            if arg.starts_with('-') {
                continue;
            }
            if matches!(*arg, "~" | "~/" | "*" | "." | "..") || arg.starts_with('/') {
                return true;
            }
        }
    }
    false
}

fn truncate(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    // Back off to a UTF-8 char boundary — String::truncate / slicing panics if
    // `max` lands inside a multibyte glyph, which arbitrary file/command output
    // can easily produce.
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("\n…[truncated]");
    out
}

// Head+tail truncation for command-ish output: test/build failures and the
// `[exit N]` marker live at the tail, so keep both ends and cut the middle.
// File reads keep plain head truncation (see `truncate_read`).
fn truncate_head_tail(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let head_len = HEAD_KEEP.min(max / 2);
    let tail_len = max - head_len;
    let mut head_end = head_len;
    while head_end > 0 && !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len() - tail_len;
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start - head_end;
    format!(
        "{}\n…[{omitted} bytes omitted]…\n{}",
        &s[..head_end],
        &s[tail_start..]
    )
}

// Head truncation for file reads, with a marker that tells the model the total
// size and how to fetch the rest instead of a bare "[truncated]".
fn truncate_read(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let total_bytes = s.len();
    let total_lines = s.lines().count();
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let shown_lines = s[..end].lines().count();
    format!(
        "{}\n…[truncated: content is {total_bytes} bytes / {total_lines} lines total; showing the first {shown_lines} lines — pass start_line/end_line to read the rest in ranges]",
        &s[..end]
    )
}

// Cap a single output line (grep matches, listings), marking the cut so the
// model knows content is missing rather than silently absent.
fn clip_line(line: &str, max: usize) -> String {
    if line.len() <= max {
        return line.to_string();
    }
    let mut end = max;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[line truncated]", &line[..end])
}

fn line_range(input: &Value) -> Option<(usize, usize)> {
    let start = input["start_line"].as_u64().map(|n| n.max(1) as usize);
    let end = input["end_line"].as_u64().map(|n| n.max(1) as usize);
    match (start, end) {
        (Some(s), Some(e)) => Some((s, e.max(s))),
        (Some(s), None) => Some((s, usize::MAX)),
        (None, Some(e)) => Some((1, e)),
        (None, None) => None,
    }
}

fn apply_line_range(text: &str, range: Option<(usize, usize)>) -> String {
    let Some((start, end)) = range else {
        return text.to_string();
    };
    text.lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let line_no = i + 1;
            if line_no >= start && line_no <= end {
                Some(line)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn skip_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".next"
            | ".turbo"
            | ".cache"
            | "vendor"
            | "__pycache__"
    )
}

fn collect_files(root: &Path, out: &mut Vec<PathBuf>, seen: &mut usize) {
    if *seen >= MAX_SEARCH_FILES {
        return;
    }
    let Ok(rd) = fs::read_dir(root) else {
        return;
    };
    for entry in rd.filter_map(|e| e.ok()) {
        if *seen >= MAX_SEARCH_FILES {
            break;
        }
        let path = entry.path();
        if path.is_dir() {
            if !skip_dir(&path) {
                collect_files(&path, out, seen);
            }
        } else if path.is_file() {
            *seen += 1;
            out.push(path);
        }
    }
}

fn collect_paths(root: &Path, out: &mut Vec<PathBuf>, seen: &mut usize, dirs: bool, files: bool) {
    if *seen >= MAX_SEARCH_FILES {
        return;
    }
    let Ok(rd) = fs::read_dir(root) else {
        return;
    };
    for entry in rd.filter_map(|e| e.ok()) {
        if *seen >= MAX_SEARCH_FILES {
            break;
        }
        let path = entry.path();
        if path.is_dir() {
            if dirs && !skip_dir(&path) {
                *seen += 1;
                out.push(path.clone());
            }
            if !skip_dir(&path) {
                collect_paths(&path, out, seen, dirs, files);
            }
        } else if files && path.is_file() {
            *seen += 1;
            out.push(path);
        }
    }
}

fn simple_glob_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let text = text.to_lowercase();
    let pattern = pattern.as_str();
    let text = text.as_str();
    if pattern.is_empty() || pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return text.contains(pattern);
    }
    let anchored_start = !pattern.starts_with('*');
    let anchored_end = !pattern.ends_with('*');
    let parts: Vec<&str> = pattern.split('*').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return true;
    }
    let mut rest = text;
    for (idx, part) in parts.iter().enumerate() {
        let Some(pos) = rest.find(part) else {
            return false;
        };
        if idx == 0 && anchored_start && pos != 0 {
            return false;
        }
        rest = &rest[pos + part.len()..];
    }
    if anchored_end {
        if let Some(last) = parts.last() {
            return text.ends_with(last);
        }
    }
    true
}

fn display_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn collect_tree(
    root: &Path,
    cwd: &Path,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<String>,
    max_entries: usize,
) {
    if out.len() >= max_entries || depth > max_depth {
        return;
    }
    let Ok(rd) = fs::read_dir(root) else {
        return;
    };
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        if out.len() >= max_entries {
            break;
        }
        let path = entry.path();
        let suffix = if path.is_dir() { "/" } else { "" };
        out.push(format!("{}{}", display_path(&path, cwd), suffix));
        if path.is_dir() && !skip_dir(&path) {
            collect_tree(&path, cwd, depth + 1, max_depth, out, max_entries);
        }
    }
}

fn todo_store() -> &'static Mutex<Vec<(String, String)>> {
    static TODO: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();
    TODO.get_or_init(|| Mutex::new(Vec::new()))
}

fn python_tool_dirs(cwd: &Path) -> Vec<PathBuf> {
    vec![
        cwd.join(".buildwithnexus").join("tools"),
        crate::config::home().join("tools"),
    ]
}

fn find_python_tool(cwd: &Path, raw: &str) -> PathBuf {
    let direct = resolve(cwd, raw);
    if direct.exists() || raw.contains('/') || raw.contains('\\') {
        return direct;
    }
    for dir in python_tool_dirs(cwd) {
        let plain = dir.join(raw);
        if plain.exists() {
            return plain;
        }
        let py = dir.join(format!("{raw}.py"));
        if py.exists() {
            return py;
        }
    }
    direct
}

fn list_python_tools(cwd: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for dir in python_tool_dirs(cwd) {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("py") {
                out.push(display_path(&path, cwd));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn docx_paragraph(text: &str, style: Option<&str>) -> String {
    let ppr = style
        .map(|s| format!("<w:pPr><w:pStyle w:val=\"{s}\"/></w:pPr>"))
        .unwrap_or_default();
    format!("<w:p>{ppr}{}</w:p>", docx_runs(text))
}

// Emit one Word run for `text` with the active inline formatting. Empty text
// produces nothing (an all-empty paragraph is still valid OOXML).
fn docx_push_run(out: &mut String, text: &str, bold: bool, italic: bool, code: bool) {
    if text.is_empty() {
        return;
    }
    let mut rpr = String::new();
    if bold {
        rpr.push_str("<w:b/>");
    }
    if italic {
        rpr.push_str("<w:i/>");
    }
    if code {
        rpr.push_str("<w:rFonts w:ascii=\"Consolas\" w:hAnsi=\"Consolas\" w:cs=\"Consolas\"/>");
    }
    if !rpr.is_empty() {
        rpr = format!("<w:rPr>{rpr}</w:rPr>");
    }
    out.push_str("<w:r>");
    out.push_str(&rpr);
    out.push_str(&format!(
        "<w:t xml:space=\"preserve\">{}</w:t></w:r>",
        xml_escape(text)
    ));
}

// Light-markdown inline formatting → Word runs: `**bold**`, `*italic*`, and
// `` `code` `` (verbatim). Emphasis markers only toggle when they flank
// non-space text, so arithmetic like `5 * 3` and glob patterns stay literal;
// underscores are deliberately left alone so `snake_case` isn't italicized.
fn docx_runs(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = String::new();
    let mut buf = String::new();
    let (mut bold, mut italic, mut code) = (false, false, false);
    let mut i = 0;
    while i < n {
        let c = chars[i];
        // Inside a code span everything is literal until the closing backtick.
        if code {
            if c == '`' {
                docx_push_run(&mut out, &buf, bold, italic, code);
                buf.clear();
                code = false;
            } else {
                buf.push(c);
            }
            i += 1;
            continue;
        }
        match c {
            '`' => {
                docx_push_run(&mut out, &buf, bold, italic, code);
                buf.clear();
                code = true;
                i += 1;
            }
            '*' if i + 1 < n && chars[i + 1] == '*' => {
                // Opening `**` must precede non-space; closing must follow it.
                let flanks = if bold {
                    i > 0 && !chars[i - 1].is_whitespace()
                } else {
                    i + 2 < n && !chars[i + 2].is_whitespace()
                };
                if flanks {
                    docx_push_run(&mut out, &buf, bold, italic, code);
                    buf.clear();
                    bold = !bold;
                } else {
                    buf.push('*');
                    buf.push('*');
                }
                i += 2;
            }
            '*' => {
                let flanks = if italic {
                    i > 0 && !chars[i - 1].is_whitespace()
                } else {
                    i + 1 < n && !chars[i + 1].is_whitespace()
                };
                if flanks {
                    docx_push_run(&mut out, &buf, bold, italic, code);
                    buf.clear();
                    italic = !italic;
                } else {
                    buf.push('*');
                }
                i += 1;
            }
            _ => {
                buf.push(c);
                i += 1;
            }
        }
    }
    docx_push_run(&mut out, &buf, bold, italic, code);
    out
}

// Split a markdown table line into trimmed cells, dropping the outer pipes.
fn parse_table_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

// A markdown table row: trimmed, starts with `|`, and has at least two pipes.
fn is_table_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.matches('|').count() >= 2
}

// The `|---|:--:|` alignment row: every cell is only dashes/colons.
fn is_table_separator(line: &str) -> bool {
    let cells = parse_table_cells(line);
    !cells.is_empty()
        && cells
            .iter()
            .all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':'))
}

// One table cell. Header cells are a single bold run; body cells get full
// inline markdown formatting.
fn docx_table_cell(text: &str, header: bool) -> String {
    let runs = if header {
        let mut s = String::new();
        docx_push_run(&mut s, text, true, false, false);
        s
    } else {
        docx_runs(text)
    };
    format!("<w:tc><w:tcPr><w:tcW w:w=\"0\" w:type=\"auto\"/></w:tcPr><w:p>{runs}</w:p></w:tc>")
}

// A bordered Word table. The first row is treated as the (bold) header.
fn docx_table(rows: &[Vec<String>]) -> String {
    let mut out = String::from(
        "<w:tbl><w:tblPr><w:tblW w:w=\"0\" w:type=\"auto\"/><w:tblBorders>\
<w:top w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>\
<w:left w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>\
<w:bottom w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>\
<w:right w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>\
<w:insideH w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>\
<w:insideV w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>\
</w:tblBorders></w:tblPr>",
    );
    for (r, row) in rows.iter().enumerate() {
        out.push_str("<w:tr>");
        for cell in row {
            out.push_str(&docx_table_cell(cell, r == 0));
        }
        out.push_str("</w:tr>");
    }
    out.push_str("</w:tbl>");
    out
}

fn docx_document(title: &str, body: &str) -> String {
    let mut out = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
    );
    out.push_str(&docx_paragraph(title, Some("Title")));
    let lines: Vec<&str> = body.lines().collect();
    let mut idx = 0;
    while idx < lines.len() {
        // A run of consecutive `|`-delimited rows becomes one Word table; the
        // alignment separator row is dropped and the first row is the header.
        if is_table_row(lines[idx]) {
            let mut rows: Vec<Vec<String>> = Vec::new();
            while idx < lines.len() && is_table_row(lines[idx]) {
                if !is_table_separator(lines[idx]) {
                    rows.push(parse_table_cells(lines[idx]));
                }
                idx += 1;
            }
            if !rows.is_empty() {
                out.push_str(&docx_table(&rows));
            }
            continue;
        }
        let trimmed = lines[idx].trim();
        if trimmed.is_empty() {
            out.push_str("<w:p/>");
        } else if let Some(h) = trimmed.strip_prefix("### ") {
            out.push_str(&docx_paragraph(h, Some("Heading3")));
        } else if let Some(h) = trimmed.strip_prefix("## ") {
            out.push_str(&docx_paragraph(h, Some("Heading2")));
        } else if let Some(h) = trimmed.strip_prefix("# ") {
            out.push_str(&docx_paragraph(h, Some("Heading1")));
        } else if let Some(item) = trimmed.strip_prefix("- ") {
            out.push_str(&docx_paragraph(&format!("• {item}"), None));
        } else {
            out.push_str(&docx_paragraph(trimmed, None));
        }
        idx += 1;
    }
    out.push_str(r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440"/></w:sectPr></w:body></w:document>"#);
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn write_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn write_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn stored_zip(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in files {
        let offset = out.len() as u32;
        let crc = crc32(data);
        write_u32(&mut out, 0x0403_4b50);
        write_u16(&mut out, 20);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u32(&mut out, crc);
        write_u32(&mut out, data.len() as u32);
        write_u32(&mut out, data.len() as u32);
        write_u16(&mut out, name.len() as u16);
        write_u16(&mut out, 0);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        write_u32(&mut central, 0x0201_4b50);
        write_u16(&mut central, 20);
        write_u16(&mut central, 20);
        write_u16(&mut central, 0);
        write_u16(&mut central, 0);
        write_u16(&mut central, 0);
        write_u16(&mut central, 0);
        write_u32(&mut central, crc);
        write_u32(&mut central, data.len() as u32);
        write_u32(&mut central, data.len() as u32);
        write_u16(&mut central, name.len() as u16);
        write_u16(&mut central, 0);
        write_u16(&mut central, 0);
        write_u16(&mut central, 0);
        write_u16(&mut central, 0);
        write_u32(&mut central, 0);
        write_u32(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = out.len() as u32;
    out.extend_from_slice(&central);
    write_u32(&mut out, 0x0605_4b50);
    write_u16(&mut out, 0);
    write_u16(&mut out, 0);
    write_u16(&mut out, files.len() as u16);
    write_u16(&mut out, files.len() as u16);
    write_u32(&mut out, central.len() as u32);
    write_u32(&mut out, central_offset);
    write_u16(&mut out, 0);
    out
}

fn docx_bytes(title: &str, body: &str) -> Vec<u8> {
    let content_types = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/></Types>"#.to_vec();
    let rels = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#.to_vec();
    let styles = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/></w:style><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/></w:style><w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/></w:style><w:style w:type="paragraph" w:styleId="Heading3"><w:name w:val="heading 3"/></w:style></w:styles>"#.to_vec();
    stored_zip(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", rels),
        ("word/document.xml", docx_document(title, body).into_bytes()),
        ("word/styles.xml", styles),
    ])
}

fn search_limit(input: &Value) -> usize {
    input["max"]
        .as_u64()
        .map(|n| n.clamp(1, 500) as usize)
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
}

// Captured result of a spawned command run under a deadline.
struct CommandCapture {
    stdout: String,
    stderr: String,
    code: Option<i32>,
    timed_out: bool,
}

// Run a command with piped stdio and a hard deadline. `Command::output()`
// blocks forever on hanging commands; here stdout/stderr are drained on
// threads while the parent polls `try_wait`, killing the child on expiry and
// returning whatever partial output was collected.
fn run_with_timeout(
    mut cmd: Command,
    stdin_payload: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<CommandCapture, String> {
    cmd.stdin(if stdin_payload.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("failed to spawn: {e}"))?;
    if let Some(payload) = stdin_payload {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = std::io::Write::write_all(&mut stdin, &payload);
        } // dropping stdin closes it so the child sees EOF
    }
    // Reader threads forward chunks over channels instead of being joined:
    // a killed shell can leave grandchildren holding the pipe write end, and
    // joining a blocked read_to_end would hang exactly like Command::output().
    fn drain<R: std::io::Read + Send + 'static>(
        pipe: Option<R>,
    ) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = std::sync::mpsc::channel();
        if let Some(mut p) = pipe {
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match std::io::Read::read(&mut p, &mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
        rx
    }
    // Drain whatever is buffered, then wait up to `grace` for stragglers; a
    // clean EOF disconnects the channel and exits immediately.
    fn collect(rx: &std::sync::mpsc::Receiver<Vec<u8>>, out: &mut Vec<u8>, grace: Duration) {
        let deadline = std::time::Instant::now() + grace;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(chunk) => out.extend_from_slice(&chunk),
                Err(_) => break, // disconnected (EOF) or grace expired
            }
        }
    }
    let out_rx = drain(child.stdout.take());
    let err_rx = drain(child.stderr.take());
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let deadline = std::time::Instant::now() + timeout;
    let mut timed_out = false;
    let code = loop {
        while let Ok(chunk) = out_rx.try_recv() {
            stdout_bytes.extend_from_slice(&chunk);
        }
        while let Ok(chunk) = err_rx.try_recv() {
            stderr_bytes.extend_from_slice(&chunk);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("failed to wait for command: {e}")),
        }
    };
    let grace = Duration::from_millis(250);
    collect(&out_rx, &mut stdout_bytes, grace);
    collect(&err_rx, &mut stderr_bytes, grace);
    Ok(CommandCapture {
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        code,
        timed_out,
    })
}

// Format captured command output the way run_command/python_tool report it,
// including the timeout notice or `[exit N]` marker at the tail.
fn command_outcome(cap: CommandCapture, timeout: Duration) -> Outcome {
    let mut s = cap.stdout;
    if !cap.stderr.trim().is_empty() {
        s.push_str("\n[stderr]\n");
        s.push_str(&cap.stderr);
    }
    if cap.timed_out {
        s.push_str(&format!(
            "\n[error] command timed out after {}s; long-running processes should use start_server",
            timeout.as_secs()
        ));
        return Outcome {
            content: truncate_head_tail(s, MAX_OUT),
            is_error: true,
            finished: false,
        };
    }
    let code = cap.code.unwrap_or(-1);
    s.push_str(&format!("\n[exit {code}]"));
    Outcome {
        content: truncate_head_tail(s, MAX_OUT),
        is_error: code != 0,
        finished: false,
    }
}

// UTC calendar conversion (Howard Hinnant's civil-from-days) so kb_record can
// stamp real timestamps without pulling in a date dependency.
fn iso8601_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn iso8601_utc_now() -> String {
    iso8601_utc(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    )
}

// Best-effort locator for edit mismatches: collapse whitespace and look for
// the first non-empty line of `old` so the model gets a concrete line number
// to re-read instead of a bare "not found".
fn whitespace_relaxed_hit(body: &str, old: &str) -> Option<usize> {
    fn squash(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }
    let target = old.lines().map(squash).find(|l| !l.is_empty())?;
    body.lines()
        .position(|line| squash(line).contains(&target))
        .map(|i| i + 1)
}

// Hint text shared by the edit tools when `old` text is missing from the file.
fn edit_not_found_hint(body: &str, old: &str) -> String {
    match whitespace_relaxed_hit(body, old) {
        Some(line) => format!(
            "closest match at line {line} differs in indentation/whitespace — re-read the file and copy the text exactly, including whitespace"
        ),
        None => "re-read the file and copy the exact current text for the old string".to_string(),
    }
}

// ── Tiered lenient edit matching ─────────────────────────────────────────
// When the exact `old` text is missing, small models have usually re-sent the
// right lines with drifted trailing whitespace or a uniform indentation shift.
// Those two failure shapes are unambiguous to repair, so a UNIQUE
// whitespace-tolerant hit is auto-applied. Anything looser (similarity-based
// fuzzy matching) is dangerous and stays diagnostic-only via
// `edit_not_found_hint`.

// Suffix appended to edit success messages when a lenient tier applied, so the
// model learns its copy of the text had drifted.
const LENIENT_MATCH_NOTE: &str =
    "matched with whitespace tolerance — original text differed in indentation";

// Re-indent transform recorded by a lenient match so the replacement text
// lands at the file's actual indentation rather than the model's drifted copy.
enum LenientReindent {
    // Tier 1: lines matched after trailing-whitespace trim; use `new` verbatim.
    Verbatim,
    // Tier 2: every file line is `prefix` deeper than the needle; prepend
    // `prefix` to each non-blank replacement line.
    Add(String),
    // Tier 2: every needle line is `prefix` deeper than the file; strip
    // `prefix` from each replacement line that carries it.
    Strip(String),
}

// A unique lenient match: the byte range of the matched file region plus the
// re-indent transform for the replacement text.
struct LenientHit {
    start: usize,
    end: usize,
    reindent: LenientReindent,
}

enum LenientMatch {
    Hit(LenientHit),
    // Multiple candidate regions — too ambiguous to auto-apply.
    Ambiguous,
    Miss,
}

// Whole-line lenient search for `old` in `body`. Tier 1 compares lines after
// trailing-whitespace trim; tier 2 additionally allows one constant
// leading-indent delta across all non-blank lines. Each tier only produces a
// hit when its match is unique in the file.
fn lenient_find(body: &str, old: &str) -> LenientMatch {
    let needle: Vec<&str> = old.lines().collect();
    if needle.is_empty() {
        return LenientMatch::Miss;
    }
    let needle_ends_nl = old.ends_with('\n');
    // Byte offset + content (newline excluded) + whether a newline follows, so
    // a line-window hit can be mapped back to a byte splice range.
    let mut lines: Vec<(usize, &str, bool)> = Vec::new();
    let mut pos = 0usize;
    for seg in body.split_inclusive('\n') {
        let has_nl = seg.ends_with('\n');
        let content = if has_nl { &seg[..seg.len() - 1] } else { seg };
        lines.push((pos, content, has_nl));
        pos += seg.len();
    }
    if lines.len() < needle.len() {
        return LenientMatch::Miss;
    }
    let mut rstrip_hits: Vec<usize> = Vec::new();
    let mut shift_hits: Vec<(usize, LenientReindent)> = Vec::new();
    for w in 0..=lines.len() - needle.len() {
        let window = &lines[w..w + needle.len()];
        // A needle that ends in a newline must consume one in the file too.
        if needle_ends_nl && !window[needle.len() - 1].2 {
            continue;
        }
        if window
            .iter()
            .zip(&needle)
            .all(|((_, f, _), o)| f.trim_end() == o.trim_end())
        {
            rstrip_hits.push(w);
        } else if let Some(reindent) = uniform_indent_shift(window, &needle) {
            shift_hits.push((w, reindent));
        }
    }
    // Tier 1 wins outright when unique; tier 2 only applies when tier 1 found
    // nothing. Two candidates in the deciding tier keep the edit failing.
    let (w, reindent) = match (rstrip_hits.len(), shift_hits.len()) {
        (1, _) => (rstrip_hits[0], LenientReindent::Verbatim),
        (0, 1) => shift_hits.remove(0),
        (0, 0) => return LenientMatch::Miss,
        _ => return LenientMatch::Ambiguous,
    };
    let (last_start, last_content, _) = lines[w + needle.len() - 1];
    let end = last_start + last_content.len() + usize::from(needle_ends_nl);
    LenientMatch::Hit(LenientHit {
        start: lines[w].0,
        end,
        reindent,
    })
}

// Checks whether a window of file lines equals the needle after shifting every
// non-blank line by one constant leading-whitespace delta (all deeper or all
// shallower). Blank lines only match blank lines; a tab-for-space swap is not
// a uniform shift and stays diagnostic-only.
fn uniform_indent_shift(
    window: &[(usize, &str, bool)],
    needle: &[&str],
) -> Option<LenientReindent> {
    let mut shift: Option<LenientReindent> = None;
    for ((_, file_line, _), needle_line) in window.iter().zip(needle) {
        let f = file_line.trim_end();
        let o = needle_line.trim_end();
        match (f.is_empty(), o.is_empty()) {
            (true, true) => continue,
            (true, false) | (false, true) => return None,
            (false, false) => {}
        }
        match &shift {
            None => {
                // Direction and prefix come from the first non-blank pair;
                // every later non-blank pair must shift by the exact same
                // whitespace prefix.
                let (deeper, prefix) = if let Some(p) = f.strip_suffix(o) {
                    (true, p)
                } else {
                    (false, o.strip_suffix(f)?)
                };
                if prefix.is_empty() || !prefix.chars().all(char::is_whitespace) {
                    return None;
                }
                shift = Some(if deeper {
                    LenientReindent::Add(prefix.to_string())
                } else {
                    LenientReindent::Strip(prefix.to_string())
                });
            }
            Some(LenientReindent::Add(prefix)) => {
                if f.strip_prefix(prefix.as_str()) != Some(o) {
                    return None;
                }
            }
            Some(LenientReindent::Strip(prefix)) => {
                if o.strip_prefix(prefix.as_str()) != Some(f) {
                    return None;
                }
            }
            // Never stored by this function.
            Some(LenientReindent::Verbatim) => return None,
        }
    }
    shift
}

// Splices a lenient hit into the body, re-indenting replacement lines by the
// same delta the match observed so the edit lands at the file's real depth.
fn apply_lenient(body: &str, hit: &LenientHit, new: &str) -> String {
    let replacement: String = match &hit.reindent {
        LenientReindent::Verbatim => new.to_string(),
        LenientReindent::Add(prefix) => new
            .split_inclusive('\n')
            .map(|seg| {
                if seg.trim().is_empty() {
                    seg.to_string()
                } else {
                    format!("{prefix}{seg}")
                }
            })
            .collect(),
        LenientReindent::Strip(prefix) => new
            .split_inclusive('\n')
            .map(|seg| seg.strip_prefix(prefix.as_str()).unwrap_or(seg))
            .collect(),
    };
    format!("{}{}{}", &body[..hit.start], replacement, &body[hit.end..])
}

// Small edit distance for "did you mean" suggestions on unknown tool names.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

pub fn run(name: &str, input: &Value, cwd: &Path) -> Outcome {
    // Mutating file tools never silently write outside the working directory;
    // the permission gate can confirm such calls, but the execution layer
    // refuses regardless so an unwired gate cannot leak a stray write.
    if let Some(p) = out_of_cwd_mutation(name, input, cwd) {
        return err(format!(
            "refusing to write outside the working directory: {} resolves beyond {}. Out-of-cwd writes require explicit user approval — ask the user first (question tool), and once approved perform the change with run_command, or have the user restart the session in the target directory.",
            p.display(),
            cwd.display()
        ));
    }
    match name {
        "read" | "read_file" => {
            let p_str = path_arg(input).unwrap_or("").trim();
            if p_str.is_empty() {
                return err("path argument is required and cannot be empty");
            }
            let p = resolve(cwd, p_str);
            match fs::read_to_string(&p) {
                Ok(c) => ok(truncate_read(
                    apply_line_range(&c, line_range(input)),
                    MAX_READ,
                )),
                Err(e) => match e.kind() {
                    std::io::ErrorKind::InvalidData => err(format!(
                        "cannot read {}: contents are not valid UTF-8 — this looks like a binary file. Do not retry read_file; use run_command with a targeted extractor instead (e.g. `file`, `strings`, `xxd | head`, or a format-specific CLI).",
                        p.display()
                    )),
                    std::io::ErrorKind::PermissionDenied => err(format!(
                        "cannot read {}: permission denied — the file exists but this process lacks read access. Check ownership/permissions or ask the user to grant access; do not retry the same call unchanged.",
                        p.display()
                    )),
                    _ => err(format!(
                        "cannot read {}: {e}\nrecovery: do not invent another path or ask the user immediately. Use list_tree/find_paths/find_files/grep_files to locate likely files; for folders use find_paths kind=`dir`; for personal files try roots like `~`, `~/Documents`, `~/Desktop`, `~/Downloads`, `~/Projects`, and `~/repos`. If a broader search is needed, propose or call a read-only find/rg command.",
                        p.display()
                    )),
                },
            }
        }
        "read_many_files" => {
            let Some(paths) = input["paths"].as_array() else {
                return err("paths is required");
            };
            if paths.is_empty() {
                return err("paths array cannot be empty");
            }
            let max = input["max_bytes_per_file"]
                .as_u64()
                .unwrap_or(MAX_READ as u64)
                .clamp(1_000, MAX_READ as u64) as usize;
            let mut sections = Vec::new();
            for path in paths.iter().filter_map(|v| v.as_str()) {
                if path.trim().is_empty() {
                    sections.push("--- [empty path] ---\n[error] path cannot be empty".to_string());
                    continue;
                }
                let p = resolve(cwd, path);
                match fs::read_to_string(&p) {
                    Ok(c) => sections.push(format!(
                        "--- {} ---\n{}",
                        display_path(&p, cwd),
                        truncate(c, max)
                    )),
                    Err(e) => sections.push(format!("--- {} ---\n[error] {e}", p.display())),
                }
            }
            ok(truncate(sections.join("\n\n"), MAX_READ))
        }
        "list" | "list_dir" => {
            let p = resolve(cwd, path_arg(input).unwrap_or("."));
            match fs::read_dir(&p) {
                Ok(rd) => {
                    let mut names: Vec<String> = rd
                        .filter_map(|e| e.ok())
                        .map(|e| {
                            let n = e.file_name().to_string_lossy().into_owned();
                            if e.path().is_dir() {
                                format!("{n}/")
                            } else {
                                n
                            }
                        })
                        .collect();
                    names.sort();
                    ok(truncate(names.join("\n"), MAX_OUT))
                }
                Err(e) => err(format!(
                    "cannot list {}: {e}\nrecovery: do not invent another path or ask the user immediately. Use list_tree/find_paths/find_files/grep_files to locate likely files; for folders use find_paths kind=`dir`; for personal files try roots like `~`, `~/Documents`, `~/Desktop`, `~/Downloads`, `~/Projects`, and `~/repos`. If a broader search is needed, propose or call a read-only find/rg command.",
                    p.display()
                )),
            }
        }
        "list_tree" => {
            let root = resolve(cwd, path_arg(input).unwrap_or("."));
            let max_depth = input["max_depth"].as_u64().unwrap_or(3).clamp(1, 8) as usize;
            let max_entries = input["max_entries"].as_u64().unwrap_or(200).clamp(1, 1000) as usize;
            let mut entries = Vec::new();
            collect_tree(&root, cwd, 1, max_depth, &mut entries, max_entries);
            if entries.is_empty() {
                ok(format!("no entries under {}", root.display()))
            } else {
                ok(truncate(entries.join("\n"), MAX_OUT))
            }
        }
        "file_info" => {
            let p_str = path_arg(input).unwrap_or("");
            if p_str.is_empty() {
                return err("path argument is required and cannot be empty");
            }
            let p = resolve(cwd, p_str);
            match fs::metadata(&p) {
                Ok(m) => {
                    let kind = if m.is_dir() {
                        "directory"
                    } else if m.is_file() {
                        "file"
                    } else {
                        "other"
                    };
                    let modified_ms = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis());
                    ok(json!({
                        "path": display_path(&p, cwd),
                        "kind": kind,
                        "len": m.len(),
                        "readonly": m.permissions().readonly(),
                        "modified_ms": modified_ms,
                    })
                    .to_string())
                }
                Err(e) => err(format!(
                    "cannot stat {}: {e}\nrecovery: do not invent another path or ask the user immediately. Use list_tree/find_paths/find_files/grep_files to locate likely files; for folders use find_paths kind=`dir`; for personal files try roots like `~`, `~/Documents`, `~/Desktop`, `~/Downloads`, `~/Projects`, and `~/repos`. If a broader search is needed, propose or call a read-only find/rg command.",
                    p.display()
                )),
            }
        }
        "glob" | "find_paths" => {
            let root = resolve(cwd, root_arg(input));
            let pattern = input["pattern"].as_str().unwrap_or("").trim();
            if pattern.is_empty() {
                return err("pattern is required");
            }
            let kind = input["kind"].as_str().unwrap_or("any");
            let include_dirs = kind != "file";
            let include_files = kind != "dir";
            let limit = search_limit(input);
            let mut paths = Vec::new();
            let mut seen = 0;
            collect_paths(&root, &mut paths, &mut seen, include_dirs, include_files);
            paths.sort();
            let matches: Vec<String> = paths
                .into_iter()
                .filter_map(|path| {
                    let rel = display_path(&path, cwd);
                    let name_match = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|name| simple_glob_match(pattern, name));
                    if simple_glob_match(pattern, &rel) || name_match {
                        Some(rel)
                    } else {
                        None
                    }
                })
                .take(limit)
                .collect();
            if matches.is_empty() {
                ok(format!("no paths matched {pattern}"))
            } else {
                ok(matches.join("\n"))
            }
        }
        "find_files" => {
            let root = resolve(cwd, root_arg(input));
            let pattern = input["pattern"].as_str().unwrap_or("").trim();
            if pattern.is_empty() {
                return err("pattern is required");
            }
            let limit = search_limit(input);
            let mut files = Vec::new();
            let mut seen = 0;
            collect_files(&root, &mut files, &mut seen);
            files.sort();
            let matches: Vec<String> = files
                .into_iter()
                .filter_map(|path| {
                    let rel = display_path(&path, cwd);
                    let name_match = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|name| simple_glob_match(pattern, name));
                    if simple_glob_match(pattern, &rel) || name_match {
                        Some(rel)
                    } else {
                        None
                    }
                })
                .take(limit)
                .collect();
            if matches.is_empty() {
                ok(format!("no files matched {pattern}"))
            } else {
                ok(matches.join("\n"))
            }
        }
        "grep" | "grep_files" => {
            let root = resolve(cwd, root_arg(input));
            let pattern = input["pattern"].as_str().unwrap_or("");
            if pattern.is_empty() {
                return err("pattern is required");
            }
            let file_pattern = input["file_pattern"]
                .as_str()
                .or_else(|| input["include"].as_str())
                .unwrap_or("*");
            let case_sensitive = input["case_sensitive"].as_bool().unwrap_or(false);
            let needle = if case_sensitive {
                pattern.to_string()
            } else {
                pattern.to_lowercase()
            };
            let limit = search_limit(input);
            let mut files = Vec::new();
            let mut seen = 0;
            collect_files(&root, &mut files, &mut seen);
            files.sort();
            let mut matches = Vec::new();
            let mut total = 0usize;
            for path in files {
                let rel = display_path(&path, cwd);
                let basename_matches = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| simple_glob_match(file_pattern, name));
                if !simple_glob_match(file_pattern, &rel) && !basename_matches {
                    continue;
                }
                let Ok(text) = fs::read_to_string(&path) else {
                    continue;
                };
                for (idx, line) in text.lines().enumerate() {
                    let haystack = if case_sensitive {
                        line.to_string()
                    } else {
                        line.to_lowercase()
                    };
                    if haystack.contains(&needle) {
                        // Keep counting past the limit so the cap header can
                        // report how much was left out.
                        total += 1;
                        if matches.len() < limit {
                            matches.push(format!(
                                "{}:{}:{}",
                                rel,
                                idx + 1,
                                clip_line(line.trim_end(), MAX_MATCH_LINE)
                            ));
                        }
                    }
                }
            }
            if matches.is_empty() {
                ok(format!("no matches for {pattern}"))
            } else {
                let shown = matches.len();
                let body = matches.join("\n");
                let out = if total > shown {
                    format!("{total} matches, showing {shown} (raise `max` or narrow the pattern)\n{body}")
                } else {
                    body
                };
                ok(truncate(out, MAX_OUT))
            }
        }
        "write" | "write_file" => {
            let p_str = path_arg(input).unwrap_or("").trim();
            if p_str.is_empty() {
                return err("path argument is required and cannot be empty");
            }
            let p = resolve(cwd, p_str);
            // Distinguish an absent `content` key from an explicit empty
            // string — a dropped key must not silently produce an empty file.
            let Some(content) = input["content"].as_str() else {
                return err(
                    "missing required param `content` — the file was NOT written; re-send the full write call including the complete `content` string (use an explicit empty string to create an empty file)",
                );
            };
            let overwrite_len = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            if content.is_empty() && overwrite_len > 0 {
                return err(format!(
                    "refusing to overwrite non-empty file {} ({overwrite_len} bytes) with empty content — if you really intend to empty it, remove it with remove_path first or write the intended replacement content",
                    p.display()
                ));
            }
            if let Some(dir) = p.parent() {
                let _ = fs::create_dir_all(dir);
            }
            checkpoint::record(cwd, &p, "write_file");
            let prior = fs::read_to_string(&p).unwrap_or_default();
            match fs::write(&p, content) {
                Ok(_) => {
                    crate::report::diff(&p.display().to_string(), &prior, content);
                    ok(format!("wrote {} ({} bytes)", p.display(), content.len()))
                }
                Err(e) => err(format!("cannot write {}: {e}", p.display())),
            }
        }
        "edit" | "edit_file" => {
            let p_str = path_arg(input).unwrap_or("").trim();
            if p_str.is_empty() {
                return err("path argument is required and cannot be empty");
            }
            let p = resolve(cwd, p_str);
            let old = input["old"]
                .as_str()
                .or_else(|| input["oldString"].as_str())
                .unwrap_or("");
            let new = input["new"]
                .as_str()
                .or_else(|| input["newString"].as_str())
                .unwrap_or("");
            let body = match fs::read_to_string(&p) {
                Ok(b) => b,
                Err(e) => return err(format!("cannot read {}: {e}", p.display())),
            };
            if old.is_empty() {
                return err("`old` text cannot be empty");
            }
            // One pass to locate, a counting pass over the remainder for the
            // occurrence total — still avoids a separate full replace scan.
            let Some(first) = body.find(old) else {
                // Tiered lenient fallback: a unique whitespace-tolerant hit is
                // auto-applied; ambiguous or absent text keeps the diagnostic.
                if let LenientMatch::Hit(hit) = lenient_find(&body, old) {
                    checkpoint::record(cwd, &p, "edit_file");
                    let updated = apply_lenient(&body, &hit, new);
                    return match fs::write(&p, &updated) {
                        Ok(_) => {
                            crate::report::diff(&p.display().to_string(), &body, &updated);
                            ok(format!("edited {} ({LENIENT_MATCH_NOTE})", p.display()))
                        }
                        Err(e) => err(format!("cannot write {}: {e}", p.display())),
                    };
                }
                return err(format!(
                    "`old` text not found in {}. {}.",
                    p.display(),
                    edit_not_found_hint(&body, old)
                ));
            };
            let extra = body[first + old.len()..].matches(old).count();
            if extra > 0 {
                return err(format!(
                    "`old` text is not unique — it appears {} times in {}; add surrounding context so it matches exactly once",
                    extra + 1,
                    p.display()
                ));
            }
            checkpoint::record(cwd, &p, "edit_file");
            let updated = body.replacen(old, new, 1);
            match fs::write(&p, &updated) {
                Ok(_) => {
                    crate::report::diff(&p.display().to_string(), &body, &updated);
                    ok(format!("edited {}", p.display()))
                }
                Err(e) => err(format!("cannot write {}: {e}", p.display())),
            }
        }
        "multi_edit" => {
            let p_str = path_arg(input).unwrap_or("");
            if p_str.is_empty() {
                return err("path argument is required and cannot be empty");
            }
            let p = resolve(cwd, p_str);
            let Some(edits) = input["edits"].as_array() else {
                return err("edits is required");
            };
            if edits.is_empty() {
                return err("edits array cannot be empty");
            }
            let mut body = match fs::read_to_string(&p) {
                Ok(b) => b,
                Err(e) => return err(format!("cannot read {}: {e}", p.display())),
            };
            let prior = body.clone();
            let n = edits.len();
            let mut lenient_applied = 0usize;
            for (i, edit) in edits.iter().enumerate() {
                let which = format!("edit #{} of {n}", i + 1);
                let old = edit["old"].as_str().unwrap_or("");
                if old.is_empty() {
                    return err(format!(
                        "{which}: old text cannot be empty (no edits were written)"
                    ));
                }
                let new = edit["new"].as_str().unwrap_or("");
                let count = body.matches(old).count();
                if count == 0 {
                    // Tiered lenient fallback: a unique whitespace-tolerant
                    // hit is auto-applied; ambiguous or absent text keeps the
                    // diagnostic and aborts before anything is written.
                    if let LenientMatch::Hit(hit) = lenient_find(&body, old) {
                        body = apply_lenient(&body, &hit, new);
                        lenient_applied += 1;
                        continue;
                    }
                    return err(format!(
                        "{which} failed: old text not found. Note that earlier edits in this batch had already changed the content later edits are matched against. {}. No edits were written.",
                        edit_not_found_hint(&body, old)
                    ));
                }
                if count > 1 {
                    return err(format!(
                        "{which} failed: old text appears {count} times — add surrounding context so it matches exactly once. No edits were written."
                    ));
                }
                body = body.replacen(old, new, 1);
            }
            checkpoint::record(cwd, &p, "multi_edit");
            let note = if lenient_applied > 0 {
                format!(" ({lenient_applied} {LENIENT_MATCH_NOTE})")
            } else {
                String::new()
            };
            match fs::write(&p, &body) {
                Ok(_) => {
                    crate::report::diff(&p.display().to_string(), &prior, &body);
                    ok(format!(
                        "edited {} with {} replacements{note}",
                        p.display(),
                        edits.len()
                    ))
                }
                Err(e) => err(format!("cannot write {}: {e}", p.display())),
            }
        }
        "patch" | "apply_patch" => {
            let patch = input["patch"].as_str().unwrap_or("");
            if patch.trim().is_empty() {
                return err("patch is required");
            }
            let mut child = match Command::new("git")
                .args(["apply", "--whitespace=nowarn", "-"])
                .current_dir(cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => return err(format!("failed to spawn git apply: {e}")),
            };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = std::io::Write::write_all(&mut stdin, patch.as_bytes());
            }
            match child.wait_with_output() {
                Ok(o) if o.status.success() => ok("patch applied"),
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    err(format!("patch failed: {stderr}"))
                }
                Err(e) => err(format!("failed to wait for git apply: {e}")),
            }
        }
        "create_dir" => {
            let p_str = path_arg(input).unwrap_or("");
            if p_str.is_empty() {
                return err("path argument is required and cannot be empty");
            }
            let p = resolve(cwd, p_str);
            match fs::create_dir_all(&p) {
                Ok(_) => ok(format!("created {}", p.display())),
                Err(e) => err(format!("cannot create {}: {e}", p.display())),
            }
        }
        "move_path" => {
            let from_str = input["from"].as_str().unwrap_or("").trim();
            let to_str = input["to"].as_str().unwrap_or("").trim();
            if from_str.is_empty() || to_str.is_empty() {
                return err("both 'from' and 'to' arguments are required and cannot be empty");
            }
            let from = resolve(cwd, from_str);
            let to = resolve(cwd, to_str);
            if let Some(parent) = to.parent() {
                let _ = fs::create_dir_all(parent);
            }
            checkpoint::record(cwd, &from, "move_path");
            match fs::rename(&from, &to) {
                Ok(_) => ok(format!("moved {} -> {}", from.display(), to.display())),
                Err(e) => err(format!(
                    "cannot move {} -> {}: {e}",
                    from.display(),
                    to.display()
                )),
            }
        }
        "remove_path" => {
            let p_str = path_arg(input).unwrap_or("");
            if p_str.is_empty() {
                return err("path argument is required and cannot be empty");
            }
            let p = resolve(cwd, p_str);
            checkpoint::record(cwd, &p, "remove_path");
            let res = if p.is_dir() {
                fs::remove_dir(&p)
            } else {
                fs::remove_file(&p)
            };
            match res {
                Ok(_) => ok(format!("removed {}", p.display())),
                Err(e) => err(format!("cannot remove {}: {e}", p.display())),
            }
        }
        "bash" | "run_command" => {
            let cmd = input["command"].as_str().unwrap_or("").trim();
            if cmd.is_empty() {
                return err("command argument is required and cannot be empty");
            }
            let mut command = if cfg!(windows) {
                let mut c = Command::new("cmd");
                c.args(["/C", cmd]);
                c
            } else {
                let mut c = Command::new("sh");
                c.args(["-c", cmd]);
                c
            };
            command.current_dir(cwd);
            match run_with_timeout(command, None, COMMAND_TIMEOUT) {
                Ok(cap) => command_outcome(cap, COMMAND_TIMEOUT),
                Err(e) => err(e),
            }
        }
        "todowrite" | "todo_write" => {
            let Some(items) = input["items"].as_array() else {
                return err("items is required");
            };
            if items.is_empty() {
                return err("items array cannot be empty");
            }
            let mut next = Vec::new();
            for item in items {
                let task = item["task"]
                    .as_str()
                    .or_else(|| item["content"].as_str())
                    .unwrap_or("")
                    .trim();
                let status = item["status"].as_str().unwrap_or("pending");
                if !task.is_empty() {
                    next.push((task.to_string(), status.to_string()));
                }
            }
            match todo_store().lock() {
                Ok(mut todos) => {
                    *todos = next;
                    ok(format!("stored {} todo item(s)", todos.len()))
                }
                Err(_) => err("todo store unavailable"),
            }
        }
        "todoread" | "todo_read" => match todo_store().lock() {
            Ok(todos) if todos.is_empty() => ok("no todo items"),
            Ok(todos) => ok(todos
                .iter()
                .enumerate()
                .map(|(i, (task, status))| format!("{}. [{status}] {task}", i + 1))
                .collect::<Vec<_>>()
                .join("\n")),
            Err(_) => err("todo store unavailable"),
        },
        "create_docx" => {
            let p_str = path_arg(input).unwrap_or("");
            if p_str.is_empty() {
                return err("path argument is required and cannot be empty");
            }
            let p = resolve(cwd, p_str);
            if p.extension().and_then(|e| e.to_str()) != Some("docx") {
                return err("path must end with .docx");
            }
            let title = input["title"].as_str().unwrap_or("Document");
            let body = input["body"].as_str().unwrap_or("");
            if let Some(dir) = p.parent() {
                let _ = fs::create_dir_all(dir);
            }
            checkpoint::record(cwd, &p, "create_docx");
            let bytes = docx_bytes(title, body);
            match fs::write(&p, &bytes) {
                Ok(_) => ok(format!("created {} ({} bytes)", p.display(), bytes.len())),
                Err(e) => err(format!("cannot write {}: {e}", p.display())),
            }
        }
        "webfetch" | "fetch_url" => {
            let url = input["url"].as_str().unwrap_or("");
            if url.is_empty() {
                return err("url is required");
            }
            match http_get_with_retry(ureq::get(url)) {
                Ok(resp) => match resp.into_string() {
                    Ok(body) => ok(truncate(body, MAX_OUT)),
                    Err(e) => err(format!("failed to read response body: {e}")),
                },
                Err(e) => err(format!("fetch failed: {e}")),
            }
        }
        "websearch" | "web_search" => {
            let query = input["query"].as_str().unwrap_or("").trim();
            if query.is_empty() {
                return err("query is required");
            }
            let encoded = url_encode(query);
            let search_url = format!("https://lite.duckduckgo.com/lite/?q={encoded}");
            match http_get_with_retry(
                ureq::get(&search_url)
                    .set("User-Agent", "Mozilla/5.0 (compatible; buildwithnexus/1.0)"),
            ) {
                Ok(resp) => match resp.into_string() {
                    Ok(html) => {
                        let hits = parse_ddg_lite(&html);
                        if hits.is_empty() {
                            // Markup didn't match — fall back to the raw text so
                            // the model still gets something.
                            ok(truncate(strip_html(&html), MAX_OUT))
                        } else {
                            let body = format_search_hits(&hits, 10);
                            ok(truncate(
                                format!("{} results for \"{query}\":\n\n{body}", hits.len()),
                                MAX_OUT,
                            ))
                        }
                    }
                    Err(e) => err(format!("search response error: {e}")),
                },
                Err(e) => err(format!("web search failed: {e}")),
            }
        }
        "headless_browser" => {
            let url = input["url"].as_str().unwrap_or("").trim();
            if url.is_empty() {
                return err("url is required");
            }
            let extract_links = input["extract_links"].as_bool().unwrap_or(false);
            match http_get_with_retry(
                ureq::get(url).set("User-Agent", "Mozilla/5.0 (compatible; buildwithnexus/1.0)"),
            ) {
                Ok(resp) => match resp.into_string() {
                    Ok(html) => {
                        // Extract <title>
                        let title = {
                            let lower = html.to_lowercase();
                            if let Some(start) = lower.find("<title") {
                                let after_tag = &html[start..];
                                if let Some(gt) = after_tag.find('>') {
                                    let rest = &after_tag[gt + 1..];
                                    if let Some(end) = rest.to_lowercase().find("</title") {
                                        rest[..end].trim().to_string()
                                    } else {
                                        String::new()
                                    }
                                } else {
                                    String::new()
                                }
                            } else {
                                String::new()
                            }
                        };

                        // Extract clean text content
                        let content = strip_html(&html);

                        // Extract links if requested
                        let links: Vec<String> = if extract_links {
                            let mut found = Vec::new();
                            let bytes = html.as_bytes();
                            let len = bytes.len();
                            let mut i = 0;
                            while i + 2 < len {
                                // Look for <a (case-insensitive)
                                if (bytes[i] == b'<')
                                    && (bytes[i + 1] == b'a' || bytes[i + 1] == b'A')
                                    && (bytes[i + 2] == b' '
                                        || bytes[i + 2] == b'\t'
                                        || bytes[i + 2] == b'\n')
                                {
                                    // Find the closing > of this tag
                                    let tag_start = i;
                                    let mut tag_end = i + 3;
                                    while tag_end < len && bytes[tag_end] != b'>' {
                                        tag_end += 1;
                                    }
                                    if tag_end < len {
                                        let tag_content = &html[tag_start..=tag_end];
                                        // Find href="..." or href='...'
                                        let tag_lower = tag_content.to_lowercase();
                                        if let Some(href_pos) = tag_lower.find("href=") {
                                            let after_href = &tag_content[href_pos + 5..];
                                            let href_val = if let Some(rest) =
                                                after_href.strip_prefix('"')
                                            {
                                                rest.find('"').map(|end| &rest[..end])
                                            } else if let Some(rest) = after_href.strip_prefix('\'')
                                            {
                                                rest.find('\'').map(|end| &rest[..end])
                                            } else {
                                                // Unquoted: take up to whitespace or >
                                                let end = after_href
                                                    .find(|c: char| c.is_whitespace() || c == '>')
                                                    .unwrap_or(after_href.len());
                                                Some(&after_href[..end])
                                            };
                                            if let Some(href) = href_val {
                                                let href = href.trim();
                                                if !href.is_empty()
                                                    && !href.starts_with('#')
                                                    && !href.starts_with("javascript:")
                                                {
                                                    found.push(href.to_string());
                                                }
                                            }
                                        }
                                    }
                                    i = tag_end + 1;
                                } else {
                                    i += 1;
                                }
                            }
                            found.dedup();
                            found
                        } else {
                            Vec::new()
                        };

                        let mut result = json!({
                            "title": title,
                            "content": content,
                        });
                        if extract_links {
                            result["links"] = json!(links);
                        }
                        ok(truncate(result.to_string(), MAX_OUT))
                    }
                    Err(e) => err(format!("failed to read response body: {e}")),
                },
                Err(e) => err(format!("headless_browser fetch failed: {e}")),
            }
        }
        "start_server" => start_server(input, cwd),
        "list_servers" => list_servers(),
        "stop_server" => stop_server(input),
        "read_server_log" => read_server_log(input),
        "wait_for_url" => wait_for_url(input),
        "open_browser" => open_browser(input, cwd),
        "list_python_tools" => {
            let rows = list_python_tools(cwd);
            if rows.is_empty() {
                ok("no Python tools found in .buildwithnexus/tools or $NEXUS_HOME/tools")
            } else {
                ok(rows.join("\n"))
            }
        }
        "python_tool" => {
            let raw = input["path"].as_str().unwrap_or("").trim();
            if raw.is_empty() {
                return err("path is required");
            }
            let path = find_python_tool(cwd, raw);
            let payload = input.get("input").cloned().unwrap_or_else(|| json!({}));
            let mut command = Command::new("python3");
            command.arg(&path).current_dir(cwd);
            match run_with_timeout(
                command,
                Some(payload.to_string().into_bytes()),
                COMMAND_TIMEOUT,
            ) {
                Ok(cap) => command_outcome(cap, COMMAND_TIMEOUT),
                Err(e) => err(format!("python tool {}: {e}", path.display())),
            }
        }
        "list_skills" => {
            let rows = crate::config::load_skill_descriptions()
                .into_iter()
                .map(|(name, desc)| format!("{name}: {desc}"))
                .collect::<Vec<_>>();
            if rows.is_empty() {
                ok("no skills available")
            } else {
                ok(format!(
                    "{}\n\nUse load_skill with a skill name to load its full instructions.",
                    rows.join("\n")
                ))
            }
        }
        "kb_query" => {
            let query = input["query"].as_str().unwrap_or("").trim();
            if query.is_empty() {
                return err("query is required");
            }
            let kb = crate::knowledge::KnowledgeBase::new(&cwd.to_string_lossy());
            let results = kb.search(query);
            if results.is_empty() {
                ok("No matching knowledge base entities found.")
            } else {
                let formatted: Vec<String> = results
                    .iter()
                    .map(|e| {
                        format!(
                            "ID: {}\nType: {:?}\nName: {}\nDesc: {}\n",
                            e.id,
                            e.entity_type,
                            e.name,
                            e.description.as_deref().unwrap_or("none")
                        )
                    })
                    .collect();
                ok(formatted.join("\n---\n"))
            }
        }
        "kb_record" => {
            let name = input["name"].as_str().unwrap_or("").trim();
            if name.is_empty() {
                return err("name is required");
            }
            let etype = match input["entity_type"].as_str().unwrap_or("Service") {
                "Function" => crate::knowledge::EntityType::Function,
                "Class" => crate::knowledge::EntityType::Class,
                "Module" => crate::knowledge::EntityType::Module,
                "Package" => crate::knowledge::EntityType::Package,
                "Endpoint" => crate::knowledge::EntityType::Endpoint,
                "DatabaseTable" => crate::knowledge::EntityType::DatabaseTable,
                "Migration" => crate::knowledge::EntityType::Migration,
                "ArchitectureDecision" => crate::knowledge::EntityType::ArchitectureDecision,
                _ => crate::knowledge::EntityType::Service,
            };
            let desc = input["description"].as_str().map(|s| s.to_string());
            let mut kb = crate::knowledge::KnowledgeBase::new(&cwd.to_string_lossy());
            let entity = crate::knowledge::Entity {
                id: format!(
                    "{}-{}",
                    name.to_lowercase().replace(' ', "-"),
                    kb.entities.len() + 1
                ),
                entity_type: etype,
                name: name.to_string(),
                path: input["path"].as_str().map(|s| s.to_string()),
                description: desc,
                metadata: input["metadata"].clone(),
                relationships: vec![],
                last_updated: iso8601_utc_now(),
            };
            let id = entity.id.clone();
            kb.add_entity(entity);
            let _ = kb.save();
            ok(format!("Successfully recorded knowledge entity: {id}"))
        }
        "rule_check" => {
            let engine = crate::rules::RuleEngine::load_defaults();
            let mut changed = Vec::new();
            if let Some(arr) = input["changed_files"].as_array() {
                for v in arr {
                    if let Some(s) = v.as_str() {
                        changed.push(s.to_string());
                    }
                }
            }
            let ctx = crate::rules::EvaluationContext {
                task_type: None,
                changed_files: changed,
                tools_called: vec![],
                tests_added: vec![],
                tests_run: false,
                dependencies_added: vec![],
                dependencies_removed: vec![],
                migration_type: None,
                has_rollback_plan: false,
                has_changelog_entry: false,
                security_review_done: false,
                custom_facts: std::collections::HashMap::new(),
            };
            let violations = engine.evaluate(&ctx);
            if violations.is_empty() {
                ok("All engineering rules passed! No violations found.")
            } else {
                ok(crate::rules::RuleEngine::format_violations(&violations))
            }
        }
        "verify" => {
            let verifier = crate::verifier::Verifier::new(&cwd.to_string_lossy());
            let mut changed = Vec::new();
            if let Some(arr) = input["changed_files"].as_array() {
                for v in arr {
                    if let Some(s) = v.as_str() {
                        changed.push(s.to_string());
                    }
                }
            }
            let ctx = crate::verifier::VerificationContext {
                task_description: input["task_description"]
                    .as_str()
                    .unwrap_or("Task verification")
                    .to_string(),
                task_type: None,
                changed_files: changed,
                tool_calls: vec![],
                evidence_gathered: vec![],
                tests_added: vec![],
                dependencies_changed: vec![],
                git_diff: None,
            };
            let report = verifier.verify(&ctx);
            ok(crate::verifier::Verifier::format_report(&report))
        }
        "mcp_call" => {
            let server = input["server"].as_str().unwrap_or("");
            let tool = input["tool"].as_str().unwrap_or("");
            let args = &input["arguments"];
            if server.is_empty() || tool.is_empty() {
                return err("server and tool names are required");
            }
            let s = crate::config::load_settings().unwrap_or_default();
            if let Some(srv_config) = s.mcp_servers.get(server) {
                let cmd = srv_config["command"].as_str().unwrap_or("");
                let srv_args: Vec<&str> = srv_config["args"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                if cmd.is_empty() {
                    return err(format!(
                        "MCP server '{server}' has no command configured in settings"
                    ));
                }
                // A real MCP server hangs without the initialize handshake, and
                // waiting for process exit hangs forever on servers that keep
                // stdin open — so handshake first, then read the tools/call
                // response line-by-line under a deadline.
                let init = json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "clientInfo": {"name": "buildwithnexus", "version": "1.0"}
                    }
                });
                let initialized = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                });
                let call_req = json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "name": tool,
                        "arguments": args
                    }
                });
                let mut child = match Command::new(cmd)
                    .args(&srv_args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    Ok(c) => c,
                    Err(e) => {
                        return err(format!(
                            "failed to spawn MCP server '{server}' ({cmd}): {e}"
                        ))
                    }
                };
                let mut stdin = child.stdin.take();
                if let Some(w) = stdin.as_mut() {
                    for msg in [&init, &initialized, &call_req] {
                        let _ = std::io::Write::write_all(w, msg.to_string().as_bytes());
                        let _ = std::io::Write::write_all(w, b"\n");
                    }
                    let _ = std::io::Write::flush(w);
                }
                let (tx, rx) = std::sync::mpsc::channel::<String>();
                if let Some(stdout) = child.stdout.take() {
                    std::thread::spawn(move || {
                        let reader = std::io::BufReader::new(stdout);
                        for line in std::io::BufRead::lines(reader) {
                            let Ok(line) = line else { break };
                            if tx.send(line).is_err() {
                                break;
                            }
                        }
                    });
                }
                let deadline = std::time::Instant::now() + MCP_TIMEOUT;
                let outcome = loop {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        break err(format!(
                            "MCP server '{server}' did not answer tools/call within {}s; verify the server command and tool name",
                            MCP_TIMEOUT.as_secs()
                        ));
                    }
                    match rx.recv_timeout(remaining) {
                        // Skip the initialize response (id 1) and any
                        // notifications; only id 2 is our tools/call answer.
                        Ok(line) => {
                            let Ok(resp) = serde_json::from_str::<Value>(&line) else {
                                continue;
                            };
                            if resp["id"] != json!(2) {
                                continue;
                            }
                            if let Some(res) = resp.get("result") {
                                break ok(res.to_string());
                            }
                            if let Some(err_val) = resp.get("error") {
                                break err(format!("MCP server error: {err_val}"));
                            }
                            break ok(resp.to_string());
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            break err(format!(
                                "MCP server '{server}' did not answer tools/call within {}s; verify the server command and tool name",
                                MCP_TIMEOUT.as_secs()
                            ))
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            break err(format!(
                                "MCP server '{server}' exited before answering tools/call"
                            ))
                        }
                    }
                };
                drop(stdin);
                let _ = child.kill();
                let _ = child.wait();
                outcome
            } else {
                err(format!(
                    "MCP server '{server}' not found in settings.json mcp_servers"
                ))
            }
        }
        "skill" | "load_skill" => {
            let wanted = input["name"]
                .as_str()
                .unwrap_or("")
                .trim()
                .trim_start_matches('/');
            if wanted.is_empty() {
                return err("name is required");
            }
            for (name, content) in crate::config::load_skills() {
                if name == wanted {
                    crate::trace::record_visible(
                        "skill",
                        format!("loaded {name}"),
                        json!({"name": name, "bytes": content.len(), "preview": crate::trace::preview(&content, 600)}),
                    );
                    return ok(format!("# Skill: {name}\n{content}"));
                }
            }
            err(format!("skill not found: {wanted}"))
        }
        "str_replace_editor" | "text_editor_20241022" | "text_editor_20250124" => {
            let command = input["command"].as_str().unwrap_or("view");
            let path = input["path"]
                .as_str()
                .or_else(|| input["filePath"].as_str())
                .unwrap_or("");
            if path.is_empty() {
                return err("path is required");
            }
            match command {
                "view" => {
                    let p = resolve(cwd, path);
                    if p.is_dir() {
                        match fs::read_dir(&p) {
                            Ok(rd) => {
                                let mut names: Vec<String> = rd
                                    .filter_map(|e| e.ok())
                                    .map(|e| {
                                        let n = e.file_name().to_string_lossy().into_owned();
                                        if e.path().is_dir() {
                                            format!("{n}/")
                                        } else {
                                            n
                                        }
                                    })
                                    .collect();
                                names.sort();
                                ok(names.join("\n"))
                            }
                            Err(e) => err(format!("cannot list {}: {e}", p.display())),
                        }
                    } else {
                        match fs::read_to_string(&p) {
                            Ok(c) => {
                                let lines: Vec<&str> = c.lines().collect();
                                let total_lines = lines.len();
                                if total_lines == 0 {
                                    return ok(String::new());
                                }
                                let (start, end) = if let Some(range) =
                                    input["view_range"].as_array()
                                {
                                    let start = range.first().and_then(|v| v.as_u64()).unwrap_or(1)
                                        as usize;
                                    let end = range
                                        .get(1)
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(total_lines as u64)
                                        as usize;
                                    (
                                        start.clamp(1, total_lines.max(1)),
                                        end.clamp(1, total_lines.max(1)),
                                    )
                                } else {
                                    (1, 500.min(total_lines))
                                };
                                let mut selected_lines = Vec::new();
                                for (i, line) in lines
                                    .iter()
                                    .enumerate()
                                    .take((end - 1).min(total_lines - 1) + 1)
                                    .skip(start - 1)
                                {
                                    selected_lines.push(format!("{:>4}: {}", i + 1, line));
                                }
                                ok(selected_lines.join("\n"))
                            }
                            Err(e) => err(format!("cannot view {}: {e}", p.display())),
                        }
                    }
                }
                "create" => {
                    let p = resolve(cwd, path);
                    // A dropped `file_text` key must not silently produce an
                    // empty file (see the same guard on write_file).
                    let Some(file_text) = input["file_text"].as_str() else {
                        return err(
                            "missing required param `file_text` — the file was NOT written; re-send the full create call including the complete `file_text` string (use an explicit empty string to create an empty file)",
                        );
                    };
                    let overwrite_len = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                    if file_text.is_empty() && overwrite_len > 0 {
                        return err(format!(
                            "refusing to overwrite non-empty file {} ({overwrite_len} bytes) with empty content — if you really intend to empty it, remove it first or send the intended replacement content",
                            p.display()
                        ));
                    }
                    if let Some(dir) = p.parent() {
                        let _ = fs::create_dir_all(dir);
                    }
                    if let Ok(old_content) = fs::read_to_string(&p) {
                        let mut guard = UNDO_BACKUP.lock().unwrap_or_else(|e| e.into_inner());
                        *guard = Some((p.clone(), old_content));
                    } else {
                        let mut guard = UNDO_BACKUP.lock().unwrap_or_else(|e| e.into_inner());
                        *guard = None;
                    }
                    match fs::write(&p, file_text) {
                        Ok(_) => ok(format!("successfully created file {}", p.display())),
                        Err(e) => err(format!("cannot write {}: {e}", p.display())),
                    }
                }
                "str_replace" => {
                    let p = resolve(cwd, path);
                    let old_str = input["old_str"].as_str().unwrap_or("");
                    let new_str = input["new_str"].as_str().unwrap_or("");
                    if old_str.is_empty() {
                        return err("old_str cannot be empty");
                    }
                    let body = match fs::read_to_string(&p) {
                        Ok(b) => b,
                        Err(e) => return err(format!("cannot read {}: {e}", p.display())),
                    };
                    let count = body.matches(old_str).count();
                    if count == 0 {
                        // Tiered lenient fallback: a unique whitespace-tolerant
                        // hit is auto-applied; ambiguous or absent text keeps
                        // the diagnostic.
                        if let LenientMatch::Hit(hit) = lenient_find(&body, old_str) {
                            let next = apply_lenient(&body, &hit, new_str);
                            {
                                let mut guard =
                                    UNDO_BACKUP.lock().unwrap_or_else(|e| e.into_inner());
                                *guard = Some((p.clone(), body));
                            }
                            return match fs::write(&p, &next) {
                                Ok(_) => ok(format!(
                                    "successfully edited file {} ({LENIENT_MATCH_NOTE})",
                                    p.display()
                                )),
                                Err(e) => err(format!("cannot write {}: {e}", p.display())),
                            };
                        }
                        return err(format!(
                            "old text not found in {}. {}.\n\nold text sought:\n{}",
                            p.display(),
                            edit_not_found_hint(&body, old_str),
                            old_str
                        ));
                    }
                    if count > 1 {
                        return err(format!(
                            "ambiguous replacement: old text matches {count} times in {}",
                            p.display()
                        ));
                    }
                    let next = body.replace(old_str, new_str);
                    {
                        let mut guard = UNDO_BACKUP.lock().unwrap_or_else(|e| e.into_inner());
                        *guard = Some((p.clone(), body));
                    }
                    match fs::write(&p, &next) {
                        Ok(_) => ok(format!("successfully edited file {}", p.display())),
                        Err(e) => err(format!("cannot write {}: {e}", p.display())),
                    }
                }
                "insert" => {
                    let p = resolve(cwd, path);
                    // Standard text_editor semantics: insert AFTER line
                    // `insert_line`; 0 means the top of the file. (The old
                    // `insert_line - 1` both inserted before the line and
                    // underflowed on 0.)
                    let insert_line = input["insert_line"].as_u64().unwrap_or(0) as usize;
                    let new_str = input["new_str"].as_str().unwrap_or("");
                    let body = match fs::read_to_string(&p) {
                        Ok(b) => b,
                        Err(e) => return err(format!("cannot read {}: {e}", p.display())),
                    };
                    let mut lines: Vec<String> = body.lines().map(|s| s.to_string()).collect();
                    let idx = insert_line.min(lines.len());
                    lines.insert(idx, new_str.to_string());
                    // Preserve the file's existing line-ending convention.
                    let eol = if body.contains("\r\n") { "\r\n" } else { "\n" };
                    let mut next = lines.join(eol);
                    if body.ends_with('\n') {
                        next.push_str(eol);
                    }
                    {
                        let mut guard = UNDO_BACKUP.lock().unwrap_or_else(|e| e.into_inner());
                        *guard = Some((p.clone(), body));
                    }
                    match fs::write(&p, &next) {
                        Ok(_) => ok(format!(
                            "successfully inserted text after line {insert_line} in {}",
                            p.display()
                        )),
                        Err(e) => err(format!("cannot write {}: {e}", p.display())),
                    }
                }
                "undo_edit" => {
                    let p = resolve(cwd, path);
                    let mut guard = UNDO_BACKUP.lock().unwrap_or_else(|e| e.into_inner());
                    let is_match = guard.as_ref().is_some_and(|(bp, _)| *bp == p);
                    if !is_match {
                        if let Some((backup_path, _)) = &*guard {
                            return err(format!(
                                "the last edit was to {}, cannot undo for {}",
                                backup_path.display(),
                                p.display()
                            ));
                        } else {
                            return err("no undo backup available for this file");
                        }
                    }
                    if let Some((_, old_content)) = guard.take() {
                        match fs::write(&p, &old_content) {
                            Ok(_) => ok(format!(
                                "successfully reverted the last edit to {}",
                                p.display()
                            )),
                            Err(e) => {
                                *guard = Some((p.clone(), old_content));
                                err(format!("cannot write {}: {e}", p.display()))
                            }
                        }
                    } else {
                        err("no undo backup available for this file")
                    }
                }
                other => err(format!("unknown editor command: {other}")),
            }
        }
        "Artifact" | "publish_artifact" => {
            let contents = input["contents"].as_str().unwrap_or("");
            let title = input["title"].as_str().unwrap_or("artifact");
            // When the model omits `type`, sniff the contents instead of
            // assuming HTML — markdown reports were being rejected by
            // HTML-app rules.
            let kind = match input["type"].as_str() {
                Some(k) => k,
                None => {
                    let head = contents.trim_start().to_ascii_lowercase();
                    if head.starts_with("<!doctype") || head.starts_with("<html") {
                        "html"
                    } else {
                        "markdown"
                    }
                }
            };
            if let Some(reason) = artifact_quality_error(contents, kind) {
                return err(reason);
            }
            let warning = artifact_game_warning(title, contents, kind);
            let safe_title: String = title
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '_' })
                .collect();
            let ext = match kind {
                "html" => "html",
                "markdown" => "md",
                "svg" => "svg",
                "json" => "json",
                _ => "txt",
            };
            let dir = cwd.join(".buildwithnexus").join("artifacts");
            let _ = fs::create_dir_all(&dir);
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let filename = format!("{}_{timestamp}.{ext}", safe_title);
            let p = dir.join(&filename);
            if let Err(e) = fs::write(&p, contents) {
                return err(format!("cannot write artifact: {e}"));
            }
            let clean_name = if (safe_title.is_empty()
                || safe_title == "_"
                || safe_title.eq_ignore_ascii_case("artifact")
                || safe_title.eq_ignore_ascii_case("index"))
                && ext == "html"
            {
                "index".to_string()
            } else {
                safe_title.to_lowercase()
            };
            let direct_file = cwd.join(format!("{clean_name}.{ext}"));
            // The workspace copy can clobber an existing cwd/index.html —
            // checkpoint it first so /undo can restore, and surface a failed
            // write instead of swallowing it.
            if direct_file.exists() {
                checkpoint::record(cwd, &direct_file, "publish_artifact");
            }
            if let Err(e) = fs::write(&direct_file, contents) {
                return err(format!(
                    "artifact archived to {} but the workspace copy {} could not be written: {e}",
                    p.display(),
                    direct_file.display()
                ));
            }
            // Auto-open at most once per session per artifact name so
            // republishing doesn't spawn a new browser tab every time.
            if ext == "html" && mark_artifact_opened(&clean_name) && !cfg!(test) {
                let _ = if cfg!(target_os = "macos") {
                    Command::new("open").arg(&direct_file).status()
                } else if cfg!(windows) {
                    Command::new("cmd")
                        .args(["/C", "start", "", &direct_file.to_string_lossy()])
                        .status()
                } else {
                    Command::new("xdg-open").arg(&direct_file).status()
                };
            }
            ok(format!(
                "Artifact successfully published locally to: {} (and created directly in workspace at: {}){}",
                p.display(),
                direct_file.display(),
                warning.map(|w| format!("\n{w}")).unwrap_or_default()
            ))
        }
        "finish" => Outcome {
            content: input["summary"].as_str().unwrap_or("done").to_string(),
            is_error: false,
            finished: true,
        },
        "exit_plan" | "ExitPlanMode" => {
            if let Some(steps) = input["steps"].as_array() {
                let rows = steps
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .enumerate()
                    .map(|(idx, step)| format!("{}. {step}", idx + 1))
                    .collect::<Vec<_>>();
                if rows.is_empty() {
                    err("exit_plan requires plan or non-empty steps")
                } else {
                    ok(rows.join("\n"))
                }
            } else if let Some(plan) = input["plan"].as_str().filter(|s| !s.trim().is_empty()) {
                ok(plan.trim().to_string())
            } else {
                err("exit_plan requires plan or non-empty steps")
            }
        }
        other => {
            // List the real tool surface and suggest the nearest name so a
            // cheap model can self-correct instead of retrying blind.
            let names: Vec<&'static str> = defs(true).iter().map(|d| d.name).collect();
            let nearest = names
                .iter()
                .min_by_key(|n| levenshtein(&other.to_ascii_lowercase(), &n.to_ascii_lowercase()))
                .copied();
            let mut msg = format!("unknown tool: {other}.");
            if let Some(n) = nearest {
                msg.push_str(&format!(" Did you mean `{n}`?"));
            }
            msg.push_str(&format!(" Valid tools: {}", names.join(", ")));
            err(msg)
        }
    }
}

// Helper for web tools: retries transient HTTP errors (429, 500..=504, transport failures)
// up to 3 times with exponential backoff (500ms, 1000ms, 2000ms).
fn http_get_with_retry(req: ureq::Request) -> Result<ureq::Response, Box<ureq::Error>> {
    let mut attempts = 0;
    let max_attempts = 4;
    let mut delay_ms = 500;

    loop {
        attempts += 1;
        let req_clone = req.clone();
        match req_clone.call() {
            Ok(resp) => return Ok(resp),
            Err(ureq::Error::Status(code, _resp))
                if (code == 429 || (500..=504).contains(&code)) && attempts < max_attempts =>
            {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                delay_ms *= 2;
                continue;
            }
            Err(ureq::Error::Transport(_)) if attempts < max_attempts => {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                delay_ms *= 2;
                continue;
            }
            Err(e) => return Err(Box::new(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ── web search parsing ──────────────────────────────────────────────────
    const DDG_LITE_SAMPLE: &str = r#"<html><body><form>search</form><table>
      <tr><td>1.&nbsp;</td><td>
        <a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fstd%2F&amp;rut=abc" class='result-link'>Rust std docs</a>
      </td></tr>
      <tr><td>&nbsp;</td><td class='result-snippet'>The Rust Standard Library &amp; API reference.</td></tr>
      <tr><td>2.&nbsp;</td><td>
        <a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fcrates.io%2F&amp;rut=def" class='result-link'>crates.io</a>
      </td></tr>
      <tr><td>&nbsp;</td><td class='result-snippet'>The Rust package registry.</td></tr>
    </table></body></html>"#;

    #[test]
    fn percent_decode_handles_escapes_and_plus() {
        assert_eq!(percent_decode("a%2Fb%20c+d"), "a/b c d");
        // A malformed trailing escape is left literal, not dropped.
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn decode_html_entities_single_pass_no_double_decode() {
        // Single pass: &amp; becomes a literal & and the following "lt;" is left
        // alone, so this must not collapse to "<".
        assert_eq!(decode_html_entities("a &amp;lt; b"), "a &lt; b");
        assert_eq!(
            decode_html_entities("x &lt;y&gt; &quot;z&quot;"),
            "x <y> \"z\""
        );
    }

    #[test]
    fn decode_html_entities_resolves_numeric_and_typographic() {
        // Decimal and hex numeric references for a curly apostrophe.
        assert_eq!(decode_html_entities("it&#8217;s"), "it’s");
        assert_eq!(decode_html_entities("it&#x2019;s"), "it’s");
        // Common typographic named entities.
        assert_eq!(decode_html_entities("a &mdash; b &hellip;"), "a — b …");
    }

    #[test]
    fn decode_html_entities_leaves_unknown_and_bare_amp_literal() {
        assert_eq!(decode_html_entities("Tom & Jerry"), "Tom & Jerry");
        assert_eq!(decode_html_entities("&notreal;"), "&notreal;");
        // A `&` with no nearby `;` is untouched.
        assert_eq!(
            decode_html_entities("cats & dogs everywhere"),
            "cats & dogs everywhere"
        );
    }

    #[test]
    fn ddg_real_url_recovers_target_from_redirect() {
        assert_eq!(
            ddg_real_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa&amp;rut=z"),
            "https://example.com/a"
        );
        assert_eq!(ddg_real_url("//example.com/x"), "https://example.com/x");
    }

    #[test]
    fn parse_ddg_lite_extracts_structured_hits() {
        let hits = parse_ddg_lite(DDG_LITE_SAMPLE);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Rust std docs");
        assert_eq!(hits[0].url, "https://doc.rust-lang.org/std/");
        assert_eq!(
            hits[0].snippet,
            "The Rust Standard Library & API reference."
        );
        assert_eq!(hits[1].title, "crates.io");
        assert_eq!(hits[1].url, "https://crates.io/");
    }

    #[test]
    fn parse_ddg_lite_returns_empty_on_unrelated_html() {
        assert!(parse_ddg_lite("<html><body>no results here</body></html>").is_empty());
    }

    #[test]
    fn format_search_hits_numbers_and_caps() {
        let hits = parse_ddg_lite(DDG_LITE_SAMPLE);
        let out = format_search_hits(&hits, 1);
        assert!(out.starts_with("1. Rust std docs"));
        assert!(out.contains("https://doc.rust-lang.org/std/"));
        // Capped at 1 — the second hit must not appear.
        assert!(!out.contains("crates.io"));
    }

    // ── docx tables ─────────────────────────────────────────────────────────
    #[test]
    fn table_row_and_separator_detection() {
        assert!(is_table_row("| a | b |"));
        assert!(is_table_row("|a|b|"));
        assert!(!is_table_row("a | b")); // no leading pipe
        assert!(!is_table_row("- bullet"));
        assert!(is_table_separator("| --- | :---: |"));
        assert!(!is_table_separator("| a | b |"));
    }

    #[test]
    fn parse_table_cells_drops_outer_pipes_and_trims() {
        assert_eq!(parse_table_cells("|  a | b  |c|"), vec!["a", "b", "c"]);
    }

    #[test]
    fn docx_document_renders_markdown_table() {
        let body = "Intro line.\n| Name | Price |\n| --- | --- |\n| Glazed | $1.50 |\n| Old Fashioned | $2.00 |\n\nOutro.";
        let doc = docx_document("Menu", body);
        // A real table with a header row is emitted.
        assert!(doc.contains("<w:tbl>"));
        assert_eq!(doc.matches("<w:tr>").count(), 3); // header + 2 body rows
                                                      // Header cells are bold; the separator row is gone.
        assert!(doc.contains("<w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">Name</w:t>"));
        assert!(!doc.contains("---"));
        // Surrounding prose still renders as paragraphs.
        assert!(doc.contains(">Intro line.</w:t>"));
        assert!(doc.contains(">Outro.</w:t>"));
    }

    #[test]
    fn docx_document_without_table_is_unchanged_shape() {
        let doc = docx_document("T", "# Head\n- item\nplain");
        assert!(!doc.contains("<w:tbl>"));
        assert!(doc.contains(">• item</w:t>"));
    }

    // ── docx inline formatting ──────────────────────────────────────────────
    #[test]
    fn docx_runs_renders_bold_italic_and_code() {
        let out = docx_runs("plain **bold** and *italic* and `code` end");
        // Bold run carries <w:b/>, italic <w:i/>, code the monospace font.
        assert!(out.contains("<w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">bold</w:t>"));
        assert!(out.contains("<w:rPr><w:i/></w:rPr><w:t xml:space=\"preserve\">italic</w:t>"));
        assert!(out.contains("w:ascii=\"Consolas\""));
        assert!(out.contains(">code</w:t>"));
        // The markers themselves are consumed, not emitted as literal text.
        assert!(!out.contains('*'));
        assert!(!out.contains('`'));
    }

    #[test]
    fn docx_runs_leaves_arithmetic_and_snake_case_literal() {
        // Spaced `*` is not emphasis; `_` never toggles italic.
        let out = docx_runs("compute 5 * 3 for my_var_name");
        assert!(!out.contains("<w:i/>"));
        assert!(!out.contains("<w:b/>"));
        assert!(out.contains("5 * 3 for my_var_name"));
    }

    #[test]
    fn docx_runs_code_span_is_verbatim() {
        // Emphasis markers inside a code span stay literal.
        let out = docx_runs("call `a*b` now");
        assert!(out.contains(">a*b</w:t>"));
        assert!(!out.contains("<w:i/>"));
    }

    #[test]
    fn docx_runs_escapes_xml_in_runs() {
        let out = docx_runs("**a < b & c**");
        assert!(out.contains("a &lt; b &amp; c"));
        assert!(out.contains("<w:b/>"));
    }

    #[test]
    fn docx_runs_unmatched_marker_stays_literal() {
        // A lone trailing `**` with nothing after it must not open emphasis.
        let out = docx_runs("trailing **");
        assert!(!out.contains("<w:b/>"));
        assert!(out.contains("trailing **"));
    }

    // ── truncate ────────────────────────────────────────────────────────────
    #[test]
    fn truncate_shorter_than_max_unchanged() {
        assert_eq!(truncate("hello".into(), 100), "hello");
    }

    #[test]
    fn truncate_appends_marker() {
        let out = truncate("abcdefghij".into(), 5);
        assert!(out.starts_with("abcde"));
        assert!(out.contains("[truncated]"));
    }

    #[test]
    fn truncate_backs_off_to_char_boundary() {
        // "é" is two bytes; cutting at an odd byte must not panic and must yield
        // valid UTF-8.
        let s = "a".to_string() + &"é".repeat(50); // 1 + 100 bytes
        let out = truncate(s, 4); // 4 lands mid-glyph
        assert!(out.is_char_boundary(out.find('\n').unwrap_or(out.len())));
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn truncate_emoji_boundary() {
        let s = "🎉".repeat(10); // 4 bytes each
        let out = truncate(s, 5); // mid-emoji
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn truncate_exactly_max_unchanged() {
        assert_eq!(truncate("abcde".into(), 5), "abcde");
    }

    // ── is_sensitive ────────────────────────────────────────────────────────
    #[test]
    fn sensitive_paths() {
        for p in [
            "/home/u/.ssh/id_rsa",
            "/home/u/.aws/credentials",
            "/home/u/.gnupg/secring.gpg",
            "/home/u/.buildwithnexus/.env.keys",
            "/proj/.env",
            "/proj/.env.local",
            "/proj/server.pem",
            "/home/u/id_ed25519",
        ] {
            assert!(is_sensitive(Path::new(p)), "{p} should be sensitive");
        }
    }

    #[test]
    fn non_sensitive_paths() {
        for p in [
            "/proj/src/main.rs",
            "/proj/README.md",
            "/proj/environment.txt",
        ] {
            assert!(!is_sensitive(Path::new(p)), "{p} should not be sensitive");
        }
    }

    #[test]
    fn sensitive_is_case_insensitive() {
        assert!(is_sensitive(Path::new("/home/U/.SSH/known_hosts")));
    }

    // ── escapes_cwd ─────────────────────────────────────────────────────────
    #[test]
    fn escapes_cwd_detects_parent_traversal() {
        let cwd = PathBuf::from("/proj/work");
        assert!(escapes_cwd(Path::new("/proj/work/../../etc/passwd"), &cwd));
        assert!(escapes_cwd(Path::new("/etc/passwd"), &cwd));
    }

    #[test]
    fn escapes_cwd_allows_inside() {
        let cwd = PathBuf::from("/proj/work");
        // Use a non-existent dir so canonicalize falls back to lexical normalize.
        assert!(!escapes_cwd(Path::new("/proj/work/src/a.rs"), &cwd));
        assert!(!escapes_cwd(Path::new("/proj/work/a/../b.rs"), &cwd));
    }

    // ── catastrophic ────────────────────────────────────────────────────────
    #[test]
    fn catastrophic_commands() {
        for c in [
            "rm -rf /",
            "rm   -rf   /",
            "rm -fr /home",
            ":(){ :|:& };:",
            "mkfs.ext4 /dev/sda1",
            "dd if=/dev/zero of=/dev/sda",
            "echo x > /dev/sda",
            "chmod -R 777 /",
        ] {
            assert!(catastrophic(c), "{c} should be catastrophic");
        }
    }

    #[test]
    fn non_catastrophic_commands() {
        for c in [
            "ls -la",
            "rm -rf ./build",
            "cargo test",
            "git status",
            "rm file.txt",
        ] {
            assert!(!catastrophic(c), "{c} should be allowed");
        }
    }

    // ── normalize ───────────────────────────────────────────────────────────
    #[test]
    fn normalize_folds_dot_segments() {
        assert_eq!(normalize(Path::new("a/./b/../c")), PathBuf::from("a/c"));
        assert_eq!(normalize(Path::new("./x")), PathBuf::from("x"));
    }

    // ── is_mutating / touched_path / preview ────────────────────────────────
    #[test]
    fn mutating_classification() {
        assert!(is_mutating("write_file"));
        assert!(is_mutating("edit_file"));
        assert!(is_mutating("run_command"));
        assert!(is_mutating("start_server"));
        assert!(is_mutating("stop_server"));
        assert!(is_mutating("open_browser"));
        assert!(!is_mutating("read_file"));
        assert!(!is_mutating("list_dir"));
        assert!(!is_mutating("list_servers"));
        assert!(!is_mutating("read_server_log"));
        assert!(!is_mutating("wait_for_url"));
        assert!(!is_mutating("exit_plan"));
        assert!(!is_mutating("ExitPlanMode"));
        assert!(!is_mutating("finish"));
    }

    #[test]
    fn touched_path_only_for_file_tools() {
        let cwd = Path::new("/proj");
        assert!(touched_path("read_file", &json!({"path": "a.rs"}), cwd).is_some());
        assert!(touched_path("run_command", &json!({"command": "ls"}), cwd).is_none());
        assert!(touched_path("finish", &json!({}), cwd).is_none());
    }

    #[test]
    fn touched_path_resolves_relative_to_cwd() {
        let cwd = Path::new("/proj");
        assert_eq!(
            touched_path("read_file", &json!({"path": "src/a.rs"}), cwd),
            Some(PathBuf::from("/proj/src/a.rs"))
        );
    }

    #[test]
    fn preview_strings() {
        assert_eq!(preview("write_file", &json!({"path": "a"})), "write a");
        assert_eq!(preview("run_command", &json!({"command": "ls"})), "run: ls");
        assert_eq!(preview("read_file", &json!({})), "read_file");
    }

    #[test]
    fn defs_includes_subagent_only_when_requested() {
        assert!(!defs(false).iter().any(|d| d.name == "spawn_subagent"));
        assert!(defs(true).iter().any(|d| d.name == "spawn_subagent"));
    }

    #[test]
    fn defs_include_server_and_browser_tools() {
        let names = defs(false).into_iter().map(|d| d.name).collect::<Vec<_>>();
        for name in [
            "start_server",
            "list_servers",
            "stop_server",
            "read_server_log",
            "wait_for_url",
            "open_browser",
        ] {
            assert!(names.contains(&name), "{name} should be advertised");
        }
    }

    #[test]
    fn defs_for_small_context_keeps_core_tools_and_drops_bulk() {
        // 32k is the top of the compact range: all realistic local models get
        // the trimmed surface; larger remote contexts keep the full set.
        let compact = defs_for_context(false, 32_768)
            .into_iter()
            .map(|d| d.name)
            .collect::<Vec<_>>();
        let full = defs_for_context(false, 200_000)
            .into_iter()
            .map(|d| d.name)
            .collect::<Vec<_>>();
        assert_eq!(
            compact,
            defs_for_context(false, 8192)
                .into_iter()
                .map(|d| d.name)
                .collect::<Vec<_>>(),
            "everything at or below the threshold gets the same compact set"
        );
        // One canonical tool per capability.
        for name in [
            "read_file",
            "write_file",
            "edit_file",
            "run_command",
            "find_files",
            "grep_files",
            "list_dir",
            "finish",
            "Artifact",
            "python_tool",
        ] {
            assert!(compact.contains(&name), "{name} should stay in compact set");
        }
        // Bulk tools and pure aliases are dropped for small/local models.
        for name in [
            "mcp_call",
            "kb_query",
            "kb_record",
            "verify",
            "text_editor_20250124",
            "question",
            "AskUserQuestion",
            "read",
            "write",
            "edit",
            "bash",
            "publish_artifact",
            "start_server",
        ] {
            assert!(
                !compact.contains(&name),
                "{name} should be omitted from compact set"
            );
            assert!(
                full.contains(&name),
                "{name} should remain available normally"
            );
        }
        // Small models degrade with big catalogs; keep the surface tight.
        assert!(
            compact.len() <= 12,
            "compact set has {} defs",
            compact.len()
        );
        assert!(compact.len() < full.len());
    }

    #[test]
    fn defs_readonly_omits_write_tools() {
        let names = defs_readonly()
            .into_iter()
            .map(|d| d.name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"run_command"));
        assert!(names.contains(&"list_servers"));
        assert!(names.contains(&"read_server_log"));
        assert!(names.contains(&"wait_for_url"));
        assert!(names.contains(&"exit_plan"));
        assert!(names.contains(&"ExitPlanMode"));
        for name in [
            "write",
            "edit",
            "patch",
            "write_file",
            "edit_file",
            "apply_patch",
            "create_docx",
            "start_server",
            "stop_server",
            "open_browser",
            "spawn_subagent",
        ] {
            assert!(
                !names.contains(&name),
                "{name} should not be advertised in PLAN"
            );
        }
    }

    #[test]
    fn safe_name_strips_shell_sensitive_characters() {
        assert_eq!(safe_name("npm dev:8080"), "npm-dev-8080");
        assert_eq!(safe_name("../../../"), "server");
        assert_eq!(safe_name("my_server-1"), "my_server-1");
    }

    #[cfg(unix)]
    #[test]
    fn run_start_list_stop_server_roundtrip() {
        let _guard = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = tempdir().join("home");
        fs::create_dir_all(&home).unwrap();
        std::env::set_var("NEXUS_HOME", &home);

        let cwd = tempdir();
        let name = format!("test-server-{}", now_ms());
        let expected = safe_name(&name);
        let started = run(
            "start_server",
            &json!({"name": name, "command": "while true; do sleep 1; done"}),
            &cwd,
        );
        assert!(!started.is_error, "{}", started.content);

        let listed = run("list_servers", &json!({}), &cwd);
        assert!(!listed.is_error, "{}", listed.content);
        let rows: Value = serde_json::from_str(&listed.content).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["name"], expected);
        assert_eq!(rows[0]["running"], true);

        let stopped = run("stop_server", &json!({"name": expected}), &cwd);
        assert!(!stopped.is_error, "{}", stopped.content);

        let listed = run("list_servers", &json!({}), &cwd);
        let rows: Value = serde_json::from_str(&listed.content).unwrap();
        assert!(rows.as_array().unwrap().is_empty());

        std::env::remove_var("NEXUS_HOME");
        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn run_wait_for_url_detects_ready_http_response() {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf);
            let body = "ready: buildwithnexus";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let out = run(
            "wait_for_url",
            &json!({
                "url": format!("http://{addr}/ready"),
                "timeout_seconds": 3,
                "expect_text": "buildwithnexus"
            }),
            Path::new("/tmp"),
        );
        handle.join().unwrap();
        assert!(!out.is_error, "{}", out.content);
        let value: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(value["status"], 200);
        assert_eq!(value["matched_text"], true);
    }

    // ── run: filesystem tools against a tempdir ─────────────────────────────
    fn tempdir() -> PathBuf {
        // Unique-enough path without external deps or Date/random (which the
        // harness forbids): use the test thread name + an atomic counter.
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let tn = std::thread::current()
            .name()
            .unwrap_or("t")
            .replace("::", "_");
        let dir = std::env::temp_dir().join(format!("bwn-test-{tn}-{id}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn run_write_then_read_roundtrip() {
        let d = tempdir();
        let w = run("write_file", &json!({"path": "f.txt", "content": "hi"}), &d);
        assert!(!w.is_error);
        let r = run("read_file", &json!({"path": "f.txt"}), &d);
        assert_eq!(r.content, "hi");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_write_creates_parent_dirs() {
        let d = tempdir();
        let w = run(
            "write_file",
            &json!({"path": "a/b/c.txt", "content": "x"}),
            &d,
        );
        assert!(!w.is_error);
        assert!(d.join("a/b/c.txt").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_read_missing_file_errors() {
        let d = tempdir();
        let r = run("read_file", &json!({"path": "nope.txt"}), &d);
        assert!(r.is_error);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_read_file_line_range() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.txt", "content": "one\ntwo\nthree\nfour"}),
            &d,
        );
        let r = run(
            "read_file",
            &json!({"path": "f.txt", "start_line": 2, "end_line": 3}),
            &d,
        );
        assert_eq!(r.content, "two\nthree");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_find_files_matches_glob_and_skips_heavy_dirs() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "src/main.rs", "content": ""}),
            &d,
        );
        run(
            "write_file",
            &json!({"path": "target/debug/skip.rs", "content": ""}),
            &d,
        );
        let r = run("find_files", &json!({"root": ".", "pattern": "*.rs"}), &d);
        assert!(r.content.contains("src/main.rs"), "{}", r.content);
        assert!(!r.content.contains("target/debug/skip.rs"), "{}", r.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_find_files_matches_vague_case_insensitive_names() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "notes/MyProjectsFile.MD", "content": "alpha"}),
            &d,
        );
        let r = run(
            "find_files",
            &json!({"root": ".", "pattern": "projects"}),
            &d,
        );
        assert!(
            r.content.contains("notes/MyProjectsFile.MD"),
            "{}",
            r.content
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_find_paths_can_find_directories() {
        let d = tempdir();
        run("create_dir", &json!({"path": "Projects/Nexus"}), &d);
        run(
            "write_file",
            &json!({"path": "Projects/Nexus/README.md", "content": "alpha"}),
            &d,
        );
        let r = run(
            "find_paths",
            &json!({"root": ".", "pattern": "nexus", "kind": "dir"}),
            &d,
        );
        assert!(r.content.contains("Projects/Nexus"), "{}", r.content);
        assert!(!r.content.contains("README.md"), "{}", r.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn resolve_expands_home_shorthand() {
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(resolve(Path::new("/tmp/work"), "~"), PathBuf::from(&home));
            assert_eq!(
                resolve(Path::new("/tmp/work"), "~/Documents"),
                PathBuf::from(home).join("Documents")
            );
        }
    }

    #[test]
    fn run_grep_files_returns_literal_path_line_matches() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "src/lib.rs", "content": "alpha\nNeedle\nbeta"}),
            &d,
        );
        run(
            "write_file",
            &json!({"path": "README.md", "content": "Needle"}),
            &d,
        );
        let r = run(
            "grep_files",
            &json!({"root": ".", "pattern": "needle", "file_pattern": "*.rs"}),
            &d,
        );
        assert!(r.content.contains("src/lib.rs:2:Needle"), "{}", r.content);
        assert!(!r.content.contains("README.md"), "{}", r.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_edit_unique_match() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.txt", "content": "alpha beta gamma"}),
            &d,
        );
        let e = run(
            "edit_file",
            &json!({"path": "f.txt", "old": "beta", "new": "BETA"}),
            &d,
        );
        assert!(!e.is_error);
        let r = run("read_file", &json!({"path": "f.txt"}), &d);
        assert_eq!(r.content, "alpha BETA gamma");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_edit_non_unique_match_errors() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.txt", "content": "x x"}),
            &d,
        );
        let e = run(
            "edit_file",
            &json!({"path": "f.txt", "old": "x", "new": "y"}),
            &d,
        );
        assert!(e.is_error);
        assert!(e.content.contains("not unique"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_edit_not_found_errors() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.txt", "content": "abc"}),
            &d,
        );
        let e = run(
            "edit_file",
            &json!({"path": "f.txt", "old": "zzz", "new": "y"}),
            &d,
        );
        assert!(e.is_error);
        assert!(e.content.contains("not found"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_edit_empty_old_errors() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.txt", "content": "abc"}),
            &d,
        );
        let e = run(
            "edit_file",
            &json!({"path": "f.txt", "old": "", "new": "y"}),
            &d,
        );
        assert!(e.is_error);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_tools_empty_input_errors() {
        let d = tempdir();
        assert!(run("read_file", &json!({"path": ""}), &d).is_error);
        assert!(run("write_file", &json!({"path": "", "content": "abc"}), &d).is_error);
        assert!(
            run(
                "edit_file",
                &json!({"path": "", "old": "a", "new": "b"}),
                &d
            )
            .is_error
        );
        assert!(
            run(
                "multi_edit",
                &json!({"path": "", "edits": [{"old": "a", "new": "b"}]}),
                &d
            )
            .is_error
        );
        assert!(run("multi_edit", &json!({"path": "f.txt", "edits": []}), &d).is_error);
        assert!(run("create_dir", &json!({"path": ""}), &d).is_error);
        assert!(run("move_path", &json!({"from": "", "to": "b"}), &d).is_error);
        assert!(run("move_path", &json!({"from": "a", "to": ""}), &d).is_error);
        assert!(run("remove_path", &json!({"path": ""}), &d).is_error);
        assert!(run("run_command", &json!({"command": ""}), &d).is_error);
        assert!(run("todo_write", &json!({"items": []}), &d).is_error);
        assert!(run("webfetch", &json!({"url": ""}), &d).is_error);
        assert!(run("websearch", &json!({"query": ""}), &d).is_error);
        assert!(run("headless_browser", &json!({"url": ""}), &d).is_error);
        assert!(run("kb_query", &json!({"query": ""}), &d).is_error);
        assert!(run("kb_record", &json!({"name": ""}), &d).is_error);
        assert!(run("file_info", &json!({"path": ""}), &d).is_error);
        assert!(run("file_info", &json!({"filePath": "   "}), &d).is_error);
        assert!(run("create_docx", &json!({"path": ""}), &d).is_error);
        assert!(run("create_docx", &json!({"filePath": "   "}), &d).is_error);
        assert!(!run("list_tree", &json!({"filePath": "."}), &d).is_error);
        assert!(!run("find_files", &json!({"dir": ".", "pattern": "*"}), &d).is_error);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_list_dir_sorted() {
        let d = tempdir();
        run("write_file", &json!({"path": "b.txt", "content": ""}), &d);
        run("write_file", &json!({"path": "a.txt", "content": ""}), &d);
        let r = run("list_dir", &json!({"path": "."}), &d);
        let lines: Vec<&str> = r.content.lines().collect();
        assert_eq!(lines, vec!["a.txt", "b.txt"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_command_captures_output_and_exit() {
        let d = tempdir();
        let r = run("run_command", &json!({"command": "echo hello"}), &d);
        assert!(!r.is_error);
        assert!(r.content.contains("hello"));
        assert!(r.content.contains("[exit 0]"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_command_nonzero_is_error() {
        let d = tempdir();
        let r = run("run_command", &json!({"command": "exit 3"}), &d);
        assert!(r.is_error);
        assert!(r.content.contains("[exit 3]"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_common_aliases_cover_file_search_shell_and_patch() {
        let d = tempdir();
        let w = run(
            "write",
            &json!({"filePath": "Projects/Nexus/README.md", "content": "alpha beta\n"}),
            &d,
        );
        assert!(!w.is_error, "{}", w.content);

        let r = run("read", &json!({"filePath": "Projects/Nexus/README.md"}), &d);
        assert_eq!(r.content, "alpha beta\n");

        let g = run(
            "glob",
            &json!({"root": ".", "pattern": "nexus", "kind": "dir"}),
            &d,
        );
        assert!(g.content.contains("Projects/Nexus"), "{}", g.content);

        let gr = run(
            "grep",
            &json!({"root": ".", "pattern": "beta", "include": "*.md"}),
            &d,
        );
        assert!(
            gr.content.contains("Projects/Nexus/README.md"),
            "{}",
            gr.content
        );

        let e = run(
            "edit",
            &json!({"filePath": "Projects/Nexus/README.md", "oldString": "beta", "newString": "gamma"}),
            &d,
        );
        assert!(!e.is_error, "{}", e.content);

        let b = run("bash", &json!({"command": "printf alias-ok"}), &d);
        assert!(!b.is_error, "{}", b.content);
        assert!(b.content.contains("alias-ok"), "{}", b.content);

        let p = run(
            "patch",
            &json!({"patch": "diff --git a/Projects/Nexus/README.md b/Projects/Nexus/README.md\n--- a/Projects/Nexus/README.md\n+++ b/Projects/Nexus/README.md\n@@ -1 +1 @@\n-alpha gamma\n+alpha delta\n"}),
            &d,
        );
        assert!(!p.is_error, "{}", p.content);
        let final_read = run("read", &json!({"path": "Projects/Nexus/README.md"}), &d);
        assert_eq!(final_read.content, "alpha delta\n");

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn python_tool_discovers_and_runs_json_stdin_script() {
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let d = tempdir();
        let script = r#"import json, sys
data = json.load(sys.stdin)
print("hello " + data.get("name", "world"))
"#;
        let w = run(
            "write_file",
            &json!({"path": ".buildwithnexus/tools/hello.py", "content": script}),
            &d,
        );
        assert!(!w.is_error, "{}", w.content);

        let listed = run("list_python_tools", &json!({}), &d);
        assert!(listed.content.contains("hello.py"), "{}", listed.content);

        let out = run(
            "python_tool",
            &json!({"path": "hello", "input": {"name": "nexus"}}),
            &d,
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("hello nexus"), "{}", out.content);

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_finish_sets_finished_flag() {
        let d = tempdir();
        let r = run("finish", &json!({"summary": "all done"}), &d);
        assert!(r.finished);
        assert_eq!(r.content, "all done");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_exit_plan_returns_plan_text() {
        let d = tempdir();
        let r = run(
            "exit_plan",
            &json!({"steps": ["Inspect the code.", "Implement the change."]}),
            &d,
        );
        assert!(!r.is_error, "{}", r.content);
        assert!(!r.finished);
        assert_eq!(r.content, "1. Inspect the code.\n2. Implement the change.");
        let alias = run("ExitPlanMode", &json!({"plan": "1. Verify behavior."}), &d);
        assert!(!alias.is_error, "{}", alias.content);
        assert_eq!(alias.content, "1. Verify behavior.");
        let both = run(
            "exit_plan",
            &json!({"plan": "Exit PLAN mode with a proposed implementation plan.", "steps": ["Inspect files.", "Apply fix."]}),
            &d,
        );
        assert!(!both.is_error, "{}", both.content);
        assert_eq!(both.content, "1. Inspect files.\n2. Apply fix.");
        let missing = run("exit_plan", &json!({}), &d);
        assert!(missing.is_error);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_unknown_tool_errors() {
        let d = tempdir();
        let r = run("bogus", &json!({}), &d);
        assert!(r.is_error);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_str_replace_editor_view_create_replace_insert_undo() {
        let d = tempdir();

        // 1. Create file via editor
        let r = run(
            "str_replace_editor",
            &json!({"command": "create", "path": "test.txt", "file_text": "line1\nline2\nline3"}),
            &d,
        );
        assert!(!r.is_error);
        assert!(d.join("test.txt").exists());

        // 2. View file via editor
        let r = run(
            "str_replace_editor",
            &json!({"command": "view", "path": "test.txt", "view_range": [1, 2]}),
            &d,
        );
        assert!(!r.is_error);
        assert!(r.content.contains("line1"));
        assert!(r.content.contains("line2"));
        assert!(!r.content.contains("line3"));

        // 3. View dir via editor
        let r = run(
            "str_replace_editor",
            &json!({"command": "view", "path": "."}),
            &d,
        );
        assert!(!r.is_error);
        assert!(r.content.contains("test.txt"));

        // 4. Replace text
        let r = run(
            "str_replace_editor",
            &json!({"command": "str_replace", "path": "test.txt", "old_str": "line2", "new_str": "line2-replaced"}),
            &d,
        );
        assert!(!r.is_error);
        let contents = fs::read_to_string(d.join("test.txt")).unwrap();
        assert!(contents.contains("line2-replaced"));

        // 5. Insert text (standard semantics: AFTER line `insert_line`)
        let r = run(
            "str_replace_editor",
            &json!({"command": "insert", "path": "test.txt", "insert_line": 2, "new_str": "inserted-line"}),
            &d,
        );
        assert!(!r.is_error);
        let contents = fs::read_to_string(d.join("test.txt")).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines[2], "inserted-line");

        // 6. Undo edit
        let r = run(
            "str_replace_editor",
            &json!({"command": "undo_edit", "path": "test.txt"}),
            &d,
        );
        assert!(!r.is_error);
        let contents = fs::read_to_string(d.join("test.txt")).unwrap();
        assert!(!contents.contains("inserted-line")); // undone to previous state

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_artifact_publishing() {
        let d = tempdir();
        let r = run(
            "Artifact",
            &json!({
                "title": "my-viz",
                "contents": "<h1>viz</h1>",
                "type": "html"
            }),
            &d,
        );
        assert!(r.is_error);
        assert!(r.content.contains("too small"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_artifact_rejects_external_canvas_game_shell() {
        let d = tempdir();
        let r = run(
            "Artifact",
            &json!({
                "title": "Canvas Game",
                "contents": "<!doctype html><canvas></canvas><script src='game.js'></script>",
                "type": "html"
            }),
            &d,
        );
        assert!(r.is_error);
        assert!(
            r.content.contains("too small")
                || r.content.contains("self-contained")
                || r.content.contains("requestAnimationFrame")
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_artifact_accepts_embedded_canvas_game() {
        let d = tempdir();
        let body = format!(
            "<!doctype html><html><head><style>{}</style></head><body><canvas id='game'></canvas><script>{}</script></body></html>",
            "html,body{margin:0;background:#111;color:white}canvas{display:block;width:100vw;height:100vh}",
            "const c=document.getElementById('game');const ctx=c.getContext('2d');function loop(){ctx.fillRect(0,0,10,10);requestAnimationFrame(loop)}loop();"
        );
        let r = run(
            "Artifact",
            &json!({
                "title": "Canvas Game",
                "contents": body,
                "type": "html"
            }),
            &d,
        );
        assert!(!r.is_error, "{}", r.content);
        assert!(r
            .content
            .contains("Artifact successfully published locally to"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_operational_judgment_tools() {
        let d = tempdir();
        let rec = run(
            "kb_record",
            &json!({
                "name": "AuthService",
                "entity_type": "Service",
                "description": "Authentication microservice",
                "path": "src/auth.rs"
            }),
            &d,
        );
        assert!(!rec.is_error, "{}", rec.content);
        assert!(rec
            .content
            .contains("Successfully recorded knowledge entity"));

        let q = run("kb_query", &json!({"query": "AuthService"}), &d);
        assert!(!q.is_error, "{}", q.content);
        assert!(q.content.contains("AuthService"));

        let rc = run("rule_check", &json!({"changed_files": ["src/auth.rs"]}), &d);
        assert!(!rc.is_error, "{}", rc.content);

        let ver = run(
            "verify",
            &json!({"task_description": "Add auth service", "changed_files": ["src/auth.rs"]}),
            &d,
        );
        assert!(!ver.is_error, "{}", ver.content);
        assert!(ver.content.contains("## Verification:"));

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn test_mcp_call_missing_server() {
        let d = tempdir();
        let res = run(
            "mcp_call",
            &json!({"server": "nonexistent_server", "tool": "test_tool"}),
            &d,
        );
        assert!(res.is_error);
        assert!(res
            .content
            .contains("not found in settings.json mcp_servers"));
        let _ = fs::remove_dir_all(&d);
    }

    // A complete-enough HTML app body (>300 chars, no placeholders) that new
    // artifact tests can extend with the pattern under test.
    fn html_app(extra: &str) -> String {
        format!(
            "<!doctype html><html><head><style>body{{margin:0;font:16px sans-serif;background:#fafafa;color:#222}}main{{max-width:640px;margin:2rem auto;padding:1rem}}</style></head><body><main><h1>App</h1><div id='root'></div></main><script>const root=document.getElementById('root');function render(items){{root.textContent=items.join(', ')}}render(['a','b','c']);{extra}</script></body></html>"
        )
    }

    // ── artifact validation (placeholders, sniffing, script src) ────────────
    #[test]
    fn artifact_accepts_spread_loading_and_todo_apps() {
        let d = tempdir();
        // JS spread syntax, a "Loading..." string, and todo-app vocabulary are
        // all legal content and must not be rejected as placeholders.
        let body = html_app(
            "function log(...args){console.log(...args)}document.title='Loading...';const todos=['todo one','todo two'];render(todos);",
        );
        let r = run(
            "Artifact",
            &json!({"title": "Todo App", "contents": body, "type": "html"}),
            &d,
        );
        assert!(!r.is_error, "{}", r.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn artifact_rejects_comment_anchored_todo_with_snippet() {
        let d = tempdir();
        let body = html_app("// TODO: implement the save handler");
        let r = run(
            "Artifact",
            &json!({"title": "Notes App", "contents": body, "type": "html"}),
            &d,
        );
        assert!(r.is_error);
        // The rejection must quote the rule and the offending snippet.
        assert!(r.content.contains("// todo"), "{}", r.content);
        assert!(
            r.content.contains("implement the save handler"),
            "{}",
            r.content
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn artifact_without_type_sniffs_markdown_and_skips_html_rules() {
        let d = tempdir();
        // Short markdown with no `type` used to default to HTML rules and get
        // rejected as "too small".
        let r = run(
            "Artifact",
            &json!({"title": "Todo Plan", "contents": "# Todo App Plan\n\n- model\n- view\n"}),
            &d,
        );
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains(".md"), "{}", r.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn artifact_rejects_local_script_src_even_with_https_elsewhere() {
        let d = tempdir();
        // The old check waved through `<script src="app.js">` whenever
        // "https://" appeared anywhere in the document.
        let body = html_app("fetch('https://api.example.com/data');")
            .replace("</body>", "<script src=\"app.js\"></script></body>");
        let r = run(
            "Artifact",
            &json!({"title": "Dashboard", "contents": body, "type": "html"}),
            &d,
        );
        assert!(r.is_error);
        assert!(r.content.contains("app.js"), "{}", r.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn artifact_rejects_local_stylesheet_link() {
        let d = tempdir();
        // A 1.5B model's canvas-game stub linked <link rel=stylesheet href=styles.css>;
        // it must be rejected with a message naming the file and telling the
        // model to inline the CSS.
        let body = html_app("draw();").replace(
            "<head>",
            "<head><link rel=\"stylesheet\" href=\"styles.css\">",
        );
        let r = run(
            "Artifact",
            &json!({"title": "Game", "contents": body, "type": "html"}),
            &d,
        );
        assert!(r.is_error);
        assert!(r.content.contains("styles.css"), "{}", r.content);
        assert!(r.content.contains("<style"), "{}", r.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn artifact_allows_external_stylesheet_link() {
        let d = tempdir();
        let body = html_app("draw();").replace(
            "<head>",
            "<head><link rel=\"stylesheet\" href=\"https://cdn.example.com/x.css\">",
        );
        let r = run(
            "Artifact",
            &json!({"title": "Game", "contents": body, "type": "html"}),
            &d,
        );
        assert!(!r.is_error, "{}", r.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn is_local_asset_classifies_paths() {
        assert!(is_local_asset("styles.css"));
        assert!(is_local_asset("./css/app.css"));
        assert!(!is_local_asset("https://x.com/a.css"));
        assert!(!is_local_asset("//cdn/a.css"));
        assert!(!is_local_asset("data:text/css,body{}"));
        assert!(!is_local_asset(""));
    }

    #[test]
    fn artifact_allows_external_script_src() {
        let d = tempdir();
        let body = html_app("").replace(
            "</body>",
            "<script src=\"https://cdn.example.com/lib.js\"></script></body>",
        );
        let r = run(
            "Artifact",
            &json!({"title": "Widget", "contents": body, "type": "html"}),
            &d,
        );
        assert!(!r.is_error, "{}", r.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn artifact_game_heuristic_warns_instead_of_rejecting() {
        let d = tempdir();
        // A "game" title without canvas/requestAnimationFrame publishes with a
        // warning rather than being rejected (DOM/text games are legitimate).
        let body = html_app("document.addEventListener('keydown',e=>render([e.key]));");
        let r = run(
            "Artifact",
            &json!({"title": "Word Game", "contents": body, "type": "html"}),
            &d,
        );
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("warning"), "{}", r.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn artifact_small_html_rejection_states_threshold() {
        let d = tempdir();
        let r = run(
            "Artifact",
            &json!({"title": "tiny", "contents": "<!doctype html><p>x</p>", "type": "html"}),
            &d,
        );
        assert!(r.is_error);
        assert!(r.content.contains("too small"));
        assert!(r.content.contains("300"), "{}", r.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn mark_artifact_opened_only_first_time() {
        assert!(mark_artifact_opened("unique-open-test-name"));
        assert!(!mark_artifact_opened("unique-open-test-name"));
    }

    // ── head+tail truncation ────────────────────────────────────────────────
    #[test]
    fn truncate_head_tail_preserves_exit_marker() {
        let mut s = String::from("START-OF-OUTPUT\n");
        s.push_str(&"x".repeat(40 * 1024));
        s.push_str("\nfinal failure line\n[exit 42]");
        let out = truncate_head_tail(s, MAX_OUT);
        assert!(out.starts_with("START-OF-OUTPUT"));
        assert!(out.contains("bytes omitted"));
        assert!(out.ends_with("[exit 42]"), "tail must survive");
        assert!(out.contains("final failure line"));
    }

    #[test]
    fn truncate_head_tail_short_input_unchanged() {
        assert_eq!(truncate_head_tail("abc".into(), 16), "abc");
    }

    #[test]
    fn truncate_read_marker_names_totals_and_line_ranges() {
        let s = "line\n".repeat(30 * 1024); // 150 KB
        let total_bytes = s.len();
        let out = truncate_read(s, MAX_READ);
        assert!(out.contains("start_line"));
        assert!(out.contains(&format!("{total_bytes} bytes")));
        assert!(out.contains("lines total"));
    }

    #[test]
    fn clip_line_marks_cut() {
        let long = "y".repeat(600);
        let out = clip_line(&long, MAX_MATCH_LINE);
        assert!(out.contains("[line truncated]"));
        assert!(out.len() < long.len());
        assert_eq!(clip_line("short", MAX_MATCH_LINE), "short");
    }

    // ── command timeouts ────────────────────────────────────────────────────
    #[test]
    fn command_outcome_reports_timeout_actionably() {
        let cap = CommandCapture {
            stdout: "partial output".into(),
            stderr: String::new(),
            code: None,
            timed_out: true,
        };
        let o = command_outcome(cap, COMMAND_TIMEOUT);
        assert!(o.is_error);
        assert!(o.content.contains("partial output"));
        assert!(o.content.contains("timed out after 120s"));
        assert!(o.content.contains("start_server"));
    }

    #[test]
    fn command_outcome_appends_exit_marker() {
        let cap = CommandCapture {
            stdout: "z".repeat(40 * 1024),
            stderr: String::new(),
            code: Some(3),
            timed_out: false,
        };
        let o = command_outcome(cap, COMMAND_TIMEOUT);
        assert!(o.is_error);
        assert!(o.content.ends_with("[exit 3]"));
        assert!(o.content.contains("bytes omitted"));
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_kills_hanging_command_and_keeps_partial_output() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo started; exec sleep 30"]);
        let start = std::time::Instant::now();
        let cap = run_with_timeout(cmd, None, Duration::from_millis(300)).unwrap();
        assert!(cap.timed_out);
        assert!(cap.stdout.contains("started"), "{}", cap.stdout);
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    // ── readonly command classification ─────────────────────────────────────
    #[test]
    fn readonly_command_rejects_shell_composition_bypass() {
        for c in [
            "cat x; rm -rf ~",
            "ls && rm -rf /",
            "grep foo f || rm f",
            "cat f | sh",
            "cat f > out",
            "sort < f",
            "echo `rm x`",
            "cat $(mktemp)",
        ] {
            assert!(!is_readonly_command(c), "{c} must not be readonly");
        }
    }

    #[test]
    fn readonly_command_excludes_mutating_lookalikes() {
        for c in [
            "sed -i s/a/b/ f.txt",
            "sed s/a/b/ f.txt",
            "find . -delete",
            "find . -name '*.log' -exec rm {} +",
            "git clean -fd",
            "git branch -D main",
            "git branch -m old new",
            "git tag v1.0",
            "git push origin main",
            "xargs rm",
            "tee out.txt",
        ] {
            assert!(!is_readonly_command(c), "{c} must not be readonly");
        }
    }

    #[test]
    fn readonly_command_allows_genuine_reads() {
        for c in [
            "grep -rn pattern src",
            "rg pattern",
            "cat README.md",
            "ls -la",
            "find . -name '*.rs'",
            "git status",
            "git log --oneline -5",
            "git diff HEAD~1",
            "git show abc123",
            "git blame src/main.rs",
            "git remote -v",
            "git branch",
        ] {
            assert!(is_readonly_command(c), "{c} should be readonly");
        }
    }

    // ── catastrophic additions ──────────────────────────────────────────────
    #[test]
    fn catastrophic_home_star_and_dot_targets() {
        for c in [
            "rm -rf ~",
            "rm -rf ~/",
            "rm -fr ~",
            "rm -rf *",
            "rm -rf .",
            "rm -rf ..",
            "rm -r -f /",
            "rm --recursive --force /",
        ] {
            assert!(catastrophic(c), "{c} should be catastrophic");
        }
    }

    #[test]
    fn catastrophic_still_allows_scoped_rm() {
        for c in ["rm -rf ./build", "rm -rf build", "rm -r tmp"] {
            assert!(!catastrophic(c), "{c} should be allowed");
        }
    }

    // ── cwd confinement ─────────────────────────────────────────────────────
    #[test]
    fn out_of_cwd_mutation_flags_writes_not_reads() {
        let cwd = Path::new("/proj/work");
        assert!(out_of_cwd_mutation(
            "write_file",
            &json!({"path": "/etc/cron.d/evil", "content": "x"}),
            cwd
        )
        .is_some());
        assert!(out_of_cwd_mutation(
            "write_file",
            &json!({"path": "src/main.rs", "content": "x"}),
            cwd
        )
        .is_none());
        assert!(out_of_cwd_mutation("read_file", &json!({"path": "/etc/passwd"}), cwd).is_none());
        assert!(out_of_cwd_mutation(
            "move_path",
            &json!({"from": "a.txt", "to": "/tmp-elsewhere/b.txt"}),
            cwd
        )
        .is_some());
        assert!(out_of_cwd_mutation(
            "str_replace_editor",
            &json!({"command": "view", "path": "/etc/hosts"}),
            cwd
        )
        .is_none());
    }

    #[test]
    fn run_refuses_out_of_cwd_write() {
        let d = tempdir();
        let outside = std::env::temp_dir().join("bwn-outside-target.txt");
        let _ = fs::remove_file(&outside);
        let r = run(
            "write_file",
            &json!({"path": outside.to_string_lossy(), "content": "x"}),
            &d,
        );
        assert!(r.is_error, "{}", r.content);
        assert!(r.content.contains("outside the working directory"));
        assert!(!outside.exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn escapes_cwd_handles_symlinked_tmp_prefix() {
        // /tmp is a symlink to /private/tmp on macOS; canonicalizing only the
        // base used to misclassify in-cwd targets as escaping.
        assert!(!escapes_cwd(Path::new("/tmp/x/f.txt"), Path::new("/tmp/x")));
        assert!(escapes_cwd(
            Path::new("/private/etc/hosts"),
            Path::new("/tmp/x")
        ));
    }

    // ── write safety ────────────────────────────────────────────────────────
    #[test]
    fn run_write_missing_content_errors_without_writing() {
        let d = tempdir();
        let r = run("write_file", &json!({"path": "f.txt"}), &d);
        assert!(r.is_error);
        assert!(r.content.contains("missing required param `content`"));
        assert!(r.content.contains("NOT written"));
        assert!(!d.join("f.txt").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_write_refuses_emptying_nonempty_file() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.txt", "content": "important data"}),
            &d,
        );
        let r = run("write_file", &json!({"path": "f.txt", "content": ""}), &d);
        assert!(r.is_error);
        assert!(r.content.contains("refusing to overwrite"), "{}", r.content);
        assert_eq!(
            fs::read_to_string(d.join("f.txt")).unwrap(),
            "important data"
        );
        // Explicit empty content on a NEW file is still fine.
        let ok_new = run(
            "write_file",
            &json!({"path": "empty.txt", "content": ""}),
            &d,
        );
        assert!(!ok_new.is_error, "{}", ok_new.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn editor_create_missing_file_text_errors() {
        let d = tempdir();
        let r = run(
            "str_replace_editor",
            &json!({"command": "create", "path": "f.txt"}),
            &d,
        );
        assert!(r.is_error);
        assert!(r.content.contains("missing required param `file_text`"));
        assert!(!d.join("f.txt").exists());
        let _ = fs::remove_dir_all(&d);
    }

    // ── insert semantics ────────────────────────────────────────────────────
    #[test]
    fn editor_insert_line_zero_inserts_at_top_without_panic() {
        let d = tempdir();
        run(
            "str_replace_editor",
            &json!({"command": "create", "path": "f.txt", "file_text": "a\nb\n"}),
            &d,
        );
        let r = run(
            "str_replace_editor",
            &json!({"command": "insert", "path": "f.txt", "insert_line": 0, "new_str": "top"}),
            &d,
        );
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(fs::read_to_string(d.join("f.txt")).unwrap(), "top\na\nb\n");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn editor_insert_preserves_crlf() {
        let d = tempdir();
        run(
            "str_replace_editor",
            &json!({"command": "create", "path": "f.txt", "file_text": "a\r\nb\r\n"}),
            &d,
        );
        let r = run(
            "str_replace_editor",
            &json!({"command": "insert", "path": "f.txt", "insert_line": 1, "new_str": "mid"}),
            &d,
        );
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            fs::read_to_string(d.join("f.txt")).unwrap(),
            "a\r\nmid\r\nb\r\n"
        );
        let _ = fs::remove_dir_all(&d);
    }

    // ── edit feedback ───────────────────────────────────────────────────────
    #[test]
    fn run_edit_not_found_reports_closest_line_hint() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.rs", "content": "fn main() {\n    let x = 1;\n}\n"}),
            &d,
        );
        let e = run(
            "edit_file",
            &json!({"path": "f.rs", "old": "\tlet x = 1;", "new": "\tlet x = 2;"}),
            &d,
        );
        assert!(e.is_error);
        assert!(e.content.contains("line 2"), "{}", e.content);
        assert!(e.content.contains("re-read"), "{}", e.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_edit_not_unique_reports_count() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.txt", "content": "x y x y x"}),
            &d,
        );
        let e = run(
            "edit_file",
            &json!({"path": "f.txt", "old": "x", "new": "z"}),
            &d,
        );
        assert!(e.is_error);
        assert!(e.content.contains("3 times"), "{}", e.content);
        let _ = fs::remove_dir_all(&d);
    }

    // ── tiered lenient edit apply ───────────────────────────────────────────
    #[test]
    fn run_edit_lenient_rstrip_tier_applies() {
        let d = tempdir();
        // The file carries trailing whitespace the model's copy lacks.
        run(
            "write_file",
            &json!({"path": "f.rs", "content": "fn main() {   \n    let x = 1;\t\n}\n"}),
            &d,
        );
        let e = run(
            "edit_file",
            &json!({
                "path": "f.rs",
                "old": "fn main() {\n    let x = 1;\n}",
                "new": "fn main() {\n    let x = 2;\n}"
            }),
            &d,
        );
        assert!(!e.is_error, "{}", e.content);
        assert!(e.content.contains("whitespace tolerance"), "{}", e.content);
        assert_eq!(
            fs::read_to_string(d.join("f.rs")).unwrap(),
            "fn main() {\n    let x = 2;\n}\n"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_edit_lenient_indent_shift_applies_and_reindents() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.rs", "content": "mod m {\n    fn f() {\n        one();\n    }\n}\n"}),
            &d,
        );
        // The model re-sent the block dedented by four spaces; the edit must
        // apply and the replacement must land at the file's real depth.
        let e = run(
            "edit_file",
            &json!({
                "path": "f.rs",
                "old": "fn f() {\n    one();\n}",
                "new": "fn f() {\n    two();\n}"
            }),
            &d,
        );
        assert!(!e.is_error, "{}", e.content);
        assert!(e.content.contains("whitespace tolerance"), "{}", e.content);
        assert_eq!(
            fs::read_to_string(d.join("f.rs")).unwrap(),
            "mod m {\n    fn f() {\n        two();\n    }\n}\n"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_edit_lenient_over_indented_old_strips_replacement() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.rs", "content": "a();\nb();\nc();\n"}),
            &d,
        );
        // The model's copy is deeper than the file; the delta is stripped
        // from the replacement on the way back in.
        let e = run(
            "edit_file",
            &json!({"path": "f.rs", "old": "    b();\n    c();", "new": "    d();"}),
            &d,
        );
        assert!(!e.is_error, "{}", e.content);
        assert_eq!(fs::read_to_string(d.join("f.rs")).unwrap(), "a();\nd();\n");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_edit_lenient_non_unique_match_still_errors() {
        let d = tempdir();
        // Two regions match after trailing-whitespace trim — too ambiguous.
        let content = "a();   \nb();\na();\t\nb();\n";
        run(
            "write_file",
            &json!({"path": "f.rs", "content": content}),
            &d,
        );
        let e = run(
            "edit_file",
            &json!({"path": "f.rs", "old": "a();\nb();", "new": "c();"}),
            &d,
        );
        assert!(e.is_error, "{}", e.content);
        assert!(e.content.contains("not found"), "{}", e.content);
        assert_eq!(fs::read_to_string(d.join("f.rs")).unwrap(), content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_edit_lenient_absent_text_still_errors_with_hint() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.rs", "content": "fn main() {}\n"}),
            &d,
        );
        let e = run(
            "edit_file",
            &json!({"path": "f.rs", "old": "fn missing()\nnowhere();", "new": "x"}),
            &d,
        );
        assert!(e.is_error, "{}", e.content);
        assert!(e.content.contains("not found"), "{}", e.content);
        assert!(e.content.contains("re-read"), "{}", e.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn run_edit_lenient_ignores_tab_for_space_swap() {
        let d = tempdir();
        // A tab-for-space substitution is not a uniform indent shift; it must
        // stay diagnostic-only rather than auto-apply.
        let content = "fn main() {\n    let x = 1;\n}\n";
        run(
            "write_file",
            &json!({"path": "f.rs", "content": content}),
            &d,
        );
        let e = run(
            "edit_file",
            &json!({"path": "f.rs", "old": "fn main() {\n\tlet x = 1;\n}", "new": "y"}),
            &d,
        );
        assert!(e.is_error, "{}", e.content);
        assert_eq!(fs::read_to_string(d.join("f.rs")).unwrap(), content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn multi_edit_lenient_match_applies_with_note() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.txt", "content": "alpha   \nbeta\ngamma\n"}),
            &d,
        );
        let e = run(
            "multi_edit",
            &json!({"path": "f.txt", "edits": [
                {"old": "alpha\nbeta", "new": "ALPHA\nBETA"},
                {"old": "gamma", "new": "GAMMA"},
            ]}),
            &d,
        );
        assert!(!e.is_error, "{}", e.content);
        assert!(e.content.contains("2 replacements"), "{}", e.content);
        assert!(
            e.content.contains("1 matched with whitespace tolerance"),
            "{}",
            e.content
        );
        assert_eq!(
            fs::read_to_string(d.join("f.txt")).unwrap(),
            "ALPHA\nBETA\nGAMMA\n"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn str_replace_lenient_indent_shift_applies() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.py", "content": "class A:\n    def f(self):\n        return 1\n"}),
            &d,
        );
        let e = run(
            "str_replace_editor",
            &json!({
                "command": "str_replace",
                "path": "f.py",
                "old_str": "def f(self):\n    return 1",
                "new_str": "def f(self):\n    return 2"
            }),
            &d,
        );
        assert!(!e.is_error, "{}", e.content);
        assert!(e.content.contains("whitespace tolerance"), "{}", e.content);
        assert_eq!(
            fs::read_to_string(d.join("f.py")).unwrap(),
            "class A:\n    def f(self):\n        return 2\n"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn multi_edit_failure_names_edit_index() {
        let d = tempdir();
        run(
            "write_file",
            &json!({"path": "f.txt", "content": "alpha\nbeta\ngamma\n"}),
            &d,
        );
        let e = run(
            "multi_edit",
            &json!({"path": "f.txt", "edits": [
                {"old": "alpha", "new": "ALPHA"},
                {"old": "missing", "new": "x"},
            ]}),
            &d,
        );
        assert!(e.is_error);
        assert!(e.content.contains("edit #2 of 2"), "{}", e.content);
        assert!(e.content.contains("earlier edits"), "{}", e.content);
        // Nothing was written: edit #1 must not have been applied.
        assert!(fs::read_to_string(d.join("f.txt"))
            .unwrap()
            .contains("alpha"));
        let _ = fs::remove_dir_all(&d);
    }

    // ── unknown tool ────────────────────────────────────────────────────────
    #[test]
    fn unknown_tool_suggests_nearest_and_lists_valid() {
        let d = tempdir();
        let r = run("red_file", &json!({}), &d);
        assert!(r.is_error);
        assert!(
            r.content.contains("Did you mean `read_file`?"),
            "{}",
            r.content
        );
        assert!(r.content.contains("Valid tools:"), "{}", r.content);
        assert!(r.content.contains("write_file"));
        let _ = fs::remove_dir_all(&d);
    }

    // ── grep output caps ────────────────────────────────────────────────────
    #[test]
    fn grep_caps_long_lines_and_reports_hidden_matches() {
        let d = tempdir();
        let long_line = format!("needle {}", "z".repeat(700));
        let many = ["needle here"; 12].join("\n");
        run(
            "write_file",
            &json!({"path": "long.txt", "content": long_line}),
            &d,
        );
        let clipped = run("grep_files", &json!({"pattern": "needle", "max": 500}), &d);
        assert!(
            clipped.content.contains("[line truncated]"),
            "{}",
            clipped.content
        );
        run(
            "write_file",
            &json!({"path": "many.txt", "content": many}),
            &d,
        );
        let capped = run("grep_files", &json!({"pattern": "needle", "max": 5}), &d);
        assert!(
            capped.content.contains("13 matches, showing 5"),
            "{}",
            capped.content
        );
        let _ = fs::remove_dir_all(&d);
    }

    // ── read error kinds ────────────────────────────────────────────────────
    #[test]
    fn read_binary_file_suggests_extractor() {
        let d = tempdir();
        fs::write(d.join("bin.dat"), [0xffu8, 0xfe, 0x00, 0x9f, 0x11]).unwrap();
        let r = run("read_file", &json!({"path": "bin.dat"}), &d);
        assert!(r.is_error);
        assert!(r.content.contains("binary"), "{}", r.content);
        assert!(r.content.contains("run_command"), "{}", r.content);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn read_missing_file_keeps_recovery_text() {
        let d = tempdir();
        let r = run("read_file", &json!({"path": "nope.txt"}), &d);
        assert!(r.is_error);
        assert!(r.content.contains("do not invent another path"));
        let _ = fs::remove_dir_all(&d);
    }

    // ── kb timestamps ───────────────────────────────────────────────────────
    #[test]
    fn iso8601_known_values() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(iso8601_utc(951_868_800), "2000-03-01T00:00:00Z"); // leap-year boundary
        let now = iso8601_utc_now();
        assert_ne!(now, "2026-07-06T00:00:00Z".to_string());
        assert!(now.ends_with('Z') && now.contains('T'));
    }
}
