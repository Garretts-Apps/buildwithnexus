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
checkout. No per-platform package carries a binary, so in practice the
binary comes from the first-run download below.

**Unregistered platform package names.** Versions 0.12.1 to 0.14.2 list
`buildwithnexus-<os>-<cpu>@<version>` in `optionalDependencies`, but no such
package was ever published, so anyone could register those names and have
their code installed with those versions. Publishing is OIDC-only, and npm's
OIDC publishing cannot create a new package, so the names stay unregistered.
Do not install 0.12.1 to 0.14.2; use `buildwithnexus@latest`.

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
expected SHA-256 comes from `checksums.json` inside the npm package: before
publishing, `publish.yml` checks every release binary against its
build-provenance attestation and records the verified hashes there, and a
published npm tarball cannot change. A binary that does not match is deleted
instead of being installed, so an asset swapped on the GitHub Release after
publishing is refused. (A checkout without `checksums.json` falls back to the
release's own `.sha256` file, which only catches corruption.)

**Before it says "ready".** A matching checksum only shows the download is
intact. The launcher then runs the new binary once with `--version` (15 s
limit, output captured) and reports it ready only if it answers with the
expected version. Otherwise it explains the failure and exits 1. On Linux the
prebuilt binaries need glibc 2.34 or later (Ubuntu 22.04+, Debian 12+,
RHEL/Rocky/AlmaLinux 9+, Amazon Linux 2023, Fedora 35+) and do not run on
musl (Alpine). A binary that endpoint protection blocks or removes gets its
path, its SHA-256 and a pointer to [For IT and Security
Teams](#for-it-and-security-teams). A later run that cannot start the binary
gets the same explanation.

Each verified download is recorded in `bin/.installed.json` (version and
SHA-256) as soon as its checksum matches, before it is moved into place, so a
file that security software quarantines on write counts too. If that record
is there but the binary is not, the launcher does not download it again on its
own, because whatever removed it would remove the next copy too. Once the file
is allowed, `bwn --bootstrap` downloads it again.

A request that gets no data for 30 s fails with the reason. Node's `https`
module ignores `HTTPS_PROXY` unless `NODE_USE_ENV_PROXY=1` is set, which Node
22.21+ and 24.5+ support; a failed download says so and prints the command.

The launcher only uses a platform package installed next to the main package,
never one found in a parent directory's `node_modules`, and ignores a relative
`BWN_BIN`.

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

## For IT and Security Teams

This section is for anyone deciding whether to allow `buildwithnexus` on
managed machines, and for the endpoint-protection teams asked to allowlist
it. `buildwithnexus` is an MIT-licensed, open-source CLI; every release is
built by the public GitHub Actions workflow in this repository.

### What is in a release

Each [GitHub Release](https://github.com/Garretts-Apps/buildwithnexus/releases)
carries, per platform:

- `buildwithnexus-<target>[.exe]`, the binary, with a `.sha256` file.
- A build-provenance attestation (see *Verifying a Release* above).
- `buildwithnexus.cdx.json`, a CycloneDX SBOM of every crate compiled in.

The Windows `.exe` carries a version resource (ProductName `buildwithnexus`,
OriginalFilename `buildwithnexus.exe`) and a manifest that requests
`asInvoker`: it never asks for elevation. Windows releases are
Authenticode-signed once code signing through the SignPath Foundation is
enabled for the project; `Get-AuthenticodeSignature buildwithnexus.exe`
shows whether a given file is signed.

### What it does when it runs

- **Processes it starts:** the shell commands the user approves (or that
  the user's `auto` mode allows), through `cmd.exe /C` on Windows; `git`,
  and `ffmpeg`/`ffprobe` when present; on Windows, `powershell.exe
  -NoProfile` to read the clipboard (`Get-Clipboard`, for image paste),
  `icacls` to restrict its settings and key files to the current user, and `tasklist`/`taskkill` for dev servers it started; the user's own
  hook scripts, with
  `.ps1` hooks run as `powershell.exe -NoProfile -ExecutionPolicy Bypass
  -File <script>`; and `npm install -g buildwithnexus` only when the user
  sets `auto_update` to `"install"`.
- **Network:** the model provider the user configures (for example
  `api.anthropic.com`, `api.openai.com`, or a local Ollama on
  `localhost:11434`); `lite.duckduckgo.com`, and the pages the agent fetches
  when the model uses the web tools (each new host needs the user's approval
  outside `auto` mode); MCP servers the user configures; and a
  daily `registry.npmjs.org` version check unless `auto_update` is `"off"`.
  There is no telemetry or analytics.
- **Files:** its settings, sessions and checkpoints under
  `~/.buildwithnexus` (or `NEXUS_HOME`); pasted images as `bwn-paste-*.png`
  in the temp directory; and the files the user asks it to edit in the
  working directory.
- **Not done:** no services, scheduled tasks, startup entries, registry
  writes, drivers, or elevation.

Controls an organization can set: `"auto_update": "off"`,
`"permission": "ask"` or `"readonly"`, `BWN_SKIP_INSTALL=1` (no first-run
download), and `BWN_BIN` (run a binary IT placed and verified itself).

### Why endpoint protection may block it

EDR products such as CrowdStrike Falcon and Microsoft Defender score
unknown executables on reputation and behavior. `buildwithnexus` is new and
has few installs, and until Authenticode signing is enabled its `.exe` is
unsigned. The npm launcher also downloads the binary on first run and then
executes it, which is the same shape as a malware loader even though the
download is checksum-verified from this repository's GitHub Release. An
agent that starts shells and PowerShell adds to the score. A block therefore
does not by itself indicate a problem with the file: compare its SHA-256 and
attestation with the release.

When the binary cannot start, the npm launcher (Node, not the binary) prints
the file's path and SHA-256 for the request below. On Windows it also names
the product that most likely blocked it by checking whether these folders
exist: `%ProgramFiles%\CrowdStrike`, `%SystemRoot%\System32\drivers\CrowdStrike`,
`%ProgramFiles%\SentinelOne`, `%ProgramFiles%\Cylance`, `%ProgramFiles%\Confer`
and `%SystemRoot%\CarbonBlack` (Carbon Black), and `%ProgramFiles%\Windows
Defender Advanced Threat Protection` and `%ProgramFiles%\Windows Defender`. It
only reads their attributes, only after a failed start, and starts no process
and queries no service.

### Allowlisting

- **By hash:** allow the SHA-256 of the exact release binary (from its
  `.sha256` file or `Get-FileHash`). In CrowdStrike Falcon this is a custom
  IOC with action *Allow*. Each release has a new hash.
- **By signer:** once releases are signed, allow the signing certificate
  instead, so updates keep working (AppLocker or WDAC publisher rules, or an
  EDR certificate exclusion).
- **By path:** if policy allows, place the verified binary in a fixed
  location (for example `C:\Program Files\buildwithnexus\buildwithnexus.exe`)
  and set `BWN_BIN` to it, so nothing is downloaded to the user profile.

A request an employee can send to their IT team:

```text
Please review and allowlist buildwithnexus <version>, an open-source (MIT)
coding CLI.
- File: buildwithnexus-x86_64-pc-windows-msvc.exe (installed as buildwithnexus.exe)
- SHA-256: <from the .sha256 file in the release>
- Source and release: https://github.com/Garretts-Apps/buildwithnexus/releases/tag/v<version>
- Provenance: gh attestation verify <file> --repo Garretts-Apps/buildwithnexus
- SBOM: buildwithnexus.cdx.json in the same release
- What it does: https://github.com/Garretts-Apps/buildwithnexus/blob/main/SECURITY.md#for-it-and-security-teams
```

## Code Signing Policy

Windows release binaries are to be Authenticode-signed through
[SignPath.io](https://signpath.io), with a free code-signing certificate
from the [SignPath Foundation](https://signpath.org) for open-source
projects. Signing is being set up; until it is active, releases ship
unsigned and are verified with their checksum and attestation instead.

- Only binaries built by `.github/workflows/release.yml` from this
  repository's `main` branch are submitted for signing, and each signing
  request is approved by hand.
- Committers and reviewers: [@geaglin](https://github.com/geaglin).
  Approver: [@geaglin](https://github.com/geaglin).
- Signed binaries are built only from this repository's source and
  third-party crates listed in the release SBOM.
- Privacy: see *What it does when it runs* above. The program sends nothing
  to the project; it talks only to the services the user configures, and
  to the npm registry for the update check unless that is turned off.

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
- The release jobs that compile code (and so run dependency build scripts)
  hold a read-only token; a separate job that compiles nothing attests the
  binaries and uploads them.
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
`~/.buildwithnexus`, is read-only, and so are the workspace's `.git` and
`.buildwithnexus` (on Linux an empty read-only placeholder stands in when
they don't exist yet, so a command cannot create a `.git/config` that runs
later outside the sandbox). On Linux, `/tmp` is a fresh private
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
