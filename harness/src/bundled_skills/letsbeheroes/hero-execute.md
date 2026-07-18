# Hero: Execute

Use this skill when working through a written plan. Execution is deliberately boring: the creativity happened in /hero-brainstorm, the thinking in /hero-plan.

## Rules

1. **One step at a time, in order.** Finish a step's verify criterion before starting the next. No pipelining "while tests run".
2. **Verify means run.** Read-the-code-and-nod is not verification; execute the criterion the plan wrote down and look at the output.
3. **A surprise stops the line.** If reality disagrees with the plan — an API that doesn't exist, a test that was already failing, a file that isn't where the plan said — do not improvise around it. Stop, diagnose (small surprises) or return to /hero-plan (structural ones), update the plan, then continue.
4. **Track state visibly.** Mark steps done as they complete. After an interruption, the plan plus its marks must be enough to resume.
5. **Never batch fixes with steps.** If step 3 reveals a bug in step 1's work, go back and re-verify step 1's criterion after fixing — downstream steps assumed it held.

## Discipline under failure

- Re-read the actual error before acting on it. The second read is where the real message is.
- Two failed attempts at the same step means the plan's assumption is wrong — stop patching, start /hero-debug.
- Keep the working tree honest: no commented-out corpses, no `TODO: put back`, no disabled tests as a way to make a step "pass".

## Done

The plan's last step is verified and /hero-ship has run. Not before.
