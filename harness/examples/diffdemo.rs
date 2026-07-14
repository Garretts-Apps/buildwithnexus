// Visual smoke check for the diff renderer: `cargo run --example diffdemo`.
// Prints a painted diff (gutter, tinted rows, word-level emphasis) to eyeball
// layout changes without driving the full TUI.
// Visual smoke: print a real painted diff to check the layout.
fn main() {
    let old = "fn greet(name: &str) {\n    println!(\"Hello, {}!\", name);\n    let count = 1;\n    for _ in 0..count {\n        wave();\n    }\n}\n\nfn wave() {}\n";
    let new = "fn greet(name: &str) {\n    println!(\"Hello, {}!\", name);\n    let count = 3;\n    for _ in 0..count {\n        wave();\n    }\n    celebrate();\n}\n\nfn wave() {}\n";
    println!("{}", buildwithnexus::report::render_diff_block(old, new));
}
