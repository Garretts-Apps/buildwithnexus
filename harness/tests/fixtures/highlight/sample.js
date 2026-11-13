'use strict';

const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const test = require('node:test');

const diag = require('./diagnose.js');
const { target } = require('./resolve-binary.js');

const REPO = path.join(__dirname, '..');
const launcher = path.join(REPO, 'bin', 'buildwithnexus.js');
const STUBS = path.join(__dirname, 'test-fixtures', 'stub-bin');
const FAKE_GLIBC = path.join(__dirname, 'test-fixtures', 'fake-glibc.js');
const FAKE_RELEASE = path.join(__dirname, 'test-fixtures', 'fake-release.js');
const SHORT_TIMERS = path.join(__dirname, 'test-fixtures', 'short-timers.js');
const proxyFixture = require('./test-fixtures/connect-proxy.js');
const {
  noProxyMatch, proxyRoute, proxyAuthorization, openTunnel, proxyHint, envProxySupported, nodeHandlesProxy,
} = require('./bootstrap.js');
const VERSION = require('../package.json').version;
const WIN = process.platform === 'win32';
const needsSh = WIN && 'the stubs are POSIX shell scripts';
const ASSET = target() && `buildwithnexus-${target()}${WIN ? '.exe' : ''}`;
const noRelease = !ASSET && 'no release binary for this platform';
// Real stderr of the 0.14.9 binary on node:20-bullseye (glibc 2.31).
const BULLSEYE_STDERR = [32, 33, 34].map((n) =>
  `/usr/local/lib/node_modules/buildwithnexus/bin/buildwithnexus: /lib/x86_64-linux-gnu/libc.so.6: ` +
  `version \`GLIBC_2.${n}' not found (required by /usr/local/lib/node_modules/buildwithnexus/bin/buildwithnexus)`,
).join('\n');

function run(args) {
  // Node stands in for an installed native binary. No download or API needed.
  return spawnSync(process.execPath, [launcher, ...args], {
    encoding: 'utf8',
    env: { ...process.env, BWN_BIN: process.execPath },
  });
}

// A throwaway NEXUS_HOME, where the launcher keeps downloaded binaries.
function tempHome(t) {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'bwn-home-'));
  t.after(() => fs.rmSync(home, { recursive: true, force: true }));
  return home;
}

// A copy of the published package layout in a temp dir, so a test can put a
// stub (or nothing) where a downloaded binary goes without touching the repo.
// `version` stands in for another release; `home` is shared between packages
// to model npm replacing the package folder on an update.
function tempPackage(t, { home = tempHome(t), version = VERSION } = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'bwn-launcher-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  for (const f of ['package.json', 'bin/buildwithnexus.js', 'scripts/resolve-binary.js',
    'scripts/bootstrap.js', 'scripts/diagnose.js']) {
    fs.mkdirSync(path.join(root, path.dirname(f)), { recursive: true });
    fs.copyFileSync(path.join(REPO, f), path.join(root, f));
  }
  if (version !== VERSION) {
    const manifest = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'));
    fs.writeFileSync(path.join(root, 'package.json'), JSON.stringify({ ...manifest, version }));
  }
  const exe = 'buildwithnexus' + (WIN ? '.exe' : '');
  const dir = path.join(home, 'bin', version);
  return {
    root, home, version,
    bin: path.join(dir, exe),
    marker: path.join(dir, '.installed.json'),
    legacyBin: path.join(root, 'bin', exe),
    legacyMarker: path.join(root, 'bin', '.installed.json'),
  };
}

function launch(pkg, args, env = {}, preload = []) {
  const clean = { ...process.env };
  for (const k of ['BWN_BIN', 'BWN_ALLOW_BOOTSTRAP', 'BWN_SKIP_INSTALL', 'BWN_INSTALL_IN_PACKAGE', 'NODE_OPTIONS',
    'NODE_USE_ENV_PROXY', 'HTTPS_PROXY', 'https_proxy', 'HTTP_PROXY', 'http_proxy']) delete clean[k];
  // stdin is a pipe, not a TTY, so nothing downloads unless a test opts in.
  return spawnSync(process.execPath,
    [...preload.flatMap((p) => ['-r', p]), path.join(pkg.root, 'bin', 'buildwithnexus.js'), ...args], {
      encoding: 'utf8',
      env: { ...clean, NEXUS_HOME: pkg.home, ...env },
      timeout: 60000,
    });
}

function sha256(file) {
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}

// The launcher with bootstrap.js downloading from test-fixtures/fake-release.js
// instead of GitHub. checksums.json pins the served file's hash, or `pin`.
function launchFake(pkg, args, { asset, pin, env = {} } = {}) {
  if (asset) {
    fs.writeFileSync(path.join(pkg.root, 'checksums.json'), JSON.stringify({ [ASSET]: pin || sha256(asset) }));
  }
  const log = path.join(pkg.root, 'requests.log');
  fs.rmSync(log, { force: true });
  const r = launch(pkg, args, {
    NODE_OPTIONS: `--require=${JSON.stringify(FAKE_RELEASE)}`,
    FAKE_ASSET: asset,
    FAKE_REQUESTS: log,
    BWN_ALLOW_BOOTSTRAP: '1',
    ...env,
  });
  r.requests = fs.existsSync(log) ? fs.readFileSync(log, 'utf8').split('\n').filter(Boolean) : [];
  return r;
}

function installStub(pkg, stub, mode = 0o755, dest = pkg.bin) {
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  fs.copyFileSync(path.join(STUBS, stub), dest);
  fs.chmodSync(dest, mode);
}

function facts(over) {
  return diag.hostFacts({ pathPresent: () => false, fileExists: () => true, ...over });
}

const WIN_ENV = { ProgramFiles: 'C:\\Program Files', SystemRoot: 'C:\\Windows' };

// ── existing launcher behavior ──────────────────────────────────────────────

test('--bootstrap is consumed when a binary already exists', () => {
  const result = run(['--bootstrap', '--version']);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout.trim(), process.version);
});

test('launcher preserves literal --bootstrap after --', () => {
  const result = run([
    '--bootstrap', '--bootstrap',
    '-e', 'process.stdout.write(JSON.stringify(process.argv.slice(1)))',
    '--', '--bootstrap',
  ]);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(JSON.parse(result.stdout), ['--bootstrap']);
