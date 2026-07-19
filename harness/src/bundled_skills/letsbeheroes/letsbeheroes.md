# letsbeheroes

The letsbeheroes collection: process skills that turn one-shot answers into dependable engineering. Use this skill when starting any substantial piece of work, or when unsure which discipline applies.

## The loop

Every non-trivial task moves through five stages. Each has a dedicated skill:

1. **/hero-brainstorm** — before designing: surface at least three approaches and pick one for stated reasons.
2. **/hero-plan** — before building: write a numbered plan where every step has its own verify criterion.
3. **/hero-execute** — while building: one step at a time; a failed assumption stops the plan, it doesn't get improvised around.
4. **/hero-debug** — when anything misbehaves: reproduce first, find the root cause, prove the fix against the reproduction.
5. **/hero-ship** — before saying "done": evidence or it didn't happen.

Two support skills apply throughout:

- **/hero-wait** — never sleep-and-hope; wait on observable conditions with deadlines.
- **/hero-subagents** — fan work out when tasks are independent; verify results before trusting them.

## When to skip stages

- One-line fixes with an obvious test: go straight to /hero-debug or /hero-ship discipline.
- Pure questions: answer them; the loop is for changes.
- Anything touching data, auth, or public APIs: never skip /hero-plan or /hero-ship.

## The oath

- Say what you will do before doing it; say what you actually did after.
- A claim without evidence is a guess. Label guesses as guesses.
- Surprises invalidate plans. Stop, update the plan, then continue.
- Leave the campsite cleaner: failing tests fixed or reported, temp files removed, no half-applied changes.
