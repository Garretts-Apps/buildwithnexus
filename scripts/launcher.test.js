'use strict';

const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const path = require('node:path');
const test = require('node:test');

const launcher = path.join(__dirname, '..', 'bin', 'buildwithnexus.js');
function run(args) {
  // Node stands in for an installed native binary. No download or API needed.
  return spawnSync(process.execPath, [launcher, ...args], {
    encoding: 'utf8',
    env: { ...process.env, BWN_BIN: process.execPath },
  });
}

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
