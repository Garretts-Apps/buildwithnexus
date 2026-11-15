# Direction: Notch

The screen is a page with a margin, not a chat window and not a cockpit.
The system is named **Notch**. A notch is the still mark at the left of
the active row. It does not animate. If the mark is present, that row is
the one the next key will change.

This is an original assembly. The historical material in the anti-pattern
note (a margin line, a fixed column of fields, weight instead of a box)
was melted down. The result is not a file manager, not a market
terminal, not a teletext page, and not an instrument cluster. Those
products remain recognizable as themselves. Notch should not be.

## Palette, and how it was derived

Constraints, in order, before any hex was picked:

1. No hue from 200° to 280°. That sector is the indigo, blue, and violet
   center of generated UI.
2. No field that is a cool near-black (`#0e1218` and its neighbors). That
   is the current "IDE dark."
3. The accent is not terracotta `#D77757`, not hot pink `#FD5DB1`, not
   cursor-blue `#B4CBFF`, not the cyan `#7EC8E3` people reach for when
   they want a CLI to feel "modern."
4. The accent is not a green phosphor and not an amber phosphor. A screen
   that is mostly one of those two is a costume.
5. Body text at least 7:1 on the field. Every other text role at least
   4.5:1 (WCAG AA). The ratios below are the WCAG contrast of the hex
   against the field, computed with the sRGB luminance formula.

The field is a warm brown-black, hue 28°, saturation 22%, lightness 9%.
That is `#1c1712`. It is 16 RGB units from `#0e1218` and 20 from
`#1a1b26`, which is enough that it is not those colors, and it is warm,
so grey-on-blue-black does not apply.

| Role | Hex | On `#1c1712` | Use |
|---|---|---:|---|
| paper | `#f1eadf` | 14.9:1 | body text |
| quiet | `#aa9e92` | 6.8:1 | secondary text |
| mark | `#44a793` | 6.1:1 | the notch, additions, "this is live" |
| wait | `#77894d` | 4.6:1 | a held decision, a modified file |
| cut | `#d26656` | 4.9:1 | a deletion, an error |

`#44a793` is hue 168° at moderate saturation. Distance in RGB to
`#D77757` is 166, to `#7EC8E3` is 104, to `#FD5DB1` is 201. It is a dull
teal used for the notch, for stamps, and for added lines. It is not a
gradient and it is not painted behind large surfaces.

Code needs three inks or a keyword, a string, and a type collapse into
one cell. Those inks are shifts of mark and wait, not new chrome:

| Ink | Dark | On the field | Light | Job |
|---|---|---:|---|---|
| bronze | `#b08a4e` | 5.6:1 | `#6a4814` | strings |
| sage | `#6e8f6a` | 4.9:1 | `#3d5228` | types |

Both stay outside hue 200–280°. Bronze is not an amber phosphor
(`#ffb000` is 148 RGB units away). Sage is not the wait olive. The
startup page, the footer, and the stamps do not use them.

Ink on the mark (`#1c1712` on `#44a793`) is also 6.1:1, so a reversed
notch stays readable. Reversal is reserved for the notch and the focused
column's title word. It is not the selection of a whole screen.

Light pages (the `light` theme, drawn on the terminal's own background)
use the same hues darkened so they clear 4.5:1 on white `#ffffff`,
Solarized Light `#fdf6e3`, `#eeeeee`, and the light diff tints:

| Role | Hex |
|---|---|
| paper / text | `#281e15` |
| quiet | `#675c51` |
| mark | `#1e6758` |
| wait | `#4d5d28` |
| cut | `#8b2818` |
| page tint, not painted as a full field | `#f7f1e6` |
| added-row tint | `#e7f0ea` |
| removed-row tint | `#f6e8e4` |
| stronger added tint | `#d5e6db` |
| stronger removed tint | `#f0d4cc` |
| selection tint | `#e4ddd2` |

### 256-color and 16-color

When the terminal is not truecolor, the existing cube mapping sends each
hex to the nearest 6×6×6 index (`16 + 36·r + 6·g + b`, each channel
quantized to 0–5). That is a projection of this palette, not a second
design.

The `ansi` theme does not pretend the terminal's 16 colors are these
hexes. Roles map to the terminal's own slots, which the user may have
recolored:

| Role | Slot |
|---|---|
| text | default |
| mark, additions | 6 (the terminal's cyan slot) |
| cut, errors | 1 |
| wait, warnings | 3 |
| quiet | 8, bright black |

If a user's 16-color palette is extreme, quiet text can miss 4.5:1.
That is the terminal's palette, not a second Notch palette painted over
it.

### NO_COLOR

`NO_COLOR` or `TERM=dumb` drops hue. Weight, underline, and the notch
character remain, because they are structure. A selected row is the row
with the notch and an underline, still readable in a monochrome dump.

## Type rhythm

A terminal has one face. Hierarchy is weight, underline, and the notch.

- A column title is one word, underlined. The focused column's title is
  also bold. Unfocused titles are quiet.
- The active row carries `⌞` in column 0 and is bold. No other row does.
- Secondary facts are quiet. They are not a sentence of middots.
- A full-width rule is a hairline of `─` used at most once, under the
  name on the startup page, 16 cells long. It does not span the
  terminal, so it cannot wrap into a stack of rules.
- Panes are not boxes. Between columns there is a single column of `·`,
  the margin. The margin is how the eye separates tree, page, and diff.

## Grid

Columns are fixed, not centered.

- Narrow (under 120): one column. The column title says which one (tree,
  page, or diff). F6 changes which column is the page.
- Wide (120 and up): three columns. Tree, then the margin, then the
  page, then the margin, then the diff. Widths are integers. The page
  keeps at least 20 columns.

The footer is one instrument line of fields, not a paragraph:

```
ASK   model-name   DIR last-segment   CTX 12%
```

While work is in flight the first field is `WORK` and a seconds count.
There is no spinner. The field changes when the state changes. That is
the motion rule: if the data did not change, the cell does not change.

Hints are a second line only when a column is focused, and only as a
whole phrase that fits. The phrases, longest first, are chosen in code.
If the next phrase does not fit, the shorter one is used whole. A phrase
is never cut mid-word and never ended with an ellipsis.

## Information hierarchy

1. The notch row. What the next key does.
2. The page. The conversation, or the diff when that column is the page.
3. The margins. Tree on the left, diff on the right, titles only as loud
   as their focus.
4. The instrument line. Mode, model, directory, context. Scannable
   fields.
5. Quiet counts. Tokens, cost, frame time. Same numbers in the strip and
   in the debug section, read from one function.

Tool events are three-letter stamps in the mark color, then the words:
`RD`, `WR`, `SH`, `GO`. Not a picture of the action.

Diff rows are a listing. A gutter of the old and new line numbers, then
`+` in the mark color or `-` in the cut color. The column title is the
path and the counts: `src/main.rs  +2 -3`. A hunk header is quiet and
underlined. Side-by-side is the same listing twice, used only when the
diff column is at least 72 cells (`v` toggles it).

The frame graph is a sparkline of `▁▂▃▄▅▆▇█`, drawn from the same sample
buffer as the number beside it. If the buffer is empty, both say `0`
and the sparkline is blank. They cannot disagree.

## Iconography

| Mark | Meaning |
|---|---|
| `⌞` | the active row. Still. |
| `·` | the margin between columns |
| `+` / `-` | added or removed line |
| `M` `A` `D` `R` | git status, in wait / mark / cut / wait |
| `RD` `WR` `SH` `GO` | read, write, shell, delegate |

No sparkle, no braille frame, no emoji, no ASCII-art wordmark. The name
on the startup page is `BUILDWITHNEXUS` in the paper color, one weight,
with the 16-cell hairline under it.

## Distinctness check

For each anti-reference: layout, color, weight, motion. A screen that
could be mistaken for that product has to be redone.

| Tool | Layout | Color | Weight | Motion |
|---|---|---|---|---|
| Claude Code | They keep the main screen and draw a dashed composer. Notch is an alternate screen whose composer is one hairline, not a dashed card. | They use terracotta `#D77757` and pink `#FD5DB1`. Notch's only accent is `#44a793`, 166 RGB units from the terracotta. | They cycle ornament glyphs. Notch uses bold, underline, and one still notch. | Their thinking spinner animates six ornaments. Notch's state is the word `WORK` and a seconds count. |
| Codex CLI | They draw a large startup plate and a shimmer. Notch's startup is four labeled fields and a 16-cell rule. | They are monochrome, or a suggested cyan `#7EC8E3`. The mark is 104 RGB units from that cyan and is not the field color. | Their plate is outline art. Notch has no plate. | Their shimmer moves on idle. Notch is still until a number changes. |
| Gemini CLI | They theme borders by tool state and spin a gradient mark at 33 fps. Notch has no per-tool border color and no gradient. | Gradient brand colors. Notch is five flat roles. | Borders carry the state. Notch's state is a word. | 33 fps spinner. Notch does not spin. |
| OpenCode | Sidebar plus message bubbles. Notch's margins are a column of `·` and a title, and the page is a log, not bubbles. | Product theme, not published as these hexes. | Bubbles separate turns. Notch separates turns with a stamp (`RD`, `WR`) in the mark color. | Streaming is their animation. Notch coalesces frames and does not add a throb. |
| Warp | Block UI, prompts as cards. Notch is one page and a margin. | Their blocks use a modern dark grey. Field is `#1c1712`, warm. | Blocks have fill. Notch's columns have no fill. | Block transitions. Notch has none. |
| Cursor | Chat column beside the editor, cool dark UI. Notch does not center a chat and does not use a blue-violet accent. | The product sits in the indigo family Notch's first constraint forbids. | Editor chrome. Notch has no editor chrome. | Inline AI decorations. Notch does not decorate a token. |
| T3 Code | Cool field `#0e1218`, cool text `#edf1f7`, blue cursor `#b4cbff`, Inter on Windows. | Field is 16 RGB units from `#0e1218` and warm, not cool. Text is `#f1eadf`, not `#edf1f7`. No blue cursor: the cursor is the mark `#44a793`. | They use a radius scale and 1.75px icons. Notch has no radius and no icons. | Hover brightening. A terminal row does not hover. |

The comparison table of timings in the performance review is a list of
numbers. It does not pick colors.

## What would fail this check

- A gradient anywhere on the startup page.
- A spinner of more than one frame.
- A rounded or dashed box around the prompt.
- A sentence of the form "model · directory · mode" as the banner.
- A hint line that ends with `…` because a word was cut.
- Purple, indigo, hot pink, or terracotta as the accent.
- A frame-time number in the strip that is not the number in the debug section.
