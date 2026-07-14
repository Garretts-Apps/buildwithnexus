'use strict';
// First-run bootstrap: fetch the checksum-verified prebuilt binary for this
// platform from the GitHub Release. Runs from the launcher the first time the
// CLI starts (NOT as an npm install script — installs stay script-free).
// Only ever downloads from GitHub's own hosts, and refuses any binary whose
// SHA-256 doesn't match the published checksum.
const fs = require('fs');
const path = require('path');
const https = require('https');
const crypto = require('crypto');
const { ROOT, ext, target, installedBinary, existing } = require('./resolve-binary.js');

const pkg = require(path.join(ROOT, 'package.json'));

function log(m) {
  process.stdout.write(m + '\n');
}

// Only ever fetch from GitHub's own hosts, including on redirects.
function allowedHost(u) {
  const h = new URL(u).host;
  return h === 'github.com' || h === 'objects.githubusercontent.com' || h.endsWith('.githubusercontent.com');
}

function get(url, redirects, onResponse) {
  return new Promise((resolve, reject) => {
    if (redirects > 5) return reject(new Error('too many redirects'));
    if (!allowedHost(url)) return reject(new Error(`refusing non-GitHub host ${new URL(url).host}`));
    https.get(url, { headers: { 'user-agent': 'buildwithnexus-installer' } }, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        res.resume();
        const next = new URL(res.headers.location, url).toString();
        return resolve(get(next, redirects + 1, onResponse));
      }
      if (res.statusCode !== 200) {
        res.resume();
        return reject(new Error(`HTTP ${res.statusCode}`));
      }
      onResponse(res, resolve, reject);
    }).on('error', reject);
  });
}

function fetchText(url) {
  return get(url, 0, (res, resolve, reject) => {
    let s = '';
    res.setEncoding('utf8');
    res.on('data', (c) => (s += c));
    res.on('end', () => resolve(s));
    res.on('error', reject);
  });
}

function download(url, dest) {
  return get(url, 0, (res, resolve, reject) => {
    fs.mkdirSync(path.dirname(dest), { recursive: true });
    const file = fs.createWriteStream(dest);
    res.pipe(file);
    file.on('finish', () => file.close(() => resolve()));
    file.on('error', reject);
  });
}

function sha256(file) {
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}

async function obtain() {
  if (process.env.BWN_SKIP_INSTALL) return false;
  if (existing()) return true;

  const t = target();
  if (t) {
    const asset = `buildwithnexus-${t}${ext()}`;
    const base = `https://github.com/Garretts-Apps/buildwithnexus/releases/download/v${pkg.version}`;
    const tmp = installedBinary() + '.download';
    try {
      log('buildwithnexus: downloading prebuilt binary…');
      // Fetch the expected hash first; refuse to install anything that doesn't match.
      const expected = (await fetchText(`${base}/${asset}.sha256`)).trim().split(/\s+/)[0];
      if (!/^[0-9a-f]{64}$/i.test(expected || '')) throw new Error('missing/invalid checksum');
      await download(`${base}/${asset}`, tmp);
      const got = sha256(tmp);
      if (got.toLowerCase() !== expected.toLowerCase()) {
        throw new Error('checksum mismatch — refusing to install');
      }
      fs.renameSync(tmp, installedBinary());
      try { fs.chmodSync(installedBinary(), 0o755); } catch {}
      log('buildwithnexus: installed prebuilt binary (sha256 verified).');
      return true;
    } catch (e) {
      try { if (fs.existsSync(tmp)) fs.unlinkSync(tmp); } catch {}
      log(`buildwithnexus: prebuilt unavailable (${e.message}).`);
    }
  }
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
    log('  Build from source: git clone https://github.com/Garretts-Apps/buildwithnexus');
    log('  then: cargo build --release --manifest-path harness/Cargo.toml');
    log('  and point BWN_BIN at the built binary.');
  }
  log('');
}

obtain()
  .then((ok) => {
    walkthrough(ok);
    process.exit(ok ? 0 : 1);
  })
  .catch(() => {
    walkthrough(false);
    process.exit(1);
  });
