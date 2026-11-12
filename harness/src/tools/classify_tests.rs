// Characterization of every per-tool classification function: for each tool
// name and alias, and a few representative inputs, the outputs are pinned in
// `EXPECTED`. Written before the tool table replaced the scattered matches,
// so it proves the table reproduces the old behaviour exactly.
use super::*;
use serde_json::json;
use std::fmt::Write as _;

// Every name `defs(true)` offers, plus names that classify specially.
fn names() -> Vec<String> {
    let mut v: Vec<String> = defs(true).iter().map(|d| d.name.to_string()).collect();
    v.push("mcp__x__y".to_string());
    v.push("mcp__bad".to_string());
    v.push("no_such_tool".to_string());
    v
}

fn inputs() -> Vec<(&'static str, Value)> {
    vec![
        ("empty", json!({})),
        (
            "view",
            json!({
                "command": "view",
                "path": " src/a.rs ",
                "paths": ["p1", " ", "p2"],
                "from": " a ",
                "to": " b ",
                "root": "src",
                "pattern": "*.rs",
                "url": "https://Example.COM:8080/x?q=1",
                "query": "rust",
                "read_only": true,
                "task": "look around",
                "patch": "--- a/one.txt\n+++ b/one.txt\n@@ -1 +1 @@\n-x\n+y\n",
            }),
        ),
        (
            "create",
            json!({
                "command": "create",
                "filePath": "/bwn-char-outside/f.txt",
                "paths": ["/bwn-char-outside/m"],
                "from": "in.txt",
                "to": "/bwn-char-outside/dest",
                "dir": "/bwn-char-outside",
                "url": "file:///etc/passwd",
                "read_only": false,
                "description": "build it",
                "items": [{"task": "a", "status": "pending"}],
            }),
        ),
        (
            "noread",
            json!({"url": "  ", "read_only": "true", "command": "ls -la"}),
        ),
    ]
}

fn snapshot() -> String {
    let cwd = Path::new("/bwn-char-proj");
    let mut out = String::new();
    for name in names() {
        let n = name.as_str();
        writeln!(
            out,
            "{n}: mut={} canon={} cmd={:?}",
            is_mutating(n),
            canonical_name(n),
            command_arg_for(n, &json!({"command": "ls"})),
        )
        .unwrap();
        for (id, input) in inputs() {
            writeln!(
                out,
                "  {id}: call={} edit={} net={:?} tp={:?} tps={:?} rt={:?} et={:?} oc={:?} pv={:?} al={:?}",
                is_mutating_call(n, &input),
                is_file_edit(n, &input),
                network_host(n, &input),
                touched_path(n, &input, cwd),
                touched_paths(n, &input, cwd),
                read_tracking_path(n, &input, cwd),
                edit_tracking_path(n, &input, cwd),
                out_of_cwd_mutation(n, &input, cwd),
                preview(n, &input),
                approval_label(n, &input),
            )
            .unwrap();
        }
    }
    out
}

#[test]
fn classification_matches_snapshot() {
    let got = snapshot();
    if std::env::var_os("BWN_CHAR_DUMP").is_some() {
        std::fs::write(std::env::var("BWN_CHAR_DUMP").unwrap(), &got).unwrap();
    }
    let expected = EXPECTED.trim_start_matches('\n');
    for (i, (g, e)) in got.lines().zip(expected.lines()).enumerate() {
        assert_eq!(g, e, "line {}", i + 1);
    }
    assert_eq!(got.lines().count(), expected.lines().count());
}

const EXPECTED: &str = r#"
bash: mut=true canon=run_command cmd=Some("ls")
  empty: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="run: ?" al="run: ?"
  view: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="run: view" al="run: view"
  create: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="run: create" al="run: create"
  noread: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="run: ls -la" al="run: ls -la"
read: mut=false canon=read_file cmd=None
  empty: call=false edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="read" al="read"
  view: call=false edit=false net=None tp=Some("/bwn-char-proj/src/a.rs") tps=["/bwn-char-proj/src/a.rs"] rt=Some("/bwn-char-proj/src/a.rs") et=None oc=None pv="read" al="read"
  create: call=false edit=false net=None tp=Some("/bwn-char-outside/f.txt") tps=["/bwn-char-outside/f.txt"] rt=Some("/bwn-char-outside/f.txt") et=None oc=None pv="read" al="read"
  noread: call=false edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="read" al="read"
write: mut=true canon=write_file cmd=None
  empty: call=true edit=true net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="write ?" al="write ?"
  view: call=true edit=true net=None tp=Some("/bwn-char-proj/src/a.rs") tps=["/bwn-char-proj/src/a.rs"] rt=None et=Some("/bwn-char-proj/src/a.rs") oc=None pv="write src/a.rs" al="write src/a.rs"
  create: call=true edit=true net=None tp=Some("/bwn-char-outside/f.txt") tps=["/bwn-char-outside/f.txt"] rt=None et=Some("/bwn-char-outside/f.txt") oc=Some("/bwn-char-outside/f.txt") pv="write /bwn-char-outside/f.txt" al="write /bwn-char-outside/f.txt"
  noread: call=true edit=true net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="write ?" al="write ?"
edit: mut=true canon=edit_file cmd=None
  empty: call=true edit=true net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="edit ?" al="edit ?"
  view: call=true edit=true net=None tp=Some("/bwn-char-proj/src/a.rs") tps=["/bwn-char-proj/src/a.rs"] rt=None et=Some("/bwn-char-proj/src/a.rs") oc=None pv="edit src/a.rs" al="edit src/a.rs"
  create: call=true edit=true net=None tp=Some("/bwn-char-outside/f.txt") tps=["/bwn-char-outside/f.txt"] rt=None et=Some("/bwn-char-outside/f.txt") oc=Some("/bwn-char-outside/f.txt") pv="edit /bwn-char-outside/f.txt" al="edit /bwn-char-outside/f.txt"
  noread: call=true edit=true net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="edit ?" al="edit ?"
patch: mut=true canon=apply_patch cmd=None
  empty: call=true edit=true net=None tp=None tps=[] rt=None et=None oc=None pv="apply patch" al="apply patch"
  view: call=true edit=true net=None tp=None tps=["/bwn-char-proj/one.txt"] rt=None et=None oc=None pv="apply patch" al="apply patch"
  create: call=true edit=true net=None tp=None tps=[] rt=None et=None oc=None pv="apply patch" al="apply patch"
  noread: call=true edit=true net=None tp=None tps=[] rt=None et=None oc=None pv="apply patch" al="apply patch"
glob: mut=false canon=find_paths cmd=None
  empty: call=false edit=false net=None tp=Some("/bwn-char-proj/.") tps=["/bwn-char-proj/."] rt=None et=None oc=None pv="find paths: ?" al="find paths: ?"
  view: call=false edit=false net=None tp=Some("/bwn-char-proj/src") tps=["/bwn-char-proj/src"] rt=None et=None oc=None pv="find paths: *.rs" al="find paths: *.rs"
  create: call=false edit=false net=None tp=Some("/bwn-char-outside") tps=["/bwn-char-outside"] rt=None et=None oc=None pv="find paths: ?" al="find paths: ?"
  noread: call=false edit=false net=None tp=Some("/bwn-char-proj/.") tps=["/bwn-char-proj/."] rt=None et=None oc=None pv="find paths: ?" al="find paths: ?"
grep: mut=false canon=grep_files cmd=None
  empty: call=false edit=false net=None tp=Some("/bwn-char-proj/.") tps=["/bwn-char-proj/."] rt=None et=None oc=None pv="grep: ?" al="grep: ?"
  view: call=false edit=false net=None tp=Some("/bwn-char-proj/src") tps=["/bwn-char-proj/src"] rt=None et=None oc=None pv="grep: *.rs" al="grep: *.rs"
  create: call=false edit=false net=None tp=Some("/bwn-char-outside") tps=["/bwn-char-outside"] rt=None et=None oc=None pv="grep: ?" al="grep: ?"
  noread: call=false edit=false net=None tp=Some("/bwn-char-proj/.") tps=["/bwn-char-proj/."] rt=None et=None oc=None pv="grep: ?" al="grep: ?"
list: mut=false canon=list_dir cmd=None
  empty: call=false edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="list" al="list"
  view: call=false edit=false net=None tp=Some("/bwn-char-proj/src/a.rs") tps=["/bwn-char-proj/src/a.rs"] rt=None et=None oc=None pv="list" al="list"
  create: call=false edit=false net=None tp=Some("/bwn-char-outside/f.txt") tps=["/bwn-char-outside/f.txt"] rt=None et=None oc=None pv="list" al="list"
  noread: call=false edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="list" al="list"
webfetch: mut=false canon=fetch_url cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="GET ?" al="GET ?"
  view: call=false edit=false net=Some("example.com:8080") tp=None tps=[] rt=None et=None oc=None pv="GET https://Example.COM:8080/x?q=1" al="GET https://Example.COM:8080/x?q=1"
  create: call=false edit=false net=Some("?") tp=None tps=[] rt=None et=None oc=None pv="GET file:///etc/passwd" al="GET file:///etc/passwd"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="GET" al="GET"
websearch: mut=false canon=web_search cmd=None
  empty: call=false edit=false net=Some("lite.duckduckgo.com") tp=None tps=[] rt=None et=None oc=None pv="web search: ?" al="web search: ?"
  view: call=false edit=false net=Some("lite.duckduckgo.com") tp=None tps=[] rt=None et=None oc=None pv="web search: rust" al="web search: rust"
  create: call=false edit=false net=Some("lite.duckduckgo.com") tp=None tps=[] rt=None et=None oc=None pv="web search: ?" al="web search: ?"
  noread: call=false edit=false net=Some("lite.duckduckgo.com") tp=None tps=[] rt=None et=None oc=None pv="web search: ?" al="web search: ?"
todowrite: mut=true canon=todo_write cmd=None
  empty: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="todo list" al="todo list"
  view: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="todo list" al="todo list"
  create: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="todo list: 1 item" al="todo list: 1 item"
  noread: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="todo list" al="todo list"
todoread: mut=false canon=todo_read cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="todoread" al="todoread"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="todoread" al="todoread"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="todoread" al="todoread"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="todoread" al="todoread"
skill: mut=false canon=load_skill cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="load skill: ?" al="load skill: ?"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="load skill: ?" al="load skill: ?"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="load skill: ?" al="load skill: ?"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="load skill: ?" al="load skill: ?"
question: mut=false canon=question cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="question" al="question"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="question" al="question"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="question" al="question"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="question" al="question"
AskUserQuestion: mut=false canon=question cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="AskUserQuestion" al="AskUserQuestion"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="AskUserQuestion" al="AskUserQuestion"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="AskUserQuestion" al="AskUserQuestion"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="AskUserQuestion" al="AskUserQuestion"
exit_plan: mut=false canon=exit_plan cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="exit plan mode" al="exit plan mode"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="exit plan mode" al="exit plan mode"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="exit plan mode" al="exit plan mode"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="exit plan mode" al="exit plan mode"
ExitPlanMode: mut=false canon=exit_plan cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="exit plan mode" al="exit plan mode"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="exit plan mode" al="exit plan mode"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="exit plan mode" al="exit plan mode"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="exit plan mode" al="exit plan mode"
str_replace_editor: mut=false canon=str_replace_editor cmd=None
  empty: call=false edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="editor (view) ?" al="editor (view) ?"
  view: call=false edit=false net=None tp=Some("/bwn-char-proj/src/a.rs") tps=["/bwn-char-proj/src/a.rs"] rt=Some("/bwn-char-proj/src/a.rs") et=None oc=None pv="editor (view) src/a.rs" al="editor (view) src/a.rs"
  create: call=true edit=true net=None tp=Some("/bwn-char-outside/f.txt") tps=["/bwn-char-outside/f.txt"] rt=None et=Some("/bwn-char-outside/f.txt") oc=Some("/bwn-char-outside/f.txt") pv="editor (create) /bwn-char-outside/f.txt" al="editor (create) /bwn-char-outside/f.txt"
  noread: call=true edit=true net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="editor (ls -la) ?" al="editor (ls -la) ?"
text_editor_20241022: mut=false canon=text_editor_20241022 cmd=None
  empty: call=false edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="editor (view) ?" al="editor (view) ?"
  view: call=false edit=false net=None tp=Some("/bwn-char-proj/src/a.rs") tps=["/bwn-char-proj/src/a.rs"] rt=Some("/bwn-char-proj/src/a.rs") et=None oc=None pv="editor (view) src/a.rs" al="editor (view) src/a.rs"
  create: call=true edit=true net=None tp=Some("/bwn-char-outside/f.txt") tps=["/bwn-char-outside/f.txt"] rt=None et=Some("/bwn-char-outside/f.txt") oc=Some("/bwn-char-outside/f.txt") pv="editor (create) /bwn-char-outside/f.txt" al="editor (create) /bwn-char-outside/f.txt"
  noread: call=true edit=true net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="editor (ls -la) ?" al="editor (ls -la) ?"
text_editor_20250124: mut=false canon=text_editor_20250124 cmd=None
  empty: call=false edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="editor (view) ?" al="editor (view) ?"
  view: call=false edit=false net=None tp=Some("/bwn-char-proj/src/a.rs") tps=["/bwn-char-proj/src/a.rs"] rt=Some("/bwn-char-proj/src/a.rs") et=None oc=None pv="editor (view) src/a.rs" al="editor (view) src/a.rs"
  create: call=true edit=true net=None tp=Some("/bwn-char-outside/f.txt") tps=["/bwn-char-outside/f.txt"] rt=None et=Some("/bwn-char-outside/f.txt") oc=Some("/bwn-char-outside/f.txt") pv="editor (create) /bwn-char-outside/f.txt" al="editor (create) /bwn-char-outside/f.txt"
  noread: call=true edit=true net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="editor (ls -la) ?" al="editor (ls -la) ?"
Artifact: mut=true canon=Artifact cmd=None
  empty: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="publish artifact: untitled" al="publish artifact: untitled"
  view: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="publish artifact: untitled" al="publish artifact: untitled"
  create: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="publish artifact: untitled" al="publish artifact: untitled"
  noread: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="publish artifact: untitled" al="publish artifact: untitled"
publish_artifact: mut=true canon=Artifact cmd=None
  empty: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="publish artifact: untitled" al="publish artifact: untitled"
  view: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="publish artifact: untitled" al="publish artifact: untitled"
  create: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="publish artifact: untitled" al="publish artifact: untitled"
  noread: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="publish artifact: untitled" al="publish artifact: untitled"
list_python_tools: mut=false canon=list_python_tools cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="list_python_tools" al="list_python_tools"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="list_python_tools" al="list_python_tools"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="list_python_tools" al="list_python_tools"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="list_python_tools" al="list_python_tools"
python_tool: mut=true canon=python_tool cmd=None
  empty: call=true edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="python tool: ?" al="python tool: ?"
  view: call=true edit=false net=None tp=Some("/bwn-char-proj/src/a.rs") tps=["/bwn-char-proj/src/a.rs"] rt=None et=None oc=None pv="python tool: src/a.rs" al="python tool: src/a.rs"
  create: call=true edit=false net=None tp=Some("/bwn-char-outside/f.txt") tps=["/bwn-char-outside/f.txt"] rt=None et=None oc=None pv="python tool: ?" al="python tool: ?"
  noread: call=true edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="python tool: ?" al="python tool: ?"
read_file: mut=false canon=read_file cmd=None
  empty: call=false edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="read_file" al="read_file"
  view: call=false edit=false net=None tp=Some("/bwn-char-proj/src/a.rs") tps=["/bwn-char-proj/src/a.rs"] rt=Some("/bwn-char-proj/src/a.rs") et=None oc=None pv="read_file" al="read_file"
  create: call=false edit=false net=None tp=Some("/bwn-char-outside/f.txt") tps=["/bwn-char-outside/f.txt"] rt=Some("/bwn-char-outside/f.txt") et=None oc=None pv="read_file" al="read_file"
  noread: call=false edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="read_file" al="read_file"
read_many_files: mut=false canon=read_many_files cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="read_many_files" al="read_many_files"
  view: call=false edit=false net=None tp=Some("/bwn-char-proj/p1") tps=["/bwn-char-proj/p1", "/bwn-char-proj/p2"] rt=None et=None oc=None pv="read_many_files" al="read_many_files"
  create: call=false edit=false net=None tp=Some("/bwn-char-outside/m") tps=["/bwn-char-outside/m"] rt=None et=None oc=None pv="read_many_files" al="read_many_files"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="read_many_files" al="read_many_files"
list_dir: mut=false canon=list_dir cmd=None
  empty: call=false edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="list_dir" al="list_dir"
  view: call=false edit=false net=None tp=Some("/bwn-char-proj/src/a.rs") tps=["/bwn-char-proj/src/a.rs"] rt=None et=None oc=None pv="list_dir" al="list_dir"
  create: call=false edit=false net=None tp=Some("/bwn-char-outside/f.txt") tps=["/bwn-char-outside/f.txt"] rt=None et=None oc=None pv="list_dir" al="list_dir"
  noread: call=false edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="list_dir" al="list_dir"
list_tree: mut=false canon=list_tree cmd=None
  empty: call=false edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="list_tree" al="list_tree"
  view: call=false edit=false net=None tp=Some("/bwn-char-proj/src/a.rs") tps=["/bwn-char-proj/src/a.rs"] rt=None et=None oc=None pv="list_tree" al="list_tree"
  create: call=false edit=false net=None tp=Some("/bwn-char-outside/f.txt") tps=["/bwn-char-outside/f.txt"] rt=None et=None oc=None pv="list_tree" al="list_tree"
  noread: call=false edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="list_tree" al="list_tree"
file_info: mut=false canon=file_info cmd=None
  empty: call=false edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="file_info" al="file_info"
  view: call=false edit=false net=None tp=Some("/bwn-char-proj/src/a.rs") tps=["/bwn-char-proj/src/a.rs"] rt=None et=None oc=None pv="file_info" al="file_info"
  create: call=false edit=false net=None tp=Some("/bwn-char-outside/f.txt") tps=["/bwn-char-outside/f.txt"] rt=None et=None oc=None pv="file_info" al="file_info"
  noread: call=false edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="file_info" al="file_info"
find_paths: mut=false canon=find_paths cmd=None
  empty: call=false edit=false net=None tp=Some("/bwn-char-proj/.") tps=["/bwn-char-proj/."] rt=None et=None oc=None pv="find paths: ?" al="find paths: ?"
  view: call=false edit=false net=None tp=Some("/bwn-char-proj/src") tps=["/bwn-char-proj/src"] rt=None et=None oc=None pv="find paths: *.rs" al="find paths: *.rs"
  create: call=false edit=false net=None tp=Some("/bwn-char-outside") tps=["/bwn-char-outside"] rt=None et=None oc=None pv="find paths: ?" al="find paths: ?"
  noread: call=false edit=false net=None tp=Some("/bwn-char-proj/.") tps=["/bwn-char-proj/."] rt=None et=None oc=None pv="find paths: ?" al="find paths: ?"
find_files: mut=false canon=find_files cmd=None
  empty: call=false edit=false net=None tp=Some("/bwn-char-proj/.") tps=["/bwn-char-proj/."] rt=None et=None oc=None pv="find files: ?" al="find files: ?"
  view: call=false edit=false net=None tp=Some("/bwn-char-proj/src") tps=["/bwn-char-proj/src"] rt=None et=None oc=None pv="find files: *.rs" al="find files: *.rs"
  create: call=false edit=false net=None tp=Some("/bwn-char-outside") tps=["/bwn-char-outside"] rt=None et=None oc=None pv="find files: ?" al="find files: ?"
  noread: call=false edit=false net=None tp=Some("/bwn-char-proj/.") tps=["/bwn-char-proj/."] rt=None et=None oc=None pv="find files: ?" al="find files: ?"
grep_files: mut=false canon=grep_files cmd=None
  empty: call=false edit=false net=None tp=Some("/bwn-char-proj/.") tps=["/bwn-char-proj/."] rt=None et=None oc=None pv="grep: ?" al="grep: ?"
  view: call=false edit=false net=None tp=Some("/bwn-char-proj/src") tps=["/bwn-char-proj/src"] rt=None et=None oc=None pv="grep: *.rs" al="grep: *.rs"
  create: call=false edit=false net=None tp=Some("/bwn-char-outside") tps=["/bwn-char-outside"] rt=None et=None oc=None pv="grep: ?" al="grep: ?"
  noread: call=false edit=false net=None tp=Some("/bwn-char-proj/.") tps=["/bwn-char-proj/."] rt=None et=None oc=None pv="grep: ?" al="grep: ?"
write_file: mut=true canon=write_file cmd=None
  empty: call=true edit=true net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="write ?" al="write ?"
  view: call=true edit=true net=None tp=Some("/bwn-char-proj/src/a.rs") tps=["/bwn-char-proj/src/a.rs"] rt=None et=Some("/bwn-char-proj/src/a.rs") oc=None pv="write src/a.rs" al="write src/a.rs"
  create: call=true edit=true net=None tp=Some("/bwn-char-outside/f.txt") tps=["/bwn-char-outside/f.txt"] rt=None et=Some("/bwn-char-outside/f.txt") oc=Some("/bwn-char-outside/f.txt") pv="write /bwn-char-outside/f.txt" al="write /bwn-char-outside/f.txt"
  noread: call=true edit=true net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="write ?" al="write ?"
edit_file: mut=true canon=edit_file cmd=None
  empty: call=true edit=true net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="edit ?" al="edit ?"
  view: call=true edit=true net=None tp=Some("/bwn-char-proj/src/a.rs") tps=["/bwn-char-proj/src/a.rs"] rt=None et=Some("/bwn-char-proj/src/a.rs") oc=None pv="edit src/a.rs" al="edit src/a.rs"
  create: call=true edit=true net=None tp=Some("/bwn-char-outside/f.txt") tps=["/bwn-char-outside/f.txt"] rt=None et=Some("/bwn-char-outside/f.txt") oc=Some("/bwn-char-outside/f.txt") pv="edit /bwn-char-outside/f.txt" al="edit /bwn-char-outside/f.txt"
  noread: call=true edit=true net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="edit ?" al="edit ?"
multi_edit: mut=true canon=multi_edit cmd=None
  empty: call=true edit=true net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="multi-edit ?" al="multi-edit ?"
  view: call=true edit=true net=None tp=Some("/bwn-char-proj/src/a.rs") tps=["/bwn-char-proj/src/a.rs"] rt=None et=Some("/bwn-char-proj/src/a.rs") oc=None pv="multi-edit src/a.rs" al="multi-edit src/a.rs"
  create: call=true edit=true net=None tp=Some("/bwn-char-outside/f.txt") tps=["/bwn-char-outside/f.txt"] rt=None et=Some("/bwn-char-outside/f.txt") oc=Some("/bwn-char-outside/f.txt") pv="multi-edit ?" al="multi-edit ?"
  noread: call=true edit=true net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="multi-edit ?" al="multi-edit ?"
apply_patch: mut=true canon=apply_patch cmd=None
  empty: call=true edit=true net=None tp=None tps=[] rt=None et=None oc=None pv="apply patch" al="apply patch"
  view: call=true edit=true net=None tp=None tps=["/bwn-char-proj/one.txt"] rt=None et=None oc=None pv="apply patch" al="apply patch"
  create: call=true edit=true net=None tp=None tps=[] rt=None et=None oc=None pv="apply patch" al="apply patch"
  noread: call=true edit=true net=None tp=None tps=[] rt=None et=None oc=None pv="apply patch" al="apply patch"
create_dir: mut=true canon=create_dir cmd=None
  empty: call=true edit=true net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="mkdir ?" al="mkdir ?"
  view: call=true edit=true net=None tp=Some("/bwn-char-proj/src/a.rs") tps=["/bwn-char-proj/src/a.rs"] rt=None et=None oc=None pv="mkdir src/a.rs" al="mkdir src/a.rs"
  create: call=true edit=true net=None tp=Some("/bwn-char-outside/f.txt") tps=["/bwn-char-outside/f.txt"] rt=None et=None oc=Some("/bwn-char-outside/f.txt") pv="mkdir ?" al="mkdir ?"
  noread: call=true edit=true net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="mkdir ?" al="mkdir ?"
move_path: mut=true canon=move_path cmd=None
  empty: call=true edit=true net=None tp=Some("/bwn-char-proj/") tps=[] rt=None et=None oc=None pv="move ? -> ?" al="move ? -> ?"
  view: call=true edit=true net=None tp=Some("/bwn-char-proj/a") tps=["/bwn-char-proj/a", "/bwn-char-proj/b"] rt=None et=None oc=None pv="move a -> b" al="move a -> b"
  create: call=true edit=true net=None tp=Some("/bwn-char-proj/in.txt") tps=["/bwn-char-proj/in.txt", "/bwn-char-outside/dest"] rt=None et=None oc=Some("/bwn-char-outside/dest") pv="move in.txt -> /bwn-char-outside/dest" al="move in.txt -> /bwn-char-outside/dest"
  noread: call=true edit=true net=None tp=Some("/bwn-char-proj/") tps=[] rt=None et=None oc=None pv="move ? -> ?" al="move ? -> ?"
remove_path: mut=true canon=remove_path cmd=None
  empty: call=true edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="remove ?" al="remove ?"
  view: call=true edit=false net=None tp=Some("/bwn-char-proj/src/a.rs") tps=["/bwn-char-proj/src/a.rs"] rt=None et=Some("/bwn-char-proj/src/a.rs") oc=None pv="remove src/a.rs" al="remove src/a.rs"
  create: call=true edit=false net=None tp=Some("/bwn-char-outside/f.txt") tps=["/bwn-char-outside/f.txt"] rt=None et=Some("/bwn-char-outside/f.txt") oc=Some("/bwn-char-outside/f.txt") pv="remove ?" al="remove ?"
  noread: call=true edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="remove ?" al="remove ?"
run_command: mut=true canon=run_command cmd=Some("ls")
  empty: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="run: ?" al="run: ?"
  view: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="run: view" al="run: view"
  create: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="run: create" al="run: create"
  noread: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="run: ls -la" al="run: ls -la"
todo_write: mut=true canon=todo_write cmd=None
  empty: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="todo list" al="todo list"
  view: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="todo list" al="todo list"
  create: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="todo list: 1 item" al="todo list: 1 item"
  noread: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="todo list" al="todo list"
todo_read: mut=false canon=todo_read cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="todo_read" al="todo_read"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="todo_read" al="todo_read"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="todo_read" al="todo_read"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="todo_read" al="todo_read"
create_docx: mut=true canon=create_docx cmd=None
  empty: call=true edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="create docx ?" al="create docx ?"
  view: call=true edit=false net=None tp=Some("/bwn-char-proj/src/a.rs") tps=["/bwn-char-proj/src/a.rs"] rt=None et=None oc=None pv="create docx src/a.rs" al="create docx src/a.rs"
  create: call=true edit=false net=None tp=Some("/bwn-char-outside/f.txt") tps=["/bwn-char-outside/f.txt"] rt=None et=None oc=Some("/bwn-char-outside/f.txt") pv="create docx ?" al="create docx ?"
  noread: call=true edit=false net=None tp=Some("/bwn-char-proj/") tps=["/bwn-char-proj/"] rt=None et=None oc=None pv="create docx ?" al="create docx ?"
finish: mut=false canon=finish cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="finish" al="finish"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="finish" al="finish"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="finish" al="finish"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="finish" al="finish"
save_memory: mut=false canon=save_memory cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="save_memory" al="save_memory"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="save_memory" al="save_memory"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="save_memory" al="save_memory"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="save_memory" al="save_memory"
fetch_url: mut=false canon=fetch_url cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="GET ?" al="GET ?"
  view: call=false edit=false net=Some("example.com:8080") tp=None tps=[] rt=None et=None oc=None pv="GET https://Example.COM:8080/x?q=1" al="GET https://Example.COM:8080/x?q=1"
  create: call=false edit=false net=Some("?") tp=None tps=[] rt=None et=None oc=None pv="GET file:///etc/passwd" al="GET file:///etc/passwd"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="GET" al="GET"
web_search: mut=false canon=web_search cmd=None
  empty: call=false edit=false net=Some("lite.duckduckgo.com") tp=None tps=[] rt=None et=None oc=None pv="web search: ?" al="web search: ?"
  view: call=false edit=false net=Some("lite.duckduckgo.com") tp=None tps=[] rt=None et=None oc=None pv="web search: rust" al="web search: rust"
  create: call=false edit=false net=Some("lite.duckduckgo.com") tp=None tps=[] rt=None et=None oc=None pv="web search: ?" al="web search: ?"
  noread: call=false edit=false net=Some("lite.duckduckgo.com") tp=None tps=[] rt=None et=None oc=None pv="web search: ?" al="web search: ?"
headless_browser: mut=false canon=headless_browser cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="headless_browser" al="headless_browser"
  view: call=false edit=false net=Some("example.com:8080") tp=None tps=[] rt=None et=None oc=None pv="headless_browser" al="headless_browser"
  create: call=false edit=false net=Some("?") tp=None tps=[] rt=None et=None oc=None pv="headless_browser" al="headless_browser"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="headless_browser" al="headless_browser"
start_server: mut=true canon=start_server cmd=None
  empty: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="start server: ?" al="start server: ?"
  view: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="start server: view" al="start server: view"
  create: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="start server: create" al="start server: create"
  noread: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="start server: ls -la" al="start server: ls -la"
list_servers: mut=false canon=list_servers cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="list servers" al="list servers"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="list servers" al="list servers"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="list servers" al="list servers"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="list servers" al="list servers"
stop_server: mut=true canon=stop_server cmd=None
  empty: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="stop server: ?" al="stop server: ?"
  view: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="stop server: ?" al="stop server: ?"
  create: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="stop server: ?" al="stop server: ?"
  noread: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="stop server: ?" al="stop server: ?"
read_server_log: mut=false canon=read_server_log cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="server log: ?" al="server log: ?"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="server log: ?" al="server log: ?"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="server log: ?" al="server log: ?"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="server log: ?" al="server log: ?"
wait_for_url: mut=false canon=wait_for_url cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="wait for URL: ?" al="wait for URL: ?"
  view: call=false edit=false net=Some("example.com:8080") tp=None tps=[] rt=None et=None oc=None pv="wait for URL: https://Example.COM:8080/x?q=1" al="wait for URL: https://Example.COM:8080/x?q=1"
  create: call=false edit=false net=Some("?") tp=None tps=[] rt=None et=None oc=None pv="wait for URL: file:///etc/passwd" al="wait for URL: file:///etc/passwd"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="wait for URL:" al="wait for URL:"
open_browser: mut=true canon=open_browser cmd=None
  empty: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="open browser: ?" al="open browser: ?"
  view: call=true edit=false net=Some("example.com:8080") tp=None tps=[] rt=None et=None oc=None pv="open browser: https://Example.COM:8080/x?q=1" al="open browser: https://Example.COM:8080/x?q=1"
  create: call=true edit=false net=Some("?") tp=None tps=[] rt=None et=None oc=None pv="open browser: file:///etc/passwd" al="open browser: file:///etc/passwd"
  noread: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="open browser:" al="open browser:"
screenshot_url: mut=false canon=screenshot_url cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="screenshot ?" al="screenshot ?"
  view: call=false edit=false net=Some("example.com:8080") tp=None tps=[] rt=None et=None oc=None pv="screenshot https://Example.COM:8080/x?q=1" al="screenshot https://Example.COM:8080/x?q=1"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="screenshot file:///etc/passwd" al="screenshot file:///etc/passwd"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="screenshot" al="screenshot"
list_skills: mut=false canon=list_skills cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="list skills" al="list skills"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="list skills" al="list skills"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="list skills" al="list skills"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="list skills" al="list skills"
load_skill: mut=false canon=load_skill cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="load skill: ?" al="load skill: ?"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="load skill: ?" al="load skill: ?"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="load skill: ?" al="load skill: ?"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="load skill: ?" al="load skill: ?"
kb_query: mut=false canon=kb_query cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="query knowledge base: ?" al="query knowledge base: ?"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="query knowledge base: rust" al="query knowledge base: rust"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="query knowledge base: ?" al="query knowledge base: ?"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="query knowledge base: ?" al="query knowledge base: ?"
kb_record: mut=true canon=kb_record cmd=None
  empty: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="record knowledge base entity: ?" al="record knowledge base entity: ?"
  view: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="record knowledge base entity: ?" al="record knowledge base entity: ?"
  create: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="record knowledge base entity: ?" al="record knowledge base entity: ?"
  noread: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="record knowledge base entity: ?" al="record knowledge base entity: ?"
rule_check: mut=false canon=rule_check cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="evaluate engineering rules" al="evaluate engineering rules"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="evaluate engineering rules" al="evaluate engineering rules"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="evaluate engineering rules" al="evaluate engineering rules"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="evaluate engineering rules" al="evaluate engineering rules"
verify: mut=false canon=verify cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="verify task: ?" al="verify task: ?"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="verify task: ?" al="verify task: ?"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="verify task: ?" al="verify task: ?"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="verify task: ?" al="verify task: ?"
check_work: mut=true canon=check_work cmd=None
  empty: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="check work: build + test + lint" al="check work: build + test + lint"
  view: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="check work: view" al="check work: view"
  create: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="check work: create" al="check work: create"
  noread: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="check work: ls -la" al="check work: ls -la"
mcp_call: mut=true canon=mcp_call cmd=None
  empty: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="MCP call: ?/?" al="MCP call: ?/?"
  view: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="MCP call: ?/?" al="MCP call: ?/?"
  create: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="MCP call: ?/?" al="MCP call: ?/?"
  noread: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="MCP call: ?/?" al="MCP call: ?/?"
task: mut=true canon=spawn_subagent cmd=None
  empty: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="subagent: ?" al="subagent: ?"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="subagent: look around" al="subagent: look around"
  create: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="subagent: build it" al="subagent: build it"
  noread: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="subagent: ?" al="subagent: ?"
spawn_subagent: mut=true canon=spawn_subagent cmd=None
  empty: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="subagent: ?" al="subagent: ?"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="subagent: look around" al="subagent: look around"
  create: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="subagent: build it" al="subagent: build it"
  noread: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="subagent: ?" al="subagent: ?"
mcp__x__y: mut=true canon=mcp__x__y cmd=None
  empty: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="MCP call: x/y" al="MCP call: x/y"
  view: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="MCP call: x/y" al="MCP call: x/y"
  create: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="MCP call: x/y" al="MCP call: x/y"
  noread: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="MCP call: x/y" al="MCP call: x/y"
mcp__bad: mut=true canon=mcp__bad cmd=None
  empty: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="MCP call: mcp__bad" al="MCP call: mcp__bad"
  view: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="MCP call: mcp__bad" al="MCP call: mcp__bad"
  create: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="MCP call: mcp__bad" al="MCP call: mcp__bad"
  noread: call=true edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="MCP call: mcp__bad" al="MCP call: mcp__bad"
no_such_tool: mut=false canon=no_such_tool cmd=None
  empty: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="no_such_tool" al="no_such_tool"
  view: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="no_such_tool" al="no_such_tool"
  create: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="no_such_tool" al="no_such_tool"
  noread: call=false edit=false net=None tp=None tps=[] rt=None et=None oc=None pv="no_such_tool" al="no_such_tool"
"#;
