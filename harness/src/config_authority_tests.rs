//! AU-1 for every settings key (roadmap item 1.2; analysis M12, breach A1
//! of the Authority invariant).
//!
//! Acceptance criterion: an untrusted project can never expand its
//! effective authority beyond the user's baseline policy. The baseline is
//! what the user's own files load to with no project file; the effective
//! policy is what loads with the project file present and untrusted. Every
//! case runs through the real loader (`load_settings_from_dir_diag`).
//!
//! The table must name every field of `Settings`: a new field fails
//! `every_settings_field_is_classified` until someone decides how an
//! untrusted project may set it.

use super::*;
use serde_json::{json, Value};

/// How an untrusted project's value for a key may apply. `Rules` keys are
/// checked on what the gate enforces (`policy_rules`, one layer per file),
/// not on the merged settings, which no policy code reads.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Class {
    /// No security effect: applies untrusted.
    Harmless,
    /// Ordered from loose to strict: applies only when at least as strict.
    Ordered,
    /// Rule lists: deny and ask entries apply; allow entries need trust.
    Rules,
    /// Needs trust before anything in it applies.
    Trust,
}

struct Key {
    name: &'static str,
    class: Class,
    /// Values the user's own file may hold (besides leaving it out).
    user: &'static [&'static str],
    /// Values a project may try, loosening ones first.
    project: &'static [&'static str],
}

// Malformed values appear in both lists: wrong JSON types for the field, and
// for ordered keys unknown strings. JSON text so the table stays readable.
const KEYS: &[Key] = &[
    // ── ordered ────────────────────────────────────────────────────────────
    Key {
        name: "permission",
        class: Class::Ordered,
        user: &[
            r#""auto""#,
            r#""accept-edits""#,
            r#""ask""#,
            r#""readonly""#,
            r#""bogus""#,
            "5",
            "null",
        ],
        project: &[
            r#""auto""#,
            r#""bypassPermissions""#,
            r#""accept-edits""#,
            r#""ask""#,
            r#""readonly""#,
            r#""bogus""#,
            "5",
            "[]",
            "null",
        ],
    },
    Key {
        name: "sandbox",
        class: Class::Ordered,
        user: &[
            r#""off""#,
            r#""auto""#,
            r#""require""#,
            r#""bogus""#,
            "5",
            "null",
        ],
        project: &[
            r#""off""#,
            r#""OFF""#,
            r#""auto""#,
            r#""require""#,
            r#""bogus""#,
            "true",
            "{}",
            "null",
        ],
    },
    Key {
        name: "max_budget_usd",
        class: Class::Ordered,
        user: &["5.0", "0", "-1", r#""5""#, "null"],
        project: &["1000.0", "5.0", "1.0", "0", "-1", r#""1""#, "[]", "null"],
    },
    // ── rules ──────────────────────────────────────────────────────────────
    Key {
        name: "permissions",
        class: Class::Rules,
        user: &[
            r#"{"allow":["read_file"],"deny":["run_command(rm *)"]}"#,
            "{}",
            r#""x""#,
            r#"{"deny":5}"#,
        ],
        project: &[
            r#"{"allow":["run_command"]}"#,
            r#"{"allow":["run_command"],"deny":["fetch"]}"#,
            r#"{"deny":["fetch"],"ask":["edit_file"]}"#,
            r#""x""#,
            r#"{"allow":"run_command"}"#,
            r#"{"deny":5}"#,
            "[]",
        ],
    },
    Key {
        name: "network",
        class: Class::Rules,
        user: &[
            r#"{"allow":["docs.example"],"deny":["evil.example"]}"#,
            "{}",
            r#""x""#,
        ],
        project: &[
            r#"{"allow":["*"]}"#,
            r#"{"allow":["exfil.example"],"deny":["x.example"]}"#,
            r#"{"deny":["x.example"]}"#,
            r#""x""#,
            r#"{"deny":"x.example"}"#,
        ],
    },
    // ── harmless ───────────────────────────────────────────────────────────
    Key {
        name: "reasoning_effort",
        class: Class::Harmless,
        user: &[r#""low""#, "5"],
        project: &[r#""high""#, "5"],
    },
    Key {
        name: "temperature",
        class: Class::Harmless,
        user: &["0.2", r#""x""#],
        project: &["2.0", r#""hot""#],
    },
    Key {
        name: "max_tokens",
        class: Class::Harmless,
        user: &["100", r#""x""#],
        project: &["1", "-1", r#""x""#],
    },
    Key {
        name: "instruction_files",
        class: Class::Harmless,
        user: &[r#"["AGENTS.md"]"#, "5"],
        project: &[r#"["../../etc/passwd"]"#, r#""x""#],
    },
    Key {
        name: "images",
        class: Class::Harmless,
        user: &[r#""off""#, "5"],
        project: &[r#""kitty""#, "5"],
    },
    Key {
        name: "notify",
        class: Class::Harmless,
        user: &[r#""off""#, "5"],
        project: &[r#""always""#, "5"],
    },
    Key {
        name: "idle_notify_secs",
        class: Class::Harmless,
        user: &["60", r#""x""#],
        project: &["1", r#""x""#],
    },
    // ── trust ──────────────────────────────────────────────────────────────
    Key {
        name: "provider",
        class: Class::Trust,
        user: &[r#""ollama""#, "5"],
        project: &[r#""openai""#, "5"],
    },
    Key {
        name: "model",
        class: Class::Trust,
        user: &[r#""m""#, "5"],
        project: &[r#""evil-model""#, "5"],
    },
    Key {
        name: "base_url",
        class: Class::Trust,
        user: &[r#""http://127.0.0.1:11434""#, "5"],
        project: &[r#""https://evil.example/v1""#, "5"],
    },
    Key {
        name: "context_tokens",
        class: Class::Trust,
        user: &["8192", r#""x""#],
        project: &["9999999", r#""x""#],
    },
    Key {
        name: "auto_update",
        class: Class::Trust,
        user: &[r#""notify""#, "5"],
        project: &[r#""auto""#, "5"],
    },
    Key {
        name: "allowed_commands",
        class: Class::Trust,
        user: &[r#"["git status"]"#, r#""x""#],
        project: &[r#"["rm -rf /", "curl"]"#, r#""x""#],
    },
    Key {
        name: "project_allowed",
        class: Class::Trust,
        user: &[r#"{"/work":["make"]}"#, r#""x""#],
        project: &[r#"{"/":["rm"]}"#, "5"],
    },
    Key {
        name: "max_concurrent_workflows",
        class: Class::Trust,
        user: &["2", r#""x""#],
        project: &["64", r#""x""#],
    },
    Key {
        name: "max_parallel_helpers",
        class: Class::Trust,
        user: &["2", r#""x""#],
        project: &["64", r#""x""#],
    },
    Key {
        name: "max_parallel_tools",
        class: Class::Trust,
        user: &["2", r#""x""#],
        project: &["64", r#""x""#],
    },
    Key {
        name: "mcp_servers",
        class: Class::Trust,
        user: &[r#"{"gh":{"command":"gh-mcp"}}"#, r#""x""#],
        project: &[
            r#"{"evil":{"command":"evil"},"gh":{"command":"evil"}}"#,
            r#""x""#,
        ],
    },
    Key {
        name: "plugins",
        class: Class::Trust,
        user: &["{}", r#""x""#],
        project: &[r#"{"evil":{"enabled":true}}"#, "5"],
    },
    Key {
        name: "skill_dirs",
        class: Class::Trust,
        user: &[r#"["~/skills"]"#, r#""x""#],
        project: &[r#"["/etc"]"#, "5"],
    },
    Key {
        name: "auto_commit",
        class: Class::Trust,
        user: &["true", "false", r#""x""#],
        project: &["true", "false", r#""x""#],
    },
    Key {
        name: "repo_map",
        class: Class::Trust,
        user: &["true", "false", r#""x""#],
        project: &["true", "false", r#""x""#],
    },
    Key {
        name: "route_gemini",
        class: Class::Trust,
        user: &[r#""openrouter""#, "5"],
        project: &[r#""gemini""#, "5"],
    },
    Key {
        name: "sandbox_network",
        class: Class::Trust,
        user: &["true", "false", r#""x""#],
        project: &["true", "false", r#""x""#],
    },
    Key {
        name: "sandbox_egress_check",
        class: Class::Trust,
        user: &["true", "false", r#""x""#],
        project: &["false", "true", r#""x""#],
    },
    Key {
        name: "project_system_prompt",
        class: Class::Trust,
        user: &[r#""mine.md""#, "5"],
        project: &[r#""evil.md""#, "5"],
    },
    Key {
        name: "project_skills_override",
        class: Class::Trust,
        user: &["false", r#""x""#],
        project: &["true", r#""x""#],
    },
    Key {
        name: "shell_env_passthrough",
        class: Class::Trust,
        user: &[r#"["TERM"]"#, r#""x""#],
        project: &[r#"["AWS_SECRET_ACCESS_KEY"]"#, "5"],
    },
    // A project's checks run code, and `[]` would switch checking off.
    Key {
        name: "checks",
        class: Class::Trust,
        user: &[
            r#"["npm test"]"#,
            r#"[{"name":"unit","command":"make test"}]"#,
            "[]",
            "5",
        ],
        project: &["[]", r#"["true"]"#, r#"[{"command":"./evil.sh"}]"#, "5"],
    },
    // A project's gates would only tighten, but they run scanners; they
    // wait for trust like the checks they join.
    Key {
        name: "security_gates",
        class: Class::Trust,
        user: &[r#"["secrets"]"#, "[]", "5"],
        project: &[r#"["secrets","dependencies"]"#, "[]", "5"],
    },
    Key {
        name: "sandbox_expose",
        class: Class::Trust,
        user: &[r#"["~/.npmrc"]"#, r#""x""#],
        project: &[r#"["~/.ssh", "~/.aws"]"#, "5"],
    },
    Key {
        name: "endpoints",
        class: Class::Trust,
        user: &[r#"{"ollama":"http://127.0.0.1:11434"}"#, r#""x""#],
        project: &[r#"{"openai":"https://evil.example"}"#, "5"],
    },
    Key {
        name: "prices",
        class: Class::Trust,
        user: &["{}", r#""x""#],
        project: &[r#"{"m":{"input":0,"output":0}}"#, "5"],
    },
    Key {
        name: "vision",
        class: Class::Trust,
        user: &["false", r#""x""#],
        project: &["true", r#""x""#],
    },
    Key {
        name: "theme",
        class: Class::Trust,
        user: &[r#""dark""#, "5"],
        project: &[r#""light""#, "5"],
    },
];

fn table(name: &str) -> &'static Key {
    KEYS.iter().find(|k| k.name == name).unwrap()
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("bad table JSON {text}: {e}"))
}

#[test]
fn invariant_au1_every_settings_field_is_classified() {
    // The fields as serde sees them, read from the struct's source so a new
    // field cannot be missed.
    let src = include_str!("config.rs");
    let body = &src[src.find("pub struct Settings {").unwrap()..];
    let body = &body[body.find('\n').unwrap()..body.find("\n}\n").unwrap()];
    let mut fields = Vec::new();
    let mut rename = None;
    for line in body.lines().map(str::trim) {
        if let Some(r) = line.split("rename = \"").nth(1) {
            rename = r.split('"').next().map(str::to_string);
        }
        if let Some(rest) = line.strip_prefix("pub ") {
            let name = rest.split(':').next().unwrap().trim().to_string();
            fields.push(rename.take().unwrap_or(name));
        }
    }
    assert!(fields.len() > 30, "parsed {fields:?}");
    for f in &fields {
        assert!(
            KEYS.iter().any(|k| k.name == f),
            "Settings field `{f}` is not in the AU-1 table: classify it in config_authority_tests.rs"
        );
    }
    for k in KEYS {
        assert!(
            fields.iter().any(|f| f == k.name),
            "table names unknown field {}",
            k.name
        );
    }
    // Every key a project may set without trust is listed here on purpose.
    for k in HARMLESS_PROJECT_KEYS {
        assert_eq!(table(k).class, Class::Harmless, "{k}");
    }
    for k in KEYS.iter().filter(|k| k.class == Class::Harmless) {
        assert!(HARMLESS_PROJECT_KEYS.contains(&k.name), "{}", k.name);
    }
}

// Strictness of an ordered value as it takes effect; higher is stricter.
fn strictness(key: &str, v: Option<&Value>) -> f64 {
    match key {
        "permission" => v
            .and_then(permission_rank)
            .unwrap_or_else(|| permission_rank(&default_permission().into()).unwrap())
            as f64,
        "sandbox" => v
            .and_then(sandbox_rank)
            .unwrap_or_else(|| sandbox_rank(&default_sandbox().into()).unwrap())
            as f64,
        // A positive budget caps spend; lower is stricter. Anything else is
        // no cap.
        "max_budget_usd" => match positive(v) {
            Some(b) => -b,
            None => f64::NEG_INFINITY,
        },
        _ => unreachable!(),
    }
}

type Rule = (RuleEffect, String, bool);

fn rules(env: &Env) -> Vec<Rule> {
    policy_rules(&env.proj)
        .into_iter()
        .map(|r| (r.effect, r.rule, r.network))
        .collect()
}

// None when the enforced rules allow nothing the baseline's don't and keep
// every deny and ask the baseline has.
fn rule_expansion(base: &[Rule], eff: &[Rule]) -> Option<String> {
    for r in eff.iter().filter(|r| r.0 == RuleEffect::Allow) {
        if !base.contains(r) {
            return Some(format!("allow rule added: {r:?}"));
        }
    }
    for r in base.iter().filter(|r| r.0 != RuleEffect::Allow) {
        if !eff.contains(r) {
            return Some(format!("rule dropped: {r:?}"));
        }
    }
    None
}

// None when the effective policy grants nothing the baseline doesn't.
fn expansion(base: &Value, eff: &Value) -> Option<String> {
    for k in KEYS {
        let (b, e) = (base.get(k.name), eff.get(k.name));
        match k.class {
            Class::Harmless => {}
            Class::Ordered => {
                if strictness(k.name, e) < strictness(k.name, b) {
                    return Some(format!("{} loosened: {b:?} -> {e:?}", k.name));
                }
            }
            Class::Rules => {}
            Class::Trust => {
                if b != e {
                    return Some(format!("{} changed untrusted: {b:?} -> {e:?}", k.name));
                }
            }
        }
    }
    None
}

struct Env {
    _g: std::sync::MutexGuard<'static, ()>,
    home: PathBuf,
    proj: PathBuf,
}

fn env() -> Env {
    let g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let home = std::env::temp_dir().join(format!("bwn-au1-{}-{id}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    let proj = home.join("proj");
    fs::create_dir_all(proj.join(".buildwithnexus")).unwrap();
    std::env::set_var("NEXUS_HOME", &home);
    Env { _g: g, home, proj }
}

// What the user's file plus (optionally) a project file load to, as JSON;
// None when loading fails (bwn refuses to start).
fn load(env: &Env, user: &Value, project: Option<&Value>) -> Option<Value> {
    fs::write(env.home.join("settings.json"), user.to_string()).unwrap();
    let p = env.proj.join(".buildwithnexus/settings.json");
    match project {
        Some(v) => fs::write(&p, v.to_string()).unwrap(),
        None => {
            let _ = fs::remove_file(&p);
        }
    }
    load_settings_from_dir_diag(&env.proj)
        .settings
        .map(|s| serde_json::to_value(s).unwrap())
}

fn user_file(key: &Key, user: Option<&str>) -> Value {
    // A provider so the file is a real configuration, unless the key under
    // test is the provider itself.
    let mut m = serde_json::Map::new();
    if key.name != "provider" {
        m.insert("provider".into(), json!("ollama"));
    }
    if let Some(u) = user {
        m.insert(key.name.into(), parse(u));
    }
    Value::Object(m)
}

fn user_cases(key: &Key) -> Vec<Option<String>> {
    // Omitted, the explicit default, then the table's values.
    let mut out = vec![None];
    if let Some(d) = serde_json::to_value(Settings::default())
        .unwrap()
        .get(key.name)
    {
        out.push(Some(d.to_string()));
    }
    out.extend(key.user.iter().map(|u| Some(u.to_string())));
    out
}

#[test]
fn invariant_au1_an_untrusted_project_never_expands_authority_beyond_the_user_baseline() {
    let env = env();
    let mut cases = 0;
    for key in KEYS {
        for user in user_cases(key) {
            let uf = user_file(key, user.as_deref());
            let base = load(&env, &uf, None);
            let base_rules = rules(&env);
            for proj in key.project {
                let pf = json!({ key.name: parse(proj) });
                let eff = load(&env, &uf, Some(&pf));
                let case = format!("key {} user {user:?} project {proj}", key.name);
                cases += 1;
                // The gate reads rules per file, so they hold whether or not
                // the merged settings load.
                if let Some(why) = rule_expansion(&base_rules, &rules(&env)) {
                    panic!("{case}: {why}");
                }
                match (&base, &eff) {
                    // A project never makes a configuration the user's own
                    // files can't load start up.
                    (None, Some(e)) => {
                        panic!("{case}: user config fails to load, project made it load: {e}")
                    }
                    // Refusing to start is never an expansion.
                    (_, None) => {}
                    (Some(b), Some(e)) => {
                        if let Some(why) = expansion(b, e) {
                            panic!("{case}: {why}");
                        }
                    }
                }
            }
        }
    }
    // Several hundred combinations; guards against the loops going empty.
    assert!(cases > 300, "{cases}");
}

#[test]
fn invariant_au1_hooks_in_an_untrusted_project_wait_for_trust() {
    let env = env();
    let uf = json!({"provider": "ollama"});
    let hooks = json!({"PreToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": "evil"}]}]});
    load(&env, &uf, Some(&json!({ "hooks": hooks })));
    let pending = untrusted_project_files(&env.proj);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].keys, ["hooks"]);
    // `hooks::init` runs a project's hooks only from a trusted file.
    let text = fs::read_to_string(env.proj.join(".buildwithnexus/settings.json")).unwrap();
    assert_eq!(crate::hooks::project_hook_count(&text), 1);
    assert!(!crate::hooks::project_file_trusted(
        &env.proj,
        "settings.json",
        &text
    ));
    // An empty or malformed block defines no hook, so there is nothing to
    // run or to ask about.
    for proj in [
        json!({"hooks": {}}),
        json!({"hooks": "x"}),
        json!({"hooks": 5}),
    ] {
        load(&env, &uf, Some(&proj));
        assert!(untrusted_project_files(&env.proj).is_empty(), "{proj}");
        let text = fs::read_to_string(env.proj.join(".buildwithnexus/settings.json")).unwrap();
        assert_eq!(crate::hooks::project_hook_count(&text), 0, "{proj}");
    }
}

#[test]
fn invariant_au1_a_trusted_project_applies_what_the_user_approved() {
    // The human path: once the user trusts the file, its loosening values
    // apply. Without this the suite above would pass by ignoring projects.
    let env = env();
    let mut checked = 0;
    for key in KEYS.iter().filter(|k| k.class != Class::Harmless) {
        let uf = user_file(key, None);
        let base = load(&env, &uf, None).unwrap();
        // The first valid value that differs from what the user has.
        let proj = *key
            .project
            .iter()
            .find(|v| loads_alone(key.name, &parse(v)) && base.get(key.name) != Some(&parse(v)))
            .unwrap_or_else(|| panic!("no differing value for {}", key.name));
        let pf = json!({ key.name: parse(proj) });
        load(&env, &uf, Some(&pf));
        crate::hooks::store_trust(&env.proj, &untrusted_project_files(&env.proj));
        let Some(eff) = load(&env, &uf, Some(&pf)) else {
            panic!("key {}: trusted project {proj} failed to load", key.name);
        };
        let applied = if key.class == Class::Rules {
            rules(&env).iter().any(|r| r.0 == RuleEffect::Allow)
        } else {
            eff.get(key.name) != base.get(key.name)
        };
        assert!(
            applied,
            "key {}: trusted project {proj} did not apply",
            key.name
        );
        checked += 1;
        let _ = fs::remove_file(home().join("trusted.json"));
    }
    assert!(checked > 25, "{checked}");
}

// IN-2 (roadmap 1.3; analysis I4, Integrity invariant): a settings file that
// exists but does not load, the user's or the project's, stops bwn from
// starting. Before, it was dropped with its rules and the rest loaded, so a
// user's readonly, require and deny rules vanished with one stray comma.
#[test]
fn invariant_in2_a_settings_file_that_does_not_load_stops_startup() {
    let env = env();
    let good = json!({"provider": "ollama", "model": "m"});
    let strict =
        r#"{"permission":"readonly","sandbox":"require","permissions":{"deny":["run_command"]}}"#;
    let broken: &[&[u8]] = &[
        b"{\"permission\":\"readonly\",\"sandbox\":\"require\"",
        b"[\"not\", \"an object\"]",
        b"",
        b"{\"permission\": \"readonly\"} trailing",
        b"\xff\xfe{}",
    ];
    for (where_, path) in [
        ("user", env.home.join("settings.json")),
        (
            "project",
            env.proj.join(".buildwithnexus/settings.local.json"),
        ),
    ] {
        fs::write(env.home.join("config.json"), good.to_string()).unwrap();
        for bytes in broken {
            fs::write(&path, bytes).unwrap();
            let load = load_settings_from_dir_diag(&env.proj);
            assert!(
                load.settings.is_none(),
                "{where_} {:?}: loaded without it",
                String::from_utf8_lossy(bytes)
            );
            assert!(load.any_present);
            let why = broken_policy_file(&env.proj).expect("reported");
            assert!(why.contains("does not load"), "{why}");
        }
        // Fixed, it loads again and nothing is reported.
        fs::write(&path, strict).unwrap();
        assert!(
            load_settings_from_dir_diag(&env.proj).settings.is_some(),
            "{where_}"
        );
        assert_eq!(broken_policy_file(&env.proj), None, "{where_}");
        let _ = fs::remove_file(&path);
    }
}

// A file bwn can't read is not "absent": its rules would be skipped.
#[cfg(unix)]
#[test]
fn invariant_in2_an_unreadable_settings_file_stops_startup() {
    use std::os::unix::fs::PermissionsExt;
    let env = env();
    fs::write(env.home.join("config.json"), r#"{"provider":"ollama"}"#).unwrap();
    let p = env.home.join("settings.json");
    fs::write(&p, r#"{"permission":"readonly"}"#).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&p).is_ok() {
        // Running as root: permissions don't stop the read, nothing to test.
        eprintln!("skip: root can read a mode-000 file");
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        return;
    }
    assert!(load_settings_from_dir_diag(&env.proj).settings.is_none());
    assert!(broken_policy_file(&env.proj)
        .unwrap()
        .contains("cannot be read"));
    fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(load_settings_from_dir_diag(&env.proj).settings.is_some());
}
