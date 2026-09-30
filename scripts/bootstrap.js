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
const { pipeline } = require('stream');
const {
  ROOT, ext, target, installedBinary, installMarker, existing, EXIT_EXPLAINED,
} = require('./resolve-binary.js');

const pkg = require(path.join(ROOT, 'package.json'));
const RELEASES = 'https://github.com/Garretts-Apps/buildwithnexus/releases';
const DOCS_URL = 'https://buildwithnexus.dev/docs/install';
// A request that gets no bytes for this long, connecting or downloading,
// fails. Without it a network that only allows a proxy hung with no output.
const IDLE_TIMEOUT_MS = 30000;

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
    let response;
    let timer;
    const stalled = () => {
      const e = Object.assign(
        new Error(`no response from ${new URL(url).host} for ${IDLE_TIMEOUT_MS / 1000} s`), { code: 'ETIMEDOUT' });
      // Fail the body stream with this error too, not a bare "aborted".
      if (response) response.destroy(e);
      req.destroy(e);
    };
    // Restarted by every chunk. The request's own timeout option (still set:
    // Node's proxy tunnel uses it) waits twice as long on a stuck TLS handshake.
    const wait = () => {
      clearTimeout(timer);
      timer = setTimeout(stalled, IDLE_TIMEOUT_MS);
    };
    const req = https.get(url, {
      headers: { 'user-agent': 'buildwithnexus-installer' },
      timeout: IDLE_TIMEOUT_MS,
    }, (res) => {
      response = res;
      wait();
      res.on('data', wait);
      res.on('close', () => clearTimeout(timer));
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        res.resume();
        const next = new URL(res.headers.location, url).toString();
        return resolve(get(next, redirects + 1, onResponse));
      }
      if (res.statusCode !== 200) {
        res.resume();
        return reject(new Error(`HTTP ${res.statusCode} from ${new URL(url).host}`));
      }
      onResponse(res, resolve, reject);
    });
    wait();
    req.on('timeout', stalled);
    req.on('error', (e) => {
      clearTimeout(timer);
      reject(e);
    });
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

// Streams the asset to dest and returns the SHA-256 of the bytes received,
// so a file quarantined as soon as it is closed still counts as verified.
function download(url, dest) {
  return get(url, 0, (res, resolve, reject) => {
    fs.mkdirSync(path.dirname(dest), { recursive: true });
    const hash = crypto.createHash('sha256');
    const file = fs.createWriteStream(dest);
    res.on('data', (c) => hash.update(c));
    pipeline(res, file, (err) => {
      if (err) return reject(err);
      // Closed before anything renames it (Windows keeps open files in place).
      file.close(() => resolve(hash.digest('hex')));
    });
  });
}

function pinnedChecksum(asset) {
  try {
    const sums = JSON.parse(fs.readFileSync(path.join(ROOT, 'checksums.json'), 'utf8'));
    return typeof sums[asset] === 'string' ? sums[asset] : null;
  } catch {
    return null;
  }
}

function sha256(file) {
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}

function discard(file) {
  try { fs.unlinkSync(file); } catch {}
}

// What obtain() did:
//   { installed: sha256 }  downloaded, verified and moved into place
//   { present: true }      a binary was already there
//   { removed: sha256 }    downloaded and verified, then gone before it ran
//   { error }              nothing installed, and why
//   {}                     nothing to do (BWN_SKIP_INSTALL, or no prebuilt)
async function obtain() {
  if (process.env.BWN_SKIP_INSTALL) return {};
  if (existing()) return { present: true };
  const t = target();
  if (!t) return {};

  const asset = `buildwithnexus-${t}${ext()}`;
  const base = `${RELEASES}/download/v${pkg.version}`;
  const bin = installedBinary();
  const tmp = bin + '.download';
  let got;
  try {
    log('buildwithnexus: downloading prebuilt binary…');
    // The expected hash comes from checksums.json in this npm package:
    // publish.yml writes it only after checking each binary's build
    // attestation, and the tarball cannot change after publishing. Release
    // assets can, so the release's own .sha256 is only a fallback for
    // checkouts that have no checksums.json.
    const expected = pinnedChecksum(asset) ??
      (await fetchText(`${base}/${asset}.sha256`)).trim().split(/\s+/)[0];
    if (!/^[0-9a-f]{64}$/i.test(expected || '')) throw new Error('missing/invalid checksum');
    got = await download(`${base}/${asset}`, tmp);
    if (got !== expected.toLowerCase()) throw new Error('checksum mismatch — refusing to install');
  } catch (e) {
    discard(tmp);
    return { error: e };
  }
  // Verified. Record that before touching the file again: security software
  // that quarantines on write removes it within moments, and later runs must
  // explain that instead of downloading it again (U29).
  try {
    fs.writeFileSync(installMarker(), JSON.stringify({ version: pkg.version, sha256: got }) + '\n');
  } catch {}
  try {
    // What is on disk is what will run, so it must hash the same.
    if (sha256(tmp) !== got) throw new Error('the file changed after it was verified — refusing to install');
    fs.renameSync(tmp, bin);
  } catch (e) {
    if (!fs.existsSync(tmp) && !fs.existsSync(bin)) return { removed: got };
    discard(tmp);
    discard(installMarker());
    return { error: e };
  }
  try { fs.chmodSync(bin, 0o755); } catch {}
  log('buildwithnexus: installed prebuilt binary (sha256 verified).');
  return { installed: got };
}

const NETWORK_CODES = new Set(['ENOTFOUND', 'EAI_AGAIN', 'ECONNREFUSED', 'ECONNRESET', 'ETIMEDOUT',
  'ENETUNREACH', 'EHOSTUNREACH', 'ENETDOWN', 'EPIPE', 'ERR_PROXY_TUNNEL']);
const TLS_CODES = new Set(['SELF_SIGNED_CERT_IN_CHAIN', 'UNABLE_TO_GET_ISSUER_CERT_LOCALLY',
  'UNABLE_TO_VERIFY_LEAF_SIGNATURE', 'DEPTH_ZERO_SELF_SIGNED_CERT', 'CERT_UNTRUSTED']);

// NODE_USE_ENV_PROXY=1 makes the https module use HTTPS_PROXY/NO_PROXY from
// Node 22.21 and 24.5 on (checked on 22.20/22.21, 23.11, 24.4/24.5, 25, 26).
function envProxySupported(version = process.versions.node) {
  const [major, minor] = version.split('.').map(Number);
  return major >= 25 || (major === 24 && minor >= 5) || (major === 22 && minor >= 21);
}

function proxyHint(win) {
  const proxyVar = ['HTTPS_PROXY', 'https_proxy', 'HTTP_PROXY', 'http_proxy'].find((k) => process.env[k]);
  const node = `Node ${process.versions.node}`;
  if (process.env.NODE_USE_ENV_PROXY === '1' && envProxySupported()) {
    return [proxyVar
      ? `  It went through the proxy in ${proxyVar}; check that the proxy allows github.com.`
      : '  If this network needs a proxy, set HTTPS_PROXY to it.'];
  }
  const lines = [proxyVar
    ? `  ${proxyVar} is set, but Node's https module ignores it unless NODE_USE_ENV_PROXY=1 is set too.`
    : "  If this network needs a proxy: Node's https module uses HTTPS_PROXY only with NODE_USE_ENV_PROXY=1."];
  if (!envProxySupported()) {
    lines.push(`  Only Node 22.21+ and 24.5+ support that (this is ${node}), so this Node cannot`,
      '  download through a proxy.');
  } else if (win) {
    lines.push(`  ${node} supports it. In PowerShell:`,
      `    ${proxyVar ? '' : "$env:HTTPS_PROXY = 'http://<proxy>:<port>'; "}$env:NODE_USE_ENV_PROXY = '1'; bwn --bootstrap`);
  } else {
    lines.push(`  ${node} supports it:`,
      `    ${proxyVar ? '' : 'HTTPS_PROXY=http://<proxy>:<port> '}NODE_USE_ENV_PROXY=1 bwn --bootstrap`);
  }
  return lines;
}

// Why nothing was installed, and what to do about it.
function downloadFailure(e) {
  const code = e && e.code;
  const msg = (e && e.message) || String(e);
  const win = process.platform === 'win32';
  const out = [`buildwithnexus: could not download the prebuilt binary: ${msg}${code && !msg.includes(code) ? ` (${code})` : ''}.`];
  if (TLS_CODES.has(code)) {
    out.push('  The connection was not trusted. A proxy that inspects TLS does this: point');
    out.push("  NODE_EXTRA_CA_CERTS at your organization's root certificate (a PEM file).");
  }
  if (NETWORK_CODES.has(code) || TLS_CODES.has(code)) out.push(...proxyHint(win));
  out.push('  To try again:  bwn --bootstrap');
  out.push(`  Other ways to install (platform package, source build and BWN_BIN): ${DOCS_URL}`);
  return out.join('\n') + '\n';
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
    log('  and point BWN_BIN at the built binary (an absolute path).');
  }
  log('');
}

// The launcher shows nothing more after EXIT_EXPLAINED, so each explained
// outcome says everything itself.
function explained(text) {
  process.stderr.write('\n' + text);
  process.exit(EXIT_EXPLAINED);
}

obtain()
  .then((r) => {
    const { checkInstalled, diagnose } = require('./diagnose.js');
    if (r.removed) {
      explained(diagnose({
        bin: installedBinary(), vanished: true, verified: true, sha256: r.removed, expectedVersion: pkg.version,
      }).text);
    }
    if (r.error) explained(downloadFailure(r.error));
    if (r.installed) {
      // "Verified" only means the download is intact. Say "ready" only once
      // it has run: a glibc too old, musl, or endpoint protection all show up
      // here, and each gets its own explanation instead of a raw error later.
      let problem;
      try {
        problem = checkInstalled(installedBinary(), pkg.version, { sha256: r.installed });
      } catch (e) {
        problem = { text: `buildwithnexus: could not check the installed binary: ${e.message}\n` };
      }
      if (problem) explained(problem.text);
    }
    const ok = Boolean(r.installed || r.present);
    walkthrough(ok);
    process.exit(ok ? 0 : 1);
  })
  .catch((e) => explained(`buildwithnexus: first-run setup failed: ${e.message}\n  Docs: ${DOCS_URL}\n`));
