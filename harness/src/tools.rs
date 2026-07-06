// The tool surface the agent can call. Definitions are data; execution is a
// single `match` — no registry, no dyn dispatch (Casey: don't build the plugin
// system before there are plugins).

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

pub fn defs_for_context(include_subagent: bool, context_tokens: usize) -> Vec<ToolDef> {
    let all = defs(include_subagent);
    if context_tokens > 8192 {
        return all;
    }
    all.into_iter().filter(|d| compact_tool(d.name)).collect()
}

fn compact_tool(name: &str) -> bool {
    matches!(
        name,
        "bash"
            | "read"
            | "write"
            | "edit"
            | "patch"
            | "glob"
            | "grep"
            | "list"
            | "webfetch"
            | "todowrite"
            | "todoread"
            | "skill"
            | "question"
            | "str_replace_editor"
            | "Artifact"
            | "publish_artifact"
            | "list_python_tools"
            | "python_tool"
            | "read_file"
            | "read_many_files"
            | "list_dir"
            | "list_tree"
            | "file_info"
            | "find_paths"
            | "find_files"
            | "grep_files"
            | "write_file"
            | "edit_file"
            | "multi_edit"
            | "apply_patch"
            | "create_dir"
            | "move_path"
            | "remove_path"
            | "run_command"
            | "todo_write"
            | "todo_read"
            | "create_docx"
            | "finish"
            | "save_memory"
            | "fetch_url"
            | "start_server"
            | "list_servers"
            | "stop_server"
            | "read_server_log"
            | "wait_for_url"
            | "open_browser"
            | "list_skills"
            | "load_skill"
            | "task"
            | "spawn_subagent"
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
    let lower = cmd.trim().to_lowercase();
    let first = lower.split_whitespace().next().unwrap_or("");
    let base = first.rsplit('/').next().unwrap_or(first);
    matches!(
        base,
        "grep"
            | "egrep"
            | "fgrep"
            | "rg"
            | "find"
            | "cat"
            | "ls"
            | "head"
            | "tail"
            | "wc"
            | "sort"
            | "uniq"
            | "diff"
            | "tree"
            | "stat"
            | "file"
            | "jq"
            | "sed"
    ) || lower.starts_with("git log")
        || lower.starts_with("git status")
        || lower.starts_with("git diff")
        || lower.starts_with("git show")
        || lower.starts_with("git branch")
        || lower.starts_with("git tag")
        || lower.starts_with("git remote")
}

// A one-line, human-readable preview of what a call will do (shown at the gate).
pub fn preview(name: &str, input: &Value) -> String {
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
    input["path"]
        .as_str()
        .or_else(|| input["filePath"].as_str())
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
    input["root"]
        .as_str()
        .or_else(|| input["path"].as_str())
        .unwrap_or(".")
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

fn html_artifact_quality_error(title: &str, contents: &str) -> Option<String> {
    let title_lower = title.to_lowercase();
    let lower = contents.to_lowercase();
    if contents.trim().len() < 300 {
        return Some("HTML artifact is too small to be a complete runnable app; include full HTML, CSS, and JavaScript in the artifact contents".to_string());
    }
    for marker in [
        "todo",
        "placeholder",
        "your code here",
        "canvas game logic here",
        "...",
    ] {
        if lower.contains(marker) {
            return Some(format!(
                "HTML artifact contains placeholder marker '{marker}'; provide complete implemented code"
            ));
        }
    }
    if lower.contains("<script src=") && !(lower.contains("https://") || lower.contains("http://"))
    {
        return Some("HTML artifact references a local external script; embed the JavaScript so the artifact is self-contained".to_string());
    }
    if title_lower.contains("game") || title_lower.contains("canvas") || lower.contains("<canvas") {
        if !lower.contains("<canvas") {
            return Some("canvas game artifact must include a <canvas> element".to_string());
        }
        if !lower.contains("requestanimationframe") {
            return Some(
                "canvas game artifact must include a requestAnimationFrame game loop".to_string(),
            );
        }
        if lower.contains("<script src=") {
            return Some("canvas game artifact must embed its game script instead of referencing a separate script file".to_string());
        }
    }
    None
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

// True if the path resolves outside the working directory.
pub fn escapes_cwd(p: &Path, cwd: &Path) -> bool {
    let base = cwd.canonicalize().unwrap_or_else(|_| normalize(cwd));
    !normalize(p).starts_with(&base)
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

    // Collapse whitespace and decode common HTML entities.
    let decoded = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&nbsp;", " ")
        .replace("&#39;", "'");

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

// Commands so destructive they require confirmation in every mode.
pub fn catastrophic(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    let nospace: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    nospace.contains("rm-rf/")          // rm -rf of an absolute path
        || nospace.contains("rm-fr/")
        || nospace.contains(":(){:|:&};:") // fork bomb
        || nospace.contains("mkfs")
        || nospace.contains("of=/dev/")    // dd onto a device
        || nospace.contains(">/dev/sd")
        || nospace.contains(">/dev/nvme")
        || nospace.contains("chmod-r777/")
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
    let style = style
        .map(|s| format!("<w:pPr><w:pStyle w:val=\"{s}\"/></w:pPr>"))
        .unwrap_or_default();
    format!(
        "<w:p>{style}<w:r><w:t>{}</w:t></w:r></w:p>",
        xml_escape(text)
    )
}

fn docx_document(title: &str, body: &str) -> String {
    let mut out = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
    );
    out.push_str(&docx_paragraph(title, Some("Title")));
    for line in body.lines() {
        let trimmed = line.trim();
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

pub fn run(name: &str, input: &Value, cwd: &Path) -> Outcome {
    match name {
        "read" | "read_file" => {
            let p_str = path_arg(input).unwrap_or("").trim();
            if p_str.is_empty() {
                return err("path argument is required and cannot be empty");
            }
            let p = resolve(cwd, p_str);
            match fs::read_to_string(&p) {
                Ok(c) => ok(truncate(apply_line_range(&c, line_range(input)), MAX_READ)),
                Err(e) => err(format!(
                    "cannot read {}: {e}\nrecovery: do not invent another path or ask the user immediately. Use list_tree/find_paths/find_files/grep_files to locate likely files; for folders use find_paths kind=`dir`; for personal files try roots like `~`, `~/Documents`, `~/Desktop`, `~/Downloads`, `~/Projects`, and `~/repos`. If a broader search is needed, propose or call a read-only find/rg command.",
                    p.display()
                )),
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
                    ok(names.join("\n"))
                }
                Err(e) => err(format!(
                    "cannot list {}: {e}\nrecovery: do not invent another path or ask the user immediately. Use list_tree/find_paths/find_files/grep_files to locate likely files; for folders use find_paths kind=`dir`; for personal files try roots like `~`, `~/Documents`, `~/Desktop`, `~/Downloads`, `~/Projects`, and `~/repos`. If a broader search is needed, propose or call a read-only find/rg command.",
                    p.display()
                )),
            }
        }
        "list_tree" => {
            let root = resolve(cwd, input["path"].as_str().unwrap_or("."));
            let max_depth = input["max_depth"].as_u64().unwrap_or(3).clamp(1, 8) as usize;
            let max_entries = input["max_entries"].as_u64().unwrap_or(200).clamp(1, 1000) as usize;
            let mut entries = Vec::new();
            collect_tree(&root, cwd, 1, max_depth, &mut entries, max_entries);
            if entries.is_empty() {
                ok(format!("no entries under {}", root.display()))
            } else {
                ok(entries.join("\n"))
            }
        }
        "file_info" => {
            let p_str = input["path"].as_str().unwrap_or("").trim();
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
            let root = resolve(cwd, input["root"].as_str().unwrap_or("."));
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
            for path in files {
                if matches.len() >= limit {
                    break;
                }
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
                        matches.push(format!("{}:{}:{}", rel, idx + 1, line.trim_end()));
                        if matches.len() >= limit {
                            break;
                        }
                    }
                }
            }
            if matches.is_empty() {
                ok(format!("no matches for {pattern}"))
            } else {
                ok(matches.join("\n"))
            }
        }
        "write" | "write_file" => {
            let p_str = path_arg(input).unwrap_or("").trim();
            if p_str.is_empty() {
                return err("path argument is required and cannot be empty");
            }
            let p = resolve(cwd, p_str);
            let content = input["content"].as_str().unwrap_or("");
            if let Some(dir) = p.parent() {
                let _ = fs::create_dir_all(dir);
            }
            checkpoint::record(cwd, &p, "write_file");
            match fs::write(&p, content) {
                Ok(_) => ok(format!("wrote {} ({} bytes)", p.display(), content.len())),
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
                return err("`old` text not found in file");
            }
            // One pass to locate, a partial pass to confirm uniqueness — instead
            // of matches().count() (materializes all) plus a separate replace.
            let Some(first) = body.find(old) else {
                return err("`old` text not found in file");
            };
            if body[first + old.len()..].contains(old) {
                return err("`old` text is not unique — add surrounding context");
            }
            checkpoint::record(cwd, &p, "edit_file");
            match fs::write(&p, body.replacen(old, new, 1)) {
                Ok(_) => ok(format!("edited {}", p.display())),
                Err(e) => err(format!("cannot write {}: {e}", p.display())),
            }
        }
        "multi_edit" => {
            let p_str = input["path"].as_str().unwrap_or("").trim();
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
            for edit in edits {
                let old = edit["old"].as_str().unwrap_or("");
                if old.is_empty() {
                    return err("old text cannot be empty");
                }
                let count = body.matches(old).count();
                if count == 0 {
                    return err("old text not found");
                }
                if count > 1 {
                    return err("old text is not unique — add surrounding context");
                }
                let new = edit["new"].as_str().unwrap_or("");
                body = body.replacen(old, new, 1);
            }
            checkpoint::record(cwd, &p, "multi_edit");
            match fs::write(&p, body) {
                Ok(_) => ok(format!(
                    "edited {} with {} replacements",
                    p.display(),
                    edits.len()
                )),
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
            let p_str = input["path"].as_str().unwrap_or("").trim();
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
            let p_str = input["path"].as_str().unwrap_or("").trim();
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
            let output = if cfg!(windows) {
                Command::new("cmd")
                    .args(["/C", cmd])
                    .current_dir(cwd)
                    .output()
            } else {
                Command::new("sh")
                    .args(["-c", cmd])
                    .current_dir(cwd)
                    .output()
            };
            match output {
                Ok(o) => {
                    let mut s = String::new();
                    s.push_str(&String::from_utf8_lossy(&o.stdout));
                    let e = String::from_utf8_lossy(&o.stderr);
                    if !e.trim().is_empty() {
                        s.push_str("\n[stderr]\n");
                        s.push_str(&e);
                    }
                    let code = o.status.code().unwrap_or(-1);
                    s.push_str(&format!("\n[exit {code}]"));
                    Outcome {
                        content: truncate(s, MAX_OUT),
                        is_error: !o.status.success(),
                        finished: false,
                    }
                }
                Err(e) => err(format!("failed to spawn: {e}")),
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
            let p = resolve(cwd, input["path"].as_str().unwrap_or(""));
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
            match ureq::get(url).call() {
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
            match ureq::get(&search_url)
                .set("User-Agent", "Mozilla/5.0 (compatible; buildwithnexus/1.0)")
                .call()
            {
                Ok(resp) => match resp.into_string() {
                    Ok(html) => ok(truncate(strip_html(&html), MAX_OUT)),
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
            match ureq::get(url)
                .set("User-Agent", "Mozilla/5.0 (compatible; buildwithnexus/1.0)")
                .call()
            {
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
            let mut child = match Command::new("python3")
                .arg(&path)
                .current_dir(cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => return err(format!("failed to spawn python3 {}: {e}", path.display())),
            };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = std::io::Write::write_all(&mut stdin, payload.to_string().as_bytes());
            }
            match child.wait_with_output() {
                Ok(o) => {
                    let mut s = String::new();
                    s.push_str(&String::from_utf8_lossy(&o.stdout));
                    let e = String::from_utf8_lossy(&o.stderr);
                    if !e.trim().is_empty() {
                        s.push_str("\n[stderr]\n");
                        s.push_str(&e);
                    }
                    let code = o.status.code().unwrap_or(-1);
                    s.push_str(&format!("\n[exit {code}]"));
                    Outcome {
                        content: truncate(s, MAX_OUT),
                        is_error: !o.status.success(),
                        finished: false,
                    }
                }
                Err(e) => err(format!("failed to wait for python tool: {e}")),
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
            let query = input["query"].as_str().unwrap_or("");
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
            let name = input["name"].as_str().unwrap_or("unnamed");
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
                last_updated: "2026-07-06T00:00:00Z".to_string(),
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
                let payload = json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {
                        "name": tool,
                        "arguments": args
                    }
                });
                let mut child = match std::process::Command::new(cmd)
                    .args(&srv_args)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                {
                    Ok(c) => c,
                    Err(e) => {
                        return err(format!(
                            "failed to spawn MCP server '{server}' ({cmd}): {e}"
                        ))
                    }
                };
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = std::io::Write::write_all(&mut stdin, payload.to_string().as_bytes());
                    let _ = std::io::Write::write_all(&mut stdin, b"\n");
                }
                match child.wait_with_output() {
                    Ok(o) => {
                        let stdout = String::from_utf8_lossy(&o.stdout);
                        if let Ok(resp) = serde_json::from_str::<serde_json::Value>(&stdout) {
                            if let Some(res) = resp.get("result") {
                                return ok(res.to_string());
                            } else if let Some(err_val) = resp.get("error") {
                                return err(format!("MCP server error: {}", err_val));
                            }
                        }
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        if !o.status.success() || stdout.trim().is_empty() {
                            err(format!(
                                "MCP server exited with status {}: stdout: {} stderr: {}",
                                o.status, stdout, stderr
                            ))
                        } else {
                            ok(stdout.to_string())
                        }
                    }
                    Err(e) => err(format!("failed to read from MCP server '{server}': {e}")),
                }
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
                    let file_text = input["file_text"].as_str().unwrap_or("");
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
                        return err(format!(
                            "old text not found in {}\n\nold text sought:\n{}",
                            p.display(),
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
                    let insert_line = input["insert_line"].as_u64().unwrap_or(1) as usize;
                    let new_str = input["new_str"].as_str().unwrap_or("");
                    let body = match fs::read_to_string(&p) {
                        Ok(b) => b,
                        Err(e) => return err(format!("cannot read {}: {e}", p.display())),
                    };
                    let mut lines: Vec<String> = body.lines().map(|s| s.to_string()).collect();
                    let idx = (insert_line - 1).min(lines.len());
                    lines.insert(idx, new_str.to_string());
                    let next = lines.join("\n") + if body.ends_with('\n') { "\n" } else { "" };
                    {
                        let mut guard = UNDO_BACKUP.lock().unwrap_or_else(|e| e.into_inner());
                        *guard = Some((p.clone(), body));
                    }
                    match fs::write(&p, &next) {
                        Ok(_) => ok(format!("successfully inserted line at {}", p.display())),
                        Err(e) => err(format!("cannot write {}: {e}", p.display())),
                    }
                }
                "undo_edit" => {
                    let p = resolve(cwd, path);
                    let backup = {
                        let guard = UNDO_BACKUP.lock().unwrap_or_else(|e| e.into_inner());
                        guard.clone()
                    };
                    if let Some((backup_path, old_content)) = backup {
                        if backup_path == p {
                            match fs::write(&p, &old_content) {
                                Ok(_) => {
                                    let mut guard = UNDO_BACKUP.lock().unwrap_or_else(|e| e.into_inner());
                                    *guard = None;
                                    ok(format!(
                                        "successfully reverted the last edit to {}",
                                        p.display()
                                    ))
                                }
                                Err(e) => err(format!("cannot write {}: {e}", p.display())),
                            }
                        } else {
                            err(format!(
                                "the last edit was to {}, cannot undo for {}",
                                backup_path.display(),
                                p.display()
                            ))
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
            let kind = input["type"].as_str().unwrap_or("html");
            if kind == "html" {
                if let Some(reason) = html_artifact_quality_error(title, contents) {
                    return err(reason);
                }
            }
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
            match fs::write(&p, contents) {
                Ok(_) => ok(format!(
                    "Artifact successfully published locally to: {}",
                    p.display()
                )),
                Err(e) => err(format!("cannot write artifact: {e}")),
            }
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
        other => err(format!("unknown tool: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
        let compact = defs_for_context(false, 8192)
            .into_iter()
            .map(|d| d.name)
            .collect::<Vec<_>>();
        let full = defs_for_context(false, 32768)
            .into_iter()
            .map(|d| d.name)
            .collect::<Vec<_>>();
        for name in [
            "read_file",
            "write_file",
            "grep_files",
            "Artifact",
            "start_server",
            "wait_for_url",
            "open_browser",
            "python_tool",
        ] {
            assert!(compact.contains(&name), "{name} should stay in compact set");
        }
        for name in [
            "mcp_call",
            "kb_query",
            "kb_record",
            "verify",
            "text_editor_20250124",
            "AskUserQuestion",
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
        let _guard = crate::config::TEST_ENV_LOCK.lock().unwrap();
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
        assert!(run("edit_file", &json!({"path": "", "old": "a", "new": "b"}), &d).is_error);
        assert!(run("multi_edit", &json!({"path": "", "edits": [{"old": "a", "new": "b"}]}), &d).is_error);
        assert!(run("multi_edit", &json!({"path": "f.txt", "edits": []}), &d).is_error);
        assert!(run("create_dir", &json!({"path": ""}), &d).is_error);
        assert!(run("move_path", &json!({"from": "", "to": "b"}), &d).is_error);
        assert!(run("move_path", &json!({"from": "a", "to": ""}), &d).is_error);
        assert!(run("remove_path", &json!({"path": ""}), &d).is_error);
        assert!(run("run_command", &json!({"command": ""}), &d).is_error);
        assert!(run("todo_write", &json!({"items": []}), &d).is_error);
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

        // 5. Insert text
        let r = run(
            "str_replace_editor",
            &json!({"command": "insert", "path": "test.txt", "insert_line": 2, "new_str": "inserted-line"}),
            &d,
        );
        assert!(!r.is_error);
        let contents = fs::read_to_string(d.join("test.txt")).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines[1], "inserted-line");

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
        assert!(ver.content.contains("Verification Report"));

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
}
