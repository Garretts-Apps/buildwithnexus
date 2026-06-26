'use strict';
// Get a working native binary on `npm install`, then walk the user into setup.
// Order: already present → download prebuilt release → build from source.
// Never hard-fails the install (a thrown postinstall aborts `npm i`).
const fs = require('fs');
const path = require('path');
const https = require('https');
const { spawnSync } = require('child_process');
const { ROOT, ext, target, installedBinary, existing } = require('./resolve-binary.js');

const pkg = require(path.join(ROOT, 'package.json'));

function log(m) {
  process.stdout.write(m + '\n');
}

function download(url, dest, redirects = 0) {
  return new Promise((resolve, reject) => {
    if (redirects > 5) return reject(new Error('too many redirects'));
    https.get(url, { headers: { 'user-agent': 'buildwithnexus-installer' } }, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        res.resume();
        return resolve(download(res.headers.location, dest, redirects + 1));
      }
      if (res.statusCode !== 200) {
        res.resume();
        return reject(new Error(`HTTP ${res.statusCode}`));
      }
      fs.mkdirSync(path.dirname(dest), { recursive: true });
      const file = fs.createWriteStream(dest);
      res.pipe(file);
      file.on('finish', () => file.close(() => resolve()));
      file.on('error', reject);
    }).on('error', reject);
  });
}

function hasCargo() {
  const r = spawnSync('cargo', ['--version'], { stdio: 'ignore' });
  return !r.error && r.status === 0;
}

function buildFromSource() {
  log('buildwithnexus: building native binary from source (cargo)…');
  const manifest = path.join(ROOT, 'harness', 'Cargo.toml');
  const offline = fs.existsSync(path.join(ROOT, 'harness', 'vendor'));
  const args = ['build', '--release', '--manifest-path', manifest];
  if (offline) args.push('--offline');
  const r = spawnSync('cargo', args, { stdio: 'inherit' });
  if (r.status !== 0) return false;
  fs.mkdirSync(path.join(ROOT, 'bin'), { recursive: true });
  fs.copyFileSync(path.join(ROOT, 'harness', 'target', 'release', 'buildwithnexus' + ext()), installedBinary());
  try { fs.chmodSync(installedBinary(), 0o755); } catch {}
  return true;
}

async function obtain() {
  if (process.env.BWN_SKIP_INSTALL) return false;
  if (existing()) return true;

  const t = target();
  if (t) {
    const asset = `buildwithnexus-${t}${ext()}`;
    const url = `https://github.com/Garretts-Apps/buildwithnexus/releases/download/v${pkg.version}/${asset}`;
    try {
      await download(url, installedBinary());
      try { fs.chmodSync(installedBinary(), 0o755); } catch {}
      log('buildwithnexus: installed prebuilt binary.');
      return true;
    } catch {
      // no release asset yet — fall through to a source build
    }
  }

  if (hasCargo() && buildFromSource()) return true;
  return false;
}

function walkthrough(ok) {
  log('');
  if (ok) {
    log('  \x1b[38;5;141mbuildwithnexus\x1b[0m is ready.');
    log('  Run  \x1b[1mbuildwithnexus\x1b[0m  — the first launch walks you through choosing a model:');
    log('    • remote  — Anthropic, OpenAI, OpenRouter, Groq, Hugging Face (paste an API key)');
    log('    • local   — Ollama, llama.cpp, LM Studio (no key, runs on your machine)');
    log('  Then describe a task. It plans, edits files, and runs commands — asking before each change.');
  } else {
    log('  \x1b[33mbuildwithnexus: native binary not available yet.\x1b[0m');
    log('  Install Rust (https://rustup.rs), then:  npm explore buildwithnexus -- npm run build');
  }
  log('');
}

obtain()
  .then(walkthrough)
  .catch(() => walkthrough(false))
  .finally(() => process.exit(0));
