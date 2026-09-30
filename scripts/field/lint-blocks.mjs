#!/usr/bin/env node
// Static lint for the command blocks people paste into a real shell: README
// and SECURITY.md fences, buildwithnexus.dev <pre><code> blocks, and chat
// instructions (--chat). Each rule comes from something that failed on a
// user's machine; the ids in catch-issues.json (U07, U10, U25-U28, U30) are
// the cases the tests replay from the 09-29 chat text. Node builtins only;
// nothing is executed.
//
//   node scripts/field/lint-blocks.mjs [--chat] [--json] <file>...
//   node scripts/field/lint-blocks.mjs --list-rules
//
// Escape a finding with a comment that gives a reason, on the offending line
// or on a comment line of its own (whole block):
//   # lint-ok: <rule-id> <reason>
// or, for Markdown/HTML, on the line before the block:
//   <!-- lint-ok: <rule-id> <reason> -->
// Exit status: 1 when any error-level finding is left, 2 on usage errors.

import { readFileSync } from 'node:fs';
import { argv, exit, stdout, stderr } from 'node:process';
import { pathToFileURL } from 'node:url';
import { parseArgs } from 'node:util';
import { extractBlocks, formatFor } from './extract-blocks.mjs';

export const RULES = {
  'ps-unassigned-var': {
    severity: 'error',
    summary: 'A PowerShell block reads a variable it does not assign.',
    fix: 'Assign it in the same block, so the block still works pasted into a new window.',
  },
  'winget-unguarded': {
    severity: 'error',
    summary: 'winget used without a Get-Command check and a fallback.',
    fix: "Windows Server has no winget: wrap it in `if (Get-Command winget -ErrorAction SilentlyContinue) { ... } else { <direct download> }`.",
  },
  'machine-scope-unelevated': {
    severity: 'error',
    summary: "Machine-scope environment or HKLM write without an elevation check.",
    fix: "Use 'User' scope, which needs no admin, or check elevation first with ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole('Administrator').",
  },
  'msiexec-bare': {
    severity: 'error',
    summary: 'msiexec without an exit-code check or a PATH refresh.',
    fix: "Use `$p = Start-Process msiexec.exe -ArgumentList ... -Wait -PassThru; if ($p.ExitCode -ne 0) { throw ... }`, then `$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')` before running the installed tool.",
  },
  'ps51-json-pipeline': {
    severity: 'error',
    summary: 'Invoke-RestMethod/ConvertFrom-Json piped straight into a filter (a warning when the call looks like it returns one object).',
    fix: 'Windows PowerShell 5.1 sends a JSON array down the pipeline as one object; wrap the call in parentheses: `(Invoke-RestMethod $url) | Where-Object ...`.',
  },
  'ps51-chain-operators': {
    severity: 'error',
    summary: '`&&` or `||` in a PowerShell block.',
    fix: 'Windows PowerShell 5.1 cannot parse them. Put the commands on separate lines and test the result: `cmd1; if ($LASTEXITCODE -eq 0) { cmd2 }` for `&&`, `if ($LASTEXITCODE -ne 0) { ... }` for `||` (`$?` for cmdlets).',
  },
  'ps51-curl-alias': {
    severity: 'error',
    summary: '`curl` or `wget` in a PowerShell block without `.exe`.',
    fix: 'In Windows PowerShell 5.1 both are aliases for Invoke-WebRequest. Call `curl.exe` (Windows 10 1803+, 11, Server 2019+), or use `Invoke-WebRequest -UseBasicParsing <url> -OutFile <file>` / `Invoke-RestMethod <url>`.',
  },
  'npm-prefix': {
    severity: 'error',
    summary: 'Changes npm\'s global prefix.',
    fix: 'Leave the prefix alone (nvm refuses to load with one set). Ask how Node is installed; with nvm/fnm/volta `npm install -g` needs no sudo, with a system Node use `sudo npm install -g`.',
  },
  'background-no-ready': {
    severity: 'error',
    summary: 'A background service is used without waiting until it is ready.',
    fix: 'After starting it, wait and fail loudly, e.g. `for _ in $(seq 1 60); do curl -sf http://localhost:11434/api/version && break; sleep 1; done; curl -sf http://localhost:11434/api/version || exit 1` (PowerShell: a loop of Invoke-RestMethod with Start-Sleep).',
  },
  'set-e-exec': {
    severity: 'error',
    summary: 'A multi-step `set -e` script ends in `exec`.',
    fix: 'Split it into numbered blocks that each end in a check, and launch the program as its own last step without exec.',
  },
  'defender-first': {
    severity: 'error',
    summary: 'Defender cmdlets before a step that identifies the security product.',
    fix: "First find out what is installed, e.g. `Get-Service | Where-Object DisplayName -match 'CrowdStrike|Falcon|SentinelOne|Sophos|Carbon Black|Cylance|Symantec|McAfee|Trellix|Trend Micro|Cortex|Defender'`; only then use Defender cmdlets. A root/SecurityCenter2 AntiVirusProduct query does not count: that namespace does not exist on Windows Server.",
  },
  'no-check-at-end': {
    severity: 'warning',
    summary: 'An install/setup block that does not end in a verification command.',
    fix: 'End the block with a check whose output the reader can compare, e.g. `bwn --version`, `node -v`, `Test-Path <file>`, `Get-Command <tool>`.',
  },
};

// ---------------------------------------------------------------- scanning

function blank(chars, from, to) {
  for (let k = from; k < to && k < chars.length; k++) if (chars[k] !== '\n') chars[k] = ' ';
}

// clean: comments and heredoc bodies blanked. bare: string contents blanked
// as well, so command words inside quotes do not count as commands. Both keep
// every offset, so a position maps back to its line.
function scanShell(code) {
  const n = code.length;
  const clean = code.split('');
  const bare = code.split('');
  const heredocs = [];
  let pending = [];
  let i = 0;
  while (i < n) {
    const c = code[i];
    if (c === '\\') {
      i += 2;
      continue;
    }
    if (c === "'") {
      const end = code.indexOf("'", i + 1);
      const stop = end < 0 ? n : end;
      blank(bare, i + 1, stop);
      i = stop + 1;
      continue;
    }
    if (c === '"') {
      let j = i + 1;
      while (j < n && code[j] !== '"') j += code[j] === '\\' ? 2 : 1;
      blank(bare, i + 1, Math.min(j, n));
      i = j + 1;
      continue;
    }
    if (c === '#' && (i === 0 || /[\s;&|(]/.test(code[i - 1]))) {
      let j = code.indexOf('\n', i);
      if (j < 0) j = n;
      blank(clean, i, j);
      blank(bare, i, j);
      i = j;
      continue;
    }
    if (c === '<' && code[i + 1] === '<' && code[i + 2] !== '<') {
      const m = /^<<(-?)[ \t]*(['"]?)([A-Za-z_][\w-]*)\2/.exec(code.slice(i, i + 80));
      if (m) {
        const ls = code.lastIndexOf('\n', i) + 1;
        const le = code.indexOf('\n', i);
        pending.push({ delim: m[3], strip: m[1] === '-', opener: code.slice(ls, le < 0 ? n : le) });
        blank(bare, i + 2, i + m[0].length);
        i += m[0].length;
        continue;
      }
    }
    if (c === '\n' && pending.length) {
      let p = i + 1;
      for (const h of pending) {
        const bodyStart = p;
        let bodyEnd = n;
        let after = n;
        while (p < n) {
          let e = code.indexOf('\n', p);
          if (e < 0) e = n;
          const text = code.slice(p, e);
          if ((h.strip ? text.replace(/^\t+/, '') : text) === h.delim) {
            bodyEnd = p;
            after = e;
            break;
          }
          p = e + 1;
        }
        heredocs.push({ ...h, bodyStart, bodyEnd, body: code.slice(bodyStart, Math.max(bodyStart, bodyEnd - 1)) });
        blank(clean, bodyStart, after);
        blank(bare, bodyStart, after);
        p = after;
      }
      pending = [];
      i = p;
      continue;
    }
    i++;
  }
  return { clean: clean.join(''), bare: bare.join(''), heredocs };
}

function scanPs(code) {
  const n = code.length;
  const clean = code.split('');
  const bare = code.split('');
  let i = 0;
  while (i < n) {
    const c = code[i];
    if (c === '`') {
      i += 2;
      continue;
    }
    if (c === '<' && code[i + 1] === '#') {
      const e = code.indexOf('#>', i + 2);
      const stop = e < 0 ? n : e + 2;
      blank(clean, i, stop);
      blank(bare, i, stop);
      i = stop;
      continue;
    }
    if (c === '#' && (i === 0 || /[\s;({|]/.test(code[i - 1]))) {
      let j = code.indexOf('\n', i);
      if (j < 0) j = n;
      blank(clean, i, j);
      blank(bare, i, j);
      i = j;
      continue;
    }
    if (c === '@' && (code[i + 1] === '"' || code[i + 1] === "'") && /^[ \t]*\n/.test(code.slice(i + 2, i + 64))) {
      const term = code.indexOf(`\n${code[i + 1]}@`, i + 2);
      const stop = term < 0 ? n : term + 1;
      blank(bare, i + 2, stop);
      i = stop + 2;
      continue;
    }
    if (c === "'") {
      let j = i + 1;
      while (j < n && !(code[j] === "'" && code[j + 1] !== "'")) j += code[j] === "'" ? 2 : 1;
      blank(bare, i + 1, j);
      i = j + 1;
      continue;
    }
    if (c === '"') {
      let j = i + 1;
      while (j < n && !(code[j] === '"' && code[j + 1] !== '"')) j += code[j] === '`' || code[j] === '"' ? 2 : 1;
      blank(bare, i + 1, j);
      i = j + 1;
      continue;
    }
    i++;
  }
  return { clean: clean.join(''), bare: bare.join(''), heredocs: [] };
}

// Splits into simple commands at ; && || | & and newlines outside ( ) and
// { }. Offsets come from `bare`; text comes from `clean`.
function splitCommands(bare, clean, lang) {
  const segs = [];
  let depth = 0;
  let start = 0;
  const push = (end, op) => {
    const raw = clean.slice(start, end);
    const lead = raw.length - raw.trimStart().length;
    if (raw.trim()) segs.push({ pos: start + lead, end, text: raw.trim(), bare: bare.slice(start, end).trim(), op });
  };
  for (let i = 0; i < bare.length; i++) {
    const c = bare[i];
    const nx = bare[i + 1];
    if (c === '\\' && lang !== 'ps') {
      i++;
      continue;
    }
    if (c === '`' && lang === 'ps') {
      i++;
      continue;
    }
    if (c === '(' || c === '{') depth++;
    else if (c === ')' || c === '}') depth = Math.max(0, depth - 1);
    if (depth > 0) continue;
    let op = null;
    let width = 1;
    if (c === '\n') {
      if (lang === 'ps' && /\|\s*$/.test(bare.slice(start, i))) continue;
      op = '\n';
    } else if (c === ';') op = ';';
    else if (c === '&' && nx === '&') [op, width] = ['&&', 2];
    else if (c === '|' && nx === '|') [op, width] = ['||', 2];
    else if (c === '|') op = '|';
    else if (c === '&' && lang !== 'ps' && !/[<>]/.test(bare[i - 1] ?? '') && nx !== '>') op = '&';
    if (!op) continue;
    push(i, op);
    start = i + width;
    i += width - 1;
  }
  push(bare.length, '');
  return segs;
}

// The command a segment runs, after keywords, env assignments and wrappers.
function commandText(text, { keepExec = false } = {}) {
  let t = text.trim();
  if (/^command\s+-v\b/.test(t)) return t;
  const prefix = keepExec
    ? /^(?:do|then|else|elif|!|\{|\(|time|nohup|setsid|command|builtin|env|sudo(?:\s+-[A-Za-z]+)*|&|[A-Za-z_]\w*=(?:"[^"]*"|'[^']*'|\S*)|\$[A-Za-z_]\w*\s*=)\s*/
    : /^(?:do|then|else|elif|!|\{|\(|time|nohup|setsid|exec|command|builtin|env|sudo(?:\s+-[A-Za-z]+)*|&|[A-Za-z_]\w*=(?:"[^"]*"|'[^']*'|\S*)|\$[A-Za-z_]\w*\s*=)\s*/;
  for (let guard = 0; guard < 8; guard++) {
    const m = prefix.exec(t);
    if (!m || m[0].length === 0) break;
    t = t.slice(m[0].length);
  }
  return t;
}

function headWord(text) {
  const m = /^(?:"([^"]+)"|'([^']+)'|([^\s;|&()]+))/.exec(commandText(text));
  return m ? m[1] ?? m[2] ?? m[3] : '';
}

function baseName(word) {
  return word.split(/[\\/]/).pop().toLowerCase().replace(/\.(?:exe|cmd|bat|ps1)$/, '');
}

// ---------------------------------------------------------------- analysis

const ESCAPE_LINE = /#\s*lint-ok:\s*([\w-]+)(?:[ \t]+(\S.*?))?\s*$/;

function analyze(block) {
  const code = block.code.replace(/\r\n?/g, '\n');
  const lang = block.lang;
  const scan = lang === 'ps' ? scanPs(code) : scanShell(code);
  const lineStarts = [0];
  for (let i = 0; i < code.length; i++) if (code[i] === '\n') lineStarts.push(i + 1);
  const lineOf = (pos) => {
    let lo = 0;
    let hi = lineStarts.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (lineStarts[mid] <= pos) lo = mid;
      else hi = mid - 1;
    }
    return block.startLine + lo;
  };
  const segs = splitCommands(scan.bare, scan.clean, lang).map((s) => ({
    ...s,
    line: lineOf(s.pos),
    endLine: lineOf(Math.max(s.pos, s.end - 1)),
    head: baseName(headWord(s.text)),
  }));
  const escapes = (block.escapes ?? []).map((e) => ({ ...e, line: null }));
  code.split('\n').forEach((l, idx) => {
    const m = ESCAPE_LINE.exec(l);
    if (m) escapes.push({ rule: m[1], reason: m[2] ?? '', line: l.trim().startsWith('#') ? null : block.startLine + idx });
  });
  const setE =
    lang !== 'ps' &&
    (/^[ \t]*set[ \t]+-[a-zA-Z]*e/m.test(scan.clean) || /^[ \t]*set[ \t]+-o[ \t]+errexit/m.test(scan.clean) || /^#!.*\s-[a-z]*e/.test(code));
  return { block, file: block.file, lang, code, ...scan, segs, lineOf, escapes, setE };
}

function nestedScripts(a) {
  const out = [];
  for (const h of a.heredocs) {
    const first = h.body.split('\n').find((l) => l.trim()) ?? '';
    const target = />\s*(\S+)/.exec(h.opener.replace(/<<.*/, ''));
    const consumer = baseName(headWord(h.opener));
    const script =
      first.startsWith('#!') || (target && /\.(?:sh|bash|ps1)$/.test(target[1])) || ['bash', 'sh', 'zsh', 'pwsh', 'powershell'].includes(consumer);
    if (!script) continue;
    const ps = /pwsh|powershell/.test(first) || (target && target[1].endsWith('.ps1')) || consumer === 'pwsh' || consumer === 'powershell';
    out.push({
      file: a.file,
      index: a.block.index,
      label: ps ? 'powershell' : 'bash',
      startLine: a.lineOf(h.bodyStart),
      code: h.body,
      source: 'heredoc',
      escapes: [],
      lang: ps ? 'ps' : 'sh',
    });
  }
  return out;
}

function finding(a, rule, line, message, endLine = line, fix = RULES[rule].fix) {
  return { file: a.file, line, endLine, rule, severity: RULES[rule].severity, message, fix, a };
}

const FOREGROUND_FIX =
  "Don't run the server inline. Check it first (`curl -s http://localhost:11434/api/version`); if nothing answers, start the Ollama app, or run `ollama serve` in a second terminal.";

// ---------------------------------------------------------------- PowerShell variables

const PS_AUTOMATIC = new Set(
  (
    '_ psitem true false null lastexitcode home pwd args input matches error erroractionpreference progresspreference ' +
    'verbosepreference warningpreference informationpreference debugpreference confirmpreference whatifpreference ' +
    'psversiontable pshome psscriptroot pscommandpath myinvocation host pid profile executioncontext this iswindows ' +
    'islinux ismacos iscoreclr psedition psculture psuiculture pscmdlet psboundparameters psdefaultparametervalues ' +
    'psstyle psnativecommanduseerroractionpreference ofs shellid stacktrace foreach switch sender eventargs event ' +
    'eventsubscriber nestedpromptlevel maximumhistorycount errorview formatenumerationlimit outputencoding ' +
    'pssessionoption pssessionconfigurationname pssessionapplicationname psemailserver enabledexperimentalfeatures ? ^ $'
  ).split(' '),
);

function psVars(src) {
  const reads = [];
  const assigned = new Set();
  const n = src.length;
  const varAt = (p) => {
    if (src[p + 1] === '{') {
      const e = src.indexOf('}', p + 2);
      if (e < 0) return null;
      const name = src.slice(p + 2, e);
      return { name: name.replace(/^\w+:/, ''), qualified: name.includes(':'), end: e + 1 };
    }
    const m = /^(?:([A-Za-z_]\w*):(?=[A-Za-z_]))?([A-Za-z_]\w*|[?^$])/.exec(src.slice(p + 1, p + 160));
    if (!m) return null;
    return { name: m[2], qualified: Boolean(m[1]), end: p + 1 + m[0].length };
  };
  const inDq = (p, stop) => {
    while (p < n && p < stop) {
      const c = src[p];
      if (c === '`') {
        p += 2;
        continue;
      }
      if (c === '"' && stop === Infinity) {
        if (src[p + 1] === '"') {
          p += 2;
          continue;
        }
        return p + 1;
      }
      if (c === '$') {
        if (src[p + 1] === '(') {
          p = inCode(p + 2, true);
          continue;
        }
        const v = varAt(p);
        if (v) {
          if (!v.qualified) reads.push({ name: v.name, pos: p });
          p = v.end;
          continue;
        }
      }
      p++;
    }
    return p;
  };
  const inCode = (p, untilParen) => {
    let depth = 0;
    while (p < n) {
      const c = src[p];
      if (c === '`') {
        p += 2;
        continue;
      }
      if (c === "'") {
        p++;
        while (p < n && !(src[p] === "'" && src[p + 1] !== "'")) p += src[p] === "'" ? 2 : 1;
        p++;
        continue;
      }
      if (c === '@' && (src[p + 1] === '"' || src[p + 1] === "'") && /^[ \t]*\n/.test(src.slice(p + 2, p + 64))) {
        const term = src.indexOf(`\n${src[p + 1]}@`, p + 2);
        const stop = term < 0 ? n : term;
        if (src[p + 1] === '"') inDq(p + 2, stop);
        p = stop + 3;
        continue;
      }
      if (c === '"') {
        p = inDq(p + 1, Infinity);
        continue;
      }
      if (c === '(') depth++;
      if (c === ')') {
        if (untilParen && depth === 0) return p + 1;
        depth--;
      }
      if (c === '@' && /[A-Za-z_]/.test(src[p + 1] ?? '') && (p === 0 || /[\s(,;=]/.test(src[p - 1]))) {
        const m = /^[A-Za-z_]\w*/.exec(src.slice(p + 1));
        reads.push({ name: m[0], pos: p });
        p += 1 + m[0].length;
        continue;
      }
      if (c === '$') {
        const v = varAt(p);
        if (v) {
          const rest = src.slice(v.end, v.end + 400);
          const key = v.name.toLowerCase();
          if (
            /^\s*(?:[+\-*/%]|\?\?)?=(?!=)/.test(rest) ||
            /^\s*(?:\+\+|--)/.test(rest) ||
            /^(?:\s*,\s*\$[A-Za-z_]\w*)+\s*=(?!=)/.test(rest) ||
            /\bforeach\s*\(\s*$/i.test(src.slice(Math.max(0, p - 40), p))
          ) {
            assigned.add(key);
          } else if (!v.qualified) {
            reads.push({ name: v.name, pos: p });
          }
          p = v.end;
          continue;
        }
      }
      p++;
    }
    return p;
  };
  inCode(0, false);
  // Parameters of param() blocks and of `function [scope:]name(...)` / filter.
  for (const m of src.matchAll(/\b(?:param|(?:function|filter)\s+(?:[A-Za-z]+:)?[\w-]+)\s*\(/gi)) {
    let depth = 0;
    let j = m.index + m[0].length - 1;
    const from = j;
    for (; j < n; j++) {
      if (src[j] === '(') depth++;
      if (src[j] === ')' && --depth === 0) break;
    }
    for (const v of src.slice(from, j).matchAll(/\$([A-Za-z_]\w*)/g)) assigned.add(v[1].toLowerCase());
  }
  for (const m of src.matchAll(/-(?:OutVariable|ov|ErrorVariable|ev|WarningVariable|wv|InformationVariable|iv|PipelineVariable|pv)\s+\+?['"]?([A-Za-z_]\w*)/gi)) {
    assigned.add(m[1].toLowerCase());
  }
  for (const m of src.matchAll(/\b(?:Set|New)-Variable\s+(?:-Name\s+)?['"]?([A-Za-z_]\w*)/gi)) assigned.add(m[1].toLowerCase());
  return { reads, assigned };
}

// ---------------------------------------------------------------- per-block rules

function rulePsUnassignedVar(a) {
  if (a.lang !== 'ps') return [];
  const { reads, assigned } = psVars(a.clean);
  const seen = new Set();
  const out = [];
  for (const r of reads) {
    const key = r.name.toLowerCase();
    if (assigned.has(key) || PS_AUTOMATIC.has(key) || seen.has(key)) continue;
    seen.add(key);
    out.push(
      finding(a, 'ps-unassigned-var', a.lineOf(r.pos), `$${r.name} is read here but never assigned in this block; run on its own or in a new window it is empty`),
    );
  }
  return out;
}

function ruleWinget(a) {
  const use = /(?:^|[\s;|&({])winget(?:\.exe)?[ \t]+[A-Za-z-]/m.exec(a.bare);
  if (!use) return [];
  const ps = a.lang === 'ps';
  // In PowerShell `where` is Where-Object, not where.exe, and `||` does not
  // parse in 5.1 (ps51-chain-operators), so neither counts there.
  const guard = ps
    ? /\b(?:Get-Command|gcm)\s+(?:-Name\s+)?['"]?winget\b|\bwhere\.exe\s+winget\b/i.test(a.clean)
    : /Get-Command\s+(?:-Name\s+)?['"]?winget\b|command\s+-v\s+winget\b|\bwhere(?:\.exe)?\s+winget\b/i.test(a.clean);
  const fallback = ps
    ? /\belse\b|(?:-not\s*|!\s*)\(\s*(?:Get-Command|gcm)\b/i.test(a.bare)
    : /\belse\b|-not\s*\(\s*Get-Command|!\s*\(\s*Get-Command|\|\|/i.test(a.bare);
  if (guard && fallback) return [];
  const what = guard ? 'checks for winget but has no fallback when it is missing' : 'uses winget without checking that it exists';
  return [finding(a, 'winget-unguarded', a.lineOf(use.index + use[0].indexOf('winget')), `this block ${what}; Windows Server 2022 and many managed machines have no winget`)];
}

function callArgs(src, open) {
  const args = [];
  let depth = 0;
  let quote = null;
  let from = open + 1;
  for (let j = open; j < src.length; j++) {
    const c = src[j];
    if (quote) {
      if (c === quote) quote = null;
      continue;
    }
    if (c === "'" || c === '"') quote = c;
    else if (c === '(') depth++;
    else if (c === ')' && --depth === 0) {
      args.push(src.slice(from, j).trim());
      return args;
    } else if (c === ',' && depth === 1) {
      args.push(src.slice(from, j).trim());
      from = j + 1;
    }
  }
  return args;
}

function ruleMachineScope(a) {
  const hits = [];
  for (const m of a.clean.matchAll(/SetEnvironmentVariable\s*\(/gi)) {
    const args = callArgs(a.clean, m.index + m[0].length - 1);
    if (args[2] && /Machine/i.test(args[2])) hits.push({ pos: m.index, what: "SetEnvironmentVariable(..., 'Machine')" });
  }
  for (const m of a.clean.matchAll(/\bsetx(?:\.exe)?\b[^\n]*\s\/M\b/gi)) hits.push({ pos: m.index, what: 'setx /M' });
  for (const m of a.clean.matchAll(/\b(?:Set-ItemProperty|New-ItemProperty|Remove-ItemProperty|New-Item|Set-Item|Remove-Item)\b[^\n;|]*?(?:HKLM:|Registry::HKEY_LOCAL_MACHINE)/gi)) {
    hits.push({ pos: m.index, what: 'an HKLM registry write' });
  }
  for (const m of a.clean.matchAll(/\breg(?:\.exe)?\s+(?:add|delete|import|copy)\s+["']?(?:HKLM|HKEY_LOCAL_MACHINE)\\/gi)) {
    hits.push({ pos: m.index, what: 'an HKLM registry write' });
  }
  if (!hits.length) return [];
  if (/IsInRole\s*\(|WindowsBuiltInRole|#Requires\s+-RunAsAdministrator|\bnet\s+session\b|S-1-16-12288|S-1-5-32-544/i.test(a.code)) return [];
  return hits.map((h) =>
    finding(a, 'machine-scope-unelevated', a.lineOf(h.pos), `${h.what} needs an elevated shell and this block never checks for one; unelevated it fails with "Requested registry access is not allowed"`),
  );
}

const PS_KEYWORDS = new Set('if elseif else switch foreach for while do until try catch finally throw return exit break continue function filter param begin process end trap data class enum using'.split(' '));
const PS_ALIASES = new Set(
  'cd ls dir echo cat cp copy mv move rm del mkdir md pwd sleep where select sort foreach iwr irm gci gi gc sc ni ri type clear cls kill ps start write tee measure group fl ft gm gps gsv sal sv gv ogv set'.split(' '),
);

function isExternalTool(seg) {
  const head = headWord(seg.text);
  if (!/^[A-Za-z][\w.-]*$/.test(head) || /^[A-Za-z]+-[A-Za-z]+$/.test(head)) return false;
  const low = head.toLowerCase();
  return !PS_KEYWORDS.has(low) && !PS_ALIASES.has(low) && !/^msiexec(?:\.exe)?$/.test(low);
}

function ruleMsiexec(a) {
  const out = [];
  a.segs.forEach((seg, idx) => {
    const quoted = /Start-Process\s+(?:-FilePath\s+)?['"]msiexec/i.test(seg.text);
    if (!/\bmsiexec(?:\.exe)?\b/i.test(seg.bare) && !quoted) return;
    const problems = [];
    const cmd = commandText(seg.text);
    // Start-Process can also sit in parentheses, read inline:
    // `if ((Start-Process msiexec ... -Wait -PassThru).ExitCode -ne 0) { throw ... }`.
    const nested = /\(\s*(Start-Process\b[^;{}\n]*)/i.exec(seg.text);
    const sp = /^Start-Process\b/i.test(cmd) ? cmd : nested && /msiexec/i.test(nested[1]) ? nested[1] : '';
    if (!sp) {
      problems.push('msiexec is run directly, so the shell does not wait for it and never sees its exit code');
    } else {
      if (!/\s-Wait\b/i.test(sp)) problems.push('Start-Process has no -Wait, so the next step races the installer');
      const v = /^\$([A-Za-z_]\w*)\s*=\s*Start-Process\b/i.exec(seg.text);
      const inline = /\)\.ExitCode\b/i.test(sp);
      if (!/\s-PassThru\b/i.test(sp) || (!v && !inline)) {
        problems.push('there is no -PassThru exit-code check, so a failed install (1603, or 1925 when not elevated) prints nothing');
      } else if (!inline && !new RegExp(`\\$${v[1]}\\.ExitCode\\b`, 'i').test(a.clean.slice(seg.end))) {
        problems.push(`$${v[1]}.ExitCode is never checked, so a failed install prints nothing`);
      }
    }
    for (const later of a.segs.slice(idx + 1)) {
      if (/\$env:Path\s*\+?=/i.test(later.text)) break;
      const prev = a.segs[a.segs.indexOf(later) - 1];
      if (prev && prev.op === '|') continue;
      if (isExternalTool(later)) {
        problems.push(`\`${headWord(later.text)}\` runs at line ${later.line} before $env:Path is refreshed, so this session cannot find what was just installed`);
        break;
      }
    }
    if (problems.length) out.push(finding(a, 'msiexec-bare', seg.line, problems.join('; '), seg.endLine));
  });
  return out;
}

const PIPE_FILTERS = new Set(['where-object', 'where', '?', 'select-object', 'select', 'foreach-object', 'foreach', '%', 'sort-object', 'sort', 'group-object', 'group', 'measure-object', 'measure']);

// Endpoints that return a JSON array: GitHub list endpoints and Node's index.json.
const ARRAY_URL = /\/(?:releases|tags|branches|commits|issues|pulls|contributors|assets|runs|jobs|artifacts|workflows)\/?(?:\?[^\s'"]*)?(?=['"\s)]|$)|\/index\.json\b/i;

function rulePs51Json(a) {
  if (a.lang !== 'ps') return [];
  const out = [];
  const src = a.bare;
  for (const m of src.matchAll(/(?<![\w-])(Invoke-RestMethod|irm|ConvertFrom-Json)(?![\w-])/gi)) {
    let depth = 0;
    for (let j = m.index + m[0].length; j < src.length; j++) {
      const c = src[j];
      if (c === '`') {
        j++;
        continue;
      }
      if (c === '(' || c === '{' || c === '[') depth++;
      else if (c === ')' || c === '}' || c === ']') {
        if (--depth < 0) break;
      } else if (depth === 0 && (c === ';' || (c === '\n' && !/\|\s*$/.test(src.slice(m.index, j))))) break;
      else if (depth === 0 && c === '|' && src[j + 1] !== '|') {
        const next = /^\s*([\w?%-]+)([^|;\n]*)/.exec(src.slice(j + 1));
        if (next && PIPE_FILTERS.has(next[1].toLowerCase())) {
          // Only an array breaks: an array endpoint, or a filter that only
          // makes sense on many items, is an error; `.../releases/latest |
          // Select-Object -ExpandProperty tag_name` works on 5.1.
          const call = a.clean.slice(m.index, j);
          const arrayUrl = ARRAY_URL.test(call);
          const many = /^(?:where-object|where|\?|sort-object|sort|group-object|group|measure-object|measure)$/i.test(next[1]) || /\s-(?:First|Last|Skip|Index|Unique)\b/i.test(next[2]);
          const f = finding(
            a,
            'ps51-json-pipeline',
            a.lineOf(m.index),
            arrayUrl || many
              ? `${m[1]} output goes straight into ${next[1]}; Windows PowerShell 5.1 passes the whole JSON array as one object, so the filter sees one item`
              : `${m[1]} output goes straight into ${next[1]}; fine for a single JSON object, but if the response is an array Windows PowerShell 5.1 passes it as one object`,
          );
          out.push(arrayUrl || many ? f : { ...f, severity: 'warning' });
        }
        break;
      }
    }
  }
  return out;
}

function rulePs51Chain(a) {
  if (a.lang !== 'ps') return [];
  const out = [];
  const lines = new Set();
  for (const m of a.bare.matchAll(/&&|\|\|/g)) {
    const line = a.lineOf(m.index);
    if (lines.has(line)) continue;
    lines.add(line);
    out.push(
      finding(a, 'ps51-chain-operators', line, `\`${m[0]}\` is a parse error in Windows PowerShell 5.1 ("The token '${m[0]}' is not a valid statement separator in this version"), so nothing in the block runs; only PowerShell 7 accepts it`),
    );
  }
  return out;
}

function rulePs51CurlAlias(a) {
  if (a.lang !== 'ps') return [];
  const out = [];
  // Command position only: `Remove-Item alias:curl` or `Get-Command curl` is not a call.
  for (const m of a.bare.matchAll(/(?:^|[;|({=&])[ \t]*(curl|wget)(?=[ \t]|$|[;)}|])/gim)) {
    const at = m.index + m[0].length - m[1].length;
    out.push(
      finding(
        a,
        'ps51-curl-alias',
        a.lineOf(at),
        `\`${m[1]}\` here is Windows PowerShell 5.1's alias for Invoke-WebRequest, so its flags bind to other parameters (\`-s\` becomes -SessionVariable and the shell stops to ask for a Uri)`,
      ),
    );
  }
  return out;
}

function ruleNpmPrefix(a) {
  const out = [];
  for (const m of a.bare.matchAll(/\bnpm\s+(?:config\s+)?set\b[^\n;|&]*?\bprefix\b/g)) {
    out.push(finding(a, 'npm-prefix', a.lineOf(m.index), '`npm config set prefix` writes a prefix to ~/.npmrc; nvm then refuses to load ("incompatible with nvm")'));
  }
  for (const m of a.clean.matchAll(/\bprefix\s*=[^\n]*>>?\s*\S*\.npmrc\b/g)) {
    out.push(finding(a, 'npm-prefix', a.lineOf(m.index), 'writing prefix= into .npmrc breaks nvm ("incompatible with nvm")'));
  }
  for (const m of a.clean.matchAll(/\bexport\s+NPM_CONFIG_PREFIX=|NPM_CONFIG_PREFIX=[^\n]*>>?\s*\S*(?:bashrc|profile|zshrc)\b/g)) {
    out.push(finding(a, 'npm-prefix', a.lineOf(m.index), 'a persistent NPM_CONFIG_PREFIX breaks nvm'));
  }
  return out;
}

function ruleSetEExec(a) {
  if (a.lang !== 'sh' || !a.setE) return [];
  const steps = a.segs.filter((s) => !/^(?:fi|done|esac|then|else|do|\{|\})$/.test(s.text));
  const last = steps[steps.length - 1];
  if (steps.length < 3 || !last) return [];
  if (!/^exec\s+(?![0-9]*[<>])\S/.test(commandText(last.text, { keepExec: true }))) return [];
  return [
    finding(
      a,
      'set-e-exec',
      last.line,
      `multi-step \`set -e\` script ends in \`exec ${headWord(last.text)}\`: a failing step stops it with only the last error on screen, and pasted into a shell the exec replaces that shell`,
      last.endLine,
    ),
  ];
}

// ---------------------------------------------------------------- no-check-at-end

const INSTALL = [
  /\bnpm\s+(?:install|i|add)\b[^\n;|&]*\s(?:-g|--global)\b/,
  /\b(?:apt-get|apt|dnf|yum|zypper)\s+(?:-\S+\s+)*install\b/,
  /\bapk\s+add\b/,
  /\bpacman\s+-S/,
  /\bsnap\s+install\b/,
  /\bbrew\s+(?:install|upgrade)\b/,
  /\bcargo\s+install\b/,
  /\bpip(?:x|3)?\s+install\b/,
  /\b(?:curl|wget)\b[^|\n]*\|\s*(?:sudo\s+(?:-\S+\s+)*)?(?:ba|z|da)?sh\b/,
  /\bwinget(?:\.exe)?\s+(?:install|upgrade)\b/i,
  /\b(?:choco|scoop)\s+(?:install|upgrade)\b/i,
  /\bmsiexec(?:\.exe)?\b/i,
  /\b(?:Invoke-WebRequest|iwr|curl\.exe)\b[^\n;]*\s-OutFile\b/i,
  /\bExpand-Archive\b/i,
  /\bSetEnvironmentVariable\b/i,
  /\bsetx(?:\.exe)?\s/i,
  /\bInstall-(?:Module|Package|Script)\b/i,
  /\bStart-Process\s+(?:-FilePath\s+)?\S*(?:setup|install|redist|\.msi)/i,
];

const CHECK_HEADS = new Set(
  (
    'test-path get-command get-item get-childitem gci gi ls dir du df stat file which where type get-filehash sha256sum shasum ' +
    'resolve-path get-content gc cat head tail grep select-string test [ [[ get-service get-process get-ciminstance ' +
    'get-wmiobject get-itemproperty get-itempropertyvalue printenv id whoami ollama-list'
  ).split(' '),
);
const SILENT_TAIL = /^(?:Out-Null|Rename-Item|Set-Content|Add-Content|Out-File|Remove-Item|Move-Item|Copy-Item|Set-ItemProperty|New-Item)\b/i;
const PROBE_TOOL = /(?:^|[\s(;{])(?:curl|wget|Invoke-RestMethod|Invoke-WebRequest|irm|iwr|Test-NetConnection|nc)\b/i;
const LOCAL_URL = /\b(?:localhost|127\.0\.0\.1|0\.0\.0\.0)\b|\[::1\]/i;

function installedNames(bare) {
  const names = new Set(['bwn', 'buildwithnexus']);
  for (const m of bare.matchAll(/\b(?:npm\s+(?:install|i|add)|cargo\s+install|(?:apt-get|apt|dnf|yum|brew|pipx?|pip3)\s+(?:-\S+\s+)*install)\s+([^\n;|&]*)/g)) {
    for (const w of m[1].split(/\s+/)) if (w && !w.startsWith('-')) names.add(w.replace(/@[^/]*$/, '').split('/').pop().toLowerCase());
  }
  return names;
}

function isCheckSeg(seg, installed) {
  const t = commandText(seg.text);
  if (INSTALL.some((re) => re.test(seg.bare))) return false;
  if (/^command\s+-v\b/.test(t)) return true;
  if (/^\$[A-Za-z_][\w:]*(?:\.\w+)*$/.test(t)) return true; // a bare variable prints its value
  if (/^if\b[\s\S]*\b(?:throw|exit|Write-Error)\b/i.test(t) || /\|\|\s*(?:exit|return)\b/.test(t)) return true;
  if (PROBE_TOOL.test(t) && LOCAL_URL.test(t)) return true;
  if (/^(?:for|foreach|while|do)\b/i.test(t) && PROBE_TOOL.test(t) && LOCAL_URL.test(t)) return true;
  if (/(?:^|\s)(?:--version|-version|-v|-V|version|doctor)(?:\s|$)/.test(t)) return true;
  if (/^(?:echo|printf|Write-Host|Write-Output)\b/i.test(t) && /\$/.test(t)) return true;
  if (/^npm\s+(?:ls|list|view|config\s+get|prefix|root|-v)\b/.test(t)) return true;
  if (/^ollama\s+(?:list|ps|show)\b/.test(t) || /^gh\s+attestation\s+verify\b/.test(t) || /^node\s+-[pe]\b/.test(t)) return true;
  const head = baseName(headWord(seg.text));
  return CHECK_HEADS.has(head) || installed.has(head);
}

function isVisible(pipeline) {
  const tail = pipeline[pipeline.length - 1];
  if (SILENT_TAIL.test(commandText(tail.text))) return false;
  return !/(?:^|\s)(?:1?>\s*\/dev\/null|>\s*\$null)(?:\s|$)/.test(tail.text);
}

// The trailing && / || list of pipelines (or the compound statement) that ends the block.
function lastList(segs) {
  let k = segs.length - 1;
  if (k < 0) return [];
  if (/^(?:fi|done|esac)$/.test(segs[k].text)) {
    let depth = 0;
    for (let j = k; j >= 0; j--) {
      const t = segs[j].text;
      if (/^(?:fi|done|esac)$/.test(t)) depth++;
      else if (/^(?:if|for|while|until|case)\b/.test(t)) depth--;
      if (depth === 0) return segs.slice(j);
    }
    return segs.slice(0);
  }
  let j = k;
  while (j > 0 && ['&&', '||', '|'].includes(segs[j - 1].op)) j--;
  return segs.slice(j);
}

function ruleNoCheckAtEnd(a) {
  if (!INSTALL.some((re) => re.test(a.bare))) return [];
  const list = lastList(a.segs);
  if (!list.length) return [];
  const installed = installedNames(a.bare);
  const pipelines = [];
  for (const s of list) {
    if (!pipelines.length || list[list.indexOf(s) - 1]?.op !== '|') pipelines.push([s]);
    else pipelines[pipelines.length - 1].push(s);
  }
  const ok = pipelines.some((p) => isCheckSeg(p[0], installed) && isVisible(p)) || list.some((s) => /^(?:for|while|until)\b/.test(s.text) && PROBE_TOOL.test(s.text));
  if (ok) return [];
  const last = list[list.length - 1];
  const shown = commandText(last.text).replace(/\s+/g, ' ');
  const what = shown.length > 60 ? `${shown.slice(0, 57)}...` : shown;
  return [finding(a, 'no-check-at-end', last.line, `this install/setup block ends in \`${what}\` rather than a check the reader can compare against`, last.endLine)];
}

// ---------------------------------------------------------------- document-order rules

const DEFENDER = /(?<![\w-])(?:Get-MpThreatDetection|Get-MpThreat|Get-MpThreatCatalog|Add-MpPreference|Set-MpPreference|Remove-MpPreference|Get-MpPreference|Get-MpComputerStatus|Start-MpScan|Remove-MpThreat|Update-MpSignature|MpCmdRun(?:\.exe)?)(?![\w-])/i;
const EDR_NAMES = /CrowdStrike|Falcon|csagent|SentinelOne|Sentinel ?Agent|Sophos|Carbon ?Black|Cylance|Symantec|McAfee|Trellix|Trend ?Micro|Cortex|Cybereason|Elastic ?(?:Agent|Endpoint)|Bitdefender|ESET|Kaspersky|Malwarebytes|Webroot/i;

// A query that names the EDR products: their services, processes, drivers or
// install folders. root/SecurityCenter2 (AntiVirusProduct) does not count, as
// the namespace does not exist on Windows Server; nor does `sc query` in
// PowerShell, where `sc` is Set-Content.
function identifiesProduct(text, lang) {
  if (/SecurityCenter2|AntiVirusProduct/i.test(text) || !EDR_NAMES.test(text)) return false;
  const sc = lang === 'ps' ? /\bsc\.exe\s+query/i : /\bsc(?:\.exe)?\s+query/i;
  return sc.test(text) || /\b(?:Get-Service|gsv|Get-Process|gps|Get-CimInstance|gcim|Get-WmiObject|gwmi|tasklist|fltmc|Test-Path|Get-ChildItem|Get-ItemProperty|reg(?:\.exe)?\s+query)\b/i.test(text);
}

function ruleDefenderFirst(a, doc) {
  const out = [];
  for (const seg of a.segs) {
    // A pipeline may list services first and filter by vendor after the |.
    const pipeline = a.segs.slice(a.segs.indexOf(seg)).reduce((acc, s, i, arr) => (i === 0 || arr[i - 1].op === '|' ? acc + ' ' + s.text : acc), '');
    if (identifiesProduct(pipeline, a.lang)) doc.identified = true;
    const m = DEFENDER.exec(seg.bare);
    if (m && !doc.identified) {
      out.push(
        finding(a, 'defender-first', seg.line, `${m[0]} assumes Microsoft Defender is the security product, but nothing before it checks what is installed (CrowdStrike and others block silently and leave Defender with nothing to report)`),
      );
      break;
    }
  }
  return out;
}

// Loops (sh) as [from, to] segment ranges.
function shLoops(segs) {
  const loops = [];
  const stack = [];
  segs.forEach((s, i) => {
    if (/^(?:for|while|until)\b/.test(s.text)) stack.push({ from: i, kind: s.text.split(/\s/)[0] });
    else if (/^done\b/.test(s.text) && stack.length) loops.push({ ...stack.pop(), to: i });
  });
  return loops.filter((l) => !loops.some((o) => o !== l && o.from < l.from && o.to > l.to));
}

function isProbe(seg) {
  const t = commandText(seg.text);
  return (PROBE_TOOL.test(t) && LOCAL_URL.test(t)) || /^ollama\s+(?:list|ps)\b/.test(t);
}

function probeGates(segs, idx) {
  const seg = segs[idx];
  if (/^(?:if|elif|while|until)\b/.test(seg.text)) return false;
  if (seg.op === '&&') return false;
  if (seg.op === '||') return /\b(?:exit|return|false)\b/.test(segs[idx + 1]?.text ?? '');
  return true;
}

function startProcessTarget(text) {
  const m = /^Start-Process\s+(?:-FilePath\s+)?(?:"([^"]+)"|'([^']+)'|(\S+))/i.exec(commandText(text));
  return m ? baseName(m[1] ?? m[2] ?? m[3]) : '';
}

function serviceEvents(a) {
  const events = [];
  const loops = a.lang === 'ps' ? [] : shLoops(a.segs);
  for (let i = 0; i < a.segs.length; i++) {
    const seg = a.segs[i];
    const loop = loops.find((l) => l.from === i);
    if (loop) {
      const body = a.segs.slice(loop.from, loop.to + 1);
      if (body.some(isProbe)) {
        const cond = a.segs[loop.from].text;
        const gating = (loop.kind === 'until' && isProbe({ text: cond.replace(/^until\s+/, '') })) || (loop.kind === 'while' && /^while\s+!/.test(cond) && isProbe({ text: cond.replace(/^while\s+!\s*/, '') }));
        events.push({ type: 'wait', gating, seg, line: seg.line });
      }
      i = loop.to;
      continue;
    }
    const cmd = commandText(seg.text);
    const lone = a.lang !== 'ps' ? /(?<![&>|<0-9])&(?![&>])/.exec(seg.bare) : null;
    // Start-Process can sit inside try/catch or if bodies, not only at the head.
    const psStarts = a.lang === 'sh' ? [] : [...seg.text.matchAll(/\bStart-Process\b[^;{}\n]*/gi)].filter((m) => !/\s-Wait\b/i.test(m[0]));
    if (psStarts.length) {
      for (const m of psStarts) events.push({ type: 'start', program: startProcessTarget(m[0]), seg, line: a.lineOf(seg.pos + m.index) });
    } else if (a.lang !== 'ps' && seg.op === '&') {
      events.push({ type: 'start', program: seg.head, seg, line: seg.line });
    } else if (lone) {
      const inner = seg.text.slice(0, lone.index).split(/[{(;]|&&|\|\|/).pop();
      events.push({ type: 'start', program: baseName(headWord(inner)), seg, line: seg.line });
    } else if (/\bsystemctl\s+(?:--\S+\s+)*(?:re)?start\s+([\w@.-]+)/.test(cmd)) {
      const unit = /\bsystemctl\s+(?:--\S+\s+)*(?:re)?start\s+([\w@.-]+)/.exec(cmd)[1].replace(/\.service$/, '');
      events.push({ type: 'start', program: unit, seg, line: seg.line });
    } else if (/^ollama\s+serve\b/.test(cmd)) {
      events.push({ type: 'foreground', program: 'ollama', seg, line: seg.line, more: a.segs.slice(i + 1).length > 0 });
    } else if (/--retry\s+\d+/.test(cmd) && /--retry-(?:connrefused|all-errors)/.test(cmd) && LOCAL_URL.test(cmd)) {
      events.push({ type: 'wait', gating: probeGates(a.segs, i), seg, line: seg.line });
    } else if (/^(?:for|foreach|while|do)\b/i.test(cmd) && PROBE_TOOL.test(cmd) && LOCAL_URL.test(cmd) && /\bStart-Sleep\b|\bsleep\b/i.test(cmd)) {
      events.push({ type: 'wait', gating: true, seg, line: seg.line });
    } else if (isProbe(seg)) {
      events.push({ type: 'probe', gating: probeGates(a.segs, i), seg, line: seg.line, ollamaOnly: !LOCAL_URL.test(cmd) });
    } else if (seg.head) {
      events.push({ type: 'use', seg, line: seg.line });
    }
  }
  return events;
}

function uses(ev, program) {
  const t = ev.seg.text;
  // Stopping or restarting it does not need it to be ready.
  if (ev.seg.head === program && !/\b(?:serve|stop|kill)\b/.test(t)) return true;
  if (LOCAL_URL.test(t)) return true;
  return program === 'ollama' && /--provider[ =]ollama\b|(?:^|\s)-p\s+ollama\b/.test(t);
}

function ruleBackground(a, doc) {
  const out = [];
  const local = new Map();
  for (const ev of serviceEvents(a)) {
    if (ev.type === 'start') {
      if (ev.program) local.set(ev.program, { line: ev.line, state: 'started' });
      continue;
    }
    if (ev.type === 'foreground') {
      if (ev.more) {
        out.push(
          finding(
            a,
            'background-no-ready',
            ev.line,
            '`ollama serve` runs in the foreground: the lines after it wait until the server exits, and where the Ollama app or service already runs it fails with "address already in use"',
            ev.line,
            FOREGROUND_FIX,
          ),
        );
      }
      continue;
    }
    if (ev.type === 'wait' || ev.type === 'probe') {
      for (const [program, s] of local) {
        if (ev.type === 'probe' && ev.ollamaOnly && program !== 'ollama') continue;
        if (ev.type === 'wait') s.state = ev.gating || !a.setE ? 'ready' : 'waited';
        else if (s.state === 'waited' && (ev.gating || !a.setE)) s.state = 'ready';
        else if (s.state === 'started') {
          out.push(finding(a, 'background-no-ready', s.line, `started in the background, then probed at line ${ev.line} straight away with no wait; the first check races the server start`));
          local.delete(program);
        }
      }
      for (const program of [...doc.pending.keys()]) if (!ev.ollamaOnly || program === 'ollama') doc.pending.delete(program);
      continue;
    }
    for (const [program, s] of local) {
      if (!uses(ev, program) || s.state === 'ready') continue;
      if (s.state === 'started') {
        out.push(finding(a, 'background-no-ready', s.line, `\`${program}\` is started in the background, and line ${ev.line} uses it without waiting until it answers`));
      } else if (s.state === 'waited') {
        out.push(
          finding(
            a,
            'background-no-ready',
            s.line,
            `\`${program}\` is started in the background; the wait loop gives up silently and nothing under \`set -e\` fails on it, so line ${ev.line} runs whether or not the server came up`,
          ),
        );
      }
      local.delete(program);
    }
    for (const [program, p] of doc.pending) {
      if (!uses(ev, program)) continue;
      out.push(finding(p.a, 'background-no-ready', p.line, `\`${program}\` is started in the background here, and line ${ev.line} (a later block) uses it with no readiness check in between`));
      doc.pending.delete(program);
    }
  }
  for (const [program, s] of local) if (s.state === 'started') doc.pending.set(program, { line: s.line, a });
  return out;
}

// ---------------------------------------------------------------- driver

const PER_BLOCK = [rulePsUnassignedVar, ruleWinget, ruleMachineScope, ruleMsiexec, rulePs51Json, rulePs51Chain, rulePs51CurlAlias, ruleNpmPrefix, ruleSetEExec, ruleNoCheckAtEnd];
const IN_ORDER = [ruleBackground, ruleDefenderFirst];

function applyEscapes(f) {
  const hit = f.a.escapes.filter((e) => e.rule === f.rule && (e.line === null || (e.line >= f.line && e.line <= f.endLine)));
  if (!hit.length) return f;
  if (hit.some((e) => e.reason.trim())) return null;
  return { ...f, message: `${f.message} (the lint-ok escape needs a reason)` };
}

export function lintBlocks(blocks) {
  const doc = { pending: new Map(), identified: false };
  const findings = [];
  const queue = blocks.filter((b) => b.lang);
  while (queue.length) {
    const a = analyze(queue.shift());
    for (const rule of IN_ORDER) findings.push(...rule(a, doc));
    for (const rule of PER_BLOCK) findings.push(...rule(a));
    queue.unshift(...nestedScripts(a));
  }
  return findings
    .map(applyEscapes)
    .filter(Boolean)
    .map(({ a, endLine, ...f }) => ({ ...f, lang: a.lang, blockLine: a.block.startLine }))
    .sort((x, y) => x.file.localeCompare(y.file) || x.line - y.line || x.rule.localeCompare(y.rule));
}

export function lintText(text, { file = '<input>', format = 'markdown' } = {}) {
  return lintBlocks(extractBlocks(text, { file, format }));
}

export function lintFile(file, { chat = false } = {}) {
  return lintText(readFileSync(file, 'utf8'), { file, format: formatFor(file, { chat }) });
}

function main() {
  let parsed;
  try {
    parsed = parseArgs({
      args: argv.slice(2),
      options: {
        chat: { type: 'boolean', default: false },
        json: { type: 'boolean', default: false },
        'list-rules': { type: 'boolean', default: false },
        help: { type: 'boolean', short: 'h', default: false },
      },
      allowPositionals: true,
    });
  } catch (err) {
    stderr.write(`${err.message}\n`);
    exit(2);
  }
  const { values, positionals } = parsed;
  if (values['list-rules']) {
    for (const [id, r] of Object.entries(RULES)) stdout.write(`${id} (${r.severity}): ${r.summary}\n  fix: ${r.fix}\n`);
    return;
  }
  if (values.help || positionals.length === 0) {
    stderr.write('usage: lint-blocks.mjs [--chat] [--json] <file>...   |   lint-blocks.mjs --list-rules\n');
    exit(values.help ? 0 : 2);
  }
  const findings = [];
  for (const file of positionals) {
    try {
      findings.push(...lintFile(file, { chat: values.chat }));
    } catch (err) {
      stderr.write(`${file}: ${err.message}\n`);
      exit(2);
    }
  }
  const errors = findings.filter((f) => f.severity === 'error').length;
  if (values.json) {
    stdout.write(`${JSON.stringify(findings, null, 2)}\n`);
  } else {
    for (const f of findings) stdout.write(`${f.file}:${f.line} ${f.rule} ${f.severity}: ${f.message}\n  fix: ${f.fix}\n`);
    stderr.write(`${errors} error(s), ${findings.length - errors} warning(s) in ${positionals.length} file(s)\n`);
  }
  exit(errors ? 1 : 0);
}

if (import.meta.url === pathToFileURL(argv[1] ?? '').href) main();
