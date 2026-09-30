#!/usr/bin/env node
// Pulls runnable command blocks out of Markdown fences, HTML <pre><code>
// blocks (buildwithnexus.dev is plain HTML) and chat text, and decides per
// block whether it is bash or PowerShell. Node builtins only.
//
//   node scripts/field/extract-blocks.mjs [--chat] <file>...   # JSON to stdout

import { readFileSync } from 'node:fs';
import { extname } from 'node:path';
import { argv, exit, stdout, stderr } from 'node:process';
import { pathToFileURL } from 'node:url';
import { parseArgs } from 'node:util';

const PS_LABELS = new Set(['powershell', 'ps1', 'ps', 'pwsh', 'posh']);
const SH_LABELS = new Set(['bash', 'sh', 'zsh', 'ksh', 'dash']);
const CMD_LABELS = new Set(['cmd', 'bat', 'batch']);
// Labels that say "a terminal" without saying which shell.
const SHELL_LABELS = new Set(['shell', 'console', 'shell-session', 'shellsession', 'terminal']);

// First words that mark an unlabeled block as something to paste into a shell.
const KNOWN_COMMANDS = new Set([
  'npm', 'npx', 'node', 'bwn', 'buildwithnexus', 'cargo', 'rustup', 'git', 'gh', 'curl', 'wget',
  'sudo', 'apt', 'apt-get', 'dnf', 'yum', 'apk', 'brew', 'pacman', 'zypper', 'snap', 'ollama',
  'docker', 'podman', 'cd', 'mkdir', 'export', 'echo', 'source', 'chmod', 'chown', 'ls', 'cat',
  'tar', 'unzip', 'pip', 'pip3', 'pipx', 'python', 'python3', 'winget', 'choco', 'scoop', 'msiexec',
  'setx', 'wsl', 'jq', 'make', 'sh', 'bash', 'pwsh', 'powershell', 'icacls', 'reg', 'where.exe',
  'which', 'command', 'sha256sum', 'shasum', 'systemctl', 'service', 'pkill', 'kill', 'nohup',
  'set', 'unset', 'rm', 'cp', 'mv', 'ln', 'touch', 'du', 'df', 'nvm', 'fnm', 'volta', 'corepack',
]);

const PS_HINTS = [
  /\b(?:Get|Set|New|Remove|Invoke|Start|Stop|Test|Add|Expand|Select|Where|ForEach|Sort|Out|Write|Import|Export|ConvertFrom|ConvertTo|Copy|Move|Rename|Join|Split|Resolve|Wait|Enable|Disable|Install|Uninstall|Register|Update|Measure|Format|Clear)-[A-Z][A-Za-z]+\b/,
  /\$env:[A-Za-z_]/i,
  /\[[A-Za-z][\w.]*\]::/,
  /^\s*\$[A-Za-z_]\w*\s*=/m,
  /\s-(?:ErrorAction|OutFile|ArgumentList|UseBasicParsing|ExecutionPolicy|PassThru)\b/i,
];
const SH_HINTS = [
  /^\s*(?:sudo|apt-get|apt|export|source|chmod|chown|brew|nohup)\s/m,
  /^\s*[A-Za-z_]\w*=\S/m,
  /\|\|\s*true\b/,
  /\d?>\s*\/dev\/null/,
  /(?:^|\s)~\//m,
  /^#!.*\b(?:ba|z|k|da)?sh\b/m,
  /(?:^|\s)set\s+-[a-z]*e/m,
  /(?:^|[;\s])(?:then|fi|done|esac)\b/m,
  /\s&&\s/,
];

export function formatFor(file, { chat = false } = {}) {
  if (chat) return 'chat';
  const ext = extname(file).toLowerCase();
  if (ext === '.html' || ext === '.htm') return 'html';
  if (ext === '.sh' || ext === '.bash') return 'sh';
  if (ext === '.ps1') return 'ps1';
  if (ext === '.txt') return 'chat';
  return 'markdown';
}

function stripIndent(line, n) {
  let i = 0;
  while (i < n && (line[i] === ' ' || line[i] === '\t')) i++;
  return line.slice(i);
}

const ESCAPE_COMMENT = /<!--\s*lint-ok:\s*([\w-]+)\s*(.*?)\s*-->/g;

function escapesIn(text) {
  return [...text.matchAll(ESCAPE_COMMENT)].map((m) => ({ rule: m[1], reason: m[2] }));
}

function fenceBlocks(text) {
  const lines = text.split(/\r?\n/);
  const blocks = [];
  for (let i = 0; i < lines.length; i++) {
    const open = /^([ \t]*)(`{3,}|~{3,})[ \t]*([^\s`]*)(.*)$/.exec(lines[i]);
    if (!open) continue;
    const [, indent, fence, info, rest] = open;
    if (fence[0] === '`' && rest.includes('`')) continue;
    const close = new RegExp(`^[ \\t]*\\${fence[0]}{${fence.length},}[ \\t]*$`);
    let j = i + 1;
    while (j < lines.length && !close.test(lines[j])) j++;
    // An HTML comment on the line(s) just above the fence can escape a rule.
    let k = i - 1;
    let above = '';
    while (k >= 0 && /^\s*<!--[\s\S]*?-->\s*$/.test(lines[k])) above = `${lines[k--]}\n${above}`;
    blocks.push({
      label: info.toLowerCase().replace(/^\{?\.?/, '').replace(/\}$/, ''),
      info: `${info}${rest}`.trim(),
      startLine: i + 2,
      code: lines.slice(i + 1, j).map((l) => stripIndent(l, indent.length)).join('\n'),
      source: 'fence',
      escapes: escapesIn(above),
    });
    i = j;
  }
  return blocks;
}

const ENTITIES = { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'", nbsp: ' ' };

// Markup inside a <pre><code> block (highlighting spans), removed until none
// is left so a tag split around another (`<sp<b>an>`) cannot survive.
function stripTags(html) {
  let out = html;
  for (let prev = ''; out !== prev; ) {
    prev = out;
    out = out.replace(/<[^>]*>/g, '');
  }
  return out;
}

export function decodeEntities(s) {
  return s.replace(/&(#x[0-9a-f]+|#\d+|[a-z]+);/gi, (m, e) => {
    if (e[0] === '#') {
      const cp = e[1] === 'x' || e[1] === 'X' ? parseInt(e.slice(2), 16) : parseInt(e.slice(1), 10);
      // fromCodePoint throws past U+10FFFF; leave a bad reference as written.
      return Number.isFinite(cp) && cp <= 0x10ffff ? String.fromCodePoint(cp) : m;
    }
    return ENTITIES[e.toLowerCase()] ?? m;
  });
}

function lineAt(text, pos) {
  let n = 1;
  for (let i = 0; i < pos; i++) if (text.charCodeAt(i) === 10) n++;
  return n;
}

function htmlBlocks(text) {
  const blocks = [];
  const re = /<pre\b([^>]*)>\s*<code\b([^>]*)>([\s\S]*?)<\/code>\s*<\/pre>/gi;
  for (const m of text.matchAll(re)) {
    const codeTagAt = m.index + m[0].search(/<code\b/i);
    const contentAt = text.indexOf('>', codeTagAt) + 1;
    let raw = m[3];
    let startLine = lineAt(text, contentAt);
    const lead = /^\r?\n/.exec(raw);
    if (lead) {
      raw = raw.slice(lead[0].length);
      startLine++;
    }
    const cls = /\bclass\s*=\s*["']([^"']*)["']/i.exec(`${m[2]} ${m[1]}`);
    const lang = cls && /\b(?:language|lang)-([\w-]+)/.exec(cls[1]);
    const dataLang = /\bdata-lang\s*=\s*["']([\w-]+)["']/i.exec(`${m[2]} ${m[1]}`);
    const before = text.slice(0, m.index).trimEnd();
    const comment = before.endsWith('-->') ? before.slice(before.lastIndexOf('<!--')) : '';
    blocks.push({
      label: ((lang && lang[1]) || (dataLang && dataLang[1]) || '').toLowerCase(),
      info: '',
      startLine,
      code: decodeEntities(stripTags(raw)),
      source: 'html',
      escapes: escapesIn(comment),
    });
  }
  return blocks;
}

// A console transcript: keep the prompt lines, blank the output lines so
// line numbers still match the file.
function stripPrompts(code) {
  const lines = code.split('\n');
  const prompt = /^(\s*)(?:\$|PS [^>\n]*>)\s/;
  if (!lines.some((l) => prompt.test(l))) return code;
  let continued = false;
  return lines
    .map((l) => {
      if (prompt.test(l)) {
        const s = l.replace(prompt, '$1');
        continued = /[\\`|]\s*$/.test(s);
        return s;
      }
      if (continued) {
        continued = /[\\`|]\s*$/.test(l);
        return l;
      }
      return '';
    })
    .join('\n');
}

function firstCommandLine(code) {
  for (const line of code.split('\n')) {
    const t = line.trim();
    if (t && !t.startsWith('#')) return t;
  }
  return '';
}

function firstLineIsCommand(first) {
  if (/^[A-Z][a-z]+-[A-Z]\w+/.test(first)) return true; // PowerShell cmdlet
  if (/^\$[A-Za-z_]\w*\s*=/.test(first)) return true; // PowerShell assignment
  if (/^\[[A-Za-z][\w.]*\]::/.test(first)) return true; // [Environment]::...
  if (/^(?:\.{1,2}[\\/]|&\s)/.test(first)) return true; // ./x, .\x.exe, & "x"
  const word = first.replace(/^(?:[A-Za-z_]\w*=\S*\s+)+/, '').split(/\s+/)[0];
  return KNOWN_COMMANDS.has(word.toLowerCase());
}

// Diagram or sentence text: arrows, box-drawing and bullets outside quotes
// and comments, or three or more plain words led by a capitalised one that
// is not a command ("NEXUS Backend (Python FastAPI, port 4200)").
function looksLikeProse(line) {
  const t = line.replace(/"[^"]*"|'[^']*'/g, '""').replace(/(?:^|\s)#.*$/, '').trim();
  if (/[\u2190-\u21ff\u2500-\u25ff\u2022\u2023\u2043]/.test(t)) return true;
  const words = t.split(/\s+/);
  return (
    words.length >= 3 &&
    /^[A-Z][A-Za-z]*$/.test(words[0]) &&
    !KNOWN_COMMANDS.has(words[0].toLowerCase()) &&
    !/[$=|<>;&\\`~]|(?:^|\s)--?[A-Za-z]/.test(t)
  );
}

export function looksLikeCommands(code) {
  const first = firstCommandLine(code);
  if (!first || !firstLineIsCommand(first)) return false;
  // An unlabeled block is often a diagram or output whose first word happens
  // to be a command name; one prose line outside a heredoc or continuation
  // is enough to leave it alone.
  let heredoc = null;
  let continued = false;
  for (const line of code.split('\n')) {
    const t = line.trim();
    if (heredoc) {
      if (t === heredoc) heredoc = null;
      continue;
    }
    if (!continued && t && !t.startsWith('#') && looksLikeProse(t)) return false;
    const h = /<<-?\s*['"]?([A-Za-z_]\w*)/.exec(t);
    if (h) heredoc = h[1];
    continued = /[\\`|]$/.test(t);
  }
  return true;
}

function hintScore(hints, code) {
  return hints.reduce((n, re) => n + (re.test(code) ? 1 : 0), 0);
}

// Returns 'ps', 'sh', 'cmd', 'any' (a shell block with nothing shell-specific
// in it) or null (not something to run).
export function classify(label, code) {
  if (PS_LABELS.has(label)) return 'ps';
  if (SH_LABELS.has(label)) return 'sh';
  if (CMD_LABELS.has(label)) return 'cmd';
  if (label !== '' && !SHELL_LABELS.has(label)) return null;
  if (label === '' && !looksLikeCommands(code)) return null;
  const ps = hintScore(PS_HINTS, code);
  const sh = hintScore(SH_HINTS, code);
  if (ps > sh) return 'ps';
  if (sh > ps) return 'sh';
  return 'any';
}

export function extractBlocks(text, { file = '<input>', format = 'markdown' } = {}) {
  let raw;
  if (format === 'sh' || format === 'ps1') {
    raw = [{ label: format === 'sh' ? 'bash' : 'powershell', info: '', startLine: 1, code: text.replace(/\r\n/g, '\n'), source: 'file', escapes: [] }];
  } else if (format === 'html') {
    raw = htmlBlocks(text);
  } else {
    raw = fenceBlocks(text);
  }
  return raw.map((b, index) => {
    const code = b.label === 'console' || b.label === '' || SHELL_LABELS.has(b.label) ? stripPrompts(b.code) : b.code;
    return { file, index, ...b, code, lang: classify(b.label, code) };
  });
}

async function main() {
  const { values, positionals } = parseArgs({
    args: argv.slice(2),
    options: { chat: { type: 'boolean', default: false }, help: { type: 'boolean', short: 'h' } },
    allowPositionals: true,
  });
  if (values.help || positionals.length === 0) {
    stderr.write('usage: extract-blocks.mjs [--chat] <file>...\n');
    exit(positionals.length === 0 && !values.help ? 2 : 0);
  }
  const all = [];
  for (const file of positionals) {
    const text = readFileSync(file, 'utf8');
    all.push(...extractBlocks(text, { file, format: formatFor(file, { chat: values.chat }) }));
  }
  stdout.write(`${JSON.stringify(all, null, 2)}\n`);
}

if (import.meta.url === pathToFileURL(argv[1] ?? '').href) await main();
