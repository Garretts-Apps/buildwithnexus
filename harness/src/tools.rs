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
    /// Images for the model, `(media_type, base64)`. The agent drops them,
    /// and says why, for a model that does not take images.
    pub images: Vec<(String, String)>,
}

fn ok(s: impl Into<String>) -> Outcome {
    Outcome {
        content: s.into(),
        is_error: false,
        finished: false,
        images: Vec::new(),
    }
}
fn err(s: impl Into<String>) -> Outcome {
    Outcome {
        content: s.into(),
        is_error: true,
        finished: false,
        images: Vec::new(),
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
        ToolDef { name: "read_file", description: "Read a UTF-8 text file and return its contents. Optional start_line/end_line return a bounded line range. A PNG, JPEG, GIF or WebP picture comes back as an image you can look at; a PDF as its text. Works anywhere on the filesystem and expands `~`.",
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
        ToolDef { name: "screenshot_url", description: "Take a screenshot of a web page served on this machine (http://localhost:3000, a dev server you started) with a local headless Chrome, Chromium or Edge, and look at it. Use it to check how a page you built renders. Loopback addresses only, unless settings allow the host; requests the page makes to other hosts are blocked.",
            schema: json!({"type":"object","properties":{"url":{"type":"string","description":"http:// or https:// URL on localhost / 127.0.0.1"},"width":{"type":"integer","minimum":320,"maximum":2560,"description":"viewport width in pixels (default 1280)"},"height":{"type":"integer","minimum":240,"maximum":2560,"description":"viewport height in pixels (default 800)"}},"required":["url"]}) },
        ToolDef { name: "list_skills", description: "List available skills with their source and short description. Use before load_skill when choosing task-specific instructions.",
            schema: json!({"type":"object","properties":{}}) },
        ToolDef { name: "load_skill", description: "Load the full instructions for one named skill. Use only when that skill is relevant to the task. Folder skills report their directory so referenced scripts/references can be read with read_file.",
            schema: json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}) },
        ToolDef { name: "kb_query", description: "Query the project's local structured knowledge base (.buildwithnexus/knowledge/) for entities and architectural decisions.",
            schema: json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}) },
        ToolDef { name: "kb_record", description: "Record an entity, relationship, or architectural decision (with risk, cost, and reversibility primitives) into the local knowledge base.",
            schema: json!({"type":"object","properties":{"name":{"type":"string"},"entity_type":{"type":"string","enum":["Function","Class","Module","Package","Service","Endpoint","DatabaseTable","Migration","ArchitectureDecision"]},"description":{"type":"string"},"path":{"type":"string"}},"required":["name","entity_type"]}) },
        ToolDef { name: "rule_check", description: "Evaluate the current project state and changed files against the project's engineering rules (.buildwithnexus/rules/).",
            schema: json!({"type":"object","properties":{"changed_files":{"type":"array","items":{"type":"string"}},"task_type":{"type":"string"}}}) },
        ToolDef { name: "verify", description: "Run the verification layer (rules, tests, static analysis, confidence calculation) to generate a structured quality and safety report.",
            schema: json!({"type":"object","properties":{"task_description":{"type":"string"},"changed_files":{"type":"array","items":{"type":"string"}}},"required":["task_description"]}) },
        ToolDef { name: "check_work", description: "Check your work: auto-detect the project (Cargo/npm/pnpm/yarn/Python/Go) and run its build, tests, and linter, returning a concise pass/fail report. Use after a code change, before finish, to confirm it actually works. Optional `only` (build|test|lint) runs just one check; optional `command` runs a single custom check command instead of auto-detection.",
            schema: json!({"type":"object","properties":{"only":{"type":"string","enum":["build","test","lint"]},"command":{"type":"string"}}}) },
        ToolDef { name: "mcp_call", description: "Call a tool or query a resource on an enterprise Model Context Protocol (MCP) server configured in settings.json.",
            schema: json!({"type":"object","properties":{"server":{"type":"string"},"tool":{"type":"string"},"arguments":{"type":"object"}},"required":["server","tool"]}) },
    ];
    if include_subagent {
        // The built-in roles, then every agent file (see config::load_agent_defs).
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let agents = crate::config::load_agent_defs(&cwd);
        let mut roles: Vec<String> = crate::config::BUILTIN_ROLES
            .iter()
            .map(|r| r.to_string())
            .collect();
        roles.extend(agents.iter().map(|a| a.name.clone()));
        let listed: String = agents
            .iter()
            .map(|a| format!(" `{}`: {}.", a.name, a.description.trim_end_matches('.')))
            .collect();
        let roles_note = if listed.is_empty() {
            String::new()
        } else {
            format!(" Roles besides engineer and researcher:{listed}")
        };
        v.push(ToolDef {
            name: "task",
            description: crate::mcp::intern(&format!(
                "Common coding-agent alias: delegate a self-contained sub-task to a fresh agent.{PARALLEL_NOTE}{roles_note}"
            )),
            schema: json!({"type":"object","properties":{
                "task":{"type":"string"},
                "description":{"type":"string"},
                "role":{"type":"string","enum":roles},
                "isolate":{"type":"boolean"},
                "read_only":{"type":"boolean"}
            }}),
        });
        v.push(ToolDef {
            name: "spawn_subagent",
            description: crate::mcp::intern(&format!(
                "Delegate a self-contained sub-task to a fresh agent with its own context window. Set isolate=true to run it in an isolated git worktree. Returns the subagent's summary.{PARALLEL_NOTE}{roles_note}"
            )),
            schema: json!({"type":"object","properties":{
                "task":{"type":"string"},
                "role":{"type":"string","enum":roles},
                "isolate":{"type":"boolean"},
                "read_only":{"type":"boolean"}
            },"required":["task"]}),
        });
    }
    v.extend(mcp_defs());
    v
}

// How delegation calls in one reply run, for the task tools' descriptions.
const PARALLEL_NOTE: &str = " Set read_only=true for a helper that only reads and reports (it cannot change anything). Read-only and isolate=true helpers called in the same reply run at the same time; other helpers run one after another.";

// Tools discovered on connected MCP servers, advertised as
// `mcp__<server>__<tool>` with the server's own description and input
// schema. Empty until discovery finishes (see mcp::start_background).
fn mcp_defs() -> Vec<ToolDef> {
    crate::mcp::tools()
        .into_iter()
        .map(|t| ToolDef {
            name: crate::mcp::intern(&t.mangled),
            description: crate::mcp::intern(&t.description),
            schema: t.input_schema,
        })
        .collect()
}

// Models reporting fewer context tokens than this get the compact tool set.
// Small open-weight models degrade sharply with large tool catalogs, so 8k
// and 16k windows get the trimmed surface; a 32k window (what bwn asks
// Ollama for) and the larger windows of remote providers keep the full set.
const FULL_TOOLS_MIN_CONTEXT: usize = 32_768;

/// Whether a model with this window gets the compact tool set: no helpers,
/// todo list or screenshots.
pub fn compact_surface(context_tokens: usize) -> bool {
    context_tokens < FULL_TOOLS_MIN_CONTEXT
}

pub fn defs_for_context(include_subagent: bool, context_tokens: usize) -> Vec<ToolDef> {
    let all = defs(include_subagent);
    if !compact_surface(context_tokens) {
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
    // MCP tools are opt-in per user, so they stay even on the compact surface.
    crate::mcp::is_mcp_tool(name)
        || matches!(
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
            (crate::mcp::is_mcp_tool(d.name) && !is_mutating(d.name))
                || matches!(
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
                        | "screenshot_url"
                        | "list_skills"
                        | "load_skill"
                        | "kb_query"
                        | "rule_check"
                        | "verify"
                )
        })
        .collect()
}

// Mutating tools pass through the permission gate; reads never do. MCP tools
// count as mutating unless the server annotated them `readOnlyHint: true`.
pub fn is_mutating(name: &str) -> bool {
    if crate::mcp::is_mcp_tool(name) {
        return !crate::mcp::is_read_only(name);
    }
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
            | "check_work"
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

/// The tool an alias stands for (`bash` for run_command), else the name.
pub fn canonical_name(name: &str) -> &str {
    match name {
        "bash" => "run_command",
        "read" => "read_file",
        "write" => "write_file",
        "edit" => "edit_file",
        "patch" => "apply_patch",
        "glob" => "find_paths",
        "grep" => "grep_files",
        "list" => "list_dir",
        "webfetch" => "fetch_url",
        "websearch" => "web_search",
        "todowrite" => "todo_write",
        "todoread" => "todo_read",
        "skill" => "load_skill",
        "AskUserQuestion" => "question",
        "ExitPlanMode" => "exit_plan",
        "publish_artifact" => "Artifact",
        "task" => "spawn_subagent",
        other => other,
    }
}

pub fn is_mutating_call(name: &str, input: &Value) -> bool {
    if matches!(
        name,
        "str_replace_editor" | "text_editor_20241022" | "text_editor_20250124"
    ) {
        let cmd = input["command"].as_str().unwrap_or("view");
        cmd != "view"
    } else if matches!(name, "task" | "spawn_subagent") {
        // A helper started with read_only: true is read-only itself (its
        // tools and its gate, see agent::helper_reads_only), so starting
        // one is a read.
        input["read_only"].as_bool() != Some(true)
    } else {
        is_mutating(name)
    }
}

// Shell composition, redirection, substitution, expansion and control
// characters can smuggle a mutating tail behind a harmless first token
// (`cat x; rm -rf ~`, `cat x\nrm -rf ~`), so any of them disqualifies a
// command from every no-prompt path. `|` also covers `||`, `&` covers `&&`.
fn has_shell_metachars(cmd: &str) -> bool {
    let b = cmd.as_bytes();
    cmd.chars()
        .any(|c| c.is_control() || matches!(c, ';' | '|' | '&' | '>' | '<' | '`'))
        || b.windows(2).any(|w| {
            w[0] == b'$'
                && (w[1] == b'(' || w[1] == b'{' || w[1] == b'_' || w[1].is_ascii_alphabetic())
        })
}

// Flags that turn a "read-only" binary into a writer or a program launcher.
// Long flags match a getopt_long-style prefix (`--compress-prog` of
// `--compress-program`), with or without `=value`; single-letter flags also
// match inside a cluster (`sort -uo out`); longer single-dash flags (find's
// predicates) match exactly.
fn dangerous_flags_for(bin: &str) -> &'static [&'static str] {
    match bin {
        // `--pre`/`--hostname-bin` run a program; `--pre-glob` selects files
        // for `--pre`.
        "rg" => &["--pre", "--pre-glob", "--hostname-bin"],
        "sort" => &["-o", "--output", "--compress-program"],
        "find" => &[
            "-fprint", "-fprint0", "-fls", "-fprintf", "-exec", "-execdir", "-ok", "-okdir",
            "-delete",
        ],
        "git" => &[
            "-c",
            // Point git at another repository, whose own config can run
            // programs (a bare repo planted in the tree).
            "-C",
            "--git-dir",
            "--work-tree",
            "--config-env",
            "--exec-path",
            "--output",
            "--ext-diff",
            "-O",
            "--open-files-in-pager",
            "--upload-pack",
            "--receive-pack",
        ],
        "tree" => &["-o", "-R"],
        "file" => &["-C", "--compile"],
        _ => &[],
    }
}

fn flag_matches(tok: &str, flag: &str) -> bool {
    if tok == flag {
        return true;
    }
    if let Some(full) = flag.strip_prefix("--") {
        // getopt_long accepts any unambiguous prefix of a long option, in both
        // the `--opt=value` and separate-token `--opt value` forms, down to a
        // single letter (`sort --o=out` is `--output`), so every non-empty
        // prefix is the flag.
        let Some(rest) = tok.strip_prefix("--") else {
            return false;
        };
        let name = rest.split('=').next().unwrap_or(rest);
        return !name.is_empty() && full.starts_with(name);
    }
    let letter = flag.strip_prefix('-').filter(|l| l.len() == 1);
    match letter {
        Some(l) => tok.starts_with('-') && !tok.starts_with("--") && tok[1..].contains(l),
        None => false,
    }
}

// The binary's name as the OS will resolve it: last path component (either
// separator), lowercased, with a Windows executable extension dropped.
// Case-insensitive filesystems (macOS, Windows) run `RG` or `rg.EXE` as `rg`,
// so every check must see the same name.
fn normalized_bin(first: &str) -> String {
    let base = first
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(first)
        .to_lowercase();
    for ext in [".exe", ".cmd", ".bat", ".com"] {
        if let Some(stem) = base.strip_suffix(ext) {
            return stem.to_string();
        }
    }
    base
}

fn command_words(cmd: &str) -> (String, Vec<&str>) {
    let mut words = cmd.split_whitespace();
    let base = normalized_bin(words.next().unwrap_or(""));
    (base, words.collect())
}

// A program invoked by a path (`./cat`, `/tmp/evil/ls`, `.\cat.exe`,
// `C:\x\ls.exe`) is not the allowlisted bare binary — a repo can ship its own
// `./cat`. Such a program is never read-only and never matches a saved
// approval for the bare name.
fn is_path_qualified(program: &str) -> bool {
    program.contains('/') || program.contains('\\')
}

fn command_program(cmd: &str) -> &str {
    cmd.split_whitespace().next().unwrap_or("")
}

// The shell rewrites a program name that has quotes, patterns or (outside
// Windows, where `\` separates paths) backslashes: `"sh"` and `s\h` run
// `sh`, which the approval key and the git check would not see.
fn program_is_literal(program: &str) -> bool {
    !program.contains(['\'', '"', '*', '?', '[', '{']) && (cfg!(windows) || !program.contains('\\'))
}

// One word as the shell hands it to the program, quotes removed. `pattern`
// is the byte offset of its first unquoted glob or brace character, from
// which the shell may expand it into other words.
struct ShellWord {
    text: String,
    pattern: Option<usize>,
}

// Split a command into words as `sh` does, removing quotes and backslashes,
// so `"--pre"`, `'-o'` and `-\o` reach the flag checks as the program sees
// them. None when the words cannot be known from the text: an unterminated
// quote, `$'…'`/`$"…"` quoting, or a parameter expansion.
fn shell_words(cmd: &str) -> Option<Vec<ShellWord>> {
    // `$` followed by one of these expands (`$x`, `${x}`, `$(…)`, `$1`, `$@`).
    let expands = |c: Option<&char>| {
        c.is_some_and(|&c| c.is_ascii_alphanumeric() || "_{([@*#?$!-".contains(c))
    };
    let mut words = Vec::new();
    let mut word: Option<ShellWord> = None;
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ' ' || c == '\t' {
            words.extend(word.take());
            continue;
        }
        let w = word.get_or_insert_with(|| ShellWord {
            text: String::new(),
            pattern: None,
        });
        match c {
            '\'' => loop {
                match chars.next()? {
                    '\'' => break,
                    q => w.text.push(q),
                }
            },
            '"' => loop {
                match chars.next()? {
                    '"' => break,
                    '\\' if matches!(chars.peek(), Some('$' | '`' | '"' | '\\')) => {
                        w.text.push(chars.next()?);
                    }
                    '$' if expands(chars.peek()) => return None,
                    q => w.text.push(q),
                }
            },
            '\\' => w.text.push(chars.next().unwrap_or('\\')),
            '$' if expands(chars.peek()) || matches!(chars.peek(), Some('\'' | '"')) => {
                return None;
            }
            _ => {
                if matches!(c, '*' | '?' | '[' | '{') {
                    w.pattern.get_or_insert(w.text.len());
                }
                w.text.push(c);
            }
        }
    }
    words.extend(word);
    Some(words)
}

/// True when a command uses a write/exec flag of its binary (`rg --pre`,
/// `sort -o`, `find -exec`, `git -c`, `tree -o`, …) or `uniq`'s output
/// operand, judged on the words the program receives from the shell.
pub fn has_dangerous_flags(cmd: &str) -> bool {
    let (bin, _) = command_words(cmd);
    let Some(words) = shell_words(cmd) else {
        return true;
    };
    let args = words.get(1..).unwrap_or_default();
    let flags = dangerous_flags_for(&bin);
    for a in args {
        // `--` ends the options, except that find's expression follows it.
        if a.text == "--" && bin != "find" {
            break;
        }
        // A glob or brace word can expand into a flag (`sort *` beside a file
        // named `-o`, `rg {--pre,./x}`) unless it starts with a literal
        // character other than `-`.
        let may_expand_to_flag = a
            .pattern
            .is_some_and(|at| at == 0 || a.text.starts_with('-'));
        if (may_expand_to_flag && !flags.is_empty())
            || flags.iter().any(|f| flag_matches(&a.text, f))
            || (cfg!(windows) && bin == "sort" && is_windows_sort_output_switch(&a.text))
        {
            return true;
        }
    }
    // `uniq IN OUT` writes OUT, and a glob or brace operand can supply both.
    bin == "uniq"
        && (args.iter().filter(|a| !a.text.starts_with('-')).count() > 1
            || args.iter().any(|a| a.pattern.is_some()))
}

/// A single simple command with no shell metacharacters, control characters
/// or dangerous flags, running the program it names: the only shape that may
/// skip a permission prompt.
pub fn is_plain_command(cmd: &str) -> bool {
    let cmd = cmd.trim();
    !has_shell_metachars(cmd)
        && !(cfg!(windows) && cmd_exe_rewrites(cmd))
        && program_is_literal(command_program(cmd))
        && !has_dangerous_flags(cmd)
}

// `run_command` runs through `cmd /C` on Windows, which rewrites the text
// before the program sees it: `^` escapes a character and `%VAR%` (with
// `%VAR:~n,m%` substrings) expands, so `rg --pr^e` or `rg --pr%X:~0,1%e`
// spells a flag that shell_words, which models sh, never sees.
fn cmd_exe_rewrites(cmd: &str) -> bool {
    cmd.contains(['^', '%'])
}

// Windows' sort.exe writes its output to a file with `/O` or `/OUTPUT`, in
// any case and abbreviation. On Windows, `sort` on PATH is that program.
fn is_windows_sort_output_switch(tok: &str) -> bool {
    tok.strip_prefix('/').is_some_and(|rest| {
        let name = rest.split(':').next().unwrap_or(rest).to_ascii_lowercase();
        !name.is_empty() && "output".starts_with(&name)
    })
}

/// Whether a command that would skip the prompt (read-only or pre-approved)
/// may really do so in `cwd`. Plain git commands honour the repository's own
/// config, and settings such as `core.fsmonitor`, `core.pager`,
/// `diff.external`, textconv or filter drivers run programs — so a repo that
/// arrived with its `.git` (an archive, a copied tree) could turn
/// `git status` into code execution. Git is only trusted when every
/// repository-level key is on a known-inert list.
pub fn skips_prompt_safely(cmd: &str, cwd: &Path) -> bool {
    let (bin, _) = command_words(cmd.trim());
    bin != "git" || git_repo_config_is_inert(cwd)
}

fn git_repo_config_is_inert(cwd: &Path) -> bool {
    let Ok(out) = Command::new("git")
        .args(["config", "--list", "--show-scope", "--no-includes"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        // git or the directory is missing: the command cannot run either.
        return true;
    };
    if !out.status.success() {
        // No repository here: nothing but the user's own config applies.
        return String::from_utf8_lossy(&out.stdout).trim().is_empty();
    }
    String::from_utf8_lossy(&out.stdout).lines().all(|line| {
        let Some((scope, rest)) = line.split_once('\t') else {
            return false;
        };
        let key = rest.split('=').next().unwrap_or(rest);
        !matches!(scope, "local" | "worktree") || git_key_is_inert(key)
    })
}

// Repository-level git config keys that never name a program to run.
// Anything else (including `include.*`, which pulls in more config) makes
// git commands ask before they run.
fn git_key_is_inert(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let Some((section, rest)) = key.split_once('.') else {
        return false;
    };
    let (sub, var) = match rest.rsplit_once('.') {
        Some((sub, var)) => (Some(sub), var),
        None => (None, rest),
    };
    match (section, sub) {
        ("core", None) => matches!(
            var,
            "repositoryformatversion"
                | "filemode"
                | "bare"
                | "logallrefupdates"
                | "ignorecase"
                | "precomposeunicode"
                | "symlinks"
                | "autocrlf"
                | "safecrlf"
                | "eol"
                | "sparsecheckout"
                | "sparsecheckoutcone"
                | "untrackedcache"
                | "commitgraph"
                | "multipackindex"
                | "splitindex"
                | "quotepath"
                | "abbrev"
                | "checkstat"
                | "trustctime"
                | "compression"
                | "longpaths"
        ),
        ("remote", Some(_)) => matches!(
            var,
            "url" | "pushurl" | "fetch" | "push" | "tagopt" | "prune" | "mirror" | "promisor"
        ),
        ("branch", Some(_)) => {
            matches!(
                var,
                "remote" | "merge" | "rebase" | "pushremote" | "description"
            )
        }
        ("submodule", Some(_)) => matches!(var, "url" | "active" | "branch"),
        // Network settings, such as the auth header actions/checkout
        // stores: none of them names a program. A cookie file git saves to
        // is a file it writes on the next fetch.
        ("http", _) => !matches!(var, "cookiefile" | "savecookies"),
        ("diff", None) => var != "external",
        // Signing runs the user's own gpg.program, which repo config could
        // only name under `gpg.*` (not inert).
        ("commit", None) => matches!(var, "gpgsign" | "verbose" | "status" | "cleanup"),
        (
            "user" | "pull" | "push" | "fetch" | "init" | "log" | "color" | "advice" | "status"
            | "rerere" | "gc" | "index" | "feature" | "extensions" | "column" | "lfs",
            _,
        ) => true,
        _ => false,
    }
}

// Tools whose second word names a different action (`git status` vs
// `git push`); approvals for them are stored per subcommand.
const MULTI_VERB: &[&str] = &[
    "git",
    "npm",
    "npx",
    "pnpm",
    "yarn",
    "bun",
    "cargo",
    "docker",
    "podman",
    "kubectl",
    "helm",
    "go",
    "dotnet",
    "pip",
    "pip3",
    "uv",
    "poetry",
    "gh",
    "brew",
    "apt",
    "apt-get",
    "terraform",
    "make",
    "just",
    "rustup",
    "gcloud",
    "aws",
    "az",
];

// Programs that run whatever code their arguments carry (`sh -c …`,
// `python3 -c …`, `awk 'BEGIN{system(…)}'`, `env X=1 anything`). Approving
// one call must not approve every later call, so their approvals are stored
// per exact command. Versioned names (`python3.12`, `node18`, `tclsh8.6`)
// count as their family; see `runs_arbitrary_code`.
const RUNS_ARBITRARY_CODE: &[&str] = &[
    // Shells.
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "mksh",
    "ash",
    "csh",
    "tcsh",
    "nu",
    "busybox",
    "cmd",
    "powershell",
    "pwsh",
    "wsl",
    // Interpreters and script runners.
    "python",
    "pythonw",
    "py",
    "pypy",
    "ipython",
    "node",
    "nodejs",
    "deno",
    "tsx",
    "ts-node",
    "bunx",
    "pnpx",
    "uvx",
    "perl",
    "ruby",
    "php",
    "lua",
    "luajit",
    "tclsh",
    "wish",
    "expect",
    "rscript",
    "r",
    "julia",
    "awk",
    "gawk",
    "mawk",
    "nawk",
    // GNU sed runs shell commands with `e`.
    "sed",
    "gsed",
    "osascript",
    "wscript",
    "cscript",
    "mshta",
    // Wrappers that run the command they are given.
    "env",
    "command",
    "builtin",
    "xargs",
    "parallel",
    "nohup",
    "nice",
    "ionice",
    "chrt",
    "taskset",
    "stdbuf",
    "setsid",
    "unbuffer",
    "script",
    "flock",
    "timeout",
    "time",
    "watch",
    "watchexec",
    "entr",
    "hyperfine",
    "strace",
    "ltrace",
    "gdb",
    "valgrind",
    "sudo",
    "doas",
    "su",
    "runuser",
    "pkexec",
    "chroot",
    "unshare",
    "nsenter",
    "systemd-run",
    "caffeinate",
    "start",
    "fakeroot",
    "fakechroot",
    "faketime",
    "firejail",
    "bwrap",
    "proot",
    "proxychains",
    "torsocks",
    "tsocks",
    "numactl",
    "chronic",
    "ifne",
    "sg",
    "ssh-agent",
    "dbus-run-session",
    "systemd-inhibit",
    "catchsegv",
    "cgexec",
    "chpst",
    "eval",
    "exec",
];

/// Whether `bin` (a normalized binary name) is in `RUNS_ARBITRARY_CODE`,
/// itself or as a versioned name: `python3.12`, `python3.6m`, `node18` and
/// `ruby-3.2` all run code as their family does.
fn runs_arbitrary_code(bin: &str) -> bool {
    let family = match bin.find(|c: char| c.is_ascii_digit()) {
        Some(i) => bin[..i].trim_end_matches(['-', '_', '.']),
        None => bin,
    };
    RUNS_ARBITRARY_CODE.contains(&family)
}

// Single-verb programs whose arguments decide what they destroy, move or
// stop: `s` on `rm -rf tests` must not approve `rm -rf tools`, so their
// approvals are stored per exact command, like interpreters.
const KEYS_ON_ARGUMENTS: &[&str] = &[
    "rm", "rmdir", "unlink", "shred", "srm", "mv", "cp", "ln", "install", "rsync", "chmod",
    "chown", "chgrp", "chattr", "setfacl", "dd", "truncate", "kill", "pkill", "killall", "del",
    "erase", "rd", "move", "copy", "robocopy", "xcopy", "icacls", "takeown",
];

/// The key an "allow this session" / "always allow" answer is stored under:
/// the binary, plus its subcommand for multi-verb tools (`git status`), or
/// the whole command for shells, interpreters and programs whose arguments
/// name what they change (`rm -rf tests`).
pub fn approval_key(cmd: &str) -> String {
    let (bin, args) = command_words(cmd);
    if runs_arbitrary_code(&bin) || KEYS_ON_ARGUMENTS.contains(&bin.as_str()) {
        return cmd.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    // Keep the path in the key for a path-qualified program, so an approval
    // for `cat` never covers `./cat`.
    let program = command_program(cmd);
    let ident = if is_path_qualified(program) {
        program.to_string()
    } else {
        bin.clone()
    };
    if bin == "git" {
        return match git_subcommand(&args) {
            Some((sub, rest)) if git_is_destructive(sub, rest) => {
                cmd.split_whitespace().collect::<Vec<_>>().join(" ")
            }
            Some((sub, _)) => format!("{ident} {sub}"),
            None => ident,
        };
    }
    if MULTI_VERB.contains(&bin.as_str()) {
        if let Some(sub) = args.iter().find(|a| !a.starts_with('-')) {
            return format!("{ident} {sub}");
        }
    }
    ident
}

// Git's own options that take the next word as their value (`git -C dir`).
const GIT_OPTIONS_WITH_VALUE: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--config-env",
    "--super-prefix",
    "--list-cmds",
    "--attr-source",
    "--shallow-file",
];

/// The subcommand of a git command line and the words after it, past git's
/// own options: `git -C sub -c k=v push -f` → (`push`, [`-f`]).
pub(crate) fn git_subcommand<'a, 'b>(args: &'b [&'a str]) -> Option<(&'a str, &'b [&'a str])> {
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if !a.starts_with('-') {
            return Some((a, &args[i + 1..]));
        }
        i += if GIT_OPTIONS_WITH_VALUE.contains(a) {
            2
        } else {
            1
        };
    }
    None
}

// Git subcommands, or the forms of them, whose arguments name what they
// discard or rewrite: `s` on `git push --force origin main` or
// `git rm -r src` must not approve the next force push or removal, so
// their approvals are stored per exact command, like rm's.
fn git_is_destructive(sub: &str, rest: &[&str]) -> bool {
    // A long option, alone or with `=value`.
    let long = |names: &[&str]| {
        rest.iter().any(|a| {
            names
                .iter()
                .any(|n| *a == *n || a.strip_prefix(n).is_some_and(|v| v.starts_with('=')))
        })
    };
    // A letter in a cluster of short options (`-uf`).
    let short = |letters: &str| {
        rest.iter().any(|a| {
            a.len() > 1
                && a.starts_with('-')
                && !a.starts_with("--")
                && a[1..].chars().any(|c| letters.contains(c))
        })
    };
    let action = rest.iter().find(|a| !a.starts_with('-')).copied();
    match sub {
        "rm" | "clean" | "checkout" | "restore" | "filter-branch" | "filter-repo" | "prune"
        | "update-ref" => true,
        "reset" => long(&["--hard", "--merge", "--keep"]),
        "push" => {
            long(&[
                "--force",
                "--force-with-lease",
                "--force-if-includes",
                "--delete",
                "--mirror",
                "--prune",
            ]) || short("fd")
                || rest
                    .iter()
                    .any(|a| a.starts_with('+') || (a.starts_with(':') && a.len() > 1))
        }
        "branch" => long(&["--delete", "--force", "--move", "--copy"]) || short("dDfmMcC"),
        "switch" => long(&["--discard-changes", "--force", "--force-create"]) || short("fC"),
        "tag" => long(&["--delete", "--force"]) || short("df"),
        "stash" => matches!(action, Some("drop" | "clear")),
        "reflog" => matches!(action, Some("expire" | "delete")),
        "worktree" => matches!(action, Some("remove" | "prune")),
        "submodule" => matches!(action, Some("deinit")),
        "notes" => matches!(action, Some("remove" | "prune")),
        "read-tree" => long(&["--reset"]) || short("u"),
        "checkout-index" => long(&["--force"]) || short("f"),
        "gc" => long(&["--prune"]),
        _ => false,
    }
}

/// `p` relative to the project root, with `/` separators, when it lies
/// inside the project: what a permission rule's path pattern is matched
/// against (`migrations/**`).
pub fn project_relative(p: &Path, cwd: &Path) -> Option<String> {
    let rel = canonicalize_lenient(p)
        .strip_prefix(canonicalize_lenient(cwd))
        .ok()?
        .to_path_buf();
    Some(
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// The commands an allow rule is checked against: each part of a compound
/// command (split at `;`, `&&`, `||`, `|`, `&` and line breaks), whitespace
/// collapsed and the program reduced to its name. The whole command comes
/// first, and an allow rule only ever matches that. Deny and ask rules use
/// `guard_subjects`.
pub fn rule_subjects(cmd: &str) -> Vec<String> {
    let flat = |c: &str| -> Option<String> {
        let mut words = c.split_whitespace();
        let first = normalized_bin(words.next()?);
        Some(
            std::iter::once(first.as_str())
                .chain(words)
                .collect::<Vec<_>>()
                .join(" "),
        )
    };
    let mut out: Vec<String> = flat(cmd).into_iter().collect();
    for part in cmd.split(['\n', '\r', ';', '|', '&']) {
        if let Some(p) = flat(part) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

// Programs that run a command given by their later words (`sudo git push`,
// `xargs git push`, `sh -c 'git push'`, `cmd /c git push`), and the shell
// words that start one. Their own options cannot all be known, so a deny
// or ask rule takes every later word as a possible start of the command.
const RUNS_A_COMMAND: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "mksh",
    "ash",
    "csh",
    "tcsh",
    "nu",
    "busybox",
    "cmd",
    "powershell",
    "pwsh",
    "wsl",
    "env",
    "command",
    "builtin",
    "exec",
    "eval",
    "xargs",
    "parallel",
    "nohup",
    "nice",
    "ionice",
    "chrt",
    "taskset",
    "stdbuf",
    "setsid",
    "unbuffer",
    "script",
    "flock",
    "timeout",
    "time",
    "watch",
    "watchexec",
    "entr",
    "hyperfine",
    "strace",
    "ltrace",
    "valgrind",
    "gdb",
    "sudo",
    "doas",
    "su",
    "runuser",
    "pkexec",
    "chroot",
    "unshare",
    "nsenter",
    "systemd-run",
    "caffeinate",
    "start",
    "find",
    "fakeroot",
    "fakechroot",
    "faketime",
    "firejail",
    "bwrap",
    "proot",
    "proxychains",
    "proxychains4",
    "torsocks",
    "tsocks",
    "numactl",
    "chronic",
    "ifne",
    "run0",
    "sg",
    "ssh-agent",
    "dbus-run-session",
    "systemd-inhibit",
    "catchsegv",
    "cgexec",
    "chpst",
    "uv",
    "poetry",
    "pipenv",
    "pdm",
    "hatch",
    "bundle",
    "direnv",
    "mise",
    "asdf",
    "nix-shell",
    "{",
    "}",
    "!",
    "if",
    "then",
    "else",
    "elif",
    "do",
    "while",
    "until",
];

// How deep `guard_subjects` follows a command inside a command
// (`sh -c "sudo sh -c 'git push'"`).
const MAX_NESTED_COMMANDS: usize = 4;

// Bounds on `guard_subjects` for one part of a command: the words after a
// wrapper that may start its command (options never do, and a wrapper takes
// at most a few operands of its own), how many starts are followed, and how
// many words of each are matched.
const STARTS_AFTER_WRAPPER: usize = 16;
const MAX_STARTS: usize = 256;
const MAX_SUBJECT_WORDS: usize = 1024;

// A shell variable assignment before a command (`FOO=1 git push`).
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

// The words of a command as the program receives them where that can be
// known, else split at whitespace with the quotes dropped.
fn loose_words(cmd: &str) -> Vec<String> {
    match shell_words(cmd) {
        Some(words) => words.into_iter().map(|w| w.text).collect(),
        None => cmd
            .split_whitespace()
            .map(|w| w.replace(['\'', '"'], ""))
            .collect(),
    }
}

/// The commands a deny or ask rule is checked against: every
/// `rule_subjects` entry, and each command as it runs once the wrappers in
/// front of it are taken off (`env`, `sudo -u x`, `nice -n 5`, `xargs`,
/// `timeout 30`, `FOO=1`, a path such as `/usr/bin/git`), with the command
/// inside `sh -c '…'`, `bash -lc "…"`, `cmd /c`, `$(…)` and backquotes, and
/// git's own options (`-C dir`, `-c k=v`, `--git-dir`) taken out before its
/// subcommand. `git -C . push` and `sudo sh -c 'git push'` both give
/// `git push`.
pub fn guard_subjects(cmd: &str) -> std::rc::Rc<[String]> {
    thread_local! {
        // Every deny and ask rule checks the same command in turn.
        static LAST: std::cell::RefCell<Option<(String, std::rc::Rc<[String]>)>> =
            const { std::cell::RefCell::new(None) };
    }
    if let Some(hit) = LAST.with_borrow(|l| {
        l.as_ref()
            .filter(|(c, _)| c == cmd)
            .map(|(_, s)| std::rc::Rc::clone(s))
    }) {
        return hit;
    }
    let out: std::rc::Rc<[String]> = guard_subjects_for(cmd, cfg!(windows)).into();
    LAST.set(Some((cmd.to_string(), std::rc::Rc::clone(&out))));
    out
}

fn guard_subjects_for(cmd: &str, windows: bool) -> Vec<String> {
    let mut out = rule_subjects(cmd);
    let mut seen: std::collections::HashSet<String> = out.iter().cloned().collect();
    add_guard_subjects(cmd, 0, &mut out, &mut seen);
    // cmd.exe, which runs commands on Windows, drops `^` and expands
    // `%VAR%` before the program sees them: `g^it push` runs `git push`.
    if windows && cmd_exe_rewrites(cmd) {
        let plain = cmd_exe_plain(cmd);
        for s in rule_subjects(&plain) {
            if seen.insert(s.clone()) {
                out.push(s);
            }
        }
        add_guard_subjects(&plain, 0, &mut out, &mut seen);
    }
    out
}

// A command as cmd.exe hands it on, as far as its text tells: `^` escapes
// dropped and each `%VAR%` taken as empty.
fn cmd_exe_plain(cmd: &str) -> String {
    let text = cmd.replace('^', "");
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        match rest[i + 1..].find('%') {
            Some(j) => rest = &rest[i + j + 2..],
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn add_guard_subjects(
    cmd: &str,
    depth: usize,
    out: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    if depth > MAX_NESTED_COMMANDS {
        return;
    }
    let push = |s: String, out: &mut Vec<String>, seen: &mut std::collections::HashSet<String>| {
        if !s.is_empty() && seen.insert(s.clone()) {
            out.push(s);
        }
    };
    // The whole command keeps quoted separators inside their word
    // (`bash -c 'a && b'`); its parts catch the rest.
    let parts = std::iter::once(cmd).chain(cmd.split(['\n', '\r', ';', '|', '&', '(', ')', '`']));
    for part in parts {
        let words = loose_words(part);
        // Every word a command may start at: the first, the one after an
        // assignment, and the words after a program that runs a command.
        let mut starts = vec![0usize];
        let mut visited = vec![false; words.len()];
        let mut unpacked = vec![false; words.len()];
        let mut followed = 0;
        while let Some(i) = starts.pop() {
            if i >= words.len() || visited[i] {
                continue;
            }
            visited[i] = true;
            let word = &words[i];
            if is_assignment(word) {
                starts.push(i + 1);
                continue;
            }
            followed += 1;
            if followed > MAX_STARTS {
                break;
            }
            let bin = normalized_bin(word);
            if RUNS_A_COMMAND.contains(&bin.as_str()) {
                starts.extend(
                    (i + 1..words.len())
                        .filter(|&j| !words[j].starts_with('-'))
                        .take(STARTS_AFTER_WRAPPER),
                );
                // A command handed over as one word: `sh -c 'git push'`,
                // `env -S 'git push'`, `watch 'git push'`.
                for j in i + 1..words.len() {
                    let w = &words[j];
                    if !unpacked[j]
                        && (w.contains(char::is_whitespace) || w.contains([';', '|', '&']))
                    {
                        unpacked[j] = true;
                        add_guard_subjects(w, depth + 1, out, seen);
                    }
                }
            }
            let end = words.len().min(i + 1 + MAX_SUBJECT_WORDS);
            let rest: Vec<&str> = words[i + 1..end].iter().map(String::as_str).collect();
            push(
                std::iter::once(bin.as_str())
                    .chain(rest.iter().copied())
                    .collect::<Vec<_>>()
                    .join(" "),
                out,
                seen,
            );
            // `git-push`, as git's own programs are named.
            let bin = match bin.strip_prefix("git-") {
                Some(sub) if !sub.is_empty() => {
                    let dashed = std::iter::once(sub)
                        .chain(rest.iter().copied())
                        .collect::<Vec<_>>();
                    push(format!("git {}", dashed.join(" ")), out, seen);
                    continue;
                }
                _ => bin,
            };
            if bin == "git" {
                if let Some((sub, args)) = git_subcommand(&rest) {
                    push(
                        ["git", sub]
                            .into_iter()
                            .chain(args.iter().copied())
                            .collect::<Vec<_>>()
                            .join(" "),
                        out,
                        seen,
                    );
                    // An alias given on the command line
                    // (`git -c alias.p=push p`): what it stands for.
                    if let Some(value) = git_inline_alias(&rest, sub) {
                        let expanded = match value.strip_prefix('!') {
                            Some(shell) => shell.to_string(),
                            None => format!("git {value}"),
                        };
                        let line = std::iter::once(expanded.as_str())
                            .chain(args.iter().copied())
                            .collect::<Vec<_>>()
                            .join(" ");
                        add_guard_subjects(&line, depth + 1, out, seen);
                    }
                }
            }
        }
    }
}

// The value of `-c alias.<sub>=…` among git's own options, if given.
fn git_inline_alias<'a>(args: &[&'a str], sub: &str) -> Option<&'a str> {
    let key = format!("alias.{sub}=");
    args.windows(2)
        .take_while(|w| w[0] != sub)
        .filter(|w| w[0] == "-c")
        .find_map(|w| {
            w[1].get(..key.len())
                .filter(|k| k.eq_ignore_ascii_case(&key))
                .map(|_| &w[1][key.len()..])
        })
}

/// Whether `cmd` is more than one simple command (a pipeline, a list, a
/// subshell or a substitution), or hands code to an interpreter or spells a
/// word in bash's `$'…'` quoting, and any word of it names a program
/// matching `program` (a `*`/`?` pattern for a program name). Such a command
/// can build what it runs from parts no rule sees (`git $(echo push)`,
/// `ruby -e 'system "git push"'`), so a deny rule for that program refuses
/// it.
pub fn compound_mentions(cmd: &str, program: &str) -> bool {
    compound_mentions_for(cmd, program, cfg!(windows))
}

// An interpreter given code as a word (`perl -e 'system "git push"'`):
// what that code runs is not read here. Shells are left out, as
// guard_subjects reads the command they are handed.
fn hands_code_to_interpreter(cmd: &str) -> bool {
    let words = loose_words(cmd);
    words.iter().enumerate().any(|(i, w)| {
        let bin = normalized_bin(w);
        runs_arbitrary_code(&bin)
            && !RUNS_A_COMMAND.contains(&bin.as_str())
            && words[i + 1..]
                .iter()
                .any(|w| w.contains(char::is_whitespace))
    })
}

// On Windows a command cmd.exe rewrites (`^`, `%VAR%`) counts as compound
// too, and its words are read with the `^` escapes dropped.
fn compound_mentions_for(cmd: &str, program: &str, windows: bool) -> bool {
    let rewritten = windows && cmd_exe_rewrites(cmd);
    let compound = rewritten
        || cmd.contains(['\n', '\r', ';', '|', '&', '(', ')', '`'])
        || cmd.contains("$'")
        || cmd.contains("$\"")
        || hands_code_to_interpreter(cmd);
    let program = normalized_bin(program);
    let text = if rewritten {
        cmd.replace('^', "")
    } else {
        cmd.to_string()
    };
    compound
        && !program.is_empty()
        && text
            .split(|c: char| c.is_whitespace() || ";|&()`'\"<>={}$!%".contains(c))
            .filter(|w| !w.is_empty())
            .any(|w| crate::hooks::glob_match(&program, &normalized_bin(w)))
}

/// A saved approval naming a shell or interpreter on its own (`python3`), as
/// 0.14.3–0.14.8 stored them. It would approve any code the program is
/// handed, so it never pre-approves anything; the settings file is left as is.
pub fn is_bare_interpreter_approval(key: &str) -> bool {
    let mut words = key.split_whitespace();
    match (words.next(), words.next()) {
        (Some(bin), None) => runs_arbitrary_code(&normalized_bin(bin)),
        _ => false,
    }
}

// Commands that are unambiguously read-only (grep, find, cat, etc.) — allowed
// even in ReadOnly permission mode despite run_command being generically mutating.
pub fn is_readonly_command(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    if !is_plain_command(trimmed) {
        return false;
    }
    // A path-qualified program (`./cat`, `/tmp/evil/ls`) is not the
    // allowlisted read-only binary, whatever its base name.
    if is_path_qualified(command_program(trimmed)) {
        return false;
    }
    let lower = trimmed.to_lowercase();
    let mut words = lower.split_whitespace();
    let base = normalized_bin(words.next().unwrap_or(""));
    match base.as_str() {
        // `find -delete`/`-exec` are refused by has_dangerous_flags. Note that
        // `sed` is deliberately absent (sed -i edits in place), as are `xargs`
        // and `tee` (they exist to run/write things).
        "find" => true,
        // Only known-readonly git subcommands; `git clean`, `git tag <name>`,
        // and branch delete/rename mutate.
        "git" => match words.next().unwrap_or("") {
            "status" | "log" | "diff" | "show" | "blame" => true,
            "remote" => words.all(|w| w == "-v" || w == "--verbose"),
            // Listing only: any other argument creates, copies, moves,
            // deletes, re-tracks a branch or opens an editor.
            "branch" => words.all(|w| {
                matches!(
                    w,
                    "-a" | "--all"
                        | "-r"
                        | "--remotes"
                        | "-v"
                        | "-vv"
                        | "--verbose"
                        | "--list"
                        | "--show-current"
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

/// `s` on one line (whitespace runs collapsed), cut to `max` characters with
/// a trailing `…` when longer.
pub(crate) fn clamp_preview(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max - 1).collect();
    format!("{cut}…")
}

pub fn preview(name: &str, input: &Value) -> String {
    clamp_preview(&raw_preview(name, input), PREVIEW_MAX_CHARS)
}

/// What the permission prompt shows. Unlike `preview`, a shell command is
/// never shortened or flattened: the user approves exactly what will run,
/// with each line break shown as a visible `⏎`.
pub fn approval_label(name: &str, input: &Value) -> String {
    match name {
        "bash" | "run_command" | "start_server" => {
            let cmd = input["command"].as_str().unwrap_or("?");
            let shown = cmd
                .replace("\r\n", " ⏎ ")
                .replace('\n', " ⏎ ")
                .replace('\r', " ␍ ")
                .replace('\t', " ");
            let verb = if name == "start_server" {
                "start server"
            } else {
                "run"
            };
            format!("{verb}: {shown}")
        }
        _ => preview(name, input),
    }
}

/// A call that only creates or changes files: what `accept-edits` lets run
/// without a prompt inside the project. Deleting (`remove_path`) and every
/// command still ask.
pub fn is_file_edit(name: &str, input: &Value) -> bool {
    match name {
        "write" | "write_file" | "edit" | "edit_file" | "multi_edit" | "patch" | "apply_patch"
        | "create_dir" | "move_path" => true,
        "str_replace_editor" | "text_editor_20241022" | "text_editor_20250124" => {
            is_mutating_call(name, input)
        }
        _ => false,
    }
}

// Where web_search sends its query.
const WEB_SEARCH_HOST: &str = "lite.duckduckgo.com";

/// The host a network tool will contact (`fetch_url`, `web_search`,
/// `headless_browser`, `wait_for_url`, `screenshot_url`, and `open_browser`
/// with a URL).
/// Fetches and search queries can carry data out (`https://evil/?k=<secret>`,
/// "my api key is …") and fetches reach local services, so outside `auto`
/// they are approved per host. `?` when the URL has no parsable host.
pub fn network_host(name: &str, input: &Value) -> Option<String> {
    if matches!(name, "websearch" | "web_search") {
        return Some(WEB_SEARCH_HOST.to_string());
    }
    if !matches!(
        name,
        "webfetch"
            | "fetch_url"
            | "headless_browser"
            | "wait_for_url"
            | "screenshot_url"
            | "open_browser"
    ) {
        return None;
    }
    let url = input["url"]
        .as_str()
        .map(str::trim)
        .filter(|u| !u.is_empty())?;
    // screenshot_url opens http(s) pages only and refuses anything else
    // itself, with that reason; there is no host to approve.
    if name == "screenshot_url"
        && !url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
    {
        return None;
    }
    Some(url_authority(url).unwrap_or_else(|| "?".to_string()))
}

// `scheme://[user@]host[:port]/…` → `host[:port]`, lowercased. The port
// stays: approving a dev server on :3000 must not approve :2375. The host
// is the one the request reaches: percent-escapes decoded and a trailing
// dot dropped, as the HTTP client and DNS treat them, so a rule naming the
// host cannot be stepped around by spelling it differently.
fn url_authority(url: &str) -> Option<String> {
    if let Ok(u) = url::Url::parse(url) {
        if let Some(host) = u.host_str().filter(|h| !h.is_empty()) {
            return Some(crate::net::canonical_authority(&match u.port() {
                Some(p) => format!("{host}:{p}"),
                None => host.to_string(),
            }));
        }
    }
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#', '\\']).next()?;
    let host = authority.rsplit('@').next()?.to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
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
        "screenshot_url" => format!("screenshot {}", input["url"].as_str().unwrap_or("?")),
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
        "check_work" => match (input["command"].as_str(), input["only"].as_str()) {
            (Some(cmd), _) => format!("check work: {cmd}"),
            (None, Some(only)) => format!("check work: {only}"),
            (None, None) => "check work: build + test + lint".to_string(),
        },
        "mcp_call" => format!(
            "MCP call: {}/{}",
            input["server"].as_str().unwrap_or("?"),
            input["tool"].as_str().unwrap_or("?")
        ),
        n if crate::mcp::is_mcp_tool(n) => match crate::mcp::demangle(n) {
            Some((server, tool)) => format!("MCP call: {server}/{tool}"),
            None => format!("MCP call: {n}"),
        },
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
        "todo_write" | "todowrite" => match input["items"].as_array().map(Vec::len) {
            Some(1) => "todo list: 1 item".to_string(),
            Some(n) => format!("todo list: {n} items"),
            None => "todo list".to_string(),
        },
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

/// Text files read and written through an editor (`bwn acp` with a client
/// that offers fs/read_text_file and fs/write_text_file): reads see unsaved
/// buffers and the editor records each write. `None` means the editor does
/// not handle that direction, so the disk is used.
pub trait EditorFiles: Send + Sync {
    fn read(&self, path: &Path) -> Option<Result<String, String>>;
    fn write(&self, path: &Path, contents: &str) -> Option<Result<(), String>>;
}

static EDITOR_FILES: OnceLock<Box<dyn EditorFiles>> = OnceLock::new();

pub fn set_editor_files(files: Box<dyn EditorFiles>) {
    let _ = EDITOR_FILES.set(files);
}

/// What replacing `old` with `new` exactly once in a file's text gave.
enum Replaced {
    /// `old` occurred once; the updated text.
    Exact(String),
    /// `old` was absent but had one whitespace-tolerant match; the updated text.
    Lenient(String),
    NotFound,
    /// `old` occurred this many times.
    Ambiguous(usize),
}

/// The shared core of edit_file and str_replace_editor's str_replace: the
/// text must match exactly once, or else once ignoring whitespace (tiered
/// lenient fallback); ambiguous or absent text keeps the diagnostic.
fn replace_once(body: &str, old: &str, new: &str) -> Replaced {
    // One pass to locate, a counting pass over the remainder for the
    // occurrence total, with no separate full replace scan.
    let Some(first) = body.find(old) else {
        return match lenient_find(body, old) {
            LenientMatch::Hit(hit) => Replaced::Lenient(apply_lenient(body, &hit, new)),
            _ => Replaced::NotFound,
        };
    };
    match body[first + old.len()..].matches(old).count() {
        0 => Replaced::Exact(body.replacen(old, new, 1)),
        extra => Replaced::Ambiguous(extra + 1),
    }
}

// A text file as the editor has it, else from disk. An editor that cannot
// read it (outside its project, say) leaves the disk to answer: the gate has
// already allowed the read.
//
// Edits go through here and write the whole text back, so a file over
// MAX_TEXT_FILE is refused rather than read in part.
fn read_text(path: &Path) -> std::io::Result<String> {
    match EDITOR_FILES.get().and_then(|e| e.read(path)) {
        Some(Ok(text)) => Ok(text),
        _ => {
            let (file, len) = open_regular(path)?;
            if len > MAX_TEXT_FILE {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "the file is {}, over the {} limit for text tools; edit it with run_command (sed, a script) instead",
                        human_bytes(len),
                        human_bytes(MAX_TEXT_FILE)
                    ),
                ));
            }
            let (text, _) = read_prefix(file, len, MAX_TEXT_FILE)?;
            Ok(text)
        }
    }
}

// Largest file the text tools take in whole. read_file shows the first
// MAX_TEXT_FILE bytes of a larger one and says so; edits refuse it. Without
// a cap a multi-gigabyte log read in full would exhaust memory, all to show
// the model its first 100 KB.
const MAX_TEXT_FILE: u64 = 64 * 1024 * 1024;

// Open `path` only when it is a regular file, and return its size. Opening a
// FIFO blocks until some writer appears, and a device such as /dev/zero or
// /dev/urandom never ends, so both are refused before they are opened.
fn open_regular(path: &Path) -> std::io::Result<(fs::File, u64)> {
    fn refuse(meta: &fs::Metadata) -> Option<std::io::Error> {
        if meta.is_file() {
            None
        } else if meta.is_dir() {
            Some(std::io::Error::new(
                std::io::ErrorKind::IsADirectory,
                "it is a directory, not a file; list it with list_dir or list_tree",
            ))
        } else {
            Some(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "it is not a regular file (a FIFO, socket or device); text tools read only regular files",
            ))
        }
    }
    if let Some(e) = refuse(&fs::metadata(path)?) {
        return Err(e);
    }
    let file = fs::File::open(path)?;
    // Checked again on the open handle: the path may have been swapped.
    let meta = file.metadata()?;
    if let Some(e) = refuse(&meta) {
        return Err(e);
    }
    Ok((file, meta.len()))
}

// Up to `cap` bytes of an open file as UTF-8, and whether any were left
// unread. A cut that lands inside a multibyte character drops that
// character's partial bytes instead of failing the whole read as binary.
fn read_prefix(file: fs::File, len: u64, cap: u64) -> std::io::Result<(String, bool)> {
    let mut bytes = Vec::with_capacity(len.min(cap) as usize);
    // One byte past the cap tells a file that grew since stat from one that
    // is exactly `cap` bytes long.
    std::io::Read::read_to_end(&mut std::io::Read::take(file, cap + 1), &mut bytes)?;
    let cut = bytes.len() as u64 > cap;
    if cut {
        bytes.truncate(cap as usize);
        if let Err(e) = std::str::from_utf8(&bytes) {
            if e.error_len().is_none() {
                bytes.truncate(e.valid_up_to());
            }
        }
    }
    String::from_utf8(bytes)
        .map(|text| (text, cut))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

// A text file for viewing: the editor's copy, else at most `cap` bytes from
// disk. The flag is true when the file was longer than what was read.
fn read_text_view(path: &Path, cap: u64) -> std::io::Result<(String, bool)> {
    match EDITOR_FILES.get().and_then(|e| e.read(path)) {
        Some(Ok(text)) => Ok((text, false)),
        _ => {
            let (file, len) = open_regular(path)?;
            read_prefix(file, len, cap)
        }
    }
}

// Writes through the editor when it takes writes; its refusal stands.
fn write_text(path: &Path, contents: &str) -> std::io::Result<()> {
    match EDITOR_FILES.get().and_then(|e| e.write(path, contents)) {
        Some(r) => r.map_err(std::io::Error::other),
        None => write_atomic(path, contents),
    }
}

// Write file contents atomically: same-directory temp + rename, so a crash
// or power loss mid-write can never leave a truncated file (std's rename
// replaces existing destinations on both POSIX and Windows). The
// destination's permissions are copied onto the temp first — a plain
// replace-by-rename would silently strip an executable bit from a script
// the agent edits.
fn write_atomic(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    crate::config::atomic_write(path, contents.as_ref(), crate::config::Perm::KeepExisting)
}

pub(crate) fn resolve(cwd: &Path, p: &str) -> PathBuf {
    // `~` expansion: $HOME, or %USERPROFILE% on Windows (where `~\x` is
    // also accepted).
    let tilde_rest = p.strip_prefix("~/").or_else(|| {
        if cfg!(windows) {
            p.strip_prefix("~\\")
        } else {
            None
        }
    });
    if let Some(rest) = tilde_rest {
        if let Some(home) = crate::config::user_home() {
            return home.join(rest);
        }
    }
    if p == "~" {
        if let Some(home) = crate::config::user_home() {
            return home;
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
        "move_path" => Some(resolve(cwd, input["from"].as_str().unwrap_or("").trim())),
        "glob" | "find_paths" | "find_files" | "grep" | "grep_files" => {
            Some(resolve(cwd, root_arg(input)))
        }
        "read_many_files" => touched_paths(name, input, cwd).into_iter().next(),
        _ => None,
    }
}

/// Every path a call touches: all of read_many_files' paths, both ends of a
/// move, otherwise touched_path.
pub fn touched_paths(name: &str, input: &Value, cwd: &Path) -> Vec<PathBuf> {
    match name {
        "read_many_files" => input["paths"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|p| !p.trim().is_empty())
                    .map(|p| resolve(cwd, p))
                    .collect()
            })
            .unwrap_or_default(),
        // The paths move_path itself uses, trimmed as it trims them.
        "move_path" => ["from", "to"]
            .iter()
            .filter_map(|k| input[*k].as_str())
            .map(|p| resolve(cwd, p.trim()))
            .collect(),
        // `git apply` reads the patch's paths from the top of the work tree
        // (from the folder itself outside one).
        "patch" | "apply_patch" => {
            let base = git_top(cwd);
            patch_files(input["patch"].as_str().unwrap_or(""))
                .into_iter()
                .map(|p| resolve(&base, &p))
                .collect()
        }
        _ => touched_path(name, input, cwd).into_iter().collect(),
    }
}

// The work tree `cwd` is in (the nearest folder up holding `.git`), else
// `cwd` itself.
fn git_top(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(cwd)
        .to_path_buf()
}

/// The files a unified diff names, as `git apply` reads them: both sides of
/// each `---`/`+++` header and of `diff --git`, with the leading component
/// (`a/`, `b/`) removed, plus rename and copy lines; `/dev/null` is left
/// out. Hunk bodies are skipped by their line counts, so a removed line
/// that reads `--- x` is never taken for a header.
pub fn patch_files(patch: &str) -> Vec<String> {
    // git's C-style quoting of unusual names: "a/x\ty".
    fn unquote(s: &str) -> String {
        let Some(inner) = s
            .strip_prefix('"')
            .and_then(|r| r.strip_suffix('"'))
            .filter(|_| s.len() >= 2)
        else {
            return s.to_string();
        };
        let b = inner.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] != b'\\' || i + 1 >= b.len() {
                out.push(b[i]);
                i += 1;
                continue;
            }
            let c = b[i + 1];
            i += 2;
            match c {
                b'0'..=b'7' => {
                    let mut v = u32::from(c - b'0');
                    for _ in 0..2 {
                        if i < b.len() && (b'0'..=b'7').contains(&b[i]) {
                            v = v * 8 + u32::from(b[i] - b'0');
                            i += 1;
                        }
                    }
                    out.push(v as u8);
                }
                b'a' => out.push(7),
                b'b' => out.push(8),
                b't' => out.push(b'\t'),
                b'n' => out.push(b'\n'),
                b'v' => out.push(11),
                b'f' => out.push(12),
                b'r' => out.push(b'\r'),
                other => out.push(other),
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    // A quoted name at the start of `s` (closing quote not escaped) and
    // what follows it.
    fn quoted(s: &str) -> Option<(String, &str)> {
        let b = s.as_bytes();
        if b.first() != Some(&b'"') {
            return None;
        }
        let mut i = 1;
        while i < b.len() {
            match b[i] {
                b'\\' => i += 2,
                b'"' => return Some((unquote(&s[..=i]), &s[i + 1..])),
                _ => i += 1,
            }
        }
        None
    }
    // A header name: quoted, or up to a tab (a timestamp may follow).
    fn header_name(rest: &str) -> String {
        let rest = rest.trim_end_matches('\r');
        match quoted(rest) {
            Some((name, _)) => name,
            None => rest.split('\t').next().unwrap_or("").trim_end().to_string(),
        }
    }
    // git apply's default -p1: drop everything up to the first `/`.
    fn strip_one(name: &str) -> Option<String> {
        if name.is_empty() || name == "/dev/null" {
            return None;
        }
        Some(name.split_once('/').map_or(name, |(_, r)| r).to_string())
    }
    let mut out: Vec<String> = Vec::new();
    let mut push = |p: Option<String>| {
        if let Some(p) = p.filter(|p| !p.is_empty()) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    };
    let (mut old_left, mut new_left) = (0usize, 0usize);
    for line in patch.lines() {
        if old_left > 0 || new_left > 0 {
            match line.as_bytes().first() {
                Some(b'-') => old_left = old_left.saturating_sub(1),
                Some(b'+') => new_left = new_left.saturating_sub(1),
                Some(b'\\') => {}
                _ => {
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                }
            }
            continue;
        }
        if let Some(h) = line.strip_prefix("@@ -") {
            // @@ -l[,n] +l[,n] @@
            let count = |r: &str| -> usize {
                let r = r.split_whitespace().next().unwrap_or("");
                r.split_once(',')
                    .map_or(Some(1), |(_, n)| n.parse().ok())
                    .unwrap_or(0)
            };
            old_left = count(h);
            new_left = h.split_once(" +").map_or(0, |(_, r)| count(r));
        } else if let Some(r) = line
            .strip_prefix("--- ")
            .or_else(|| line.strip_prefix("+++ "))
        {
            push(strip_one(&header_name(r)));
        } else if let Some(r) = ["rename from ", "rename to ", "copy from ", "copy to "]
            .iter()
            .find_map(|p| line.strip_prefix(p))
        {
            push(Some(header_name(r)));
        } else if let Some(r) = line.strip_prefix("diff --git ") {
            let r = r.trim_end_matches('\r');
            if let Some((first, rest)) = quoted(r) {
                push(strip_one(&first));
                push(strip_one(&header_name(rest.trim_start())));
            } else {
                // `a/<name> b/<name>`: the split where both halves agree,
                // else the only one there is.
                let splits: Vec<usize> = r.match_indices(" b/").map(|(i, _)| i).collect();
                let agreed = splits
                    .iter()
                    .find(|&&i| r.get(..i).and_then(strip_one) == strip_one(&r[i + 1..]));
                if let Some(&i) = agreed.or(if splits.len() == 1 {
                    splits.first()
                } else {
                    None
                }) {
                    push(strip_one(&r[..i]));
                    push(strip_one(r[i + 1..].trim_end()));
                }
            }
        }
    }
    out
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
        "write" | "write_file" | "edit" | "edit_file" | "multi_edit" | "remove_path" => {
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

// Where a search without a folder of its own looks: the working folder and
// every added one. A named folder is searched alone.
fn search_roots(input: &Value, cwd: &Path) -> Vec<PathBuf> {
    match root_arg(input) {
        "." => std::iter::once(cwd.to_path_buf())
            .chain(crate::workdirs::list())
            .collect(),
        r => vec![resolve(cwd, r)],
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

// The variable names in the global environment of a tmux server that is
// already running (none when there is no server).
fn tmux_server_env() -> Vec<String> {
    let Ok(out) = Command::new("tmux")
        .args(["show-environment", "-g"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, _)| k.to_string()))
        .collect()
}

// `script` run with `names` unset, whatever shell tmux starts it with.
fn without_env(names: &[String], script: &str) -> String {
    if names.is_empty() {
        return script.to_string();
    }
    let unset: Vec<String> = names.iter().map(|n| format!("-u {n}")).collect();
    format!("env {} sh -c {}", unset.join(" "), shell_quote(script))
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
    #[cfg(windows)]
    {
        // `tasklist` exits 0 whether or not anything matched (a miss prints
        // "INFO: No tasks are running..."), so the answer is in the output.
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|o| tasklist_reports_pid(&String::from_utf8_lossy(&o.stdout), pid))
            .unwrap_or(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

// Parses `tasklist /FO CSV /NH` output: one `"image","pid","session",...`
// row per match. True when a row's PID column is exactly `pid`.
#[cfg_attr(not(windows), allow(dead_code))]
fn tasklist_reports_pid(output: &str, pid: u64) -> bool {
    let want = pid.to_string();
    output.lines().any(|line| {
        let line = line.trim();
        if !line.starts_with('"') {
            return false;
        }
        line.split("\",\"")
            .nth(1)
            .map(|f| f.trim_matches('"') == want)
            .unwrap_or(false)
    })
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
    write_atomic(&path, text)
        .map_err(|e| format!("cannot write server record {}: {e}", path.display()))
}

// The sandboxed launcher for a server command, per `mode`: Ok(None) runs it
// unsandboxed as before, Err is `require` without a backend. Writes are
// confined to the workspace even when the server runs from a subdirectory.
fn server_sandbox(
    mode: crate::sandbox::Mode,
    command: &str,
    run_cwd: &Path,
    cwd: &Path,
) -> Result<Option<Command>, String> {
    let script = format!(
        "cd {} && {command}",
        shell_quote(&run_cwd.to_string_lossy())
    );
    crate::sandbox::wrap_in(mode, &script, cwd)
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
    let sandboxed = match server_sandbox(crate::sandbox::mode(), command, &run_cwd, cwd) {
        Ok(c) => c,
        Err(e) => return err(e),
    };
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

    // A tmux server would run the command outside the sandbox, so a
    // sandboxed server is spawned directly (and dies with the session).
    if sandboxed.is_none() && command_available("tmux") {
        let session = format!("bwn-{}", safe_name(&name));
        let shell = format!(
            "cd {} && {} 2>&1 | tee {}",
            shell_quote(&run_cwd.to_string_lossy()),
            command,
            shell_quote(&log_path.to_string_lossy())
        );
        // A tmux server that is already running hands the session its own
        // environment, keys included, so the command unsets them itself.
        let shell = without_env(
            &crate::sandbox::credentials_to_unset(tmux_server_env()),
            &shell,
        );
        let mut tmux = Command::new("tmux");
        // A tmux server started here inherits this environment.
        crate::sandbox::scrub_credentials(&mut tmux);
        match tmux
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
        let mut cmd = if let Some(c) = sandboxed {
            c
        } else if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", command]).current_dir(&run_cwd);
            crate::sandbox::scrub_credentials(&mut c);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-lc", command]).current_dir(&run_cwd);
            crate::sandbox::scrub_credentials(&mut c);
            c
        };
        match cmd
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
        #[cfg(windows)]
        {
            // The recorded pid is the `cmd /C` wrapper; `/T` takes the
            // server it spawned down with it.
            stopped = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
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
    if let Some(why) = blocked_url(url) {
        return err(why);
    }
    let started = std::time::Instant::now();
    let deadline = started + Duration::from_secs(timeout);

    loop {
        let last_error = match web_get(url, Some("buildwithnexus/1.0")) {
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
            Err(WebError::Blocked(why)) => return err(why),
            Err(WebError::Http(e)) if matches!(*e, ureq::Error::Status(..)) => {
                let ureq::Error::Status(status, resp) = *e else {
                    unreachable!()
                };
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

// Files the OS opener hands to a viewer, never to an interpreter: opening
// `payload.bat`, an `.app` or a `.desktop` file would run it.
const BROWSER_FILE_EXTS: &[&str] = &[
    "html", "htm", "xhtml", "svg", "pdf", "png", "jpg", "jpeg", "gif", "webp", "txt", "md", "json",
];

/// Hands `target` (a URL or a file the checks above allowed) to the OS
/// opener and waits for the opener, not the browser, to exit.
pub fn spawn_opener(target: &str) -> std::io::Result<std::process::ExitStatus> {
    if cfg!(target_os = "macos") {
        Command::new("open").arg(target).status()
    } else if cfg!(windows) {
        // Not `cmd /C start`: cmd re-parses the target, so `&` in a URL
        // would run a second command.
        Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", target])
            .status()
    } else {
        Command::new("xdg-open").arg(target).status()
    }
}

fn open_browser(input: &Value, cwd: &Path) -> Outcome {
    let target = if let Some(url) = input["url"].as_str().filter(|s| !s.trim().is_empty()) {
        let url = url.trim();
        let lower = url.to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) {
            return err("open_browser only opens http:// and https:// URLs");
        }
        url.to_string()
    } else if let Some(path) = input["path"].as_str().filter(|s| !s.trim().is_empty()) {
        let p = resolve(cwd, path);
        let ext = p
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if !BROWSER_FILE_EXTS.contains(&ext.as_str()) || !p.is_file() {
            return err(format!(
                "open_browser only opens existing {} files",
                BROWSER_FILE_EXTS.join("/")
            ));
        }
        p.to_string_lossy().into_owned()
    } else {
        return err("url or path is required");
    };
    match spawn_opener(&target) {
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

// Canonicalize the deepest existing ancestor of the raw path and re-append
// the remainder, so symlinked prefixes (macOS `/tmp` → `/private/tmp`)
// compare consistently even for paths that don't exist yet (e.g. write
// targets). `..` is folded only in the non-existent remainder: folding it
// first would turn `link/../x` into `x` while the OS resolves it through
// the symlink's target.
fn canonicalize_lenient(p: &Path) -> PathBuf {
    use std::path::Component;
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    let comps: Vec<Component> = p.components().collect();
    for i in (1..comps.len()).rev() {
        let prefix: PathBuf = comps[..i].iter().collect();
        let Ok(mut out) = prefix.canonicalize() else {
            continue;
        };
        for c in &comps[i..] {
            match c {
                Component::ParentDir => {
                    out.pop();
                }
                Component::CurDir => {}
                other => {
                    out.push(other.as_os_str());
                    // Reached again after a `..`: resolve it like the OS would.
                    if let Ok(r) = out.canonicalize() {
                        out = r;
                    }
                }
            }
        }
        return out;
    }
    normalize(p)
}

// True if the path resolves outside the working directory. Both sides go
// through the same lenient canonicalization — canonicalizing only the base
// (as this used to) made cwd=/tmp/x become /private/tmp/x on macOS while the
// target stayed /tmp/x/f, misclassifying in both directions.
pub fn escapes_cwd(p: &Path, cwd: &Path) -> bool {
    let base = canonicalize_lenient(cwd);
    !canonicalize_lenient(p).starts_with(&base)
}

/// True if the path resolves outside the working directory and every folder
/// added with --add-dir. Links are followed, so one inside an added folder
/// that leads elsewhere is outside.
pub fn escapes_roots(p: &Path, cwd: &Path) -> bool {
    escapes_cwd(p, cwd) && crate::workdirs::list().iter().all(|r| escapes_cwd(p, r))
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
        // Trimmed, as move_path trims them before it moves anything.
        "move_path" => {
            let from = resolve(cwd, input["from"].as_str().unwrap_or("").trim());
            if escapes_roots(&from, cwd) {
                return Some(from);
            }
            resolve(cwd, input["to"].as_str().unwrap_or("").trim())
        }
        _ => return None,
    };
    if escapes_roots(&candidate, cwd) {
        Some(candidate)
    } else {
        None
    }
}

// Credential stores and key material, matched by path component (never by
// substring, so `~/.ssh` itself counts and `notes.sshx` does not).
const SENSITIVE_DIRS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".kube",
    ".azure",
    ".buildwithnexus",
];
const SENSITIVE_SUBPATHS: &[[&str; 2]] = &[
    [".docker", "config.json"],
    [".config", "gh"],
    [".config", "gcloud"],
];
const SENSITIVE_FILES: &[&str] = &[
    ".netrc",
    "_netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".pgpass",
    ".env",
];

fn is_sensitive_dir_name(name: &str, windows: bool) -> bool {
    SENSITIVE_DIRS.iter().any(|d| name_is(name, d, windows))
}

fn is_sensitive_file_name(name: &str, windows: bool) -> bool {
    SENSITIVE_FILES.iter().any(|f| name_is(name, f, windows))
        || name.starts_with(".env.")
        || ["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"]
            .iter()
            .any(|k| name.starts_with(k) || (windows && is_short_name_of(name, k)))
        || [".pem", ".p12", ".pfx"].iter().any(|e| name.ends_with(e))
}

// `name` is `long`, or on Windows its 8.3 short name (`SSH~1` for `.ssh`,
// `GIT-CR~1` for `.git-credentials`), which opens the same file.
fn name_is(name: &str, long: &str, windows: bool) -> bool {
    name == long || (windows && is_short_name_of(name, long))
}

// A short name keeps up to six characters of the long name's base (leading
// dots and embedded dots dropped), then `~N` and up to three of its extension.
fn is_short_name_of(name: &str, long: &str) -> bool {
    let Some((stem, tail)) = name.split_once('~') else {
        return false;
    };
    let digits = tail.split('.').next().unwrap_or("");
    if stem.is_empty() || digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let long = long.trim_start_matches('.');
    let base = long.rsplit_once('.').map_or(long, |(b, _)| b);
    let base: String = base.chars().filter(|c| !matches!(c, '.' | ' ')).collect();
    base.starts_with(stem)
}

// A path's lowercased component names. With `windows` the path is read as
// Windows reads it, whatever the host: `\` and `/` both separate, a drive,
// UNC or device prefix (`C:`, `\\?\`, `\\.\`, `\??\`) is dropped,
// trailing dots and spaces are ignored (`.ssh.` opens `.ssh`) and a stream
// suffix names its file (`.env::$DATA`).
fn path_names(p: &Path, windows: bool) -> Vec<String> {
    if !windows {
        return p
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(n) => Some(n.to_string_lossy().to_lowercase()),
                _ => None,
            })
            .collect();
    }
    let s = p.to_string_lossy().replace('\\', "/").to_lowercase();
    let mut rest = s.as_str();
    for prefix in ["//?/unc/", "//?/", "//./", "/??/"] {
        if let Some(r) = rest.strip_prefix(prefix) {
            rest = r;
            break;
        }
    }
    let b = rest.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        rest = &rest[2..];
    }
    let mut names: Vec<String> = Vec::new();
    for comp in rest.split('/') {
        let comp = comp.split(':').next().unwrap_or("");
        match comp {
            "" | "." => {}
            ".." => {
                names.pop();
            }
            _ => {
                let comp = comp.trim_end_matches(['.', ' ']);
                if !comp.is_empty() {
                    names.push(comp.to_string());
                }
            }
        }
    }
    names
}

fn sensitive_names(names: &[String], windows: bool, in_project: bool) -> bool {
    names.iter().any(|n| {
        // A project's own `.buildwithnexus` (instructions, skills) is
        // ordinary repo content.
        !(in_project && n == ".buildwithnexus") && is_sensitive_dir_name(n, windows)
    }) || names.windows(2).any(|w| {
        SENSITIVE_SUBPATHS
            .iter()
            .any(|s| name_is(&w[0], s[0], windows) && name_is(&w[1], s[1], windows))
    }) || names
        .last()
        .is_some_and(|n| is_sensitive_file_name(n, windows))
        || (!windows && !in_project && is_proc_environ(names))
}

// `/proc/<pid>/environ` (or `/proc/<pid>/task/<tid>/environ`, or a glob of
// either): another process's environment holds the keys bwn scrubs from the
// commands it runs, the parent shell's included.
fn is_proc_environ(names: &[String]) -> bool {
    names.len() >= 3
        && names[0] == "proc"
        && names
            .last()
            .is_some_and(|n| n == "environ" || component_glob_match(n, "environ"))
}

// `/proc`, `/proc/<pid>`, `/proc/self` (or a glob of a pid): a folder that
// holds environ files.
fn is_proc_tree(names: &[String]) -> bool {
    match names {
        [p] => p == "proc",
        [p, pid] => {
            p == "proc"
                && (pid == "self"
                    || pid == "thread-self"
                    || pid.contains(['*', '?', '['])
                    || pid.bytes().all(|b| b.is_ascii_digit())
                    || pid.starts_with('$'))
        }
        [p, _, task] => p == "proc" && task == "task",
        _ => false,
    }
}

// `environ`, or a glob of it that spells part of the name (`env*`): a bare
// `*` is any file.
fn is_environ_name(n: &str) -> bool {
    n == "environ"
        || (n.contains(['*', '?', '[']) && n.contains("env") && component_glob_match(n, "environ"))
}

/// Sensitivity of a path inside a project tree, relative to its root: the
/// same credential names as `is_sensitive`, except that the project's own
/// `.buildwithnexus` (instructions, skills) is ordinary repo content.
pub fn is_sensitive_in_project(rel: &Path) -> bool {
    let windows = cfg!(windows);
    sensitive_names(&path_names(rel, windows), windows, true)
}

// Paths that should never be read/written without explicit confirmation, even in
// auto mode — credential stores and key material are the prime exfil targets.
// Checked on the lexical path and on the symlink-resolved one, so a link in
// the workspace pointing at ~/.ssh is caught too.
pub fn is_sensitive(p: &Path) -> bool {
    is_sensitive_for(p, cfg!(windows))
}

/// `is_sensitive` with the platform passed in, so tests can run the Windows
/// path rules anywhere.
fn is_sensitive_for(p: &Path, windows: bool) -> bool {
    let check = |q: &Path| sensitive_names(&path_names(q, windows), windows, false);
    check(&normalize(p)) || check(&canonicalize_lenient(p))
}

// Shell glob match for one path component (`*`, `?`, `[...]` as any one
// char). Like the shell, a leading wildcard never matches a leading dot.
fn component_glob_match(pat: &str, name: &str) -> bool {
    if name.starts_with('.') && !pat.starts_with('.') {
        return false;
    }
    fn go(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| go(&p[1..], &n[i..])),
            Some('?') => !n.is_empty() && go(&p[1..], &n[1..]),
            Some('[') => {
                let close = p.iter().position(|c| *c == ']').unwrap_or(p.len() - 1);
                !n.is_empty() && go(&p[close + 1..], &n[1..])
            }
            Some(c) => n.first() == Some(c) && go(&p[1..], &n[1..]),
        }
    }
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    go(&p, &n)
}

fn glob_component_is_sensitive(comp: &str) -> bool {
    SENSITIVE_DIRS
        .iter()
        .chain(SENSITIVE_FILES)
        .chain([".docker", ".config"].iter())
        .filter(|name| name.starts_with('.'))
        .any(|name| component_glob_match(comp, name))
}

/// The first path-like argument of a shell command that names a sensitive
/// path (`cat ~/.aws/credentials`, `--file=$HOME/.netrc`). Quotes and
/// backslashes are stripped and `~`/`$HOME` expanded first; glob components
/// are matched against the sensitive names. A word is also read as a Windows
/// path, with `\` separating, on Windows or when it has a backslash
/// (`C:\Users\me\.ssh\id_rsa` passed to cmd or PowerShell from anywhere).
pub fn command_sensitive_path(cmd: &str, cwd: &Path) -> Option<PathBuf> {
    command_sensitive_path_for(cmd, cwd, cfg!(windows))
}

thread_local! {
    // The script command being run (`with_script_command`): its own path is
    // not a sensitive path to touch, though it sits in `.buildwithnexus`.
    static SCRIPT_COMMAND: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Runs `f` (the gate and run of a custom script command) with `script`
/// exempt from the sensitive-path check: the file was trusted to load as a
/// command, so running it is not a touch of `.buildwithnexus`. Its arguments
/// are still checked.
pub fn with_script_command<T>(script: &Path, f: impl FnOnce() -> T) -> T {
    SCRIPT_COMMAND.with(|s| *s.borrow_mut() = Some(script.to_path_buf()));
    let r = f();
    SCRIPT_COMMAND.with(|s| *s.borrow_mut() = None);
    r
}

fn is_script_command(p: &Path) -> bool {
    SCRIPT_COMMAND.with(|s| s.borrow().as_deref() == Some(p))
}

fn command_sensitive_path_for(cmd: &str, cwd: &Path, windows: bool) -> Option<PathBuf> {
    let home = crate::config::user_home().map(|h| h.to_string_lossy().into_owned());
    for raw in cmd.split(|c: char| {
        c.is_whitespace() || matches!(c, ';' | '|' | '&' | '<' | '>' | '(' | ')' | '`')
    }) {
        let unquoted: String = raw.chars().filter(|c| !matches!(c, '\'' | '"')).collect();
        let win = unquoted.rsplit('=').next().unwrap_or("");
        // PowerShell's `-Path:value`.
        let win = match win.strip_prefix('-') {
            Some(p) => p.split_once(':').map_or("", |(_, v)| v),
            None => win,
        };
        if (windows || win.contains('\\')) && !win.is_empty() && !is_script_command(Path::new(win))
        {
            let names = path_names(Path::new(win), true);
            if sensitive_names(&names, true, false)
                || names
                    .iter()
                    .any(|n| n.contains(['*', '?', '[']) && glob_component_is_sensitive(n))
            {
                return Some(PathBuf::from(win));
            }
        }
        let tok: String = raw
            .chars()
            .filter(|c| !matches!(c, '\'' | '"' | '\\'))
            .collect();
        // `--file=path` / `-fpath`-style values: check what follows `=`.
        let tok = tok.rsplit('=').next().unwrap_or("").to_string();
        if tok.is_empty() || tok.starts_with('-') {
            continue;
        }
        let tok = match &home {
            Some(h) => tok.replacen("${HOME}", h, 1).replacen("$HOME", h, 1),
            None => tok,
        };
        let p = resolve(cwd, &tok);
        if is_script_command(&p) {
            continue;
        }
        // Any `…/environ` word: a loop variable (`$p/environ`) or a `cd
        // /proc/1` first hides the `/proc` the path check needs.
        if !windows
            && tok
                .rsplit('/')
                .next()
                .is_some_and(|n| is_environ_name(&n.to_lowercase()))
        {
            return Some(p);
        }
        // `/proc` or one process's folder: a recursive search (`rg -a KEY
        // /proc`) reads every environ under it.
        if !windows && is_proc_tree(&path_names(&normalize(&p), false)) {
            return Some(p);
        }
        if is_sensitive_for(&p, windows) {
            return Some(p);
        }
        if tok.contains(['*', '?', '[']) {
            let hit = p.components().any(|c| match c {
                std::path::Component::Normal(n) => {
                    let n = n.to_string_lossy().to_lowercase();
                    n.contains(['*', '?', '[']) && glob_component_is_sensitive(&n)
                }
                _ => false,
            });
            if hit {
                return Some(p);
            }
        }
    }
    None
}

// ── PATH lookup ──────────────────────────────────────────────────────────────

/// Full path of the executable `bin` would run as, searching PATH. Used
/// instead of spawning `which`, which stock Windows does not have.
pub fn find_on_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let pathext = std::env::var_os("PATHEXT");
    find_in_path(bin, &path, pathext.as_deref(), cfg!(windows))
}

/// `find_on_path` with its inputs passed in, so tests can run the Windows
/// rules anywhere. On Windows a bare name also matches each PATHEXT suffix
/// (`git` finds `git.exe`), as cmd.exe does.
pub(crate) fn find_in_path(
    bin: &str,
    path: &std::ffi::OsStr,
    pathext: Option<&std::ffi::OsStr>,
    windows: bool,
) -> Option<PathBuf> {
    // Only bare names are looked up; `./x` or `C:\x` is not a PATH search.
    if bin.is_empty() || bin.contains(['/', '\\']) {
        return None;
    }
    let mut names = Vec::new();
    if windows {
        let exts = pathext
            .and_then(|s| s.to_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(".COM;.EXE;.BAT;.CMD");
        let exts: Vec<&str> = exts.split(';').filter(|e| !e.is_empty()).collect();
        let lower = bin.to_ascii_lowercase();
        if exts
            .iter()
            .any(|e| lower.ends_with(&e.to_ascii_lowercase()))
        {
            names.push(bin.to_string());
        }
        names.extend(exts.iter().map(|e| format!("{bin}{e}")));
    } else {
        names.push(bin.to_string());
    }
    std::env::split_paths(path)
        // A relative entry (including an empty one) would resolve against the
        // current directory, which may be an untrusted checkout.
        .filter(|dir| dir.is_absolute())
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .find(|p| is_executable_file(p))
}

fn is_executable_file(p: &Path) -> bool {
    let Ok(meta) = fs::metadata(p) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
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

/// True when a shell command string references `/mnt/<drive>/` paths in WSL2.
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

mod web_search;
use web_search::*;

mod destructive;
pub use destructive::catastrophic;

// ── output truncation ────────────────────────────────────────────────────

fn truncate(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    // Back off to a UTF-8 char boundary — String::truncate / slicing panics if
    // `max` lands inside a multibyte glyph, which arbitrary file/command output
    // can easily produce.
    let mut out = crate::config::cut_at_char_boundary(&s, max).to_string();
    out.push_str("\n…[truncated]");
    out
}

// Head+tail truncation for command-ish output: test/build failures and the
// `[exit N]` marker live at the tail, so keep both ends and cut the middle.
// File reads keep plain head truncation (see `truncate_read`).
fn truncate_head_tail(s: String, max: usize) -> String {
    truncate_head_tail_omitting(s, max, 0)
}

// truncate_head_tail for text that already lost `extra` bytes from its
// middle (run_with_timeout's bounded capture): the marker counts them too, so
// the model sees the same report as if the whole output had been kept.
fn truncate_head_tail_omitting(s: String, max: usize, extra: usize) -> String {
    if s.len() <= max && extra == 0 {
        return s;
    }
    let head_len = HEAD_KEEP.min(max / 2).min(s.len());
    let tail_len = (max - HEAD_KEEP.min(max / 2)).min(s.len() - head_len);
    let head_end = crate::config::cut_at_char_boundary(&s, head_len).len();
    let mut tail_start = s.len() - tail_len;
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start - head_end + extra;
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
    let shown = crate::config::cut_at_char_boundary(&s, max);
    let shown_lines = shown.lines().count();
    format!(
        "{shown}\n…[truncated: content is {total_bytes} bytes / {total_lines} lines total; showing the first {shown_lines} lines — pass start_line/end_line to read the rest in ranges]"
    )
}

// Cap a single output line (grep matches, listings), marking the cut so the
// model knows content is missing rather than silently absent.
fn clip_line(line: &str, max: usize) -> String {
    if line.len() <= max {
        return line.to_string();
    }
    format!(
        "{}…[line truncated]",
        crate::config::cut_at_char_boundary(line, max)
    )
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

fn apply_line_range(text: String, range: Option<(usize, usize)>) -> String {
    let Some((start, end)) = range else {
        return text;
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
            | ".venv"
            | ".tox"
            | ".mypy_cache"
            | ".pytest_cache"
            | ".ruff_cache"
    ) || is_sensitive_dir_name(&name.to_lowercase(), cfg!(windows))
        // A Python virtualenv under any name (`venv`, `env`, `.py311`).
        || path.join("pyvenv.cfg").is_file()
}

mod walk;
use walk::*;
pub use walk::{project_files, rank_by_name, visible_entries};

// ── to-do store and Python tools ─────────────────────────────────────────

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

// `python3` on a stock Windows install is a Microsoft Store alias that opens
// the Store instead of running; prefer `python` there when `python3` fails.
// python_tool runs project code like run_command does, so it gets the same
// sandbox policy; `require` without a backend refuses it.
fn python_tool_command(
    mode: crate::sandbox::Mode,
    script: &Path,
    cwd: &Path,
) -> Result<Command, String> {
    let line = format!(
        "{} {}",
        python_interpreter(),
        shell_quote(&script.to_string_lossy())
    );
    if let Some(c) = crate::sandbox::wrap_in(mode, &line, cwd)? {
        return Ok(c);
    }
    let mut command = Command::new(python_interpreter());
    command.arg(script).current_dir(cwd);
    crate::sandbox::scrub_credentials(&mut command);
    Ok(command)
}

fn python_interpreter() -> &'static str {
    static PY: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    PY.get_or_init(|| {
        if cfg!(windows) {
            let ok = Command::new("python3")
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if !ok {
                return "python";
            }
        }
        "python3"
    })
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

mod docx;
use docx::*;

// ── search limits and command capture ────────────────────────────────────

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
    // Bytes dropped from the middle of stdout and stderr while the command
    // ran (see BoundedBuf); zero unless a stream outgrew what is kept.
    omitted: usize,
}

// Bytes of each stream kept from its start and from its end while a command
// runs. The model only ever sees MAX_OUT of head and tail (command_outcome),
// so keeping MAX_OUT at each end changes nothing it sees, and a `yes` left
// running until the deadline cannot grow memory.
const CAPTURE_KEEP: usize = MAX_OUT;

// A byte sink that keeps the first `head_cap` and the last `tail_cap` bytes
// written to it and counts what fell out between them.
struct BoundedBuf {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    head_cap: usize,
    tail_cap: usize,
    omitted: usize,
}

impl BoundedBuf {
    fn new(head_cap: usize, tail_cap: usize) -> Self {
        BoundedBuf {
            head: Vec::new(),
            tail: std::collections::VecDeque::new(),
            head_cap,
            tail_cap,
            omitted: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        let room = self
            .head_cap
            .saturating_sub(self.head.len())
            .min(chunk.len());
        self.head.extend_from_slice(&chunk[..room]);
        let rest = &chunk[room..];
        if rest.len() >= self.tail_cap {
            self.omitted += self.tail.len() + rest.len() - self.tail_cap;
            self.tail.clear();
            self.tail.extend(&rest[rest.len() - self.tail_cap..]);
            return;
        }
        self.tail.extend(rest);
        let excess = self.tail.len().saturating_sub(self.tail_cap);
        if excess > 0 {
            self.tail.drain(..excess);
            self.omitted += excess;
        }
    }

    // Head and tail joined. When bytes were omitted the join is a seam in the
    // middle, which command_outcome's head/tail truncation always cuts away
    // because each end kept here is at least as long as what it shows.
    fn into_parts(self) -> (Vec<u8>, usize) {
        let mut out = self.head;
        out.extend(self.tail);
        (out, self.omitted)
    }
}

// Kill the command and everything it started. run_with_timeout puts the
// child in its own process group on unix, so `sh -c 'make'` takes make and
// its compilers down with it instead of leaving them running (and holding
// the output pipes open) after the shell is gone.
fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        if let Ok(pid) = libc::pid_t::try_from(child.id()) {
            // SAFETY: kill(2) with a negative pid signals that process group
            // and touches no memory of ours. The group is the child's own
            // (process_group(0) at spawn), and the child is not yet reaped,
            // so its id cannot have been reused.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
    #[cfg(windows)]
    {
        // Best effort: taskkill /T walks the child's descendants. When it is
        // missing or fails, child.kill() below still ends the direct child.
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

// Run a command with piped stdio and a hard deadline. `Command::output()`
// blocks forever on hanging commands; here stdout/stderr are drained on
// threads while the parent polls `try_wait`, killing the child's whole
// process group on expiry and returning whatever partial output was kept.
fn run_with_timeout(
    cmd: Command,
    stdin_payload: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<CommandCapture, String> {
    run_with_timeout_keeping(cmd, stdin_payload, timeout, CAPTURE_KEEP, CAPTURE_KEEP)
}

// run_with_timeout with explicit per-stream head/tail sizes, for a caller
// that needs more than the model-facing MAX_OUT (pdftotext feeds line ranges).
fn run_with_timeout_keeping(
    mut cmd: Command,
    stdin_payload: Option<Vec<u8>>,
    timeout: Duration,
    head_keep: usize,
    tail_keep: usize,
) -> Result<CommandCapture, String> {
    // stdin is never the terminal (null, or a pipe for the payload), so the
    // separate process group below cannot strand a command waiting on the
    // TUI's tty for input it would otherwise have stolen.
    cmd.stdin(if stdin_payload.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn().map_err(|e| format!("failed to spawn: {e}"))?;
    if let Some(payload) = stdin_payload {
        if let Some(mut stdin) = child.stdin.take() {
            // Written on a thread so a child that never reads its input (or
            // reads it slowly) cannot block us before the deadline starts;
            // killing it breaks the pipe and ends the write. Dropping stdin
            // at the end closes it so the child sees EOF.
            std::thread::spawn(move || {
                let _ = std::io::Write::write_all(&mut stdin, &payload);
            });
        }
    }
    // Reader threads forward chunks over channels instead of being joined:
    // a killed shell can leave grandchildren holding the pipe write end, and
    // joining a blocked read_to_end would hang exactly like Command::output().
    // The channel is bounded so a fast writer waits on us instead of queueing
    // gigabytes between two polls.
    fn drain<R: std::io::Read + Send + 'static>(
        pipe: Option<R>,
    ) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
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
    fn collect(rx: &std::sync::mpsc::Receiver<Vec<u8>>, out: &mut BoundedBuf, grace: Duration) {
        let deadline = std::time::Instant::now() + grace;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(chunk) => out.push(&chunk),
                Err(_) => break, // disconnected (EOF) or grace expired
            }
        }
    }
    let out_rx = drain(child.stdout.take());
    let err_rx = drain(child.stderr.take());
    let mut stdout_buf = BoundedBuf::new(head_keep, tail_keep);
    let mut stderr_buf = BoundedBuf::new(head_keep, tail_keep);
    let deadline = std::time::Instant::now() + timeout;
    let mut timed_out = false;
    let code = loop {
        while let Ok(chunk) = out_rx.try_recv() {
            stdout_buf.push(&chunk);
        }
        while let Ok(chunk) = err_rx.try_recv() {
            stderr_buf.push(&chunk);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {
                if crate::tui::interrupted() {
                    kill_process_tree(&mut child);
                    return Err("command interrupted by user".to_string());
                }
                if std::time::Instant::now() >= deadline {
                    timed_out = true;
                    kill_process_tree(&mut child);
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("failed to wait for command: {e}")),
        }
    };
    let grace = Duration::from_millis(250);
    collect(&out_rx, &mut stdout_buf, grace);
    collect(&err_rx, &mut stderr_buf, grace);
    let (stdout_bytes, out_omitted) = stdout_buf.into_parts();
    let (stderr_bytes, err_omitted) = stderr_buf.into_parts();
    Ok(CommandCapture {
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        code,
        timed_out,
        omitted: out_omitted + err_omitted,
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
            content: truncate_head_tail_omitting(s, MAX_OUT, cap.omitted),
            is_error: true,
            finished: false,
            images: Vec::new(),
        };
    }
    let code = cap.code.unwrap_or(-1);
    s.push_str(&format!("\n[exit {code}]"));
    Outcome {
        content: truncate_head_tail_omitting(s, MAX_OUT, cap.omitted),
        is_error: code != 0,
        finished: false,
        images: Vec::new(),
    }
}

// ── check_work ────────────────────────────────────────────────────────────────
// Tests/builds can be slow, so this gets a longer leash than a normal command.
const CHECK_TIMEOUT: Duration = Duration::from_secs(300);

// One thing to run: a human label, a kind (build|test|lint|custom) so `only` can
// filter, and the shell command itself.
struct WorkCheck {
    label: String,
    kind: &'static str,
    cmd: String,
}

// The platform shell for `cmd` in `cwd`, wrapped in the OS sandbox when the
// session's sandbox policy confines it (sandbox.rs). Err = policy refuses.
fn shell_command(cmd: &str, cwd: &Path) -> Result<Command, String> {
    if let Some(c) = crate::sandbox::wrap(cmd, cwd)? {
        return Ok(c);
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
    // Provider keys stay with the agent: an approved `env` must not show them.
    crate::sandbox::scrub_credentials(&mut command);
    Ok(command)
}

// Script names declared in package.json (empty on any read/parse failure).
fn package_json_scripts(cwd: &Path) -> Vec<String> {
    std::fs::read_to_string(cwd.join("package.json"))
        .ok()
        .and_then(|txt| serde_json::from_str::<Value>(&txt).ok())
        .and_then(|v| {
            v.get("scripts")
                .and_then(|s| s.as_object())
                .map(|m| m.keys().cloned().collect())
        })
        .unwrap_or_default()
}

// Auto-detect the project's build/test/lint commands from marker files. Returns
// the first matching ecosystem's checks — projects aren't run through two
// toolchains at once.
fn detect_checks(cwd: &Path) -> Vec<WorkCheck> {
    let has = |f: &str| cwd.join(f).exists();
    let owned = |s: &str| s.to_string();
    if has("Cargo.toml") {
        return vec![
            WorkCheck {
                label: owned("cargo build"),
                kind: "build",
                cmd: owned("cargo build --quiet"),
            },
            WorkCheck {
                label: owned("cargo test"),
                kind: "test",
                cmd: owned("cargo test --quiet"),
            },
            WorkCheck {
                label: owned("cargo clippy"),
                kind: "lint",
                cmd: owned("cargo clippy --quiet -- -D warnings"),
            },
        ];
    }
    if has("package.json") {
        let pm = if has("pnpm-lock.yaml") {
            "pnpm"
        } else if has("yarn.lock") {
            "yarn"
        } else {
            "npm"
        };
        let scripts = package_json_scripts(cwd);
        let has_script = |s: &str| scripts.iter().any(|x| x == s);
        let mut checks = Vec::new();
        if has_script("typecheck") {
            checks.push(WorkCheck {
                label: owned("typecheck"),
                kind: "build",
                cmd: format!("{pm} run typecheck"),
            });
        }
        if has_script("build") {
            checks.push(WorkCheck {
                label: owned("build"),
                kind: "build",
                cmd: format!("{pm} run build"),
            });
        }
        if has_script("test") {
            checks.push(WorkCheck {
                label: owned("test"),
                kind: "test",
                cmd: format!("{pm} test"),
            });
        }
        if has_script("lint") {
            checks.push(WorkCheck {
                label: owned("lint"),
                kind: "lint",
                cmd: format!("{pm} run lint"),
            });
        }
        return checks;
    }
    if has("go.mod") {
        return vec![
            WorkCheck {
                label: owned("go build"),
                kind: "build",
                cmd: owned("go build ./..."),
            },
            WorkCheck {
                label: owned("go test"),
                kind: "test",
                cmd: owned("go test ./..."),
            },
            WorkCheck {
                label: owned("go vet"),
                kind: "lint",
                cmd: owned("go vet ./..."),
            },
        ];
    }
    if has("pyproject.toml")
        || has("setup.py")
        || has("setup.cfg")
        || has("requirements.txt")
        || has("tox.ini")
    {
        return vec![
            WorkCheck {
                label: owned("pytest"),
                kind: "test",
                cmd: owned("python -m pytest -q"),
            },
            WorkCheck {
                label: owned("ruff"),
                kind: "lint",
                cmd: owned("ruff check ."),
            },
        ];
    }
    Vec::new()
}

// A non-zero exit that means "the checker itself isn't installed" — reported as
// skipped, not failed, so a missing linter never masquerades as a real problem.
fn check_tool_missing(output: &str, code: Option<i32>) -> bool {
    let l = output.to_lowercase();
    code == Some(127)
        || l.contains("command not found")
        || l.contains(": not found")
        || l.contains("is not recognized as an internal or external command")
        || l.contains("no such command")
        || l.contains("no module named pytest")
}

fn run_check_work(input: &Value, cwd: &Path) -> Outcome {
    let checks = if let Some(cmd) = input["command"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        vec![WorkCheck {
            label: cmd.to_string(),
            kind: "custom",
            cmd: cmd.to_string(),
        }]
    } else {
        let only = input["only"].as_str().unwrap_or("").trim();
        let mut checks = detect_checks(cwd);
        if !only.is_empty() {
            checks.retain(|c| c.kind == only);
        }
        checks
    };

    if checks.is_empty() {
        return ok(
            "check_work: no build/test/lint commands detected for this project. \
             Pass `command` to run a specific check, or run the project's checks with run_command.",
        );
    }

    let mut sections: Vec<String> = Vec::new();
    let mut any_fail = false;
    let mut ran = 0usize;
    for c in &checks {
        let cap = match shell_command(&c.cmd, cwd)
            .and_then(|command| run_with_timeout(command, None, CHECK_TIMEOUT))
        {
            Ok(cap) => cap,
            Err(e) => {
                any_fail = true;
                sections.push(format!("✗ {} — could not run: {e} (`{}`)", c.label, c.cmd));
                continue;
            }
        };
        let combined = format!("{}\n{}", cap.stdout, cap.stderr);
        if cap.timed_out {
            any_fail = true;
            sections.push(format!(
                "✗ {} — timed out after {}s (`{}`)",
                c.label,
                CHECK_TIMEOUT.as_secs(),
                c.cmd
            ));
        } else if check_tool_missing(&combined, cap.code) {
            sections.push(format!(
                "– {} — skipped (checker not installed: `{}`)",
                c.label, c.cmd
            ));
        } else if cap.code == Some(0) {
            ran += 1;
            sections.push(format!("✓ {} passed (`{}`)", c.label, c.cmd));
        } else {
            ran += 1;
            any_fail = true;
            let tail = truncate_head_tail(combined.trim().to_string(), 1500);
            sections.push(format!(
                "✗ {} FAILED (exit {}, `{}`)\n{}",
                c.label,
                cap.code.unwrap_or(-1),
                c.cmd,
                tail
            ));
        }
    }

    let header = if any_fail {
        "check_work: FAILED — fix what's below and run check_work again."
    } else if ran == 0 {
        "check_work: nothing ran (every checker was missing)."
    } else {
        "check_work: all checks passed."
    };
    Outcome {
        content: truncate_head_tail(format!("{header}\n\n{}", sections.join("\n\n")), MAX_OUT),
        is_error: any_fail,
        finished: false,
        images: Vec::new(),
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

// Small edit distance for "did you mean" suggestions on unknown tool names
// (and unknown command-line options).
pub(crate) fn levenshtein(a: &str, b: &str) -> usize {
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

// `isError: true` from the server is a tool error, like any local failure.
// ── pictures and PDFs ────────────────────────────────────────────────────────

// The picture formats every vision API takes, by extension.
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];
// The largest picture sent to a model: the Anthropic API's per-image limit.
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const PDF_TIMEOUT: Duration = Duration::from_secs(60);

/// Why a picture of `len` bytes is not sent to a model, if it is too big:
/// the same limit for read_file and a picture attached in the prompt. The
/// caller ends the sentence ("… and read that.").
pub(crate) fn image_too_big(shown: &str, len: u64) -> Option<String> {
    (len > MAX_IMAGE_BYTES).then(|| {
        format!(
            "{shown} is {}; pictures over 5 MB are not sent to the model. Make a smaller copy \
             (for example `ffmpeg -i {shown} -vf scale=1280:-1 small.png`)",
            human_bytes(len)
        )
    })
}

// read_file on a picture or a PDF. None for anything else, a file that is
// not there (the usual error follows), and a file whose bytes are not what
// its extension says (it is read as text).
fn read_media(p: &Path, input: &Value, cwd: &Path) -> Option<Outcome> {
    let ext = p.extension()?.to_str()?.to_ascii_lowercase();
    if ext == "pdf" {
        return read_pdf(p, input);
    }
    if !IMAGE_EXTS.contains(&ext.as_str()) {
        return None;
    }
    let meta = fs::metadata(p).ok().filter(|m| m.is_file())?;
    let shown = display_path(p, cwd);
    if let Some(why) = image_too_big(&shown, meta.len()) {
        return Some(err(format!("{why} and read that.")));
    }
    let bytes = fs::read(p).ok()?;
    // The bytes decide the media type: APIs refuse one that does not match.
    let mime = crate::media::image_media_type(&bytes)?;
    let dims = crate::media::image_dimensions(&bytes)
        .map(|(w, h)| format!(", {w}x{h}"))
        .unwrap_or_default();
    let mut out = ok(format!(
        "image {shown} ({mime}{dims}, {})",
        human_bytes(meta.len())
    ));
    out.images
        .push((mime.to_string(), crate::media::b64_encode(&bytes)));
    Some(out)
}

// A PDF's text, through pdftotext (poppler) when it is installed.
fn read_pdf(p: &Path, input: &Value) -> Option<Outcome> {
    let mut head = [0u8; 5];
    let (mut f, _) = open_regular(p).ok()?;
    if std::io::Read::read_exact(&mut f, &mut head).is_err() || &head != b"%PDF-" {
        return None;
    }
    let Some(bin) = find_on_path("pdftotext") else {
        return Some(err(format!(
            "cannot read {} as text: it is a PDF, and pdftotext is not installed. Install poppler \
             (`apt install poppler-utils`, `brew install poppler`, or `choco install poppler` on \
             Windows), or ask the user for the text. Do not retry read_file on it.",
            p.display()
        )));
    };
    let mut cmd = Command::new(bin);
    cmd.args(["-layout", "-enc", "UTF-8", "-q"]).arg(p).arg("-");
    crate::sandbox::scrub_credentials(&mut cmd);
    // The text is cut to MAX_TEXT_FILE like any other read, but kept from the
    // start so start_line/end_line can reach past the first MAX_OUT bytes.
    let keep = MAX_TEXT_FILE as usize;
    Some(
        match run_with_timeout_keeping(cmd, None, PDF_TIMEOUT, keep, 0) {
            Ok(cap) if cap.timed_out => err(format!(
                "pdftotext did not finish reading {} within {}s",
                p.display(),
                PDF_TIMEOUT.as_secs()
            )),
            Ok(cap) if cap.code == Some(0) && cap.stdout.trim().is_empty() => ok(format!(
                "{} has no text layer (a scanned PDF?): pdftotext found nothing to read",
                p.display()
            )),
            Ok(cap) if cap.code == Some(0) => ok(truncate_read(
                apply_line_range(cap.stdout, line_range(input)),
                MAX_READ,
            )),
            Ok(cap) => err(format!(
                "pdftotext could not read {}: {}",
                p.display(),
                cap.stderr.trim()
            )),
            Err(e) => err(format!("pdftotext could not start: {e}")),
        },
    )
}

// ── screenshots ──────────────────────────────────────────────────────────────
// See screenshot.rs for how the browser is found and kept off the network.
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(45);

fn screenshot_url(input: &Value, cwd: &Path) -> Outcome {
    use crate::screenshot;
    let url = input["url"].as_str().unwrap_or("").trim();
    let parsed =
        match url::Url::parse(url) {
            Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => u,
            _ => return err(
                "screenshot_url takes an http:// or https:// URL, such as http://localhost:3000",
            ),
        };
    let url = parsed.as_str();
    let host = parsed
        .host_str()
        .unwrap_or("")
        .trim_matches(['[', ']'])
        .to_string();
    // The gate refuses these too; the tool never relies on it alone.
    let direct = if screenshot::is_loopback_host(&host) {
        None
    } else if crate::agent::network_allows(&host, cwd) {
        Some(host.as_str())
    } else {
        return err(screenshot::off_loopback_refusal(&host));
    };
    if let Some(why) = blocked_url(url) {
        return err(why);
    }
    let Some(browser) = screenshot::find_chrome() else {
        return err(
            "no Chrome, Chromium or Edge found for screenshot_url: install one, or set BWN_CHROME \
             to the browser's path (a Playwright Chromium is found on its own). Do not retry \
             until one is installed.",
        );
    };
    // Nothing listening is a server to start, not a picture of Chrome's
    // error page. One request, no redirects followed.
    let status = match web_client().get(url).call() {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => {
            return err(format!(
                "nothing answered at {url} ({e}). Start the server first (start_server), wait for \
                 it (wait_for_url), then take the screenshot."
            ))
        }
    };
    let size = |k: &str, default: u32, min: u64| {
        input[k]
            .as_u64()
            .map_or(default, |v| v.clamp(min, 2560) as u32)
    };
    let (w, h) = (
        size("width", screenshot::DEFAULT_SIZE.0, 320),
        size("height", screenshot::DEFAULT_SIZE.1, 240),
    );
    let dir = std::env::temp_dir().join(format!(
        "bwn-screenshot-{}-{}",
        std::process::id(),
        now_ms()
    ));
    if let Err(e) = fs::create_dir(&dir) {
        return err(format!("cannot make a folder for the screenshot: {e}"));
    }
    let png_path = dir.join("shot.png");
    let hole = match screenshot::Blackhole::start() {
        Ok(h) => h,
        Err(e) => {
            let _ = fs::remove_dir_all(&dir);
            return err(format!("cannot start the screenshot's network guard: {e}"));
        }
    };
    let mut cmd = Command::new(&browser);
    cmd.args(screenshot::chrome_args(
        url,
        (w, h),
        &dir.join("profile"),
        &png_path,
        hole.port,
        direct,
        running_as_root(),
    ));
    crate::sandbox::scrub_credentials(&mut cmd);
    let ran = run_with_timeout(cmd, None, SCREENSHOT_TIMEOUT);
    let png = fs::read(&png_path).ok();
    let blocked = hole.hosts();
    drop(hole);
    let _ = fs::remove_dir_all(&dir);
    let png = match (png, ran) {
        (Some(png), _) if crate::media::image_media_type(&png) == Some("image/png") => png,
        (_, Ok(cap)) if cap.timed_out => {
            return err(format!(
                "{} did not finish the screenshot within {}s",
                browser.display(),
                SCREENSHOT_TIMEOUT.as_secs()
            ))
        }
        (_, Ok(cap)) => {
            let tail: Vec<&str> = cap.stderr.lines().rev().take(5).collect();
            return err(format!(
                "{} wrote no screenshot (exit {}): {}",
                browser.display(),
                cap.code.unwrap_or(-1),
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            ));
        }
        (_, Err(e)) => return err(format!("cannot start {}: {e}", browser.display())),
    };
    if png.len() as u64 > MAX_IMAGE_BYTES {
        return err(format!(
            "the screenshot is {}, over the 5 MB a model takes: try a smaller width and height",
            human_bytes(png.len() as u64)
        ));
    }
    let mut text = format!(
        "screenshot of {url} ({w}x{h}, HTTP {status}) taken with {}",
        screenshot::browser_name(&browser)
    );
    if !blocked.is_empty() {
        text.push_str(&format!(
            "\nrequests to other hosts were blocked, so what they serve is missing: {}",
            blocked.join(", ")
        ));
    }
    let mut out = ok(text);
    out.images
        .push(("image/png".into(), crate::media::b64_encode(&png)));
    out
}

// Chrome will not start its sandbox as root (containers, CI).
fn running_as_root() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn human_bytes(n: u64) -> String {
    match n {
        0..1024 => format!("{n} bytes"),
        1024..1_048_576 => format!("{} KB", n / 1024),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

fn mcp_outcome(r: Result<(String, bool), String>) -> Outcome {
    match r {
        Ok((text, false)) => ok(truncate(text, MAX_OUT)),
        Ok((text, true)) => err(truncate(text, MAX_OUT)),
        Err(e) => err(e),
    }
}

pub fn run(name: &str, input: &Value, cwd: &Path) -> Outcome {
    // Mutating file tools never silently write outside the working directory;
    // the permission gate can confirm such calls, but the execution layer
    // refuses regardless so an unwired gate cannot leak a stray write.
    if let Some(p) = out_of_cwd_mutation(name, input, cwd) {
        return err(format!(
            "refusing to write outside the working directory: {} resolves beyond {}. Out-of-cwd writes require explicit user approval — ask the user first (question tool), and once approved perform the change with run_command, or have the user add the folder with /add-dir (--add-dir at launch) or restart the session in the target directory.",
            p.display(),
            cwd.display()
        ));
    }
    let _fetch_policy = network_host(name, input).map(|_| FetchPolicy::set(cwd));
    match name {
        "read" | "read_file" => {
            let p_str = path_arg(input).unwrap_or("").trim();
            if p_str.is_empty() {
                return err("path argument is required and cannot be empty");
            }
            let p = resolve(cwd, p_str);
            if let Some(out) = read_media(&p, input, cwd) {
                return out;
            }
            match read_text_view(&p, MAX_TEXT_FILE) {
                Ok((c, false)) => ok(truncate_read(apply_line_range(c, line_range(input)), MAX_READ)),
                Ok((c, true)) => {
                    let mut out = truncate_read(apply_line_range(c, line_range(input)), MAX_READ);
                    out.push_str(&format!(
                        "\n…[{} is larger than {}: only that much was read, so line numbers past it are not available here; use run_command (`sed -n 'A,Bp'`, `tail`) for the rest]",
                        p.display(),
                        human_bytes(MAX_TEXT_FILE)
                    ));
                    ok(out)
                }
                Err(e) => match e.kind() {
                    std::io::ErrorKind::InvalidData => err(format!(
                        "cannot read {}: contents are not valid UTF-8 — this looks like a binary file. Do not retry read_file; use run_command with a targeted extractor instead (e.g. `file`, `strings`, `xxd | head`, or a format-specific CLI).",
                        p.display()
                    )),
                    std::io::ErrorKind::PermissionDenied => err(format!(
                        "cannot read {}: permission denied — the file exists but this process lacks read access. Check ownership/permissions or ask the user to grant access; do not retry the same call unchanged.",
                        p.display()
                    )),
                    std::io::ErrorKind::InvalidInput | std::io::ErrorKind::IsADirectory => {
                        err(format!("cannot read {}: {e}", p.display()))
                    }
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
                // Read a few bytes past `max` so truncate() still sees there
                // is more and marks it, without reading the rest of the file.
                match read_text_view(&p, max as u64 + 4) {
                    Ok((c, cut)) => sections.push(format!(
                        "--- {} ---\n{}",
                        display_path(&p, cwd),
                        if cut && c.len() <= max {
                            format!("{c}\n…[truncated]")
                        } else {
                            truncate(c, max)
                        }
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
            for root in search_roots(input, cwd) {
                collect_paths(&root, &mut paths, &mut seen, include_dirs, include_files);
            }
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
            let pattern = input["pattern"].as_str().unwrap_or("").trim();
            if pattern.is_empty() {
                return err("pattern is required");
            }
            let limit = search_limit(input);
            let mut files = Vec::new();
            let mut seen = 0;
            for root in search_roots(input, cwd) {
                collect_files(&root, &mut files, &mut seen);
            }
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
        "grep" | "grep_files" => run_grep(input, cwd),
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
            let prior = read_text(&p).unwrap_or_default();
            match write_text(&p, content) {
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
            let body = match read_text(&p) {
                Ok(b) => b,
                Err(e) => return err(format!("cannot read {}: {e}", p.display())),
            };
            if old.is_empty() {
                return err("`old` text cannot be empty");
            }
            let (updated, note) = match replace_once(&body, old, new) {
                Replaced::Exact(u) => (u, String::new()),
                Replaced::Lenient(u) => (u, format!(" ({LENIENT_MATCH_NOTE})")),
                Replaced::NotFound => {
                    return err(format!(
                        "`old` text not found in {}. {}.",
                        p.display(),
                        edit_not_found_hint(&body, old)
                    ))
                }
                Replaced::Ambiguous(n) => {
                    return err(format!(
                        "`old` text is not unique — it appears {n} times in {}; add surrounding context so it matches exactly once",
                        p.display()
                    ))
                }
            };
            checkpoint::record(cwd, &p, "edit_file");
            match write_text(&p, &updated) {
                Ok(_) => {
                    crate::report::diff(&p.display().to_string(), &body, &updated);
                    ok(format!("edited {}{note}", p.display()))
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
            let mut body = match read_text(&p) {
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
            match write_text(&p, &body) {
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
            match shell_command(cmd, cwd)
                .and_then(|command| run_with_timeout(command, None, COMMAND_TIMEOUT))
            {
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
                    // The person sees the list as a checklist that ticks
                    // items off as later calls mark them completed.
                    if !crate::report::is_json() {
                        crate::tui::line(&crate::tui::todo_checklist(&todos));
                    }
                    crate::report::todos(&json!(todos
                        .iter()
                        .map(|(task, status)| json!({"task": task, "status": status}))
                        .collect::<Vec<_>>()));
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
            match write_atomic(&p, &bytes) {
                Ok(_) => ok(format!("created {} ({} bytes)", p.display(), bytes.len())),
                Err(e) => err(format!("cannot write {}: {e}", p.display())),
            }
        }
        "webfetch" | "fetch_url" => {
            let url = input["url"].as_str().unwrap_or("");
            if url.is_empty() {
                return err("url is required");
            }
            if let Some(why) = blocked_url(url) {
                return err(why);
            }
            match http_get_with_retry(url, None) {
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
                &search_url,
                Some("Mozilla/5.0 (compatible; buildwithnexus/1.0)"),
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
        "headless_browser" => run_headless_browser(input),
        "start_server" => start_server(input, cwd),
        "list_servers" => list_servers(),
        "stop_server" => stop_server(input),
        "read_server_log" => read_server_log(input),
        "wait_for_url" => wait_for_url(input),
        "screenshot_url" => screenshot_url(input, cwd),
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
            let command = match python_tool_command(crate::sandbox::mode(), &path, cwd) {
                Ok(c) => c,
                Err(e) => return err(e),
            };
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
            let rows = crate::config::discover_skills(cwd)
                .into_iter()
                .map(|s| {
                    format!(
                        "{} [{}]: {}",
                        s.name,
                        s.source.label(),
                        s.description_or_default()
                    )
                })
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
                last_updated: iso8601_utc_now(),
            };
            let id = entity.id.clone();
            kb.add_entity(entity);
            if let Err(e) = kb.save() {
                return err(e);
            }
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
                tests_passed: None,
            };
            let report = verifier.verify(&ctx);
            ok(crate::verifier::Verifier::format_report(&report))
        }
        "check_work" => run_check_work(input, cwd),
        "mcp_call" => {
            let server = input["server"].as_str().unwrap_or("");
            let tool = input["tool"].as_str().unwrap_or("");
            if server.is_empty() || tool.is_empty() {
                return err("server and tool names are required");
            }
            // Legacy entry point: same persistent connection as `mcp__*`.
            crate::mcp::ensure_ready();
            mcp_outcome(crate::mcp::call(server, tool, &input["arguments"]))
        }
        n if crate::mcp::is_mcp_tool(n) => {
            let Some((server, tool)) = crate::mcp::demangle(n) else {
                return err(format!(
                    "malformed MCP tool name: {n} (expected mcp__<server>__<tool>)"
                ));
            };
            mcp_outcome(crate::mcp::call(&server, &tool, input))
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
            for skill in crate::config::discover_skills(cwd) {
                if skill.name == wanted {
                    let name = &skill.name;
                    let content = skill.loaded_text();
                    crate::trace::record_visible(
                        "skill",
                        format!("loaded {name}"),
                        json!({"name": name, "source": skill.source.label(), "bytes": content.len(), "preview": crate::trace::preview(&content, 600)}),
                    );
                    return ok(format!("# Skill: {name}\n{content}"));
                }
            }
            err(format!("skill not found: {wanted}"))
        }
        "str_replace_editor" | "text_editor_20241022" | "text_editor_20250124" => {
            run_str_replace_editor(input, cwd)
        }
        "Artifact" | "publish_artifact" => run_artifact(input, cwd),
        "finish" => Outcome {
            content: input["summary"].as_str().unwrap_or("done").to_string(),
            is_error: false,
            finished: true,
            images: Vec::new(),
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

/// `grep_files`: search file contents under a folder.
fn run_grep(input: &Value, cwd: &Path) -> Outcome {
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
    for root in search_roots(input, cwd) {
        collect_files(&root, &mut files, &mut seen);
    }
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
            let hit = if case_sensitive {
                line.contains(needle.as_str())
            } else {
                line.to_lowercase().contains(&needle)
            };
            if hit {
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

/// `headless_browser`: drive a local headless browser.
fn run_headless_browser(input: &Value) -> Outcome {
    let url = input["url"].as_str().unwrap_or("").trim();
    if url.is_empty() {
        return err("url is required");
    }
    if let Some(why) = blocked_url(url) {
        return err(why);
    }
    let extract_links = input["extract_links"].as_bool().unwrap_or(false);
    match http_get_with_retry(url, Some("Mozilla/5.0 (compatible; buildwithnexus/1.0)")) {
        Ok(resp) => match resp.into_string() {
            Ok(html) => {
                // Extract <title>
                let title = {
                    // ASCII-only lowercasing keeps byte offsets
                    // valid for slicing `html` (`İ` grows under
                    // full Unicode lowercasing).
                    let lower = html.to_ascii_lowercase();
                    if let Some(start) = lower.find("<title") {
                        let after_tag = &html[start..];
                        if let Some(gt) = after_tag.find('>') {
                            let rest = &after_tag[gt + 1..];
                            if let Some(end) = rest.to_ascii_lowercase().find("</title") {
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
                                let tag_lower = tag_content.to_ascii_lowercase();
                                if let Some(href_pos) = tag_lower.find("href=") {
                                    let after_href = &tag_content[href_pos + 5..];
                                    let href_val = if let Some(rest) = after_href.strip_prefix('"')
                                    {
                                        rest.find('"').map(|end| &rest[..end])
                                    } else if let Some(rest) = after_href.strip_prefix('\'') {
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

/// `str_replace_editor`: the text-editor tool's view, create, str_replace, insert and undo_edit.
fn run_str_replace_editor(input: &Value, cwd: &Path) -> Outcome {
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
                        let (start, end) = if let Some(range) = input["view_range"].as_array() {
                            let start =
                                range.first().and_then(|v| v.as_u64()).unwrap_or(1) as usize;
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
            match write_atomic(&p, file_text) {
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
            let body = match read_text(&p) {
                Ok(b) => b,
                Err(e) => return err(format!("cannot read {}: {e}", p.display())),
            };
            let (next, note) = match replace_once(&body, old_str, new_str) {
                Replaced::Exact(u) => (u, String::new()),
                Replaced::Lenient(u) => (u, format!(" ({LENIENT_MATCH_NOTE})")),
                Replaced::NotFound => {
                    return err(format!(
                        "old text not found in {}. {}.\n\nold text sought:\n{}",
                        p.display(),
                        edit_not_found_hint(&body, old_str),
                        old_str
                    ))
                }
                Replaced::Ambiguous(n) => {
                    return err(format!(
                        "ambiguous replacement: old text matches {n} times in {}",
                        p.display()
                    ))
                }
            };
            {
                let mut guard = UNDO_BACKUP.lock().unwrap_or_else(|e| e.into_inner());
                *guard = Some((p.clone(), body));
            }
            match write_text(&p, &next) {
                Ok(_) => ok(format!("successfully edited file {}{note}", p.display())),
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
            match write_atomic(&p, &next) {
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
                match write_atomic(&p, &old_content) {
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

/// `Artifact`: publish a self-contained local artifact.
fn run_artifact(input: &Value, cwd: &Path) -> Outcome {
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
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let filename = format!("{}_{timestamp}.{ext}", safe_title);
    let p = dir.join(&filename);
    // Both copies land in the checkout: never through a symlink.
    if let Err(e) = crate::config::write_project_file(&p, cwd, contents.as_bytes()) {
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
    if let Err(e) = crate::config::write_project_file(&direct_file, cwd, contents.as_bytes()) {
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

// Helper for web tools: retries transient HTTP errors (429, 500..=504, transport failures)
// up to 3 times with exponential backoff (500ms, 1000ms, 2000ms).
mod net_guard;
pub use net_guard::blocked_url;
use net_guard::*;

#[cfg(test)]
pub(crate) mod tests;
