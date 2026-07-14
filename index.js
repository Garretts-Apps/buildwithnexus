'use strict';
// Library entry point. buildwithnexus is a CLI first — this module exists so
// package tooling (bundle analyzers, require() probes) has a valid entry, and
// it exposes a tiny programmatic API for locating and launching the binary.
const { spawn } = require('child_process');
const { existing } = require('./scripts/resolve-binary.js');
const { version } = require('./package.json');

// Absolute path to the installed native binary, or null if missing.
function binaryPath() {
  return existing();
}

// Spawn the CLI with the given args; returns the ChildProcess.
function run(args = [], options = { stdio: 'inherit' }) {
  const bin = binaryPath();
  if (!bin) {
    throw new Error('buildwithnexus: native binary not found — reinstall the package');
  }
  return spawn(bin, args, options);
}

module.exports = { version, binaryPath, run };
