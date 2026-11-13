// The tool table agrees with what is offered and what `run` dispatches.
use super::table::{lookup, Mutation, Tool, TOOLS};
use super::*;
use serde_json::json;
use std::collections::BTreeSet;

fn is_unknown(o: &Outcome) -> bool {
    o.is_error && o.content.starts_with("unknown tool:")
}

#[test]
fn table_rows_are_unique_and_match_the_offered_tools() {
    let rows: Vec<&str> = TOOLS.iter().map(|s| s.name).collect();
    let unique: BTreeSet<&str> = rows.iter().copied().collect();
    assert_eq!(rows.len(), unique.len(), "duplicate table rows");
    let offered: BTreeSet<&str> = defs(true)
        .iter()
        .map(|d| d.name)
        .filter(|n| !crate::mcp::is_mcp_tool(n))
        .collect();
    assert_eq!(unique, offered, "table rows vs defs(true)");
}

#[test]
fn every_alias_points_at_a_canonical_row_for_the_same_tool() {
    for s in TOOLS {
        let c = lookup(s.canonical).unwrap_or_else(|| panic!("{}: no row {}", s.name, s.canonical));
        assert_eq!(c.canonical, c.name, "{} is not canonical", c.name);
        assert_eq!(c.tool, s.tool, "{} and {}", s.name, c.name);
        assert!(!matches!(s.tool, Tool::Mcp | Tool::Unknown), "{}", s.name);
        // Only rows whose paths the gate can read are confined.
        if s.confined {
            assert!(
                matches!(s.paths, table::Paths::PathArg | table::Paths::Move),
                "{}",
                s.name
            );
            assert!(s.mutation != Mutation::Never, "{}", s.name);
        }
        if s.parallel != table::Parallel::No {
            assert_eq!(s.mutation, Mutation::Never, "{}", s.name);
        }
    }
}

#[test]
fn run_dispatches_every_row_and_refuses_unknown_names() {
    // Handlers that would do real work with no arguments (run the build,
    // verify the folder, publish an empty artifact) are left out; the
    // exhaustive match in `run` already proves they have a handler.
    const SIDE_EFFECTS: &[&str] = &["check_work", "verify", "Artifact", "publish_artifact"];
    let cwd = std::env::temp_dir().join(format!(
        "bwn-tool-table-{}-{}",
        std::process::id(),
        now_ms()
    ));
    fs::create_dir_all(&cwd).unwrap();
    for s in TOOLS {
        if SIDE_EFFECTS.contains(&s.name) {
            continue;
        }
        let out = run(s.name, &json!({}), &cwd);
        if s.tool == Tool::AgentLoop {
            assert!(is_unknown(&out), "{} reached run: {}", s.name, out.content);
        } else {
            assert!(
                !is_unknown(&out),
                "{} not dispatched: {}",
                s.name,
                out.content
            );
        }
    }
    for name in ["no_such_tool", "Read", "read_file ", "", "mcp_", "Bash"] {
        let out = run(name, &json!({}), &cwd);
        assert!(is_unknown(&out), "{name:?}: {}", out.content);
    }
    let out = run("mcp__nosrv__t", &json!({}), &cwd);
    assert!(out.is_error && !is_unknown(&out), "{}", out.content);
    let _ = fs::remove_dir_all(&cwd);
}

#[test]
fn parallel_safe_exactly_the_pure_reads() {
    let expected: BTreeSet<&str> = [
        "read",
        "read_file",
        "read_many_files",
        "list",
        "list_dir",
        "list_tree",
        "file_info",
        "glob",
        "find_paths",
        "find_files",
        "grep",
        "grep_files",
        "kb_query",
        "webfetch",
        "fetch_url",
        "websearch",
        "web_search",
    ]
    .into_iter()
    .collect();
    let inputs = [
        json!({}),
        json!({"path": "a", "command": "view", "read_only": true}),
        json!({"command": "create", "url": "https://x.test"}),
    ];
    let mut names: Vec<&str> = TOOLS.iter().map(|s| s.name).collect();
    names.extend(["mcp__x__y", "mcp_call", "no_such_tool"]);
    for n in names {
        for input in &inputs {
            assert_eq!(
                is_parallel_safe(n, input),
                expected.contains(n),
                "{n} {input}"
            );
        }
        let bad = json!({ INVALID_ARGS: "{bad" });
        assert!(!is_parallel_safe(n, &bad), "{n} with invalid args");
    }
}
