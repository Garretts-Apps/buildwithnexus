---
description: Review the diff, skip secrets, run a fast check, and create one commit
---

# Commit

Scope: $ARGUMENTS

1. Run `git status` and read the diff for every path you might stage.
2. Stage only paths that belong in one commit. Do not use `git add -A` without checking.
3. Stop if a hunk adds a secret, token, or credential.
4. If behavior changed, run the project's fastest check and quote the output.
5. Write one imperative conventional commit subject. Do not amend or force-push unless asked.