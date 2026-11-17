# CI examples (opt-in)

Copy workflows into `.github/workflows/` and tune triggers. They use upstream actions; nothing runs until you enable them in GitHub.

| File | Purpose |
| --- | --- |
| `secret-scan.yml` | TruffleHog on push and pull_request |
| `dependency-audit.yml` | Ecosystem audits via matrix (Node, Rust, Python) |
| `codeql-analysis.yml` | CodeQL init/analyze for JS, Python, and Java/Kotlin |

Runs on `ubuntu-latest` and `windows-latest` where noted. Secrets scanning is read-only on checkout.