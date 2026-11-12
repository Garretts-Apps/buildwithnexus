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
});

test('ordinary arguments still reach the installed binary', () => {
  const result = run(['--version']);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout.trim(), process.version);
});

test('a failing run of a working binary passes its status through quietly', () => {
  const result = spawnSync(process.execPath, [launcher, '-e', 'process.exit(3)'], {
    encoding: 'utf8',
    env: { ...process.env, BWN_BIN: process.execPath },
  });
  assert.equal(result.status, 3);
  assert.equal(result.stderr, '');
});

// ── diagnosis, with the machine's facts injected ────────────────────────────

test('glibc below the floor: names the floor, the distros and the fixes', () => {
  const found = diag.diagnose(
    { bin: '/x/bin/buildwithnexus', status: 1, stderr: BULLSEYE_STDERR, expectedVersion: VERSION },
    facts({ platform: 'linux', glibc: '2.31' }),
  );
  assert.equal(found.kind, 'glibc');
  assert.match(found.text, /needs glibc 2\.34 or later; this system has 2\.31/);
  assert.match(found.text, /Ubuntu 22\.04\+, Debian 12\+, RHEL\/Rocky\/AlmaLinux 9\+/);
  assert.match(found.text, /cargo install buildwithnexus --locked/);
  assert.match(found.text, /Loader error: .*GLIBC_2\.34' not found/);
  assert.doesNotMatch(found.text, /is ready/);
});

test('musl: says Alpine is not supported yet and offers a glibc image or a source build', () => {
  const found = diag.diagnose(
    { bin: '/x/bin/buildwithnexus', error: Object.assign(new Error('spawnSync x ENOENT'), { code: 'ENOENT' }) },
    facts({ platform: 'linux', glibc: null, musl: true }),
  );
  assert.equal(found.kind, 'musl');
  assert.match(found.text, /musl libc \(Alpine or similar\)/);
  assert.match(found.text, /node:22-bookworm-slim/);
  assert.match(found.text, /apk add build-base/);
  assert.doesNotMatch(found.text, /spawnSync/);
});

test('Windows EPERM names every security product found, third-party first', () => {
  const asked = [];
  const present = new Set(['C:\\Program Files\\Windows Defender', 'C:\\Windows\\System32\\drivers\\CrowdStrike']);
  const found = diag.diagnose(
    {
      bin: 'C:\\Users\\u\\AppData\\Roaming\\npm\\node_modules\\buildwithnexus\\bin\\buildwithnexus.exe',
      error: Object.assign(new Error('spawnSync buildwithnexus.exe EPERM'), { code: 'EPERM' }),
      verified: true,
      sha256: 'ab'.repeat(32),
      expectedVersion: VERSION,
    },
    facts({ platform: 'win32', env: WIN_ENV, pathPresent: (p) => (asked.push(p), present.has(p)) }),
  );
  assert.equal(found.kind, 'blocked');
  // Each product's folders until one is found, third-party products first.
  assert.deepEqual(asked, [
    'C:\\Program Files\\CrowdStrike',
    'C:\\Windows\\System32\\drivers\\CrowdStrike',
    'C:\\Program Files\\SentinelOne',
    'C:\\Program Files\\Cylance',
    'C:\\Program Files\\Confer',
    'C:\\Windows\\CarbonBlack',
    'C:\\Program Files\\Windows Defender Advanced Threat Protection',
    'C:\\Program Files\\Windows Defender',
  ]);
  assert.match(found.text, /Windows refused to start the binary \(EPERM\)/);
  assert.match(found.text, /passed its SHA-256 check/);
  assert.match(found.text, /Security software on this machine: CrowdStrike Falcon, Microsoft Defender\./);
  assert.match(found.text, /File: +C:\\Users\\u\\AppData.*buildwithnexus\.exe/);
  assert.match(found.text, new RegExp(`SHA-256: ${'ab'.repeat(32)}`));
  assert.ok(found.text.includes(diag.IT_GUIDE_URL));
});

test('Windows block with no known product still says what to do', () => {
  for (const code of ['EACCES', 'UNKNOWN']) {
    const found = diag.diagnose(
      { bin: 'C:\\b.exe', error: Object.assign(new Error(code), { code }) },
      facts({ platform: 'win32' }),
    );
    assert.equal(found.kind, 'blocked', code);
    assert.match(found.text, /AppLocker, WDAC/);
    assert.match(found.text, /No known endpoint product was found in its usual folder/);
  }
});

test('each product maps from its install or driver folder', () => {
  const folders = {
    'C:\\Program Files\\CrowdStrike': 'CrowdStrike Falcon',
    'C:\\Windows\\System32\\drivers\\CrowdStrike': 'CrowdStrike Falcon',
    'C:\\Program Files\\SentinelOne': 'SentinelOne',
    'C:\\Program Files\\Cylance': 'Cylance',
    'C:\\Program Files\\Confer': 'Carbon Black',
    'C:\\Windows\\CarbonBlack': 'Carbon Black',
    'C:\\Program Files\\Windows Defender Advanced Threat Protection': 'Microsoft Defender',
    'C:\\Program Files\\Windows Defender': 'Microsoft Defender',
  };
  for (const [folder, product] of Object.entries(folders)) {
    const f = facts({ platform: 'win32', env: WIN_ENV, pathPresent: (p) => p === folder });
    assert.deepEqual(diag.securityProducts(f), [product], folder);
  }
  assert.deepEqual(diag.securityProducts(facts({ platform: 'linux', pathPresent: () => true })), []);
  // A 32-bit Node sees the x86 folder as ProgramFiles; the products live in the 64-bit one.
  const wow = { ...WIN_ENV, ProgramFiles: 'C:\\Program Files (x86)', ProgramW6432: 'C:\\Program Files' };
  assert.equal(diag.securityPaths(wow)[0][0], 'C:\\Program Files\\CrowdStrike');
});

test('finding security products starts no process', (t) => {
  // Querying services from a program the product just blocked is itself an
  // EDR signal; the check may only look at paths.
  const cp = require('node:child_process');
  const started = [];
  for (const fn of ['spawn', 'spawnSync', 'exec', 'execSync', 'execFile', 'execFileSync', 'fork']) {
    t.mock.method(cp, fn, () => { started.push(fn); throw new Error(`${fn} called`); });
  }
  const file = require.resolve('./diagnose.js');
  delete require.cache[file];
  t.after(() => delete require.cache[file]);
  const fresh = require('./diagnose.js');
  const found = fresh.diagnose(
    { bin: 'C:\\b.exe', error: Object.assign(new Error('EPERM'), { code: 'EPERM' }), verified: true },
    fresh.hostFacts({ platform: 'win32', env: WIN_ENV, fileExists: () => true }),
  );
  assert.equal(found.kind, 'blocked');
  assert.deepEqual(started, []);
});

test('a binary gone after a verified install is reported as removed, with the recorded hash', () => {
  const found = diag.diagnose(
    { bin: 'C:\\b.exe', vanished: true, verified: true, sha256: 'cd'.repeat(32) },
    facts({
      platform: 'win32', env: WIN_ENV, fileExists: () => false,
      pathPresent: (p) => p === 'C:\\Program Files\\CrowdStrike',
    }),
  );
  assert.equal(found.kind, 'removed');
  assert.match(found.text, /removed after it was downloaded and verified/);
  assert.match(found.text, /CrowdStrike Falcon/);
  assert.match(found.text, new RegExp(`SHA-256: ${'cd'.repeat(32)}`));
  assert.match(found.text, /run `bwn --bootstrap` to download it again/);
});

test('EPERM followed by the file disappearing keeps the error code', () => {
  const found = diag.diagnose(
    { bin: 'C:\\b.exe', error: Object.assign(new Error('EPERM'), { code: 'EPERM' }) },
    facts({ platform: 'win32', fileExists: () => false }),
  );
  assert.equal(found.kind, 'removed');
  assert.match(found.text, /refused to start the binary \(EPERM\), and the file is now gone/);
});

test('Linux EACCES points at the execute bit and noexec mounts', () => {
  const found = diag.diagnose(
    { bin: '/home/u/.npm/lib/node_modules/buildwithnexus/bin/buildwithnexus',
      error: Object.assign(new Error('EACCES'), { code: 'EACCES' }) },
    facts({ platform: 'linux', glibc: '2.39' }),
  );
  assert.equal(found.kind, 'noexec');
  assert.match(found.text, /chmod \+x/);
  assert.match(found.text, /findmnt -T '\/home\/u\/\.npm\/lib\/node_modules\/buildwithnexus\/bin'/);
});

test('Linux EPERM names security policies, not Windows products', () => {
  const found = diag.diagnose(
    { bin: '/b', error: Object.assign(new Error('EPERM'), { code: 'EPERM' }) },
    facts({ platform: 'linux', glibc: '2.39' }),
  );
  assert.equal(found.kind, 'blocked');
  assert.match(found.text, /SELinux, fapolicyd, AppArmor/);
});

test('a missing DLL on Windows points at the VC++ runtime', () => {
  const found = diag.diagnose({ bin: 'C:\\b.exe', status: 0xC0000135 }, facts({ platform: 'win32' }));
  assert.equal(found.kind, 'dll');
  assert.match(found.text, /vc_redist\.x64\.exe/);
});

test('version check: the expected version passes, anything else is explained', () => {
  const f = facts({ platform: 'linux', glibc: '2.39' });
  const ok = { bin: '/b', status: 0, stdout: `buildwithnexus ${VERSION}\n`, expectedVersion: VERSION };
  assert.equal(diag.diagnose(ok, f), null);
  const found = diag.diagnose({ ...ok, stdout: 'buildwithnexus 0.0.1\n' }, f);
  assert.equal(found.kind, 'version');
  assert.match(found.text, new RegExp(`reported "buildwithnexus 0\\.0\\.1", expected buildwithnexus ${VERSION}`));
});

test('anything else shows the real error and the docs', () => {
  const found = diag.diagnose(
    { bin: '/b', status: 127, stderr: 'error while loading shared libraries: libz.so.1' },
    facts({ platform: 'linux', glibc: '2.39' }),
  );
  assert.equal(found.kind, 'other');
  assert.match(found.text, /exited with status 127/);
  assert.match(found.text, /libz\.so\.1/);
  assert.match(found.text, /Docs: https:\/\/buildwithnexus\.dev\/docs\/install/);
});

test('the launcher re-checks a failed run only when the binary may not have started', () => {
  const linux = (glibc, musl = false) => facts({ platform: 'linux', glibc, musl });
  assert.equal(diag.needsProbe(127, facts({ platform: 'darwin' })), true);
  assert.equal(diag.needsProbe(1, linux('2.31')), true);
  assert.equal(diag.needsProbe(1, linux(null, true)), true);
  assert.equal(diag.needsProbe(1, linux('2.34')), false);
  assert.equal(diag.needsProbe(1, linux('2.39')), false);
  assert.equal(diag.needsProbe(1, facts({ platform: 'win32' })), false);
  assert.equal(diag.needsProbe(0, linux('2.31')), false);
  assert.equal(diag.needsProbe(null, linux('2.31')), false);
});

test('version comparison is numeric', () => {
  assert.equal(diag.compareVersions('2.4', '2.34'), -1);
  assert.equal(diag.compareVersions('2.34', '2.34.0'), 0);
  assert.equal(diag.compareVersions('2.39', '2.34'), 1);
});

test('the docs state the glibc floor the binaries are checked against', () => {
  const floor = diag.GLIBC_FLOOR.replace('.', '\\.');
  for (const doc of ['README.md', 'SECURITY.md']) {
    const text = fs.readFileSync(path.join(REPO, doc), 'utf8');
    assert.match(text, new RegExp(`glibc ${floor} or later`), doc);
  }
  // The gate reads the constant rather than repeating the number, and runs
  // on PRs (ci.yml) as well as on each release binary.
  const gate = fs.readFileSync(path.join(__dirname, 'check-glibc-floor.sh'), 'utf8');
  assert.match(gate, /require\(process\.argv\[1\]\)\.GLIBC_FLOOR" "\$here\/diagnose\.js"/);
  for (const wf of ['ci.yml', 'release.yml']) {
    const text = fs.readFileSync(path.join(REPO, '.github', 'workflows', wf), 'utf8');
    assert.match(text, /scripts\/check-glibc-floor\.sh /, wf);
  }
});

test('the glibc gate fails a binary that needs more than the floor', {
  // CI runs it on ubuntu only; Alpine has no bash.
  skip: (process.platform !== 'linux' || spawnSync('bash', ['-c', 'true']).status !== 0) && 'needs Linux and bash',
}, (t) => {
  // A fake readelf stands in for real binaries.
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'bwn-gate-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  fs.writeFileSync(path.join(dir, 'readelf'),
    '#!/bin/sh\n[ "$1" = -V ] && printf "%s\\n" $FAKE_GLIBC_NEEDS | sed "s/^/  Name: GLIBC_/"\n' +
    '[ "$1" = -W ] && printf "0: 0 0 FUNC GLOBAL DEFAULT UND clock_gettime@GLIBC_2.99 (5)\\n"\nexit 0\n',
    { mode: 0o755 });
  const gate = (needs) => spawnSync('bash', [path.join(__dirname, 'check-glibc-floor.sh'), 'the-binary'], {
    encoding: 'utf8',
    env: { ...process.env, PATH: `${dir}${path.delimiter}${process.env.PATH}`, FAKE_GLIBC_NEEDS: needs },
  });
  const ok = gate('2.2.5 2.17 2.34');
  assert.equal(ok.status, 0, ok.stdout + ok.stderr);
  assert.match(ok.stdout, /needs glibc 2\.34; the floor is 2\.34/);
  const newer = gate('2.2.5 2.34 2.35');
  assert.equal(newer.status, 1);
  assert.match(newer.stdout, /::error::the-binary needs glibc 2\.35; the documented floor is 2\.34/);
  assert.match(newer.stdout, /clock_gettime@GLIBC_2\.99/);
  const none = gate('');
  assert.equal(none.status, 1);
  assert.match(none.stdout, /::error::the-binary has no GLIBC_ symbol versions/);
});

test('package.json, both crates and Cargo.lock carry the same version', () => {
  // "Ready" needs the binary's --version to equal package.json's version, so
  // a bump that misses a file would fail every first run.
  const { version, problems } = require('./check-versions.js').check(REPO);
  assert.equal(version, VERSION);
  assert.deepEqual(problems, []);
});

test('docs/VERSIONING.md lists every --json event type and the schema version', () => {
  const doc = fs.readFileSync(path.join(REPO, 'docs', 'VERSIONING.md'), 'utf8');
  const report = fs.readFileSync(path.join(REPO, 'harness', 'src', 'report.rs'), 'utf8');
  const types = [...new Set([...report.matchAll(/emit\(\s*json!\(\{"type": "(\w+)"/g)].map((m) => m[1]))];
  assert.ok(types.length >= 10, types.join(','));
  for (const ty of types) assert.match(doc, new RegExp(`^\\| \`${ty}\` \\|`, 'm'), ty);
  const version = report.match(/pub const JSON_SCHEMA_VERSION: u32 = (\d+);/)[1];
  assert.match(doc, new RegExp(`\`schema_version\`,\\s+now \`${version}\``));
});

test('the version gate names each file that disagrees', (t) => {
  const { check } = require('./check-versions.js');
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'bwn-versions-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  for (const f of ['package.json', 'harness/Cargo.toml', 'bwn/Cargo.toml', 'Cargo.lock', 'CHANGELOG.md']) {
    fs.mkdirSync(path.dirname(path.join(dir, f)), { recursive: true });
    fs.copyFileSync(path.join(REPO, f), path.join(dir, f));
  }
  const edit = (f, from, to) => {
    const p = path.join(dir, f);
    const text = fs.readFileSync(p, 'utf8');
    assert.ok(text.includes(from), `${f} has ${from}`);
    fs.writeFileSync(p, text.replace(from, to));
  };
  assert.deepEqual(check(dir, { requireChangelog: true }).problems, []);

  // A release bump that only touched package.json and harness/Cargo.toml.
  edit('package.json', `"version": "${VERSION}"`, '"version": "9.8.7"');
  edit('harness/Cargo.toml', `version = "${VERSION}"`, 'version = "9.8.7"');
  const { problems } = check(dir, { requireChangelog: true });
  const files = problems.map((p) => p.split(':')[0]);
  assert.deepEqual(files, ['bwn/Cargo.toml', 'bwn/Cargo.toml', 'Cargo.lock', 'Cargo.lock', 'CHANGELOG.md'], problems.join('\n'));
  assert.match(problems[1], /buildwithnexus dependency version is .*, package\.json is 9\.8\.7/);
  assert.match(problems[4], /no "## \[9\.8\.7\]" section/);
  // The CHANGELOG section is only required before the version is tagged.
  assert.equal(check(dir).problems.length, 4);

  // The CLI exits 1 and prints a GitHub annotation per file.
  const r = spawnSync(process.execPath, [path.join(__dirname, 'check-versions.js'), '--require-changelog', dir], { encoding: 'utf8' });
  assert.equal(r.status, 1, r.stdout + r.stderr);
  assert.equal(r.stdout.match(/^::error::/gm).length, 5);
  assert.match(r.stdout, /^::error::Cargo\.lock: bwn version is /m);

  const ci = fs.readFileSync(path.join(REPO, '.github', 'workflows', 'ci.yml'), 'utf8');
  assert.match(ci, /2\) node scripts\/check-versions\.js --require-changelog ;;/);
});

// ── post-install check (bootstrap.js) against stub binaries ─────────────────

test('bootstrap check: a stub that fails with a glibc loader error gets glibc guidance', { skip: needsSh }, () => {
  const found = diag.checkInstalled(path.join(STUBS, 'glibc-too-old'), VERSION,
    { facts: diag.hostFacts({ glibc: '2.31' }) });
  assert.equal(found.kind, 'glibc');
  assert.match(found.text, /this system has 2\.31/);
  assert.match(found.text, /GLIBC_2\.34' not found/);
});

test('bootstrap check: "ready" needs the expected version', { skip: needsSh }, (t) => {
  const stub = path.join(STUBS, 'prints-version');
  t.after(() => delete process.env.STUB_VERSION);
  process.env.STUB_VERSION = VERSION;
  assert.equal(diag.checkInstalled(stub, VERSION), null);
  process.env.STUB_VERSION = '0.0.1';
  assert.equal(diag.checkInstalled(stub, VERSION).kind, 'version');
});

test('bootstrap check: a binary gone right after install is reported as removed', () => {
  const found = diag.checkInstalled(path.join(os.tmpdir(), 'bwn-no-such-binary'), VERSION,
    { sha256: 'ef'.repeat(32) });
  assert.equal(found.kind, 'removed');
  assert.match(found.text, new RegExp(`SHA-256: ${'ef'.repeat(32)}`));
});

// ── the launcher end to end, with stubs where the download goes ─────────────

test('launcher: a non-executable binary gets EACCES guidance, not a bare spawnSync error', { skip: needsSh }, (t) => {
  const pkg = tempPackage(t);
  installStub(pkg, 'prints-version', 0o644);
  const r = launch(pkg, ['--version']);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /permission denied starting the binary \(EACCES\)/);
  // The download is in the user's home now, not the npm prefix.
  assert.match(r.stderr, /point NEXUS_HOME at a filesystem that allows programs to run/);
  assert.ok(r.stderr.includes(pkg.bin));
  assert.doesNotMatch(r.stderr, /^buildwithnexus: spawnSync/m);
});

test('launcher: glibc loader failure on an old distro gets glibc guidance', {
  skip: process.platform !== 'linux' && 'fakes the Linux glibc report',
}, (t) => {
  const pkg = tempPackage(t);
  installStub(pkg, 'glibc-too-old');
  const r = launch(pkg, ['--version'], { FAKE_GLIBC: '2.31' }, [FAKE_GLIBC]);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /needs glibc 2\.34 or later; this system has 2\.31/);
});

test('launcher: a run that started and failed gets no guidance', { skip: needsSh }, (t) => {
  const pkg = tempPackage(t);
  installStub(pkg, 'prints-version');
  const r = launch(pkg, ['run', 'task'], { STUB_STATUS: '1', FAKE_GLIBC: '2.31' },
    process.platform === 'linux' ? [FAKE_GLIBC] : []);
  assert.equal(r.status, 1);
  assert.equal(r.stderr, '');
});

test('launcher: status 127 with no output is explained', { skip: needsSh }, (t) => {
  const pkg = tempPackage(t);
  installStub(pkg, 'exit-127');
  const r = launch(pkg, ['--version']);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /did not start: it exited with status 127/);
  assert.match(r.stderr, /Docs: /);
});

function writeMarker(pkg, marker) {
  fs.mkdirSync(path.dirname(pkg.marker), { recursive: true });
  fs.writeFileSync(pkg.marker, JSON.stringify(marker));
}

test('launcher: a verified install that vanished is not downloaded again', (t) => {
  const pkg = tempPackage(t);
  writeMarker(pkg, { version: VERSION, sha256: '12'.repeat(32) });
  // BWN_SKIP_INSTALL keeps a regression off the network; the assertions on
  // stdout show whether bootstrap ran at all.
  const r = launch(pkg, ['version'], { BWN_ALLOW_BOOTSTRAP: '1', BWN_SKIP_INSTALL: '1' });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /removed after it was downloaded and verified/);
  assert.match(r.stderr, new RegExp(`SHA-256: ${'12'.repeat(32)}`));
  assert.ok(r.stderr.includes(diag.IT_GUIDE_URL));
  assert.doesNotMatch(r.stdout, /downloading|not available yet/);
});

test('launcher: --bootstrap still downloads again after a vanished install', (t) => {
  const pkg = tempPackage(t);
  writeMarker(pkg, { version: VERSION, sha256: '12'.repeat(32) });
  const r = launch(pkg, ['--bootstrap', '--version'], { BWN_SKIP_INSTALL: '1' });
  assert.equal(r.status, 1);
  assert.match(r.stdout, /native binary not available yet/);
  assert.doesNotMatch(r.stderr, /removed after it was/);
  assert.match(r.stderr, /BWN_SKIP_INSTALL is set, so the launcher does not download it/);
  assert.doesNotMatch(r.stderr, /no terminal here/);
});

test('launcher: a marker from another version does not block the download', (t) => {
  const pkg = tempPackage(t);
  writeMarker(pkg, { version: '0.0.1', sha256: '12'.repeat(32) });
  const r = launch(pkg, ['version'], { BWN_ALLOW_BOOTSTRAP: '1', BWN_SKIP_INSTALL: '1' });
  assert.match(r.stdout, /native binary not available yet/);
});

test('launcher without a terminal on old glibc or musl explains the platform, not --bootstrap', {
  skip: process.platform !== 'linux' && 'fakes the Linux glibc report',
}, (t) => {
  const pkg = tempPackage(t);
  for (const [fake, said] of [
    ['2.31', /needs glibc 2\.34 or later; this system has 2\.31/],
    ['musl', /musl libc \(Alpine or similar\)/],
  ]) {
    const r = launch(pkg, ['version'], { FAKE_GLIBC: fake }, [FAKE_GLIBC]);
    assert.equal(r.status, 1, fake);
    assert.match(r.stderr, said);
    // It would download a binary that cannot run here.
    assert.doesNotMatch(r.stderr, /--bootstrap|BWN_ALLOW_BOOTSTRAP|not found/, fake);
  }
});

test('launcher without a terminal on a supported system offers --bootstrap', (t) => {
  const pkg = tempPackage(t);
  const linux = process.platform === 'linux';
  const r = launch(pkg, ['version'], linux ? { FAKE_GLIBC: '2.39' } : {}, linux ? [FAKE_GLIBC] : []);
  assert.equal(r.status, 1);
  if (!ASSET) return;
  assert.match(r.stderr, /no terminal here[\s\S]*bwn --bootstrap/);
  assert.equal(r.stdout, '');
});

// ── first-run download, against test-fixtures/fake-release.js ───────────────

test('first run: a verified download that runs is "ready" and then runs', { skip: needsSh || noRelease }, (t) => {
  const pkg = tempPackage(t);
  const asset = path.join(STUBS, 'prints-version');
  const r = launchFake(pkg, ['version'], { asset, env: { STUB_VERSION: VERSION } });
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /installed prebuilt binary \(sha256 verified\)[\s\S]*is ready/);
  // Piped, not a terminal: no colour codes in the text.
  assert.doesNotMatch(r.stdout, /\x1b/);
  assert.match(r.stdout, /By default it asks before each change/);
  assert.ok(r.stdout.endsWith(`buildwithnexus ${VERSION}\n`), r.stdout);
  assert.deepEqual(r.requests.map((u) => u.split('/').pop()), [ASSET]);
  assert.equal(JSON.parse(fs.readFileSync(pkg.marker, 'utf8')).sha256, sha256(asset));
  assert.equal(sha256(pkg.bin), sha256(asset));
});

// npm deletes and recreates the package folder on every update or reinstall,
// so a binary kept in it was lost each time (and downloaded again).
test('upgrade: the binary survives npm replacing the package, and an upgrade replaces the old version\'s', {
  skip: needsSh || noRelease,
}, (t) => {
  const home = tempHome(t);
  const asset = path.join(STUBS, 'prints-version');
  const first = tempPackage(t, { home });
  const r1 = launchFake(first, ['version'], { asset, env: { STUB_VERSION: VERSION } });
  assert.equal(r1.status, 0, r1.stderr);
  assert.equal(r1.requests.length, 1);
  assert.ok(fs.existsSync(first.bin));
  assert.ok(!fs.existsSync(first.legacyBin));

  // Same version again in a fresh package folder: no download.
  const again = tempPackage(t, { home });
  const r2 = launchFake(again, ['version'], { asset, env: { STUB_VERSION: VERSION } });
  assert.equal(r2.status, 0, r2.stderr);
  assert.deepEqual(r2.requests, []);
  assert.equal(r2.stdout, `buildwithnexus ${VERSION}\n`);

  // A new version downloads its own binary, and the old version's is removed
  // once the new one runs (they are ~6 MB each and nothing runs them again).
  const next = tempPackage(t, { home, version: '99.0.0' });
  const r3 = launchFake(next, ['version'], { asset, env: { STUB_VERSION: '99.0.0' } });
  assert.equal(r3.status, 0, r3.stderr);
  assert.equal(r3.requests.length, 1);
  assert.match(r3.requests[0], /\/download\/v99\.0\.0\//);
  assert.ok(r3.stdout.endsWith('buildwithnexus 99.0.0\n'), r3.stdout);
  assert.notEqual(next.bin, first.bin);
  assert.ok(!fs.existsSync(path.dirname(first.bin)));
  assert.ok(fs.existsSync(next.bin));
  assert.equal(JSON.parse(fs.readFileSync(next.marker, 'utf8')).version, '99.0.0');
});

test('first run: other versions\' bin/<version> directories are removed, nothing else is', {
  skip: needsSh || noRelease,
}, (t) => {
  const home = tempHome(t);
  const binDir = path.join(home, 'bin');
  for (const v of ['0.14.0', '0.15.0', '1.0.0-rc.1']) {
    fs.mkdirSync(path.join(binDir, v), { recursive: true });
    fs.writeFileSync(path.join(binDir, v, 'buildwithnexus'), 'old');
  }
  // Not a version directory, a file named like a version, and files elsewhere
  // in the home: all stay.
  fs.mkdirSync(path.join(binDir, 'tools'));
  fs.writeFileSync(path.join(binDir, 'tools', 'keep'), 'x');
  fs.writeFileSync(path.join(binDir, '0.13.0'), 'a file');
  fs.writeFileSync(path.join(home, 'settings.json'), '{}');
  const outside = fs.mkdtempSync(path.join(os.tmpdir(), 'bwn-outside-'));
  t.after(() => fs.rmSync(outside, { recursive: true, force: true }));
  fs.writeFileSync(path.join(outside, 'keep'), 'x');
  try { fs.symlinkSync(outside, path.join(binDir, '0.12.0'), 'dir'); } catch {}

  const pkg = tempPackage(t, { home });
  const r = launchFake(pkg, ['version'], {
    asset: path.join(STUBS, 'prints-version'), env: { STUB_VERSION: VERSION },
  });
  assert.equal(r.status, 0, r.stderr);
  assert.ok(fs.existsSync(pkg.bin));
  for (const v of ['0.14.0', '0.15.0', '1.0.0-rc.1']) assert.ok(!fs.existsSync(path.join(binDir, v)), v);
  assert.ok(fs.existsSync(path.join(binDir, 'tools', 'keep')));
  assert.equal(fs.readFileSync(path.join(binDir, '0.13.0'), 'utf8'), 'a file');
  assert.ok(fs.existsSync(path.join(home, 'settings.json')));
  assert.ok(fs.existsSync(path.join(outside, 'keep')), 'a symlink must not be followed');
});

test('first run: a failed download removes no other version', { skip: noRelease }, (t) => {
  const pkg = tempPackage(t);
  const old = path.join(pkg.home, 'bin', '0.14.0');
  fs.mkdirSync(old, { recursive: true });
  const r = launchFake(pkg, ['version'], {
    asset: path.join(STUBS, 'prints-version'), pin: '0'.repeat(64),
  });
  assert.equal(r.status, 1);
  assert.ok(fs.existsSync(old));
});

test('pruneOldVersions never removes the version it is told to keep, and ignores a missing bin folder', (t) => {
  const { pruneOldVersions } = require('./resolve-binary.js');
  const home = tempHome(t);
  const saved = process.env.NEXUS_HOME;
  t.after(() => { if (saved === undefined) delete process.env.NEXUS_HOME; else process.env.NEXUS_HOME = saved; });
  process.env.NEXUS_HOME = home;
  assert.deepEqual(pruneOldVersions('1.2.3'), []);
  for (const v of ['1.2.3', '1.2.2']) fs.mkdirSync(path.join(home, 'bin', v), { recursive: true });
  assert.deepEqual(pruneOldVersions('1.2.3'), ['1.2.2']);
  assert.ok(fs.existsSync(path.join(home, 'bin', '1.2.3')));
});

test('--version / -v before the binary is downloaded answer from package.json, without downloading', (t) => {
  const pkg = tempPackage(t);
  for (const flag of ['--version', '-v']) {
    // BWN_ALLOW_BOOTSTRAP would download for any other command.
    const r = launchFake(pkg, [flag], { asset: path.join(STUBS, 'prints-version') });
    assert.equal(r.status, 0, r.stderr);
    assert.equal(r.stdout, `buildwithnexus ${VERSION}\n`);
    assert.equal(r.stderr, '');
    assert.deepEqual(r.requests, []);
    assert.ok(!fs.existsSync(pkg.bin));
  }
});

test('--version with other arguments is left to the binary', (t) => {
  const pkg = tempPackage(t);
  const r = launch(pkg, ['run', '--version']);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /native binary not found|no terminal here/);
});

test('the launcher no longer looks for per-platform npm packages or suggests them', (t) => {
  const pkg = tempPackage(t);
  // A package planted where the old lookup would have found it.
  const name = `buildwithnexus-${process.platform}-${process.arch}`;
  const planted = path.join(pkg.root, 'node_modules', name, 'bin');
  fs.mkdirSync(planted, { recursive: true });
  fs.writeFileSync(path.join(planted, 'buildwithnexus' + (WIN ? '.exe' : '')), '#!/bin/sh\necho planted\n', { mode: 0o755 });
  const r = launch(pkg, ['run', 'x']);
  assert.equal(r.status, 1);
  assert.doesNotMatch(r.stdout, /planted/);
  assert.doesNotMatch(r.stderr, /omit=optional|buildwithnexus-(linux|darwin|win32)-|package is available/);
  const resolve = require('./resolve-binary.js');
  assert.equal(resolve.packagedBinary, undefined);
  assert.equal(resolve.platformPackage, undefined);
});

test('without NEXUS_HOME the binary goes under ~/.buildwithnexus/bin/<version>', {
  skip: needsSh || noRelease,
}, (t) => {
  const pkg = tempPackage(t);
  const user = tempHome(t);
  const r = launchFake(pkg, ['version'], {
    asset: path.join(STUBS, 'prints-version'),
    env: { NEXUS_HOME: undefined, HOME: user, USERPROFILE: user, STUB_VERSION: VERSION },
  });
  assert.equal(r.status, 0, r.stderr);
  assert.ok(fs.existsSync(path.join(user, '.buildwithnexus', 'bin', VERSION, path.basename(pkg.bin))));
  assert.ok(!fs.existsSync(pkg.bin));
});

test('a binary downloaded into the package by an earlier release still runs', { skip: needsSh }, (t) => {
  const pkg = tempPackage(t);
  installStub(pkg, 'prints-version', 0o755, pkg.legacyBin);
  const r = launch(pkg, ['version'], { STUB_VERSION: VERSION });
  assert.equal(r.status, 0, r.stderr);
  assert.equal(r.stdout, `buildwithnexus ${VERSION}\n`);
});

test('BWN_INSTALL_IN_PACKAGE=1 downloads into the package, as before 0.15', { skip: needsSh || noRelease }, (t) => {
  const pkg = tempPackage(t);
  const asset = path.join(STUBS, 'prints-version');
  const r = launchFake(pkg, ['version'], { asset, env: { STUB_VERSION: VERSION, BWN_INSTALL_IN_PACKAGE: '1' } });
  assert.equal(r.status, 0, r.stderr);
  assert.equal(sha256(pkg.legacyBin), sha256(asset));
  assert.equal(JSON.parse(fs.readFileSync(pkg.legacyMarker, 'utf8')).version, VERSION);
  assert.ok(!fs.existsSync(path.dirname(pkg.bin)));
});

// Endpoint protection that quarantines on write deletes the download as soon
// as it is written, before it is moved into place, or right after.
for (const when of ['on-close', 'before-rename', 'after-rename']) {
  test(`first run: a verified download quarantined ${when} is explained, then not fetched again`, {
    skip: noRelease,
  }, (t) => {
    const pkg = tempPackage(t);
    const asset = path.join(STUBS, 'prints-version');
    const hash = sha256(asset);
    const first = launchFake(pkg, ['version'], { asset, env: { FAKE_QUARANTINE: when } });
    assert.equal(first.status, 1, first.stdout + first.stderr);
    assert.match(first.stderr, /removed after it was downloaded and verified/);
    assert.match(first.stderr, /blocked or\s+(quarantined|removed) by/);
    assert.match(first.stderr, new RegExp(`SHA-256: ${hash}`));
    assert.ok(first.stderr.includes(diag.IT_GUIDE_URL));
    assert.doesNotMatch(first.stdout + first.stderr,
      /prebuilt unavailable|could not download|not found|no terminal here|is ready|build from source/i);
    assert.equal(first.requests.length, 1);
    assert.deepEqual(JSON.parse(fs.readFileSync(pkg.marker, 'utf8')), { version: VERSION, sha256: hash });
    assert.ok(!fs.existsSync(`${pkg.bin}.download`));

    const again = launchFake(pkg, ['version'], { asset });
    assert.equal(again.status, 1);
    assert.match(again.stderr, /removed after it was downloaded and verified/);
    assert.match(again.stderr, new RegExp(`SHA-256: ${hash}`));
    assert.deepEqual(again.requests, []);
    assert.doesNotMatch(again.stdout, /downloading/);
  });
}

test('first run: a download with the wrong checksum is refused and not recorded', { skip: noRelease }, (t) => {
  const pkg = tempPackage(t);
  const r = launchFake(pkg, ['version'], { asset: path.join(STUBS, 'prints-version'), pin: '0'.repeat(64) });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /could not download the prebuilt binary: checksum mismatch/);
  assert.doesNotMatch(r.stderr, /removed after|no terminal here|proxy/);
  assert.ok(!fs.existsSync(pkg.marker));
  assert.ok(!fs.existsSync(pkg.bin));
  assert.ok(!fs.existsSync(`${pkg.bin}.download`));
});

test('first run: a failed download says why and how proxies work, not "no terminal"', { skip: noRelease }, async (t) => {
  const pkg = tempPackage(t);
  const port = await proxyFixture.closedPort();
  const cases = [
    [['version'], {}, /connect ECONNREFUSED 140/,
      // The retry line is in the user's own shell: PowerShell on Windows.
      WIN
        ? /If this network needs a proxy, set HTTPS_PROXY to its http:\/\/ URL[\s\S]*\$env:HTTPS_PROXY = 'http:\/\/<proxy>:<port>'; bwn --bootstrap/
        : /If this network needs a proxy, set HTTPS_PROXY to its http:\/\/ URL[\s\S]*HTTPS_PROXY=http:\/\/<proxy>:<port> bwn --bootstrap/],
    // The launcher's own tunnel: the error names the proxy it could not reach.
    [['--bootstrap', '--version'], { BWN_ALLOW_BOOTSTRAP: undefined, HTTPS_PROXY: `http://127.0.0.1:${port}` },
      new RegExp(`proxy 127\\.0\\.0\\.1:${port} \\(HTTPS_PROXY\\): connect ECONNREFUSED`),
      new RegExp(`It went through the proxy in HTTPS_PROXY \\(127\\.0\\.0\\.1:${port}\\)`)],
    [['version'], { HTTPS_PROXY: `http://127.0.0.1:${port}`, NO_PROXY: '.github.com,githubusercontent.com' },
      /connect ECONNREFUSED 140/, /HTTPS_PROXY is set, but NO_PROXY matches github\.com, so the launcher connected directly/],
  ];
  if (envProxySupported()) {
    cases.push([['version'], { HTTPS_PROXY: `http://127.0.0.1:${port}`, NODE_USE_ENV_PROXY: '1' },
      /connect ECONNREFUSED 140/, /Node sent it through the proxy in HTTPS_PROXY/]);
  }
  for (const [how, env, why, said] of cases) {
    const r = launchFake(pkg, how, { env: { FAKE_NET_ERROR: 'ECONNREFUSED', NO_PROXY: undefined, no_proxy: undefined, ...env } });
    assert.equal(r.status, 1);
    assert.match(r.stderr, /could not download the prebuilt binary: /);
    assert.match(r.stderr, why);
    assert.match(r.stderr, said);
    // Every Node from 18 on can download through a proxy now.
    assert.doesNotMatch(r.stderr, /cannot|only with NODE_USE_ENV_PROXY|ignores it/);
    assert.match(r.stderr, /To try again: +bwn --bootstrap/);
    assert.doesNotMatch(r.stderr, /no terminal here|native binary not found/);
    assert.ok(!fs.existsSync(pkg.marker));
  }
});

test('first run: a download that gets no answer times out instead of hanging', { skip: noRelease }, (t) => {
  const pkg = tempPackage(t);
  // The fake cuts the launcher's 30 s timer to 300 ms. The request's own
  // timeout option alone would fire after 60 s here, past the test's limit.
  const r = launchFake(pkg, ['version'], { env: { FAKE_HANG: '300' } });
  assert.equal(r.status, 1, r.stderr);
  assert.match(r.stderr, /no response from github\.com for 30 s \(ETIMEDOUT\)/);
  assert.match(r.stderr, /If this network needs a proxy, set HTTPS_PROXY/);
});

test('launcher (Windows): an exe denied execute gets security guidance', { skip: !WIN && 'Windows only' }, (t) => {
  const pkg = tempPackage(t);
  fs.mkdirSync(path.dirname(pkg.bin), { recursive: true });
  fs.copyFileSync(process.execPath, pkg.bin);
  // Deny execute to Everyone (by SID, so it works in any locale); read stays
  // allowed so the hash can still be shown.
  const deny = spawnSync('icacls', [pkg.bin, '/deny', '*S-1-1-0:(X)'], { encoding: 'utf8' });
  assert.equal(deny.status, 0, deny.stdout + deny.stderr);
  t.after(() => spawnSync('icacls', [pkg.bin, '/remove:d', '*S-1-1-0']));
  const r = launch(pkg, ['--version']);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /Windows refused to start the binary \((EPERM|EACCES)\)/);
  assert.match(r.stderr, /SHA-256: [0-9a-f]{64}/);
  assert.ok(r.stderr.includes(diag.IT_GUIDE_URL));
});

// ── signals, fail-closed checksums, unsupported platforms ───────────────────

test('launcher: SIGTERM to the launcher reaches the binary', { skip: WIN && 'POSIX signals' }, async (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'bwn-sig-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const got = path.join(dir, 'got-sigterm');
  const { spawn } = require('node:child_process');
  // Node stands in for the binary: it says when it is listening, and records
  // the SIGTERM before exiting 7.
  const script = `process.on('SIGTERM', () => { require('fs').writeFileSync(${JSON.stringify(got)}, 'x'); process.exit(7); });` +
    "process.stdout.write('ready\\n'); setInterval(() => {}, 1000);";
  const child = spawn(process.execPath, [launcher, '-e', script], {
    env: { ...process.env, BWN_BIN: process.execPath }, stdio: ['ignore', 'pipe', 'inherit'],
  });
  await new Promise((resolve) => child.stdout.on('data', (d) => String(d).includes('ready') && resolve()));
  child.kill('SIGTERM');
  const status = await new Promise((resolve) => child.on('exit', (code) => resolve(code)));
  assert.equal(status, 7);
  assert.ok(fs.existsSync(got), 'the binary did not get SIGTERM');
});

test('launcher: a binary killed by a signal makes the launcher die the same way', { skip: WIN && 'POSIX signals' }, () => {
  const result = spawnSync(process.execPath, [launcher, '-e', "process.kill(process.pid, 'SIGKILL')"], {
    encoding: 'utf8',
    env: { ...process.env, BWN_BIN: process.execPath },
  });
  assert.equal(result.signal, 'SIGKILL');
  assert.equal(result.stderr, '');
});

test('first run: a checksums.json without this asset refuses to download', { skip: needsSh || noRelease }, (t) => {
  const pkg = tempPackage(t);
  fs.writeFileSync(path.join(pkg.root, 'checksums.json'), JSON.stringify({ 'buildwithnexus-other': 'a'.repeat(64) }));
  const r = launchFake(pkg, ['version'], { env: { FAKE_ASSET: path.join(STUBS, 'prints-version') } });
  assert.equal(r.status, 1);
  assert.match(r.stderr, new RegExp(`checksums\\.json in this package has no entry for ${ASSET}`));
  // Not even the release's own .sha256 is asked for.
  assert.deepEqual(r.requests, []);
  assert.ok(!fs.existsSync(pkg.bin));
});

test('first run: musl is explained before anything is downloaded', {
  skip: (process.platform !== 'linux' || noRelease) && 'Linux only',
}, (t) => {
  const pkg = tempPackage(t);
  const r = launchFake(pkg, ['version'], {
    asset: path.join(STUBS, 'prints-version'),
    // Both fakes, in the launcher and in the bootstrap it starts.
    env: { FAKE_GLIBC: 'musl', NODE_OPTIONS: `--require=${JSON.stringify(FAKE_RELEASE)} --require=${JSON.stringify(FAKE_GLIBC)}` },
  });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /musl/i);
  assert.deepEqual(r.requests, []);
});

// ── proxy: the launcher's own CONNECT tunnel (test-fixtures/connect-proxy.js) ─

test('NO_PROXY matches the way curl and npm read it', () => {
  for (const [host, port, list, want] of [
    ['github.com', 443, '*', true],
    ['github.com', 443, 'github.com', true],
    ['api.github.com', 443, 'github.com', true],
    ['api.github.com', 443, '.github.com', true],
    ['github.com', 443, '.github.com', true],
    ['api.github.com', 443, '*.github.com', true],
    ['objects.githubusercontent.com', 443, 'githubusercontent.com', true],
    ['notgithub.com', 443, 'github.com', false],
    ['github.com.evil.example', 443, 'github.com', false],
    ['github.com', 443, 'GitHub.COM', true],
    ['github.com.', 443, 'github.com', true],
    ['github.com', 443, 'github.com:443', true],
    ['github.com', 443, 'github.com:8443', false],
    ['github.com', 443, 'localhost, 127.0.0.1 ,github.com', true],
    ['github.com', 443, 'localhost 127.0.0.1', false],
    ['github.com', 443, '', false],
    ['github.com', 443, ',, ,', false],
    ['github.com', 443, '.', false],
    ['10.1.2.3', 443, '10.1.2.3', true],
    ['10.1.2.3', 443, '2.3', false],
    ['::1', 443, '[::1]:443', true],
    ['[::1]', 443, '::1', true],
    ['::1', 443, '[::1]:80', false],
  ]) {
    assert.equal(noProxyMatch(host, port, list), want, `${host}:${port} against ${JSON.stringify(list)}`);
  }
});

test('proxyRoute: which variable, which proxy, and what is refused', () => {
  const route = (env) => proxyRoute('github.com', 443, env);
  assert.deepEqual(route({}), { proxy: null });
  assert.equal(route({ HTTPS_PROXY: 'http://a:1', https_proxy: 'http://b:2', HTTP_PROXY: 'http://c:3' }).display, 'a:1');
  assert.equal(route({ HTTPS_PROXY: '', https_proxy: 'http://b:2' }).var, 'https_proxy');
  assert.equal(route({ HTTP_PROXY: 'http://c:3' }).var, 'HTTP_PROXY');
  assert.equal(route({ http_proxy: 'c' }).display, 'c:80');
  // curl and npm take a bare host:port as http://.
  assert.deepEqual(route({ HTTPS_PROXY: 'proxy.corp:3128' }).proxy, { hostname: 'proxy.corp', port: 3128, auth: null });
  assert.equal(route({ HTTPS_PROXY: 'http://[::1]:3128' }).display, '[::1]:3128');
  // Percent-encoded credentials are decoded for the header, never displayed.
  const withAuth = route({ HTTPS_PROXY: 'http://me%40corp:p%3Ass%2Fw@proxy.corp:8080/' });
  assert.equal(withAuth.proxy.auth, 'me@corp:p:ss/w');
  assert.equal(withAuth.display, 'proxy.corp:8080');
  assert.equal(proxyAuthorization(withAuth.proxy.auth), `Basic ${Buffer.from('me@corp:p:ss/w').toString('base64')}`);
  assert.deepEqual(route({ HTTPS_PROXY: 'http://a:1', no_proxy: 'github.com' }),
    { proxy: null, var: 'HTTPS_PROXY', bypass: 'no_proxy' });
  for (const [value, said] of [
    ['https://user:secret@proxy.corp:443', /HTTPS_PROXY is a https:\/\/ URL \(proxy\.corp\), but the launcher only connects to http:\/\/ proxies/],
    ['socks5://user:secret@proxy.corp:1080', /HTTPS_PROXY is a socks5:\/\/ URL/],
    ['http://user:secret@', /HTTPS_PROXY is not a valid proxy URL/],
  ]) {
    assert.throws(() => route({ HTTPS_PROXY: value }), (e) => {
      assert.equal(e.code, 'BWN_PROXY_URL');
      assert.match(e.message, said);
      assert.doesNotMatch(e.message, /secret|user/);
      return true;
    });
  }
});

test('Node\'s own proxy support is used only where it exists, so nothing is proxied twice', () => {
  for (const [env, version, want] of [
    [{ NODE_USE_ENV_PROXY: '1' }, '22.21.0', true],
    [{ NODE_USE_ENV_PROXY: '1' }, '24.5.0', true],
    [{ NODE_USE_ENV_PROXY: '1' }, '26.0.0', true],
    [{ NODE_OPTIONS: '--use-env-proxy' }, '24.5.0', true],
    [{ NODE_USE_ENV_PROXY: '1' }, '22.20.0', false],
    [{ NODE_USE_ENV_PROXY: '1' }, '24.4.0', false],
    [{ NODE_USE_ENV_PROXY: '1' }, '20.19.0', false],
    [{ NODE_USE_ENV_PROXY: '1' }, '18.20.8', false],
    [{ NODE_USE_ENV_PROXY: '0' }, '24.5.0', false],
    [{}, '24.5.0', false],
  ]) {
    assert.equal(nodeHandlesProxy(env, version), want, `${JSON.stringify(env)} on ${version}`);
  }
  const env = { HTTPS_PROXY: 'http://proxy.corp:8080', NODE_USE_ENV_PROXY: '1' };
  assert.deepEqual(proxyRoute('github.com', 443, env), envProxySupported() ? { proxy: null, node: true }
    : { var: 'HTTPS_PROXY', proxy: { hostname: 'proxy.corp', port: 8080, auth: null }, display: 'proxy.corp:8080' });
  // The hint never says a Node cannot use a proxy.
  for (const e of [{}, { HTTPS_PROXY: 'http://u:pw@proxy.corp:8080' }, env, { HTTPS_PROXY: 'x', NO_PROXY: '*' }]) {
    for (const win of [false, true]) {
      const text = proxyHint(win, e).join('\n');
      assert.doesNotMatch(text, /cannot|ignores it|Only Node/);
      assert.doesNotMatch(text, /pw/);
    }
  }
  assert.match(proxyHint(true, {}).join('\n'), /\$env:HTTPS_PROXY = 'http:\/\/<proxy>:<port>'; bwn --bootstrap/);
});

// A plain TCP server that answers "hello" through the tunnel.
async function helloServer(t) {
  const net = require('node:net');
  const server = net.createServer((s) => s.end('hello'));
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  return server.address().port;
}

async function proxyFor(t, opts) {
  const proxy = await proxyFixture.startProxy(opts);
  t.after(() => proxy.close());
  return proxy;
}

test('openTunnel: a 200 hands back the tunnel, with Basic credentials sent', async (t) => {
  const upstream = await helloServer(t);
  const proxy = await proxyFor(t, { upstream: ['127.0.0.1', upstream], user: 'me@corp', password: 'p:ss' });
  const route = proxyRoute('github.com', 443, { HTTPS_PROXY: `http://me%40corp:p%3Ass@127.0.0.1:${proxy.port}` });
  let active = 0;
  const { socket } = openTunnel(route, 'github.com', 443, () => active++);
  const sock = await socket;
  const got = await new Promise((resolve) => {
    let s = '';
    sock.on('data', (c) => (s += c));
    sock.on('end', () => resolve(s));
  });
  assert.equal(got, 'hello');
  assert.ok(active >= 1, 'the CONNECT answer counts as activity');
  assert.deepEqual(proxy.log, [{
    target: 'github.com:443',
    auth: `Basic ${Buffer.from('me@corp:p:ss').toString('base64')}`,
    userAgent: 'buildwithnexus-installer',
  }]);
});

test('openTunnel: 407 says the proxy needs credentials; another status names the proxy and status', async (t) => {
  const upstream = await helloServer(t);
  const authProxy = await proxyFor(t, { upstream: ['127.0.0.1', upstream], user: 'u', password: 'right' });
  const refusing = await proxyFor(t, { upstream: ['127.0.0.1', upstream], status: 403 });
  for (const [url, code, said] of [
    [`http://127.0.0.1:${authProxy.port}`, 'BWN_PROXY_AUTH',
      `proxy 127.0.0.1:${authProxy.port} (HTTPS_PROXY) needs credentials (HTTP 407 Proxy Authentication Required)`],
    [`http://u:wrong-secret@127.0.0.1:${authProxy.port}`, 'BWN_PROXY_AUTH',
      `proxy 127.0.0.1:${authProxy.port} (HTTPS_PROXY) rejected the credentials in HTTPS_PROXY (HTTP 407)`],
    [`http://127.0.0.1:${refusing.port}`, 'BWN_PROXY_REFUSED',
      `proxy 127.0.0.1:${refusing.port} (HTTPS_PROXY) refused the tunnel to github.com:443 (HTTP 403 Forbidden)`],
  ]) {
    const { socket } = openTunnel(proxyRoute('github.com', 443, { HTTPS_PROXY: url }), 'github.com', 443);
    await assert.rejects(socket, (e) => {
      assert.equal(e.code, code);
      assert.equal(e.message, said);
      return true;
    });
  }
});

const noOpenssl = !proxyFixture.opensslAvailable() && 'needs openssl for a throwaway certificate';

// The launcher, started without blocking this process (the proxy and the
// fake GitHub run here), with only the proxy variables a test gives it.
function launchAsync(pkg, args, env = {}, preload = []) {
  const { spawn } = require('node:child_process');
  const clean = { ...process.env };
  for (const k of ['BWN_BIN', 'BWN_SKIP_INSTALL', 'BWN_INSTALL_IN_PACKAGE', 'NODE_OPTIONS', 'NODE_USE_ENV_PROXY',
    'HTTPS_PROXY', 'https_proxy', 'HTTP_PROXY', 'http_proxy', 'NO_PROXY', 'no_proxy', 'NODE_EXTRA_CA_CERTS']) delete clean[k];
  return new Promise((resolve) => {
    const child = spawn(process.execPath,
      [...preload.flatMap((p) => ['-r', p]), path.join(pkg.root, 'bin', 'buildwithnexus.js'), ...args],
      { env: { ...clean, NEXUS_HOME: pkg.home, BWN_ALLOW_BOOTSTRAP: '1', ...env } });
    child.stdin.end();
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (c) => (stdout += c));
    child.stderr.on('data', (c) => (stderr += c));
    const killer = setTimeout(() => child.kill(), 60000);
    child.on('close', (status) => {
      clearTimeout(killer);
      resolve({ status, stdout, stderr });
    });
  });
}

async function fakeGitHub(t, opts) {
  const gh = await proxyFixture.startFakeGitHub(opts);
  t.after(() => gh.close());
  return gh;
}

function pinAsset(pkg, asset) {
  fs.writeFileSync(path.join(pkg.root, 'checksums.json'), JSON.stringify({ [ASSET]: sha256(asset) }));
}

test('first run through a proxy: tunnel per hop, TLS to GitHub, credentials sent and never shown', {
  skip: needsSh || noRelease || noOpenssl,
}, async (t) => {
  const pkg = tempPackage(t);
  const asset = path.join(STUBS, 'prints-version');
  pinAsset(pkg, asset);
  const gh = await fakeGitHub(t, { asset });
  const proxy = await proxyFor(t, { upstream: ['127.0.0.1', gh.port], user: 'me@corp', password: 'Sup3r/s3cret' });
  const r = await launchAsync(pkg, ['version'], {
    HTTPS_PROXY: `http://me%40corp:Sup3r%2Fs3cret@127.0.0.1:${proxy.port}`,
    // The throwaway certificate is trusted only this way: verification is on.
    NODE_EXTRA_CA_CERTS: gh.certFile,
    STUB_VERSION: VERSION,
  });
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /installed prebuilt binary \(sha256 verified\)[\s\S]*is ready/);
  assert.ok(r.stdout.endsWith(`buildwithnexus ${VERSION}\n`), r.stdout);
  assert.equal(sha256(pkg.bin), sha256(asset));
  // The redirect to objects.githubusercontent.com got its own tunnel.
  assert.deepEqual(proxy.log.map((l) => l.target), ['github.com:443', 'objects.githubusercontent.com:443']);
  for (const l of proxy.log) assert.equal(l.auth, `Basic ${Buffer.from('me@corp:Sup3r/s3cret').toString('base64')}`);
  assert.deepEqual(gh.requests, [
    `github.com/Garretts-Apps/buildwithnexus/releases/download/v${VERSION}/${ASSET}`,
    `objects.githubusercontent.com/release-asset/${ASSET}`,
  ]);
  assert.doesNotMatch(r.stdout + r.stderr, /s3cret|me%40corp|me@corp/);
});

test('first run through a proxy: a redirect off GitHub is refused before any tunnel to it', {
  skip: noRelease || noOpenssl,
}, async (t) => {
  const pkg = tempPackage(t);
  pinAsset(pkg, path.join(STUBS, 'prints-version'));
  const gh = await fakeGitHub(t, { redirectTo: 'https://evil.example/payload' });
  const proxy = await proxyFor(t, { upstream: ['127.0.0.1', gh.port] });
  const r = await launchAsync(pkg, ['version'], {
    HTTPS_PROXY: `http://127.0.0.1:${proxy.port}`, NODE_EXTRA_CA_CERTS: gh.certFile,
  });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /could not download the prebuilt binary: refusing non-GitHub host evil\.example/);
  assert.deepEqual(proxy.log.map((l) => l.target), ['github.com:443']);
  assert.ok(!fs.existsSync(pkg.bin));
});

test('first run through a proxy: a certificate the system does not trust is refused', {
  skip: noRelease || noOpenssl,
}, async (t) => {
  const pkg = tempPackage(t);
  pinAsset(pkg, path.join(STUBS, 'prints-version'));
  const gh = await fakeGitHub(t, {});
  const proxy = await proxyFor(t, { upstream: ['127.0.0.1', gh.port] });
  // No NODE_EXTRA_CA_CERTS: the tunnel does not loosen certificate checks.
  const r = await launchAsync(pkg, ['version'], { HTTPS_PROXY: `http://127.0.0.1:${proxy.port}` });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /DEPTH_ZERO_SELF_SIGNED_CERT|SELF_SIGNED_CERT_IN_CHAIN|self.signed/i);
  assert.match(r.stderr, /NODE_EXTRA_CA_CERTS/);
  assert.deepEqual(gh.requests, []);
});

test('first run through a proxy: 407, a refused tunnel, a stall and an https:// proxy URL are each explained', {
  skip: noRelease,
}, async (t) => {
  const pkg = tempPackage(t);
  const upstream = await helloServer(t);
  const auth = await proxyFor(t, { upstream: ['127.0.0.1', upstream], user: 'u', password: 'right' });
  const refusing = await proxyFor(t, { upstream: ['127.0.0.1', upstream], status: 502 });
  const silent = await proxyFor(t, { hang: true });
  const literal = (s) => new RegExp(s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'));
  for (const [proxyUrl, said, env = {}] of [
    [`http://127.0.0.1:${auth.port}`, [
      literal(`proxy 127.0.0.1:${auth.port} (HTTPS_PROXY) needs credentials (HTTP 407 Proxy Authentication Required).`),
      /Put the proxy user name and password in the proxy URL/,
      WIN ? /\$env:HTTPS_PROXY = 'http:\/\/<user>:<password>@<proxy>:<port>'; bwn --bootstrap/
        : /HTTPS_PROXY='http:\/\/<user>:<password>@<proxy>:<port>' bwn --bootstrap/]],
    [`http://u:Wr0ng-s3cret@127.0.0.1:${auth.port}`, [
      literal(`proxy 127.0.0.1:${auth.port} (HTTPS_PROXY) rejected the credentials in HTTPS_PROXY (HTTP 407).`)]],
    [`http://127.0.0.1:${refusing.port}`, [
      literal(`proxy 127.0.0.1:${refusing.port} (HTTPS_PROXY) refused the tunnel to github.com:443 (HTTP 502 Bad Gateway).`),
      /Ask for github\.com and objects\.githubusercontent\.com to be allowed through the proxy/]],
    // The idle timeout covers the CONNECT phase too (cut to 300 ms here).
    [`http://127.0.0.1:${silent.port}`, [
      literal(`no response from proxy 127.0.0.1:${silent.port} (HTTPS_PROXY) for 30 s (ETIMEDOUT).`),
      literal(`It went through the proxy in HTTPS_PROXY (127.0.0.1:${silent.port})`)], {
      // NODE_OPTIONS, so bootstrap.js (a child of the launcher) gets it too.
      NODE_OPTIONS: `--require=${JSON.stringify(SHORT_TIMERS)}`, SHORT_TIMERS: '300' }],
    ['https://u:Wr0ng-s3cret@127.0.0.1:1', [
      literal('HTTPS_PROXY is a https:// URL (127.0.0.1:1), but the launcher only connects to http:// proxies.'),
      /Set it to the http:\/\/ URL of the proxy/]],
  ]) {
    const r = await launchAsync(pkg, ['version'], { HTTPS_PROXY: proxyUrl, ...env });
    assert.equal(r.status, 1, r.stderr);
    for (const s of said) assert.match(r.stderr, s);
    assert.match(r.stderr, /To try again: +bwn --bootstrap/);
    assert.doesNotMatch(r.stdout + r.stderr, /s3cret|right/);
    assert.ok(!fs.existsSync(pkg.bin));
  }
  assert.equal(auth.log.length, 2);
  assert.equal(auth.log[0].auth, null);
});

test('first run with NODE_USE_ENV_PROXY=1 on a Node that supports it: Node tunnels, the launcher does not', {
  skip: (needsSh || noRelease || noOpenssl) || (!envProxySupported() && 'Node lacks NODE_USE_ENV_PROXY'),
}, async (t) => {
  const pkg = tempPackage(t);
  const asset = path.join(STUBS, 'prints-version');
  pinAsset(pkg, asset);
  const gh = await fakeGitHub(t, { asset });
  const proxy = await proxyFor(t, { upstream: ['127.0.0.1', gh.port] });
  const r = await launchAsync(pkg, ['version'], {
    HTTPS_PROXY: `http://127.0.0.1:${proxy.port}`, NODE_USE_ENV_PROXY: '1', NODE_EXTRA_CA_CERTS: gh.certFile,
    STUB_VERSION: VERSION,
  });
  assert.equal(r.status, 0, r.stderr);
  assert.deepEqual(proxy.log.map((l) => l.target), ['github.com:443', 'objects.githubusercontent.com:443']);
  // The launcher's own CONNECT carries its user agent; Node's does not.
  for (const l of proxy.log) assert.notEqual(l.userAgent, 'buildwithnexus-installer');
});
