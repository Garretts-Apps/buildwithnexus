//! Recurring findings become proposed rules (roadmap 5.4, SD-6).
//!
//! When the person refuses the same kind of call again and again, across
//! sessions, bwn proposes the deny rule that would refuse it for them. A
//! proposal is only text in `~/.buildwithnexus/proposals.json`, a hidden
//! store no command or file tool reaches: it enters the policy only when
//! the person runs `buildwithnexus rules accept`, which writes the rule into
//! their own settings. No tool the model has can accept one.
//!
//! A test that fails in the project's checks turn after turn becomes a
//! proposed check that runs that test on every change; accepted, it joins
//! the project's `checks` (with the detected ones, so none are dropped).

use serde_json::{json, Value};

/// Refusals of the same call before a rule is proposed.
pub const THRESHOLD: u64 = 3;

fn path() -> std::path::PathBuf {
    crate::config::home().join("proposals.json")
}

fn load() -> Value {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn save(v: &Value) {
    crate::config::ensure_home();
    let _ = std::fs::write(path(), serde_json::to_string_pretty(v).unwrap_or_default());
}

/// The deny rule that would refuse this call: the command's binary (and
/// subcommand), the host for a network tool, else the tool.
pub fn rule_for(name: &str, tool_key: &str) -> String {
    if matches!(name, "run_command" | "bash" | "check_work" | "start_server") {
        return format!("run_command({tool_key}*)");
    }
    match tool_key.strip_prefix("fetch ") {
        Some(host) => format!("{name}({host})"),
        None => name.to_string(),
    }
}

/// The person refused a call (agent::confirm_call). The third refusal of
/// the same rule proposes it, and says how to accept it.
pub fn note_user_denial(name: &str, tool_key: &str) {
    let rule = rule_for(name, tool_key);
    let mut v = load();
    let seen = ["proposed", "accepted", "dismissed"].iter().any(|k| {
        v[*k]
            .as_array()
            .is_some_and(|a| a.iter().any(|r| r == rule.as_str()))
    });
    if seen {
        return;
    }
    let n = v["denials"][&rule].as_u64().unwrap_or(0) + 1;
    v["denials"][&rule] = n.into();
    if n >= THRESHOLD {
        if !v["proposed"].is_array() {
            v["proposed"] = json!([]);
        }
        v["proposed"]
            .as_array_mut()
            .unwrap()
            .push(rule.clone().into());
        crate::report::notice(&format!(
            "  you have refused {} {n} times: bwn proposes the deny rule `{}` (`buildwithnexus rules accept {}` adds it to your settings)",
            crate::tui::sanitize_terminal(tool_key),
            crate::tui::sanitize_terminal(&rule),
            crate::tui::sanitize_terminal(&rule)
        ));
    }
    save(&v);
}

/// The failing tests a check report names, with the command that runs
/// that one test: cargo's `test x ... FAILED`, pytest's `FAILED a.py::t`
/// and Go's `--- FAIL: TestX`.
pub fn failing_tests(report: &str) -> Vec<(String, String)> {
    let safe = |s: &str| {
        !s.is_empty()
            && s.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '/' | '-' | '[' | ']')
            })
    };
    let mut out = Vec::new();
    for line in report.lines() {
        let l = line.trim();
        if let Some(name) = l
            .strip_prefix("test ")
            .and_then(|r| r.strip_suffix(" ... FAILED"))
        {
            if safe(name) {
                out.push((
                    format!("cargo:{name}"),
                    format!("cargo test -q {name} -- --exact"),
                ));
            }
        } else if let Some(id) = l.strip_prefix("FAILED ") {
            let id = id.split(" - ").next().unwrap_or(id).trim();
            if safe(id) && id.contains("::") {
                out.push((format!("pytest:{id}"), format!("python3 -m pytest -q {id}")));
            }
        } else if let Some(rest) = l.strip_prefix("--- FAIL: ") {
            let name = rest.split_whitespace().next().unwrap_or("");
            if safe(name) {
                out.push((
                    format!("go:{name}"),
                    format!("go test -run '^{name}$' ./..."),
                ));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The project's checks failed with `report`: a test that failed in
/// `THRESHOLD` turns becomes a proposed check that runs it on every change.
/// It joins the project's `checks` only when the person accepts it.
pub fn note_check_failure(cwd: &std::path::Path, report: &str) {
    let project = cwd
        .canonicalize()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .display()
        .to_string();
    let mut v = load();
    for (id, cmd) in failing_tests(report) {
        let key = format!("check:{id}");
        let seen = ["proposed", "accepted", "dismissed"].iter().any(|k| {
            v[*k]
                .as_array()
                .is_some_and(|a| a.iter().any(|r| r == key.as_str()))
        });
        if seen {
            continue;
        }
        let n = v["failures"][&project][&key].as_u64().unwrap_or(0) + 1;
        v["failures"][&project][&key] = n.into();
        if n >= THRESHOLD {
            if !v["proposed"].is_array() {
                v["proposed"] = json!([]);
            }
            v["proposed"]
                .as_array_mut()
                .unwrap()
                .push(key.clone().into());
            v["checks"][&key] =
                json!({"project": project, "name": format!("regression {id}"), "command": cmd});
            crate::report::notice(&format!(
                "  {} has failed in {n} turns: bwn proposes it as a check of its own (`buildwithnexus rules accept {}` adds it to this project's checks)",
                crate::tui::sanitize_terminal(&id),
                crate::tui::sanitize_terminal(&key)
            ));
        }
    }
    save(&v);
}

/// The rules proposed and not yet accepted or dismissed.
pub fn proposed() -> Vec<String> {
    load()["proposed"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| r.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn settle(rule: &str, into: &str) -> Result<(), String> {
    let mut v = load();
    let list = v["proposed"].as_array_mut().ok_or("nothing is proposed")?;
    let before = list.len();
    list.retain(|r| r != rule);
    if list.len() == before {
        return Err(format!(
            "`{rule}` isn't proposed (`buildwithnexus rules` lists what is)"
        ));
    }
    if !v[into].is_array() {
        v[into] = json!([]);
    }
    v[into].as_array_mut().unwrap().push(rule.into());
    save(&v);
    Ok(())
}

/// The person accepts a proposed rule: it joins their settings' deny list.
pub fn accept(rule: &str) -> Result<(), String> {
    if !proposed().iter().any(|r| r == rule) {
        return Err(format!(
            "`{rule}` isn't proposed (`buildwithnexus rules` lists what is)"
        ));
    }
    if rule.starts_with("check:") {
        let check = load()["checks"][rule].clone();
        let project = std::path::PathBuf::from(
            check["project"]
                .as_str()
                .ok_or("the proposal names no project")?,
        );
        add_project_check(
            &project,
            json!({"name": check["name"], "command": check["command"]}),
        )?;
    } else {
        crate::config::add_user_deny_rule(rule)?;
    }
    settle(rule, "accepted")
}

// Append `check` to the project's `.buildwithnexus/settings.json` checks.
// With none named yet, the detected ones are named with it, so the project
// keeps them. The file changed, so it waits for trust again like any edit.
fn add_project_check(project: &std::path::Path, check: Value) -> Result<(), String> {
    let path = project.join(".buildwithnexus").join("settings.json");
    let mut v: Value = std::fs::read_to_string(&path)
        .ok()
        .map(|t| serde_json::from_str(&t).map_err(|e| format!("{}: {e}", path.display())))
        .transpose()?
        .unwrap_or_else(|| json!({}));
    if !v.is_object() {
        return Err(format!(
            "{}: top level must be a JSON object",
            path.display()
        ));
    }
    if !v["checks"].is_array() {
        v["checks"] = Value::Array(crate::tools::detected_check_entries(project));
    }
    v["checks"].as_array_mut().unwrap().push(check);
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap_or_default())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// `buildwithnexus rules [accept|dismiss <rule>]`.
pub fn cli(args: &[String]) -> i32 {
    match args {
        [] => {
            let p = proposed();
            if p.is_empty() {
                println!("no proposed rules");
            }
            for r in p {
                println!("{r}");
            }
            0
        }
        [verb, rule] if verb == "accept" || verb == "dismiss" => {
            let done = if verb == "accept" {
                accept(rule)
            } else {
                settle(rule, "dismissed")
            };
            match done {
                Ok(()) => {
                    println!(
                        "{} {rule}",
                        if verb == "accept" {
                            "added the deny rule"
                        } else {
                            "dismissed"
                        }
                    );
                    0
                }
                Err(e) => {
                    eprintln!("buildwithnexus: {e}");
                    1
                }
            }
        }
        _ => {
            eprintln!("usage: buildwithnexus rules [accept|dismiss <rule>]");
            2
        }
    }
}
