---
name: verify-before-done
description: Prove the claim before marking work complete
---

# Verify before done

Use this before saying a task is finished.

## Checklist

1. Restate what "done" means in one sentence tied to user-visible behavior.
2. Run the narrowest command that proves it (test, build, curl, screenshot). Quote full command and output.
3. If you fixed a bug, show the test failing before and passing after when you can.
4. List what you did not run (larger suite, other platforms) without hiding it.
5. Stop if output contradicts the claim; fix or report the mismatch.

Do not mark todos complete on intent alone. Do not paste truncated logs without saying they are truncated.