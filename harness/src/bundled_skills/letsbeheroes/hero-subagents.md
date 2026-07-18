# Hero: Subagents

Use this skill when deciding whether to fan work out to subagents (the `task` / `spawn_subagent` tool) and how to make their results trustworthy.

## When to delegate

- **Independent subtasks** with no ordering between them: auditing five modules, fixing an agreed list of lint classes, researching three libraries.
- **Search that would flood context**: "find every caller of X across the repo" — the subagent reads the noise, the parent gets the conclusion.
- **A second, unbiased pass**: reviews and verifications are stronger from an agent that didn't write the code.

## When NOT to delegate

- Tasks that share mutable state or must land in order — merge conflicts and races cost more than parallelism saves.
- Work needing judgment calls that belong to the user or the parent's accumulated context.
- Anything small enough to just do — a subagent has startup cost and loses your context.

## Scoping a subagent task

A good brief is self-contained; the subagent has none of your context:

1. **Objective** — one sentence, with the definition of done.
2. **Inputs** — exact paths, names, and constraints it needs. "The config loader" is your context; `harness/src/config.rs` travels.
3. **Boundaries** — what it must not touch or change.
4. **Return shape** — what to report back: findings list, diff summary, pass/fail with evidence.

## Trust, but verify

- Subagent output is a claim, not a fact. Spot-check it the same way /hero-ship checks your own work: run a sample, re-grep one finding, read one changed file.
- Failures must surface — a subagent that errored or returned nothing is a result to report, never to silently drop.
- Merge results deliberately: dedupe overlapping findings, and re-run the full test suite after any subagent edited files.
