use super::*;

#[test]
fn startup_tips_fit_one_line_and_stay_in_character() {
    assert!(STARTUP_TIPS.len() >= 12, "keep the rotation fresh");
    for t in STARTUP_TIPS {
        assert!(t.starts_with("tip: "), "uniform prefix: {t}");
        assert!(t.chars().count() <= 100, "must fit one line: {t}");
        assert!(!t.contains('!'), "exclamation marks are hype: {t}");
    }
    // The picker always returns a member, whatever the clock says.
    assert!(STARTUP_TIPS.contains(&startup_tip()));
}

#[test]
fn model_pick_routes_to_serving_provider() {
    let p = |s: &str| parse_model_pick(s, "anthropic");
    assert_eq!(
        p("claude-sonnet-4-6"),
        ("anthropic".into(), "claude-sonnet-4-6".into())
    );
    assert_eq!(p("gpt-4o"), ("openai".into(), "gpt-4o".into()));
    assert_eq!(
        p("ollama/qwen2.5-coder"),
        ("ollama".into(), "qwen2.5-coder".into())
    );
    assert_eq!(
        p("local/phi-4.gguf"),
        ("llamacpp".into(), "phi-4.gguf".into())
    );
    // Gemini has no native preset — routed through OpenRouter's naming.
    assert_eq!(
        p("gemini-2.5-pro"),
        ("openrouter".into(), "google/gemini-2.5-pro".into())
    );
    // org/model naming is OpenRouter's scheme.
    assert_eq!(
        p("meta-llama/llama-3.3-70b"),
        ("openrouter".into(), "meta-llama/llama-3.3-70b".into())
    );
    // Explicit "<provider> <model>" wins over inference.
    assert_eq!(
        p("groq llama-3.3-70b-versatile"),
        ("groq".into(), "llama-3.3-70b-versatile".into())
    );
    // Unknown names stay on the current provider for the swap to validate.
    assert_eq!(
        p("mystery-model"),
        ("anthropic".into(), "mystery-model".into())
    );
    // Custom endpoints are addressable by preset name too.
    assert_eq!(
        p("custom vllm-model"),
        ("custom".into(), "vllm-model".into())
    );
}

#[test]
fn custom_provider_keyless_http_and_key_guard() {
    let _g = config::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let h = std::env::temp_dir().join("bwn-custom-provider-test");
    let _ = std::fs::remove_dir_all(&h);
    std::fs::create_dir_all(&h).unwrap();
    std::env::set_var("NEXUS_HOME", &h);
    std::env::remove_var(config::CUSTOM_KEY);

    // Keyless over plain http to loopback: fine (vLLM's default posture).
    let s = Settings {
        provider: "custom".into(),
        model: "my-vllm-model".into(),
        base_url: Some("http://localhost:8000/v1".into()),
        ..Default::default()
    };
    let p = build_provider(&s).expect("keyless custom endpoint must build");
    assert!(p.api_key.is_none());
    assert_eq!(p.base_url, "http://localhost:8000/v1");
    assert_eq!(p.model, "my-vllm-model");

    // With a key configured, plain http to a REMOTE host is refused…
    let mut remote = s.clone();
    remote.base_url = Some("http://gateway.example.com/v1".into());
    config::save_custom_key("http://gateway.example.com/v1", "sk-custom");
    assert!(build_provider(&remote).is_err());
    // …but loopback and https are both fine.
    config::save_custom_key("http://localhost:8000/v1", "sk-local");
    assert_eq!(
        build_provider(&s).unwrap().api_key.as_deref(),
        Some("sk-local")
    );
    let mut tls = s.clone();
    tls.base_url = Some("https://gateway.example.com/v1".into());
    config::save_custom_key("https://gateway.example.com/v1", "sk-tls");
    assert_eq!(
        build_provider(&tls).unwrap().api_key.as_deref(),
        Some("sk-tls")
    );
    // Each endpoint gets its own key, or none.
    let mut other = s.clone();
    other.base_url = Some("http://127.0.0.1:9000/v1".into());
    assert_eq!(build_provider(&other).unwrap().api_key, None);

    std::env::remove_var("NEXUS_HOME");
    let _ = std::fs::remove_dir_all(&h);
}

#[test]
fn loopback_url_detection() {
    assert!(is_loopback_url("http://localhost:8000/v1"));
    assert!(is_loopback_url("http://127.0.0.1:11434"));
    assert!(is_loopback_url("http://[::1]:8080/v1"));
    assert!(!is_loopback_url("http://gateway.example.com/v1"));
    assert!(!is_loopback_url("http://localhost.evil.com/v1"));
}

#[test]
fn classify_brainstorm_phrases() {
    assert!(matches!(
        classify("what should I name this?"),
        Mode::Brainstorm
    ));
    assert!(matches!(
        classify("any ideas for the API?"),
        Mode::Brainstorm
    ));
    assert!(matches!(classify("why is this slow"), Mode::Brainstorm));
    assert!(matches!(
        classify("i need some ideas on what to build"),
        Mode::Brainstorm
    ));
}

#[test]
fn classify_plan_phrases() {
    assert!(matches!(classify("design the auth system"), Mode::Plan));
    assert!(matches!(classify("architect a new module"), Mode::Plan));
    assert!(matches!(classify("break down the migration"), Mode::Plan));
}

#[test]
fn classify_build_phrases() {
    assert!(matches!(classify("add a login button"), Mode::Build));
    assert!(matches!(classify("fix the off-by-one bug"), Mode::Build));
    assert!(matches!(classify("refactor the parser"), Mode::Build));
}

#[test]
fn classify_short_defaults_brainstorm() {
    assert!(matches!(classify("foo bar"), Mode::Brainstorm));
}

#[test]
fn classify_long_unmatched_defaults_plan() {
    assert!(matches!(
        classify("the quick brown fox jumps over the lazy sleeping dog today"),
        Mode::Plan
    ));
}

#[test]
fn classify_is_case_insensitive() {
    assert!(matches!(classify("DESIGN the system"), Mode::Plan));
}

#[test]
fn a_task_typed_in_brainstorm_gets_a_hint_never_a_mode_change() {
    // The hint names PLAN; the mode stays where the person put it.
    let (target, hint) = mode_hint("build me a snake game", &Mode::Brainstorm).unwrap();
    assert_eq!(target, "PLAN");
    assert!(hint.contains("this looks like a task — Shift+Tab for PLAN"));
    // A long paste of notes reads as a plan-sized task: still only a hint.
    let notes = "we should build a cache layer, add retries and fix the flaky test. ".repeat(30);
    assert!(notes.len() > 2000);
    assert_eq!(mode_hint(&notes, &Mode::Brainstorm).unwrap().0, "PLAN");
    // A build task in PLAN suggests BUILD; matching modes say nothing.
    assert_eq!(
        mode_hint("fix the parser bug", &Mode::Plan).unwrap().0,
        "BUILD"
    );
    assert!(mode_hint("fix the parser bug", &Mode::Build).is_none());
    assert!(mode_hint("what if we used sqlite?", &Mode::Brainstorm).is_none());
}

fn git_fixture(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bwn-lib-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(&d)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(d.join("app.py"), "def main():\n    pass\n").unwrap();
    git(&["add", "."]);
    git(&["-c", "commit.gpgsign=false", "commit", "-qm", "init"]);
    d
}

fn log_count(d: &std::path::Path) -> usize {
    git_text(d, &["log", "--oneline"]).unwrap().lines().count()
}

#[test]
fn commit_shows_the_draft_and_commits_only_after_c() {
    let d = git_fixture("commit");
    // Nothing staged: no draft is requested.
    let mut drafted = false;
    let r = commit_flow(
        &d,
        |_, _| {
            drafted = true;
            Ok("x".into())
        },
        &mut |_| Some("c".into()),
    );
    assert!(r.is_none() && !drafted);

    std::fs::write(d.join("app.py"), "def greet():\n    pass\n").unwrap();
    git_text(&d, &["add", "app.py"]).unwrap();
    let draft = |stat: &str, diff: &str| {
        assert!(stat.contains("app.py") && diff.contains("+def greet"));
        Ok("feat: add greet helper".to_string())
    };
    let answer = |script: &'static [&'static str]| {
        let mut i = 0;
        move |q: &str| {
            if q.contains("run git here anyway") {
                return Some("y".to_string());
            }
            i += 1;
            script.get(i - 1).map(|a| a.to_string())
        }
    };
    // n: the draft is shown, git log is unchanged and the change stays staged.
    assert!(commit_flow(&d, draft, &mut answer(&["n"])).is_none());
    assert_eq!(log_count(&d), 1);
    assert!(!git_text(&d, &["diff", "--staged", "--stat"])
        .unwrap()
        .trim()
        .is_empty());
    // e, a new message, then c: bwn commits the edited message.
    let summary =
        commit_flow(&d, draft, &mut answer(&["e", "feat: greet people", "c"])).expect("committed");
    assert!(summary.ends_with("feat: greet people"), "{summary}");
    assert_eq!(log_count(&d), 2);
    assert!(crate::checkpoint::committed_since_turn(&d));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn undo_after_a_commit_says_commits_are_not_undone() {
    // /commit with no agent turn recorded in this folder.
    assert_eq!(
        undo_preamble(None, true),
        [
            "  commits are not undone by /undo — git reset --soft HEAD~1 keeps the changes",
            "  no agent turn recorded in this folder — /checkpoints lists what can be restored",
        ]
    );
    assert_eq!(
        undo_preamble(None, false),
        ["  no agent turn recorded in this folder — /checkpoints lists what can be restored"]
    );
    // A turn, then a commit made outside bwn (HEAD moved).
    let last = checkpoint::LastTurn {
        turn: checkpoint::Turn::default(),
        checkpoints: Vec::new(),
        this_session: true,
        head_moved: true,
    };
    assert_eq!(undo_preamble(Some(&last), false).len(), 1);
    let last = checkpoint::LastTurn {
        head_moved: false,
        ..last
    };
    assert!(undo_preamble(Some(&last), false).is_empty());
}

#[test]
fn a_model_pick_with_an_unknown_provider_word_is_named() {
    assert_eq!(
        unknown_provider_word("nonsense some-model"),
        Some("nonsense")
    );
    assert_eq!(unknown_provider_word("ollama tinycoder:3b"), None);
    assert_eq!(unknown_provider_word("tinycoder:3b"), None);
}

#[test]
fn resume_rows_name_age_messages_and_folder() {
    let mk = |title: &str, cwd: &str| session::SessionInfo {
        id: title.into(),
        title: title.into(),
        cwd: cwd.into(),
        model: "m".into(),
        created_ms: 0,
        updated_ms: 0,
        msg_count: 0,
        name: None,
    };
    let all = [
        mk("fix the parser", "/work/api"),
        mk("add a flag", "/work/cli"),
    ];
    let item = session_item(&all[0], std::path::Path::new("/work/api"));
    assert_eq!(item.label, "fix the parser");
    assert!(
        item.detail.ends_with("0 msgs · this folder"),
        "{}",
        item.detail
    );
    let here = std::path::Path::new("/work/api");
    assert_eq!(
        session_folder(&all[0].cwd, all[0].is_in(here)),
        "this folder"
    );
    assert_eq!(
        session_folder(&all[1].cwd, all[1].is_in(here)),
        "…/work/cli"
    );
}

#[test]
fn diff_lists_changed_and_new_files_with_one_summary() {
    let d = git_fixture("diff");
    std::fs::write(d.join("app.py"), "def main():\n    print('hi')\n").unwrap();
    std::fs::create_dir_all(d.join("pkg")).unwrap();
    std::fs::write(d.join("pkg/core.py"), "a = 1\nb = 2\n").unwrap();
    std::fs::create_dir_all(d.join("tests")).unwrap();
    std::fs::write(d.join("tests/test_core.py"), "x\n").unwrap();
    git_text(&d, &["add", "tests/test_core.py"]).unwrap();
    let (_, files) = changed_files(&d).unwrap();
    let listed: Vec<(&str, &str)> = files.iter().map(|f| (f.path.as_str(), f.kind())).collect();
    assert_eq!(
        listed,
        [
            ("app.py", "modified"),
            ("tests/test_core.py", "added"),
            ("pkg/", "new folder")
        ]
    );
    let app = &files[0];
    assert_eq!((app.added, app.removed), (1, 1));
    assert_eq!(files[2].added, 2, "lines in the new folder count");
    assert_eq!(
        diff_summary(&files),
        "3 files changed, 4 insertions(+), 1 deletion(-)"
    );
    // Porcelain with a rename keeps the new path only.
    assert_eq!(
        parse_porcelain("R  new.py\0old.py\0?? x/\0"),
        [
            ("R ".to_string(), "new.py".to_string()),
            ("??".into(), "x/".into())
        ]
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn copy_takes_the_last_answer_and_wraps_it_for_the_clipboard() {
    let t = vec![
        provider::Msg::User("q".into()),
        provider::Msg::Assistant {
            text: "first answer".into(),
            calls: vec![],
        },
        provider::Msg::Assistant {
            text: "  ".into(),
            calls: vec![],
        },
    ];
    assert_eq!(last_answer(&t), Some("first answer"));
    assert_eq!(last_answer(&[]), None);
    assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
}

#[test]
fn rewind_cuts_the_conversation_just_before_the_prompt() {
    use provider::Msg;
    let answer = |t: &str| Msg::Assistant {
        text: t.into(),
        calls: vec![],
    };
    let mut t = vec![
        Msg::System("sys".into()),
        Msg::User("first".into()),
        answer("a1"),
        Msg::User("second\n\n[hook context]\nbranch main".into()),
        answer("a2"),
        Msg::User("third".into()),
        answer("a3"),
    ];
    let point = |index, prompt: &str| RewindPoint {
        index,
        started_ms: 0,
        prompt: prompt.into(),
        typed: prompt.into(),
    };
    // A hook appended context to "second": still found.
    assert!(rewind_transcript(&mut t, &point(3, "second")));
    assert_eq!(t.len(), 3);
    assert!(matches!(t.last(), Some(Msg::Assistant { text, .. }) if text == "a1"));
    // The first prompt of a conversation (recorded at index 0, behind the
    // system prompt the turn added) leaves an empty conversation.
    assert!(rewind_transcript(&mut t, &point(0, "first")));
    assert!(t.is_empty());
    // A prompt that is no longer there changes nothing.
    let mut t = vec![Msg::System("sys".into()), Msg::User("summary".into())];
    assert!(!rewind_transcript(&mut t, &point(1, "gone")));
    assert_eq!(t.len(), 2);
}

#[test]
fn a_cancelled_plan_stays_in_plan() {
    assert!(matches!(
        mode_after_plan(agent::PlanEnd::Executed),
        Mode::Build
    ));
    assert!(matches!(
        mode_after_plan(agent::PlanEnd::Cancelled),
        Mode::Plan
    ));
    assert!(matches!(
        mode_after_plan(agent::PlanEnd::Answered),
        Mode::Plan
    ));
}

#[test]
fn conversational_turns_bypass_plan_and_build_dispatch() {
    assert!(should_answer_conversationally("hello", &Mode::Build));
    assert!(should_answer_conversationally(
        "what can you do?",
        &Mode::Plan
    ));
    assert!(should_answer_conversationally(
        "why is this slow?",
        &Mode::Build
    ));
}

#[test]
fn action_turns_stay_in_active_dispatch() {
    assert!(!should_answer_conversationally(
        "build me a canvas game",
        &Mode::Build
    ));
    assert!(!should_answer_conversationally(
        "find a folder named nexus",
        &Mode::Build
    ));
    assert!(!should_answer_conversationally(
        "read my projects file and tell me what repos I have",
        &Mode::Plan
    ));
}

#[test]
fn mode_cycles_correctly() {
    assert!(matches!(Mode::Plan.next(), Mode::Build));
    assert!(matches!(Mode::Build.next(), Mode::Brainstorm));
    assert!(matches!(Mode::Brainstorm.next(), Mode::Plan));
}

#[test]
fn detect_mode_switch_verb_prefixes() {
    assert!(matches!(
        detect_mode_switch("switch to plan mode"),
        Some(Mode::Plan)
    ));
    assert!(matches!(
        detect_mode_switch("change to build"),
        Some(Mode::Build)
    ));
    assert!(matches!(
        detect_mode_switch("go to brainstorm"),
        Some(Mode::Brainstorm)
    ));
    assert!(matches!(
        detect_mode_switch("set mode to planning"),
        Some(Mode::Plan)
    ));
    assert!(matches!(
        detect_mode_switch("use build mode"),
        Some(Mode::Build)
    ));
    assert!(matches!(
        detect_mode_switch("use brainstorm mode"),
        Some(Mode::Brainstorm)
    ));
}

#[test]
fn detect_mode_switch_bare_short_form() {
    assert!(matches!(detect_mode_switch("plan mode"), Some(Mode::Plan)));
    assert!(matches!(
        detect_mode_switch("build mode"),
        Some(Mode::Build)
    ));
    assert!(matches!(
        detect_mode_switch("brainstorm mode"),
        Some(Mode::Brainstorm)
    ));
    assert!(matches!(detect_mode_switch("planning"), Some(Mode::Plan)));
}

#[test]
fn detect_mode_switch_no_false_positives() {
    assert!(detect_mode_switch("build me a todo app").is_none());
    assert!(detect_mode_switch("plan the migration carefully").is_none());
    assert!(detect_mode_switch("let's brainstorm some ideas").is_none());
    assert!(detect_mode_switch("use this library instead").is_none());
}

#[test]
fn detect_permission_switch_verb_prefixes() {
    assert_eq!(
        detect_permission_switch("switch to readonly"),
        Some("readonly")
    );
    assert_eq!(detect_permission_switch("change to auto"), Some("auto"));
    assert_eq!(
        detect_permission_switch("set permission to ask"),
        Some("ask")
    );
    assert_eq!(
        detect_permission_switch("use readonly mode"),
        Some("readonly")
    );
}

#[test]
fn parse_cli_options_extracts_model_and_permission() {
    let (opts, rest) = parse_cli_options(vec![
        "--model".into(),
        "qwen3".into(),
        "--permission-mode=acceptEdits".into(),
        "fix".into(),
        "tests".into(),
    ])
    .unwrap();
    assert_eq!(opts.model.as_deref(), Some("qwen3"));
    assert_eq!(opts.permission_mode.as_deref(), Some("acceptEdits"));
    assert_eq!(rest, vec!["fix", "tests"]);
}

#[test]
fn parse_cli_options_extracts_sandbox_mode() {
    let (opts, rest) = parse_cli_options(
        ["--sandbox", "require", "run", "x"]
            .map(str::to_string)
            .to_vec(),
    )
    .unwrap();
    assert_eq!(opts.sandbox.as_deref(), Some("require"));
    assert_eq!(rest, ["run", "x"]);
    let (opts, _) = parse_cli_options(["--sandbox=auto"].map(str::to_string).to_vec()).unwrap();
    assert_eq!(opts.sandbox.as_deref(), Some("auto"));
    assert!(parse_cli_options(
        ["run", "--", "--sandbox", "auto"]
            .map(str::to_string)
            .to_vec()
    )
    .unwrap()
    .0
    .sandbox
    .is_none());
}

#[test]
fn parse_cli_options_effort_and_budget() {
    let (opts, rest) = parse_cli_options(
        ["--effort", "high", "--max-budget-usd=1.50", "run", "x"]
            .map(str::to_string)
            .to_vec(),
    )
    .unwrap();
    assert_eq!(opts.effort.as_deref(), Some("high"));
    assert_eq!(opts.max_budget_usd, Some(1.5));
    assert_eq!(rest, ["run", "x"]);
    // Defaults: no level, no budget.
    let (opts, _) = parse_cli_options(vec![]).unwrap();
    assert!(opts.effort.is_none());
    assert!(opts.max_budget_usd.is_none());
}

#[test]
fn legacy_exit_codes_flag_zeroes_only_incomplete_runs() {
    let (opts, rest) = parse_cli_options(
        ["--legacy-exit-codes", "run", "x"]
            .map(str::to_string)
            .to_vec(),
    )
    .unwrap();
    assert!(opts.legacy_exit_codes);
    assert_eq!(rest, ["run", "x"]);
    assert!(!parse_cli_options(vec![]).unwrap().0.legacy_exit_codes);
    use agent::Outcome;
    assert_eq!(headless_exit_code(Outcome::StepLimit, true, false), 6);
    assert_eq!(headless_exit_code(Outcome::StepLimit, true, true), 0);
    // A turn that errored keeps its code either way.
    assert_eq!(headless_exit_code(Outcome::ApprovalBlocked, false, true), 3);
    assert_eq!(headless_exit_code(Outcome::Failed, false, true), 1);
}

#[test]
fn the_blocked_line_names_what_was_blocked() {
    let calls: Vec<String> = ["write a.txt", "run: npm test", "remove b", "run: ls", "x"]
        .map(str::to_string)
        .to_vec();
    assert_eq!(
        blocked_line(&calls[..1], 0),
        "1 change was blocked for lack of approval and not made: write a.txt. \
             Re-run with --permission-mode auto to allow it."
    );
    assert_eq!(
        blocked_line(&calls, 0),
        "5 changes were blocked for lack of approval and not made: write a.txt; \
             run: npm test; remove b and 2 more. Re-run with --permission-mode auto to allow them."
    );
    // Auto would not allow these either: it is not offered.
    let one = blocked_line(&calls[..1], 1);
    assert!(!one.contains("Re-run with --permission-mode auto"), "{one}");
    assert!(
            one.ends_with("This one always needs a person to approve, even with --permission-mode auto: run bwn in a terminal to approve it."),
            "{one}"
        );
    let mixed = blocked_line(&calls, 2);
    assert!(
            mixed.ends_with("2 of them always need a person to approve, even with --permission-mode auto (run bwn in a terminal for those); auto allows the rest."),
            "{mixed}"
        );
}

#[test]
fn parse_cli_options_budget_rejects_missing_or_non_positive_values() {
    let err = parse_cli_options(["--max-budget-usd"].map(str::to_string).to_vec()).unwrap_err();
    assert!(err.contains("--max-budget-usd requires a value"), "{err}");
    let err =
        parse_cli_options(["--max-budget-usd", "abc"].map(str::to_string).to_vec()).unwrap_err();
    assert!(err.contains("positive dollar amount"), "{err}");
    let err =
        parse_cli_options(["--max-budget-usd", "0"].map(str::to_string).to_vec()).unwrap_err();
    assert!(err.contains("positive dollar amount"), "{err}");
    // The missing-value rule applies to --effort like every other option.
    let err = parse_cli_options(["--effort", "--json"].map(str::to_string).to_vec()).unwrap_err();
    assert!(err.contains("--effort requires a value"), "{err}");
    // After `--` the flags are literal task words.
    let (opts, rest) =
        parse_cli_options(["--", "--max-budget-usd", "5"].map(str::to_string).to_vec()).unwrap();
    assert!(opts.max_budget_usd.is_none());
    assert_eq!(rest, ["--max-budget-usd", "5"]);
}

#[test]
fn cli_separator_preserves_literal_option_names() {
    let (opts, rest) = parse_cli_options(
        ["--json", "run", "--", "explain", "--model", "--json"]
            .map(str::to_string)
            .to_vec(),
    )
    .unwrap();
    assert!(opts.json);
    assert!(opts.model.is_none());
    assert_eq!(rest, ["run", "explain", "--model", "--json"]);

    let (opts, rest) =
        parse_cli_options(["run", "--", "--json"].map(str::to_string).to_vec()).unwrap();
    assert!(!opts.json);
    assert_eq!(rest, ["run", "--json"]);

    // The shape workflow.rs spawns: a flag-like task stays the task.
    let (opts, rest) = parse_cli_options(
        ["run", "--json", "--", "--permission auto rm it"]
            .map(str::to_string)
            .to_vec(),
    )
    .unwrap();
    assert!(opts.json && opts.permission_mode.is_none());
    assert_eq!(rest, ["run", "--permission auto rm it"]);
}

#[test]
fn cli_options_reject_missing_or_empty_values() {
    for flag in [
        "--provider",
        "--model",
        "--permission-mode",
        "--permission",
        "--sandbox",
        "--prompt",
    ] {
        for args in [
            vec![flag.to_string()],
            vec![flag.to_string(), "--json".to_string()],
            vec![flag.to_string(), "".to_string()],
            vec![format!("{flag}=")],
        ] {
            let error = parse_cli_options(args).unwrap_err();
            assert!(
                error.contains(&format!("{flag} requires a value")),
                "{error}"
            );
        }
    }
}

#[test]
fn cli_inline_prompt_can_start_with_a_dash() {
    let (opts, rest) = parse_cli_options(
        ["--prompt=--model", "--model=example"]
            .map(str::to_string)
            .to_vec(),
    )
    .unwrap();
    assert_eq!(opts.prompt.as_deref(), Some("--model"));
    assert_eq!(opts.model.as_deref(), Some("example"));
    assert!(rest.is_empty());
}

#[test]
fn attachment_range_parsing() {
    assert_eq!(
        split_attachment_range("src/lib.rs:10-12"),
        ("src/lib.rs", Some((10, 12)))
    );
    assert_eq!(
        split_attachment_range("src/lib.rs:5"),
        ("src/lib.rs", Some((5, 5)))
    );
    assert_eq!(
        split_attachment_range("src/lib.rs:nope"),
        ("src/lib.rs:nope", None)
    );
}

#[test]
fn model_endpoint_pick_is_a_custom_url_not_openrouter() {
    // `/model http://localhost:8000/v1 my-model` → custom preset with
    // that base URL — never an OpenRouter "org/model" swap.
    assert_eq!(
        parse_model_endpoint("http://localhost:8000/v1 my-model"),
        Some(("http://localhost:8000/v1".into(), "my-model".into()))
    );
    assert_eq!(
        parse_model_endpoint("HTTPS://api.example.com/v1   gpt-x"),
        Some(("HTTPS://api.example.com/v1".into(), "gpt-x".into()))
    );
    // URL only: swap_model asks for the model name.
    assert_eq!(
        parse_model_endpoint("http://localhost:8000/v1"),
        Some(("http://localhost:8000/v1".into(), String::new()))
    );
    // Not a URL: the regular picker handles it.
    assert_eq!(parse_model_endpoint("custom my-model"), None);
    assert_eq!(parse_model_endpoint("meta-llama/llama-3.3-70b"), None);
    assert_eq!(parse_model_endpoint(""), None);
    // A stray URL reaching parse_model_pick stays on the current
    // provider instead of being misread as an OpenRouter org/model.
    assert_eq!(
        parse_model_pick("http://localhost:8000/v1", "anthropic"),
        ("anthropic".into(), "http://localhost:8000/v1".into())
    );
    // `/model custom <model>` keeps working.
    assert_eq!(
        parse_model_pick("custom my-model", "anthropic"),
        ("custom".into(), "my-model".into())
    );
}

#[test]
fn unknown_top_level_flags_are_rejected_not_prompts() {
    assert!(is_unknown_option("--modle"));
    assert!(is_unknown_option("-x"));
    assert!(is_unknown_option("--json=yes"));
    // Known flag spellings of subcommands and options pass through.
    for known in [
        "-v",
        "--version",
        "-h",
        "--help",
        "-p",
        "--print",
        "-c",
        "-r",
        "--resume",
    ] {
        assert!(!is_unknown_option(known), "{known}");
    }
    // Plain words still become the interactive prompt (`bwn fix the bug`).
    assert!(!is_unknown_option("fix"));
    assert!(!is_unknown_option(""));
    assert!(!is_unknown_option("-"));
    // Everything after `--` is literal text, even if it looks like a flag.
    let (opts, rest) =
        parse_cli_options(vec!["--".into(), "-weird".into(), "task".into()]).unwrap();
    assert!(opts.args_literal);
    assert_eq!(rest, vec!["-weird".to_string(), "task".to_string()]);
    let (opts, _) = parse_cli_options(vec!["fix".into(), "the".into(), "bug".into()]).unwrap();
    assert!(!opts.args_literal);
}

#[test]
fn oversized_text_attachment_is_truncated_with_marker() {
    let dir = std::env::temp_dir().join(format!("bwn-attach-big-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let big = dir.join("big.log");
    // 300 KiB of short lines, ending with a multi-byte char run so the
    // cut lands mid-sequence and must be repaired.
    let mut body = "0123456789abcde\n".repeat(300 * 1024 / 16);
    body.push_str("éééé");
    std::fs::write(&big, &body).unwrap();

    let att = read_text_attachment(&big, None).expect("big files attach truncated");
    assert_eq!(att.truncated_from_kib, Some(301));
    assert!(att
        .text
        .ends_with("\n[truncated: file is 301 KiB, first 256 KiB attached]"));
    let attached = att.text.split("\n[truncated").next().unwrap();
    assert!(attached.len() <= MAX_ATTACHED_FILE_BYTES as usize);
    assert!(attached.len() > MAX_ATTACHED_FILE_BYTES as usize - 32);
    assert!(attached.starts_with("0123456789abcde\n"));

    // Line ranges still apply on top of the truncated text.
    let ranged = read_text_attachment(&big, Some((2, 2))).unwrap();
    assert_eq!(ranged.text, "0123456789abcde");
    assert_eq!(ranged.truncated_from_kib, Some(301));

    // Small files are untouched.
    let small = dir.join("small.txt");
    std::fs::write(&small, "hi\n").unwrap();
    let att = read_text_attachment(&small, None).unwrap();
    assert_eq!(att.text, "hi\n");
    assert_eq!(att.truncated_from_kib, None);

    // The prompt carries the marker so the model knows the file is cut.
    let (text, _) = extract_attachments("look at @big.log", &dir, false);
    assert!(text.contains("[file: "));
    assert!(text.contains("[truncated: file is 301 KiB, first 256 KiB attached]"));
    assert!(!text.contains(" @big.log"), "token must not be pasted raw");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn workspace_rule_files_report_parse_failures() {
    let dir = std::env::temp_dir().join(format!("bwn-rules-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("good.json"),
        r#"{"rules":[{"id":"x","description":"d","severity":"low","message":"m"}]}"#,
    )
    .unwrap();
    std::fs::write(dir.join("bad.yaml"), "rules:\n  - id: nope\n").unwrap();

    let (rules, failures) = load_workspace_rule_files(&dir);
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].id, "x");
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].0, "bad.yaml");
    assert!(!failures[0].1.is_empty());
    assert!(
        !failures[0].1.contains("Failed to parse rules file"),
        "reason only: {}",
        failures[0].1
    );
    // No rules dir at all is not an error.
    let (rules, failures) = load_workspace_rule_files(&dir.join("missing"));
    assert!(rules.is_empty() && failures.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rules_listing_neutralizes_escapes_from_repo_rules() {
    let dir = std::env::temp_dir().join(format!("bwn-rules-esc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let rules = dir.join(".buildwithnexus").join("rules");
    std::fs::create_dir_all(&rules).unwrap();
    // OSC 52 sets the clipboard; the payload is base64 for "rm -rf ~".
    let rule = serde_json::json!({"rules": [{
        "id": "clip\u{1b}[2J",
        "description": "harmless\u{1b}]52;c;cm0gLXJmIH4=\u{7}",
        "severity": "low",
        "message": "m"
    }]});
    std::fs::write(rules.join("evil.json"), rule.to_string()).unwrap();
    if cfg!(unix) {
        std::fs::write(rules.join("bad\u{1b}]0;x\u{7}.yaml"), "rules: [").unwrap();
    }

    let out = rules_listing(&dir).join("\n");
    assert!(!out.contains("\u{1b}]"), "live OSC in /rules: {out:?}");
    assert!(
        !out.contains("\u{1b}[2J") && !out.contains('\u{7}'),
        "{out:?}"
    );
    assert!(out.contains("harmless␛]52;c;cm0gLXJmIH4="), "{out:?}");
    assert!(out.contains("clip␛[2J"), "{out:?}");
    if cfg!(unix) {
        assert!(out.contains("bad␛]0;x.yaml"), "{out:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn text_attachment_context_is_extracted() {
    let dir = std::env::temp_dir().join(format!("bwn-attach-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "one\ntwo\nthree\n").unwrap();

    let (text, images) = extract_attachments("please read @src/lib.rs:2-3", &dir, true);
    assert!(images.is_empty());
    assert!(text.contains("[file:"));
    assert!(text.contains("two\nthree"));
    assert!(!text.contains("\none\n"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sensitive_attachment_is_not_read() {
    let dir = std::env::temp_dir().join(format!("bwn-attach-secret-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".env.local"), "API_KEY=sk-live-secret\n").unwrap();
    std::fs::write(dir.join("id_rsa.png"), "not an image").unwrap();
    let (text, images) = extract_attachments("check @.env.local and id_rsa.png please", &dir, true);
    assert!(images.is_empty());
    assert!(!text.contains("sk-live-secret"), "{text}");
    assert!(!text.contains("[attached files]"), "{text}");
    assert!(text.contains("@.env.local"), "token stays as typed: {text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_at_word_naming_no_file_says_so() {
    let dir = std::env::temp_dir().join(format!("bwn-attach-missing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    tui::capture_start();
    let (text, _) = extract_attachments("explain @nosuchfile.py and @diff", &dir, false);
    let shown = tui::capture_take().join("\n");
    assert_eq!(text, "explain @nosuchfile.py and @diff");
    assert!(shown.contains("@nosuchfile.py not found"), "{shown}");
    // @diff with no changes keeps its own meaning and stays quiet.
    assert!(!shown.contains("@diff not found"), "{shown}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn url_attachment_requires_web_scheme() {
    assert!(is_web_url("https://example.com/x"));
    assert!(is_web_url("HTTP://example.com"));
    for bad in [
        "-K/etc/passwd",
        "file:///etc/passwd",
        "--config=x",
        "ftp://h",
        "",
    ] {
        assert!(!is_web_url(bad), "{bad}");
    }
    // Rejected values are never handed to curl, so nothing is attached.
    let (text, _) = extract_attachments("@url:-K/etc/passwd", &std::env::temp_dir(), false);
    assert!(!text.contains("[web:"), "{text}");
}

#[test]
fn symbol_attachment_treats_query_as_pattern() {
    let dir = std::env::temp_dir().join(format!("bwn-attach-sym-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.sh"), "rm --files-from list\n").unwrap();
    // A leading dash is a pattern, not a grep option.
    let (text, _) = extract_attachments("@symbol:--files-from", &dir, false);
    assert!(text.contains("[symbol search: --files-from]"), "{text}");
    assert!(text.contains("rm --files-from list"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn what_you_type_is_what_the_model_gets() {
    let dir = std::env::temp_dir().join(format!("bwn-attach-exact-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("app.py"), "line 1\nline 2\nline 3\n").unwrap();
    std::fs::write(dir.join("my notes.md"), "spaced\n").unwrap();
    // Prompts with no attachment are sent byte for byte.
    for typed in [
        "why does int(\"abc\") fail with 'abc'?",
        "line one\nline two",
        "tabs\tand  two spaces, \"double\" and 'single' quotes",
        "unbalanced \"quote and it's fine",
        "a \\ backslash and C:\\path\\x",
    ] {
        let (text, images) = extract_attachments(typed, &dir, true);
        assert_eq!(text, typed);
        assert!(images.is_empty());
    }
    // Attachments are replaced where they stand; the rest stays as typed.
    let typed = "see @app.py:1-2,\n\tthen \"explain\" it";
    let (text, _) = extract_attachments(typed, &dir, true);
    let file = format!("[file: {}]", dir.join("app.py").display());
    assert!(
        text.starts_with(&format!(
            "see {file},\n\tthen \"explain\" it\n\n[attached files]\n"
        )),
        "{text}"
    );
    assert!(
        text.contains("line 1\nline 2") && !text.contains("line 3"),
        "{text}"
    );
    // Quoted and escaped @paths with spaces are one attachment.
    for typed in [
        "read @\"my notes.md\" now",
        "read @'my notes.md' now",
        "read @my\\ notes.md now",
    ] {
        let (text, _) = extract_attachments(typed, &dir, true);
        let file = format!("[file: {}]", dir.join("my notes.md").display());
        assert!(
            text.starts_with(&format!("read {file} now\n\n")),
            "{typed}: {text}"
        );
    }
    // The composer's own tokens for dropped files round-trip.
    for name in ["my notes.md", "say \"hi\" notes.md", "back\\slash notes.md"] {
        std::fs::write(dir.join(name), "dropped\n").unwrap();
        let token = crate::tui::attachment_token(&dir.join(name));
        let (text, _) = extract_attachments(&format!("read {token}"), &dir, true);
        assert!(text.contains("dropped"), "{token}: {text}");
    }
    // A missing file stays exactly as typed.
    let (text, _) = extract_attachments("open @nope.py  please", &dir, true);
    assert_eq!(text, "open @nope.py  please");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn image_path_before_sentence_punctuation_attaches() {
    let dir = std::env::temp_dir().join(format!("bwn-attach-punct-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // 1x1 PNG.
    let png = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82u8,
    ];
    std::fs::write(dir.join("shot.png"), png).unwrap();
    for prompt in [
        "what is in @shot.png?",
        "describe shot.png.",
        "see @shot.png, then fix it",
    ] {
        let (_, images) = extract_attachments(prompt, &dir, true);
        assert_eq!(images.len(), 1, "{prompt}");
    }
    std::fs::write(dir.join("My Shot.png"), png).unwrap();
    let typed = format!(
        "what's in '{}'?\nand (shot.png)",
        dir.join("My Shot.png").display()
    );
    let (text, images) = extract_attachments(&typed, &dir, true);
    assert_eq!(images.len(), 2, "{text}");
    assert_eq!(
        text,
        "what's in [image: My Shot.png]?\nand ([image: shot.png])"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unattended_settings_prefers_flag_then_first_env_key() {
    let none = |_: &str| false;
    assert!(unattended_settings(None, none).is_none());
    let openai_only = |k: &str| k == "OPENAI_API_KEY";
    assert_eq!(
        unattended_settings(None, openai_only).unwrap().provider,
        "openai"
    );
    let both = |k: &str| k == "OPENAI_API_KEY" || k == "ANTHROPIC_API_KEY";
    assert_eq!(
        unattended_settings(None, both).unwrap().provider,
        "anthropic"
    );
    assert_eq!(
        unattended_settings(Some("ollama"), none).unwrap().provider,
        "ollama"
    );
    assert_eq!(
        unattended_settings(Some("nope"), none).unwrap().provider,
        "nope"
    );
}

#[test]
fn build_provider_rejects_http_for_keyed_preset() {
    let s = Settings {
        provider: "openai".into(),
        model: String::new(),
        permission: "ask".into(),
        base_url: Some("http://insecure.local/v1".into()),
        allowed_commands: Vec::new(),
        ..Default::default()
    };
    match build_provider(&s) {
        Err(e) => assert!(e.contains("non-HTTPS")),
        Ok(_) => panic!("expected http base_url to be rejected"),
    }
}

#[test]
fn build_provider_unknown_provider() {
    let s = Settings {
        provider: "does-not-exist".into(),
        model: String::new(),
        permission: "ask".into(),
        base_url: None,
        allowed_commands: Vec::new(),
        ..Default::default()
    };
    match build_provider(&s) {
        Err(e) => assert!(e.contains("unknown provider")),
        Ok(_) => panic!("expected unknown provider error"),
    }
}

#[test]
fn build_provider_local_preset_allows_http() {
    let s = Settings {
        provider: "ollama".into(),
        model: String::new(),
        permission: "ask".into(),
        base_url: Some("http://localhost:11434/v1".into()),
        allowed_commands: Vec::new(),
        ..Default::default()
    };
    match build_provider(&s) {
        Ok(p) => {
            assert!(p.api_key.is_none());
            assert_eq!(p.model, "llama3.2");
        }
        Err(e) => panic!("local http should build: {e}"),
    }
}

#[test]
fn test_handle_voice_missing_file_returns_none() {
    let res = super::handle_voice("/nonexistent/audio/path.wav");
    assert!(res.is_none());
}

#[test]
fn test_handle_kb_index_and_rules() {
    let dir = std::env::temp_dir().join(format!("bwn-kb-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file_path = dir.join("test_code.rs");
    std::fs::write(
        &file_path,
        "pub fn authenticate_user() {}\npub struct SessionData {}",
    )
    .unwrap();
    super::handle_kb_index(&dir);

    let kb = crate::knowledge::KnowledgeBase::new(&dir.to_string_lossy());
    assert!(!kb.entities.is_empty());
    assert!(kb.entities.values().any(|e| e.name == "authenticate_user"));
    assert!(kb.entities.values().any(|e| e.name == "SessionData"));

    super::handle_rules(&dir);
    let _ = std::fs::remove_dir_all(&dir);
}

// A symlink back up the tree must not make the indexer walk the project
// again and again under ever longer paths.
#[cfg(unix)]
#[test]
fn kb_index_does_not_follow_a_symlink_loop() {
    let dir = std::env::temp_dir().join(format!("bwn-kb-loop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
    std::os::unix::fs::symlink("..", dir.join("src/up")).unwrap();
    super::handle_kb_index(&dir);
    let kb = crate::knowledge::KnowledgeBase::new(&dir.to_string_lossy());
    let ids: Vec<&String> = kb.entities.keys().collect();
    assert_eq!(ids, ["alpha@src/a.rs"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_handle_verify_audit() {
    let dir = std::env::temp_dir().join(format!("bwn-verify-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file_path = dir.join("test_file.rs");
    std::fs::write(&file_path, "fn main() {}\n").unwrap();
    assert!(super::handle_verify_audit(
        crate::agent::Permission::Auto,
        &dir
    ));
    // Read-only sessions must not run the project's build/test commands.
    assert!(!super::handle_verify_audit(
        crate::agent::Permission::ReadOnly,
        &dir
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_check_and_offer_install_dependencies() {
    super::check_and_offer_install_dependencies();
}

#[test]
fn missing_tools_get_an_install_command_that_bwn_never_runs() {
    let page = "https://github.com/BurntSushi/ripgrep#installation";
    assert_eq!(
        install_hint("ripgrep", "ripgrep", page, false, true, false),
        "sudo apt-get install ripgrep"
    );
    assert_eq!(
        install_hint("ripgrep", "ripgrep", page, true, true, false),
        "brew install ripgrep"
    );
    // No package manager known, and Windows (where winget may be absent):
    // the download page.
    assert_eq!(
        install_hint("ripgrep", "ripgrep", page, false, false, false),
        page
    );
    assert_eq!(
        install_hint("ripgrep", "ripgrep", page, true, true, true),
        page
    );
    // Only git is needed by bwn itself; the rest are optional.
    let needed: Vec<&str> = DEPENDENCIES.iter().filter(|d| d.5).map(|d| d.0).collect();
    assert_eq!(needed, ["git"]);
}

// A loopback HTTP server answering every request with `respond(method,
// path, body)` → (status, JSON body). Runs until the test process exits.
fn mock_http(respond: fn(&str, &str, &str) -> (u16, String)) -> u16 {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).is_err() {
                continue;
            }
            let mut parts = first.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let path = parts.next().unwrap_or("").to_string();
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            let _ = reader.read_exact(&mut body);
            let (status, reply) = respond(&method, &path, &String::from_utf8_lossy(&body));
            let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
        }
    });
    port
}

// Runs `swap_model` against settings saved under a scratch NEXUS_HOME and
// returns the settings it left behind plus the live provider.
fn swap_with_saved(
    saved: config::Settings,
    target: &str,
    model: &str,
) -> (config::Settings, Provider) {
    let _g = config::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!(
        "bwn-swap-{}-{}",
        std::process::id(),
        saved.provider
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("NEXUS_HOME", &home);
    config::save_settings(&saved);
    let mut provider = build_provider(&saved).unwrap();
    swap_model(&mut provider, target, model, None);
    let after = config::load_settings().unwrap();
    std::env::remove_var("NEXUS_HOME");
    let _ = std::fs::remove_dir_all(&home);
    (after, provider)
}

#[test]
fn same_provider_swap_on_a_remote_ollama_keeps_its_base_url() {
    let port = mock_http(|_, path, _| match path {
        "/api/tags" => (200, r#"{"models":[{"name":"m1"},{"name":"m2"}]}"#.into()),
        _ => (200, "{}".into()),
    });
    let url = format!("http://127.0.0.1:{port}");
    let (after, provider) = swap_with_saved(
        config::Settings {
            provider: "ollama".into(),
            model: "m1".into(),
            base_url: Some(url.clone()),
            ..Default::default()
        },
        "ollama",
        "m2",
    );
    assert_eq!(after.model, "m2");
    assert_eq!(after.base_url.as_deref(), Some(url.as_str()));
    assert_eq!(provider.base_url, url);
}

// An OpenAI-compatible local server that only answers as "local-model".
fn local_model_only(method: &str, path: &str, body: &str) -> (u16, String) {
    match (method, path) {
        ("GET", "/v1/models") => (200, r#"{"data":[{"id":"local-model"}]}"#.into()),
        ("POST", "/v1/chat/completions") if body.contains(r#""model":"local-model""#) => {
            (200, r#"{"choices":[{"message":{"content":"ok"}}]}"#.into())
        }
        ("POST", _) => (404, r#"{"error":"model not found"}"#.into()),
        _ => (200, "{}".into()),
    }
}

#[test]
fn local_server_swaps_use_the_saved_base_url_and_report_the_fallback() {
    for preset in ["lmstudio", "llamacpp"] {
        let port = mock_http(local_model_only);
        let url = format!("http://127.0.0.1:{port}/v1");
        let (after, provider) = swap_with_saved(
            config::Settings {
                provider: preset.into(),
                model: "old".into(),
                base_url: Some(url.clone()),
                ..Default::default()
            },
            preset,
            "qwen-7b",
        );
        assert_eq!(after.base_url.as_deref(), Some(url.as_str()), "{preset}");
        assert_eq!(after.model, "local-model", "{preset}");
        assert_eq!(provider.model, "local-model", "{preset}");
        assert_eq!(provider.base_url, url, "{preset}");
    }
}

// (from, saved base_url, to, override URL, expected target)
type SwapCase = (
    &'static str,
    Option<&'static str>,
    &'static str,
    Option<&'static str>,
    SwapTarget,
);

#[test]
fn plan_swap_over_preset_transitions() {
    let preset = |id| config::preset(id).unwrap();
    let keep = |url: &str| SwapTarget {
        base_url: url.into(),
        save: None,
    };
    let reset = |id| SwapTarget {
        base_url: preset(id).base_url.into(),
        save: Some(None),
    };
    let remote_ollama = "http://gpu-box:11434";
    let lan_lmstudio = "http://10.0.0.5:1234/v1";
    let lan_llama = "http://10.0.0.6:8080/v1";
    let table: Vec<SwapCase> = vec![
        (
            "ollama",
            Some(remote_ollama),
            "ollama",
            None,
            keep(remote_ollama),
        ),
        (
            "ollama",
            None,
            "ollama",
            None,
            keep(preset("ollama").base_url),
        ),
        (
            "ollama",
            Some(remote_ollama),
            "anthropic",
            None,
            reset("anthropic"),
        ),
        ("anthropic", None, "ollama", None, reset("ollama")),
        (
            "lmstudio",
            Some(lan_lmstudio),
            "lmstudio",
            None,
            keep(lan_lmstudio),
        ),
        (
            "llamacpp",
            Some(lan_llama),
            "llamacpp",
            None,
            keep(lan_llama),
        ),
        (
            "llamacpp",
            Some(lan_llama),
            "lmstudio",
            None,
            reset("lmstudio"),
        ),
        (
            "lmstudio",
            Some(lan_lmstudio),
            "ollama",
            None,
            reset("ollama"),
        ),
        ("openrouter", None, "openai", None, reset("openai")),
        (
            "anthropic",
            None,
            "custom",
            Some("http://127.0.0.1:9000/v1"),
            SwapTarget {
                base_url: "http://127.0.0.1:9000/v1".into(),
                save: Some(Some("http://127.0.0.1:9000/v1".into())),
            },
        ),
        (
            "custom",
            Some("http://127.0.0.1:9000/v1"),
            "custom",
            None,
            keep("http://127.0.0.1:9000/v1"),
        ),
    ];
    for (from, saved, to, url, want) in table {
        assert_eq!(
            plan_swap(from, saved, None, preset(to), url),
            want,
            "{from} ({saved:?}) → {to} ({url:?})"
        );
    }
}

#[test]
fn plan_swap_returns_to_the_address_last_used_with_a_provider() {
    let preset = |id| config::preset(id).unwrap();
    let lan = "http://192.168.50.10:11434";
    // LM Studio → Ollama: back to the LAN box, and it is saved again.
    assert_eq!(
        plan_swap("lmstudio", None, Some(lan), preset("ollama"), None),
        SwapTarget {
            base_url: lan.into(),
            save: Some(Some(lan.into())),
        }
    );
    // Within a provider the saved address still wins, and a typed URL
    // wins over both.
    assert_eq!(
        plan_swap(
            "ollama",
            Some("http://a:11434"),
            Some(lan),
            preset("ollama"),
            None
        )
        .base_url,
        "http://a:11434"
    );
    assert_eq!(
        plan_swap(
            "lmstudio",
            None,
            Some(lan),
            preset("ollama"),
            Some("http://b:11434")
        )
        .base_url,
        "http://b:11434"
    );

    // Leaving a provider remembers its address; arriving records the new one.
    let own = |provider: &str, url: Option<&str>| config::Settings {
        provider: provider.into(),
        base_url: url.map(String::from),
        ..Default::default()
    };
    let map = remember_endpoints(
        &own("ollama", Some(lan)),
        "ollama",
        "lmstudio",
        Some(Some("http://127.0.0.1:1234/v1")),
    );
    assert_eq!(map.get("ollama").map(String::as_str), Some(lan));
    assert_eq!(
        map.get("lmstudio").map(String::as_str),
        Some("http://127.0.0.1:1234/v1")
    );
    // A preset default (no saved URL) adds nothing.
    assert!(
        remember_endpoints(&own("anthropic", None), "anthropic", "openai", Some(None)).is_empty()
    );
    // Only the user's own addresses are remembered: a base_url a trusted
    // project layers in serves that project, never every project.
    let mine = own("custom", Some("http://my-gateway:8080/v1"));
    let map = remember_endpoints(&mine, "custom", "ollama", Some(Some(lan)));
    assert_eq!(map["custom"], "http://my-gateway:8080/v1");
    let map = remember_endpoints(&mine, "custom", "custom", None);
    assert_eq!(map["custom"], "http://my-gateway:8080/v1");
    let map = remember_endpoints(&own("anthropic", None), "custom", "ollama", Some(Some(lan)));
    assert_eq!(map.keys().collect::<Vec<_>>(), ["ollama"]);
}

#[test]
fn ollama_addresses_pick_the_ollama_preset() {
    assert!(is_ollama_address("http://192.168.50.10:11434"));
    assert!(is_ollama_address("http://gpu-box:11434/"));
    assert!(is_ollama_address("http://localhost:11434/v1"));
    assert!(is_ollama_address("http://user:pw@host:11434"));
    assert!(!is_ollama_address("http://127.0.0.1:8081/v1"));
    assert!(!is_ollama_address("https://gateway.example/11434"));
    assert_eq!(endpoint_preset("http://192.168.50.10:11434"), "ollama");
    assert_eq!(endpoint_preset("http://192.168.50.10:11434/"), "ollama");
    // Ollama's OpenAI-compatible /v1 stays the custom preset, as before.
    assert_eq!(endpoint_preset("http://192.168.50.10:11434/v1"), "custom");
    // A path, or nothing answering /api/tags, means an OpenAI-compatible
    // endpoint.
    assert_eq!(endpoint_preset("http://127.0.0.1:9/v1"), "custom");
    assert_eq!(endpoint_preset("http://127.0.0.1:9"), "custom");
    // An Ollama on another port is recognised by its own API.
    let port = mock_http(|_, path, _| match path {
        "/api/tags" => (200, r#"{"models":[{"name":"tinycoder:3b"}]}"#.into()),
        _ => (404, "{}".into()),
    });
    assert_eq!(
        endpoint_preset(&format!("http://127.0.0.1:{port}")),
        "ollama"
    );
}

#[test]
fn swap_back_to_ollama_goes_to_the_remembered_host() {
    let port = mock_http(|_, path, _| match path {
        "/api/tags" => (200, r#"{"models":[{"name":"tinycoder:3b"}]}"#.into()),
        _ => (200, "{}".into()),
    });
    let lan = format!("http://127.0.0.1:{port}");
    let (after, provider) = swap_with_saved(
        config::Settings {
            provider: "lmstudio".into(),
            model: "tinycoder-7b-instruct".into(),
            endpoints: [("ollama".to_string(), lan.clone())].into(),
            ..Default::default()
        },
        "ollama",
        "tinycoder:3b",
    );
    assert_eq!(after.provider, "ollama");
    assert_eq!(after.base_url.as_deref(), Some(lan.as_str()));
    assert_eq!(provider.base_url, lan);
    assert_eq!(after.endpoints.get("ollama"), Some(&lan));
}

#[test]
fn swap_success_line_names_the_model_actually_saved() {
    let same = swap_success_line("qwen", "qwen", "LM Studio");
    assert!(same.contains("→ qwen on LM Studio"), "{same}");
    let fell_back = swap_success_line("qwen", "local-model", "LM Studio");
    assert!(
        fell_back.contains("→ local-model on LM Studio"),
        "{fell_back}"
    );
    assert!(fell_back.contains("'qwen'"), "{fell_back}");
}

#[test]
fn llama_server_is_found_as_an_exe_on_windows() {
    let dir = std::env::temp_dir().join(format!("bwn-llama-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // PATHEXT spells .EXE in upper case; Windows disks ignore case, this
    // one may not.
    let exe = dir.join("llama-server.EXE");
    std::fs::write(&exe, "x").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::join_paths([&dir]).unwrap();
    assert_eq!(find_llama_server_in(&path, None, true), Some(exe.clone()));
    assert_eq!(
        find_llama_server_in(&path, Some(std::ffi::OsStr::new(".COM;.EXE")), true),
        Some(exe)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_typed_permission_switch_lasts_for_the_session_only() {
    let _g = crate::config::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("bwn-perm-scope-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("NEXUS_HOME", &home);
    let file = home.join("settings.json");
    std::fs::write(
        &file,
        r#"{"provider":"anthropic","model":"m","permission":"ask"}"#,
    )
    .unwrap();
    let saved = || -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap()
    };
    let cwd = home.clone();
    let mut perm = Permission::Ask;

    // "use auto" and `/permissions auto` change the session only.
    let phrase = super::detect_permission_switch("use auto").unwrap();
    super::apply_permission(&mut perm, phrase, super::PermScope::Session);
    assert_eq!(perm, Permission::Auto);
    assert_eq!(saved()["permission"], "ask");
    super::handle_permissions_arg(&mut perm, &cwd, "accept-edits");
    assert_eq!(perm, Permission::AcceptEdits);
    assert_eq!(saved()["permission"], "ask");
    // Saving the default is its own, explicit choice.
    super::handle_permissions_arg(&mut perm, &cwd, "default auto");
    assert_eq!(perm, Permission::Auto);
    assert_eq!(saved()["permission"], "auto");
    // A name that isn't a mode changes nothing.
    super::handle_permissions_arg(&mut perm, &cwd, "yolo2");
    assert_eq!(perm, Permission::Auto);
    // The number shortcuts keep their 0.14 meaning (3 never loosens
    // to auto); accept-edits is the new 4.
    for (arg, want) in [
        ("1", Permission::Ask),
        ("3", Permission::ReadOnly),
        ("2", Permission::Auto),
        ("4", Permission::AcceptEdits),
    ] {
        super::handle_permissions_arg(&mut perm, &cwd, arg);
        assert_eq!(perm, want, "/permissions {arg}");
    }

    std::env::remove_var("NEXUS_HOME");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn local_servers_start_with_the_configured_one() {
    let s = Settings {
        provider: "ollama".into(),
        base_url: Some("http://192.168.50.10:11434".into()),
        ..Default::default()
    };
    let bases: Vec<(&str, String)> = local_servers(&s)
        .into_iter()
        .map(|l| (l.preset, l.base))
        .collect();
    assert_eq!(
        bases,
        vec![
            ("ollama", "http://192.168.50.10:11434".to_string()),
            ("ollama", "http://localhost:11434".to_string()),
            ("llamacpp", "http://localhost:8080/v1".to_string()),
            ("lmstudio", "http://localhost:1234/v1".to_string()),
            ("custom", "http://localhost:8000/v1".to_string()),
        ]
    );
    // LM Studio moved to another port: listed once, at that port.
    let s = Settings {
        provider: "lmstudio".into(),
        base_url: Some("http://localhost:1235/v1/".into()),
        ..Default::default()
    };
    let servers = local_servers(&s);
    assert_eq!(servers[0].base, "http://localhost:1235/v1");
    assert_eq!(servers[0].label, "LM Studio");
    assert_eq!(servers.len(), 5);
    // A hosted provider adds nothing of its own.
    let s = Settings {
        provider: "openai".into(),
        base_url: Some("https://api.openai.com/v1".into()),
        ..Default::default()
    };
    assert_eq!(local_servers(&s).len(), 4);
    // Addresses /model remembers for local presets come next; a hosted
    // one is not a local server.
    let s = Settings {
        provider: "openai".into(),
        base_url: Some("https://api.openai.com/v1".into()),
        endpoints: [
            (
                "ollama".to_string(),
                "http://192.168.50.10:11434".to_string(),
            ),
            (
                "openai".to_string(),
                "https://api.openai.com/v1".to_string(),
            ),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let servers = local_servers(&s);
    assert_eq!(servers[0].base, "http://192.168.50.10:11434");
    assert_eq!(servers[0].preset, "ollama");
    assert_eq!(servers.len(), 5);
}

// Answers GETs: /api/tags only when `ollama`, /v1/models always.
fn model_server(ollama: bool) -> String {
    use std::io::{BufRead, BufReader, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in l.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut first = String::new();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let _ = reader.read_line(&mut first);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
            }
            let (code, body) = if first.contains("/api/tags") && ollama {
                (200, r#"{"models":[{"name":"tinycoder:3b"}]}"#)
            } else if first.contains("/v1/models") {
                (200, r#"{"data":[{"id":"x"}]}"#)
            } else {
                (404, r#"{"error":"Unexpected endpoint"}"#)
            };
            let _ = write!(
                    stream,
                    "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
        }
    });
    base
}

#[test]
fn a_gguf_swap_never_lands_on_ollama() {
    let ollama = format!("{}/v1", model_server(true));
    let llama = format!("{}/v1", model_server(false));
    assert_eq!(
        find_active_local_base_url_in(&[&ollama, &llama]),
        Some(llama.clone())
    );
    assert_eq!(find_active_local_base_url_in(&[&ollama]), None);

    assert_eq!(
        gguf_unservable("tinycoder-7b-q4_k_m.gguf", false),
        Some("llama-server is not installed — install llama.cpp, or load the file in LM Studio")
    );
    assert!(gguf_unservable("sub/Model.GGUF", false).is_some());
    assert_eq!(gguf_unservable("tinycoder-7b-q4_k_m.gguf", true), None);
    assert_eq!(gguf_unservable("tinycoder-7b-instruct", false), None);
}

#[test]
fn context_breakdown_counts_each_part_of_the_next_request() {
    let msgs = vec![
        provider::Msg::System("s".repeat(400)),
        provider::Msg::User("u".repeat(80)),
        provider::Msg::UserImages {
            text: "t".repeat(40),
            images: vec![("image/png".into(), "A".repeat(1_200))],
        },
        provider::Msg::Assistant {
            text: "a".repeat(40),
            calls: vec![],
        },
        provider::Msg::Tool(vec![provider::ToolResult {
            id: "1".into(),
            content: "r".repeat(40),
            is_error: false,
            images: Vec::new(),
        }]),
    ];
    let tools = vec![tools::ToolDef {
        name: "read_file",
        description: "Read a file.",
        schema: serde_json::json!({}),
    }];
    let b = context_breakdown(&msgs, &tools);
    assert_eq!(b.system, 100);
    assert_eq!(b.conversation, 20 + 10 + 10 + 10);
    assert_eq!(b.images, 100);
    assert_eq!(b.tools, (9 + 12 + 2) / 4);
    assert_eq!(b.mcp_tools, 0);
    assert_eq!(b.total(), 100 + 50 + 100 + 5);
}
