<#
Opens bwn's first-run setup in a classic console window (conhost running
cmd.exe, as a user's cmd window does) and reads back what the console holds.
Fails when colour codes show up as text (an arrow, then [38;2;...m), which
is what a console does with them when the program has not turned on
virtual-terminal processing.

  powershell -NoProfile -File scripts\check-console-colors.ps1 -Command <exe or .cmd>

-Command is what the user types: a buildwithnexus.exe, or the npm shim
buildwithnexus.cmd (set BWN_BIN first to run a local build through it). The
run gets a fresh NEXUS_HOME and no API keys, so setup always starts, and
HKCU\Console\VirtualTerminalLevel is 0 for the window, the default on a
machine nobody has tuned. The report prints the console's output mode and
the first lines as the user sees them, with ESC shown as the arrow the
console draws for it.

Must stay Windows PowerShell 5.1 compatible (C# 5 in Add-Type), like
scripts\field\run-blocks.ps1.
#>
param(
    [Parameter(Mandatory = $true)][string]$Command,
    [int]$TimeoutSec = 180,
    [string]$Ready = 'provider number or name'
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

# Reading another process's console means leaving this one (FreeConsole,
# then AttachConsole), after which PowerShell's own output is lost. So a small
# helper exe does the reading and writes what it saw to a file.
$helperSrc = @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;

public static class ReadConsole {
    [StructLayout(LayoutKind.Sequential)] struct Coord { public short X; public short Y; }
    [StructLayout(LayoutKind.Sequential)] struct SmallRect { public short Left; public short Top; public short Right; public short Bottom; }
    [StructLayout(LayoutKind.Sequential)] struct BufferInfo { public Coord Size; public Coord Cursor; public ushort Attributes; public SmallRect Window; public Coord MaxSize; }

    [DllImport("kernel32.dll", SetLastError = true)] static extern bool FreeConsole();
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool AttachConsole(uint pid);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr CreateFileW(string name, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetConsoleScreenBufferInfo(IntPtr h, out BufferInfo info);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetConsoleMode(IntPtr h, out uint mode);
    // CharSet.Unicode: without it the char[] is marshalled as one byte per
    // character and the W call writes past its end.
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool ReadConsoleOutputCharacterW(IntPtr h, [Out] char[] text, uint length, Coord at, out uint read);

    // readconsole <pid> <file>: writes the console that process <pid> is
    // attached to into <file>: its output mode on the first line
    // ("mode=0x..."), then one line per row up to the cursor. Exit 0 on
    // success, 2 when it cannot attach or read.
    public static int Main(string[] args) {
        uint pid = uint.Parse(args[0]);
        FreeConsole();
        if (!AttachConsole(pid)) return 2;
        // GENERIC_READ | GENERIC_WRITE, shared, OPEN_EXISTING
        IntPtr h = CreateFileW("CONOUT$", 0xC0000000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero);
        if (h == new IntPtr(-1)) return 2;
        try {
            BufferInfo info;
            if (!GetConsoleScreenBufferInfo(h, out info)) return 2;
            uint mode;
            GetConsoleMode(h, out mode);
            StringBuilder all = new StringBuilder();
            all.Append("mode=0x").Append(mode.ToString("x")).Append('\n');
            char[] row = new char[info.Size.X];
            for (short y = 0; y <= info.Cursor.Y; y++) {
                Coord at;
                at.X = 0;
                at.Y = y;
                uint n;
                if (!ReadConsoleOutputCharacterW(h, row, (uint)row.Length, at, out n)) n = 0;
                all.Append(new string(row, 0, (int)n).TrimEnd()).Append('\n');
            }
            File.WriteAllText(args[1], all.ToString(), new UTF8Encoding(false));
            return 0;
        } finally {
            CloseHandle(h);
        }
    }
}
'@

$esc = [string][char]0x1B
$arrow = [string][char]0x2190

# A fresh first run: no settings, no keys, no colour overrides.
$work = Join-Path ([IO.Path]::GetTempPath()) ('bwn-console-' + [Guid]::NewGuid().ToString('N'))
$home_ = Join-Path $work 'home'
New-Item -ItemType Directory -Force -Path $home_ | Out-Null
$helper = Join-Path $work 'readconsole.exe'
$screen = Join-Path $work 'screen.txt'
Add-Type -TypeDefinition $helperSrc -OutputAssembly $helper -OutputType ConsoleApplication
$env:NEXUS_HOME = $home_
foreach ($name in @(Get-ChildItem Env: | ForEach-Object { $_.Name })) {
    if ($name -match '_API_KEY$|_API_TOKEN$|^(NO_COLOR|FORCE_COLOR|COLORTERM|TERM|WT_SESSION)$') {
        Remove-Item -LiteralPath ('Env:' + $name)
    }
}

# conhost reads VirtualTerminalLevel when it creates the window.
$consoleKey = 'HKCU:\Console'
if (-not (Test-Path -LiteralPath $consoleKey)) { New-Item -Path $consoleKey | Out-Null }
$before = (Get-ItemProperty -LiteralPath $consoleKey).PSObject.Properties['VirtualTerminalLevel']
Set-ItemProperty -LiteralPath $consoleKey -Name VirtualTerminalLevel -Value 0 -Type DWord

$conhost = Join-Path $env:SystemRoot 'System32\conhost.exe'
$cmd = Join-Path $env:SystemRoot 'System32\cmd.exe'
$p = Start-Process -FilePath $conhost -ArgumentList ('"' + $cmd + '" /d /c "' + $Command + '"') -PassThru
Write-Output ('console-check: conhost pid ' + $p.Id + ' running ' + $Command)

$text = $null
$attachTo = 0
$helperExit = 'not run'
try {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 1000
        if ($attachTo -eq 0) {
            $child = Get-CimInstance Win32_Process -Filter ("ParentProcessId=" + $p.Id) | Select-Object -First 1
            if ($child) { $attachTo = [uint32]$child.ProcessId }
            continue
        }
        $r = Start-Process -FilePath $helper -ArgumentList $attachTo, ('"' + $screen + '"') -WindowStyle Hidden -Wait -PassThru
        $helperExit = $r.ExitCode
        if ($r.ExitCode -eq 0) { $text = [IO.File]::ReadAllText($screen) }
        if ($text -and $text.Contains($Ready)) { break }
    }
} finally {
    & taskkill.exe /T /F /PID $p.Id 2>&1 | Out-Null
    if ($before) {
        Set-ItemProperty -LiteralPath $consoleKey -Name VirtualTerminalLevel -Value $before.Value -Type DWord
    } else {
        Remove-ItemProperty -LiteralPath $consoleKey -Name VirtualTerminalLevel -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Output ('console-check: read the console of pid ' + $attachTo + ', helper exit ' + $helperExit)
if (-not $text) {
    Write-Output 'console-check: could not read the console window'
    exit 1
}
$lines = $text -split "`n"
$mode = [Convert]::ToUInt32($lines[0].Substring('mode=0x'.Length), 16)
$shown = (($lines | Select-Object -Skip 1) -join "`n").Replace($esc, $arrow)
$raw = [regex]::IsMatch($shown, [regex]::Escape($arrow) + '\[[0-9;]*[A-Za-z]')
$ready = $shown.Contains($Ready)
Write-Output ('vt-processing=' + $(if ($mode -band 4) { 'on' } else { 'off' }))
Write-Output ('raw-codes=' + $raw)
Write-Output ('setup-prompt=' + $ready)
Write-Output ('--- console (ESC shown as ' + $arrow + ') ---')
$shown -split "`n" | Where-Object { $_.Trim() } | Select-Object -First 14 | ForEach-Object { Write-Output $_ }
if ($raw -or -not $ready) { exit 1 }
exit 0
