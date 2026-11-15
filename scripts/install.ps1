# Install the native buildwithnexus binary. No elevation, no Node.
#   irm https://raw.githubusercontent.com/Garretts-Apps/buildwithnexus/main/scripts/install.ps1 | iex
#   powershell -File install.ps1 -Version 0.21.0
#   powershell -File install.ps1 -Uninstall
# CI passes -File and -Sha256 for a binary this job just built.
param(
  [string]$Version = '',
  [string]$Prefix = '',
  [string]$File = '',
  [string]$Sha256 = '',
  [switch]$Uninstall,
  [switch]$NoAttest,
  [switch]$RequireAttest
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Repo = 'Garretts-Apps/buildwithnexus'

if (-not $Prefix) {
  $Prefix = Join-Path $env:LOCALAPPDATA 'Programs\bwn'
}
if ($Version.StartsWith('v')) { $Version = $Version.Substring(1) }

function Say([string]$Text) { Write-Output $Text }

if ($NoAttest -and $RequireAttest) {
  throw '-NoAttest and -RequireAttest cannot be combined'
}

if ($Uninstall) {
  foreach ($name in @('bwn.exe', 'buildwithnexus.exe')) {
    $p = Join-Path $Prefix $name
    if (Test-Path -LiteralPath $p) { Remove-Item -LiteralPath $p -Force }
  }
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if ($userPath) {
    $kept = @()
    foreach ($part in ($userPath -split ';')) {
      if ($part -and ($part.TrimEnd('\') -ne $Prefix.TrimEnd('\'))) { $kept += $part }
    }
    [Environment]::SetEnvironmentVariable('Path', ($kept -join ';'), 'User')
  }
  Say "removed $Prefix\bwn.exe and buildwithnexus.exe"
  return
}

if ($File -and -not $Sha256) { throw '-File needs -Sha256' }

$asset = 'buildwithnexus-x86_64-pc-windows-msvc.exe'
$tmp = Join-Path $env:TEMP ("bwn-install-" + [guid]::NewGuid().ToString('n') + '.exe')
try {
  if ($File) {
    Copy-Item -LiteralPath $File -Destination $tmp -Force
  } else {
    if ($Version) {
      $base = "https://github.com/$Repo/releases/download/v$Version"
    } else {
      $base = "https://github.com/$Repo/releases/latest/download"
    }
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $tmp
    $sumFile = "$tmp.sha256"
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset.sha256" -OutFile $sumFile
    $line = Get-Content -LiteralPath $sumFile -TotalCount 1
    $Sha256 = ($line -split '\s+')[0]
  }

  $got = (Get-FileHash -Algorithm SHA256 -LiteralPath $tmp).Hash.ToLower()
  if ($got -ne $Sha256.ToLower()) {
    throw "checksum mismatch (got $got, expected $Sha256). Not installing."
  }

  # The checksum above is the install gate. Attestation runs only when gh is
  # logged in. Anything that stops the check (no gh, logged out, offline, or
  # an older gh) is one warning, and the install continues. A verification
  # that runs and rejects the file does not install. -RequireAttest makes
  # that warning a failure.
  if (-not $NoAttest) {
    function Warn-Attest([string]$Text) {
      if ($RequireAttest) { throw "-RequireAttest: $Text" }
      [Console]::Error.WriteLine("install.ps1: $Text")
    }
    function Invoke-Gh([string[]]$GhArgs) {
      $prev = $ErrorActionPreference
      $ErrorActionPreference = 'Continue'
      $lines = & gh @GhArgs 2>&1
      $code = $LASTEXITCODE
      $ErrorActionPreference = $prev
      $text = @($lines | ForEach-Object { "$_" }) -join "`n"
      return @{ Code = $code; Text = $text }
    }
    function Test-Rejected([string]$Text) {
      return ($Text -match 'attestations/sha256') -or ($Text -match 'failed to verify') -or ($Text -match 'no attestation') -or ($Text -match 'did not match') -or ($Text -match 'verification failed')
    }
    $ghCmd = Get-Command gh -ErrorAction SilentlyContinue
    if (-not $ghCmd) {
      Warn-Attest 'gh is not installed, so the build-provenance attestation was not checked. The SHA-256 matched.'
    } else {
      $auth = Invoke-Gh @('auth', 'status')
      if ($auth.Code -ne 0) {
        Warn-Attest 'gh is not logged in, so the build-provenance attestation was not checked. The SHA-256 matched.'
      } else {
        $help = Invoke-Gh @('attestation', 'verify', '--help')
        if ($help.Code -ne 0) {
          Warn-Attest 'this gh has no attestation command, so the build-provenance attestation was not checked. The SHA-256 matched.'
        } else {
          $verify = Invoke-Gh @('attestation', 'verify', $tmp, '--repo', $Repo)
          if ($verify.Code -ne 0) {
            if (Test-Rejected $verify.Text) {
              throw "gh attestation verify rejected this binary. Not installing.`n$($verify.Text)"
            }
            Warn-Attest 'gh attestation verify could not run, so the build-provenance attestation was not checked. The SHA-256 matched.'
          }
        }
      }
    }
  }

  New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
  $dest = Join-Path $Prefix 'bwn.exe'
  $alias = Join-Path $Prefix 'buildwithnexus.exe'
  Copy-Item -LiteralPath $tmp -Destination $dest -Force
  Copy-Item -LiteralPath $dest -Destination $alias -Force

  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  $already = $false
  if ($userPath) {
    foreach ($part in ($userPath -split ';')) {
      if ($part -and ($part.TrimEnd('\') -eq $Prefix.TrimEnd('\'))) { $already = $true }
    }
  }
  if (-not $already) {
    $joined = if ($userPath) { "$userPath;$Prefix" } else { $Prefix }
    [Environment]::SetEnvironmentVariable('Path', $joined, 'User')
    Say "Added $Prefix to your user PATH. Open a new window before bwn works there."
  } else {
    Say "bwn is already on your user PATH"
  }
  $env:Path = "$Prefix;" + $env:Path
  Say "installed $dest"
  & $dest version
  if ($LASTEXITCODE -ne 0) { throw "installed bwn version exited $LASTEXITCODE" }
} finally {
  Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
  Remove-Item -LiteralPath "$tmp.sha256" -Force -ErrorAction SilentlyContinue
}
