'use strict';

const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');
const { formula } = require('./update-install-manifests.js');
const native = require('./native-handoff.js');

const WIN = process.platform === 'win32';
const installSh = path.join(__dirname, 'install.sh');

function sha(file) {
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}

function tmp(tag) {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'bwn-' + tag + '-'));
}

function toolPath(dir) {
  const bin = path.join(dir, 'tools');
  fs.mkdirSync(bin, { recursive: true });
  for (const name of ['sh', 'sha256sum', 'shasum', 'awk', 'mktemp', 'uname', 'cp', 'mkdir', 'chmod', 'ln', 'rm', 'cat', 'tr']) {
    if (fs.existsSync(path.join(bin, name))) continue;
    const found = ['/usr/bin', '/bin'].map((dir) => path.join(dir, name)).find((p) => fs.existsSync(p));
    if (found) fs.symlinkSync(found, path.join(bin, name));
  }
  return bin;
}

function posixEnv(dir, extra) {
  return Object.assign({
    PATH: toolPath(dir),
    HOME: dir,
    TMPDIR: dir,
    GH_LOG: path.join(dir, 'gh.log'),
  }, extra);
}

function writeGh(dir, mode) {
  const bin = path.join(dir, 'fake-bin');
  fs.mkdirSync(bin, { recursive: true });
  const gh = path.join(bin, 'gh');
  fs.writeFileSync(gh, `#!/bin/sh
printf '%s\\n' "$*" >> "$GH_LOG"
mode="$GH_MODE"
if [ "$mode" = unauth ]; then
  if [ "$1" = auth ] && [ "$2" = status ]; then
    echo "You are not logged into any GitHub hosts." >&2
    exit 1
  fi
  echo "unexpected: $*" >&2
  exit 99
fi
if [ "$mode" = old ]; then
  if [ "$1" = auth ] && [ "$2" = status ]; then exit 0; fi
  echo 'unknown command "attestation" for "gh"' >&2
  exit 1
fi
if [ "$mode" = offline ]; then
  if [ "$1" = auth ] && [ "$2" = status ]; then exit 0; fi
  if [ "$1" = attestation ] && [ "$3" = --help ]; then exit 0; fi
  echo "error creating Sigstore verifier: no valid Sigstore verifiers" >&2
  exit 1
fi
if [ "$mode" = reject ]; then
  if [ "$1" = auth ] && [ "$2" = status ]; then exit 0; fi
  if [ "$1" = attestation ] && [ "$3" = --help ]; then exit 0; fi
  echo "Error: HTTP 404: Not Found (https://api.github.com/repos/Garretts-Apps/buildwithnexus/attestations/sha256:dead)" >&2
  exit 1
fi
echo "unset GH_MODE: $*" >&2
exit 99
`);
  fs.chmodSync(gh, 0o755);
  return bin;
}

function installStub(dir) {
  const stub = path.join(dir, 'stub');
  fs.writeFileSync(stub, '#!/bin/sh\necho buildwithnexus test-stub\n');
  fs.chmodSync(stub, 0o755);
  return stub;
}

test('install.sh verifies the digest, installs, and uninstalls', { skip: WIN && 'POSIX installer' }, () => {
  const dir = tmp('inst');
  const stub = path.join(dir, 'stub');
  fs.writeFileSync(stub, '#!/bin/sh\necho buildwithnexus test-stub\n');
  fs.chmodSync(stub, 0o755);
  const prefix = path.join(dir, 'bin');
  const sum = sha(stub);
  const ok = spawnSync('sh', [installSh, '--file', stub, '--sha256', sum, '--prefix', prefix, '--no-attest'], { encoding: 'utf8' });
  assert.equal(ok.status, 0, ok.stderr + ok.stdout);
  assert.match(ok.stdout, /installed /);
  const listed = fs.readlinkSync(path.join(prefix, 'buildwithnexus'));
  assert.equal(listed, 'bwn');
  const bad = spawnSync('sh', [installSh, '--file', stub, '--sha256', '0'.repeat(64), '--prefix', prefix, '--no-attest'], { encoding: 'utf8' });
  assert.notEqual(bad.status, 0);
  assert.match(bad.stderr, /checksum mismatch/);
  assert.equal(fs.existsSync(path.join(prefix, 'bwn')), true);
  const gone = spawnSync('sh', [installSh, '--uninstall', '--prefix', prefix], { encoding: 'utf8' });
  assert.equal(gone.status, 0, gone.stderr);
  assert.equal(fs.existsSync(path.join(prefix, 'bwn')), false);
  assert.equal(fs.existsSync(path.join(prefix, 'buildwithnexus')), false);
});

test('install.sh still installs when gh is absent', { skip: WIN && 'POSIX installer' }, () => {
  const dir = tmp('no-gh');
  const stub = installStub(dir);
  const prefix = path.join(dir, 'bin');
  const ran = spawnSync('sh', [installSh, '--file', stub, '--sha256', sha(stub), '--prefix', prefix], {
    encoding: 'utf8',
    env: posixEnv(dir),
  });
  assert.equal(ran.status, 0, ran.stderr + ran.stdout);
  assert.match(ran.stderr, /gh is not installed/);
  assert.match(ran.stdout, /installed /);
  assert.equal(fs.existsSync(path.join(prefix, 'bwn')), true);
});

test('install.sh still installs when gh is not logged in', { skip: WIN && 'POSIX installer' }, () => {
  const dir = tmp('unauth');
  const stub = installStub(dir);
  const prefix = path.join(dir, 'bin');
  const fake = writeGh(dir, 'unauth');
  const ran = spawnSync('sh', [installSh, '--file', stub, '--sha256', sha(stub), '--prefix', prefix], {
    encoding: 'utf8',
    env: posixEnv(dir, { PATH: `${fake}:${toolPath(dir)}`, GH_MODE: 'unauth' }),
  });
  assert.equal(ran.status, 0, ran.stderr + ran.stdout);
  assert.match(ran.stderr, /not logged in/);
  assert.equal(fs.existsSync(path.join(prefix, 'bwn')), true);
  const log = fs.readFileSync(path.join(dir, 'gh.log'), 'utf8');
  assert.match(log, /^auth status$/m);
  assert.doesNotMatch(log, /attestation verify/);
});

test('install.sh refuses a checksum mismatch before it installs', { skip: WIN && 'POSIX installer' }, () => {
  const dir = tmp('badsum');
  const stub = installStub(dir);
  const prefix = path.join(dir, 'bin');
  const ran = spawnSync('sh', [installSh, '--file', stub, '--sha256', 'f'.repeat(64), '--prefix', prefix], {
    encoding: 'utf8',
    env: posixEnv(dir),
  });
  assert.notEqual(ran.status, 0);
  assert.match(ran.stderr, /checksum mismatch/);
  assert.equal(fs.existsSync(path.join(prefix, 'bwn')), false);
});

test('install.sh fails when attestation runs and rejects the file', { skip: WIN && 'POSIX installer' }, () => {
  const dir = tmp('reject');
  const stub = installStub(dir);
  const prefix = path.join(dir, 'bin');
  const fake = writeGh(dir, 'reject');
  const ran = spawnSync('sh', [installSh, '--file', stub, '--sha256', sha(stub), '--prefix', prefix], {
    encoding: 'utf8',
    env: posixEnv(dir, { PATH: `${fake}:${toolPath(dir)}`, GH_MODE: 'reject' }),
  });
  assert.notEqual(ran.status, 0);
  assert.match(ran.stderr, /rejected this binary/);
  assert.equal(fs.existsSync(path.join(prefix, 'bwn')), false);
});

test('install.sh warns and continues when gh cannot attest', { skip: WIN && 'POSIX installer' }, () => {
  for (const mode of ['old', 'offline']) {
    const dir = tmp(mode);
    const stub = installStub(dir);
    const prefix = path.join(dir, 'bin');
    const fake = writeGh(dir, mode);
    const ran = spawnSync('sh', [installSh, '--file', stub, '--sha256', sha(stub), '--prefix', prefix], {
      encoding: 'utf8',
      env: posixEnv(dir, { PATH: `${fake}:${toolPath(dir)}`, GH_MODE: mode }),
    });
    assert.equal(ran.status, 0, mode + '\n' + ran.stderr + ran.stdout);
    assert.match(ran.stderr, /was not checked/);
    assert.equal(fs.existsSync(path.join(prefix, 'bwn')), true);
  }
});

test('install.sh --require-attest fails closed and --no-attest skips gh', { skip: WIN && 'POSIX installer' }, () => {
  const dir = tmp('strict');
  const stub = installStub(dir);
  const prefix = path.join(dir, 'strict-bin');
  const missing = spawnSync('sh', [installSh, '--file', stub, '--sha256', sha(stub), '--prefix', prefix, '--require-attest'], {
    encoding: 'utf8',
    env: posixEnv(dir),
  });
  assert.notEqual(missing.status, 0);
  assert.match(missing.stderr, /--require-attest/);
  assert.equal(fs.existsSync(path.join(prefix, 'bwn')), false);

  const fake = writeGh(dir, 'unauth');
  const loggedOut = spawnSync('sh', [installSh, '--file', stub, '--sha256', sha(stub), '--prefix', prefix, '--require-attest'], {
    encoding: 'utf8',
    env: posixEnv(dir, { PATH: `${fake}:${toolPath(dir)}`, GH_MODE: 'unauth' }),
  });
  assert.notEqual(loggedOut.status, 0);
  assert.match(loggedOut.stderr, /not logged in/);
  assert.equal(fs.existsSync(path.join(prefix, 'bwn')), false);

  const skip = path.join(dir, 'skip-bin');
  fs.writeFileSync(path.join(dir, 'gh.log'), '');
  const quiet = spawnSync('sh', [installSh, '--file', stub, '--sha256', sha(stub), '--prefix', skip, '--no-attest'], {
    encoding: 'utf8',
    env: posixEnv(dir, { PATH: `${fake}:${toolPath(dir)}`, GH_MODE: 'reject' }),
  });
  assert.equal(quiet.status, 0, quiet.stderr + quiet.stdout);
  assert.equal(fs.readFileSync(path.join(dir, 'gh.log'), 'utf8').trim(), '');
  assert.equal(fs.existsSync(path.join(skip, 'bwn')), true);
});

test('a rewritten shim is the native file, not Node', () => {
  const dir = tmp('hand');
  const bin = path.join(dir, 'payload');
  fs.writeFileSync(bin, WIN ? '@echo off\r\necho native-ok\r\n' : '#!/bin/sh\necho native-ok\n');
  if (!WIN) fs.chmodSync(bin, 0o755);
  const destDir = path.join(dir, 'native');
  const shimDir = path.join(dir, 'shims');
  fs.mkdirSync(shimDir, { recursive: true });
  const js = path.join(dir, 'buildwithnexus.js');
  fs.writeFileSync(js, 'console.log("node")\n');
  if (WIN) {
    const shim = path.join(shimDir, 'bwn.cmd');
    fs.writeFileSync(shim, `@echo off\r\nnode "${js}" %*\r\n`);
    const result = native.handoff(bin, {
      BWN_NATIVE_BIN: destDir,
      PATH: shimDir,
      NEXUS_HOME: path.join(dir, 'home'),
      npm_config_prefix: '',
    });
    assert.equal(fs.existsSync(result.dest), true);
    const cmd = fs.readFileSync(shim, 'utf8');
    assert.match(cmd, /bwn\.exe/);
    assert.doesNotMatch(cmd, /node /);
  } else {
    const shim = path.join(shimDir, 'bwn');
    fs.symlinkSync(js, shim);
    const result = native.handoff(bin, {
      BWN_NATIVE_BIN: destDir,
      PATH: shimDir,
      NEXUS_HOME: path.join(dir, 'home'),
      npm_config_prefix: '',
    });
    const ran = spawnSync(shim, [], { encoding: 'utf8' });
    assert.equal(ran.status, 0, ran.stderr);
    assert.match(ran.stdout, /native-ok/);
    assert.equal(result.rewrote, true);
    assert.equal(fs.readlinkSync(shim), result.dest);
  }
});

test('the formula names this package version', () => {
  const ver = require('../package.json').version;
  const text = formula('9.9.9', {
    'buildwithnexus-aarch64-apple-darwin': 'a'.repeat(64),
    'buildwithnexus-x86_64-apple-darwin': 'b'.repeat(64),
    'buildwithnexus-aarch64-unknown-linux-gnu': 'c'.repeat(64),
    'buildwithnexus-x86_64-unknown-linux-gnu': 'd'.repeat(64),
  });
  assert.match(text, /version "9\.9\.9"/);
  assert.match(text, new RegExp('a'.repeat(64)));
  const committed = fs.readFileSync(path.join(__dirname, '..', 'Formula', 'buildwithnexus.rb'), 'utf8');
  assert.match(committed, new RegExp(`version "${ver}"`));
});
