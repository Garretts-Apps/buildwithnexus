use super::*;

#[test]
fn at_completion_offers_files_from_added_folders_by_full_path() {
    let base = std::env::temp_dir().join(format!("bwn-at-added-{}", std::process::id()));
    let (cwd, other) = (base.join("cwd"), base.join("other"));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(other.join("src")).unwrap();
    std::fs::write(other.join("src/zebra_widget.rs"), "").unwrap();
    crate::workdirs::clear();
    assert!(path_candidates("zebra_w", &cwd).is_empty());
    crate::workdirs::add(&other.display().to_string(), &cwd).unwrap();
    let want = other
        .canonicalize()
        .unwrap()
        .join("src/zebra_widget.rs")
        .display()
        .to_string();
    assert!(path_candidates("zebra_w", &cwd).contains(&want));
    crate::workdirs::clear();
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn at_completion_skips_what_gitignore_and_the_file_tools_skip() {
    let cwd = std::env::temp_dir().join(format!("bwn-at-ignore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cwd);
    for f in [
        "gen/out.py",
        "src/app.py",
        "src/debug.log",
        "node_modules/x.js",
    ] {
        std::fs::create_dir_all(cwd.join(f).parent().unwrap()).unwrap();
        std::fs::write(cwd.join(f), "").unwrap();
    }
    std::fs::create_dir_all(cwd.join(".git")).unwrap();
    std::fs::write(cwd.join("id_rsa"), "").unwrap();
    std::fs::write(cwd.join(".gitignore"), "gen/\n*.log\n").unwrap();
    // The folder and the files inside it are hidden, by name or by path.
    assert!(path_candidates("ge", &cwd).is_empty());
    assert!(path_candidates("gen/", &cwd).is_empty());
    assert!(path_candidates("node_m", &cwd).is_empty());
    assert!(path_candidates("id_r", &cwd).is_empty());
    assert_eq!(path_candidates("src/", &cwd), vec!["src/app.py"]);
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn the_footer_counts_added_folders() {
    assert_eq!(dirs_badge(0), "");
    assert_eq!(strip_ansi(&dirs_badge(1)), " · +1 dir");
    assert_eq!(strip_ansi(&dirs_badge(3)), " · +3 dirs");
}

#[test]
fn a_capturing_thread_keeps_its_lines_and_others_are_untouched() {
    capture_start();
    line("one\ntwo");
    write_stream("thinking ›");
    write_stream(" a\npart");
    commit_stream_line("rendered part");
    write_stream("chrome");
    retract_stream_line();
    assert!(capture_event("{\"type\":\"notice\"}"));
    // A prompt from the helper goes to the screen, not into the block.
    assert!(uncaptured(|| !capturing()));
    assert!(capturing());
    // Another thread is never captured.
    assert!(!std::thread::spawn(capturing).join().unwrap());
    assert_eq!(
        capture_take(),
        [
            "one",
            "two",
            "thinking › a",
            "rendered part",
            "{\"type\":\"notice\"}"
        ]
    );
    assert!(!capturing());
    assert!(capture_take().is_empty());
}

#[test]
fn cube_quantizes_to_nearest_step() {
    assert_eq!(cube(0), 0);
    assert_eq!(cube(255), 5);
    assert_eq!(cube(135), 2);
    assert_eq!(cube(94), 1);
}

#[test]
fn word_motion_boundaries() {
    let b: Vec<char> = "foo  bar baz".chars().collect();
    assert_eq!(prev_word(&b, b.len()), 9);
    assert_eq!(prev_word(&b, 9), 5);
    assert_eq!(prev_word(&b, 0), 0);
    assert_eq!(next_word(&b, 0), 3);
    assert_eq!(next_word(&b, 3), 8);
    assert_eq!(next_word(&b, b.len()), b.len());
}

#[test]
fn viewport_keeps_cursor_visible() {
    let buf = &['a'; 25];
    assert_eq!(viewport(buf, 3, 10, 0), (0, 3));
    assert_eq!(viewport(buf, 20, 10, 0), (11, 9));
    assert_eq!(viewport(buf, 2, 10, 11), (2, 0));
    assert_eq!(viewport(buf, 9, 10, 0), (0, 9));
    assert_eq!(viewport(buf, 15, 10, 11), (11, 4));
}

#[test]
fn viewport_handles_zero_width() {
    let buf = &['a'; 10];
    let (s, col) = viewport(buf, 5, 0, 3);
    assert!(col < 1 || s <= 5);
    let _ = (s, col);
}

#[test]
fn selection_range_handles_single_and_multi_line_drags() {
    let one = Selection {
        anchor: SelectPos { row: 2, col: 3 },
        focus: SelectPos { row: 2, col: 7 },
        sticky: false,
    };
    assert_eq!(selection_range_for(one, 2, &"x".repeat(20)), Some((3, 8)));
    assert_eq!(selection_range_for(one, 1, &"x".repeat(20)), None);

    let many = Selection {
        anchor: SelectPos { row: 1, col: 4 },
        focus: SelectPos { row: 3, col: 2 },
        sticky: false,
    };
    assert_eq!(selection_range_for(many, 1, &"x".repeat(10)), Some((4, 10)));
    assert_eq!(selection_range_for(many, 2, &"x".repeat(10)), Some((0, 10)));
    assert_eq!(selection_range_for(many, 3, &"x".repeat(10)), Some((0, 3)));
}

#[test]
fn selection_uses_terminal_cells_and_keeps_combining_marks() {
    let select = |line: &str, from, to| {
        let sel = Selection {
            anchor: SelectPos { row: 0, col: from },
            focus: SelectPos { row: 0, col: to },
            sticky: false,
        };
        selection_range_for(sel, 0, line)
            .map(|(a, b)| line.chars().skip(a).take(b - a).collect::<String>())
    };
    assert_eq!(select("你 hello", 3, 7).as_deref(), Some("hello"));
    assert_eq!(select("你 hello", 1, 1).as_deref(), Some("你"));
    assert_eq!(select("🙂 ok", 3, 4).as_deref(), Some("ok"));
    assert_eq!(select("e\u{301} ok", 0, 0).as_deref(), Some("e\u{301}"));
    assert_eq!(select("e\u{301} ok", 2, 3).as_deref(), Some("ok"));
    assert_eq!(select("你 hello", 7, 3).as_deref(), Some("hello"));
    assert_eq!(select("short", 20, 25), None);
}

#[test]
fn word_selection_reports_display_columns() {
    assert_eq!(word_span_at("你 hello", 3), Some((3, 7)));
    assert_eq!(word_span_at("你 hello", 1), Some((0, 1)));
    assert_eq!(word_span_at("🙂 hello", 4), Some((3, 7)));
    assert_eq!(word_span_at("e\u{301} hello", 0), Some((0, 0)));
    assert_eq!(word_span_at("你 hello", 2), None);
}

#[test]
fn completion_replaces_suffix_without_duplicating_spaces() {
    let mut buf: Vec<char> = "/help".chars().collect();
    let mut cursor = 3;
    apply_completion(&mut buf, &mut cursor, "/help", true);
    assert_eq!(buf.iter().collect::<String>(), "/help ");
    assert_eq!(cursor, 6);

    let mut buf: Vec<char> = "/mode brainstorm tail".chars().collect();
    let mut cursor = 8;
    apply_completion(&mut buf, &mut cursor, "build", true);
    assert_eq!(buf.iter().collect::<String>(), "/mode build tail");
    assert_eq!(cursor, 12);

    let mut buf: Vec<char> = "read @文档.txt".chars().collect();
    let mut cursor = 7;
    apply_completion(&mut buf, &mut cursor, "@文件/", true);
    assert_eq!(buf.iter().collect::<String>(), "read @文件/");
    assert_eq!(cursor, buf.len());
}

#[test]
fn json_tool_call_blocks_are_classified_as_protocol_artifacts() {
    let tool_json = r#"{
  "name": "start_server",
  "arguments": {
    "command": "npm start",
    "port": 3000
  }
}"#;
    assert!(is_tool_call_json_block("json", tool_json));
    assert!(!is_tool_call_json_block(
        "json",
        r#"{"message":"ordinary data"}"#
    ));
    assert!(!is_tool_call_json_block("rust", tool_json));
}

#[test]
fn raw_json_tool_calls_are_classified_as_protocol_artifacts() {
    let value: serde_json::Value = serde_json::from_str(
        r#"{"tool_calls":[{"function":{"name":"write_file","arguments":"{\"path\":\"x\"}"}}]}"#,
    )
    .unwrap();
    assert!(json_value_looks_like_tool_call(&value));
    assert!(starts_like_top_level_json(
        "  {\"name\":\"read_file\",\"arguments\":{}}"
    ));
    assert!(!starts_like_top_level_json(
        "Here is JSON: {\"name\":\"read_file\"}"
    ));
}

#[test]
fn maybe_json_buffer_has_short_lookahead_cap() {
    // Line cap: past 5 buffered lines the lookahead gives up and flushes.
    let lines = vec!["{".to_string(); 6];
    assert!(maybe_json_buffer_is_too_large(&lines));
    let lines = vec!["{".to_string(); 5];
    assert!(!maybe_json_buffer_is_too_large(&lines));
    // Byte cap: a single line over 2KB also flushes.
    let lines = vec!["x".repeat(2 * 1024 + 1)];
    assert!(maybe_json_buffer_is_too_large(&lines));
    let lines = vec!["{".to_string(), "\"message\":\"ordinary\"".to_string()];
    assert!(!maybe_json_buffer_is_too_large(&lines));
}

#[test]
fn osc8_hyperlinks_are_zero_width_for_strip_clip_and_wrap() {
    // OSC 8 link: ESC ] 8 ; ; URL ST label ESC ] 8 ; ; ST
    let link = "\x1b]8;;https://example.com/doc\x1b\\click me\x1b]8;;\x1b\\ tail";
    assert_eq!(strip_ansi(link), "click me tail");
    // The URL inside the OSC string must cost zero display columns.
    assert_eq!(strip_ansi(&clip_ansi_line(link, 8)), "click me");
    assert_eq!(
        wrap_ansi_line(link, 100)
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<String>(),
        "click me tail"
    );
    // BEL-terminated OSC variant too.
    let bel = "\x1b]8;;file:///tmp/a.png\x07shot\x1b]8;;\x07";
    assert_eq!(strip_ansi(bel), "shot");
}

#[test]
fn clip_ansi_line_counts_display_columns_not_chars() {
    // ASCII: unchanged behavior.
    assert_eq!(clip_ansi_line("abcdef", 4), "abcd");
    // CJK chars are 2 columns wide each.
    assert_eq!(clip_ansi_line("ab你cd", 4), "ab你");
    // A width-2 char is never split across the boundary.
    assert_eq!(clip_ansi_line("ab你cd", 3), "ab");
    assert_eq!(clip_ansi_line("你好世界", 5), "你好");
    // ANSI escapes cost zero columns.
    assert_eq!(strip_ansi(&clip_ansi_line("\x1b[31m你好\x1b[0m", 2)), "你");
    assert_eq!(clip_ansi_line("abc", 0), "");
}

#[test]
fn wrap_ansi_line_is_width_aware() {
    // ASCII: unchanged behavior.
    assert_eq!(wrap_ansi_line("abcd", 2), vec!["ab", "cd"]);
    // Four CJK chars = 8 columns → two rows of 2 chars at width 4.
    assert_eq!(wrap_ansi_line("你好世界", 4), vec!["你好", "世界"]);
    // Odd width: the next width-2 char wraps whole instead of splitting.
    assert_eq!(wrap_ansi_line("你好世界", 5), vec!["你好", "世界"]);
    assert_eq!(wrap_ansi_line("a你b", 2), vec!["a", "你", "b"]);
    // Degenerate width still terminates (one over-wide char per row).
    assert_eq!(wrap_ansi_line("你好", 1), vec!["你", "好"]);
    assert_eq!(wrap_ansi_line("", 4), vec![""]);
    assert_eq!(wrap_ansi_line("abc", 0), vec![""]);
}

#[test]
fn hist_match_filters_by_prefix_and_scans_both_ways() {
    let h: Vec<String> = ["cargo test", "git push", "cargo bench", "ls"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    // Backward from the end, unfiltered → newest entry.
    assert_eq!(hist_match(&h, "", None, true), Some(3));
    // Prefix filter: ↑ from a "cargo" draft skips "ls" and "git push".
    assert_eq!(hist_match(&h, "cargo", None, true), Some(2));
    assert_eq!(hist_match(&h, "cargo", Some(2), true), Some(0));
    assert_eq!(hist_match(&h, "cargo", Some(0), true), None);
    // Forward again (↓): back toward newer matches, then off the end.
    assert_eq!(hist_match(&h, "cargo", Some(0), false), Some(2));
    assert_eq!(hist_match(&h, "cargo", Some(2), false), None);
    assert_eq!(hist_match(&h, "zzz", None, true), None);
}

#[test]
fn word_span_at_selects_identifiers_and_symbol_runs() {
    let line = "let total_rows = t.wrapped.len();";
    // Inside an identifier → the whole identifier.
    assert_eq!(word_span_at(line, 6), Some((4, 13))); // total_rows
                                                      // On punctuation → the symbol run, not neighbours.
    assert_eq!(word_span_at(line, 15), Some((15, 15))); // '='
                                                        // On whitespace → nothing.
    assert_eq!(word_span_at(line, 3), None);
    // Out of range → nothing.
    assert_eq!(word_span_at(line, 999), None);
}

#[test]
fn history_search_finds_newest_first() {
    let h = vec![
        "git status".to_string(),
        "cargo test".to_string(),
        "git push".to_string(),
    ];
    assert_eq!(history_search(&h, "git", 0).as_deref(), Some("git push"));
    assert_eq!(history_search(&h, "git", 1).as_deref(), Some("git status"));
    assert_eq!(history_search(&h, "git", 2), None);
    assert_eq!(history_search(&h, "", 0), None);
    assert_eq!(
        history_search(&h, "cargo", 0).as_deref(),
        Some("cargo test")
    );
}

#[test]
fn token_at_grabs_trailing_token() {
    let b: Vec<char> = "go @src/ma".chars().collect();
    let (start, tok) = token_at(&b, b.len());
    assert_eq!((start, tok.as_str()), (3, "@src/ma"));
}

#[test]
fn common_prefix_works() {
    assert_eq!(common_prefix(&["/resume".into(), "/run".into()]), "/r");
    assert_eq!(common_prefix(&["abc".into()]), "abc");
    assert_eq!(common_prefix(&[]), "");
}

#[test]
fn completions_slash_only_at_line_start() {
    let b: Vec<char> = "/re".chars().collect();
    assert!(completions(&b, 0, "/re").contains(&"/resume".to_string()));
    let b2: Vec<char> = "do /re".chars().collect();
    assert!(completions(&b2, 3, "/re").is_empty());
    let b3: Vec<char> = "/scr".chars().collect();
    assert!(completions(&b3, 0, "/scr").contains(&"/scroll".to_string()));
}

#[test]
fn path_candidates_matches_prefix_and_marks_dirs() {
    use std::fs;
    let d = std::env::temp_dir().join(format!("bwn-comp-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("alpha.txt"), "").unwrap();
    fs::write(d.join("apple.txt"), "").unwrap();
    fs::create_dir_all(d.join("assets")).unwrap();
    fs::write(d.join("beta.txt"), "").unwrap();
    assert_eq!(
        path_candidates("a", &d),
        vec![
            "alpha.txt".to_string(),
            "apple.txt".to_string(),
            "assets/".to_string()
        ]
    );
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn completion_skips_names_with_escapes_or_bidi() {
    use std::fs;
    let d = std::env::temp_dir().join(format!("bwn-comp-evil-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("ok.txt"), "").unwrap();
    fs::write(d.join("o\x1b]0;pwned\x07.txt"), "").unwrap();
    fs::write(d.join("o\u{202E}txt.exe"), "").unwrap();
    let raw = path_candidates("o", &d);
    assert_eq!(raw.len(), 3);
    assert_eq!(drop_unsafe_candidates(raw), vec!["ok.txt".to_string()]);
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn no_color_strips_escapes() {
    assert_eq!(strip_ansi("\x1b[31mhi\x1b[0m"), "hi");
}

#[test]
fn mode_badge_contains_label() {
    let b = strip_ansi(&mode_badge("BUILD"));
    assert!(b.contains("BUILD"), "{b}");
    let p = strip_ansi(&mode_badge("PLAN"));
    assert!(p.contains("PLAN"), "{p}");
    let bs = strip_ansi(&mode_badge("BRAINSTORM"));
    assert!(bs.contains("BRAINSTORM"), "{bs}");
}

#[test]
fn test_context_meter_noop_when_zero() {
    super::context_meter(0, 0);
    super::context_meter(5000, 100000);
}

#[test]
fn test_vim_mode_toggle_and_state_label() {
    let initial = super::is_vim_mode();
    if initial {
        super::toggle_vim_mode();
    }
    assert_eq!(super::get_vim_state_label(), "");
    super::toggle_vim_mode();
    assert!(super::is_vim_mode());
    assert_eq!(super::get_vim_state_label(), "NORMAL");
    super::toggle_vim_mode();
    assert!(!super::is_vim_mode());
}

#[test]
fn test_path_candidates_semantic_prefixes() {
    use std::fs;
    let d = std::env::temp_dir().join(format!("bwn-sem-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    let cands = super::path_candidates("rules:bug", &d);
    assert!(!cands.is_empty());
    assert!(cands
        .iter()
        .any(|c| c.contains("bug_fix_requires_regression_test")));
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn test_end_word_helper() {
    let b: Vec<char> = "hello world foo".chars().collect();
    assert_eq!(super::end_word(&b, 0), 4);
    assert_eq!(super::end_word(&b, 4), 10);
    assert_eq!(super::end_word(&b, 10), 14);
}

#[test]
fn inline_md_unbalanced_markers_stay_literal() {
    // A single `*` (arithmetic), a lone backtick, and an unmatched `**`
    // must not style the rest of the line — they render verbatim.
    assert_eq!(format_inline_md("5 * 3 = 15"), "5 * 3 = 15");
    assert_eq!(format_inline_md("call `foo now"), "call `foo now");
    assert_eq!(format_inline_md("a ** b"), "a ** b");
}

#[test]
fn inline_md_balanced_markers_consume_delimiters() {
    // Balanced emphasis/code: the markers are consumed and the inner text
    // preserved (checked after stripping ANSI, so this holds in any color
    // mode).
    assert_eq!(strip_ansi(&format_inline_md("a *word* b")), "a word b");
    assert_eq!(strip_ansi(&format_inline_md("a **bold** c")), "a bold c");
    assert_eq!(
        strip_ansi(&format_inline_md("use `code` here")),
        "use code here"
    );
}

#[test]
fn render_md_dim_line_formats_markers_and_stays_muted() {
    // Markers are consumed — no raw ** or # or ` reaches the screen.
    assert_eq!(
        strip_ansi(&render_md_dim_line("a **bold** and `code`")),
        "a bold and code"
    );
    assert_eq!(strip_ansi(&render_md_dim_line("## a heading")), "a heading");
    assert_eq!(strip_ansi(&render_md_dim_line("- a bullet")), "• a bullet");
    assert_eq!(strip_ansi(&render_md_dim_line("> a quote")), "│ a quote");

    // Bold is emitted as an attribute toggle (1…22), and the foreground
    // never flips to the bright TEXT color mid-line — the whole line stays
    // in the muted thinking palette.
    let out = render_md_dim_line("plain **bold** plain");
    assert!(out.contains("\x1b[1m"), "bold attribute present: {out:?}");
    let text_fg = sgr_fg(pal().text);
    assert!(
        !out.contains(&text_fg),
        "dim line must never switch to bright TEXT fg: {out:?}"
    );

    // An unbalanced marker stays literal instead of eating the rest.
    assert_eq!(strip_ansi(&render_md_dim_line("2 * 3 = 6")), "2 * 3 = 6");
}

#[test]
fn render_md_draws_fenced_code_blocks() {
    let doc = "before\n```rust\nlet x = 1;\n```\nafter";
    let out = strip_ansi(&render_md(doc));
    assert!(out.contains("rust"), "{out}");
    assert!(out.contains("│ let x = 1;"), "{out}");
    assert!(out.contains("╭") && out.contains("╰"), "{out}");
    assert!(!out.contains("```"), "{out}");
    assert!(out.contains("before") && out.contains("after"), "{out}");
}

#[test]
fn render_md_closes_unterminated_fence() {
    let out = strip_ansi(&render_md("```py\nprint(1)"));
    assert!(out.contains("py"), "{out}");
    assert!(out.contains("│ print(1)"), "{out}");
    assert!(out.contains("╰"), "{out}");
    assert!(!out.contains("```"), "{out}");
}

#[test]
fn render_md_without_fences_matches_per_line_rendering() {
    let doc = "# Head\n- item\nplain";
    let expected: Vec<String> = doc.lines().map(render_md_line).collect();
    assert_eq!(render_md(doc), expected.join("\n"));
}

#[test]
fn stream_renderer_fence_has_label_and_no_raw_backticks() {
    let mut r = StreamRenderer::new();
    r.push("```python\nx = 1\ny = 2\n```\nafter\n");
    r.flush();
    let joined = strip_ansi(&r.sink.join("\n"));
    assert!(joined.contains("python"), "{joined}");
    assert!(
        joined.contains("│ x = 1") && joined.contains("│ y = 2"),
        "{joined}"
    );
    assert!(joined.contains("╭") && joined.contains("╰"), "{joined}");
    assert!(!joined.contains("```"), "{joined}");
    assert!(joined.contains("after"), "{joined}");
}

#[test]
fn sixel_placements_follow_the_visible_rows() {
    let m = |id: usize, r: usize| format!("{IMG_MARK}{id}:{r}");
    let rows = [
        "text".to_string(),
        m(3, 0),
        m(3, 1),
        m(3, 2),
        "after".to_string(),
    ];
    let visible: Vec<&String> = rows.iter().collect();
    // Whole image on screen.
    assert_eq!(sixel_placements(&visible, 5), vec![(3, 1, 0, 3)]);
    // Output area ends mid-image: only the rows that fit.
    assert_eq!(sixel_placements(&visible, 3), vec![(3, 1, 0, 2)]);
    // Scrolled so the image's first rows are above the screen.
    let top = [m(3, 2), m(3, 3), "after".to_string()];
    let visible: Vec<&String> = top.iter().collect();
    assert_eq!(sixel_placements(&visible, 3), vec![(3, 0, 2, 2)]);
    // A registered-image marker line is not a row placeholder.
    assert_eq!(sixel_row(&format!("{IMG_MARK}3")), None);
}

#[test]
fn sixel_rows_crop_to_the_visible_slice() {
    // 2 x 40 pixels at 10-pixel cells: 4 rows.
    let rgb = vec![200u8; 2 * 40 * 3];
    let mut img = InlineImage {
        path: std::path::PathBuf::new(),
        native: (2, 40),
        tier: ImageTier::Sixel,
        made_for: (0, 0, (0, 0)),
        rows: 4,
        blocks: Vec::new(),
        sixel: "FULL".into(),
        pixels: (2, 40, rgb),
        cell_h: 10,
        crop: None,
    };
    assert_eq!(img.sixel_rows(0, 4), "FULL");
    assert!(img.sixel_rows(1, 2).contains("\"1;1;2;20"));
    assert!(img.sixel_rows(3, 5).contains("\"1;1;2;10"));
    assert!(img.sixel_rows(9, 1).is_empty());
}

#[test]
fn image_budget_is_a_thumbnail_on_any_terminal_size() {
    // A wide, tall terminal caps at 80 columns by 16 rows.
    assert_eq!(image_cell_budget_for(240, 60, 4), (80, 16));
    // A standard 80x24 terminal gets half the width, a third of the rows.
    assert_eq!(image_cell_budget_for(80, 24, 4), (38, 5));
    // A tiny terminal still gets a readable minimum.
    assert_eq!(image_cell_budget_for(10, 8, 4), (8, 4));
}

#[test]
fn stream_renderer_commits_rendered_lines_at_any_chunk_split() {
    let doc = "# Title\nplain **bold** text\n```rs\nfn main() {}\n```\ntail\n";
    // Whole-document reference run.
    let mut whole = StreamRenderer::new();
    whole.push(doc);
    whole.flush();
    // Split at every char boundary; the committed transcript must not
    // depend on where the stream chunks happened to land.
    for cut in 1..doc.len() {
        if !doc.is_char_boundary(cut) {
            continue;
        }
        let mut split = StreamRenderer::new();
        split.push(&doc[..cut]);
        split.push(&doc[cut..]);
        split.flush();
        assert_eq!(split.sink, whole.sink, "split at byte {cut}");
    }
    let joined = strip_ansi(&whole.sink.join("\n"));
    assert!(
        joined.contains("Title") && !joined.contains("# Title"),
        "{joined}"
    );
    assert!(joined.contains("│ fn main() {}"), "{joined}");
    assert!(!joined.contains("```"), "{joined}");
    assert!(!joined.contains("**"), "{joined}");
}

#[test]
fn stream_renderer_flushes_partial_last_line_rendered() {
    let mut r = StreamRenderer::new();
    r.push("**no trailing newline**");
    r.flush();
    let joined = strip_ansi(&r.sink.join("\n"));
    assert!(joined.contains("no trailing newline"), "{joined}");
    assert!(!joined.contains("**"), "{joined}");
}

// The incremental wrap cache must always agree with wrapping every line
// from scratch — for pushes, appends, in-place edits, removals, front
// drains, and resizes.
fn assert_cache_coherent(t: &Transcript) {
    let expect: Vec<Vec<String>> = t.lines.iter().map(|l| wrap_ansi_line(l, t.width)).collect();
    assert_eq!(t.wrapped, expect);
}

#[test]
fn transcript_cache_tracks_mutations() {
    let mut t = Transcript::new();
    t.ensure_width(10);
    t.push("short".to_string());
    t.push("a line that definitely wraps at ten cols".to_string());
    assert_cache_coherent(&t);
    assert!(t.append_to(0, " and more appended text"));
    assert!(!t.append_to(99, "nope"));
    assert_cache_coherent(&t);
    t.set(1, "replaced".to_string());
    assert_cache_coherent(&t);
    t.push("third".to_string());
    t.remove(0);
    assert_cache_coherent(&t);
    t.drain_front(1);
    assert_cache_coherent(&t);
    t.ensure_width(4); // resize rewraps everything
    assert_cache_coherent(&t);
    assert_eq!(
        t.total_rows(),
        t.wrapped.iter().map(|w| w.len()).sum::<usize>()
    );
    t.clear();
    assert_eq!(t.total_rows(), 0);
}

#[test]
fn transcript_rows_range_matches_flattened_window() {
    let mut t = Transcript::new();
    t.ensure_width(6);
    for i in 0..10 {
        t.push(format!("line {i} with extra width to wrap"));
    }
    let flat: Vec<String> = t.wrapped.iter().flatten().cloned().collect();
    let total = t.total_rows();
    assert_eq!(total, flat.len());
    for start in [0usize, 1, 5, total.saturating_sub(3), total] {
        for count in [0usize, 1, 4, total] {
            let got: Vec<String> = t.rows_range(start, count).into_iter().cloned().collect();
            let want: Vec<String> = flat.iter().skip(start).take(count).cloned().collect();
            assert_eq!(got, want, "start={start} count={count}");
        }
    }
}

#[test]
fn eat_escape_handles_apc_and_dcs_strings() {
    // kitty APC and tmux DCS payloads must be invisible to width math.
    let apc = "\x1b_Ga=T,f=100,i=1;QUJDRA==\x1b\\x";
    assert_eq!(strip_ansi(apc), "x");
    assert_eq!(str_width(&strip_ansi(apc)), 1);
    let dcs = "\x1bPtmux;\x1b\x1b_Ga=d\x1b\x1b\\\x1b\\y";
    assert_eq!(strip_ansi(dcs), "y");
    // OSC still terminates at BEL.
    assert_eq!(strip_ansi("\x1b]8;;http://x\x07lbl\x1b]8;;\x07"), "lbl");
    // Placeholder cells copy as spaces; their diacritics vanish.
    let row = crate::graphics::placeholder_rows(3, 2, 1, 0).remove(0);
    assert_eq!(strip_ansi(&row), "  ");
    assert_eq!(char_width('\u{0483}'), 0);
    assert_eq!(char_width(crate::graphics::PLACEHOLDER), 1);
}

#[test]
fn wrap_carries_sgr_onto_continuation_rows() {
    let s = format!("{}abcdef", "\x1b[31m");
    let rows = wrap_ansi_line(&s, 3);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], "\x1b[31mabc");
    assert_eq!(rows[1], "\x1b[31mdef");
    // A reset clears the carried state.
    let s = "\x1b[31mab\x1b[0mcdef";
    let rows = wrap_ansi_line(s, 3);
    assert_eq!(rows[1], "def");
    // An image row wrapped in a narrow terminal keeps its id colour.
    let row = crate::graphics::placeholder_rows(9, 4, 1, 0).remove(0);
    let rows = wrap_ansi_line(&row, 2);
    assert_eq!(rows.len(), 2);
    assert!(rows[1].starts_with("\x1b[38;5;9m"));
}

#[test]
fn pipe_tables_render_aligned() {
    std::env::remove_var("NO_COLOR");
    let rows = vec![
        "| Name | Qty | Price |".to_string(),
        "|:-----|:---:|------:|".to_string(),
        "| apple | 3 | 1.50 |".to_string(),
        "| **kiwi** | 12 | 0.25 |".to_string(),
    ];
    let out = render_table(&rows, 80);
    assert_eq!(out.len(), 4); // header, rule, two rows
    let plain: Vec<String> = out.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain[0], "  Name  │ Qty │ Price");
    assert_eq!(plain[1], "  ──────┼─────┼──────");
    assert_eq!(plain[2], "  apple │  3  │  1.50");
    assert_eq!(plain[3], "  kiwi  │ 12  │  0.25");
    // Every row is exactly one terminal row.
    assert!(plain.iter().all(|l| str_width(l) == str_width(&plain[0])));
    // Too wide: the widest column is cut with an ellipsis, never wrapped.
    let wide = vec![
        "| a | b |".to_string(),
        "|---|---|".to_string(),
        format!("| {} | x |", "y".repeat(100)),
    ];
    let out = render_table(&wide, 40);
    assert!(out.iter().all(|l| str_width(&strip_ansi(l)) <= 40));
    assert!(strip_ansi(&out[2]).contains('…'));
    assert!(is_table_row("| a | b |"));
    assert!(!is_table_row("|"));
    assert!(!is_table_row("a | b"));
    assert_eq!(
        split_table_cells("| a \\| b | `c|d` |"),
        vec!["a | b", "`c|d`"]
    );
}

#[test]
fn stream_renderer_collects_tables() {
    std::env::remove_var("NO_COLOR");
    let mut r = StreamRenderer::new();
    r.push("| h1 | h2 |\n|--|--|\n| 1 | 2 |\nafter\n");
    r.flush();
    let plain: Vec<String> = r.sink.iter().map(|l| strip_ansi(l)).collect();
    // One emit for the whole table (three rows joined), then the line.
    assert_eq!(plain.len(), 2);
    assert!(plain[0].starts_with("  h1 │ h2\n"));
    assert_eq!(plain[1], "after");
}

#[test]
fn markdown_extras() {
    std::env::remove_var("NO_COLOR");
    assert!(strip_ansi(&render_md_line("---")).trim().starts_with('─'));
    assert!(strip_ansi(&render_md_line("* * *")).trim().starts_with('─'));
    assert_eq!(strip_ansi(&render_md_line("#### Deep")), "Deep");
    assert_eq!(strip_ansi(&render_md_line("- [ ] todo")), "  ☐ todo");
    assert_eq!(strip_ansi(&render_md_line("- [x] done")), "  ☑ done");
    let s = render_md_line("a ~~gone~~ b");
    assert!(s.contains("\x1b[9m"));
    assert_eq!(strip_ansi(&s), "a gone b");
    assert_eq!(
        strip_ansi(&render_md_line("~~ not closed")),
        "~~ not closed"
    );
}

#[test]
fn pasted_media_paths_become_tokens() {
    let dir = std::env::temp_dir().join(format!("bwn-paste-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("shot one.png");
    std::fs::write(&img, b"png").unwrap();
    let txt = dir.join("notes.txt");
    std::fs::write(&txt, b"t").unwrap();
    let p = img.display().to_string();
    assert_eq!(pasted_media_path(&p).as_deref(), Some(img.as_path()));
    assert_eq!(
        pasted_media_path(&format!("'{p}'")).as_deref(),
        Some(img.as_path())
    );
    assert_eq!(
        pasted_media_path(&format!("\"{p}\"\n")).as_deref(),
        Some(img.as_path())
    );
    assert_eq!(
        pasted_media_path(&format!("file://{p}")).as_deref(),
        Some(img.as_path())
    );
    assert_eq!(
        pasted_media_path(&p.replace(' ', "\\ ")).as_deref(),
        Some(img.as_path())
    );
    assert!(pasted_media_path(&txt.display().to_string()).is_none());
    assert!(pasted_media_path("just some words about a .png file").is_none());
    assert!(pasted_media_path(&format!("{p}\nsecond line")).is_none());
    assert!(pasted_media_path(&dir.join("missing.png").display().to_string()).is_none());
    // Token quoting: spaces get double quotes, plain paths don't.
    assert_eq!(attachment_token(&img), format!("@\"{p}\" "));
    let plain = std::path::Path::new("/tmp/a.png");
    assert_eq!(attachment_token(plain), "@/tmp/a.png ");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn image_preview_renders_half_blocks() {
    // 2x2 image: red/green over blue/white → one text row, two cells.
    let rgb = [255u8, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
    let rows = image_preview_cells(&rgb, 2, 2);
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r.matches('▀').count(), 2, "{r}");
    assert!(
        r.contains("38;2;255;0;0") && r.contains("48;2;0;0;255"),
        "{r}"
    );
    assert!(
        r.contains("38;2;0;255;0") && r.contains("48;2;255;255;255"),
        "{r}"
    );
    // Odd height: the last pixel row renders fg-only.
    let rgb3 = [10u8, 20, 30, 40, 50, 60, 70, 80, 90];
    let rows3 = image_preview_cells(&rgb3, 1, 3);
    assert_eq!(rows3.len(), 2);
    assert!(rows3[1].contains("38;2;70;80;90"), "{}", rows3[1]);
    // Malformed buffer → nothing.
    assert!(image_preview_cells(&[1, 2], 2, 2).is_empty());
    assert!(image_preview_cells(&rgb, 0, 2).is_empty());
}

#[test]
fn popup_window_keeps_selection_visible() {
    // Fewer candidates than the cap: show everything from the top.
    assert_eq!(popup_window(0, 3, 8), (0, 3));
    assert_eq!(popup_window(2, 3, 8), (0, 3));
    // More candidates than the cap: window follows the selection.
    assert_eq!(popup_window(0, 20, 8), (0, 8));
    assert_eq!(popup_window(7, 20, 8), (0, 8));
    assert_eq!(popup_window(10, 20, 8), (3, 8));
    assert_eq!(popup_window(19, 20, 8), (12, 8));
    // Empty list.
    assert_eq!(popup_window(0, 0, 8), (0, 0));
}

#[test]
fn every_builtin_slash_command_has_a_description() {
    for cmd in builtin_names() {
        assert!(
            !slash_command_desc(cmd).is_empty(),
            "missing popup description for {cmd}"
        );
    }
    // Completion lists that live elsewhere stay in step.
    let theme: Vec<&str> = THEME_NAMES.iter().copied().chain(["auto"]).collect();
    assert_eq!(find_command("/theme").unwrap().subs, theme);
    assert_eq!(
        find_command("/effort").unwrap().subs,
        crate::config::Effort::LEVELS
    );
    // No spelling is listed twice.
    let mut all: Vec<&str> = builtin_names().collect();
    let n = all.len();
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), n);
}

#[test]
fn popup_candidates_only_for_interesting_tokens() {
    // Ordinary prose never spawns a popup.
    let prose: Vec<char> = "fix the bug".chars().collect();
    assert!(popup_candidates(&prose, prose.len()).is_empty());
    // A line-start slash token does.
    let cmd: Vec<char> = "/re".chars().collect();
    assert!(popup_candidates(&cmd, cmd.len()).contains(&"/resume".to_string()));
    // Sub-arguments of a slash command do.
    let sub: Vec<char> = "/mode pl".chars().collect();
    assert_eq!(popup_candidates(&sub, sub.len()), vec!["plan".to_string()]);
    // A slash mid-message does not.
    let mid: Vec<char> = "see /etc".chars().collect();
    assert!(popup_candidates(&mid, mid.len()).is_empty());
    // Empty token: nothing to suggest.
    let blank: Vec<char> = "/help ".chars().collect();
    assert!(popup_candidates(&blank, blank.len()).is_empty());
}

#[test]
fn test_markdown_rendering_formatting() {
    let md = super::render_md_line("# Hello **world** *italic* `code`");
    let p = strip_ansi(&md);
    assert!(p.contains("Hello"), "{p}");
    assert!(p.contains("world"), "{p}");
    assert!(p.contains("italic"), "{p}");
    assert!(p.contains("code"), "{p}");

    let num_list = super::render_md_line("1. First item");
    let p_num = strip_ansi(&num_list);
    assert!(p_num.contains("1."), "{p_num}");
    assert!(p_num.contains("First item"), "{p_num}");

    let quote = super::render_md_line("> A blockquote");
    let p_quote = strip_ansi(&quote);
    assert!(p_quote.contains("│"), "{p_quote}");
    assert!(p_quote.contains("A blockquote"), "{p_quote}");

    let link = super::render_md_line("Click [Google](https://google.com) now");
    let p_link = strip_ansi(&link);
    assert!(p_link.contains("Google"), "{p_link}");
    assert!(p_link.contains("(https://google.com)"), "{p_link}");

    let bullet = super::render_md_line("- Bullet item");
    let p_bullet = strip_ansi(&bullet);
    assert!(p_bullet.contains("•"), "{p_bullet}");
    assert!(p_bullet.contains("Bullet item"), "{p_bullet}");
}

#[test]
fn queue_edit_and_remove_act_on_the_message_sent_next() {
    // ask_task sends index 0 next; Ctrl+Q / Ctrl+X must take that one,
    // and an edited (or abandoned) message goes back to the front.
    let mut mq = vec!["first".to_string(), "second".to_string()];
    assert_eq!(queue_take_next(&mut mq).as_deref(), Some("first"));
    assert_eq!(mq, vec!["second".to_string()]);
    queue_put_back(&mut mq, "first (edited)".to_string());
    assert_eq!(mq, vec!["first (edited)".to_string(), "second".to_string()]);
    assert_eq!(queue_take_next(&mut mq).as_deref(), Some("first (edited)"));
    assert_eq!(queue_take_next(&mut mq).as_deref(), Some("second"));
    assert_eq!(queue_take_next(&mut mq), None);
    // Only the front row advertises the keys.
    assert!(queued_row_hint(0).contains("Ctrl+Q edit"));
    assert!(queued_row_hint(0).contains("Ctrl+X rm"));
    assert_eq!(queued_row_hint(1), "");
}

#[cfg(unix)]
#[test]
fn clipboard_helper_is_fed_and_reaped() {
    // `cat` stands in for pbcopy/clip.exe: it exits once its stdin closes,
    // and the helper thread must wait for it (no zombie per copy).
    let h = pipe_to_clipboard_helper("cat", "copied").expect("spawn cat");
    let status = h.join().unwrap().expect("waited for the helper");
    assert!(status.success());
    assert!(pipe_to_clipboard_helper("definitely-not-a-clipboard-xyz", "x").is_none());
}

#[test]
fn osc52_confirmation_suppressed_only_for_known_unsupported_terminals() {
    // WSL/macOS copy through clip.exe/pbcopy, so the check is moot there.
    if crate::tools::is_wsl() || cfg!(target_os = "macos") {
        return;
    }
    // Known-unsupported: plain xterm / VTE without a TERM_PROGRAM.
    assert!(osc52_known_unsupported_for("xterm", None, false));
    assert!(osc52_known_unsupported_for("xterm-256color", None, true));
    assert!(osc52_known_unsupported_for("linux", None, false));
    // Unsure → keep the confirmation.
    assert!(!osc52_known_unsupported_for("xterm-256color", None, false));
    assert!(!osc52_known_unsupported_for("screen-256color", None, false));
    assert!(!osc52_known_unsupported_for("xterm-kitty", None, false));
    // A TERM_PROGRAM (iTerm2, WezTerm, vscode, tmux…) wins over TERM.
    assert!(!osc52_known_unsupported_for(
        "xterm",
        Some("iTerm.app"),
        false
    ));
    assert!(!osc52_known_unsupported_for(
        "xterm-256color",
        Some("WezTerm"),
        true
    ));
    assert!(osc52_known_unsupported_for("xterm", Some(""), false));
}

#[test]
fn popup_desc_covers_skills_and_custom_commands() {
    // Builtins keep their static text.
    assert_eq!(popup_desc("/help"), slash_command_desc("/help"));
    // `/mcp login` is how a server that wants OAuth is used at all.
    assert!(slash_command_desc("/mcp").contains("login"));
    assert_eq!(
        popup_desc("/local"),
        "probe local servers and list GGUF models"
    );
    // A bundled skill shows its first line via the skill loader.
    let (name, content) = crate::config::bundled_skills()[0];
    let desc = popup_desc(&format!("/{name}"));
    assert!(!desc.is_empty(), "skill /{name} needs a popup description");
    assert_eq!(desc, crate::config::skill_description(content));
    // @mentions and unknown commands stay blank.
    assert_eq!(popup_desc("@src/lib.rs"), "");
    assert_eq!(popup_desc("/no-such-command-xyz"), "");
}

#[test]
fn sanitize_terminal_neutralizes_escapes_but_keeps_newlines_and_tabs() {
    let evil = "hi\x1b]52;c;ZXZpbA==\x07 \x1b[2J\x1b[1;1H\u{9b}31m\u{85}x\x7f\x00\r\n\tend";
    let out = sanitize_terminal(evil);
    assert!(!out.contains('\x1b'));
    assert!(!out
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t'));
    assert_eq!(out, "hi␛]52;c;ZXZpbA== ␛[2J␛[1;1H31mx\n\tend");
    // No control chars: borrowed, no allocation on the streaming path.
    let clean = "plain ✓ text\n\twith ünïcode";
    assert!(matches!(
        sanitize_terminal(clean),
        std::borrow::Cow::Borrowed(_)
    ));
    // Idempotent, so sanitizing at several entry points is harmless.
    assert_eq!(sanitize_terminal(&out), out);
}

#[test]
fn sanitize_prompt_keeps_harness_colors_only() {
    // `ask`/`ask_task` prompts carry harness SGR plus text a model or
    // server can supply (the question tool's default, a model name).
    for (prompt, want) in [
        (
            "  Answer \x1b[38;5;103m[a\x1b]52;c;cm0gLXJmIH4=\x07]\x1b[39m: ",
            "  Answer \x1b[38;5;103m[a␛]52;c;cm0gLXJmIH4=]\x1b[39m: ",
        ),
        (
            "\x1b[38;2;1;2;3m[BUILD]\x1b[0m \x1b[1m›\x1b[22m ",
            "\x1b[38;2;1;2;3m[BUILD]\x1b[0m \x1b[1m›\x1b[22m ",
        ),
        ("  model [x\x1b[2J\x1b[1;1H]: ", "  model [x␛[2J␛[1;1H]: "),
        (
            "  model [\x1b]0;pwned\x1b\\x]: ",
            "  model [␛]0;pwned␛\\x]: ",
        ),
        (
            "  model [x\x1bPtmux;\x1b\x1b\\]: ",
            "  model [x␛Ptmux;␛␛\\]: ",
        ),
        ("  [\u{9b}31mx\u{202E}y\x1b]: ", "  [31mx<U+202E>y␛]: "),
        ("  [trailing\x1b", "  [trailing␛"),
        ("  [\x1bé\x1b[]: ", "  [␛é␛[]: "),
    ] {
        assert_eq!(sanitize_prompt(prompt), want, "{prompt:?}");
        assert_eq!(sanitize_prompt(want), want, "idempotent: {want:?}");
    }
}

#[test]
fn sanitize_terminal_marks_bidi_and_invisible_format_chars() {
    let evil = "a\u{202E}b\u{2066}c\u{200B}d\u{2028}e\u{FEFF}f\u{061C}g\u{200F}";
    let out = sanitize_terminal(evil);
    assert_eq!(
        out,
        "a<U+202E>b<U+2066>c<U+200B>d<U+2028>e<U+FEFF>f<U+061C>g<U+200F>"
    );
    assert_eq!(sanitize_terminal(&out), out);
    // Neighbours sharing a lead byte (Arabic, dashes, full-width forms)
    // are untouched and stay borrowed.
    for ok in ["\u{0627}\u{061B}", "\u{2014}\u{2026}\u{2070}", "\u{FF21}"] {
        assert!(matches!(
            sanitize_terminal(ok),
            std::borrow::Cow::Borrowed(_)
        ));
    }
}

#[test]
fn browse_items_neutralize_skill_and_mcp_text() {
    // /skills and /tools rows: a skill file's name and description, an
    // MCP tool's name and description.
    let items = vec![(
        "/evil\x1b]52;c;cm0gLXJmIH4=\x07".to_string(),
        "[project] fine\u{202E}txt\n\nbody \x1b[2J".to_string(),
    )];
    let safe = browse_safe(&items);
    assert_eq!(safe[0].0, "/evil␛]52;c;cm0gLXJmIH4=");
    assert_eq!(safe[0].1, "[project] fine<U+202E>txt\n\nbody ␛[2J");
}

#[test]
fn untrusted_markdown_keeps_harness_styling_only() {
    let md = "**bold** \x1b]52;c;AAAA\x07 \x1b[31mred\n\n| a | b |\n|---|---|\n| \x1b[2J | 2 |\n\n```rust\nlet x = \"\x1b[1A\";\n```";
    let out = render_md(md);
    for bad in ["\x1b]52", "\x1b[31m", "\x1b[2J", "\x1b[1A", "\x07"] {
        assert!(!out.contains(bad), "{bad:?} leaked: {out:?}");
    }
    let p = strip_ansi(&out);
    assert!(p.contains("␛]52;c;AAAA ␛[31mred"), "{p}");
    assert!(p.contains("␛[2J"), "{p}");
    assert!(p.contains("let x = \"␛[1A\";"), "{p}");
    if !no_color() {
        // Bold, table borders and the highlighted code block still style.
        assert!(out.contains('\x1b'));
    }
    let mut r = StreamRenderer::new();
    r.push("ok \x1b]52;c;QQ");
    r.push("==\x07\u{9d} done\n");
    r.flush();
    let joined = r.sink.join("\n");
    assert!(!joined.contains("\x1b]52") && !joined.contains('\x07'));
    assert!(strip_ansi(&joined).contains("ok ␛]52;c;QQ== done"));
}

#[test]
fn osc8_url_drops_control_chars() {
    let url = "https://example.test/\x1b\\\x1b]52;c;AA\x07x\u{9c}y\n";
    let clean = osc8_url(url);
    assert_eq!(clean, "https://example.test/\\]52;c;AAxy");
    assert!(!clean.chars().any(char::is_control));
}

#[test]
fn typeahead_keys_survive_an_interrupt_poll() {
    use crossterm::event::KeyEvent;
    let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
    let _ = take_typeahead();
    for c in "good point".chars() {
        assert_eq!(
            typeahead_event(key(KeyCode::Char(c)), true),
            InterruptKind::None
        );
    }
    // Esc while the agent works raises the interrupt without dropping
    // the keys buffered around it.
    assert_eq!(
        typeahead_event(key(KeyCode::Esc), true),
        InterruptKind::Escape
    );
    typeahead_event(key(KeyCode::Char('!')), true);
    typeahead_event(Event::Paste("\x1b[31m more\n".into()), true);
    let (buf, cursor) = take_typeahead();
    INTERRUPT_KIND_VAL.store(0, Ordering::Relaxed);
    // The paste keeps its line break; its escape is dropped.
    assert_eq!(buf.iter().collect::<String>(), "good point![31m more\n");
    assert_eq!(cursor, buf.len());
}

#[test]
fn restore_resets_every_terminal_mode() {
    let restore = std::str::from_utf8(RESTORE).unwrap();
    for seq in [
        "\x1b[r",
        "\x1b[0m",
        "\x1b[?25h",
        "\x1b[?1000l",
        "\x1b[?1002l",
        "\x1b[?1003l",
        "\x1b[?1015l",
        "\x1b[?1006l",
        "\x1b[?1004l",
        "\x1b[?2004l",
        "\x1b[0 q",
    ] {
        assert!(restore.contains(seq), "RESTORE lacks {seq:?}");
    }
    // Margins reset before leaving the alternate screen, which comes last.
    assert!(restore.ends_with("\x1b[?1049l"));
}

#[test]
fn the_composer_keeps_pasted_line_breaks_and_shows_them() {
    let got: Vec<char> = clean_paste("one\r\ntwo\rthree\tx\x1b[31m")
        .chars()
        .collect();
    assert_eq!(got.iter().collect::<String>(), "one\ntwo\nthree\tx[31m");
    // One column per character, so the cursor arithmetic holds.
    let shown = composer_glyphs(&got);
    assert_eq!(shown, "one↵two↵three x[31m");
    assert_eq!(
        str_width(&shown),
        got.iter().copied().map(char_width).sum::<usize>()
    );
}

#[test]
fn sanitize_paste_flattens_lines_and_drops_controls() {
    let got: String = sanitize_paste("a\r\nb\tc\x1b[31m\u{9b}d\x07\x7f ✓")
        .into_iter()
        .collect();
    assert_eq!(got, "a  b c[31md ✓");
    assert!(sanitize_paste("").is_empty());
}

#[test]
fn ask_secret_never_echoes_and_esc_or_ctrl_c_cancel() {
    use crossterm::event::KeyEvent;
    let key = |code, mods| Event::Key(KeyEvent::new(code, mods));
    let typed = |s: &str| -> Vec<Event> {
        s.chars()
            .map(|c| key(KeyCode::Char(c), KeyModifiers::NONE))
            .collect()
    };
    let secret = "sk-ant-api03-NEWCOMERfakekey0001-SECRETTAIL9";
    let run = |events: Vec<Event>| {
        let mut events = events.into_iter();
        let mut screen = String::new();
        let got = read_secret(
            || events.next(),
            |n| screen.push_str(&secret_frame("  ANTHROPIC_API_KEY: ", n, 200)),
        );
        (got, screen)
    };

    // Typed and pasted characters show only as dots; Enter returns the
    // key, and what stays on screen is the masked form.
    let mut evs = typed(&secret[..10]);
    evs.push(Event::Paste(secret[10..].into()));
    evs.push(key(KeyCode::Enter, KeyModifiers::NONE));
    let (got, screen) = run(evs);
    assert_eq!(got.as_deref(), Some(secret));
    for part in ["sk-ant", "NEWCOMER", "SECRETTAIL9", "AIL9"] {
        assert!(!screen.contains(part), "{part} echoed: {screen}");
    }
    assert!(screen.contains(&"•".repeat(secret.len())));
    assert_eq!(secret_echo(secret), "sk-a…AIL9");
    assert_eq!(secret_echo("  "), "");

    // Esc and Ctrl+C cancel even with text typed; Ctrl+D only on an
    // empty line. Backspace and Ctrl+U edit what was typed.
    let mut evs = typed("sk-partial");
    evs.push(key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(run(evs).0, None);
    let mut evs = typed("sk-partial");
    evs.push(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(run(evs).0, None);
    assert_eq!(
        run(vec![key(KeyCode::Char('d'), KeyModifiers::CONTROL)]).0,
        None
    );
    let mut evs = typed("abx");
    evs.push(key(KeyCode::Backspace, KeyModifiers::NONE));
    evs.push(key(KeyCode::Char('d'), KeyModifiers::CONTROL));
    evs.push(key(KeyCode::Char('c'), KeyModifiers::NONE));
    evs.push(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(run(evs).0.as_deref(), Some("abc"));
    let mut evs = typed("wrong");
    evs.push(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
    evs.extend(typed("ok"));
    evs.push(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(run(evs).0.as_deref(), Some("ok"));
    // The input closing (a read error) is a cancel, not an empty key.
    assert_eq!(run(typed("sk-")).0, None);

    // A paste of several lines is not glued into one key: its line
    // breaks become spaces, which the key question then refuses.
    let (got, _) = run(vec![
        Event::Paste("sk-first\nsk-second\n".into()),
        key(KeyCode::Enter, KeyModifiers::NONE),
    ]);
    assert_eq!(got.as_deref(), Some("sk-first sk-second "));

    // A long key never wraps the row: the dots stop at the width.
    let frame = secret_frame("  KEY: ", 500, 40);
    assert_eq!(frame.matches('•').count(), 40 - 7 - 1);
}

#[test]
fn only_web_and_mail_model_links_are_clickable() {
    assert!(model_link_clickable("https://example.com/x"));
    assert!(model_link_clickable("HTTP://example.com"));
    assert!(model_link_clickable("mailto:a@b.c"));
    for url in [
        "file:///etc/passwd",
        "ssh://host",
        "vscode://open?x",
        "javascript:alert(1)",
        "x-man-page://ls",
        "relative/path",
    ] {
        assert!(!model_link_clickable(url), "{url}");
    }
}
