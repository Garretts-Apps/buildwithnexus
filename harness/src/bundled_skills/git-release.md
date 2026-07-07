# Git And Release

Use this skill for commit hygiene, npm package release prep, GitHub CI repair, and deployment-oriented changes.

## Git discipline

- Inspect `git status` before staging.
- Do not revert unrelated user changes.
- Keep commits scoped and authored with the configured repository identity.
- For this project, release commits should use `geaglin09 <geaglin09@gmail.com>`.

## Version discipline

- Do not ask users to retest a version they already reported as broken.
- Bump `package.json`, `harness/Cargo.toml`, and `harness/Cargo.lock` together.
- Verify the npm package does not include unrelated build artifacts.
- Next.js is not a CLI dependency.

## Verification

- Use `cargo test --manifest-path harness/Cargo.toml`.
- Use `cargo clippy --manifest-path harness/Cargo.toml --all-targets --locked -- -D warnings`.
- Use `npm pack --dry-run` to inspect package contents before publishing.
