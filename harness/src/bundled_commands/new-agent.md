---
description: Scaffold a helper agent file with tools and read-only default
---

# New agent

Name: $ARGUMENTS

1. Create `<name>.md` under the project's agents folder with `name`, `description`, and `tools`.
2. Set `read_only: true` unless the agent must edit.
3. Body: goal, output format, and off-limits paths.
4. Match tool names the runtime already documents.