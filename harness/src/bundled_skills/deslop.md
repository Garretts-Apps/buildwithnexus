---
name: deslop
description: Sweep a repository for generated-code artifacts, report them by severity, and fix them in small commits.
---

# Deslop

Use this to clean a whole repository after generated edits. The job is to remove leftovers that do not change the program's behavior, and to leave anything that might be intentional.

## 1. Scope and mode

The scope is the path the person named, or the whole working tree when they named none.

- `--report`, `report`, or `dry-run`: scan, print the report, and stop. Do not edit and do not commit.
- `--ci`: run the Unicode scanner with `--ci` and stop. Do not edit. A failing exit is the result.
- Any other scope: scan, report, then fix.

## 2. Unicode scanner

The scanner ships with bwn at `harness/bundled_workflow/scripts/scan_slop.py` in a source checkout. Copy it to `~/.buildwithnexus/scripts/scan_slop.py` if you run outside a checkout.

```
python3 harness/bundled_workflow/scripts/scan_slop.py --report
python3 harness/bundled_workflow/scripts/scan_slop.py --ci
```

Each hit is `file:line:U+XXXX`. `--ci` fails only on invisible Unicode (U+200B-U+200D, U+2060, stray BOM, bidi controls, tag characters U+E0000-U+E007F). Smart quotes, dashes, NBSP, and line separators are reported in `--report` and do not fail `--ci`.

## 3. Report ranks

Blocker: invisible or bidi characters in code, swallowed errors, debug prints, placeholder data on shipped paths, imports the build rejects.

Should-fix: narrating comments, commented-out code, TODOs with no owner, stock doc phrasing, stray `.bak`/`.tmp`, duplicate tool configs.

Note: smart punctuation in prose, long but straight functions, exports you cannot prove unused.

End with a Kept list. In `--report` and `--ci`, stop after the report.

## 4. Fix order

When edits are allowed: unicode, comments, repo artifacts, formatting, writing, code smells, design. One commit per category with subject `deslop: <category>`. Run the project's tests after each commit and quote the result. Do not amend or push.

When the tree has UI, read `distinctive-design` and report generated-default patterns. Fix design only in a design commit for the screen in scope.

For the full checklist (comments, writing, formatting, code smells), follow the same rules as the public workflow kit's `deslop` skill body.