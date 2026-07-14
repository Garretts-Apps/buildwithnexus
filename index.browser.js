'use strict';
// Browser-bundler entry (package.json "browser"). buildwithnexus is a native
// CLI — the real entry (index.js) touches child_process and the platform
// binary packages, which no browser bundle can resolve. Analyzers like
// bundlephobia bundle THIS file instead: a few bytes, no Node built-ins, and
// the CLI surface fails loudly only if actually called.
const { version } = require('./package.json');

function unavailable() {
  throw new Error('buildwithnexus is a native CLI and cannot run in a browser');
}

module.exports = { version, binaryPath: unavailable, run: unavailable };
