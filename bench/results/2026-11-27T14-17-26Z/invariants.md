# Invariant suite score, 2026-11-27T14-17-26Z

Machine: Linux 6.18.44-fc-v880 x86_64. Each cell is passed/total tests for that
acceptance case on that backend. A test that does not apply to a
backend (for example a Landlock-only check on bubblewrap) passes
without asserting; the case IDs are defined in docs/roadmap.md and
section 5 of docs/competitor-analysis.md.

| Case | bwrap | landlock | none |
|---|---|---|---|
| AU1 | 7/7 | 7/7 | 7/7 |
| AU2 | 2/2 | 2/2 | 2/2 |
| AU5 | 1/1 | 1/1 | 1/1 |
| D1 | 3/3 | 3/3 | 3/3 |
| G3 | 1/1 | 1/1 | 1/1 |
| IN1 | 2/2 | 2/2 | 2/2 |
| IN2 | 9/9 | 9/9 | 9/9 |
| IN3 | 2/2 | 2/2 | 2/2 |
| M8B | 1/1 | 1/1 | 1/1 |
