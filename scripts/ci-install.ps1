param(
  [Parameter(Mandatory = $true)][string]$Binary
)
$ErrorActionPreference = 'Stop'
$prefix = Join-Path $env:RUNNER_TEMP 'bwn-prefix'
if (Test-Path -LiteralPath $prefix) { Remove-Item -LiteralPath $prefix -Recurse -Force }
$hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $Binary).Hash.ToLower()
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "$PSScriptRoot\install.ps1" -File $Binary -Sha256 $hash -Prefix $prefix -NoAttest
$bwn = Join-Path $prefix 'bwn.exe'
$out = & $bwn version
Write-Output $out
if ($LASTEXITCODE -ne 0) { throw "bwn version exited $LASTEXITCODE" }
if ("$out" -notmatch 'buildwithnexus') { throw "unexpected version output: $out" }
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "$PSScriptRoot\install.ps1" -Uninstall -Prefix $prefix
if (Test-Path -LiteralPath $bwn) { throw "uninstall left $bwn" }
Write-Output 'installer ok'
