# Hero: Ship

Use this skill before declaring any work finished — before "done", before committing, before telling the user it works. The standard: evidence or it didn't happen.

## The evidence checklist

Collect what applies; skip nothing that does:

1. **Run the real thing.** Tests passing is necessary, not sufficient — exercise the actual flow a user would hit (launch the CLI, run the command, load the page). A change verified only by its own unit test has not been verified.
2. **Full relevant test suite**, not just the new test. Paste the summary line, not a paraphrase.
3. **Lint/typecheck clean** if the project has them configured.
4. **Read the final diff top to bottom** — hunt for debug prints, leftover TODOs, accidental file touches, and changes that don't belong to this task.
5. **Fresh-eyes pass on names and messages**: error text a stranger can act on, names that mean what they say.

## Reporting — the honesty contract

- State what was verified and **how** ("ran `cargo test`: 405 passed; exercised broken-config startup in a PTY").
- State what was **not** verified and why ("untested on Windows — no runner here"). An unverified claim marked as such is honest; unmarked, it's a lie with latency.
- If something failed, lead with that. Never bury a red result under a green summary.
- "Should work" is banned vocabulary. Either it was observed working, or the report says it wasn't observed.

## Rules

- Incomplete work stated plainly beats complete-looking work. The reader will build on what you claim.
- If the checklist finds a problem, fix it and re-run the checklist — shipping is a loop, not a gate you argue with.
