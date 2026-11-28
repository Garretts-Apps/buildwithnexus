//! Recurring findings become proposed rules (roadmap 5.4, SD-6).
//!
//! When the person refuses the same kind of call again and again, across
//! sessions, bwn proposes the deny rule that would refuse it for them. A
//! proposal is only text in `~/.buildwithnexus/proposals.json`, a hidden
//! store no command or file tool reaches: it enters the policy only when
//! the person runs `buildwithnexus rules accept`, which writes the rule into
//! their own settings. No tool the model has can accept one.

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
    crate::config::add_user_deny_rule(rule)?;
    settle(rule, "accepted")
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
