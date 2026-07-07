# Security Policy

## Supported Versions

`buildwithnexus` is published to npm under semantic versioning. Only the latest
released minor line receives security fixes.

| Version    | Supported          |
|------------|--------------------|
| 0.10.x     | :white_check_mark: |
| < 0.10     | :x:                |

## Reporting a Vulnerability

**Do not open a public GitHub issue for security problems.**

Report vulnerabilities privately via GitHub's coordinated-disclosure flow:

  https://github.com/Garretts-Apps/buildwithnexus/security/advisories/new

Or by email to `security@buildwithnexus.dev`.

We aim to acknowledge new reports within **3 business days** and to ship a fix
or mitigation within **30 days** of acknowledgement, depending on severity. We
will credit reporters in the advisory unless they ask to remain anonymous.

## Scope

In scope:

- The `buildwithnexus` npm package: the Node wrapper (`bin/`, `scripts/`) and
  the Rust harness source (`harness/`) it builds or downloads.
- The CI, release, and publish pipelines in `.github/workflows/`.
- The prebuilt binaries and checksums attached to GitHub Releases.

Out of scope:

- Vulnerabilities in upstream dependencies — please report those upstream first;
  if exploitable through `buildwithnexus`, also notify us.
- Self-XSS or social-engineering against your own developer machine.
- Issues that require an attacker to already control your CI secrets, npm
  account, or developer machine.

## Verifying a Release

Every release on npm from `v0.9.0` onward ships with **npm provenance** — an
attestation cryptographically tying the tarball back to the specific GitHub
Actions workflow run that built it:

```sh
npm view buildwithnexus@<version> --json | jq .dist.attestations
```

Each per-platform binary on a GitHub Release carries a `.sha256` checksum file
and a build-provenance attestation:

```sh
gh attestation verify buildwithnexus-<target> --repo Garretts-Apps/buildwithnexus
```

You can also skip the prebuilt binaries entirely and build from the tagged
source:

```sh
git clone --branch v<version> https://github.com/Garretts-Apps/buildwithnexus
cd buildwithnexus/harness
cargo build --release --locked
```

## How `npm install` Behaves

The npm package is a thin wrapper with **zero runtime npm dependencies**. It
declares one lifecycle script, `postinstall`, which:

1. Downloads the prebuilt binary for your platform from the matching GitHub
   Release over HTTPS.
2. **Verifies its SHA-256** against the checksum published with the release
   before marking it executable.
3. Falls back to a local `cargo build` from the bundled Rust sources if no
   binary matches your platform (or the download fails verification).

It executes nothing else and installs no transitive npm packages.

## Hardening Inside the Package

- Publishing uses npm's **OIDC Trusted Publisher** flow — no long-lived npm
  token exists to steal; `id-token: write` is granted only to the publish job.
- npm publish is **gated on the release workflow succeeding**, so a package
  version can never point at binaries that don't exist.
- All GitHub Actions in our workflows are version-pinned (Dependabot proposes
  bumps weekly).
- Workflows run with least-privilege permissions (`permissions: {}` at the
  workflow level, escalated per job only where needed).
- Inside the harness itself: mutating file tools are gated by the permission
  model, sensitive paths and catastrophic commands require confirmation even in
  `auto`, API keys are refused over non-HTTPS endpoints, and key-like tokens are
  redacted from surfaced errors.
