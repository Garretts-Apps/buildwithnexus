//! The tool table: one row per tool name or alias, the single source of
//! truth for what `run` dispatches and how the gates classify each call.
//! Adding a tool means adding a `Tool` variant (which `run` must handle, or
//! the build fails) and its rows here; the classification functions in
//! `tools.rs` read only this table, so a new tool cannot silently skip a
//! gate that its row names.
//!
//! MCP tools (`mcp__<server>__<tool>`) are not in the table: their names
//! are discovered at run time and every function treats them on its own.

/// The handler `run` dispatches a row to. Aliases share their tool's
/// variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tool {
    ReadFile,
    ReadManyFiles,
    ListDir,
    ListTree,
    FileInfo,
    FindPaths,
    FindFiles,
    GrepFiles,
    WriteFile,
    EditFile,
    MultiEdit,
    ApplyPatch,
    CreateDir,
    MovePath,
    RemovePath,
    RunCommand,
    TodoWrite,
    TodoRead,
    CreateDocx,
    FetchUrl,
    WebSearch,
    HeadlessBrowser,
    StartServer,
    ListServers,
    StopServer,
    ReadServerLog,
    WaitForUrl,
    ScreenshotUrl,
    OpenBrowser,
    ListPythonTools,
    PythonScript,
    ListSkills,
    LoadSkill,
    KbQuery,
    KbRecord,
    RuleCheck,
    Verify,
    CheckWork,
    McpCall,
    Editor,
    Artifact,
    Finish,
    ExitPlan,
    /// Handled by the agent loop before `run` (questions, memory, helpers);
    /// `run` refuses these as unknown, as it always has.
    AgentLoop,
    /// Not a row: an `mcp__<server>__<tool>` name.
    Mcp,
    /// Not a row: a name nothing handles.
    Unknown,
}

/// Whether a call can change anything (and so passes the permission gate).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mutation {
    Never,
    Always,
    /// The text-editor tools: every `command` but `view` (the default)
    /// writes. `is_mutating` (by name alone) says no.
    UnlessView,
    /// `task`: a helper started with `read_only: true` is read-only itself
    /// (see agent::helper_reads_only). `is_mutating` (by name alone) says
    /// yes.
    UnlessReadOnlyHelper,
}

/// The host a call contacts, for per-host network approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Network {
    None,
    /// web_search: always the search engine's host.
    Search,
    /// The authority of `url`.
    Url,
    /// As `Url`, but only for http(s) URLs (screenshot_url refuses others
    /// itself).
    HttpUrl,
}

/// How to read the filesystem paths a call touches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Paths {
    None,
    /// `path` (or its spellings, see path_arg).
    PathArg,
    /// A search root (`root`/`path`/`dir`/`directory`, default `.`).
    Root,
    /// read_many_files' `paths`.
    Many,
    /// move_path's `from` and `to`, trimmed as move_path trims them.
    Move,
    /// The files a unified diff in `patch` names. Never a single
    /// touched_path.
    Patch,
}

/// Which read-before-edit tracker records a call's path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tracking {
    None,
    Read,
    Edit,
    /// The text-editor tools: `view` is a read, every other command an edit.
    Editor,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ToolSpec {
    pub name: &'static str,
    /// The tool an alias stands for; the name itself for a canonical row.
    pub canonical: &'static str,
    pub tool: Tool,
    pub mutation: Mutation,
    pub network: Network,
    pub paths: Paths,
    /// Mutating calls whose paths `run` confines to the working folders
    /// (out_of_cwd_mutation). Only `PathArg` and `Move` rows can set it.
    pub confined: bool,
    /// What `accept-edits` lets run without a prompt (is_file_edit).
    pub file_edit: bool,
    pub tracking: Tracking,
    /// Whether, and how, the call runs within a batch of pure reads.
    pub parallel: Parallel,
}

/// How a call runs when one reply asks for several pure reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Parallel {
    /// Not a pure read: runs alone, in the order the model gave.
    No,
    /// A pure read that costs less than starting a thread for it (one
    /// local file or folder): runs on the turn's own thread.
    Inline,
    /// A pure read worth a thread: a network call, or a walk of the tree.
    Spawn,
}

const fn spec(name: &'static str, tool: Tool) -> ToolSpec {
    ToolSpec {
        name,
        canonical: name,
        tool,
        mutation: Mutation::Never,
        network: Network::None,
        paths: Paths::None,
        confined: false,
        file_edit: false,
        tracking: Tracking::None,
        parallel: Parallel::No,
    }
}

impl ToolSpec {
    const fn alias_of(mut self, canonical: &'static str) -> Self {
        self.canonical = canonical;
        self
    }
    const fn mutating(mut self) -> Self {
        self.mutation = Mutation::Always;
        self
    }
    const fn mutation(mut self, m: Mutation) -> Self {
        self.mutation = m;
        self
    }
    const fn network(mut self, n: Network) -> Self {
        self.network = n;
        self
    }
    const fn paths(mut self, p: Paths) -> Self {
        self.paths = p;
        self
    }
    const fn confined(mut self) -> Self {
        self.confined = true;
        self
    }
    const fn file_edit(mut self) -> Self {
        self.file_edit = true;
        self
    }
    const fn tracking(mut self, t: Tracking) -> Self {
        self.tracking = t;
        self
    }
    const fn inline(mut self) -> Self {
        self.parallel = Parallel::Inline;
        self
    }
    const fn spawn(mut self) -> Self {
        self.parallel = Parallel::Spawn;
        self
    }
}

use Mutation::{UnlessReadOnlyHelper, UnlessView};
use Network::{HttpUrl, Search, Url};
use Paths::{Many, Move, Patch, PathArg, Root};
use Tool::*;

// Row helpers for the families that share every flag.
const fn read_file(name: &'static str) -> ToolSpec {
    spec(name, ReadFile)
        .paths(PathArg)
        .tracking(Tracking::Read)
        .inline()
}
const fn write_file(name: &'static str, tool: Tool) -> ToolSpec {
    spec(name, tool)
        .mutating()
        .paths(PathArg)
        .confined()
        .file_edit()
        .tracking(Tracking::Edit)
}
const fn editor(name: &'static str) -> ToolSpec {
    spec(name, Editor)
        .mutation(UnlessView)
        .paths(PathArg)
        .confined()
        .file_edit()
        .tracking(Tracking::Editor)
}

pub(crate) static TOOLS: &[ToolSpec] = &[
    // Reading and searching.
    read_file("read").alias_of("read_file"),
    read_file("read_file"),
    spec("read_many_files", ReadManyFiles).paths(Many).inline(),
    spec("list", ListDir)
        .alias_of("list_dir")
        .paths(PathArg)
        .inline(),
    spec("list_dir", ListDir).paths(PathArg).inline(),
    spec("list_tree", ListTree).paths(PathArg).spawn(),
    spec("file_info", FileInfo).paths(PathArg).inline(),
    spec("glob", FindPaths)
        .alias_of("find_paths")
        .paths(Root)
        .spawn(),
    spec("find_paths", FindPaths).paths(Root).spawn(),
    spec("find_files", FindFiles).paths(Root).spawn(),
    spec("grep", GrepFiles)
        .alias_of("grep_files")
        .paths(Root)
        .spawn(),
    spec("grep_files", GrepFiles).paths(Root).spawn(),
    // Changing files.
    write_file("write", WriteFile).alias_of("write_file"),
    write_file("write_file", WriteFile),
    write_file("edit", EditFile).alias_of("edit_file"),
    write_file("edit_file", EditFile),
    write_file("multi_edit", MultiEdit),
    spec("patch", ApplyPatch)
        .alias_of("apply_patch")
        .mutating()
        .paths(Patch)
        .file_edit(),
    spec("apply_patch", ApplyPatch)
        .mutating()
        .paths(Patch)
        .file_edit(),
    spec("create_dir", CreateDir)
        .mutating()
        .paths(PathArg)
        .confined()
        .file_edit(),
    spec("move_path", MovePath)
        .mutating()
        .paths(Move)
        .confined()
        .file_edit(),
    // Deleting is not a file edit: accept-edits still asks.
    spec("remove_path", RemovePath)
        .mutating()
        .paths(PathArg)
        .confined()
        .tracking(Tracking::Edit),
    spec("create_docx", CreateDocx)
        .mutating()
        .paths(PathArg)
        .confined(),
    editor("str_replace_editor"),
    editor("text_editor_20241022"),
    editor("text_editor_20250124"),
    // Commands and processes.
    spec("bash", RunCommand).alias_of("run_command").mutating(),
    spec("run_command", RunCommand).mutating(),
    spec("check_work", CheckWork).mutating(),
    spec("start_server", StartServer).mutating(),
    spec("list_servers", ListServers),
    spec("stop_server", StopServer).mutating(),
    spec("read_server_log", ReadServerLog),
    spec("list_python_tools", ListPythonTools),
    // python_tool's `path` names the tool script, not a write target.
    spec("python_tool", PythonScript).mutating().paths(PathArg),
    // Network.
    spec("webfetch", FetchUrl)
        .alias_of("fetch_url")
        .network(Url)
        .spawn(),
    spec("fetch_url", FetchUrl).network(Url).spawn(),
    spec("websearch", WebSearch)
        .alias_of("web_search")
        .network(Search)
        .spawn(),
    spec("web_search", WebSearch).network(Search).spawn(),
    spec("headless_browser", HeadlessBrowser).network(Url),
    spec("wait_for_url", WaitForUrl).network(Url),
    spec("screenshot_url", ScreenshotUrl).network(HttpUrl),
    spec("open_browser", OpenBrowser).mutating().network(Url),
    // Planning, skills, knowledge, control.
    spec("todowrite", TodoWrite)
        .alias_of("todo_write")
        .mutating(),
    spec("todo_write", TodoWrite).mutating(),
    spec("todoread", TodoRead).alias_of("todo_read"),
    spec("todo_read", TodoRead),
    spec("skill", LoadSkill).alias_of("load_skill"),
    spec("load_skill", LoadSkill),
    spec("list_skills", ListSkills),
    spec("kb_query", KbQuery).inline(),
    spec("kb_record", KbRecord).mutating(),
    spec("rule_check", RuleCheck),
    spec("verify", Verify),
    spec("mcp_call", McpCall).mutating(),
    spec("Artifact", Artifact).mutating(),
    spec("publish_artifact", Artifact)
        .alias_of("Artifact")
        .mutating(),
    spec("finish", Finish),
    spec("exit_plan", ExitPlan),
    spec("ExitPlanMode", ExitPlan).alias_of("exit_plan"),
    // Handled by the agent loop, never by `run`.
    spec("question", AgentLoop),
    spec("AskUserQuestion", AgentLoop).alias_of("question"),
    spec("save_memory", AgentLoop),
    spec("task", AgentLoop)
        .alias_of("spawn_subagent")
        .mutation(UnlessReadOnlyHelper),
    spec("spawn_subagent", AgentLoop).mutation(UnlessReadOnlyHelper),
];

/// The row for a tool name or alias; `None` for MCP and unknown names.
pub(crate) fn lookup(name: &str) -> Option<&'static ToolSpec> {
    TOOLS.iter().find(|s| s.name == name)
}
