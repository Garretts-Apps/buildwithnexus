# Rebuild Plan: buildwithnexus

**Date:** 2026-11-06
**Scope:** whole repository at `main` 57cdb9f (`harness/`, `bwn/`, `bin/`, `scripts/`, `field/`, `.buildwithnexus/`)
**Focus:** full entropy analysis, plan only. No code was changed.

## Summary

The codebase is already lean where it matters most. It ships 10 direct crates, the npm package has no dependencies, `cargo audit` reports no advisories, and the build has no dead-code warnings. The entropy sits in three places:

- **Speculative types** in the knowledge, rules and verifier modules that nothing constructs or calls.
- **Small helpers written several times over**, two of which have drifted apart into real bugs.
- **Four very long functions** that hold a lot of state at once.

| Measure | Value |
| --- | --- |
| Items in the plan | 32 |
| Estimated removable lines | about 950: about 600 of Rust, plus two orphan files of about 350 |
| Removable dependencies | none; one redundant manifest line |
| Correctness issues found along the way | 4 |
| Estimated effort | P1 about 6 hours, P2 about 3 days, P3 about 2 weeks, performance track about 3 days |

**Inventory**

| Language | Files | Lines |
| --- | --- | --- |
| Rust | 40 | 89,576 |
| JavaScript and MJS | 19 | 5,265 |
| Python | 6 | 2,745 |
| Shell and PowerShell | 8 | 2,934 |
| Markdown | 37 | 5,498 |
| YAML, TOML, JSON | 23 | 3,312 |

There are 152 tracked files. The four largest Rust files are about one third tests each, so their size overstates the production code:

| File | Production lines | Test lines |
| --- | --- | --- |
| `harness/src/tools.rs` | 7,511 | 3,761 |
| `harness/src/lib.rs` | 8,565 | 2,309 |
| `harness/src/agent.rs` | about 7,140 | about 3,305 |
| `harness/src/tui.rs` | about 7,940 | about 1,705 |

**How removals affect the published crate.** The crate is on crates.io, but `harness/src/lib.rs:6-9` says it "is not a stable API for building other tools on, and minor versions may rearrange it." Removing public items is therefore fine in the next minor release. Batch the removals into 0.17.0 rather than a patch release.

**Before any change.** `CLAUDE.md` requires a reported problem to be reproduced in a matching field cell before it is fixed. That applies to the Windows, terminal and install items below. Pure deletions of unreferenced code need only the test suites. The baseline is 923 unit tests and 207 integration tests passing, plus 113 of 114 Node tests passing.

## Correctness issues found along the way

These are bugs rather than entropy. They are listed first because the duplicate-logic fixes in P2 would fix them as a side effect.

- **C-1: four home-directory lookups ignore Windows.** `config::user_home` (`harness/src/config.rs:891`) falls back from `HOME` to `USERPROFILE`. Four other places read only `HOME`: `agent.rs:1685`, `local.rs:19`, `lib.rs:8201` and `tui.rs:1323`. Native Windows usually has no `HOME`. The likely effects are a blank home path in the system prompt, missing LM Studio models, and `~/` attachments resolving against the working directory. Reproduce in a Windows field cell before fixing. Fixed by ENT-020.
- **C-2: the knowledge-base indexer follows symlinked directories.** `handle_kb_index` (`lib.rs:4873-4894`) walks with `path.is_dir()`, which follows symlinks, and keeps no visited set. A symlink loop would keep it walking until the user interrupts. It also ignores `.gitignore`, unlike the walkers in `tools.rs`. Not reproduced. Fixed by ENT-031.
- **C-3: the two find-and-replace edit paths read files differently.** `edit_file` (`tools.rs:6451`) reads through the project's `read_text`, which is the path that editor integration (ACP) overrides. `str_replace_editor`'s `str_replace` (`tools.rs:7219`) calls `fs::read_to_string` directly. Under an editor integration, the second path may edit the on-disk file instead of the editor buffer. Fixed by ENT-022.
- **C-4: a failed workflow save leaves its temp file behind.** The atomic write in `workflow.rs:335` never removes its temp file after a failed rename. It and `session.rs:212` use a fixed temp name, so two processes can collide. Fixed by ENT-023.

## Priority 1: quick wins (under 2 hours each)

All P1 items delete code that nothing outside its own definition, or only tests, refers to. Each was confirmed by searching `harness/src`, `harness/tests`, `harness/benches`, `harness/examples` and `bwn/`.

### ENT-001: Delete the unused judgment and evidence types

- **Files:** `harness/src/knowledge.rs:123-217`, covering `RiskLevel`, `CostLevel`, `ReversibilityLevel`, `JudgmentPrimitive`, `default_confidence`, `ConfidenceLevel` and its `Display` impl, and `EvidenceRecord`.
- **Evidence:** each name appears only at its definition or as a field of another type in this list.
- **Impact:** about 95 fewer lines. **Effort:** XS. **Risk:** low, public API removal.

### ENT-002: Delete the unused relationship graph

- **Files:** `knowledge.rs:60` `RelationType` with 13 variants, the `Relationship` struct, the `Entity.relationships` field, and `KnowledgeBase::find_related` at `knowledge.rs:284`.
- **Evidence:** no variant is ever constructed. All four `Entity` constructions pass an empty list, at `lib.rs:5003`, `lib.rs:7134`, `tools.rs:7007` and `knowledge.rs:355`. `find_related` has no callers.
- **Note:** existing `entities.json` files carry a `relationships` key. Keep loading tolerant by ignoring unknown fields, which serde does by default unless `deny_unknown_fields` is set.
- **Impact:** about 60 lines. **Effort:** S. **Risk:** low.

### ENT-003: Delete the tree-sitter import stub and the test-only accessors

- **Files:** `knowledge.rs:312` `index_from_tree_sitter_output`, which has no callers. Also `remove_entity` at 250, `get_entity` at 255 and `find_by_type` at 276, which only the test at `knowledge.rs:473-477` uses.
- **Impact:** about 75 lines. **Effort:** XS. **Risk:** low.

### ENT-004: Delete the verifier's static-analysis stub and unread fields

- **Files:** `harness/src/verifier.rs:327` `run_static_analysis` always returns `None`. Delete it with `StaticAnalysisResult` and `VerificationReport.static_analysis_status`. Also remove `VerificationContext.git_diff`, which is only ever set to `None` at `lib.rs:5442` and `tools.rs:7069`. Remove `TestsStatus.coverage_delta`, which is only set to `None` at `verifier.rs:322`. Remove `check_tests`'s unused `_changed_files` parameter.
- **Impact:** about 30 lines. **Effort:** S. **Risk:** low.

### ENT-005: Delete uncalled verifier and rule-engine methods

- **Files:** `verifier.rs:383` `format_decision_memo`, `verifier.rs:150` `with_rules`, `rules.rs:146` `RuleEngine::new`, `rules.rs:480` `is_blocked` and `rules.rs:507` `format_violations_json`.
- **Evidence:** each name appears only at its definition. Production uses `Verifier::new`, `format_report`, `format_report_json` and `format_violations`.
- **Impact:** about 55 lines. **Effort:** XS. **Risk:** low.

### ENT-006: Delete or inline small dead and pass-through functions

- **Files:**
  - `tui.rs:5174` `drain_stdin`: an empty body that nothing calls.
  - `config.rs:1484` `skill_warnings_once`: only tests call it.
  - `sandbox.rs:292` `scrubbed_env`: only tests call it; they can call `scrubbed_env_keeping(names, &[])`.
  - `session.rs:109` `path`: an alias for the private `file`. Rename `file` to `path` instead.
  - `tui.rs:678` `color_disabled`: an alias for the private `no_color`.
  - `provider.rs:2435` `openai_request`: a one-caller wrapper around `openai_request_at`.
  - `harness/tests/fixtures/oauth_mcp_server.py:35`: an unused `import time`.
- **Already done:** `agent.rs:1983` `confirm` is removed on the `claude/anti-slop-pass` branch.
- **Impact:** about 30 lines. **Effort:** XS. **Risk:** none.

### ENT-007: Drop the redundant dev-dependency line

- **Files:** `harness/Cargo.toml:64` re-declares `serde_json = "1"` under dev-dependencies. It is already a normal dependency at line 46, so tests can use it.
- **Impact:** 2 lines. **Effort:** XS. **Risk:** none.

### ENT-008: Move or delete two orphan files in `.buildwithnexus/`

- **Files:**
  - `.buildwithnexus/hooks/PreToolUse/rule_gate.py`, 181 lines. Nothing in the repo references it, bwn only auto-discovers hooks under `~/.buildwithnexus/hooks/`, and the file's own docstring says to copy it there. Its `load_rules` function at line 27 is never called.
  - `.buildwithnexus/knowledge/schema.json`, 167 lines. Nothing reads it, because `KnowledgeBase` reads only `entities.json`.
- **Proposal:** move `rule_gate.py` to `examples/` with `load_rules` removed. Delete `schema.json`, or regenerate it after ENT-001 to ENT-003 if it is meant as documentation.
- **Impact:** up to 348 lines. **Effort:** XS. **Risk:** low.

## Priority 2: medium effort (2 to 8 hours each)

### ENT-020: One home-directory lookup and one `~` expander

- **Files:** home lookups at `config.rs:891`, `config.rs:612`, `agent.rs:2772`, `tools.rs:1923`, `tools.rs:3350`, `workdirs.rs:98`, `agent.rs:1685`, `local.rs:19`, `lib.rs:8201` and `tui.rs:1323`. `~/` expansion is written five times, and `tools::resolve` at `tools.rs:1924` is the only complete one.
- **Proposal:** make `config::user_home` the only home lookup and `tools::resolve` the only expander.
- **Impact:** about 30 lines, and fixes C-1. **Effort:** S. **Risk:** medium, because Windows behavior changes on purpose. Needs a Windows field-cell rehearsal.

### ENT-021: One UTF-8-safe truncation helper

- **Files:** `agent.rs:42` and `config.rs:908` are two equivalent functions. The same four-line char-boundary loop is inlined at `tools.rs:4102`, `tools.rs:4145`, `tools.rs:4164`, `acp.rs:949`, `hooks.rs:2947` and `lib.rs:5113`.
- **Proposal:** keep one `pub(crate)` helper. Check whether `str::floor_char_boundary` is stable on the 1.94 MSRV first. Each caller keeps its own truncation marker.
- **Impact:** about 25 lines. **Effort:** S. **Risk:** low.

### ENT-022: Share the find-and-replace core between the two edit tools

- **Files:** `tools.rs:6451-6511` (`edit_file`) and `tools.rs:7219-7272` (`str_replace`). Both count matches, fall back to `lenient_find`, store `UNDO_BACKUP` and call `write_atomic`.
- **Proposal:** one `replace_once(path, old, new)` that reads through `read_text`. This fixes C-3.
- **Impact:** about 45 lines. **Effort:** S. **Risk:** low; both tools have tests.

### ENT-023: One atomic-write helper with a permission policy

- **Files:** `checkpoint.rs:654`, `config.rs:3226`, `tools.rs:1900`, `session.rs:212` and `workflow.rs:335`.
- **Differences to keep:** `tools` copies the target's mode, `config` restricts the secrets file to 0600 or an ACL, and `checkpoint` takes an optional mode.
- **Proposal:** one `write_atomic(path, bytes, policy)` with a per-process temp name that cleans up on failure. This fixes C-4.
- **Impact:** about 35 lines. **Effort:** M. **Risk:** medium; the ordering of the secrets-file permission must not change.

### ENT-024: One settings-layer iterator

- **Files:** `config.rs:2682`, `config.rs:2740`, `hooks.rs:225` and `hooks.rs:2965` each list the user and project settings files and check trust.
- **Open question:** the `config` copies include the legacy `config.json` and the `hooks` copies do not. Confirm whether that is deliberate before merging them.
- **Impact:** about 25 lines. **Effort:** M. **Risk:** medium, because this touches trust semantics.

### ENT-025: Small duplicates, one batch

- `tools.rs:1639` and `mcp.rs:1330`: collapse whitespace and clip with an ellipsis. Make `clamp_preview` take a limit and delete `mcp::clip`.
- `provider.rs:948` and `provider.rs:968`: `finish_ollama` and `finish_openai` differ only in which parse functions they call.
- `tui.rs:3446` inlines `provider::short_tokens` at `provider.rs:393` twice.
- `tui.rs:4624` `pad_ansi_line` repeats `clip_ansi_line` at `tui.rs:4601` and then pads.
- `highlight.rs:973`, `report.rs:1154` and two copies in `tui.rs` are test-only ANSI strippers. They can use the existing `tui::strip_ansi`.
- `mcp_auth.rs:689` and `mcp_auth.rs:733` share an accept-any-status match.
- About 26 inline plural suffixes sit beside a private `mcp::plural` at `mcp.rs:956`, and the spinner frame table is defined three times at `agent.rs:668`, `tui.rs:3393` and `tui.rs:8264`.
- **Impact:** about 100 lines. **Effort:** M in total. **Risk:** low.

### ENT-026: Lift the four oversized arms out of the tool dispatcher

- **Files:** `tools.rs:6134-7473` `run` is a flat match with about 45 arms and is fine as a shape. Four arms are whole functions written inline: `str_replace_editor` at 7117-7338, `headless_browser` at 6783-6896, `Artifact` at 7339-7427 and `grep` at 6348-6417.
- **Proposal:** move each into a `run_*` function, the same way `start_server` and `check_work` already work. Leave the small arms inline.
- **Impact:** no line savings; `run` drops from 1,338 to about 800 lines. **Effort:** S. **Risk:** low.

## Priority 3: larger refactors (over 8 hours)

These move code rather than remove it. Do each as a pure move in its own PR, with no behavior change, and lean on the existing tests.

### ENT-030: Give the agent loop a turn-state struct

- **File:** `agent.rs:3248-4189` `build_turn`, 942 lines. It declares 25 mutable per-turn flags and counters at 3333-3357, which three large inner regions read and write.
- **Proposal:** first move the flags into a `TurnState` struct. That step alone cuts what a reader must track. Then extract the text-reply branch (3461-3616), the per-call handling (3632-3969) and the finish handling (3998-4141).
- **Effort:** L. **Risk:** high, because the order-dependent guards and early returns are easy to reorder by accident.

### ENT-031: Split the knowledge-base indexer

- **File:** `lib.rs:4858-5028` `handle_kb_index`, with nesting about 12 levels deep.
- **Proposal:** extract a pure `extract_symbol(ext, line)` from 4903-4985, which also makes it unit-testable. Replace the hand-rolled walk at 4873-4894 with the gitignore-aware walker in `tools.rs`. This fixes C-2.
- **Effort:** S to M. **Risk:** low. It is listed here only because it pairs naturally with ENT-001 to ENT-003.

### ENT-032: Give the REPL loop a state struct

- **File:** `lib.rs:1764-2802` `repl`, 1,039 lines. It runs about 30 prefix checks, a 41-arm match and turn execution over about 8 shared locals.
- **Proposal:** `setup_repl` returns a `ReplState`. Then extract `tick_background`, `read_task`, `reset_session` (for `/clear` and `/new`, which repeat each other) and `run_mode_turn`, which `/plan`, `/build` and `/brainstorm` currently copy.
- **Effort:** M. **Risk:** medium; the order of the prefix checks matters, and custom commands must stay after the built-ins.

### ENT-033: Give the raw line editor an editor struct

- **File:** `tui.rs:7381-8248` `read_line_raw_prefill`, 868 lines with about 20 locals and two macros.
- **Proposal:** an `Editor` struct with methods. Then extract reverse search (7771-7858), the vim NORMAL keymap (inside 7867-8023), history up and down (8068-8126) and non-key events (7494-7599).
- **Effort:** L. **Risk:** high. This is terminal behavior across platforms, and unit tests do not cover drawing. Rehearse in field cells on Linux, macOS, Windows and WSL.

### ENT-034: Split the giant files along their existing seams

- `provider.rs` splits cleanly by wire protocol: Anthropic at 1893-2342, OpenAI-compatible at 2343-2766 and Ollama at 2767-3227.
- `tools.rs` splits into tool definitions (about 1-480), shell-command policy (about 600-1640), background servers (about 2240-2840), file walking (4230-5382) and tests (7672-11272).
- `tui.rs` splits into images (961-1388), markdown rendering (1389-1986) and the line editor (6220-8249).
- `agent.rs` splits into compaction (53-1565), permission policy (1566-2998), and the BUILD, PLAN and BRAINSTORM modes.
- **Effort:** L in total, but each file is an independent PR. **Risk:** low, because these are moves with no logic changes.

### Left as is on purpose

- **`agent.rs:2394-2610` `gate`:** this is the permission policy, written as a chain of early returns. The order is the policy, and each step's comment says why it sits where it does. At most, share the two near-identical sensitive-path blocks at 2462-2486.
- **The `tools::run` match itself:** a flat dispatch is the clearest shape for about 45 tools.

## Performance track

There is no database, so the N+1, pagination and index checks do not apply. Every gain below is an estimate; nothing was benchmarked. Add a benchmark to `harness/benches/perf.rs` before changing any per-token item.

### PERF-001: Stop re-reading the session file on every save

- **File:** `session.rs:184`. `save` runs after every tool round (`agent.rs:3984`). It reloads and parses the whole existing file just to keep `created_ms` and `name`, clones every message, and rewrites the whole file. Messages carry base64 images, so this grows with the session.
- **Proposal:** cache `created_ms` and `name` in memory and serialize from a borrowed view. Later, consider append-only JSONL, as `trace.rs:189` already does.
- **Effort:** S. **Risk:** low.

### PERF-002: List checkpoints once per turn, filtered by name first

- **File:** `checkpoint.rs:214` `end_turn` calls `seal_since`, which lists every checkpoint, then lists them again at line 226. Every checkpoint for every project lives in one directory (`checkpoint.rs:59-62`), holds a file snapshot of up to 2 MB, and is parsed before the per-project filter applies.
- **Proposal:** the id starts with its creation time, so filter on the file name before parsing. List once, and store checkpoints per project.
- **Effort:** S. **Risk:** low.

### PERF-003: Make `grep_files` skip big and binary files and stop allocating per line

- **File:** `tools.rs:6381`. It reads up to 10,000 files in full with no size cap and allocates a new string for every line, even when the search is case-sensitive (`tools.rs:6384-6389`).
- **Effort:** S. **Risk:** low.

### PERF-004: Cheaper gitignore matching and directory walks

- **File:** `tools.rs:4340`. `IgnoreRule::matches` builds a fresh list of path components for every rule on every path. The walkers stat each entry twice (`tools.rs:4459`, `tools.rs:4469`) where `DirEntry::file_type` is free. This sits under every search tool and `@` completion.
- **Effort:** S. **Risk:** low.

### PERF-005: Keep `@` completion off the input thread

- **File:** `tui.rs:7121`. Each keystroke clones the cached list of up to 10,000 paths and lowercases every one. Every 5 seconds it re-walks the project synchronously while the user types.
- **Proposal:** a shared, pre-lowercased list refreshed in the background or after tool writes.
- **Effort:** M. **Risk:** low.

### PERF-006: Stop repainting the composer twice per streamed chunk

- **File:** `agent.rs:742-744`. Each chunk calls `poll_typeahead`, which repaints the composer and footer unconditionally, and then repaints them again. The typeahead thread already repaints every 15 ms.
- **Proposal:** repaint only when a key was actually read, the way `get_interrupt_kind` already does.
- **Effort:** S. **Risk:** low. Rehearse in a terminal field cell.

### PERF-007 to PERF-012: smaller items

- **PERF-007:** `read_file` at `tools.rs:6156` reads the whole file before applying the 100 KB cap or a line range. Check the size first and stream the head.
- **PERF-008:** `tui.rs:317` re-wraps the whole open line on every chunk, which is quadratic for long unbroken paragraphs. Add a long-line benchmark before acting.
- **PERF-009:** `tui.rs:1465` reallocates the pending stream buffer for every newline. Use `drain`.
- **PERF-010:** `estimate_tokens` at `agent.rs:66` re-serializes every tool input on every step. Sum lengths instead.
- **PERF-011:** each turn spawns about 8 git processes through `checkpoint.rs:362`. Cache the per-folder git checks and merge the two `rev-parse` calls.
- **PERF-012:** `session::list` at `session.rs:271` parses every message of every session just to show titles, and the continue path does it twice (`lib.rs:3223-3225`). Parse a header struct only.

## Excluded findings

- **Public API removals:** ENT-001 to ENT-006 remove `pub` items. They stay in the plan because the crate declares itself unstable, but ship them in a minor release.
- **`EntityType` variants that are never constructed** (`knowledge.rs:20`): removing them would break any hand-written `entities.json` that uses those names. Defer until loading maps unknown names to a fallback.
- **`EvaluationContext` fields that are always defaulted** (`rules.rs:125`): user rule files can reference these conditions, so they are config surface.
- **`EditorFiles`, a trait with a single implementor** (`tools.rs:1865`): it keeps `tools` from depending on `acp`. That is a defensible boundary.
- **`Effort::LEVELS`** (`config.rs:167`): only tests use it, but it keeps the `/effort` completion list in sync with the enum.
- **Pedantic clippy style warnings:** about 1,400, mostly missing attributes, redundant closures and doc formatting. Clippy's `--fix` can apply them, but they are style, not entropy.
- **Election, voter and ballot handling:** not applicable; this project handles none.
- **In-flight SDD work:** none; the repo has no `.claude/state/` files.

## Checks that ran clean

- `cargo audit`: no advisories across 170 locked crates.
- **Runtime crates:** 84. The two duplicated versions do not ship twice: `webpki-roots` 0.26.11 is a re-export shim over 1.0.11, and the second `syn` is build-time only.
- **Every direct crate is imported.**
- **Commented-out code:** none longer than five lines in any language.
- **Every JS, MJS and shell script has an inbound reference.**
