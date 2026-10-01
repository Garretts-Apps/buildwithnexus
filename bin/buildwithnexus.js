#!/usr/bin/env node
'use strict';
// Thin launcher: resolve the platform binary and hand off with stdio inherited
// so the alternate-screen TUI works exactly as if it were invoked directly.
// Deliberately boring — no install scripts, no network, no dynamic code. The
// binary itself handles update checks.
const path = require('path');
const { spawnSync } = require('child_process');
const {
  existing, platformPackage, target, installedBinary, readInstallMarker, EXIT_EXPLAINED,
} = require('../scripts/resolve-binary.js');

let args = process.argv.slice(2);
// Consume launcher options even when the binary is already installed. Respect
// `--` so a task can still refer to the literal flag.
const separatorIdx = args.indexOf('--');
const flagIdx = args.findIndex((a, i) =>
  a === '--bootstrap' && (separatorIdx === -1 || i < separatorIdx));
args = args.filter((a, i) =>
  a !== '--bootstrap' || (separatorIdx !== -1 && i > separatorIdx));
let bin = existing();
// Interactive runs (TTY) auto-download — the user already consented by
// running `npm install -g buildwithnexus`. Non-TTY environments (CI, pipes,
// scripts) never auto-download; use --bootstrap or BWN_ALLOW_BOOTSTRAP=1 to
// opt in explicitly there. process.stdin is only touched when no binary was
// found: opening it can change the stdin the binary inherits.
const consented = !bin && (
  flagIdx !== -1 ||
  process.env.BWN_ALLOW_BOOTSTRAP === '1' ||
  Boolean(process.stdin.isTTY));  // interactive: user is present; they installed it, they want it to work
if (!bin) {
  // A verified download of this version was installed, and now it is gone.
  // Something removed it (typically endpoint protection), so downloading it
  // again would only repeat that on every run. Only an explicit --bootstrap
  // downloads again.
  const marker = readInstallMarker();
  if (marker && marker.version === require('../package.json').version && flagIdx === -1) {
    const { diagnose } = require('../scripts/diagnose.js');
    const found = diagnose({
      bin: installedBinary(), vanished: true, verified: true, sha256: marker.sha256,
      expectedVersion: marker.version,
    });
    process.stderr.write(found.text);
    process.exit(1);
  }
  // The platform optionalDependency is absent (platform packages not yet
  // published, or installed with --omit=optional). Fall back to a
  // checksum-verified download from the GitHub release.
  if (consented) {
    const boot = spawnSync(process.execPath, [path.join(__dirname, '..', 'scripts', 'bootstrap.js')], {
      stdio: 'inherit',
    });
    // It could not install a binary that runs, and already said why.
    if (boot.status === EXIT_EXPLAINED) process.exit(1);
    bin = existing();
  } else {
    // Suggesting --bootstrap here would download a binary that cannot run.
    const { platformProblem } = require('../scripts/diagnose.js');
    const problem = platformProblem();
    if (problem) {
      process.stderr.write(problem.text);
      process.exit(1);
    }
  }
}
if (!bin) {
  const pkg = platformPackage();
  const t = target();
  let fetchOnce = '';
  if (pkg && process.env.BWN_SKIP_INSTALL) {
    fetchOnce = '  BWN_SKIP_INSTALL is set, so the launcher does not download it.\n';
  } else if (pkg && !consented) {
    fetchOnce = '  Download the checksum-verified release binary once (no terminal here, so the\n' +
      '  launcher will not download it on its own):\n' +
      '    bwn --bootstrap        (or BWN_ALLOW_BOOTSTRAP=1 bwn)\n';
  }
  process.stderr.write(
    'buildwithnexus: native binary not found.\n' +
    fetchOnce +
    (pkg
      ? `  If you installed with --omit=optional, reinstalling without it also works once the\n` +
        `  "${pkg}" package is available.\n`
      : `  No prebuilt binary exists for this platform (${process.platform} ${process.arch}).\n`) +
    (t
      ? '  Or build from source and point BWN_BIN at the result (an absolute path):\n' +
        '    git clone https://github.com/Garretts-Apps/buildwithnexus\n' +
        '    cargo build --release --manifest-path buildwithnexus/harness/Cargo.toml\n'
      : '') +
    '  Docs: https://buildwithnexus.dev/docs/install\n'
  );
  process.exit(1);
}

const result = spawnSync(bin, args, { stdio: 'inherit' });
if (result.error || result.status !== 0) {
  // Did the binary start at all? If not (blocked, removed, wrong libc), say
  // why instead of a bare "spawnSync ... EPERM" or loader error.
  const { explainRunFailure } = require('../scripts/diagnose.js');
  const marker = readInstallMarker(bin);
  const found = explainRunFailure(bin, result, {
    verified: Boolean(marker),
    sha256: marker && marker.sha256,
  });
  if (found) process.stderr.write(found.text);
  else if (result.error) process.stderr.write(`buildwithnexus: ${result.error.message}\n`);
  if (found || result.error) process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
