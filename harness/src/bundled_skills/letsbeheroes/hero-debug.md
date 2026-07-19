# Hero: Debug

Use this skill the moment behavior differs from expectation — a failing test, a bug report, a "that's odd" in command output. Symptoms get patched; causes get fixed.

## The sequence — no skipping

1. **Reproduce first.** A bug you can't trigger on demand is a rumor. Find the smallest command, input, or test that shows it, and record it — this reproduction is also your proof at the end.
2. **Read the actual error.** The whole message, the real line numbers, the caused-by chain. Most debugging time is wasted acting on a skimmed error.
3. **Locate by bisection.** Halve the search space each move: which commit (git bisect), which layer (does the bad value exist before or after this boundary?), which input half. Print/log the value at the midpoint and cut again.
4. **Name the root cause in one sentence** before writing the fix. If the sentence contains "somehow" or "sometimes", keep digging. "The parser drops the last field because the loop tests `<` instead of `<=`" is a root cause; "the parser is flaky" is not.
5. **Fix the cause, prove it with the reproduction from step 1**, then run the full relevant suite — fixes that break neighbors aren't fixes.
6. **Pin it with a regression test** that fails on the old code and names the scenario, so the bug needs a new idea to come back.

## Rules

- Change one thing per experiment. Two changes and a pass teaches you nothing.
- Never "fix" by weakening the assertion, widening the type, or catching-and-ignoring — that's hiding, and it converts a loud bug into a quiet one.
- A fix you can't explain is a coincidence that will expire. If the bug "went away", the sequence hasn't finished.
- Keep a note of dead ends explored — the second debugging session shouldn't repeat the first.
