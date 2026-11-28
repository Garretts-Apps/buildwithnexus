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

## Past Disclosures

- **Anthropic API key in git history.** Commit `4c158d1` (2026-03-16, shipped
  around 0.6.11) added a `.env.local` holding an Anthropic API key. The key
  was revoked; revocation confirmed 2026-09-30. The commit remains in the
  public history, and the key in it no longer works. CI now scans every pull
  request and push for committed secrets (gitleaks).

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
binary downloaded on an earlier run, then a local `cargo build` in a repo
checkout. The current launcher does not look for the binary in any npm
package, so in practice the binary comes from the first-run download below.

**Old versions that name unpublished packages.** Versions 0.12.1 to 0.14.2
list `buildwithnexus-<os>-<cpu>@<version>` in `optionalDependencies`, but no
such package was ever published, so the names are not registered on npm.
Publishing from this project is OIDC-only, which stops this project's own
workflow from creating them by accident; it does not stop anyone else from
registering those names, and an install of one of those old versions would
then fetch that person's code. The current launcher and package no longer list
or look for those names. If you have 0.12.1 to 0.14.2 installed, upgrade to
the latest version (`npm install -g buildwithnexus@latest`), and do not
install those versions.

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
publishing is refused. If the package's `checksums.json` has no entry for
this platform's asset, nothing is downloaded. (A source checkout, which has
no `checksums.json`, falls back to the release's own `.sha256` file, which
only catches corruption.) Each first run downloads to a temp file of its
own, so two first runs at once cannot mix their bytes. On a platform with
no prebuilt binary, or on musl or a glibc below the floor, the launcher
explains that and downloads nothing.

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

The binary is downloaded to `~/.buildwithnexus/bin/<version>/` (under
`NEXUS_HOME` if that is an absolute path), outside the npm package, so an npm
update or reinstall does not delete it; `BWN_INSTALL_IN_PACKAGE=1` keeps the
pre-0.15 location, the package's own `bin/`. Each verified download is
recorded in `.installed.json` next to it (version and
SHA-256) as soon as its checksum matches, before it is moved into place, so a
file that security software quarantines on write counts too. If that record
is there but the binary is not, the launcher does not download it again on its
own, because whatever removed it would remove the next copy too. Once the file
is allowed, `bwn --bootstrap` downloads it again.

A request that gets no data for 30 s fails with the reason, including while
it waits for a proxy. The download goes through the proxy in `HTTPS_PROXY`
(or `HTTP_PROXY`; either case) on every supported Node, unless `NO_PROXY`
matches the host. The launcher opens a CONNECT tunnel through that `http://`
proxy for each request and redirect, and checks each one against the GitHub
host list first. TLS runs inside the tunnel to GitHub itself, with Node's usual
certificate checks (`NODE_EXTRA_CA_CERTS` adds a root), so the proxy cannot
read or alter the download. `user:password@` in the proxy URL is sent as
`Proxy-Authorization: Basic`, and no message prints it. With
`NODE_USE_ENV_PROXY=1` on Node 22.21+ or 24.5+, Node's own proxy support is
used instead.

The launcher ignores a relative `BWN_BIN`, and only runs a binary from `BWN_BIN`,
`<home>/bin/<version>/` (where the download goes), the package's own `bin/`
(earlier releases) or a repo checkout's `target/`.

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
  -File <script>`; the OS URL opener (`open`, `xdg-open`, or `rundll32
  url.dll,FileProtocolHandler`) for `open_browser` and for the sign-in page
  of `buildwithnexus mcp login`; `pdftotext` (poppler), when installed, to
  read a PDF the model opens; a local Chrome, Chromium or Edge in headless
  mode for `screenshot_url` (only for models that take images, with a
  throwaway profile in the temp directory, deleted afterwards, and without
  provider keys in its environment); and `npm install -g buildwithnexus` only when the user
  runs `buildwithnexus update` or sets `auto_update` to `"install"` or
  `"install-any"`. It never installs other software: `buildwithnexus doctor`
  prints the install command for a missing tool, once, and runs none. Commands the
  agent runs do not inherit credentials from the environment: variables whose
  names end in `_API_KEY`, `_API_TOKEN`, `_TOKEN`, `_SECRET`, `_SECRETS` or
  `_PASSWORD`, every `AWS_*` variable, and `HF_TOKEN` and the other provider
  presets' key variables. The user can list any of them in
  `shell_env_passthrough` to keep it. MCP servers started over stdio get
  the same scrubbed environment, plus whatever their own
  `mcp_servers.<name>.env` entry sets. A command that runs past its timeout,
  or that the user interrupts, is killed with everything it started (its
  process group on Linux and macOS, its process tree on Windows), and
  commands run without a controlling terminal, so one that tries to prompt
  on `/dev/tty` fails at once.
- **Network:** the model provider the user configures (for example
  `api.anthropic.com`, `api.openai.com`, or a local Ollama on
  `localhost:11434`); `lite.duckduckgo.com` for web searches, and the pages
  the agent fetches when the model uses the web tools (each new host,
  including the search host, needs the user's approval outside `auto` mode;
  in `auto` mode a fetch of `localhost`, a loopback, private (RFC 1918),
  CGNAT or IPv6 unique-local address, or of a name that resolves to one,
  still asks first, and a headless run refuses it, unless `network.allow` or
  an allow rule names the host; link-local and cloud-metadata addresses are
  refused in every mode, including their IPv4-compatible and NAT64 forms);
  MCP servers the user configures, and the OAuth authorization servers they
  name when the user runs `mcp login` (only over HTTPS, or to this machine;
  sign-in tokens are sent to nothing else); the URLs of `http` hooks the user
  configures (a project's only once the folder is trusted; `network.deny`
  applies; no redirects are followed); pages on this machine that
  `screenshot_url` opens in the headless browser (another host only when
  `network.allow` or an allow rule names it; every other request the page or
  the browser makes, link-local and cloud-metadata addresses included, goes
  to a local proxy inside bwn that refuses it); and a daily `registry.npmjs.org` version
  check unless `auto_update` is `"off"` (`BWN_UPDATE_REGISTRY` or
  `npm_config_registry` points it at a mirror). There is no telemetry or
  analytics. All of these go through the proxy named by `HTTPS_PROXY`,
  `HTTP_PROXY` or `ALL_PROXY` unless `NO_PROXY` matches the host; loopback
  addresses never do, and neither do model servers on private, link-local,
  CGNAT or IPv6 unique-local addresses unless `BWN_PROXY_PRIVATE=1`. Certificates are checked
  against the bundled webpki roots plus the OS certificate store, or plus
  `SSL_CERT_FILE`/`SSL_CERT_DIR` when set, so a TLS-inspecting proxy with an
  installed root works. Through a proxy, the proxy resolves host names, so
  the web tools' check that a name does not resolve to a link-local or
  metadata address is the proxy's to make; bwn still refuses those
  addresses and the metadata host names given literally, at every redirect
  hop.
- **Files:** its settings, keys (`.env.keys`), MCP sign-ins
  (`mcp-auth/<server>.json`, owner-only like the key file, each bound to its
  server's URL), sessions, checkpoints,
  traces, conversation exports (`exports/`), which upgrade notices were shown
  (`notices.json`) and whether each saved key passed its last check
  (`key-checks.json`, a hash, never the key) under `~/.buildwithnexus` (or
  `NEXUS_HOME`), and there, in `bin/<version>/`, the binary the npm launcher
  downloaded; pasted images as `bwn-paste-*.png` in the temp directory; the
  files the user asks it to edit in the working directory and in folders
  added with `--add-dir` or `/add-dir`; and, with
  `--worktree <name>`, a git worktree in `.bwn/worktrees/<name>` (listed in
  the repository's `.git/info/exclude`).
- **Keys for custom endpoints:** a key for an OpenAI-compatible endpoint is
  saved for that endpoint only (`CUSTOM_API_KEY@<scheme://host[:port]>`),
  read the way the HTTP client reads the URL, and is never sent to another
  address. A single key saved by 0.14 is tied to the custom endpoint in the
  user's own settings, or else kept unbound and never sent until the user
  says which endpoint it belongs to. `CUSTOM_API_KEY` from the environment
  goes only to the custom endpoint the run started on.
- **MCP sign-ins:** tokens refresh on their own and are sent only in the
  `Authorization` header to the server URL they were saved for.
  `buildwithnexus mcp logout` asks the authorization server to revoke the
  token and deletes the file. A token a server echoes back is replaced with
  `[redacted]` before the model, the transcript or the terminal sees it.
- **Editors (`buildwithnexus acp`):** it runs only when an editor starts it,
  and talks to that editor on stdin and stdout. Permission modes, rules,
  hooks, folder trust and the sandbox apply as in the terminal; approvals
  become questions in the editor. The protocol's output pipe is not passed
  to commands, hooks or MCP servers, so they cannot send the editor requests
  (such as writing a file) of their own.
- **GitHub Action (`action.yml`):** it installs bwn from npm, defaults to
  `--permission-mode readonly`, passes its inputs to its scripts as
  environment variables, gives the token only to the step that posts the
  pull request comment, and updates only a comment written by that token's
  own account.
- **Not done:** no services, scheduled tasks, startup entries, registry
  writes, drivers, or elevation.

Folder trust is explicit. A project's `base_url` (where requests and the key
go) and `permission` take effect from a `--trust-project` digest only when
`--trust-project-allow` (or `BWN_TRUST_PROJECT_ALLOW`) names them; a
`permission` of `readonly` only tightens and needs no name. The digest covers
skills from a `skill_dirs` the project settings add. A repository's AGENTS.md
and CLAUDE.md enter the prompt only after a `y` for that folder and content.
The key question refuses answers that cannot be a key (spaces, a leading `/`,
only digits, a provider name, over 4096 characters) instead of sending them
to the provider, and bwn does not ask for a key for plain http to another
machine.

Controls an organization can set: `"auto_update": "off"`,
`"permission": "ask"`, `"accept-edits"` or `"readonly"`, `permissions.deny`
rules (for example `run_command(git push*)`) and `network.deny` hosts, which
refuse in every mode and which a project's settings cannot remove,
`BWN_SKIP_INSTALL=1` (no first-run download), `BWN_BIN` (run a binary IT
placed and verified itself), and `BWN_TLS_ROOTS=bundled` (trust only the
roots built into the binary, not the OS store or `SSL_CERT_FILE`).

A project's policy files (`.buildwithnexus/settings.json` and
`settings.local.json`, `rules/`, `tools/`, `agents/`, `commands/`, `skills/`,
`system.md`, `Agents.md`, and the folder itself) are protected from the file
tools: any change asks a person, in every mode, one change at a time, and no
saved approval or allow rule covers it. Without a terminal to ask, the change
is refused. Shell commands under Landlock or with no backend can still write
them; that gap is roadmap item 2.2.

A broken setting never loosens these. A `sandbox` value bwn can't parse runs
as `require`, and a `permission` value as `readonly`, each with a warning
naming the fix. A settings file that exists but doesn't load (bad JSON, a
top level that isn't an object, or a file bwn can't read), the user's or a
project's, stops bwn from starting, so its deny rules can't be skipped. If
one stops loading during a session, every tool call and every connection a
sandboxed command makes is refused until it loads again.

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
| `"install"` | Daily check plus silent background `npm install -g` of patch releases within the running minor version; notice on the next launch. A new minor or major version is announced with its install command, never installed on its own. |
| `"install-any"` | As `"install"`, but installs any newer version (what `"install"` did before 0.15). |

`BWN_NO_AUTO_UPDATE=1` is honored for back-compat and caps `"install"` and
`"install-any"` to `"notify"`. The check runs inside the CLI (not the npm wrapper), never blocks
startup, and installs performed by other means (cargo, source builds) are
never auto-updated.

## Hardening Inside the Package

- Publishing uses npm's **OIDC Trusted Publisher** flow and crates.io
  **Trusted Publishing**, so no long-lived registry tokens are needed for
  releases, and the publish workflows read no registry token.
- npm publish is **gated on the release workflow succeeding**, so a package
  version can never point at binaries that don't exist. Before anything is
  published, every release binary is started once on the machine that
  built it and must report the version being released. The order is then:
  the GitHub release becomes public, the npm tarball packed from it is
  installed in clean containers, and only when that passes are crates.io
  and then npm published, so a package that does not install reaches
  neither registry.
- A pushed release tag must match `package.json` and be on `main`. A manual
  release run for a version that is already tagged rebuilds nothing (new
  binaries would not match the hashes pinned in the published npm package);
  it only retries the crates.io publish. The npm publish workflow can be
  started by hand only from `main`.
- Every third-party GitHub Action is pinned to a full commit SHA (the
  version is in a trailing comment), and tools the workflows download are
  pinned to a version and checked against a recorded SHA-256. Dependabot
  proposes updates weekly for Actions, npm and Cargo.
- Workflows run with least-privilege permissions: `permissions: {}` at the
  workflow level, with each job granted only what it uses. `id-token: write`
  is granted only to the jobs that sign attestations or exchange OIDC tokens
  with npm and crates.io. Checkouts do not keep the `GITHUB_TOKEN` in
  `.git/config`.
- The release jobs that compile code (and so run dependency build scripts)
  hold a read-only token; a separate job that compiles nothing attests the
  binaries and uploads them.
- Releases run one at a time, a manual release can only be started from
  `main`, and a newly created tag points at the commit the workflow built.
- Inside the harness itself: mutating file tools are gated by the permission
  model, sensitive paths and well-known destructive commands (a best-effort
  pattern check, not a guarantee) require confirmation even in `auto` (also
  when they are the command of `check_work` or `start_server`),
  API keys are refused over non-HTTPS endpoints, typed hidden and saved only
  after the provider accepts them, key-like tokens are redacted from surfaced
  errors, provider keys and other credentials (tokens, secrets, passwords,
  `AWS_*`) are removed from the environment of the commands
  the agent runs and provider keys are blanked in bwn's own environment block (which
  `/proc/<pid>/environ` shows), any process's `/proc/<pid>/environ` is a
  sensitive path, descriptors bwn inherited (a CI runner's pipe) reach no
  command it starts, a tool call quoted inside an answer never runs, and an
  isolated helper's worktree runs no repository hooks.

## Permission Gates and the Optional Sandbox

`ask` / `accept-edits` / `auto` / `readonly` modes, allow, ask and deny
rules, protected paths, and checkpoints are guardrails against mistakes. They are **not OS-level isolation**. An approved
command runs with your user's permissions, and checkpoints can rewind file
edits in the working tree but not network calls, pushed commits, published
packages, or other external effects.

Read-only calls the model makes in a row (reads, searches, fetches) may run
at the same time (`max_parallel_tools`, default 4). Their checks and
approval prompts still happen one by one, in order, before any of them
runs; a refused call is never run, and nothing that writes, runs a command
or asks a question is ever run beside another call.

Shell commands (`run_command`/`bash`, `!cmd`, and `check_work`) run inside
an OS sandbox by default since 0.21. Set it with the `"sandbox"` setting,
`--sandbox <mode>`, or `/sandbox <mode>`:

- `auto` (default) confines commands when a backend works, and otherwise runs
  them unconfined and says so once per session (auto mode then asks before
  each command).
- `require` refuses to run commands when no backend works.
- `off` runs them unconfined.

When no backend works and the mode is `auto`, commands are refused until a
person allows them to run unconfined for that session. bwn asks once, at the
first command; no hook, saved approval or allow rule answers that question,
the answer is not saved, and the posture check shows it. With no one to ask
(headless, CI), commands are refused; `--sandbox off` is the explicit way to
run them unconfined. `--accept-posture` does not grant it.

Backends on Linux, in order: `bwrap` (bubblewrap); bwn's own namespace
backend, a user and mount namespace with Landlock and seccomp inside, where
the kernel allows unprivileged user namespaces (Ubuntu 24.04's AppArmor
restriction and `kernel.unprivileged_userns_clone=0` block them); and
Landlock and seccomp alone. WSL skips bubblewrap and starts at the namespace
backend. `sandbox-exec` (Seatbelt) on macOS. Windows has none: commands there are refused until you allow them to
run unconfined for the session, and the posture check and `doctor` say so.

What each backend guarantees, and the test that proves it. "No" is a
documented gap, not a bug report: each one has a roadmap item. With no backend,
commands are refused until allowed (see above), and once allowed nothing in
this table applies.

<!-- capabilities:begin (generated from harness/src/capabilities.rs; do not edit by hand) -->

| Guarantee | bubblewrap | Namespace (built-in) | Landlock alone | Seatbelt | Proven by |
|---|---|---|---|---|---|
| Writes only in the workspace, added folders and temp | Yes | Yes, file metadata included: the rest of the tree is read-only. Plus all of `/dev` and the real `/tmp` | Contents yes — a write, rename, link, create, `mkdir` or remove outside is refused — plus all of `/dev` and the real `/tmp`. A file's metadata outside stays writable (`chmod`, `chown`, `utime`, xattrs), and below ABI 3 truncation is seccomp's (see Older kernels). bwn uses its namespace backend instead where user namespaces are allowed | Yes | `bwrap_confines_writes_to_the_workspace`, `invariant_userns_confines_metadata_outside`, `invariant_userns_keeps_the_workspace_writable_and_git_read_only`, `a_confined_command_can_write_inside_and_not_outside`, `invariant_landlock_confines_write_syscall_paths_outside`, `landlock_does_not_confine_file_metadata_outside`, `seatbelt_profile_denies_writes_except_workspace_and_tmp` |
| `.git` and `.buildwithnexus` read-only | Yes | Yes | No: the posture check says so and auto mode asks before every command (roadmap 2.2) | Yes | `bwrap_argv_keeps_git_and_settings_read_only`, `invariant_userns_keeps_the_workspace_writable_and_git_read_only`, `invariant_au5_the_landlock_policy_gap_is_disclosed_and_auto_asks`, `seatbelt_profile_denies_writes_except_workspace_and_tmp` |
| Nested repositories' `.git` read-only | Up to 64 found in 10,000 folders, skipping `node_modules` and `target` | Same bounds as bubblewrap | No | Same bounds as bubblewrap | `nested_repositories_stay_read_only_too`, `invariant_userns_keeps_the_workspace_writable_and_git_read_only` |
| Credential stores unreadable | Folders yes, contents and names. A hidden file is covered by an empty file, so its contents are hidden but whether it exists still shows | Folders: contents and names (an empty folder covers each store, so a guessed name is not found). A hidden file's contents are hidden, but whether it exists still shows | Contents yes. Listing the folder is refused, so names can't be enumerated, but `stat` of a guessed name confirms it exists and a file's metadata stays writable (bwn's namespace backend closes both where user namespaces are allowed). Listing is blocked by also refusing to list `/`, `/home`, your home folder and `~/.config` (files in them stay readable by name) | Yes | `bwrap_hides_credential_stores`, `landlock_hides_credential_stores`, `invariant_m5_landlock_does_not_list_hidden_store_names`, `landlock_credential_names_leak_via_stat`, `invariant_userns_hides_store_names`, `invariant_userns_command_cannot_undo_the_mounts`, `seatbelt_hides_credential_stores` |
| A store that contains the workspace hidden | No | No (auto mode asks) | No (auto mode asks) | No | `invariant_g3_no_home_folder_means_nothing_is_claimed_hidden` covers the no-home case |
| SSH agent socket unreachable | Yes | Yes while the network is checked or off: only TCP sockets, in the session's private network namespace | Only while the network is checked or off | Yes | `invariant_bwrap_agent_socket_is_unreachable`, `invariant_userns_limits_the_network_to_the_proxy`, `invariant_netns_command_reaches_only_the_checker`, `landlock_limits_the_network_to_the_proxy`, `seatbelt_profile_denies_reads_of_hidden_paths` |
| Network only through the host checker | Yes where bwn can make the session's private network namespace: bubblewrap starts in it, as on the namespace backend. Where it can't (a setuid bubblewrap with unprivileged namespaces off), through the checker held by port, so the checker's port on another address also connects. Only plain TCP sockets. No network if bwn's own binary can't be found | Yes: each command joins the session's private network namespace, which holds only loopback and the checker, on any Landlock ABI. Only plain TCP sockets; no DNS. Through the checker, `localhost` is the namespace, not this machine. Servers a command starts reach later commands and bwn's own tools (fetch, `wait_for_url`, `screenshot_url`), not model providers | No network, unless the user allows it for the session (`/sandbox network`, `--sandbox-network`): Landlock alone holds a command by port only, and no private network namespace is possible here. Once allowed, with Landlock ABI 4 (kernel 6.7), as bubblewrap | Yes | `bwrap_limits_the_network_to_the_proxy`, `invariant_bwrap_netns_command_reaches_only_the_checker`, `invariant_bwrap_netns_screenshot_reaches_a_server_a_command_started`, `landlock_limits_the_network_to_the_proxy`, `invariant_userns_limits_the_network_to_the_proxy`, `seatbelt_limits_the_network_to_the_proxy`, `invariant_m8b_no_network_limiter_means_no_network`, `invariant_sec1_mptcp_socket_is_refused_under_the_proxy`, `invariant_m8c_x32_syscalls_are_refused`, `invariant_landlock_alone_has_no_network_until_allowed`, `invariant_netns_command_reaches_only_the_checker`, `invariant_netns_checker_does_not_reach_host_loopback`, `invariant_netns_holds_when_its_holder_dies`, `invariant_netns_command_cannot_join_the_host_or_read_the_holder`, `invariant_netns_tools_reach_session_servers_and_providers_do_not`, `invariant_netns_screenshot_reaches_a_server_a_command_started` |
| No inbound connections | No: a command can listen | Yes: nothing outside the session's namespace can connect in; a server there answers later commands and bwn's own tools only | No | No | `invariant_netns_servers_persist_and_only_the_session_reaches_them`; not guaranteed elsewhere |
| Provider keys and tokens stripped from the environment | Yes | Yes | Yes | Yes (and with no backend) | `sandboxed_children_lose_sockets_and_provider_keys` |
| Older kernels | n/a | As Landlock alone, except the network: the private namespace holds it on every ABI, so commands get the checker below ABI 4 too. The namespace needs `mount_setattr` (kernel 5.12), which every Landlock kernel has | ABI 1 (5.13 to 5.18, Ubuntu 22.04) and ABI 2 (5.19 to 6.1, Debian 12): writes, credential stores and no-network hold. Landlock can't confine truncation there, so seccomp refuses `truncate(2)` and a read-only `open` with `O_TRUNC`, inside the workspace too (write-mode truncation — `: > file`, `ftruncate`, opening for writing with `O_TRUNC` — still works, because Landlock confines it); the posture check says so. Confirmed on 5.15 and 6.1 | n/a | `a_confined_command_can_write_inside_and_not_outside`, `invariant_m2_a_confined_command_cannot_truncate_outside`, `invariant_m2_seccomp_refuses_truncate_below_abi_3`, `invariant_m2_seccomp_refuses_otrunc_open_below_abi3`, `invariant_userns_confines_metadata_outside`, run in 5.15 and 6.1 VMs |

<!-- capabilities:end -->

The invariant suite (`cargo test -- invariant_`) runs once per backend in CI
and gates every release.

Inside the sandbox, the command can write only to the working directory,
the folders added with `--add-dir` and the temp directories; the rest of the
filesystem, including `~/.buildwithnexus`, is read-only, and so are the
`.git` and `.buildwithnexus` of the workspace and of each added folder, and
the `.git` of repositories nested inside them that exist when the command
starts (on Linux an empty read-only placeholder stands in when they don't
exist yet, so a command cannot create a `.git/config` that runs later outside
the sandbox). That read-only rule holds on bubblewrap, the namespace backend and Seatbelt.
Landlock cannot carve `.git` and `.buildwithnexus` out of the writable
workspace, so on Landlock alone a command can write them; the posture check
says so, and auto mode asks before every command on that backend. On Linux
with bubblewrap, `/tmp` is a fresh private directory that is discarded when
the command exits.

The user's credential stores cannot be read from inside it, whatever path a
command uses to reach them: `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.kube`,
`~/.azure`, `~/.config/gh`, `~/.config/gcloud`, `~/.docker/config.json`,
`~/.netrc`, `~/.npmrc`, `~/.pypirc`, `~/.git-credentials`, `~/.pgpass`,
`~/.buildwithnexus`, and the SSH agent's socket. Bubblewrap mounts empty
read-only stand-ins over them, Seatbelt denies the reads (and connections to
the socket), and Landlock grants reads everywhere else. Landlock cannot hide a
store that lies inside the workspace (a session run from the home folder); the
posture check says so, and auto mode asks before commands there.
`"sandbox_expose"` makes a store readable again.

Commands reach the network only through a checker inside bwn: TCP to its
proxy and nothing else (Landlock ABI 4 and seccomp on Linux, which also
refuse UDP and Unix sockets; a Seatbelt rule on macOS). It lets through ports
443 and 80 to package and source hosts, hosts the user named in a message,
and `network.allow`; private, link-local and metadata addresses are refused
unless `network.allow` names them; this machine's own servers stay reachable.
On bubblewrap and bwn's namespace backend every command joins one private
network namespace made for the session. It holds only a loopback interface and the
checker's listener, so a command cannot reach the internet, the local network
or this machine's own services except through the checker, on any Landlock
ABI. It has no DNS. Through the checker, `localhost` means the namespace, not
this machine.

Servers a command starts stay reachable from later commands and from bwn's own
tools (fetch, `wait_for_url`, `screenshot_url`, which try the namespace first,
then this machine). Model providers on loopback are never dialled there, so a
command cannot stand in for one. Nothing outside the session can connect in.

The namespace is made by a small holder process forked from bwn. It is not
dumpable, so a command cannot read its memory. bwn keeps the namespace open
for the session and replaces a holder that dies.

Where bwn can't make that namespace but bubblewrap still works (a setuid
bubblewrap with unprivileged namespaces off), bubblewrap keeps the host
network held by port. A command there could reach another machine on the
checker's own port, which is random per session. The posture check shows
which applies.

On Landlock alone, where no namespace is possible (Ubuntu 24.04 with
AppArmor's default user-namespace restriction, for example), commands get no
network unless you allow it for the session with `/sandbox network` or
`--sandbox-network`. Without Landlock ABI 4 they get none there. `"sandbox_network": false` blocks it
entirely; `"sandbox_egress_check": false` removes the checker.

Data can still leave through an allowed host (publishing a package, for
example), which is why the credential files are hidden as well.

The sandbox does not confine: other reads (including a project's `.env`),
the agent's own file tools (which are fenced to the working directory and the
added folders by the permission gate instead), hooks and MCP servers.
`start_server` runs inside it when it is on (directly, not in tmux, since a
tmux server would run the command outside it). It never approves anything; it
only limits what an already-approved command can touch. For untrusted work,
use a container or VM.

Before a session's first model request, the posture check shows all of this,
plus what this machine's network does (the model endpoint, name lookup, the
cloud metadata address, listening ports), and asks to accept or stop.
Headless runs need `--accept-posture`. The acceptance is tied to what the
session can reach: the permission and sandbox modes, the added folders, which
credential stores are hidden, and the hosts `network.allow` names. If any of
these changes in the session (`/permissions`, `/sandbox`, `/add-dir`, a typed
switch, an edited settings file), the check asks again before the next model
request. Text from web pages, search results
and MCP servers is labelled as untrusted for the model, and after a session
reads any, auto mode asks before commands, fetches and MCP calls that can
change things.

### What the automatic checks do not guarantee

When the model finishes after editing files without running `check_work`,
bwn runs the project's checks once itself, and a top-level run whose checks
still fail ends with `check_work_failed` (exit 7), not `success`. That is
the limit of it today; roadmap 3.1 makes the checks mandatory:

- **A check that was refused does not fail the run.** If the permission gate
  denies the automatic round, or nobody can approve it (no terminal), the
  checks are not run and the run can still end as `success`. The run prints
  "checks were not run" and the tool record says the round was skipped.
- **Edits are committed before any check.** With `auto_commit` on (the
  default), each file edit is committed when it is made, so a run that ends
  with failing checks leaves those commits in place.
- **The model chooses the check.** `check_work` accepts a command, and its
  exit status is the verdict; a later passing call replaces an earlier
  failure.
- **Shell edits do not trigger the round.** Only file-tool edits count as
  changes, so a run that changes files only through commands is not checked
  automatically.
- **Helpers are not checked.** The automatic round and the verifier run only
  for the top-level agent.
