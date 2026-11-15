# Anti-patterns: what coding-tool screens converge on

This note is a list of looks to refuse. The products named here are
anti-references. Nothing in this file is a layout, palette, or glyph to
borrow. Facts are here so a later design can say how it differs. They are
not a menu.

## Why the screens converge

A model asked for "a modern dark UI" returns the middle of its training
set. The middle, counted rather than guessed, is a dark page, a gradient,
and a row of icon cards.

Adrian Krebs scored 1,590 Show HN landing pages against sixteen patterns
designers had already named as tells. The most common was a permanently
dark theme (34%), then gradient backgrounds (27%), then icon-card grids
(22%). The write-up is [AI Design Slop](https://ghulamqadir.me/blog/ai-design-slop).
A later pass on 500 Show HN posts, reported by
[Gigazine](https://gigazine.net/gsc_news/en/20260423-design-slop/), found
gradients on 28%, an untouched shadcn/ui theme on 23.5%, a centered title
on 23.5%, and a dark page with grey text and uppercase labels on 20.3%.

Adam Wathan, quoted in that first essay, apologized for shipping Tailwind
UI's buttons as `bg-indigo-500`, "leading to every AI generated UI on
earth also being indigo." One library default became a high-probability
token. Sascha Becker's
[Same Same but Different](https://saschb2b.com/blog/same-same-but-different)
calls the result a statistical fingerprint, recognizable the way a stock
photo is recognizable.

[Attuna](https://attuna.design/blog/why-ai-generated-ui-looks-generic)
lists the same center: a purple-to-blue gradient, one accent used
everywhere, grey secondary text, a centered hero, identical rounded cards,
and even spacing so nothing groups. [Dripatch](https://dripatch.com/purple-ui-problem/)
names the purple-or-blue accent, the left rail, and the rounded card grid
as the Purple UI Problem, and notes that shadcn, Radix, and Material
arrive with radius, shadow, and spacing already chosen.
[PixilUI](https://pixilui.com/guides/why-ai-built-apps-look-the-same)
adds Inter or Geist, a badge above the headline, glass panels, and emoji
in the navigation. Laith Junaidy read the 150 most-starred `shadcn-ui`
repositories on 25 September 2026
([We read 150 shadcn/ui repos](https://laithjunaidy.com/writing/taken-apart-shadcn-defaults.md)):
59 of 67 readable typefaces were Geist or Inter, and 65 of 87 readable
primaries were neutral grey. The shadcn changelog of December 2025 says
the defaults were "a little too good" and that apps started looking the
same ([npx shadcn create](https://ui.shadcn.com/docs/changelog/2025-12-shadcn-create)).
James Midzi compares that to the Bootstrap years
([Why I Avoid shadcn/ui](https://dantedecodes.vercel.app/articles/why-i-avoid-shadcn/)).

None of these patterns is a crime on its own. They read as generic
because they were not chosen.

## Patterns, and why each one reads as generic

**Purple-to-blue gradient.** The accent everyone reaches for when no one
named a color. It says "AI product" before the name is read. Sources:
Ghulam Qadir, Dripatch, Gigazine.

**Indigo buttons.** Tailwind UI's old `bg-indigo-500`, now a reflex.
Source: Wathan, via Ghulam Qadir.

**Grey on near-black, forever.** A dark page with medium-grey body text.
T3 Code's own terminal fallback is `rgb(14, 18, 24)` behind
`rgb(237, 241, 247)`, a cool grey pair
([ThreadTerminalDrawer.tsx](https://github.com/pingdotgg/t3code/blob/78f462c4/apps/web/src/components/ThreadTerminalDrawer.tsx)).
Cool grey on blue-black is the Vercel/shadcn dark surface. It has no
temperature, so it belongs to nobody.

**Rounded cards, identical, in a row of three.** The component the model
can finish without a decision. Sources: Attuna, PixilUI, Dripatch.

**Centered column.** The chat transcript sat in the middle with empty
sides, or a hero headline centered over a gradient. Gigazine counted
centered titles on 23.5% of the sample.

**Sparkle glyphs and "thinking…" spinners.** Claude Code's thinking
spinner cycles `· ✢ ✳ ✶ ✻ ✽`. Gemini CLI runs a gradient brand spinner
at 33 frames a second. The motion means "a model is alive," not "this
measurement changed." Catalogued in
[awesome-tui-design](https://github.com/cola-runner/awesome-tui-design).

**Emoji as status.** A sparkle for a write, a bolt for a shell, a colored
dot for a mode. PixilUI and Gigazine both count emoji in navigation as a
tell. The glyph is decoration; it does not name the event.

**Dashed or rounded input frames.** Claude Code draws the composer with
dashed ASCII borders. A rounded box around the prompt is the same card,
shrunk. It frames the typing instead of stating whose turn it is.

**ASCII-art logo banners.** Codex CLI ships ten variants of a 36-frame
startup drawing (awesome-tui-design). The banner is a poster. It is not
information, and it burns rows the session needs.

**One "model · cwd · mode" sentence.** Coding CLIs announce themselves
with the same three facts in the same order, joined by middots, then a
dim line of every key. The sentence is a template. A reader cannot scan
it as fields.

**Dim hint footers that list every key.** The line is longer than the
terminal, so it ends mid-word with an ellipsis. The hint is then neither
complete nor quiet.

**Whimsical tool colors.** Claude Code uses terracotta `#D77757` as the
accent and hot pink `#FD5DB1` on tool blocks (awesome-tui-design). The
pink does not mean "this is a tool." It means "this product." Copying it
would make bwn look like that product.

**Cool-blue cursor and selection.** T3 Code's dark terminal cursor is
`rgb(180, 203, 255)` and the selection is that blue at 25% opacity
(ThreadTerminalDrawer.tsx). A blue cursor on a blue-black field is the
current default "calm IDE."

**Message bubbles.** OpenCode is cited by Claude Code users as the richer
TUI because it draws bubbles and a sidebar of tokens and cost
([claude-code#52518](https://github.com/anthropics/claude-code/issues/52518)).
Bubbles are a chat app. A coding session is a log of actions.

**Cosine shimmer and gradient borders.** Codex CLI's shimmer and Gemini
CLI's semantic gradient borders (awesome-tui-design) move when nothing
in the data moved. Motion without a change is the spinner problem again.

**Inter, Geist, and one weight.** Laith Junaidy's count: 88% of readable
shadcn typefaces were Geist or Inter. A terminal cannot ship those
faces, but it can imitate their even, low-contrast grey. Hierarchy then
disappears.

## T3 Code

Official product: [t3.codes](https://t3.codes/). Official repository:
[pingdotgg/t3code](https://github.com/pingdotgg/t3code). The screenshot
on the repository front page is linked, not copied:
[hero.webp](https://github.com/pingdotgg/t3code/blob/main/.github/readme/hero.webp)
(desktop thread beside a phone approval).

**Layout.** A server plus desktop, web, and phone clients. The hero is
two surfaces at once. The review UI is a diff pane, a commit list, and
pull-request lists
([source control](https://github.com/pingdotgg/t3code/blob/main/docs/user/source-control.md)).
bwn's page is one terminal. Tree, log, and diff are columns, and under
120 columns only one column is showing.

**Density.** T3 Code spends the width on panes, lists, and a command
palette (`Cmd/Ctrl+K` for a new project or a clone). bwn spends the
width on the log. A diff row is line numbers and a sign. A tool event
is a three-letter stamp.

**Diff and tree.** T3 Code gives each thread its own worktree, and a
turn's file list omits files that arrived only because the turn pulled,
merged, or rebased. That isolation is a real property of the product.
bwn diffs the file you opened, against the session baseline or git
HEAD, inside the column. It does not grow a second app to hold the
tree.

**Agent visibility.** The README's promise is every agent in one thread,
including handing a task from one to another. The server starts those
CLIs as subprocesses
([AGENTS.md](https://github.com/pingdotgg/t3code/blob/main/AGENTS.md)).
bwn is the agent. The stamp on the log is the tool call. F12 is the
same process, and it records nothing while it is hidden.

**What it does well.** It drives the subscription you already have. It
keeps a thread's dirty tree off the next thread. It refuses to present
a merge's files as the turn's own edit. The README says a dropped frame
is a bug. Those are operating rules, and they do not require this
terminal to look like that application.

**What is generic.** The terminal fallback is cool blue-black
`rgb(14, 18, 24)`, cool text `rgb(237, 241, 247)`, and a blue cursor
`rgb(180, 203, 255)`
([ThreadTerminalDrawer.tsx](https://github.com/pingdotgg/t3code/blob/main/apps/web/src/components/ThreadTerminalDrawer.tsx)).
The default sans stack names `"Inter Variable"` first
([appearanceFonts.ts](https://github.com/pingdotgg/t3code/blob/main/apps/web/src/appearanceFonts.ts)).
A command palette over rounded panes is the desktop kit described in
the essays above. bwn does not take that kit.

Numbers for startup, memory, and install size are in
[comparison.md](comparison.md). They are not a palette.

## What "do not copy" means here

T3 Code, Claude Code, Codex, OpenCode, Warp, Cursor, and Gemini CLI are
anti-references. Studying them is for the distinctness check in
`direction.md`. Their layouts, palettes, color assignments, glyph sets,
spinners, headers, footers, and interaction patterns are not a palette
to sample.

Older non-AI screens (instrument panels, Swiss grids, Teletext,
mainframe fields, dense market terminals, file managers, tracker
software) are raw material for a synthesis. Shipping a screen a person
could name as one of those products is the same failure as shipping a
Claude Code lookalike.
