<#
Rehearses the command blocks we hand the maintainer, on a Windows runner:
each block runs in a fresh process of the requested shell, as a standard
local user unless elevated is asked for, and the report gives exit code,
duration, stdout/stderr and the "# expect:" check per block.

Used by .github/workflows/field-adhoc.yml (windows-2022). Linux and macOS use
run-blocks.sh, which has the same block format and report.

Must stay Windows PowerShell 5.1 compatible: no ?? or ?: operators, no &&
or || pipeline chains, no PowerShell 7-only parameters, and C# 5 in the
Add-Type source (5.1 compiles it with the .NET Framework compiler).

Environment:
  BLOCKS_B64 or BLOCKS_FILE  the blocks, separated by a line "### block"
  FIELD_SHELL     powershell (Windows PowerShell 5.1, default) | pwsh
  FIELD_ELEVATED  false (default): blocks run as a new standard user
                  "fieldstd"; true: as the runner's admin account
  FIELD_CELL      label for the report (default: windows)
  FIELD_OUT       output directory (default .\field-out)
  BLOCK_TIMEOUT   seconds per block (default 1200)

How the standard user works: New-LocalUser creates fieldstd (member of
Users only, which holds the "allow log on locally" right the logon needs)
and each block starts through .NET Process.Start with UserName/Password/
LoadUserProfile, which is CreateProcessWithLogonW. Because no environment
block is passed, the child's environment is built from the new user's
registry profile, the same as a window opened from the Start menu. The
child inherits this process's window station and desktop, but the call
does not give the new user access to them (only XP SP2 and Server 2003
did), and without it the child can fail to start (0xC0000142). So
Grant-SessionAccess adds allow entries for fieldstd to both first. UAC is
off on hosted runners, so this user's token really is non-admin. A probe
runs first as that user and must report elevated=False; if the user
cannot be created or started (for example because the grant failed), or
the probe says otherwise, every block runs as the admin account and the
report says "admin only" with the reason.
#>

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

function Fail([string]$msg) {
    [Console]::Error.WriteLine("run-blocks: $msg")
    exit 2
}

$onWindows = ($env:OS -eq 'Windows_NT')
$utf8 = New-Object System.Text.UTF8Encoding $false
$utf8Bom = New-Object System.Text.UTF8Encoding $true
$inv = [Globalization.CultureInfo]::InvariantCulture

$shell = $env:FIELD_SHELL
if (-not $shell) { $shell = 'powershell' }
$elevated = $env:FIELD_ELEVATED
if (-not $elevated) { $elevated = 'false' }
if ($elevated -ne 'true' -and $elevated -ne 'false') { Fail 'FIELD_ELEVATED must be true or false' }
$timeoutSec = 1200
if ($env:BLOCK_TIMEOUT) { $timeoutSec = [int]$env:BLOCK_TIMEOUT }
$cell = $env:FIELD_CELL
if (-not $cell) { $cell = 'windows' }

$winPs = 'powershell.exe'
if ($onWindows) { $winPs = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe' }
switch ($shell) {
    'powershell' {
        if (-not $onWindows) { Fail 'powershell (Windows PowerShell 5.1) only exists on Windows' }
        $exe = $winPs
    }
    'pwsh' {
        $c = Get-Command pwsh -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if (-not $c) { Fail 'pwsh is not installed on this machine' }
        $exe = $c.Path
    }
    'bash' { Fail 'bash on Windows would be Git Bash, not WSL (hosted runners have no WSL); rehearse WSL blocks on ubuntu-22.04 or ubuntu-24.04' }
    default { Fail "FIELD_SHELL must be powershell or pwsh, not '$shell'" }
}

$out = $env:FIELD_OUT
if (-not $out) { $out = Join-Path (Get-Location).Path 'field-out' }
New-Item -ItemType Directory -Force -Path $out | Out-Null
$out = (Resolve-Path -LiteralPath $out).Path

if ($env:BLOCKS_FILE) {
    if (-not (Test-Path -LiteralPath $env:BLOCKS_FILE)) { Fail "no such file: $env:BLOCKS_FILE" }
    $bytes = [IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $env:BLOCKS_FILE).Path)
} elseif ($env:BLOCKS_B64) {
    try { $bytes = [Convert]::FromBase64String(($env:BLOCKS_B64 -replace '\s', '')) }
    catch { Fail 'blocks input is not valid base64' }
} else {
    Fail 'set BLOCKS_FILE or BLOCKS_B64'
}
$text = $utf8.GetString($bytes)
if ($text.Length -gt 0 -and $text[0] -eq [char]0xFEFF) { $text = $text.Substring(1) }
[IO.File]::WriteAllText((Join-Path $out 'blocks.txt'), $text, $utf8)

$blocks = @()
$cur = @()
foreach ($l in (($text -split "`r?`n") + '### block')) {
    if ($l -match '^### block[ \t]*$') {
        if (($cur -join '').Trim().Length -gt 0) { $blocks += (($cur -join "`r`n").TrimEnd() + "`r`n") }
        $cur = @()
        continue
    }
    $cur += $l
}
if ($blocks.Count -eq 0) { Fail "no blocks found (separate blocks with a line '### block')" }

# Block files and their logs live where the block user can write; the
# checkout under the admin's work folder may not be.
if ($onWindows) { $runDir = 'C:\field-run' } else { $runDir = Join-Path ([IO.Path]::GetTempPath()) 'field-run' }
if (Test-Path -LiteralPath $runDir) { Remove-Item -LiteralPath $runDir -Recurse -Force }
New-Item -ItemType Directory -Force -Path $runDir | Out-Null

$mode = 'admin'
$reason = ''
$user = ''
$secPw = $null
$grantError = ''

# CreateProcessWithLogonW leaves window station and desktop access to the
# caller (see the header). This gives the user the specific rights an
# interactive logon gets on both, but not the right to change their ACLs.
function Grant-SessionAccess([string]$sid) {
    if (-not ('BwnField.SessionAccess' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.AccessControl;
using System.Security.Principal;
using System.Text;

namespace BwnField {
    public static class SessionAccess {
        [DllImport("user32.dll", SetLastError = true)]
        static extern IntPtr GetProcessWindowStation();
        [DllImport("user32.dll", SetLastError = true)]
        static extern IntPtr GetThreadDesktop(uint threadId);
        [DllImport("kernel32.dll")]
        static extern uint GetCurrentThreadId();
        [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        static extern bool GetUserObjectInformationW(IntPtr obj, int index, StringBuilder info, int length, out int needed);
        [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        static extern IntPtr OpenWindowStationW(string name, bool inherit, uint access);
        [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        static extern IntPtr OpenDesktopW(string name, uint flags, bool inherit, uint access);
        [DllImport("user32.dll", SetLastError = true)]
        static extern bool GetUserObjectSecurity(IntPtr obj, ref uint info, byte[] sd, uint length, out uint needed);
        [DllImport("user32.dll", SetLastError = true)]
        static extern bool SetUserObjectSecurity(IntPtr obj, ref uint info, byte[] sd);
        [DllImport("user32.dll")]
        static extern bool CloseWindowStation(IntPtr winsta);
        [DllImport("user32.dll")]
        static extern bool CloseDesktop(IntPtr desktop);

        const int UOI_NAME = 2;
        const uint DACL_SECURITY_INFORMATION = 4;
        const uint READ_CONTROL = 0x20000;
        const uint WRITE_DAC = 0x40000;
        const int WINSTA_ALL_SPECIFIC = 0x37F;
        const int DESKTOP_ALL_SPECIFIC = 0x1FF;

        static string NameOf(IntPtr obj) {
            StringBuilder name = new StringBuilder(256);
            int needed;
            if (!GetUserObjectInformationW(obj, UOI_NAME, name, name.Capacity * 2, out needed)) throw new Win32Exception();
            return name.ToString();
        }

        static void Allow(IntPtr obj, SecurityIdentifier sid, int mask) {
            uint info = DACL_SECURITY_INFORMATION;
            uint needed;
            GetUserObjectSecurity(obj, ref info, null, 0, out needed);
            byte[] buf = new byte[needed];
            if (!GetUserObjectSecurity(obj, ref info, buf, needed, out needed)) throw new Win32Exception();
            RawSecurityDescriptor sd = new RawSecurityDescriptor(buf, 0);
            RawAcl dacl = sd.DiscretionaryAcl;
            if (dacl == null) return; // a null DACL already allows everyone
            // Explicit entries go before inherited ones (canonical order).
            int at = dacl.Count;
            for (int i = 0; i < dacl.Count; i++) {
                if ((dacl[i].AceFlags & AceFlags.Inherited) != 0) { at = i; break; }
            }
            dacl.InsertAce(at, new CommonAce(AceFlags.None, AceQualifier.AccessAllowed, mask, sid, false, null));
            byte[] updated = new byte[sd.BinaryLength];
            sd.GetBinaryForm(updated, 0);
            if (!SetUserObjectSecurity(obj, ref info, updated)) throw new Win32Exception();
        }

        public static string Grant(string sidText) {
            SecurityIdentifier sid = new SecurityIdentifier(sidText);
            string winstaName = NameOf(GetProcessWindowStation());
            string desktopName = NameOf(GetThreadDesktop(GetCurrentThreadId()));
            IntPtr winsta = OpenWindowStationW(winstaName, false, READ_CONTROL | WRITE_DAC);
            if (winsta == IntPtr.Zero) throw new Win32Exception();
            try { Allow(winsta, sid, WINSTA_ALL_SPECIFIC); } finally { CloseWindowStation(winsta); }
            IntPtr desktop = OpenDesktopW(desktopName, 0, false, READ_CONTROL | WRITE_DAC);
            if (desktop == IntPtr.Zero) throw new Win32Exception();
            try { Allow(desktop, sid, DESKTOP_ALL_SPECIFIC); } finally { CloseDesktop(desktop); }
            return winstaName + "\\" + desktopName;
        }
    }
}
'@
    }
    return [BwnField.SessionAccess]::Grant($sid)
}
if ($elevated -eq 'false') {
    if (-not $onWindows) {
        $reason = 'not on Windows (developer test run)'
    } else {
        try {
            $user = 'fieldstd'
            $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
            $raw = New-Object byte[] 18
            $rng.GetBytes($raw)
            # Fixed prefix covers the complexity rule: upper, lower, digit, symbol.
            $pw = 'Fx9!' + ([Convert]::ToBase64String($raw) -replace '[+/=]', 'a')
            Write-Output "::add-mask::$pw"
            $secPw = New-Object System.Security.SecureString
            foreach ($ch in $pw.ToCharArray()) { $secPw.AppendChar($ch) }
            if (Get-LocalUser -Name $user -ErrorAction SilentlyContinue) {
                Set-LocalUser -Name $user -Password $secPw
            } else {
                New-LocalUser -Name $user -Password $secPw -PasswordNeverExpires -AccountNeverExpires -Description 'bwn field rehearsal' | Out-Null
            }
            # Users (by SID: group names are localized) grants local logon.
            try { Add-LocalGroupMember -SID 'S-1-5-32-545' -Member $user }
            catch { Write-Output "run-blocks: Users group: $($_.Exception.Message)" }
            # CreateProcessWithLogonW goes through the Secondary Logon service.
            Set-Service -Name seclogon -StartupType Manual
            Start-Service -Name seclogon
            & icacls.exe $runDir /grant "${user}:(OI)(CI)M" | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "icacls exited $LASTEXITCODE" }
            $mode = 'standard'
        } catch {
            $reason = 'could not create the standard user: ' + $_.Exception.Message
        }
        if ($mode -eq 'standard') {
            # Not fatal here: if the child still cannot start, the probe
            # below fails and the run falls back to admin only with this note.
            try {
                $where = Grant-SessionAccess (Get-LocalUser -Name $user).SID.Value
                Write-Output "run-blocks: gave $user access to $where"
            } catch {
                $grantError = $_.Exception.Message
                Write-Output "run-blocks: window station grant failed: $grantError"
            }
        }
    }
}

# A new window builds its environment from the registry, so a PATH change an
# earlier block made shows up in the next one and runner variables do not.
# Only needed for the admin account: the standard user's process gets a
# profile-built environment from Windows itself.
function Use-FreshEnvironment($psi) {
    $envs = $psi.EnvironmentVariables
    $drop = @()
    foreach ($k in $envs.Keys) {
        if ($k -match '^(GITHUB_|RUNNER_|ACTIONS_|INPUT_|BLOCKS_|FIELD_|BLOCK_TIMEOUT)') { $drop += $k }
    }
    foreach ($k in $drop) { $envs.Remove($k) }
    if (-not $onWindows) { return }
    foreach ($scope in 'Machine', 'User') {
        $vars = [Environment]::GetEnvironmentVariables($scope)
        foreach ($k in $vars.Keys) {
            if ($k -ne 'Path') { $envs[$k] = [string]$vars[$k] }
        }
    }
    $paths = @([Environment]::GetEnvironmentVariable('Path', 'Machine'), [Environment]::GetEnvironmentVariable('Path', 'User')) | Where-Object { $_ }
    $envs['Path'] = $paths -join ';'
}

# Starts one fresh shell process for a script file, stdout/stderr to files,
# stdin empty so a prompt fails instead of hanging the job.
function Invoke-Fresh([string]$shellExe, [string]$file, [string]$so, [string]$se) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.WorkingDirectory = $runDir
    # RemoteSigned is Windows Server's default policy: npm's .ps1 shims and
    # the block file (local, no mark of the web) run, as they would there.
    $shellArgs = '-NoLogo -NoProfile -NonInteractive -ExecutionPolicy RemoteSigned -File "' + $file + '"'
    if ($onWindows) {
        $psi.FileName = Join-Path $env:SystemRoot 'System32\cmd.exe'
        $psi.Arguments = '/d /s /c "cd /d "%USERPROFILE%" & "' + $shellExe + '" ' + $shellArgs + ' <NUL 1>"' + $so + '" 2>"' + $se + '""'
    } else {
        # Developer test runs of this script under pwsh on Linux or macOS
        # (ArgumentList is .NET Core only, which is all this branch sees).
        $psi.FileName = '/bin/sh'
        foreach ($a in @('-c', 'cd "$HOME"; exec "$0" -NoLogo -NoProfile -NonInteractive -File "$1" </dev/null >"$2" 2>"$3"', $shellExe, $file, $so, $se)) {
            $psi.ArgumentList.Add($a)
        }
    }
    if ($mode -eq 'standard') {
        $psi.UserName = $user
        $psi.Domain = $env:COMPUTERNAME
        $psi.Password = $secPw
        $psi.LoadUserProfile = $true
        # EnvironmentVariables stays untouched on purpose (see the header).
    } else {
        Use-FreshEnvironment $psi
    }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $p = [Diagnostics.Process]::Start($psi)
    $done = $p.WaitForExit($timeoutSec * 1000)
    if (-not $done) {
        if ($onWindows) { & taskkill.exe /PID $p.Id /T /F | Out-Null } else { $p.Kill() }
        $p.WaitForExit()
    }
    $sw.Stop()
    $code = 0
    if ($done) { $code = $p.ExitCode }
    return New-Object PSObject -Property @{ Code = $code; Ms = $sw.ElapsedMilliseconds; TimedOut = (-not $done) }
}

function Read-Log([string]$path) {
    if (-not (Test-Path -LiteralPath $path)) { return '' }
    $b = [IO.File]::ReadAllBytes($path)
    # powershell.exe writes redirected output in the OEM code page, pwsh in UTF-8.
    try { $t = (New-Object System.Text.UTF8Encoding($false, $true)).GetString($b) }
    catch { $t = [Text.Encoding]::GetEncoding([Globalization.CultureInfo]::CurrentCulture.TextInfo.OEMCodePage).GetString($b) }
    if ($t.Length -gt 0 -and $t[0] -eq [char]0xFEFF) { $t = $t.Substring(1) }
    # Colour codes (pwsh colours errors) would hide text from the expect check.
    return ($t -replace ([string][char]27 + '\[[0-9;?]*[A-Za-z]'), '')
}

function Get-HtmlText([string]$s) {
    return $s.Replace('&', '&amp;').Replace('<', '&lt;').Replace('>', '&gt;')
}

# Last 150 lines and 16 KB: the job summary is capped at 1 MiB per step.
function Get-Excerpt([string]$s) {
    $lines = $s -split "`r?`n"
    $note = ''
    if ($lines.Count -gt 150) {
        $note = "[last 150 of $($lines.Count) lines; the full log is in the artifact]`n"
        $s = ($lines[($lines.Count - 150)..($lines.Count - 1)]) -join "`n"
    }
    if ($s.Length -gt 16000) { $s = $s.Substring($s.Length - 16000) }
    return $note + $s
}

function Get-Label([string]$block) {
    foreach ($l in ($block -split "`r?`n")) {
        $t = $l.Trim()
        if ($t) {
            if ($t.Length -gt 70) { $t = $t.Substring(0, 70) }
            return (Get-HtmlText $t).Replace('|', '&#124;')
        }
    }
    return ''
}

function Format-Duration([long]$ms) {
    return ($ms / 1000.0).ToString('0.0', $inv)
}

# Cell facts from the same kind of process the blocks get. For the standard
# user this is also the check that the user switch works and is non-admin.
$probe = @'
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$elev = (New-Object Security.Principal.WindowsPrincipal $id).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
$os = 'unknown'; $grp = 'n/a'; $sec = @()
if ($env:OS -eq 'Windows_NT') {
    $o = Get-CimInstance Win32_OperatingSystem
    $os = "$($o.Caption) $($o.Version)"
    $grp = [bool](whoami.exe /groups | Select-String 'S-1-5-32-544')
    foreach ($s in 'CSFalconService', 'csagent', 'WinDefend', 'Sense', 'SentinelAgent', 'CylanceSvc', 'CbDefense') {
        if (sc.exe query $s | Select-String 'STATE') { $sec += $s }
    }
}
$tools = @()
foreach ($c in 'winget', 'node', 'npm', 'git', 'choco', 'ollama', 'ffmpeg', 'rg') {
    if (Get-Command $c -ErrorAction SilentlyContinue) { $tools += $c }
}
"facts: os=$os; ps=$($PSVersionTable.PSVersion); user=$($id.Name) adminGroup=$grp elevated=$elev; profile=$env:USERPROFILE; security services: $($sec -join ', '); on PATH: $($tools -join ' ')"
'@
$probeFile = Join-Path $runDir 'probe.ps1'
[IO.File]::WriteAllText($probeFile, $probe, $utf8Bom)
$probeShell = $winPs
if (-not $onWindows) { $probeShell = $exe }

function Get-CellFact {
    $so = Join-Path $runDir 'probe.out'
    $se = Join-Path $runDir 'probe.err'
    $r = $null
    try { $r = Invoke-Fresh $probeShell $probeFile $so $se }
    catch { return New-Object PSObject -Property @{ Ok = $false; Line = ''; Why = $_.Exception.Message } }
    $line = ((Read-Log $so) -split "`r?`n" | Where-Object { $_ -like 'facts:*' } | Select-Object -Last 1)
    if (-not $line) {
        $why = "probe exited $($r.Code): " + ((Read-Log $se).Trim() -replace '\s+', ' ')
        return New-Object PSObject -Property @{ Ok = $false; Line = ''; Why = $why }
    }
    return New-Object PSObject -Property @{ Ok = $true; Line = $line.Substring(7); Why = '' }
}

$facts = Get-CellFact
if ($mode -eq 'standard') {
    if (-not $facts.Ok) {
        $reason = 'could not start a process as the standard user: ' + $facts.Why
        if ($grantError) { $reason += ' (window station grant failed: ' + $grantError + ')' }
    } elseif ($facts.Line -notmatch 'elevated=False') {
        $reason = 'the standard user came up elevated: ' + $facts.Line
    }
    if ($reason) {
        $mode = 'admin'
        $facts = Get-CellFact
    }
}
if (-not $facts.Ok) { Fail ('cannot run a probe process: ' + $facts.Why) }

$gha = ($env:GITHUB_ACTIONS -eq 'true')
$rows = New-Object System.Text.StringBuilder
$details = New-Object System.Text.StringBuilder
$tsv = New-Object System.Text.StringBuilder
[void]$tsv.Append("block`tresult`texit`tseconds`tmissing_expect`n")
$failed = $false
$totalMs = 0

for ($i = 1; $i -le $blocks.Count; $i++) {
    $body = $blocks[$i - 1]
    $n = $i.ToString('00')
    $f = Join-Path $runDir "block$n.ps1"
    $so = Join-Path $runDir "block$n.out"
    $se = Join-Path $runDir "block$n.err"
    # UTF-8 with BOM, or Windows PowerShell 5.1 reads the file as ANSI.
    [IO.File]::WriteAllText($f, $body, $utf8Bom)
    Copy-Item -LiteralPath $f -Destination $out
    $label = Get-Label $body

    if ($failed) {
        [void]$rows.Append("| $i | <code>$label</code> | | | | not run |`n")
        [void]$tsv.Append("$i`tnot run`t`t`t`n")
        continue
    }

    $wantExit = '0'
    $expects = @()
    foreach ($l in ($body -split "`r?`n")) {
        if ($l -match '^\s*#\s*expect-exit:\s*(\S+)') { $wantExit = $Matches[1] }
        elseif ($l -match '^\s*#\s*expect:\s*(.*?)\s*$') { if ($Matches[1]) { $expects += $Matches[1] } }
    }

    $r = Invoke-Fresh $exe $f $so $se
    $totalMs += $r.Ms
    $secs = Format-Duration $r.Ms
    $stdout = Read-Log $so
    $stderr = Read-Log $se
    [IO.File]::WriteAllText((Join-Path $out "block$n.out"), $stdout, $utf8)
    [IO.File]::WriteAllText((Join-Path $out "block$n.err"), $stderr, $utf8)

    # PowerShell repeats a failing source line in its error ("+ line" in 5.1,
    # "  2 |  line" in 7), so a check line that failed would still contain its
    # own expect text. Those lines are left out of the match.
    $echoLine = '^\++ |^\s*\d+ \| |^\s*\| *~+\s*$|^\s*Line \|\s*$'
    $combined = (($stdout + "`n" + $stderr) -split "`r?`n" | Where-Object { $_ -notmatch $echoLine }) -join "`n"
    $missing = @()
    if ($expects.Count -eq 0) {
        $missing += '(no # expect: line)'
    } else {
        foreach ($e in $expects) { if (-not $combined.Contains($e)) { $missing += $e } }
    }

    $result = 'pass'
    $codeText = [string]$r.Code
    if ($r.TimedOut) {
        $result = "TIMEOUT (${timeoutSec}s)"
        $codeText = 'killed'
    } elseif ($wantExit -ne 'any' -and $codeText -ne $wantExit) {
        $result = "FAIL: exit $codeText"
    } elseif ($missing.Count -gt 0) {
        $result = 'FAIL: expect'
    }
    if ($result -ne 'pass') { $failed = $true }

    $missingText = $missing -join '; '
    $expectCell = 'ok'
    if ($missingText) { $expectCell = 'missing: ' + (Get-HtmlText $missingText).Replace('|', '&#124;') }
    [void]$rows.Append("| $i | <code>$label</code> | $codeText | $expectCell | ${secs}s | $result |`n")
    [void]$tsv.Append("$i`t$result`t$codeText`t$secs`t$missingText`n")

    if ($gha) { Write-Output "::group::block ${i}: $result (exit $codeText, ${secs}s)" } else { Write-Output "== block ${i}: $result (exit $codeText, ${secs}s)" }
    Write-Output '--- block'
    Write-Output $body
    Write-Output '--- stdout'
    Write-Output $stdout
    Write-Output '--- stderr'
    Write-Output $stderr
    if ($missingText) { Write-Output "--- expect not found: $missingText" }
    if ($gha) { Write-Output '::endgroup::' }

    [void]$details.Append("<details><summary>Block ${i}: $result (exit $codeText, ${secs}s)</summary>`n`n")
    [void]$details.Append('<pre>' + (Get-HtmlText $body.TrimEnd()) + "</pre>`n`nstdout:`n`n")
    [void]$details.Append('<pre>' + (Get-HtmlText (Get-Excerpt $stdout.TrimEnd())) + "</pre>`n`nstderr:`n`n")
    [void]$details.Append('<pre>' + (Get-HtmlText (Get-Excerpt $stderr.TrimEnd())) + "</pre>`n`n</details>`n`n")
}

$verdict = "all $($blocks.Count) blocks passed"
if ($failed) { $verdict = 'FAILED' }
$shellName = $shell
if ($shell -eq 'powershell') { $shellName = 'Windows PowerShell 5.1 (powershell.exe)' }
if ($mode -eq 'standard') { $who = "standard user $user (not elevated)" } else { $who = 'admin account (elevated)' }

$md = New-Object System.Text.StringBuilder
[void]$md.Append("## Field rehearsal: $cell, $shell, ${who}: $verdict`n`n")
if ($elevated -eq 'false' -and $mode -ne 'standard') {
    [void]$md.Append('**Admin only:** these blocks were asked to run as a standard user but ran elevated, because ' + (Get-HtmlText $reason) + ". Anything that needs admin rights is not rehearsed as non-admin.`n`n")
}
[void]$md.Append('Cell: ' + (Get-HtmlText $facts.Line) + "`n`n")
[void]$md.Append("Each block ran in a new $shellName process with -NoProfile -ExecutionPolicy RemoteSigned, started in the user's profile folder with an environment rebuilt from the registry (as a new window gets). Total $(Format-Duration $totalMs)s.`n`n")
[void]$md.Append("| # | Block | Exit | Expect | Time | Result |`n|---|---|---|---|---|---|`n")
[void]$md.Append($rows.ToString())
[void]$md.Append("`n")
[void]$md.Append($details.ToString())

$summary = Join-Path $out 'summary.md'
[IO.File]::WriteAllText($summary, $md.ToString(), $utf8)
[IO.File]::WriteAllText((Join-Path $out 'results.tsv'), $tsv.ToString(), $utf8)
if ($env:GITHUB_STEP_SUMMARY) {
    [IO.File]::AppendAllText($env:GITHUB_STEP_SUMMARY, $md.ToString(), $utf8)
}

Write-Output ''
Write-Output "run-blocks: $cell / $shell / ${who}: $verdict ($(Format-Duration $totalMs)s)"
if ($reason) { Write-Output "run-blocks: admin only: $reason" }
Write-Output "run-blocks: cell: $($facts.Line)"
Write-Output $tsv.ToString()
Write-Output "run-blocks: report in $summary"
if ($failed) { exit 1 }
exit 0
