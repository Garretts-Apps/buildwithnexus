---
name: distinctive-design
description: Replace a generic generated look with one committed visual tradition, then check it on a real screen for contrast and accessibility.
---

# Distinctive design

Use this when building or restyling a web page, an application view, or a terminal screen. The goal is one look that a person could name, not a blend of familiar generated defaults.

A later note may attach researched pictures and citations. Until that note exists, follow only the rules in this file. Describe a tradition by what you can see: grid, type, color, density, and motion. Do not credit a living author, a studio, or a company as the source of the look.

## 1. Name the defaults you will not ship

Before choosing a direction, list the patterns a generated interface falls into. Mark the ones this screen would hit if you started drawing now.

- A purple or blue gradient used as decoration, often with a glow.
- Sparkle, star, or wand marks that do not name a status.
- Rounded cards with soft shadows stacked until every region looks the same.
- One narrow column centered on a wide page, laid out like a chat transcript.
- Gray text on near-black, with a single bright accent.
- Emoji used as the only status, the only icon, or the only heading mark.
- The default look of a popular component kit: same radius, same gray scale, same button, same sidebar.
- In a terminal: a title bar, a box-drawing frame around every panel, and a help line, with no other structure.

Add a pattern when you see it in the current draft. Do not add decoration to "fix" an item on this list.

## 2. Pick one tradition

Choose one reference tradition and write a single sentence about the job it does (reading fast, comparing numbers, filling a form, watching a status). Use one of these, or a tradition you can describe with the same kind of rules. Do not mix two.

- Instrument panel. Labels are short. Position and color carry status. Almost no marketing copy.
- Swiss grid. A strict column grid, one type family, wide margins, no shadows, no ornaments.
- Teletext. A coarse cell grid, a handful of flat colors, chunky type, no photographs.
- 3270 form. Fixed rows and columns, labeled fields, actions named like function keys, green or amber on a dark field.
- Tracker. Columns of events, dense rows, keyboard-first, color used to separate channels.
- Market terminal. Many small figures, tight tables, color only for direction or alarm, almost no empty hero space.

Say which list items from section 1 this choice makes impossible.

## 3. Commit the concept before coding

Write a short concept the implementation is not allowed to drift from.

- Palette. Exact colors. On the web, hex values. In a terminal, the palette indexes or named colors you will actually use. Name the background, the body text, one accent, and the alarm color. No extra accents.
- Type. One family. The sizes you will use, and which size is body text. In a terminal, name the width and whether the face is the terminal's normal face.
- Grid. Column count, row height or baseline, and the gap. Say what is allowed to span more than one column.
- Motion. What may move, for how long, and what must never move. If the answer is nothing, write that.

Stop and show this concept if the person asked to approve the direction first. Otherwise keep it in the reply and then build it.

## 4. Implement only that concept

Build the screen so a reviewer can match every color, size, and gap to the concept. Do not add a second tradition "for polish." Do not import a component kit and then recolor it unless the kit is stripped to the grid and type you committed to.

Web and terminal both count. A web view uses ordinary layout and type. A terminal view uses the cell grid, and it may not pretend to be a web page made of boxes.

## 5. Verify on a real screen

Do not judge the work from a description.

- Capture the screen you shipped. On the web, a screenshot of the running page. In a terminal, a snapshot of the real program, not a mock drawn in a browser.
- Check contrast. Body text against its background should reach 4.5:1. Large text and meaningful UI boundaries should reach 3:1. Record the pairs you measured and the ratios.
- Check access. On the web: accessible names, visible focus, and keyboard reach for every control. In a terminal: every status that uses color also uses a label or a position a person can name. Do not use color as the only signal.
- Compare the screenshot to section 1. If it still reads as one of those defaults, change the concept and rebuild. Do not cover it with more decoration.

The pass is done when the concept, the screenshot, the contrast pairs, and the access notes are all in the reply.