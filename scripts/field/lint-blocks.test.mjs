// Tests for the block lint. The chat-0929-* fixtures hold the command blocks,
// as sent, from the 09-29 replies whose commands failed on real machines (U07,
// U10, U25-U28, U30 in the field-testing issue list), plus corrected versions
// that must come back clean. The bad-*/fixed-* fixtures are shorter
// reconstructions.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { classify, extractBlocks } from './extract-blocks.mjs';
import { lintFile, lintText, RULES } from './lint-blocks.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const cli = join(here, 'lint-blocks.mjs');
const fx = (name) => join(here, 'fixtures', name);
const lineText = (file, n) => readFileSync(file, 'utf8').split('\n')[n - 1] ?? '';
const fence = (lang, body) => `\`\`\`${lang}\n${body}\n\`\`\`\n`;
const chat = (text) => lintText(text, { file: 'chat.txt', format: 'chat' });
const ids = (findings) => findings.map((f) => f.rule);

function expectAt(findings, file, rule, snippet) {
  const hit = findings.find((f) => f.rule === rule && lineText(file, f.line).includes(snippet));
  assert.ok(hit, `${rule} not reported on a line containing ${JSON.stringify(snippet)}; got ${JSON.stringify(findings.map((f) => [f.line, f.rule]))}`);
  return hit;
}

// ------------------------------------------------------------ the 09-29 command blocks, as sent

// [issue, fixture, rule, text on the reported line, how many times in the fixture]
const VERBATIM = [
  ['U07', 'chat-0929-wsl.txt', 'set-e-exec', 'exec bwn --provider ollama --model "$MODEL"', 1],
  ['U08', 'chat-0929-wsl.txt', 'background-no-ready', 'OLLAMA_MODELS="$MODELS" nohup ollama serve', 2],
  ['U10', 'chat-0929-wsl.txt', 'npm-prefix', 'npm config set prefix ~/.npm-global', 3],
  ['U25', 'chat-0929-windows.txt', 'winget-unguarded', 'winget install --id Microsoft.WindowsTerminal', 6],
  ['U26', 'chat-0929-windows.txt', 'ps51-json-pipeline', '$v = (Invoke-RestMethod https://nodejs.org/dist/index.json | Where-Object', 2],
  ['U26', 'chat-0929-windows.txt', 'ps-unassigned-var', 'Invoke-WebRequest -UseBasicParsing "https://nodejs.org/dist/$v/node-$v-x64.msi"', 3],
  ['U27', 'chat-0929-windows.txt', 'machine-scope-unelevated', ";C:\\tools\\ffmpeg\\bin;C:\\tools\\ripgrep', 'Machine')", 3],
  ['U28', 'chat-0929-windows.txt', 'msiexec-bare', "Start-Process msiexec.exe -ArgumentList '/i','node.msi','/qn' -Wait", 5],
  ['U28', 'chat-0929-windows.txt', 'msiexec-bare', '-Verb RunAs -Wait', 5],
  ['U30', 'chat-0929-windows.txt', 'defender-first', 'Get-MpThreatDetection | Sort-Object InitialDetectionTime', 4],
];

for (const [id, name, rule, snippet, count] of VERBATIM) {
  test(`${id}: ${rule} in the 09-29 blocks of ${name}`, () => {
    const file = fx(name);
    const findings = lintFile(file);
    const hit = expectAt(findings, file, rule, snippet);
    assert.equal(hit.severity, 'error');
    assert.equal(findings.filter((f) => f.rule === rule && f.severity === 'error').length, count, `every ${rule} in ${name}`);
  });
}

test('the 09-29 blocks: no errors from rules they do not break', () => {
  const errors = (name) => lintFile(fx(name)).filter((f) => f.severity === 'error');
  const wsl = errors('chat-0929-wsl.txt');
  assert.deepEqual([...new Set(ids(wsl))].sort(), ['background-no-ready', 'npm-prefix', 'set-e-exec']);
  const win = errors('chat-0929-windows.txt');
  assert.deepEqual(
    [...new Set(ids(win))].sort(),
    ['background-no-ready', 'defender-first', 'machine-scope-unelevated', 'msiexec-bare', 'ps-unassigned-var', 'ps51-json-pipeline', 'winget-unguarded'],
  );
  // The later reply that splits the ripgrep download reads a property of a
  // parenthesized call, which 5.1 enumerates; it must stay clean.
  const file = fx('chat-0929-windows.txt');
  assert.ok(!win.some((f) => lineText(file, f.line).includes('$rg = (Invoke-RestMethod')));
});

test('the corrected versions pass, each section pasted on its own', () => {
  const text = readFileSync(fx('chat-0929-fixed.txt'), 'utf8');
  const sections = new Map(text.split(/^(?=### U\d+)/m).slice(1).map((s) => [s.slice(4, 7), s]));
  assert.deepEqual([...sections.keys()], [...new Set(VERBATIM.map(([id]) => id))]);
  for (const [id, section] of sections) {
    const blocks = extractBlocks(section, { format: 'chat' }).filter((b) => b.lang);
    assert.ok(blocks.length > 0, `${id} has a command block`);
    assert.deepEqual(lintText(section, { file: id, format: 'chat' }), [], id);
  }
  assert.deepEqual(lintFile(fx('chat-0929-fixed.txt')), []);
});

// ------------------------------------------------------------ the reconstructions

test('U07/U08/U10: the one-paste WSL setup script', () => {
  const file = fx('bad-wsl-setup.sh');
  const findings = lintFile(file);
  expectAt(findings, file, 'set-e-exec', 'exec bwn');
  const bg = expectAt(findings, file, 'background-no-ready', 'nohup ollama serve');
  assert.match(bg.message, /gives up silently/);
  expectAt(findings, file, 'npm-prefix', 'npm config set prefix');
  assert.ok(findings.every((f) => f.severity === 'error'));
});

test('U25-U28, U30: the Windows Server 2022 steps', () => {
  const file = fx('bad-windows-server.txt');
  const findings = lintFile(file);
  expectAt(findings, file, 'winget-unguarded', 'winget install'); // U25
  expectAt(findings, file, 'ps51-json-pipeline', '$v = (Invoke-RestMethod'); // U26
  const v = expectAt(findings, file, 'ps-unassigned-var', 'node-$v-x64.msi'); // U26
  assert.match(v.message, /^\$v /);
  expectAt(findings, file, 'machine-scope-unelevated', "'Machine')"); // U27
  const msi = expectAt(findings, file, 'msiexec-bare', 'Start-Process msiexec.exe'); // U28
  assert.match(msi.message, /-PassThru/);
  assert.match(msi.message, /`node` runs at line \d+ before \$env:Path is refreshed/);
  expectAt(findings, file, 'defender-first', 'Get-MpThreatDetection'); // U30
  expectAt(findings, file, 'defender-first', 'Add-MpPreference');
  expectAt(findings, file, 'background-no-ready', 'Start-Process ollama');
});

test('the fixed versions of those steps pass with no findings at all', () => {
  assert.deepEqual(lintFile(fx('fixed-wsl.txt')), []);
  assert.deepEqual(lintFile(fx('fixed-windows-server.txt')), []);
});

test('HTML pages: <pre><code> blocks, entities, line numbers and comment escapes', () => {
  const file = fx('docs-page.html');
  const blocks = extractBlocks(readFileSync(file, 'utf8'), { file, format: 'html' });
  assert.deepEqual(
    blocks.map((b) => [b.startLine, b.lang]),
    [
      [5, 'any'],
      [7, 'ps'],
      [9, null],
      [10, null],
      [12, 'sh'],
      [15, 'ps'],
    ],
  );
  assert.equal(blocks[1].code.split('\n')[1], 'Write-Output "Node $v"');
  const findings = lintFile(file);
  expectAt(findings, file, 'ps-unassigned-var', '$missing');
  assert.ok(!ids(findings).includes('npm-prefix'), 'the <!-- lint-ok --> comment should suppress it');
  assert.ok(!findings.some((f) => f.rule === 'ps-unassigned-var' && f.message.startsWith('$v ')));
  // A numeric reference past U+10FFFF is kept as written instead of throwing.
  const [odd] = extractBlocks('<pre><code>npm i -g x &#x110000; &#128512;</code></pre>', { format: 'html' });
  assert.equal(odd.code, 'npm i -g x &#x110000; \u{1F600}');
});

// ------------------------------------------------------------ extraction

test('Markdown fences: indented, tilde, console prompts, unlabeled commands', () => {
  const md = [
    '1. In a list:', //                                      1
    '   ```powershell', //                                   2
    '   $x = 1', //                                          3
    '   Write-Output $y', //                                 4
    '   ```', //                                             5
    '~~~sh', //                                              6
    'echo hi', //                                            7
    '~~~', //                                                8
    '```console', //                                         9
    '$ npm install -g buildwithnexus', //                   10
    'added 1 package in 2s', //                             11
    '$ bwn --version', //                                   12
    '```', //                                               13
    '```', //                                               14
    '/model gpt-4o', //                                     15
    '```', //                                               16
    '```', //                                               17
    'npm install -g buildwithnexus', //                     18
    '```', //                                               19
    '```json', //                                           20
    '{ "a": 1 }', //                                        21
    '```', //                                               22
  ].join('\n');
  const blocks = extractBlocks(md, { file: 'x.md' });
  assert.deepEqual(
    blocks.map((b) => [b.startLine, b.label, b.lang]),
    [
      [3, 'powershell', 'ps'],
      [7, 'sh', 'sh'],
      [10, 'console', 'any'],
      [15, '', null],
      [18, '', 'any'],
      [21, 'json', null],
    ],
  );
  assert.equal(blocks[0].code, '$x = 1\nWrite-Output $y');
  assert.equal(blocks[2].code.split('\n')[1], '', 'console output lines are blanked');
  const findings = lintText(md, { file: 'x.md' });
  assert.deepEqual(
    findings.map((f) => [f.line, f.rule]),
    [
      [4, 'ps-unassigned-var'],
      [18, 'no-check-at-end'],
    ],
  );
});

test('shell detection for unlabeled and generic blocks', () => {
  assert.equal(classify('', 'npm install -g buildwithnexus'), 'any');
  assert.equal(classify('', '$v = 1; $v'), 'ps');
  assert.equal(classify('', 'Get-ChildItem C:\\tools'), 'ps');
  assert.equal(classify('shell', 'Invoke-RestMethod http://localhost:11434/api/version'), 'ps');
  assert.equal(classify('', 'sudo apt-get install -y curl'), 'sh');
  assert.equal(classify('', 'curl -fsSL https://x.invalid/a.sh | sh && echo ok'), 'sh');
  assert.equal(classify('', '/model gpt-4o'), null);
  assert.equal(classify('', '> what is in @a.png?'), null);
  assert.equal(classify('', '~/.buildwithnexus/skills/   ./.buildwithnexus/skills/'), null);
  assert.equal(classify('text', 'npm install -g x'), null);
  assert.equal(classify('bash', 'Get-ChildItem'), 'sh', 'an explicit label wins');
  // Diagrams and sentences whose first word happens to be a command name.
  const diagram = 'buildwithnexus CLI (TypeScript/Node.js)\n         │\n         ▼\nNEXUS Backend (Python FastAPI, port 4200)\n  • ML & Data → 6 agents';
  assert.equal(classify('', diagram), null);
  assert.deepEqual(lintText(fence('', diagram), { file: 'x.md' }), []);
  assert.equal(classify('', 'bwn --version\nThen open a new terminal and run it again'), null);
  assert.equal(classify('', 'bwn run "list files"   # → prints the answer'), 'any');
  assert.notEqual(classify('', "cat > notes.md <<'EOF'\nSome Notes For Later\nEOF\nbwn --version"), null);
});

test('heredoc scripts are linted as their own block, at their own lines', () => {
  const text = fence('bash', ["cat > ~/s.sh <<'EOF'", '#!/usr/bin/env bash', 'set -e', 'mkdir -p ~/x', 'cd ~/x', 'exec bwn', 'EOF', 'bash ~/s.sh'].join('\n'));
  assert.deepEqual(
    chat(text).map((f) => [f.line, f.rule]),
    [[7, 'set-e-exec']],
  );
});

// ------------------------------------------------------------ rules, one by one

test('ps-unassigned-var ignores automatic, env, single-quoted and loop variables', () => {
  const ok = [
    '$env:BWN_IMAGES = "sixel"; bwn --version',
    'Write-Output $PSVersionTable.PSVersion $HOME $PWD $LASTEXITCODE $true $false $null $args',
    "$ErrorActionPreference = 'Stop'; $ProgressPreference = 'SilentlyContinue'; Write-Output $ErrorActionPreference",
    "Get-ChildItem | ForEach-Object { $_.Name }; Write-Output 'literal $notavar'",
    'foreach ($f in Get-ChildItem) { $f.Name }',
    'Get-Process -OutVariable procs | Out-Null; $procs.Count',
    'function Show($name) { "hi $name" }',
    '$a, $b = 1, 2; "$a $b"',
    '$s = "x"; "${s}y $($s.Length)"',
    'function global:deactivate ([switch]$NonDestructive) { if (-not $NonDestructive) { Remove-Item function:deactivate } }',
    'filter script:Show-Line([string]$Prefix) { "$Prefix $_" }',
    '[CmdletBinding()]\nparam(\n  [Parameter(Mandatory = $false)]\n  [String]\n  $VenvDir\n)\nWrite-Output $VenvDir',
  ];
  for (const body of ok) assert.deepEqual(ids(chat(fence('powershell', body))), [], body);
  const bad = chat(fence('powershell', 'Invoke-WebRequest "https://x.invalid/$ver/a.zip" -OutFile a.zip; Test-Path a.zip; "$($other.Name)"'));
  assert.deepEqual(
    bad.filter((f) => f.rule === 'ps-unassigned-var').map((f) => f.message.split(' ')[0]),
    ['$ver', '$other'],
  );
});

test('winget-unguarded needs both a Get-Command guard and a fallback', () => {
  assert.deepEqual(ids(chat(fence('powershell', 'winget install --id Gyan.FFmpeg -e'))).filter((r) => r !== 'no-check-at-end'), ['winget-unguarded']);
  const guardOnly = chat(fence('powershell', 'if (Get-Command winget -ErrorAction SilentlyContinue) { winget install --id Gyan.FFmpeg -e }\nffmpeg -version'));
  assert.match(guardOnly.find((f) => f.rule === 'winget-unguarded').message, /no fallback/);
  const guarded = 'if (Get-Command winget -ErrorAction SilentlyContinue) { winget install --id Gyan.FFmpeg -e } else { Write-Output "download the zip" }\nGet-Command ffmpeg';
  assert.deepEqual(ids(chat(fence('powershell', guarded))), []);
  assert.deepEqual(ids(chat(fence('powershell', 'Write-Output "winget install is not available here"'))), []);
  // In PowerShell `where` is Where-Object and `||` does not parse, so neither
  // is a guard or a fallback there; in cmd both are.
  const whereGuard = chat(fence('powershell', 'if (where winget) { winget install --id Gyan.FFmpeg -e } else { Write-Output "download the zip" }\nGet-Command ffmpeg'));
  assert.match(whereGuard.find((f) => f.rule === 'winget-unguarded').message, /without checking/);
  const orFallback = chat(fence('powershell', 'Get-Command winget -ErrorAction Stop\nwinget install --id Gyan.FFmpeg -e || Write-Output "download the zip"\nGet-Command ffmpeg'));
  assert.match(orFallback.find((f) => f.rule === 'winget-unguarded').message, /no fallback/);
  assert.ok(ids(orFallback).includes('ps51-chain-operators'));
  assert.deepEqual(ids(chat(fence('powershell', 'if (where.exe winget) { winget install --id Gyan.FFmpeg -e } else { Write-Output "download the zip" }\nGet-Command ffmpeg'))), []);
  assert.deepEqual(ids(chat(fence('cmd', 'where winget && winget install --id Gyan.FFmpeg -e || echo download the zip\nwhere ffmpeg'))), []);
});

test('machine-scope-unelevated: Machine and HKLM writes need an elevation check', () => {
  const machine = "[Environment]::SetEnvironmentVariable('Path', $env:Path + ';C:\\t', 'Machine')";
  assert.ok(ids(chat(fence('powershell', machine))).includes('machine-scope-unelevated'));
  assert.ok(ids(chat(fence('powershell', "Set-ItemProperty -Path 'HKLM:\\SOFTWARE\\X' -Name A -Value 1"))).includes('machine-scope-unelevated'));
  assert.ok(ids(chat(fence('cmd', 'reg add HKLM\\SOFTWARE\\X /v A /d 1 /f'))).includes('machine-scope-unelevated'));
  assert.ok(ids(chat(fence('cmd', 'setx /M PATH "%PATH%;C:\\t"'))).includes('machine-scope-unelevated'));
  const user = "[Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path','Machine') + ';C:\\t', 'User')";
  assert.ok(!ids(chat(fence('powershell', user))).includes('machine-scope-unelevated'), 'reading Machine scope is fine');
  const checked = `#Requires -RunAsAdministrator\n${machine}`;
  assert.ok(!ids(chat(fence('powershell', checked))).includes('machine-scope-unelevated'));
});

test('msiexec-bare: direct, no -Wait, no exit-code check, no PATH refresh', () => {
  const msg = (body) => chat(fence('powershell', body)).find((f) => f.rule === 'msiexec-bare')?.message ?? '';
  assert.match(msg('msiexec /i node.msi /qn'), /run directly/);
  assert.match(msg("Start-Process msiexec.exe -ArgumentList '/i','node.msi','/qn' -PassThru"), /no -Wait/);
  assert.match(msg("$p = Start-Process msiexec.exe -ArgumentList '/i','node.msi' -Wait -PassThru\nnode -v"), /ExitCode is never checked/);
  const good = [
    "$p = Start-Process msiexec.exe -ArgumentList '/i','node.msi','/qn' -Wait -PassThru",
    "if ($p.ExitCode -ne 0) { throw \"exit $($p.ExitCode)\" }",
    "$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')",
    'node -v',
  ].join('\n');
  assert.equal(msg(good), '');
  assert.deepEqual(ids(chat(fence('powershell', good))), []);
  // The exit code read inline from a parenthesized Start-Process is a check too.
  const refresh = "$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')";
  const inlineIf = "if ((Start-Process msiexec.exe -ArgumentList '/i','node.msi','/qn' -Wait -PassThru).ExitCode -ne 0) { throw 'node.msi failed' }";
  const inlineVar = "$code = (Start-Process msiexec.exe -ArgumentList '/i','node.msi','/qn' -Wait -PassThru).ExitCode; if ($code -ne 0) { throw \"exit $code\" }";
  assert.equal(msg(`${inlineIf}\n${refresh}\nnode -v`), '');
  assert.equal(msg(`${inlineVar}\n${refresh}\nnode -v`), '');
  assert.match(msg(`${inlineIf}\nnode -v`), /before \$env:Path is refreshed/);
  assert.match(msg("if ((Start-Process msiexec.exe -ArgumentList '/i','node.msi' -PassThru).ExitCode -ne 0) { throw 'x' }"), /no -Wait/);
  assert.match(msg("if (Test-Path node.msi) { msiexec /i node.msi /qn }"), /run directly/);
});

test('ps51-json-pipeline: only an unwrapped call piped into a filter', () => {
  const sev = (body) => chat(fence('powershell', body)).find((f) => f.rule === 'ps51-json-pipeline')?.severity;
  const has = (body) => sev(body) !== undefined;
  // An error for an array endpoint or a filter that only makes sense on many items.
  assert.equal(sev('Invoke-RestMethod https://x.invalid/i.json | Where-Object lts'), 'error');
  assert.equal(sev('irm https://x.invalid/i.json |\n  Select-Object -First 1'), 'error');
  assert.equal(sev('irm https://api.github.com/repos/o/r/releases | Select-Object -ExpandProperty tag_name'), 'error');
  assert.equal(sev("irm 'https://api.github.com/repos/o/r/releases?per_page=5' | ForEach-Object { $_.tag_name }"), 'error');
  assert.equal(sev('irm https://nodejs.org/dist/index.json | ForEach-Object { $_.version }'), 'error');
  // A warning when it looks like one object, which 5.1 handles fine.
  assert.equal(sev('irm https://api.github.com/repos/o/r/releases/latest | Select-Object -ExpandProperty tag_name'), 'warning');
  assert.equal(sev('Get-Content a.json | ConvertFrom-Json | ForEach-Object { $_.name }'), 'warning');
  assert.ok(!has('(Invoke-RestMethod https://x.invalid/i.json) | Where-Object lts'));
  assert.ok(!has('(Invoke-RestMethod https://x.invalid/r.json).assets | Where-Object name -like "*.zip"'));
  assert.ok(!has('$j = Get-Content a.json | ConvertFrom-Json; $j | Where-Object lts'));
  assert.ok(!has('Invoke-RestMethod http://localhost:11434/api/version'));
});

test('ps51-chain-operators: && and || do not parse in Windows PowerShell 5.1', () => {
  const rules = (lang, body) => ids(chat(fence(lang, body)));
  assert.deepEqual(rules('powershell', 'node -v && npm -v'), ['ps51-chain-operators']);
  assert.deepEqual(rules('powershell', 'Test-Path C:\\x || Write-Output missing'), ['ps51-chain-operators']);
  const two = chat(fence('powershell', 'node -v && npm -v\nbwn --version || exit 1'));
  assert.deepEqual(two.map((f) => [f.line, f.rule]), [[2, 'ps51-chain-operators'], [3, 'ps51-chain-operators']]);
  assert.match(two[0].message, /not a valid statement separator/);
  assert.deepEqual(rules('powershell', "cmd /c 'node -v && npm -v'\n# a && b\nWrite-Output \"a || b\""), []);
  assert.deepEqual(rules('powershell', 'node -v; if ($LASTEXITCODE -eq 0) { npm -v }'), []);
  assert.deepEqual(rules('bash', 'node -v && npm -v'), []);
  assert.deepEqual(rules('cmd', 'node -v && npm -v'), []);
});

test('ps51-curl-alias: curl and wget are Invoke-WebRequest in Windows PowerShell 5.1', () => {
  const lines = (body) => chat(fence('powershell', body)).filter((f) => f.rule === 'ps51-curl-alias').map((f) => f.line);
  assert.deepEqual(lines('curl -s http://localhost:11434/api/version'), [2]);
  assert.deepEqual(lines('$r = curl https://x.invalid/a\nwget https://x.invalid/b -O b'), [2, 3]);
  assert.deepEqual(lines('try { curl http://localhost:11434/api/version } catch { Write-Output down }'), [2]);
  assert.deepEqual(lines('curl.exe -s http://localhost:11434/api/version'), []);
  assert.deepEqual(lines("Remove-Item alias:curl; Get-Command curl; Write-Output 'curl -s x' # curl -s"), []);
  assert.match(chat(fence('powershell', 'curl -s http://x.invalid')).find((f) => f.rule === 'ps51-curl-alias').message, /-SessionVariable/);
  assert.deepEqual(ids(chat(fence('bash', 'curl -s http://localhost:11434/api/version'))), []);
});

test('npm-prefix: every way of persisting a prefix, but not reading it', () => {
  const has = (body) => ids(chat(fence('bash', body))).includes('npm-prefix');
  assert.ok(has('npm config set prefix ~/.npm-global'));
  assert.ok(has('npm set prefix=$HOME/.npm'));
  assert.ok(has("echo 'prefix=~/.npm-global' >> ~/.npmrc"));
  assert.ok(has("echo 'export NPM_CONFIG_PREFIX=~/.npm-global' >> ~/.bashrc"));
  assert.ok(!has('npm config get prefix'));
  assert.ok(!has("echo 'npm config set prefix breaks nvm'"));
});

test('background-no-ready: foreground servers, races, silent waits and later blocks', () => {
  const rules = (text) => ids(chat(text));
  assert.ok(rules(fence('bash', 'ollama serve\nollama pull llama3.2')).includes('background-no-ready'));
  assert.deepEqual(rules(fence('bash', 'ollama serve')), []);
  assert.ok(rules(fence('bash', 'nohup ollama serve > s.log 2>&1 &\ncurl http://localhost:11434/api/version')).includes('background-no-ready'));
  const waited = 'nohup ollama serve > s.log 2>&1 &\nfor _ in $(seq 1 30); do curl -sf http://localhost:11434/api/version && break; sleep 1; done\nollama pull llama3.2';
  assert.deepEqual(rules(fence('bash', waited)), [], 'without set -e the reader sees the wait fail');
  assert.ok(rules(fence('bash', `set -e\n${waited}`)).includes('background-no-ready'));
  const gated = 'set -e\nnohup ollama serve > s.log 2>&1 &\nuntil curl -sf http://localhost:11434/api/version; do sleep 1; done\nollama pull llama3.2';
  assert.deepEqual(rules(fence('bash', gated)), []);
  const acrossChecked = fence('bash', 'nohup ollama serve > s.log 2>&1 &') + fence('bash', 'curl http://localhost:11434/api/version') + fence('bash', 'ollama pull llama3.2');
  assert.deepEqual(rules(acrossChecked), []);
  const acrossUnchecked = fence('bash', 'nohup ollama serve > s.log 2>&1 &') + fence('bash', 'ollama pull llama3.2');
  assert.deepEqual(rules(acrossUnchecked), ['background-no-ready']);
  const stopLater = fence('bash', 'nohup ollama serve > s.log 2>&1 &') + fence('bash', 'ollama stop llama3.2');
  assert.deepEqual(rules(stopLater), []);
  const psNoWait = fence('powershell', 'try { Invoke-RestMethod http://localhost:11434/api/version } catch { Start-Process ollama -ArgumentList serve -WindowStyle Hidden }\nollama pull llama3.2');
  assert.deepEqual(rules(psNoWait), ['background-no-ready']);
});

test('set-e-exec: only a multi-step set -e script whose last command is exec', () => {
  const rules = (body) => ids(chat(fence('bash', body)));
  assert.deepEqual(rules('set -euo pipefail\nmkdir -p a\ncd a\nexec bwn --version'), ['set-e-exec']);
  assert.deepEqual(rules('set -e\nmkdir -p a\ncd a\nbwn --version'), []);
  assert.deepEqual(rules('mkdir -p a\ncd a\nexec bwn'), []);
  assert.deepEqual(rules('set -e\nmkdir -p a\ncd a\nexec > log 2>&1'), []);
});

test('defender-first: Defender cmdlets only after the product is identified', () => {
  const triage = fence('powershell', 'Get-MpThreatDetection | Select-Object -Last 3');
  assert.deepEqual(ids(chat(triage)), ['defender-first']);
  const after = (lang, body) => ids(chat(fence(lang, body) + triage));
  assert.deepEqual(after('powershell', "Get-Service | Where-Object DisplayName -match 'CrowdStrike|Falcon|Defender' | Select-Object Status, DisplayName"), []);
  assert.deepEqual(after('powershell', 'sc.exe query csagent'), []);
  assert.deepEqual(after('cmd', 'sc query csagent'), []);
  assert.deepEqual(after('powershell', "Test-Path 'C:\\Program Files\\CrowdStrike'"), []);
  // root/SecurityCenter2 does not exist on Windows Server, `sc` is Set-Content
  // in PowerShell, and a Defender-only check names no other product.
  assert.deepEqual(after('powershell', 'Get-CimInstance -Namespace root/SecurityCenter2 -ClassName AntiVirusProduct'), ['defender-first']);
  assert.deepEqual(after('powershell', "Get-CimInstance -Namespace root/SecurityCenter2 -ClassName AntiVirusProduct | Where-Object displayName -match 'CrowdStrike'"), ['defender-first']);
  assert.deepEqual(after('powershell', 'sc query csagent'), ['defender-first']);
  assert.deepEqual(after('powershell', 'Get-Service WinDefend'), ['defender-first']);
  assert.deepEqual(ids(chat(fence('cmd', 'MpCmdRun.exe -GetFiles'))), ['defender-first']);
});

test('no-check-at-end is a warning and accepts visible checks only', () => {
  const [w] = chat(fence('bash', 'npm install -g buildwithnexus'));
  assert.equal(w.rule, 'no-check-at-end');
  assert.equal(w.severity, 'warning');
  const clean = (lang, body) => assert.deepEqual(ids(chat(fence(lang, body))), [], body);
  clean('bash', 'npm install -g buildwithnexus\nbwn --version');
  clean('bash', 'sudo apt-get install -y ripgrep && rg --version');
  clean('bash', 'cargo install bwn --locked\nbwn doctor');
  clean('powershell', 'Expand-Archive rg.zip C:\\tools -Force; Test-Path C:\\tools\\rg.exe');
  clean('powershell', 'Expand-Archive rg.zip C:\\tools -Force; Get-ChildItem C:\\tools -Filter rg.exe | Select-Object FullName');
  clean('bash', 'git clone https://x.invalid/r.git\ncargo build --release');
  const silent = chat(fence('powershell', "Expand-Archive rg.zip C:\\tools -Force; Get-ChildItem C:\\tools -Filter 'ripgrep-*' | Rename-Item -NewName ripgrep"));
  assert.deepEqual(ids(silent), ['no-check-at-end']);
});

test('lint-ok escapes need a rule id and a reason', () => {
  const inline = chat(fence('bash', 'npm config set prefix ~/.npm-global # lint-ok: npm-prefix shown as the command that breaks nvm'));
  assert.deepEqual(inline, []);
  const blockWide = chat(fence('bash', '# lint-ok: npm-prefix documenting the broken setup\nnpm config set prefix ~/.npm-global'));
  assert.deepEqual(blockWide, []);
  const [noReason] = chat(fence('bash', 'npm config set prefix ~/.npm-global # lint-ok: npm-prefix'));
  assert.match(noReason.message, /needs a reason/);
  assert.deepEqual(ids(chat(fence('bash', 'npm config set prefix ~/x # lint-ok: winget-unguarded wrong rule'))), ['npm-prefix']);
  const mdComment = `<!-- lint-ok: npm-prefix quoted from the nvm FAQ -->\n${fence('bash', 'npm config set prefix ~/x')}`;
  assert.deepEqual(lintText(mdComment, { file: 'x.md' }), []);
});

// ------------------------------------------------------------ command line

test('CLI: exit codes, --json and --list-rules', () => {
  const run = (...args) => spawnSync(process.execPath, [cli, ...args], { encoding: 'utf8' });
  const bad = run(fx('bad-wsl-setup.sh'));
  assert.equal(bad.status, 1, bad.stderr);
  assert.match(bad.stdout, /bad-wsl-setup\.sh:16 set-e-exec error: /);
  assert.match(bad.stdout, /\n {2}fix: /);
  const json = run('--json', fx('bad-windows-server.txt'));
  assert.equal(json.status, 1);
  const parsed = JSON.parse(json.stdout);
  assert.ok(parsed.length >= 9);
  for (const f of parsed) assert.deepEqual(Object.keys(f).sort(), ['blockLine', 'file', 'fix', 'lang', 'line', 'message', 'rule', 'severity']);
  const good = run(fx('fixed-windows-server.txt'), fx('fixed-wsl.txt'));
  assert.equal(good.status, 0, good.stdout);
  const warnOnly = run('--chat', fx('docs-page.html'));
  assert.equal(warnOnly.status, 0, 'as chat text the page has no fences, so nothing to report');
  const rules = run('--list-rules');
  assert.equal(rules.status, 0);
  assert.equal(rules.stdout.match(/^\S+ \((?:error|warning)\): /gm).length, Object.keys(RULES).length);
  assert.equal(Object.keys(RULES).length, 12);
  assert.equal(run().status, 2);
  assert.equal(run(fx('does-not-exist.md')).status, 2);
  assert.equal(run('--bogus', fx('fixed-wsl.txt')).status, 2);
});
