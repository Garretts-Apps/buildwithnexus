'use strict';
// Fails when the files that carry the release version disagree. The binary's
// --version must equal package.json's for the launcher to call it ready, the
// published bwn crate resolves buildwithnexus by the version in its
// dependency, and release.yml takes the release notes from CHANGELOG.md.
// ci.yml runs this on every PR and push, with --require-changelog when
// package.json's version is not tagged yet (a release is coming).
// Usage: node scripts/check-versions.js [--require-changelog] [ROOT]
const fs = require('fs');
const path = require('path');

// The body of a TOML table, from after its [name] header to the next header.
function tomlTable(text, name) {
  const parts = text.split(/^\[/m);
  const part = parts.find((p) => p.startsWith(`${name}]`));
  return part === undefined ? null : part.slice(name.length + 1);
}

function tomlString(body, key) {
  const m = body && body.match(new RegExp(`^${key}\\s*=\\s*"([^"]*)"`, 'm'));
  return m ? m[1] : null;
}

// bwn's `buildwithnexus = { version = "…", path = "…" }`.
function dependencyVersion(text, dep) {
  const body = tomlTable(text, 'dependencies');
  const line = body && body.match(new RegExp(`^${dep}\\s*=\\s*\\{([^}]*)\\}`, 'm'));
  const m = line && line[1].match(/\bversion\s*=\s*"([^"]*)"/);
  return m ? m[1] : null;
}

// Cargo.lock's [[package]] version for a crate built from this workspace
// (no `source` line; a registry crate of the same name would have one).
function lockVersion(text, name) {
  for (const block of text.split(/^\[\[package\]\]$/m)) {
    if (tomlString(block, 'name') === name && tomlString(block, 'source') === null) {
      return tomlString(block, 'version');
    }
  }
  return null;
}

function changelogHas(text, version) {
  const esc = version.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return new RegExp(`^## \\[${esc}\\]`, 'm').test(text);
}

// Returns { version, problems }: one problem per file that disagrees with
// package.json, each naming the file.
function check(root, { requireChangelog = false } = {}) {
  const read = (f) => {
    try {
      return fs.readFileSync(path.join(root, f), 'utf8');
    } catch {
      return null;
    }
  };
  const problems = [];
  let version = null;
  try {
    version = JSON.parse(read('package.json')).version;
  } catch {
    // Reported below.
  }
  if (typeof version !== 'string' || !version) {
    return { version: null, problems: ['package.json: no "version"'] };
  }
  const expect = (file, what, found) => {
    if (found === null) problems.push(`${file}: no ${what} (package.json is ${version})`);
    else if (found !== version) problems.push(`${file}: ${what} is ${found}, package.json is ${version}`);
  };
  const harness = read('harness/Cargo.toml');
  const bwn = read('bwn/Cargo.toml');
  const lock = read('Cargo.lock');
  const changelog = read('CHANGELOG.md');
  expect('harness/Cargo.toml', '[package] version', harness && tomlString(tomlTable(harness, 'package'), 'version'));
  expect('bwn/Cargo.toml', '[package] version', bwn && tomlString(tomlTable(bwn, 'package'), 'version'));
  expect('bwn/Cargo.toml', 'buildwithnexus dependency version', bwn && dependencyVersion(bwn, 'buildwithnexus'));
  expect('Cargo.lock', 'buildwithnexus version', lock && lockVersion(lock, 'buildwithnexus'));
  expect('Cargo.lock', 'bwn version', lock && lockVersion(lock, 'bwn'));
  if (requireChangelog && !(changelog && changelogHas(changelog, version))) {
    problems.push(`CHANGELOG.md: no "## [${version}]" section; v${version} is not tagged yet, so release.yml would ship it without notes`);
  }
  return { version, problems };
}

module.exports = { check, lockVersion, dependencyVersion };

if (require.main === module) {
  const args = process.argv.slice(2);
  const requireChangelog = args.includes('--require-changelog');
  const root = args.find((a) => !a.startsWith('--')) || path.join(__dirname, '..');
  const { version, problems } = check(root, { requireChangelog });
  if (problems.length) {
    for (const p of problems) console.log(`::error::${p}`);
    console.log(`${problems.length} version mismatch${problems.length === 1 ? '' : 'es'}; bump every file above to the same version.`);
    process.exit(1);
  }
  console.log(`Versions agree: ${version}${requireChangelog ? ' (CHANGELOG.md has its section)' : ''}`);
}
