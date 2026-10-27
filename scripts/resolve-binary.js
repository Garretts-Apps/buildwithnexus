'use strict';
// Locate the native binary. No network, no shell, no install scripts: this
// module only resolves paths (the binary arrives by the checksum-verified
// first-run download in bootstrap.js). Node builtins only.
const path = require('path');
const fs = require('fs');
const os = require('os');

const ROOT = path.join(__dirname, '..');

function ext() {
  return process.platform === 'win32' ? '.exe' : '';
}

// Platforms the GitHub Release has a prebuilt binary for. The launcher never
// looks for the binary in an npm package: 0.12.1 to 0.14.2 listed per-platform
// packages that were never published, and nothing should install code from
// those names. The binary comes from the checksum-verified download.
const PREBUILT_PLATFORMS = new Set(['linux x64', 'linux arm64', 'darwin x64', 'darwin arm64', 'win32 x64']);

function hasPrebuilt() {
  return PREBUILT_PLATFORMS.has(`${process.platform} ${process.arch}`);
}

// BWN_BIN, if it is an absolute path. A relative value would resolve against
// whatever directory `bwn` is run in, so an untrusted checkout could supply it.
function overrideBinary() {
  const p = process.env.BWN_BIN;
  if (!p) return null;
  if (!path.isAbsolute(p)) {
    process.stderr.write(`buildwithnexus: ignoring BWN_BIN=${p}: it must be an absolute path.\n`);
    return null;
  }
  return p;
}

// Rust target triple for the current platform (used in docs/error messages).
function target() {
  const arch = { arm64: 'aarch64', x64: 'x86_64' }[process.arch] || process.arch;
  switch (process.platform) {
    case 'darwin': return `${arch}-apple-darwin`;
    case 'win32': return `${arch}-pc-windows-msvc`;
    case 'linux': return `${arch}-unknown-linux-gnu`;
    default: return null;
  }
}

// ~/.buildwithnexus, or NEXUS_HOME, as the binary resolves it. A relative
// NEXUS_HOME is ignored here: the launcher runs what it finds in this
// directory, and a relative path would resolve against the current project.
function nexusHome() {
  const h = process.env.NEXUS_HOME;
  if (h && path.isAbsolute(h)) return h;
  return path.join(process.env.HOME || process.env.USERPROFILE || os.homedir(), '.buildwithnexus');
}

function packageVersion() {
  return require(path.join(ROOT, 'package.json')).version;
}

// Where releases before 0.15 downloaded the binary: inside this package, so
// every npm update or reinstall deleted it.
function legacyBinary() {
  return path.join(ROOT, 'bin', 'buildwithnexus' + ext());
}

// Where the first run downloads the binary for this package version:
// <home>/bin/<version>/, outside the package so npm leaves it alone, and one
// directory per version so an upgrade never runs or replaces the old one.
// BWN_INSTALL_IN_PACKAGE=1 keeps the pre-0.15 location.
function installDir() {
  if (process.env.BWN_INSTALL_IN_PACKAGE === '1') return path.join(ROOT, 'bin');
  return path.join(nexusHome(), 'bin', packageVersion());
}

function installedBinary() {
  return path.join(installDir(), 'buildwithnexus' + ext());
}

// Written by bootstrap.js next to the binary after a verified download:
// { version, sha256 }. If it is here but the binary is not, something removed
// the binary after it was installed (typically endpoint protection), and
// downloading it again would only repeat that.
function installMarker() {
  return path.join(installDir(), '.installed.json');
}

// The marker for `bin`, if `bin` is a downloaded binary (either location).
function readInstallMarker(bin = installedBinary()) {
  if (bin !== installedBinary() && bin !== legacyBinary()) return null;
  try {
    const m = JSON.parse(fs.readFileSync(path.join(path.dirname(bin), '.installed.json'), 'utf8'));
    return m && typeof m.version === 'string' ? m : null;
  } catch {
    return null;
  }
}

// bootstrap.js exits with this after it printed why the binary it installed
// cannot run, so the launcher does not add a second message.
const EXIT_EXPLAINED = 3;

// Best-effort cleanup after a successful download: removes the other
// versions' <home>/bin/<version>/ directories (about 6 MB each). Only a
// directory directly under <home>/bin whose name looks like a version, and
// never `keep`. Failures are ignored (a binary still running from an old
// version cannot be deleted on Windows). Returns the versions it removed.
function pruneOldVersions(keep = packageVersion()) {
  const removed = [];
  try {
    const binDir = path.join(nexusHome(), 'bin');
    for (const entry of fs.readdirSync(binDir, { withFileTypes: true })) {
      if (!entry.isDirectory() || entry.name === keep) continue;
      if (!/^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/.test(entry.name)) continue;
      try {
        fs.rmSync(path.join(binDir, entry.name), { recursive: true, force: true });
        removed.push(entry.name);
      } catch {}
    }
  } catch {}
  return removed;
}

// Local development builds in a repo checkout.
function devBinary() {
  const rootTarget = path.join(ROOT, 'target', 'release', 'buildwithnexus' + ext());
  if (fs.existsSync(rootTarget)) return rootTarget;
  return path.join(ROOT, 'harness', 'target', 'release', 'buildwithnexus' + ext());
}

// First existing binary: explicit override, downloaded, downloaded by an
// earlier release, dev.
function existing() {
  const candidates = [overrideBinary(), installedBinary(), legacyBinary(), devBinary()]
    .filter(Boolean);
  return candidates.find((p) => fs.existsSync(p)) || null;
}

module.exports = {
  ROOT, ext, target, hasPrebuilt, nexusHome, installDir, installedBinary, legacyBinary,
  installMarker, readInstallMarker, pruneOldVersions, devBinary, existing, EXIT_EXPLAINED,
};
