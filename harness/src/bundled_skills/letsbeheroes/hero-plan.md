# Hero: Plan

Use this skill after an approach is chosen and before the first edit. A plan is a checklist a different agent could execute without asking questions.

## Shape of a plan

1. **Goal** — one sentence, testable ("`/model` swap refuses unconfigured providers and walks the user through setup").
2. **Non-goals** — what this change deliberately does not touch. Scope creep dies here.
3. **Steps** — numbered, each with:
   - the concrete action (files, functions, commands)
   - a **verify criterion**: the observable fact that proves the step worked (a passing test name, a command and its expected output, a rendered screen)
4. **Risks** — the two or three most likely ways this goes wrong, and what to watch for.
5. **Rollback** — how to get back to known-good if step N fails (usually: git, checkpoints, or "revert the commit").

## Rules

- A step without a verify criterion is a hope, not a step. Add one or merge it into a step that has one.
- Front-load the assumption checks named during /hero-brainstorm — cheapest disproof first.
- Steps small enough that failure points at one cause. "Wire everything up" is three steps pretending to be one.
- Write the plan down (a file, the transcript, a PR description) — plans that live only in working memory don't survive interruptions.

## Hand-off

Execute with /hero-execute. If planning revealed the approach is wrong, go back to /hero-brainstorm — that's the plan working, not failing.
