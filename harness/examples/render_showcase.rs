// Prints a scripted reply through the production markdown pipeline — a
// syntax-highlighted code block, an aligned pipe table, an inline-image
// header and the working footer — as plain ANSI on stdout, so the docs site
// can show real renderer output (converted to HTML) without a screenshot.
//
//   cargo run --example render_showcase > showcase.ansi

use buildwithnexus::tui;

fn main() {
    let reply = "Here's why the overlap happens and the fix.\n\n\
The dropdown is positioned `absolute` inside a header that has no stacking context, \
so it paints above the toolbar. Two lines fix it:\n\n\
```css\n\
.site-header { position: relative; z-index: 20; }\n\
.menu-dropdown { z-index: 10; } /* below the header now */\n\
```\n\n\
| Element | Before | After |\n\
|:--------|:------:|------:|\n\
| `.site-header` | static | `z-index: 20` |\n\
| `.menu-dropdown` | `z-index: 9999` | `z-index: 10` |\n\
| toolbar overlap | **yes** | none |\n\n\
- [x] reproduce at 1280×720\n\
- [ ] add a Playwright check for the header stack\n\n\
```rust\n\
#[test]\n\
fn header_stays_on_top() {\n\
    let page = Page::open(\"/\")?; // 1280x720 viewport\n\
    assert_eq!(page.z_index(\".site-header\"), 20);\n\
}\n\
```";
    println!(
        "{} {} {}",
        tui::dim("⎘"),
        tui::underline("Screenshot 2026-09-23 at 14.02.11.png"),
        tui::dim("· 1280×720")
    );
    println!("[[IMAGE]]");
    println!(
        "{} {}",
        tui::accent("›"),
        "why does the dropdown overlap the header? @\"Screenshot 2026-09-23 at 14.02.11.png\""
    );
    println!();
    println!("{}", tui::render_md(reply));
    println!("[[FOOTER]]");
    println!(
        "{} {} {} {}",
        tui::dim("claude-sonnet-5"),
        tui::accent("⠹"),
        tui::bold("working"),
        tui::dim("· 4s · 212 tok/s · Esc to interrupt")
    );
}
