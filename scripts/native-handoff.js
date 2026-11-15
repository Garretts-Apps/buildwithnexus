'use strict';
// After the npm launcher has a checksum-verified binary, copy it onto PATH
// and point the npm `bwn` shim at that file. The next `bwn` is the native
// executable: Node is not started. This run is still the Node process that
// noticed the shim.
const fs = require('fs');
const os = require('os');
const path = require('path');

function homeOf(env) {
  if (env.NEXUS_HOME && path.isAbsolute(env.NEXUS_HOME)) return env.NEXUS_HOME;
  return path.join(env.HOME || env.USERPROFILE || os.homedir(), '.buildwithnexus');
}

function nativeDir(env = process.env) {
  if (env.BWN_NATIVE_BIN) return env.BWN_NATIVE_BIN;
  const home = env.HOME || env.USERPROFILE || os.homedir();
  if (process.platform === 'win32') {
    const base = env.LOCALAPPDATA || path.join(home, 'AppData', 'Local');
    return path.join(base, 'Programs', 'bwn');
  }
  return path.join(home, '.local', 'bin');
}

function copyNative(bin, dir) {
  fs.mkdirSync(dir, { recursive: true });
  const ext = process.platform === 'win32' ? '.exe' : '';
  const dest = path.join(dir, 'bwn' + ext);
  const alias = path.join(dir, 'buildwithnexus' + ext);
  fs.copyFileSync(bin, dest);
  try { fs.chmodSync(dest, 0o755); } catch {}
  if (process.platform === 'win32') {
    fs.copyFileSync(dest, alias);
  } else {
    try { fs.rmSync(alias, { force: true }); } catch {}
    fs.symlinkSync('bwn', alias);
  }
  return dest;
}

function looksLikeOurShim(textOrTarget) {
  return String(textOrTarget).includes('buildwithnexus');
}

function rewriteUnixShim(shim, dest) {
  let owned = false;
  try {
    owned = looksLikeOurShim(fs.readlinkSync(shim));
  } catch (err) {
    if (!err || err.code !== 'EINVAL') return false;
    try { owned = looksLikeOurShim(fs.readFileSync(shim, 'utf8')); } catch { return false; }
  }
  if (!owned) return false;
  try { fs.rmSync(shim); } catch { return false; }
  fs.symlinkSync(dest, shim);
  return true;
}

function rewriteCmd(shim, dest) {
  let text = '';
  try { text = fs.readFileSync(shim, 'utf8'); } catch { return false; }
  if (!looksLikeOurShim(text)) return false;
  fs.writeFileSync(shim, `@echo off\r\n"${dest}" %*\r\n`);
  return true;
}

function candidateShims(env) {
  const out = [];
  const prefix = env.npm_config_prefix;
  if (prefix) {
    if (process.platform === 'win32') {
      out.push(path.join(prefix, 'bwn.cmd'), path.join(prefix, 'buildwithnexus.cmd'));
    } else {
      out.push(path.join(prefix, 'bin', 'bwn'), path.join(prefix, 'bin', 'buildwithnexus'));
    }
  }
  const dirs = (env.PATH || '').split(path.delimiter).filter(Boolean);
  const names = process.platform === 'win32'
    ? ['bwn.cmd', 'buildwithnexus.cmd']
    : ['bwn', 'buildwithnexus'];
  for (const dir of dirs) {
    for (const name of names) out.push(path.join(dir, name));
  }
  return out;
}

// Returns the native path, and whether an npm shim now points at it.
function handoff(bin, env = process.env) {
  const dir = nativeDir(env);
  const dest = copyNative(bin, dir);
  let rewrote = false;
  const seen = new Set();
  for (const shim of candidateShims(env)) {
    const key = path.resolve(shim);
    if (seen.has(key) || path.resolve(dest) === key) continue;
    seen.add(key);
    try {
      const ok = process.platform === 'win32' ? rewriteCmd(shim, dest) : rewriteUnixShim(shim, dest);
      if (ok) rewrote = true;
    } catch {
      // A shim we cannot replace stays the Node launcher until the next run.
    }
  }
  const tip = path.join(homeOf(env), 'native-handoff');
  let first = false;
  try {
    fs.mkdirSync(path.dirname(tip), { recursive: true });
    fs.writeFileSync(tip, '1\n', { flag: 'wx' });
    first = true;
  } catch {}
  return { dest, rewrote, first };
}

function tip(result) {
  if (!result.first) return '';
  const install = process.platform === 'win32'
    ? "irm https://raw.githubusercontent.com/Garretts-Apps/buildwithnexus/main/scripts/install.ps1 | iex"
    : 'curl -fsSL https://raw.githubusercontent.com/Garretts-Apps/buildwithnexus/main/scripts/install.sh | sh';
  return 'buildwithnexus: the next `bwn` starts the native binary, without Node.\n' +
    `  Native install: ${install}\n`;
}

module.exports = { nativeDir, handoff, tip, copyNative, rewriteUnixShim, rewriteCmd };
