# Advanced Git Discipline & Operations

Use this skill for any git operation: status checks, staging, committing, branch management, worktrees, PR creation, merge conflict resolution, and diff inspection.

## Core Rules

1. **Always Inspect Status & Diff First:**
   - Run `git status` and `git diff` before staging or committing files.
   - Never use `git add .` or `git add -A` blindly without verifying what files are touched.

2. **Commit Hygiene & Conventional Commits:**
   - Write clear, imperative conventional commit messages (`feat: ...`, `fix: ...`, `refactor: ...`, `test: ...`, `docs: ...`).
   - Keep commits atomic: one feature or fix per commit.

3. **Branching & Worktree Discipline:**
   - Always create dedicated feature branches (`git checkout -b feat/<name>`).
   - Use `git worktree` for isolated parallel work without dirtying the working directory.
   - Clean up temporary branches and worktrees on completion (`git worktree remove --force`).

4. **CI & PR Workflow:**
   - Run local linting (`cargo fmt --check` / `cargo clippy`) and tests (`cargo test`) BEFORE opening a PR or pushing to remote.
   - Monitor GitHub CI status with `gh run list` and `gh run view`.
   - Never merge a PR if CI checks are failing. Fix all CI failures first.
