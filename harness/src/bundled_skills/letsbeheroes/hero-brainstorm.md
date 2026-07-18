# Hero: Brainstorm

Use this skill before designing or building anything non-trivial — a new feature, a refactor, an architectural choice — and whenever the user asks "how should we…" or seems undecided.

## Rules

1. **Restate the problem in one sentence** — the actual need, not the requested mechanism. If the user asked for a caching layer, the problem might be "page loads are slow".
2. **Generate at least three genuinely different approaches.** Different libraries are not different approaches; different shapes of solution are. Include the boring option and the do-nothing option when honest.
3. **For each approach, state**: what it costs now, what it costs later, what it can't do, and what has to be true for it to work.
4. **Pick one and say why** — in terms of the constraints that decided it, not adjectives. "B, because the data fits in memory and we own both callers" beats "B is cleaner".
5. **Name the assumptions that would change the pick.** These become the first things to check during /hero-plan.

## Bias

- YAGNI: the approach that solves today's problem with the least machinery wins ties.
- Reversible beats optimal: prefer choices that are cheap to undo while information is still arriving.
- Steal before building: search the codebase for prior art first — an existing pattern beats a better new one.

## Output

A short brief: problem, options with tradeoffs, the pick with reasons, the assumptions to verify. Then hand off to /hero-plan.
