# Security Review

Use this skill for threat modeling, dependency risk, command execution risk, hook/plugin safety, credential handling, and prompt-injection-sensitive workflows.

## Workflow

1. Use `grep_files` for risky patterns: secrets, shell execution, network fetches, deserialization, path traversal, auth, permissions.
2. Use `read_many_files` for security-critical flows.
3. Use `run_command` for native scanners only if already installed or configured by the repo.
4. Use `todo_write` to track threat model areas.

## Review checklist

- Inputs are validated before filesystem, shell, or network actions.
- Paths are normalized and sensitive locations are protected.
- Hooks and commands are visible/auditable.
- Credentials are not logged, traced, or sent to non-HTTPS endpoints.
- Permission gates cover mutating and destructive operations.
- External content cannot silently instruct the agent to run unsafe commands.
