# Platform support

The rest of the [Platform support](../README.md#platform-support) section.

Linux and macOS are the primary targets. Native Windows (PowerShell, cmd,
Windows Terminal) and WSL are supported with the differences below. Nothing
here needs extra dependencies — Windows integration shells out to the
built-in tools (`powershell.exe`, `cmd.exe`, `icacls`, `tasklist`,
`taskkill`).

Works on native Windows:

- The full TUI (alternate screen, raw mode, mouse, bracketed paste) via
  crossterm. Ctrl+Break, closing the console window, logoff and shutdown
  restore the terminal before the process ends; panics restore it too.
- Ctrl+V paste of a clipboard **image** (PNG via `powershell.exe`
  `Get-Clipboard -Format Image`) and clipboard text (`Get-Clipboard -Raw`).
  WSL uses the same PowerShell path with a base64 round-trip. Windows
  Terminal shows the half-block preview and the OSC 9;4 taskbar progress.
- `~/.buildwithnexus/.env.keys` and `settings.json` are restricted to the
  current user with `icacls /inheritance:r /grant:r` — the ACL equivalent of
  the `0600` mode used on Unix. A failure is reported as a dim warning and
  never blocks the save.
- Background dev servers (`start_server` / `list_servers` / `stop_server`)
  without tmux: liveness via `tasklist /FI "PID eq <pid>"`, shutdown via
  `taskkill /PID <pid> /T /F` (the recorded pid is the `cmd /C` wrapper;
  `/T` takes its children down with it).
- Hook scripts by extension: `.ps1` → `powershell.exe -NoProfile
  -ExecutionPolicy Bypass -File`, `.cmd`/`.bat` → `cmd.exe /C`, `.py` →
  `python3` if it runs, else `python`. `.sh`/`.bash` run with `sh`/`bash`
  from PATH (Git for Windows provides both); without them the hook fails with
  an error naming the missing interpreter instead of a silent `sh` spawn
  failure. Shell-string hooks and the `bash`/`run_command` tools run under
  `cmd /C`.
- `~` and `~/…` (also `~\…`) resolve against `%USERPROFILE%` when `HOME` is
  unset. Temp files use the system temp directory everywhere.

Still Unix-only:

- tmux-backed dev servers (Windows uses the plain background-process path).
- Copy-to-clipboard from the transcript is emitted as OSC 52 on every
  platform; the extra `clip.exe` / `pbcopy` fallback only runs on WSL and
  macOS, so on native Windows it relies on the terminal honouring OSC 52
  (Windows Terminal does).
- Auto-discovery of `~/.buildwithnexus/hooks/<Event>/` scripts still looks
  for `.sh`/`.bash`/`.py`/`.rs` only; `.ps1`/`.cmd`/`.bat` hooks must be
  listed in `settings.json` as `type: "script"`.
- The `python_tool` runner always invokes `python3`.
- `xdg-open`-style helpers, `/proc`-based WSL detection and Unix signal
  handling (`SIGTERM`/`SIGHUP`) have no Windows equivalent beyond the above.

## Managed and corporate machines

Endpoint protection such as CrowdStrike Falcon or Microsoft Defender may
block or quarantine the Windows binary: it is new, has few installs, and the
npm launcher downloads it on first run and then executes it. The file itself
matches the release checksum and attestation. On a managed machine, ask IT to
allowlist it. When a blocked binary cannot start, the launcher prints its path
and SHA-256 and names the endpoint product it finds, and it does not download
the binary again on its own. [SECURITY.md](../SECURITY.md#for-it-and-security-teams) lists what
bwn runs, which hosts it connects to and which files it writes, explains how
to verify a release, and includes a request you can send. Each Windows `.exe`
carries a version resource and an `asInvoker` manifest, and each release has
a CycloneDX SBOM (`buildwithnexus.cdx.json`).

## Behind a proxy

bwn's own connections (model providers, the web tools, MCP servers over
HTTP, the update check) use the standard proxy variables: `HTTPS_PROXY`,
`HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY`, in either case. `NO_PROXY` takes
host names (`corp.example` also covers its subdomains; `.corp.example` and
`*.corp.example` mean the same), IP addresses, CIDR blocks such as
`10.0.0.0/8`, and `*`. The proxy URL must be `http://`, optionally with
`user:password@`; an `https://` proxy URL fails at once. Local model servers
on `localhost`, `127.0.0.1`, `::1` or `0.0.0.0` are always reached directly,
and so are model servers on private, link-local, CGNAT (100.64/10) or IPv6
unique-local addresses, or host names that resolve only to them, as they were
before 0.15 (`BWN_PROXY_PRIVATE=1` sends those through the proxy). A proxy
failure names the proxy and its variable: not answering, 407, rejected
credentials, or a refused tunnel.

HTTPS is checked against bwn's bundled roots plus the operating system's
certificate store, so a TLS-inspecting proxy whose root certificate IT
installed works without extra steps. If the root is not in the OS store, set
`SSL_CERT_FILE` to a PEM file holding it (or `SSL_CERT_DIR` to a directory
of them); these replace the OS store, and the bundled roots still apply. An
untrusted certificate fails at once with `UnknownIssuer` and names these
settings.

To connect as bwn 0.14 did, run it with `NO_PROXY='*'` (no proxy) and
`BWN_TLS_ROOTS=bundled` (bundled roots only).

The npm launcher's first-run download uses the same variables on every
supported Node (18 and later): `HTTPS_PROXY` or `HTTP_PROXY` (an `http://`
URL, optionally with `user:password@`, percent-encoded), and `NO_PROXY` with
`*`, domains (which also cover their subdomains) and `host:port`. It tunnels
through the proxy to GitHub, and TLS is checked against GitHub's certificate
as on a direct connection: if the proxy inspects TLS, point
`NODE_EXTRA_CA_CERTS` at its root certificate (a PEM file). A failed download
names the proxy and says whether it needed credentials, refused the tunnel or
did not answer, and `bwn --bootstrap` tries again.

