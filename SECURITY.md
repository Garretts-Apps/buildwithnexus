# Security Policy

## Supported Versions

`buildwithnexus` is published to npm and crates.io under semantic versioning.
Only the latest released minor line receives security fixes. Before 1.0,
configuration and session formats may change between minor releases.

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

- The `buildwithnexus` npm package: the Node launcher (`bin/`, `scripts/`,
  `index.js`) and the per-platform binary packages
  (`buildwithnexus-<os>-<cpu>`).
- The `buildwithnexus` and `bwn` crates on crates.io.
- The CI, release, and publish pipelines in `.github/workflows/`.
- The prebuilt binaries, checksums, and attestations attached to GitHub
  Releases.

Out of scope:

- Vulnerabilities in upstream dependencies — please report those upstream first;
  if exploitable through `buildwithnexus`, also notify us.
- Self-XSS or social-engineering against your own developer machine.
- Issues that require an attacker to already control your CI secrets, npm
  account, or developer machine.

## How `npm install` Behaves

The npm package is a thin, script-free wrapper with **zero runtime npm
dependencies and no lifecycle scripts** — nothing executes and no network
access happens during `npm install`.

The prebuilt binary arrives as one of five per-platform packages
(`buildwithnexus-<os>-<cpu>`) declared as `optionalDependencies` and selected
by npm via `os`/`cpu` — the same pattern esbuild uses.

**First-run fallback (consent-gated):** if no platform package is installed
(for example, `--omit=optional`, an unsupported combination, or a platform
package not yet published for a brand-new version), the launcher offers to
download the binary for your platform from the matching GitHub Release over
HTTPS — it asks `[y/N]` on a TTY, and in non-interactive use requires
`bwn --bootstrap` or `BWN_ALLOW_BOOTSTRAP=1`. Downloads are restricted to
`github.com` / `objects.githubusercontent.com` (`scripts/bootstrap.js`) and
the SHA-256 is verified against the checksum published with the release
before the binary is marked executable. Nothing is ever downloaded without
that explicit consent; alternatively set `BWN_BIN` to a binary you built or
verified yourself.

## Verifying a Release

Binary **build-provenance attestations are the primary integrity control** —
they cryptographically tie each artifact to the exact GitHub Actions workflow
run that built it. The `.sha256` files detect corruption in transit; they are
published alongside the binaries and do not by themselves prove the release
pipeline was not compromised.

```sh
# npm tarball provenance
npm view buildwithnexus@<version> --json | jq .dist.attestations

# per-platform binary provenance
gh attestation verify buildwithnexus-<target> --repo Garretts-Apps/buildwithnexus
```

You can also skip the prebuilt binaries entirely and build from the tagged
source:

```sh
git clone --branch v<version> https://github.com/Garretts-Apps/buildwithnexus
cd buildwithnexus
cargo build --release --locked --manifest-path harness/Cargo.toml
```

## Auto-updates

Update behavior is controlled by the `auto_update` setting in
`~/.buildwithnexus/settings.json`:

| Value       | Behavior                                                        |
|-------------|-----------------------------------------------------------------|
| `"off"`     | No registry check, no notices.                                  |
| `"notify"`  | **Default.** Daily check; a one-line notice on the next launch when a newer version exists. Never installs. |
| `"install"` | Daily check plus silent background `npm install -g`; notice on the next launch. |

`BWN_NO_AUTO_UPDATE=1` is honored for back-compat and caps `"install"` to
`"notify"`. The check runs inside the CLI (not the npm wrapper), never blocks
startup, and installs performed by other means (cargo, source builds) are
never auto-updated.

## Hardening Inside the Package

- Publishing uses npm's **OIDC Trusted Publisher** flow and crates.io
  **Trusted Publishing** — no long-lived registry tokens exist to steal;
  `id-token: write` is granted only to the publish job.
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

## Permission Gates Are Not a Sandbox

`ask` / `auto` / `readonly` modes, protected paths, and checkpoints are
guardrails against mistakes — they are **not OS-level isolation**. An approved
command runs with your user's full permissions, and checkpoints can rewind
file edits in the working tree but not network calls, pushed commits,
published packages, or other external effects. Run untrusted work inside a
container or VM.
