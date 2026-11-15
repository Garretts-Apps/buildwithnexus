# The same attestation cases as scripts/install.test.js, on Windows PowerShell 5.1.
# A fake gh.cmd is first on PATH. The checksum, the missing login, and a real
# rejection are what decide whether the file is installed.
$ErrorActionPreference = 'Stop'
$install = Join-Path $PSScriptRoot 'install.ps1'
# Windows PowerShell lives under System32\WindowsPowerShell, not in System32
# itself. The cases below replace PATH, so the child host has to be named by
# its own path and that directory has to stay on PATH.
$psExe = Join-Path $PSHOME 'powershell.exe'
if (-not (Test-Path -LiteralPath $psExe)) { $psExe = (Get-Command powershell.exe).Source }
$psDir = Split-Path -Parent $psExe
$root = Join-Path $env:TEMP ("bwn-attest-" + [guid]::NewGuid().ToString('n'))
New-Item -ItemType Directory -Force -Path $root | Out-Null

function Fail([string]$Message) {
  throw $Message
}

function New-Stub {
  $dir = Join-Path $root 'stub-src'
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  $cs = Join-Path $dir 'stub.cs'
  $exe = Join-Path $dir 'stub.exe'
  Set-Content -LiteralPath $cs -Encoding ASCII -Value @'
using System;
class S {
  static int Main() {
    Console.WriteLine("buildwithnexus test-stub");
    return 0;
  }
}
'@
  $csc = Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
  if (-not (Test-Path -LiteralPath $csc)) {
    $csc = Join-Path $env:WINDIR 'Microsoft.NET\Framework\v4.0.30319\csc.exe'
  }
  if (-not (Test-Path -LiteralPath $csc)) { Fail "csc.exe is not installed" }
  & $csc /nologo "/out:$exe" $cs
  if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $exe)) { Fail "could not compile the stub exe" }
  return $exe
}

function New-Gh([string]$Mode) {
  $dir = Join-Path $root ("gh-" + $Mode)
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  $log = Join-Path $dir 'gh.log'
  Set-Content -LiteralPath $log -Encoding ASCII -Value ''
  $cs = Join-Path $dir 'gh.cs'
  $exe = Join-Path $dir 'gh.exe'
  # A real exe, so PowerShell records its exit code. gh.cmd's exit /b does not.
  Set-Content -LiteralPath $cs -Encoding ASCII -Value @'
using System;
using System.IO;
class G {
  static int Main(string[] args) {
    string log = Environment.GetEnvironmentVariable("GH_LOG");
    if (!string.IsNullOrEmpty(log)) File.AppendAllText(log, string.Join(" ", args) + "\n");
    string mode = Environment.GetEnvironmentVariable("GH_MODE") ?? "";
    string a0 = args.Length > 0 ? args[0] : "";
    string a1 = args.Length > 1 ? args[1] : "";
    string a2 = args.Length > 2 ? args[2] : "";
    if (mode == "unauth") {
      if (a0 == "auth" && a1 == "status") {
        Console.Error.WriteLine("You are not logged into any GitHub hosts.");
        return 1;
      }
      Console.Error.WriteLine("unexpected");
      return 99;
    }
    if (mode == "old") {
      if (a0 == "auth" && a1 == "status") return 0;
      Console.Error.WriteLine("unknown command \"attestation\" for \"gh\"");
      return 1;
    }
    if (mode == "offline") {
      if (a0 == "auth" && a1 == "status") return 0;
      if (a0 == "attestation" && a2 == "--help") return 0;
      Console.Error.WriteLine("error creating Sigstore verifier: no valid Sigstore verifiers");
      return 1;
    }
    if (mode == "reject") {
      if (a0 == "auth" && a1 == "status") return 0;
      if (a0 == "attestation" && a2 == "--help") return 0;
      Console.Error.WriteLine("Error: HTTP 404: Not Found (https://api.github.com/repos/Garretts-Apps/buildwithnexus/attestations/sha256:dead)");
      return 1;
    }
    Console.Error.WriteLine("unset GH_MODE");
    return 99;
  }
}
'@
  $csc = Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
  if (-not (Test-Path -LiteralPath $csc)) {
    $csc = Join-Path $env:WINDIR 'Microsoft.NET\Framework\v4.0.30319\csc.exe'
  }
  & $csc /nologo "/out:$exe" $cs
  if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $exe)) { Fail "could not compile gh.exe" }
  return @{ Dir = $dir; Log = $log }
}

function Invoke-Install {
  param([string[]]$ArgList, [string]$GhDir, [string]$Mode)
  $savedPath = $env:PATH
  $savedMode = $env:GH_MODE
  $savedLog = $env:GH_LOG
  # Local to this function. PowerShell 5.1 turns a native command's stderr into
  # a terminating NativeCommandError when the preference is Stop, even if the
  # child exited 0. Continue lets a warning stay data we can assert on.
  $ErrorActionPreference = 'Continue'
  $merged = $null
  $code = 1
  try {
    $basePath = "$psDir;$env:SystemRoot\System32"
    if ($GhDir) {
      $env:PATH = "$GhDir;$basePath"
      $env:GH_MODE = $Mode
      $env:GH_LOG = Join-Path $GhDir 'gh.log'
    } else {
      $env:PATH = $basePath
      Remove-Item Env:GH_MODE -ErrorAction SilentlyContinue
      Remove-Item Env:GH_LOG -ErrorAction SilentlyContinue
    }
    $merged = & $psExe -NoLogo -NoProfile -ExecutionPolicy Bypass -File $install @ArgList 2>&1
    $code = $LASTEXITCODE
  } finally {
    $env:PATH = $savedPath
    if ($null -eq $savedMode) { Remove-Item Env:GH_MODE -ErrorAction SilentlyContinue }
    else { $env:GH_MODE = $savedMode }
    if ($null -eq $savedLog) { Remove-Item Env:GH_LOG -ErrorAction SilentlyContinue }
    else { $env:GH_LOG = $savedLog }
  }
  $outLines = @()
  $errLines = @()
  if ($null -ne $merged) {
    foreach ($item in @($merged)) {
      if ($item -is [System.Management.Automation.ErrorRecord]) {
        $errLines += [string]$item.Exception.Message
      } else {
        $outLines += [string]$item
      }
    }
  }
  return @{ Code = $code; Out = ($outLines -join "`n"); Err = ($errLines -join "`n") }
}

$stub = New-Stub
$hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $stub).Hash.ToLower()

function Prefix([string]$Name) { Join-Path $root $Name }

$absent = Invoke-Install -ArgList @('-File', $stub, '-Sha256', $hash, '-Prefix', (Prefix 'absent'))
if ($absent.Code -ne 0) { Fail "gh absent exited $($absent.Code): $($absent.Err)" }
if ($absent.Err -notmatch 'gh is not installed') { Fail "gh absent did not warn: $($absent.Err)" }
if (-not (Test-Path -LiteralPath (Join-Path (Prefix 'absent') 'bwn.exe'))) { Fail 'gh absent did not install' }

$unauthGh = New-Gh 'unauth'
$unauth = Invoke-Install -ArgList @('-File', $stub, '-Sha256', $hash, '-Prefix', (Prefix 'unauth')) -GhDir $unauthGh.Dir -Mode 'unauth'
if ($unauth.Code -ne 0) { Fail "unauthenticated gh exited $($unauth.Code): $($unauth.Err)" }
if ($unauth.Err -notmatch 'not logged in') { Fail "unauthenticated gh did not warn: $($unauth.Err)" }
if (-not (Test-Path -LiteralPath (Join-Path (Prefix 'unauth') 'bwn.exe'))) { Fail 'unauthenticated gh did not install' }
$unauthLog = [IO.File]::ReadAllText($unauthGh.Log)
if ($unauthLog -notmatch 'auth status') { Fail "unauthenticated gh was not asked for auth status: $unauthLog" }
if ($unauthLog -match 'attestation') { Fail "unauthenticated gh was asked to attest: $unauthLog" }

$bad = Invoke-Install -ArgList @('-File', $stub, '-Sha256', ('f' * 64), '-Prefix', (Prefix 'badsum'))
if ($bad.Code -eq 0) { Fail 'checksum mismatch exited 0' }
if (($bad.Err + $bad.Out) -notmatch 'checksum mismatch') { Fail "checksum mismatch was not reported: $($bad.Err) $($bad.Out)" }
if (Test-Path -LiteralPath (Join-Path (Prefix 'badsum') 'bwn.exe')) { Fail 'checksum mismatch installed the file' }

$rejectGh = New-Gh 'reject'
$reject = Invoke-Install -ArgList @('-File', $stub, '-Sha256', $hash, '-Prefix', (Prefix 'reject')) -GhDir $rejectGh.Dir -Mode 'reject'
if ($reject.Code -eq 0) { Fail 'attestation rejection exited 0' }
if (($reject.Err + $reject.Out) -notmatch 'rejected this binary') { Fail "rejection was not reported: $($reject.Err)" }
if (Test-Path -LiteralPath (Join-Path (Prefix 'reject') 'bwn.exe')) { Fail 'attestation rejection installed the file' }

foreach ($mode in @('old', 'offline')) {
  $fake = New-Gh $mode
  $ran = Invoke-Install -ArgList @('-File', $stub, '-Sha256', $hash, '-Prefix', (Prefix $mode)) -GhDir $fake.Dir -Mode $mode
  if ($ran.Code -ne 0) { Fail "$mode exited $($ran.Code): $($ran.Err)" }
  if ($ran.Err -notmatch 'was not checked') { Fail "$mode did not warn: $($ran.Err)" }
  if (-not (Test-Path -LiteralPath (Join-Path (Prefix $mode) 'bwn.exe'))) { Fail "$mode did not install" }
}

$strict = Invoke-Install -ArgList @('-File', $stub, '-Sha256', $hash, '-Prefix', (Prefix 'strict'), '-RequireAttest')
if ($strict.Code -eq 0) { Fail '--require-attest with no gh exited 0' }
if (($strict.Err + $strict.Out) -notmatch 'RequireAttest') { Fail "require-attest did not say so: $($strict.Err)" }
if (Test-Path -LiteralPath (Join-Path (Prefix 'strict') 'bwn.exe')) { Fail 'require-attest installed without gh' }

$strictLogin = Invoke-Install -ArgList @('-File', $stub, '-Sha256', $hash, '-Prefix', (Prefix 'strict-login'), '-RequireAttest') -GhDir $unauthGh.Dir -Mode 'unauth'
if ($strictLogin.Code -eq 0) { Fail 'require-attest while logged out exited 0' }
if (($strictLogin.Err + $strictLogin.Out) -notmatch 'not logged in') { Fail "require-attest login warning missing: $($strictLogin.Err)" }

$skipGh = New-Gh 'reject'
$skip = Invoke-Install -ArgList @('-File', $stub, '-Sha256', $hash, '-Prefix', (Prefix 'skip'), '-NoAttest') -GhDir $skipGh.Dir -Mode 'reject'
if ($skip.Code -ne 0) { Fail "-NoAttest exited $($skip.Code): $($skip.Err)" }
$skipLog = [IO.File]::ReadAllText($skipGh.Log).Trim()
if ($skipLog) { Fail "-NoAttest invoked gh: $skipLog" }
if (-not (Test-Path -LiteralPath (Join-Path (Prefix 'skip') 'bwn.exe'))) { Fail '-NoAttest did not install' }

Write-Output 'installer attestation ok'
