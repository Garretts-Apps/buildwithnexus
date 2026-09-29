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
dependencies and no lifecycle scripts**. Nothing executes and no network
access happens during `npm install`.

The native binary is not in the npm tarball. The launcher
(`bin/buildwithnexus.js`) looks for it in this order: `BWN_BIN`, then a
per-platform package (`buildwithnexus-<os>-<cpu>`) if one is installed, then
a binary downloaded on an earlier run, then a local `cargo build` in a repo
checkout. The per-platform packages are listed in the published
`optionalDependencies` only for platforms whose package exists on npm for
that version. A platform without a published package uses the first-run
download below.

**First-run download.** If no binary is found, the launcher runs
`scripts/bootstrap.js`, which downloads the release asset for your platform
from the GitHub Release matching the installed version:

- In an interactive terminal (stdin is a TTY) it downloads automatically on
  the first run. There is no confirmation prompt: installing the package is
  taken as the request to fetch its binary.
- In non-interactive use (CI, pipes, scripts) it never downloads on its own.
  Opt in with `bwn --bootstrap` or `BWN_ALLOW_BOOTSTRAP=1`; otherwise the
  launcher exits 1 with instructions.
- `BWN_SKIP_INSTALL=1` disables the download entirely.

The download is HTTPS only and follows at most five redirects. Every URL,
including each redirect target, must be on `github.com` or any
`*.githubusercontent.com` host (which covers
`objects.githubusercontent.com`, where release assets are served, but also
hosts such as `raw.githubusercontent.com`). Other hosts are refused. The
SHA-256 checksum is read from the `.sha256` file published in the same
GitHub Release, and a binary that does not match is deleted instead of being
installed. Because the checksum and the binary come from the same release,
the check catches corruption and a swapped asset, not a compromised release;
use the attestations below for that.

To avoid the download, set `BWN_BIN` to a binary you built or verified
yourself.

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
  **Trusted Publishing**, so no long-lived registry tokens are needed for
  routine releases. The one exception is the one-time first publish of each
  per-platform npm package, which npm only allows with a token.
- npm publish is **gated on the release workflow succeeding**, so a package
  version can never point at binaries that don't exist.
- Every third-party GitHub Action is pinned to a full commit SHA (the
  version is in a trailing comment), and tools the workflows download are
  pinned to a version and checked against a recorded SHA-256. Dependabot
  proposes updates weekly for Actions, npm and Cargo.
- Workflows run with least-privilege permissions: `permissions: {}` at the
  workflow level, with each job granted only what it uses. `id-token: write`
  is granted only to the jobs that sign attestations or exchange OIDC tokens
  with npm and crates.io. Checkouts do not keep the `GITHUB_TOKEN` in
  `.git/config`, except in the manual publish path that pushes a version
  bump.
- Releases run one at a time, a manual release can only be started from
  `main`, and a newly created tag points at the commit the workflow built.
- Inside the harness itself: mutating file tools are gated by the permission
  model, sensitive paths and catastrophic commands require confirmation even in
  `auto`, API keys are refused over non-HTTPS endpoints, and key-like tokens are
  redacted from surfaced errors.

## Permission Gates and the Optional Sandbox

`ask` / `auto` / `readonly` modes, protected paths, and checkpoints are
guardrails against mistakes. They are **not OS-level isolation**. An approved
command runs with your user's permissions, and checkpoints can rewind file
edits in the working tree but not network calls, pushed commits, published
packages, or other external effects.

Shell commands (`run_command`/`bash`, `!cmd`, and `check_work`) can also run
inside an opt-in OS sandbox. It is off by default. Turn it on with the
`"sandbox"` setting, `--sandbox <mode>`, or `/sandbox <mode>`:

- `auto` confines commands when a backend works, and otherwise runs them
  unconfined and says so once per session.
- `require` refuses to run commands when no backend works.

Backends: `bwrap` (bubblewrap) on Linux and `sandbox-exec` (Seatbelt) on
macOS. Windows and WSL have no backend, so `auto` runs unconfined there and
`require` refuses to run commands.

Inside the sandbox, the command can write only to the working directory and
the temp directories; the rest of the filesystem, including
`~/.buildwithnexus`, is read-only. On Linux, `/tmp` is a fresh private
directory that is discarded when the command exits. On macOS, `/tmp` and
`$TMPDIR` are the real, shared directories. Network access stays on unless
you set `"sandbox_network": false`.

The sandbox does not confine: reads (the whole filesystem stays readable,
including files such as `~/.ssh` and `.env` unless their permissions stop
your user), network access by default, the agent's own file tools (which are
fenced to the working directory by the permission gate instead), hooks, MCP
servers, and `start_server`. It never approves anything; it only limits what
an already-approved command can touch. For untrusted work, use a container
or VM.
