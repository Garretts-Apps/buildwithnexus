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

// A copy of the published package layout in a temp dir, so a test can put a
// stub (or nothing) where a downloaded binary goes without touching the repo.
function tempPackage(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'bwn-launcher-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  for (const f of ['package.json', 'bin/buildwithnexus.js', 'scripts/resolve-binary.js',
    'scripts/bootstrap.js', 'scripts/diagnose.js']) {
    fs.mkdirSync(path.join(root, path.dirname(f)), { recursive: true });
    fs.copyFileSync(path.join(REPO, f), path.join(root, f));
  }
  const bin = path.join(root, 'bin', 'buildwithnexus' + (WIN ? '.exe' : ''));
  return { root, bin, marker: path.join(root, 'bin', '.installed.json') };
}

function launch(pkg, args, env = {}, preload = []) {
  const clean = { ...process.env };
  for (const k of ['BWN_BIN', 'BWN_ALLOW_BOOTSTRAP', 'BWN_SKIP_INSTALL', 'NODE_OPTIONS', 'NODE_USE_ENV_PROXY',
    'HTTPS_PROXY', 'https_proxy', 'HTTP_PROXY', 'http_proxy']) delete clean[k];
  // stdin is a pipe, not a TTY, so nothing downloads unless a test opts in.
  return spawnSync(process.execPath,
    [...preload.flatMap((p) => ['-r', p]), path.join(pkg.root, 'bin', 'buildwithnexus.js'), ...args], {
      encoding: 'utf8',
      env: { ...clean, ...env },
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

function installStub(pkg, stub, mode = 0o755) {
  fs.copyFileSync(path.join(STUBS, stub), pkg.bin);
  fs.chmodSync(pkg.bin, mode);
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

test('package.json and both crates carry the same version', () => {
  // "Ready" needs the binary's --version to equal package.json's version, so
  // a bump that misses a file would fail every first run.
  for (const toml of ['harness/Cargo.toml', 'bwn/Cargo.toml']) {
    const text = fs.readFileSync(path.join(REPO, toml), 'utf8');
    const section = text.split(/^\[/m).find((part) => part.startsWith('package]')) || '';
    const version = (section.match(/^version\s*=\s*"([^"]+)"/m) || [])[1];
    assert.equal(version, VERSION, `${toml} [package] version`);
  }
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

test('launcher: a verified install that vanished is not downloaded again', (t) => {
  const pkg = tempPackage(t);
  fs.writeFileSync(pkg.marker, JSON.stringify({ version: VERSION, sha256: '12'.repeat(32) }));
  // BWN_SKIP_INSTALL keeps a regression off the network; the assertions on
  // stdout show whether bootstrap ran at all.
  const r = launch(pkg, ['--version'], { BWN_ALLOW_BOOTSTRAP: '1', BWN_SKIP_INSTALL: '1' });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /removed after it was downloaded and verified/);
  assert.match(r.stderr, new RegExp(`SHA-256: ${'12'.repeat(32)}`));
  assert.ok(r.stderr.includes(diag.IT_GUIDE_URL));
  assert.doesNotMatch(r.stdout, /downloading|not available yet/);
});

test('launcher: --bootstrap still downloads again after a vanished install', (t) => {
  const pkg = tempPackage(t);
  fs.writeFileSync(pkg.marker, JSON.stringify({ version: VERSION, sha256: '12'.repeat(32) }));
  const r = launch(pkg, ['--bootstrap', '--version'], { BWN_SKIP_INSTALL: '1' });
  assert.equal(r.status, 1);
  assert.match(r.stdout, /native binary not available yet/);
  assert.doesNotMatch(r.stderr, /removed after it was/);
  assert.match(r.stderr, /BWN_SKIP_INSTALL is set, so the launcher does not download it/);
  assert.doesNotMatch(r.stderr, /no terminal here/);
});

test('launcher: a marker from another version does not block the download', (t) => {
  const pkg = tempPackage(t);
  fs.writeFileSync(pkg.marker, JSON.stringify({ version: '0.0.1', sha256: '12'.repeat(32) }));
  const r = launch(pkg, ['--version'], { BWN_ALLOW_BOOTSTRAP: '1', BWN_SKIP_INSTALL: '1' });
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
    const r = launch(pkg, ['--version'], { FAKE_GLIBC: fake }, [FAKE_GLIBC]);
    assert.equal(r.status, 1, fake);
    assert.match(r.stderr, said);
    // It would download a binary that cannot run here.
    assert.doesNotMatch(r.stderr, /--bootstrap|BWN_ALLOW_BOOTSTRAP|not found/, fake);
  }
});

test('launcher without a terminal on a supported system offers --bootstrap', (t) => {
  const pkg = tempPackage(t);
  const linux = process.platform === 'linux';
  const r = launch(pkg, ['--version'], linux ? { FAKE_GLIBC: '2.39' } : {}, linux ? [FAKE_GLIBC] : []);
  assert.equal(r.status, 1);
  if (!ASSET) return;
  assert.match(r.stderr, /no terminal here[\s\S]*bwn --bootstrap/);
  assert.equal(r.stdout, '');
});

// ── first-run download, against test-fixtures/fake-release.js ───────────────

test('first run: a verified download that runs is "ready" and then runs', { skip: needsSh || noRelease }, (t) => {
  const pkg = tempPackage(t);
  const asset = path.join(STUBS, 'prints-version');
  const r = launchFake(pkg, ['--version'], { asset, env: { STUB_VERSION: VERSION } });
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /installed prebuilt binary \(sha256 verified\)[\s\S]*is ready/);
  assert.ok(r.stdout.endsWith(`buildwithnexus ${VERSION}\n`), r.stdout);
  assert.deepEqual(r.requests.map((u) => u.split('/').pop()), [ASSET]);
  assert.equal(JSON.parse(fs.readFileSync(pkg.marker, 'utf8')).sha256, sha256(asset));
  assert.equal(sha256(pkg.bin), sha256(asset));
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
    const first = launchFake(pkg, ['--version'], { asset, env: { FAKE_QUARANTINE: when } });
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

    const again = launchFake(pkg, ['--version'], { asset });
    assert.equal(again.status, 1);
    assert.match(again.stderr, /removed after it was downloaded and verified/);
    assert.match(again.stderr, new RegExp(`SHA-256: ${hash}`));
    assert.deepEqual(again.requests, []);
    assert.doesNotMatch(again.stdout, /downloading/);
  });
}

test('first run: a download with the wrong checksum is refused and not recorded', { skip: noRelease }, (t) => {
  const pkg = tempPackage(t);
  const r = launchFake(pkg, ['--version'], { asset: path.join(STUBS, 'prints-version'), pin: '0'.repeat(64) });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /could not download the prebuilt binary: checksum mismatch/);
  assert.doesNotMatch(r.stderr, /removed after|no terminal here|proxy/);
  assert.ok(!fs.existsSync(pkg.marker));
  assert.ok(!fs.existsSync(pkg.bin));
  assert.ok(!fs.existsSync(`${pkg.bin}.download`));
});

test('first run: a failed download says why and how proxies work, not "no terminal"', { skip: noRelease }, (t) => {
  const pkg = tempPackage(t);
  const [major, minor] = process.versions.node.split('.').map(Number);
  const envProxy = major >= 25 || (major === 24 && minor >= 5) || (major === 22 && minor >= 21);
  for (const [how, env, said] of [
    [['--version'], {}, /If this network needs a proxy: Node's https module uses HTTPS_PROXY only with NODE_USE_ENV_PROXY=1/],
    [['--bootstrap', '--version'], { BWN_ALLOW_BOOTSTRAP: undefined, HTTPS_PROXY: 'http://proxy.example:8080' },
      /HTTPS_PROXY is set, but Node's https module ignores it unless NODE_USE_ENV_PROXY=1 is set too/],
  ]) {
    const r = launchFake(pkg, how, { env: { FAKE_NET_ERROR: 'ECONNREFUSED', ...env } });
    assert.equal(r.status, 1);
    assert.match(r.stderr, /could not download the prebuilt binary: connect ECONNREFUSED/);
    assert.match(r.stderr, said);
    assert.match(r.stderr, envProxy ? /supports it/ : /Only Node 22\.21\+ and 24\.5\+ support that/);
    assert.match(r.stderr, /To try again: +bwn --bootstrap/);
    assert.doesNotMatch(r.stderr, /no terminal here|native binary not found/);
    assert.ok(!fs.existsSync(pkg.marker));
  }
});

test('first run: a download that gets no answer times out instead of hanging', { skip: noRelease }, (t) => {
  const pkg = tempPackage(t);
  // The fake cuts the launcher's 30 s timer to 300 ms. The request's own
  // timeout option alone would fire after 60 s here, past the test's limit.
  const r = launchFake(pkg, ['--version'], { env: { FAKE_HANG: '300' } });
  assert.equal(r.status, 1, r.stderr);
  assert.match(r.stderr, /no response from github\.com for 30 s \(ETIMEDOUT\)/);
  assert.match(r.stderr, /NODE_USE_ENV_PROXY/);
});

test('launcher (Windows): an exe denied execute gets security guidance', { skip: !WIN && 'Windows only' }, (t) => {
  const pkg = tempPackage(t);
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
