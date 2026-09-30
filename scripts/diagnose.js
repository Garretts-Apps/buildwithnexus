'use strict';
// Explains why an installed binary cannot start, so a failed first run says
// what to do instead of "is ready" followed by a raw loader error or
// "spawnSync ... EPERM". Shared by bootstrap.js (right after a download) and
// the launcher (when a run fails). Node builtins only, loaded only on those
// failure paths.
const fs = require('fs');
const path = require('path');
const crypto = require('crypto');
const { spawnSync } = require('child_process');

// Oldest glibc the Linux release binaries run on: the highest GLIBC_x.y
// symbol version they need (readelf -V; 2.34 for 0.14.9 x64 and arm64).
// release.yml reads this value and fails a build that needs a newer one.
const GLIBC_FLOOR = '2.34';
// Default glibc: Ubuntu 22.04 2.35, Debian 12 2.36, EL9 2.34, AL2023 2.34,
// Fedora 35 2.34. Ubuntu 20.04 and Debian 11 (2.31), EL8 (2.28) fall short.
const GLIBC_DISTROS = 'Ubuntu 22.04+, Debian 12+, RHEL/Rocky/AlmaLinux 9+, Amazon Linux 2023, Fedora 35+';

const DOCS_URL = 'https://buildwithnexus.dev/docs/install';
const IT_GUIDE_URL = 'https://github.com/Garretts-Apps/buildwithnexus/blob/main/SECURITY.md#for-it-and-security-teams';
const ISSUES_URL = 'https://github.com/Garretts-Apps/buildwithnexus/issues';
const PROBE_TIMEOUT_MS = 15000;
// NTSTATUS a process exits with when Windows cannot load one of its DLLs.
const STATUS_DLL_NOT_FOUND = 0xC0000135;

// Folders endpoint-protection products install to on Windows: [root env var,
// path under it, product]. They are only stat'ed: querying services from a
// program the product just blocked is itself a "security software discovery"
// signal on the EDR console. Third-party products come first: Defender goes
// passive when another one is installed, so the other one is the likelier
// blocker (U30 triaged Defender on a Falcon box).
const SECURITY_PATHS = [
  ['ProgramFiles', 'CrowdStrike', 'CrowdStrike Falcon'],
  ['SystemRoot', 'System32\\drivers\\CrowdStrike', 'CrowdStrike Falcon'],
  ['ProgramFiles', 'SentinelOne', 'SentinelOne'],
  ['ProgramFiles', 'Cylance', 'Cylance'],
  ['ProgramFiles', 'Confer', 'Carbon Black'], // Carbon Black Cloud sensor
  ['SystemRoot', 'CarbonBlack', 'Carbon Black'], // Carbon Black EDR sensor
  ['ProgramFiles', 'Windows Defender Advanced Threat Protection', 'Microsoft Defender'],
  ['ProgramFiles', 'Windows Defender', 'Microsoft Defender'],
];
const WIN_ROOTS = {
  // The 64-bit folder even from a 32-bit Node.
  ProgramFiles: (env) => env.ProgramW6432 || env.ProgramFiles || 'C:\\Program Files',
  SystemRoot: (env) => env.SystemRoot || 'C:\\Windows',
};

const LOADER_TEXT = /GLIBC_\d|Error relocating|symbol not found|error while loading shared libraries|ld-linux/;

function compareVersions(a, b) {
  const pa = String(a).split('.').map(Number);
  const pb = String(b).split('.').map(Number);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const d = (pa[i] || 0) - (pb[i] || 0);
    if (d) return Math.sign(d);
  }
  return 0;
}

// The glibc this Node runs on; undefined on musl builds of Node.
function runtimeGlibc() {
  try {
    process.report.excludeNetwork = true; // skip the report's DNS lookups
    return process.report.getReport().header.glibcVersionRuntime || null;
  } catch {
    return null;
  }
}

function hasMuslLoader() {
  try {
    return fs.readdirSync('/lib').some((f) => f.startsWith('ld-musl-'));
  } catch {
    return false;
  }
}

// Present when stat works, or fails only because this user may not look
// inside: security products lock their own folders down.
function pathPresent(p) {
  try {
    fs.statSync(p);
    return true;
  } catch (e) {
    return e.code === 'EPERM' || e.code === 'EACCES';
  }
}

// Facts about this machine. Tests pass overrides instead of probing.
function hostFacts(over = {}) {
  const platform = over.platform || process.platform;
  const linux = platform === 'linux';
  const glibc = 'glibc' in over ? over.glibc : linux ? runtimeGlibc() : null;
  const musl = 'musl' in over ? over.musl : linux && !glibc && hasMuslLoader();
  return {
    platform,
    glibc,
    musl,
    env: over.env || process.env,
    pathPresent: over.pathPresent || pathPresent,
    fileExists: over.fileExists || exists,
  };
}

// The SECURITY_PATHS entries as full Windows paths.
function securityPaths(env) {
  return SECURITY_PATHS.map(([root, sub, product]) => [path.win32.join(WIN_ROOTS[root](env), sub), product]);
}

function securityProducts(facts) {
  if (facts.platform !== 'win32') return [];
  const found = [];
  for (const [p, product] of securityPaths(facts.env)) {
    if (found.includes(product)) continue;
    let present = false;
    try { present = facts.pathPresent(p); } catch {}
    if (present) found.push(product);
  }
  return found;
}

function fileSha256(p) {
  try {
    return crypto.createHash('sha256').update(fs.readFileSync(p)).digest('hex');
  } catch {
    return null;
  }
}

function exists(p) {
  try {
    return fs.existsSync(p);
  } catch {
    return false;
  }
}

// Which failure this is, or null when the binary ran as expected.
// f: { bin, error, status, signal, stdout, stderr, expectedVersion,
//      verified (passed the install checksum), vanished, sha256 }
function classify(f, facts) {
  const code = f.error && f.error.code;
  const err = String(f.stderr || '');
  const win = facts.platform === 'win32';
  if (f.vanished || (f.bin && !facts.fileExists(f.bin))) return 'removed';
  if (code === 'ETIMEDOUT') return win ? 'blocked' : 'other';
  // Node reports ERROR_ACCESS_DENIED as EPERM; AppLocker/WDAC blocks as UNKNOWN.
  if (win && ['EPERM', 'EACCES', 'UNKNOWN'].includes(code)) return 'blocked';
  if (code === 'EPERM') return 'blocked';
  if (code === 'EACCES') return 'noexec';
  if (facts.musl && (code === 'ENOENT' || LOADER_TEXT.test(err))) return 'musl';
  if (/GLIBC_\d/.test(err)) return 'glibc';
  if (code) return 'other';
  if (win && f.status === STATUS_DLL_NOT_FOUND) return 'dll';
  if (f.status === 0) {
    if (!f.expectedVersion) return null;
    const want = `buildwithnexus ${f.expectedVersion}`;
    return String(f.stdout || '').trim().split(/\r?\n/)[0] === want ? null : 'version';
  }
  return 'other';
}

function indent(text, pad, max = 6) {
  const lines = String(text || '').trim().split(/\r?\n/).filter(Boolean);
  const shown = lines.slice(0, max).map((l) => pad + l);
  if (lines.length > max) shown.push(`${pad}... (${lines.length - max} more lines)`);
  return shown;
}

function fileBlock(f, withHash) {
  const lines = [`    File:    ${f.bin}`];
  if (withHash) {
    const sum = (f.bin && fileSha256(f.bin)) || f.sha256;
    lines.push(`    SHA-256: ${sum || 'unavailable (the file could not be read)'}`);
  }
  if (f.expectedVersion) lines.push(`    Version: ${f.expectedVersion}`);
  return lines;
}

function fromSource(compiler) {
  return [
    '    - or build it here from source with Rust 1.94+ (https://rustup.rs) and a C',
    `      compiler${compiler}:`,
    '        cargo install buildwithnexus --locked',
    '      then set BWN_BIN to the absolute path of the result',
    '      (usually ~/.cargo/bin/buildwithnexus) so this command uses it.',
  ];
}

function describe(kind, f, facts) {
  const code = f.error && f.error.code;
  const err = String(f.stderr || '');
  const out = [];
  switch (kind) {
    case 'glibc': {
      const wanted = [...err.matchAll(/GLIBC_(\d+(?:\.\d+)+)/g)].map((m) => m[1]);
      const need = wanted.sort(compareVersions).pop() || GLIBC_FLOOR;
      const line = err.split(/\r?\n/).filter((l) => l.includes(`GLIBC_${need}'`)).pop() || '';
      const loader = (line.match(/\S*libc\.so\.\d+: version `GLIBC_[\d.]+' not found/) || [line])[0];
      out.push('buildwithnexus: this system\'s glibc is too old for the prebuilt binary.');
      out.push(`  It needs glibc ${need} or later; this system has ${facts.glibc || 'an older one'}.`);
      out.push(`  Distros that meet it: ${GLIBC_DISTROS}.`);
      out.push('  Either:');
      out.push('    - run it on one of those, or in a container based on one (for example node:22-bookworm)');
      out.push(...fromSource(''));
      if (loader) out.push(`  Loader error: ${loader.trim()}`);
      break;
    }
    case 'musl':
      out.push('buildwithnexus: this system uses musl libc (Alpine or similar), and the prebuilt');
      out.push(`  Linux binary needs glibc ${GLIBC_FLOOR}+. There is no prebuilt musl binary yet. Either:`);
      out.push('    - use a glibc-based system or image, for example node:22-bookworm-slim');
      out.push('      instead of node:22-alpine');
      out.push(...fromSource(' (apk add build-base)'));
      break;
    case 'blocked':
    case 'removed': {
      const win = facts.platform === 'win32';
      if (kind === 'removed') {
        out.push(code
          ? `buildwithnexus: ${win ? 'Windows' : 'the system'} refused to start the binary (${code}), and the file is now gone.`
          : 'buildwithnexus: the binary is gone: it was removed after it was downloaded and verified.');
      } else if (code === 'ETIMEDOUT') {
        out.push(`buildwithnexus: the binary did not answer --version within ${PROBE_TIMEOUT_MS / 1000} s.`);
      } else {
        out.push(`buildwithnexus: ${win ? 'Windows' : 'the system'} refused to start the binary (${code}).`);
      }
      out.push(f.verified
        ? '  It passed its SHA-256 check at install, so it was most likely blocked or'
        : '  It was most likely blocked or');
      if (win) {
        out.push('  quarantined by security software or an application-control policy (AppLocker, WDAC).');
        const products = securityProducts(facts);
        if (products.length) {
          out.push(`  Security software on this machine: ${products.join(', ')}.`);
          out.push("  Look for a block on this file in that product's console or notifications.");
        } else {
          out.push('  No known endpoint product was found in its usual folder; ask IT which one is installed.');
        }
      } else {
        out.push('  removed by a security policy (SELinux, fapolicyd, AppArmor) or an endpoint agent.');
      }
      out.push('  Ask IT to allow this file:');
      out.push(...fileBlock(f, true));
      out.push(`  What to send IT: ${IT_GUIDE_URL}`);
      if (kind === 'removed') {
        out.push('  Once it is allowed, run `bwn --bootstrap` to download it again.');
        out.push('  It is not downloaded again on its own, so a block does not turn into a loop.');
      }
      break;
    }
    case 'noexec': {
      const dir = path.dirname(f.bin || '.');
      out.push('buildwithnexus: permission denied starting the binary (EACCES).');
      out.push('  The file is not executable, or it is on a filesystem mounted noexec.');
      out.push(...fileBlock(f, false));
      out.push(`  Check: ls -l '${f.bin}'`);
      if (facts.platform === 'linux') out.push(`         findmnt -T '${dir}'   (look for noexec)`);
      out.push(`  Fix:   chmod +x '${f.bin}'`);
      out.push('         or install npm packages on a filesystem that allows programs to run,');
      out.push('         or set BWN_BIN to a copy that can run.');
      break;
    }
    case 'dll':
      out.push('buildwithnexus: Windows could not load a DLL the binary needs (0xC0000135).');
      out.push('  It is most likely VCRUNTIME140.dll. Install the Microsoft Visual C++');
      out.push('  Redistributable (x64): https://aka.ms/vs/17/release/vc_redist.x64.exe');
      out.push(...fileBlock(f, false));
      break;
    case 'version': {
      const got = String(f.stdout || '').trim().split(/\r?\n/)[0] || '(nothing)';
      out.push(`buildwithnexus: the binary reported "${got}", expected buildwithnexus ${f.expectedVersion}.`);
      out.push(...fileBlock(f, true));
      out.push(`  Reinstall with npm i -g buildwithnexus@${f.expectedVersion}, or check BWN_BIN if you set it.`);
      break;
    }
    default: {
      const how = f.error
        ? f.error.message
        : f.signal ? `it was stopped by ${f.signal}` : `it exited with status ${f.status}`;
      out.push(`buildwithnexus: the binary did not start: ${how}.`);
      if (code === 'ENOENT' && facts.platform !== 'win32') {
        out.push('  The file exists, so the system could not find the program loader it names:');
        out.push('  usually a binary for another libc or CPU.');
      }
      if (code === 'ENOEXEC') out.push('  The file is not a program this system can run (wrong CPU or OS?).');
      if (err.trim()) {
        out.push('  Output:');
        out.push(...indent(err, '    '));
      }
      out.push(...fileBlock(f, false));
      out.push(`  If this looks like a bug, report it with this output: ${ISSUES_URL}`);
    }
  }
  out.push(`  Docs: ${DOCS_URL}`);
  return out.join('\n') + '\n';
}

// { kind, text } explaining the failure, or null when there is none.
function diagnose(failure, facts = hostFacts()) {
  try {
    const kind = classify(failure, facts);
    return kind ? { kind, text: describe(kind, failure, facts) } : null;
  } catch (e) {
    // Never let the explanation hide the original error.
    const raw = failure.error ? failure.error.message : `status ${failure.status}`;
    return { kind: 'other', text: `buildwithnexus: ${raw} (${e.message})\n  Docs: ${DOCS_URL}\n` };
  }
}

function probe(bin, timeout = PROBE_TIMEOUT_MS) {
  return spawnSync(bin, ['--version'], {
    encoding: 'utf8',
    timeout,
    windowsHide: true,
    stdio: ['ignore', 'pipe', 'pipe'],
  });
}

function fromResult(bin, r, extra) {
  return { bin, error: r.error, status: r.status, signal: r.signal, stdout: r.stdout, stderr: r.stderr, ...extra };
}

// A Linux system the prebuilt binary cannot run on, known without
// downloading it: musl, or a glibc below the floor. null otherwise.
function platformProblem(facts = hostFacts()) {
  if (facts.platform !== 'linux') return null;
  let kind = null;
  if (facts.musl) kind = 'musl';
  else if (facts.glibc && compareVersions(facts.glibc, GLIBC_FLOOR) < 0) kind = 'glibc';
  return kind && { kind, text: describe(kind, {}, facts) };
}

// bootstrap.js: does the binary it just installed run and report the
// expected version? null if so, else the diagnosis.
function checkInstalled(bin, expectedVersion, { facts, sha256 } = {}) {
  const f = facts || hostFacts();
  const extra = { expectedVersion, verified: true, sha256 };
  if (!f.fileExists(bin)) return diagnose({ bin, vanished: true, ...extra }, f);
  return diagnose(fromResult(bin, probe(bin), extra), f);
}

// Whether a failed run may mean the binary never started. The launcher
// inherits stdio, so it cannot see a loader message; it re-runs --version
// with output captured only in these cases.
function needsProbe(status, facts) {
  if (status === 0 || status === null) return false;
  if (status === 127) return true;
  if (facts.platform !== 'linux') return false;
  return Boolean(facts.musl || (facts.glibc && compareVersions(facts.glibc, GLIBC_FLOOR) < 0));
}

// Launcher: explain a run that failed before the binary started. null when
// the binary itself ran (its own exit status is the answer).
function explainRunFailure(bin, result, { facts, verified, sha256 } = {}) {
  const f = facts || hostFacts();
  const extra = { verified, sha256 };
  // An error means it never started. It is not run again: a second attempt
  // at a blocked file is one more alert on the IT console.
  if (result.error) return diagnose({ bin, error: result.error, ...extra }, f);
  if (f.platform === 'win32' && result.status === STATUS_DLL_NOT_FOUND) {
    return diagnose({ bin, status: result.status, ...extra }, f);
  }
  if (!needsProbe(result.status, f)) return null;
  const r = probe(bin);
  if (!r.error && r.status === 0) return null;
  return diagnose(fromResult(bin, r, extra), f);
}

module.exports = {
  GLIBC_FLOOR,
  GLIBC_DISTROS,
  IT_GUIDE_URL,
  SECURITY_PATHS,
  securityPaths,
  compareVersions,
  hostFacts,
  securityProducts,
  fileSha256,
  diagnose,
  checkInstalled,
  needsProbe,
  platformProblem,
  explainRunFailure,
};
