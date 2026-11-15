'use strict';

const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');
const { formula, handoff } = (() => {
  return {
    formula: require('./update-install-manifests.js').formula,
    handoff: null,
  };
})();
const native = require('./native-handoff.js');

const WIN = process.platform === 'win32';
const installSh = path.join(__dirname, 'install.sh');

function sha(file) {
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}

function tmp(tag) {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'bwn-' + tag + '-'));
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
