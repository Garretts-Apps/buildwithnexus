// Draws one reply through the production markdown pipeline on the alternate
// screen, at whatever size the terminal is. A capture harness opens a PTY at
// 80 and 120 columns and snapshots the frame.
//
//   cargo run -p buildwithnexus --example md_reply

use std::io::Read;

use buildwithnexus::tui;

fn main() {
    let _ = tui::set_theme("dark");
    tui::enter_alt(true);
    tui::set_permission_mode("ask");
    tui::line(&format!("{} show every markdown element", tui::accent("›")));
    tui::line("");
    let reply = include_str!("../tests/fixtures/markdown/sample.md");
    tui::line(&tui::render_md(reply));
    let mut byte = [0u8; 1];
    let _ = std::io::stdin().read(&mut byte);
    tui::leave_alt();
}
