---
name: bwn-field
description: Load before giving the buildwithnexus maintainer any commands to run on their own machine (install, setup, upgrade, triage or diagnostic steps; WSL, Linux, macOS, Windows, PowerShell or cmd blocks; bwn launch lines), and before calling a platform, install or release fix "done". Rehearses every block in a matching clean cell first, so the maintainer never tests for us.
---

# bwn field: rehearse before you hand over commands

Most of the 31 issues the maintainer found on 09-24..09-29 came through chat:
one 7-step script (U07), `npm config set prefix` under nvm (U10), winget on
Server 2022 (U25), an empty `$v` (U26), Machine-scope PATH without elevation
(U27), a silent msiexec (U28), Defender-first triage on a CrowdStrike box
(U30). None of those commands had been run anywhere. This skill makes us run
them first.

**Hard rule.** The maintainer does not test. They will not run checks on their
CrowdStrike-managed VM, eyeball Windows Terminal, time their GPU, log in to
npm, click "Run workflow" or promote a release. Never ask them to. You may ask
about their environment (step 0). Anything CI cannot reproduce is reported as
information ("Not rehearsed: ..."), never turned into a task for them.

## 0. Deliverable and environment

1. Write one line: `Deliverable: <what they will have, on which machine, as which user, in which shell>`. Read their request against it before writing any block (U06: we built the wrong thing).
2. Learn the environment by asking (AskUserQuestion). You may also offer the read-only diagnostic block below as an optional shortcut ("or paste the output of this, if that is easier"). Never make it a step: don't wait on it, and don't ask them to run anything else before rehearsal. Ask for:
   - OS and edition: Windows Server 2022 vs Windows 11; Server Core vs Desktop Experience; WSL distro and version; macOS version.
   - Shell: Windows PowerShell 5.1 vs PowerShell 7, bash, zsh, cmd.
   - Elevation on Windows: being in Administrators is not the same as running elevated (U27). The diagnostic prints both (`adminGroup`, `elevated`).
   - Security software: which product runs (CrowdStrike Falcon, Defender, SentinelOne, ...). Defender cmdlets come only after that, and only when Defender is the one (U30).
   - Node manager (nvm, nvm-windows, system package, MSI), and CPU/GPU when model speed matters (U09).
3. Anything unanswered means the hardest case: Windows Server 2022, PowerShell 5.1, not elevated, CrowdStrike present, no winget, nvm on WSL, CPU only. Rehearse for that and go on.

Optional diagnostic for Windows (the same block works in PowerShell 5.1 and 7):

```powershell
# bwn diagnostic: read-only, changes nothing
$os = Get-CimInstance Win32_OperatingSystem
$cv = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
"os=$($os.Caption) build=$($os.BuildNumber) type=$($cv.InstallationType)"
"shell=$($PSVersionTable.PSEdition) $($PSVersionTable.PSVersion) wt=$([bool]$env:WT_SESSION) policy=$(Get-ExecutionPolicy)"
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$elevated = (New-Object Security.Principal.WindowsPrincipal $id).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
"user=$($id.Name) adminGroup=$([bool](whoami.exe /groups | Select-String 'S-1-5-32-544')) elevated=$elevated"
foreach ($s in 'CSFalconService', 'csagent', 'WinDefend', 'Sense', 'SentinelAgent', 'CylanceSvc', 'CbDefense') {
    $st = sc.exe query $s | Select-String 'STATE'
    if ($st) { "security=$s $(($st.Line.Trim() -split '\s+')[-1])" }
}
foreach ($c in 'winget', 'node', 'npm', 'nvm', 'git', 'ollama', 'ffmpeg', 'rg', 'bwn') {
    $g = Get-Command $c -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($g) { "$c=$($g.Source)" } else { "$c=missing" }
}
$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
"cpu=$($cpu.Name) threads=$([Environment]::ProcessorCount) ramGB=$([math]::Round($os.TotalVisibleMemorySize / 1MB, 1))"
"gpu=$((Get-CimInstance Win32_VideoController | ForEach-Object { $_.Name }) -join '; ')"
"diagnostic done"
# expect: diagnostic done
```

Optional diagnostic for WSL, Linux and macOS (bash):

```bash
# bwn diagnostic: read-only, changes nothing
(. /etc/os-release 2>/dev/null && echo "os=$PRETTY_NAME"); uname -srm; sw_vers 2>/dev/null | tr '\n' ' '
if grep -qi microsoft /proc/version 2>/dev/null; then echo "wsl=yes distro=${WSL_DISTRO_NAME:-?}"; else echo "wsl=no"; fi
echo "init=$(cat /proc/1/comm 2>/dev/null) libc=$(ldd --version 2>&1 | head -n 1)"
echo "shell=$SHELL user=$(id -un) uid=$(id -u) groups=$(id -Gn | tr ' ' ',')"
echo "node=$(command -v node || echo missing) $(node -v 2>/dev/null) nvm_dir=${NVM_DIR:-none} npm_prefix=$(npm config get prefix 2>/dev/null)"
for c in curl git rg ffmpeg ollama bwn docker; do printf '%s=%s ' "$c" "$(command -v "$c" || echo missing)"; done; echo
echo "cpus=$(getconf _NPROCESSORS_ONLN) mem=$(awk '/MemTotal/ {printf "%.1fG", $2/1048576}' /proc/meminfo 2>/dev/null) dxg=$(test -e /dev/dxg && echo yes || echo no) nvidia=$(nvidia-smi -L 2>/dev/null | head -n 1)"
echo "term=$TERM wt=${WT_SESSION:+yes} cols=$(tput cols 2>/dev/null)"
echo "diagnostic done"
# expect: diagnostic done
```

## 1. Reuse verified docs blocks

- First choice: copy a block verbatim from buildwithnexus.dev (the buildwithnexus-docs repo), README.md or SECURITY.md, provided it has passed rehearsal in the matching cell. Verbatim means no "small tweaks": a tweak is a new block.
- A command anyone else would also need (install, prerequisites, PATH, keeping Ollama running, EDR triage) goes into the docs first, as a docs PR. Rehearse it there, then quote it. Chat-only blocks are for one-off situations.

## 2. Write the blocks

- Number the blocks. Each does one thing and stands alone: it must not rely on a variable, `cd`, `$env:` change or background process from an earlier block. Treat every block as running in a new window (U07, U08, U26).
- End every block with a check, followed by an `# expect: <literal text>` comment inside the fence. The check's output has to contain that text. When failure is the expected result, add `# expect-exit: N` (or `any`).
- No multi-step scripts, no `set -e` ... `exec`, no `exit` (a pasted `exit` closes their window).
- Windows:
  - No winget on Server. Where it might exist (Windows 11), guard it with `Get-Command winget` and fall back to a direct download (U25).
  - Use User-scope `SetEnvironmentVariable` unless step 0 showed `elevated=True` (U27).
  - Run msiexec as `$p = Start-Process msiexec.exe -ArgumentList ... -Wait -PassThru`, check `$p.ExitCode` (0, or 3010 for reboot needed), then refresh `$env:Path` from the registry inside the same block (U28). The Node.js MSI is per-machine, so it needs elevation: for someone in Administrators who is not elevated, add `-Verb RunAs` (one UAC prompt); with no admin rights at all, say the MSI needs an admin. Install Node from the MSI, not the zip. The MSI's built-in npmrc sets npm's global prefix to `%APPDATA%\npm`; the zip has none, so `npm i -g` installs into the versioned Node folder and the next Node upgrade drops `bwn`.
  - In PS 5.1, wrap `Invoke-RestMethod` in parentheses before piping it: `(Invoke-RestMethod URL) | Where-Object ...`. Without them the JSON array goes down the pipeline as one object (U26).
  - After a PATH change, tell them to open a new window, and make the next block's check prove the change took.
  - For triage, identify the security product first (step 0 list); use Defender cmdlets only when WinDefend or Sense is the one running (U30).
  - npm's `npm.ps1` shim needs execution policy RemoteSigned or looser. Windows 11 defaults to Restricted.
- Linux/WSL:
  - Never `npm config set prefix` under nvm (U10).
  - A `nohup ollama serve &` dies when the window closes. Any block that depends on it checks readiness itself (for example `curl -sf localhost:11434/api/version`) and says how to restart it (U08).
- Launch lines (`bwn ...`): name the mode bwn starts in and what the first screen shows. Rehearse the exact line, or list it under Not rehearsed (U15).

## 3. Lint

Write the draft to a scratch file and run:

```bash
node scripts/field/lint-blocks.mjs --chat draft.md
```

Fix every error, and never silence a rule. `--list-rules` prints each rule with its fix. If the script isn't on your checkout yet, check the draft against section 2 by hand and write "lint unavailable" in the Not rehearsed line.

## 4. Rehearse every block, yourself

Extract exactly what you will send:

```bash
scripts/field/run-blocks.sh --extract draft.md bash > blocks-linux.txt
scripts/field/run-blocks.sh --extract draft.md powershell > blocks-win.txt
```

Each block runs in a new login shell or a new powershell.exe, with a fresh environment and stdin closed. The runner checks the expects, stops at the first failure, and reports exit code, time and output per block.

**Linux/WSL: local docker, in the image that matches their distro.** `FIELD_SETUP` runs as root before the blocks. Use it to give the cell what their machine already has, and nothing more: WSL Ubuntu has sudo and curl, and their user has nvm if step 0 said so (`FIELD_USER` names the block user). The proxy and CA lines exist only because this cloud container reaches the internet through a TLS-intercepting proxy.

```bash
docker run --rm --network host -e HTTPS_PROXY -e https_proxy -e NO_PROXY -e no_proxy \
  -v /root/.ccr/ca-bundle.crt:/ccr-ca.crt:ro -e NODE_EXTRA_CA_CERTS=/ccr-ca.crt \
  -v "$PWD":/repo:ro -v "$SCRATCH":/work -e BLOCKS_FILE=/work/blocks-linux.txt \
  -e FIELD_OUT=/work/out -e FIELD_CELL='ubuntu:22.04 WSL-like + nvm' \
  -e FIELD_SETUP='apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq sudo curl ca-certificates >/dev/null && cp /ccr-ca.crt /usr/local/share/ca-certificates/ccr.crt && update-ca-certificates >/dev/null && runuser -u "$FIELD_USER" -- bash -c "cd && curl -fsSL https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.3/install.sh | bash && . ~/.nvm/nvm.sh && nvm install 22"' \
  ubuntu:22.04 bash /repo/scripts/field/run-blocks.sh
```

**Windows Server 2022 and macOS: dispatch field-adhoc yourself.** Dispatching, waiting and reading the result are all your job. Use `gh`, or the GitHub MCP tools `actions_run_trigger`, `actions_list` and `get_job_logs`:

```bash
gh workflow run field-adhoc.yml -R Garretts-Apps/buildwithnexus -f os=windows-2022 \
  -f shell=powershell -f elevated=false -f blocks="$(base64 -w0 blocks-win.txt)"
gh run list -R Garretts-Apps/buildwithnexus -w field-adhoc.yml -L 1 --json databaseId,createdAt,status
gh run watch <id> -R Garretts-Apps/buildwithnexus --exit-status; gh run view <id> -R Garretts-Apps/buildwithnexus --log
```

- Choose the cell from step 0. `shell=powershell` is PS 5.1. With `elevated=false` the blocks run as a new standard local user, and `elevated=true` runs them as admin. If their elevation is unknown, rehearse both. For macOS use `os=macos-15 shell=bash`; their login shell is probably zsh, so say so in Not rehearsed.
- The dispatch payload is capped at about 65 KB, so split long drafts across runs.
- When the report says "admin only", the standard user could not be created or started on the runner. Say "rehearsed as admin only" and give the reason from the report.
- If you cannot dispatch at all (no `gh` and no MCP, a 403, or the workflow is not on the default branch yet), do not ask the maintainer to run it. Send the blocks marked `Not rehearsed on Windows: <exact reason>`.
- The windows-2022 runner differs from their VM, and the report's Cell line shows how. It preinstalls developer tools (Node, Git, pwsh, 7-Zip, Chocolatey), so a Node install there upgrades an existing one rather than starting fresh. It also sets `npm_config_prefix=C:\npm\prefix` machine-wide, which overrides the MSI's npmrc, so `npm config get prefix` reads `C:\npm\prefix` there: never expect `%APPDATA%\npm`. It runs Defender and no third-party EDR. UAC is off, so a filtered admin token cannot be reproduced. It has no Windows Terminal and no GPU. Every one of these that matters for the blocks goes in the Not rehearsed line.

A failing rehearsal means fixing the block and rehearsing again. Never send a block whose rehearsal is red.

## 5. Send

- Put the deliverable line first. Then give each block with its number, purpose and measured time, for example `3. Install Node LTS (41 s on windows-2022)`.
- Then one line saying where it was rehearsed, with the run URL or cell and the pass count: `Rehearsed: windows-2022, Windows PowerShell 5.1, standard user (run <url>), 5/5 pass; ubuntu:22.04 + nvm in docker, 3/3 pass.`
- Then `Not rehearsed:` listing what the cells cannot show. For example: CrowdStrike Falcon's behavioral block (the runner has only Defender), Windows Terminal rendering and Sixel, your GPU speed, your existing ~/.npmrc and PATH. Present it as information, not a request.
- Never close with "let me know if it works" or "run this and send me the output". They report what they see on their own.

## 6. A reported problem: reproduce first

1. Rebuild their situation in the matching cell: same image or runner, same shell, same elevation, and the same commands they ran, taken from their message. Rehearse it with the observed error as the expectation (`# expect: <their error text>`, `# expect-exit: any`). If it doesn't go red, you haven't understood the problem yet.
2. Fix the cause. Rerun the same blocks, and they should now pass.
3. Keep it caught: add a check and a backtest row (field/backtest.yml when it exists), a lint rule, a launcher test or a matrix assertion, so the same class of problem goes red in CI.
4. If the problem cannot be reproduced (a real Falcon block, their GPU), say so, and put a guard where CI can reach it (for example, launcher guidance that names the product).

## 7. Walkthroughs: list the surfaces first

Before any walkthrough, UX run or "I tested it", list the surfaces:
- CLI commands (`bwn --help`), modes (chat, BRAINSTORM, PLAN, BUILD), slash commands
- install channels (npm, crates.io, release asset, `BWN_BIN`)
- OS and libc, shell, privilege, terminal (Windows Terminal, conhost, xterm, pyte)
- model axis (tools or no tools, vision, slow CPU)

Mark each row walked or not walked, with the reason. Claim coverage only for walked rows (U04).

## 8. Definition of done for a fix

A platform, install or release fix is done only when all of these are true and you have checked each one yourself:

1. CI on main is green for the merge commit.
2. A release.yml run exists for that commit and succeeded. A merge with no release run is not done (U22).
3. The pre-publish field gate for the packed npm tarball is green (Linux cells, windows-2022 PS 5.1, macos-15).
4. npm `latest` and crates.io both show the version (`npm view buildwithnexus dist-tags.latest`; `curl -sS -H 'User-Agent: bwn-field' https://crates.io/api/v1/crates/buildwithnexus | jq -r .crate.max_version`).
5. The post-publish matrix is green for the affected cells.
6. The docs (buildwithnexus.dev, README, SECURITY.md) describe the new behavior or command (U05).

Publishing is automatic, so never ask for an npm login, a dist-tag move or a workflow click. Then tell the maintainer the result with links. If any item is red, the fix is not done: say which item and what you are doing about it.

## 9. Worked example: 09-29, Windows Server 2022

What we sent at 21:51 (installs in "admin" PowerShell), each block unrehearsed:

```text
$v = (Invoke-RestMethod https://nodejs.org/dist/index.json | Where-Object lts | Select-Object -First 1).version; $v
Invoke-WebRequest -UseBasicParsing "https://nodejs.org/dist/$v/node-$v-x64.msi" -OutFile node.msi
Start-Process msiexec.exe -ArgumentList '/i','node.msi','/qn' -Wait
[Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path','Machine') + ';C:\tools\ffmpeg\bin;C:\tools\ripgrep', 'Machine')
```

What went wrong: the earlier winget step failed. `$v` came out empty in PS 5.1 (404). The MSI failed silently because the shell was not elevated. Machine PATH access was denied. Later, Defender-first triage printed nothing on a Falcon machine.

What step 0 would have shown: `type=Server`, `shell=Desktop 5.1`, `adminGroup=True elevated=False`, `security=CSFalconService RUNNING`. So PATH changes use User scope, the MSI is started elevated with `-Verb RunAs` (one UAC prompt) and its exit code checked, and triage starts from Falcon. Not the zip: it would skip the prompt, but npm globals would land in the versioned Node folder (section 2). The shape of the fix follows. It is an example, not a verified docs block, so rehearse it before reuse.

```powershell
# 1. Node.js LTS from the official MSI (per-machine: one UAC prompt)
$ProgressPreference = 'SilentlyContinue'
$rel = (Invoke-RestMethod https://nodejs.org/dist/index.json) | Where-Object lts | Select-Object -First 1
$msi = "$env:TEMP\node-$($rel.version)-x64.msi"
Invoke-WebRequest -UseBasicParsing "https://nodejs.org/dist/$($rel.version)/node-$($rel.version)-x64.msi" -OutFile $msi
$p = Start-Process msiexec.exe -ArgumentList "/i `"$msi`" /qn /norestart" -Verb RunAs -Wait -PassThru
if ($p.ExitCode -ne 0 -and $p.ExitCode -ne 3010) { throw "msiexec failed with exit code $($p.ExitCode)" }
$env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' + [Environment]::GetEnvironmentVariable('Path', 'User')
Write-Output "node $(node -v), npm global prefix $(npm.cmd config get prefix)"
# expect: node v
# expect: \npm
```

Then: "Open a new PowerShell window (PATH is read when a window opens), then:"

```powershell
# 2. Check Node and npm from a new window
"node " + (node -v) + " npm " + (npm -v)
# expect: node v
```

The message ends like this: `Rehearsed: windows-2022, Windows PowerShell 5.1, as admin because the MSI needs it (run <url>), 2/2 pass, <n> s. Not rehearsed: the UAC prompt and a non-elevated window (UAC is off on the runner); CrowdStrike Falcon (the runner has Defender only); the runner already had Node, so the MSI upgraded it rather than installing fresh; the %APPDATA%\npm global prefix (the runner forces C:\npm\prefix).`
