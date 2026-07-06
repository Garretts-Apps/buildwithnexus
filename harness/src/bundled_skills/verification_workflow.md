# Verification Workflow

When implementing code changes (bug fixes, features, refactors), follow this constrained workflow. Do NOT edit files immediately.

## Before Writing Code

1. **Understand the scope**
   - Search the repo for relevant files: `search` for keywords
   - Read the affected code: `read_file` on key files
   - Map the call graph: `grep` for callers of affected functions
   - Check existing tests: `glob` for test files, `read_file` to review them

2. **Identify constraints**
   - Check for public API contracts that must not break
   - Check for database schema dependencies
   - Check for naming and error-handling conventions
   - Check for existing CI configuration
   - Check git history for related changes: `bash git log --oneline -10 -- <file>`

3. **Reproduce the issue** (for bug fixes)
   - Read the test suite for the affected area
   - If possible, write a failing test first
   - Confirm the failure before fixing

## Writing Code

4. **Apply the smallest safe fix**
   - Do not change public API behavior unless required
   - Prefer narrow patches over broad rewrites
   - Do not weaken validation to fix parsing bugs
   - Respect existing naming and error-handling conventions
   - Do not introduce new runtime dependencies unless justified

5. **Add or update tests**
   - Every bug fix MUST include a regression test
   - Every new feature MUST include unit tests
   - Tests should cover the happy path AND edge cases

## After Writing Code

6. **Verify the change**
   - Run the relevant test suite: `bash` with the project's test command
   - Check for lint/format issues if a linter is configured
   - Review the diff for unintended changes

7. **Summarize the change**
   - Root cause (for bugs)
   - Files affected
   - Fix strategy
   - Tests added or changed
   - Validation result (tests pass/fail)
   - Residual risk (what could still go wrong)

## Rules

- A bug fix without a regression test is INCOMPLETE
- Do not claim a fix is complete until tests pass
- If a change touches authentication, flag for security review
- If a migration is destructive, require a rollback strategy
- If a function is public, search for all callers before changing its contract
- If logs might contain secrets, block the change
- If the change adds a dependency, check license and maintenance status
