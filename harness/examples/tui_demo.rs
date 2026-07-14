// Drives the real TUI through a scripted code-editing session — used to
// capture authentic screenshots of the interface (run under a PTY and
// snapshot the terminal). Every line goes through the production rendering
// pipeline: banner, tool headers, the diff renderer, finish banner, composer.
//
//   cargo run --example tui_demo    (best under a 120x36 terminal)
//
// Waits on the composer at the end; Ctrl+C twice (or Ctrl+D) exits.

use buildwithnexus::{report, tui};

fn main() {
    let old = "fn page_bounds(total: usize, page: usize) -> (usize, usize) {\n    let start = page * PAGE_SIZE;\n    let end = start + PAGE_SIZE - 1;\n    (start, end.min(total - 1))\n}";
    let new = "fn page_bounds(total: usize, page: usize) -> (usize, usize) {\n    let start = page * PAGE_SIZE;\n    let end = start + PAGE_SIZE;\n    (start, end.min(total))\n}";

    tui::enter_alt(true);
    tui::set_permission_mode("ask");
    tui::show_banner(
        "anthropic",
        "claude-sonnet-4-5",
        "BUILD",
        "/home/garrett/acme-app",
    );
    tui::line("");
    tui::line(&format!(
        "{} {}",
        tui::accent("›"),
        "pagination drops the last item on every page — find it and fix it"
    ));
    tui::line("");
    tui::line(&tui::render_md(
        "Found it — `page_bounds` in `src/pagination.rs` is off by one: the `- 1` \
         on `end` excludes the final row because the range is already exclusive.",
    ));
    tui::line("");
    report::tool_call(
        "bash",
        "run: cargo test -p acme-core pagination",
        &serde_json::json!({"command": "cargo test -p acme-core pagination"}),
    );
    report::tool_result(
        "bash",
        "running 6 tests\ntest last_page_keeps_final_row ... FAILED\ntest result: FAILED. 5 passed; 1 failed",
        false,
    );
    tui::line("");
    report::diff("src/pagination.rs", old, new);
    tui::line("");
    report::tool_call(
        "bash",
        "run: cargo test -p acme-core pagination",
        &serde_json::json!({"command": "cargo test -p acme-core pagination"}),
    );
    report::tool_result(
        "bash",
        "running 6 tests\ntest result: ok. 6 passed; 0 failed",
        false,
    );
    report::finish("Fixed the off-by-one in `page_bounds` — `end` no longer trims the final row; all 6 pagination tests pass.");

    // Block on the real composer so the input box renders live.
    loop {
        match tui::ask_task(&format!("{} ", tui::accent("›"))) {
            None => break,
            Some(_) => {}
        }
    }
    tui::leave_alt();
}
