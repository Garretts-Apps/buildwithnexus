'use strict';
// Background self-updater. Node builtins only — no deps.
//
// The launcher spawns this detached (stdio ignored) at most once per day, so
// startup cost is one fork; nothing here ever blocks the TUI. Flow:
//   1. ask the npm registry for the latest published version
//   2. if it's newer than what's installed, run `npm install -g` silently
//   3. record the outcome in ~/.buildwithnexus/update-state.json — the next
//      launch prints a one-line notice ("updated to vX.Y.Z")
//
// Opt out with BWN_NO_AUTO_UPDATE=1 (checked by the launcher, and honored
// here too in case this script is invoked directly).

const fs = require('fs');
const os = require('os');
const path = require('path');
const https = require('https');
const { execFile } = require('child_process');

const PKG = 'buildwithnexus';
const HOME = path.join(os.homedir(), '.buildwithnexus');
const STATE = path.join(HOME, 'update-state.json');

function readState() {
  try { return JSON.parse(fs.readFileSync(STATE, 'utf8')); } catch { return {}; }
}

function writeState(patch) {
  try {
    fs.mkdirSync(HOME, { recursive: true });
    fs.writeFileSync(STATE, JSON.stringify({ ...readState(), ...patch }, null, 2));
  } catch { /* never fail the updater over a state file */ }
}

// a > b in semver terms (numeric segments only; pre-releases never win).
function newer(a, b) {
  const pa = String(a).split('-')[0].split('.').map(Number);
  const pb = String(b).split('-')[0].split('.').map(Number);
  if (String(a).includes('-')) return false;
  for (let i = 0; i < 3; i++) {
    if ((pa[i] || 0) > (pb[i] || 0)) return true;
    if ((pa[i] || 0) < (pb[i] || 0)) return false;
  }
  return false;
}

function latestVersion(cb) {
  const req = https.get(
    `https://registry.npmjs.org/${PKG}/latest`,
    { headers: { accept: 'application/json' }, timeout: 10_000 },
    (res) => {
      let body = '';
      res.on('data', (c) => { body += c; });
      res.on('end', () => {
        try { cb(null, JSON.parse(body).version); } catch (e) { cb(e); }
      });
    }
  );
  req.on('timeout', () => req.destroy(new Error('timeout')));
  req.on('error', cb);
}

function main() {
  if (process.env.BWN_NO_AUTO_UPDATE === '1') return;
  const current = require('../package.json').version;
  writeState({ lastCheck: Date.now() });
  latestVersion((err, latest) => {
    if (err || !latest) return;
    writeState({ latestSeen: latest });
    if (!newer(latest, current)) return;
    // Silent global update; postinstall fetches the matching binary.
    const npm = process.platform === 'win32' ? 'npm.cmd' : 'npm';
    execFile(
      npm,
      ['install', '-g', `${PKG}@${latest}`, '--no-fund', '--no-audit'],
      { timeout: 300_000, windowsHide: true },
      (installErr) => {
        if (installErr) {
          writeState({ updateFailed: latest });
        } else {
          writeState({ updatedTo: latest, updateFailed: null });
        }
      }
    );
  });
}

if (require.main === module) main();

module.exports = { newer, readState, writeState, STATE };
