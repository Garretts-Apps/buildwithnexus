// Rule matching works out what a call's rules are matched against once,
// however many rules there are.

use super::*;
use crate::config::{PolicyRule, RuleEffect};
use serde_json::json;

fn rule(effect: RuleEffect, rule: &str) -> PolicyRule {
    PolicyRule {
        effect,
        rule: rule.to_string(),
        network: false,
        source: "user settings",
    }
}

fn runs() -> usize {
    tools::GUARD_SUBJECT_RUNS.get()
}

#[test]
fn a_command_checked_against_many_rules_is_taken_apart_once() {
    let cwd = std::env::temp_dir();
    let mut rules: Vec<PolicyRule> = (0..60)
        .map(|i| rule(RuleEffect::Deny, &format!("run_command(tool{i} *)")))
        .collect();
    rules.extend((0..60).map(|i| rule(RuleEffect::Ask, &format!("Bash(other{i}:*)"))));
    rules.push(rule(RuleEffect::Ask, "run_command(git push*)"));
    rules.push(rule(RuleEffect::Deny, "write_file(*.lock)"));
    let input = json!({"command": "cd sub && sudo sh -c 'git -C . push origin main'"});

    // No deny rule covers it: all 60 were tried, on one reading.
    let before = runs();
    let hit = rule_for(&rules, RuleEffect::Deny, "run_command", &input, &cwd, None);
    assert!(hit.is_none());
    assert_eq!(runs() - before, 1);

    // The last of 61 ask rules does.
    let before = runs();
    let hit = rule_for(&rules, RuleEffect::Ask, "run_command", &input, &cwd, None);
    assert_eq!(hit.map(|r| r.rule.as_str()), Some("run_command(git push*)"));
    assert_eq!(runs() - before, 1);

    // Allow rules never take the command apart, and a call no command rule
    // is asked about never does either.
    let before = runs();
    let _ = rule_for(&rules, RuleEffect::Allow, "run_command", &input, &cwd, None);
    let edit = json!({"path": "Cargo.lock", "content": ""});
    let hit = rule_for(&rules, RuleEffect::Deny, "write_file", &edit, &cwd, None);
    assert_eq!(hit.map(|r| r.rule.as_str()), Some("write_file(*.lock)"));
    assert_eq!(runs() - before, 0);
}

#[test]
fn one_reading_for_all_rules_decides_as_one_reading_per_rule_did() {
    let cwd = std::env::temp_dir();
    let patterns = [
        "run_command(git push*)",
        "Bash(git push:*)",
        "run_command(rm *)",
        "run_command(ls*)",
        "run_command(cargo test*)",
        "run_command",
        "run_command(*)",
        "bash(echo *)",
        "check_work(git *)",
        "write_file(*.lock)",
        "Edit(src/**)",
        "read_file(*.rs)",
        "fetch_url(domain:example.com)",
        "web_search(*secret*)",
    ];
    let calls = [
        ("run_command", json!({"command": "git push origin main"})),
        ("run_command", json!({"command": "ls -la"})),
        ("run_command", json!({"command": "ls && git -C . push"})),
        ("run_command", json!({"command": "env A=1 sudo rm -rf x"})),
        ("bash", json!({"command": "sh -c 'echo hi; git push'"})),
        ("run_command", json!({"command": "cargo test --all"})),
        ("run_command", json!({"command": "echo $(git push)"})),
        ("check_work", json!({"command": "git status"})),
        ("write_file", json!({"path": "Cargo.lock", "content": "x"})),
        ("write_file", json!({"path": "src/a.rs", "content": "x"})),
        ("read_file", json!({"path": "src/a.rs"})),
        ("fetch_url", json!({"url": "https://example.com/a"})),
        ("web_search", json!({"query": "the secret plan"})),
        ("list_dir", json!({})),
    ];
    for effect in [RuleEffect::Deny, RuleEffect::Ask, RuleEffect::Allow] {
        let rules: Vec<PolicyRule> = patterns.iter().map(|p| rule(effect, p)).collect();
        for (name, input) in &calls {
            let host = tools::network_host(name, input);
            let host = host.as_deref();
            let together = rule_for(&rules, effect, name, input, &cwd, host).map(|r| &r.rule);
            let apart = rules
                .iter()
                .find(|r| rule_covers(r, &RuledCall::new(name, input, &cwd, host)))
                .map(|r| &r.rule);
            assert_eq!(together, apart, "{effect:?} {name} {input}");
            // And every rule on its own agrees with its turn in the loop.
            let call = RuledCall::new(name, input, &cwd, host);
            for r in &rules {
                assert_eq!(
                    rule_covers(r, &call),
                    rule_covers(r, &RuledCall::new(name, input, &cwd, host)),
                    "{} {name} {input}",
                    r.rule
                );
            }
        }
    }
}
