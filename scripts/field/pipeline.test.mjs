// The release pipeline's ordering, read from the workflow files: a public
// GitHub release only after its assets are on it (v0.14.9 was public for
// about three minutes with none), npm only after the exact tarball passed an
// install run in clean containers, and no publish.yml input that tags a
// version release.yml never builds. actionlint checks syntax; these check
// the order and the permissions it depends on.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const repo = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const workflow = (name) => readFileSync(join(repo, '.github', 'workflows', name), 'utf8');

// Top-level jobs as { name: text }, split on two-space job keys under `jobs:`.
function jobs(text) {
  const body = text.slice(text.search(/^jobs:$/m));
  const out = {};
  let name = null;
  for (const line of body.split('\n').slice(1)) {
    const m = line.match(/^ {2}([\w-]+):\s*$/);
    if (m) {
      name = m[1];
      out[name] = '';
    } else if (name) {
      out[name] += `${line}\n`;
    }
  }
  return out;
}

// A job's `needs:` as a list (inline list or single name).
function needs(job) {
  const m = job.match(/^ {4}needs:\s*(.+)$/m);
  if (!m) return [];
  return m[1].replace(/[[\]]/g, '').split(',').map((s) => s.trim()).filter(Boolean);
}

// Every step in a job, as its text.
function steps(job) {
  return job.split(/^ {6}- /m).slice(1);
}

test('release.yml keeps the release a draft until every asset is on it', () => {
  const all = jobs(workflow('release.yml'));
  const uploads = Object.entries(all).filter(([, job]) => /softprops\/action-gh-release@/.test(job));
  assert.deepEqual(uploads.map(([name]) => name).sort(), ['assets', 'release', 'sbom']);
  for (const [name, job] of uploads) {
    for (const step of steps(job).filter((s) => /softprops\/action-gh-release@/.test(s))) {
      // Without draft: true, softprops publishes a new release when its step
      // ends, and un-drafts an existing draft after uploading to it.
      assert.match(step, /^ {10}draft: true$/m, `${name}: ${step.split('\n')[0]}`);
    }
  }

  // One job publishes it, after everything that writes to it.
  const publishers = Object.entries(all).filter(([, job]) => /--draft=false/.test(job));
  assert.deepEqual(publishers.map(([name]) => name), ['publish-release']);
  const last = all['publish-release'];
  for (const dep of ['release', 'assets', 'sbom']) {
    assert.ok(needs(last).includes(dep), `publish-release needs ${dep}`);
  }
  assert.match(last, /if: needs\.gate\.outputs\.release == 'true'/);
  // It checks the assets before publishing.
  const [check, publish] = steps(last);
  assert.match(check, /buildwithnexus-\$t\$ext\.sha256/);
  assert.match(check, /buildwithnexus\.cdx\.json/);
  assert.match(publish, /gh release edit "\$TAG" --repo "\$GITHUB_REPOSITORY" --draft=false/);
});

test('release.yml publishes crates.io last, after the packed tarball installed', () => {
  // A crate version cannot be replaced, so nothing irreversible happens
  // until the public release's npm tarball has installed in clean containers.
  const all = jobs(workflow('release.yml'));
  const gate = all['tarball-gate'];
  assert.ok(gate, 'release.yml has a tarball-gate job');
  assert.ok(needs(gate).includes('publish-release'), 'the gate needs the public release');
  assert.match(gate, /bash scripts\/pack-verified\.sh/);
  assert.match(gate, /linux-install\.sh --tarball "\$TARBALL"/);
  for (const c of ['ubuntu:22.04', 'node:22-bookworm', 'node:20-bullseye', 'node:22-alpine']) {
    assert.ok(gate.includes(c), `the gate runs ${c}`);
  }
  assert.doesNotMatch(gate, /id-token|contents: write/, 'the job running third-party images holds no write or OIDC token');
  const crates = all.crates;
  assert.ok(needs(crates).includes('tarball-gate'), 'crates needs tarball-gate');
  assert.match(crates, /needs\.tarball-gate\.result == 'success'/);
  assert.match(crates, /needs\.gate\.outputs\.crates_retry == 'true'/);
  // Nothing runs after crates.io: it is the last job.
  for (const [name, job] of Object.entries(all)) {
    assert.ok(!needs(job).includes('crates'), `${name} must not wait on crates`);
  }
  // Both workflows pack with the same script, so both gates test one thing.
  assert.match(jobs(workflow('publish.yml')).pack, /bash scripts\/pack-verified\.sh/);
  const script = readFileSync(join(repo, 'scripts', 'pack-verified.sh'), 'utf8');
  assert.match(script, /gh attestation verify/);
  assert.match(script, /--signer-workflow "\$\{GITHUB_REPOSITORY\}\/\.github\/workflows\/release\.yml"/);
});

test('publish.yml chains on release.yml completing and has no version bump', () => {
  const text = workflow('publish.yml');
  assert.match(text, /^ {2}workflow_run:\n {4}workflows: \["release"\]\n {4}types: \[completed\]$/m);
  assert.match(text, /^ {2}workflow_dispatch:$/m);
  assert.doesNotMatch(text, /version_bump|npm version\b|git push/);
  const on = text.slice(text.search(/^on:$/m), text.search(/^permissions:/m));
  assert.doesNotMatch(on, /inputs:/, 'the sentinel dispatches publish.yml with no inputs');
  // Nothing in publish.yml writes to the repository any more.
  assert.doesNotMatch(text, /contents: write/);
});

test('publish.yml installs the exact tarball in clean containers before npm publish', () => {
  const all = jobs(workflow('publish.yml'));
  assert.deepEqual(Object.keys(all), ['pack', 'field', 'publish']);
  assert.deepEqual(needs(all.field), ['pack']);
  assert.deepEqual(needs(all.publish), ['pack', 'field']);

  // pack: the shared script writes checksums.json first, then the tarball,
  // which is handed on as an artifact.
  const pack = steps(all.pack).map((s) => s.split('\n')[0]);
  const at = (re) => pack.findIndex((s) => re.test(s));
  assert.ok(at(/name: Pack$/) >= 0 && at(/name: Pack$/) < at(/Hand the tarball/), pack.join('\n'));
  const packScript = readFileSync(join(repo, 'scripts', 'pack-verified.sh'), 'utf8');
  assert.ok(packScript.indexOf("echo '{}' > checksums.json") < packScript.indexOf('npm pack --pack-destination dist --json'));
  assert.match(packScript, /npm pack --pack-destination dist --json/);

  // field: the install matrix against that tarball and this release.yml,
  // on a supported glibc, a glibc below the floor and musl.
  const run = all.field.match(/bash scripts\/field\/linux-install\.sh[^\n]*\n(?: {10}[^\n]*\n)*/);
  assert.ok(run, 'field job runs linux-install.sh');
  assert.match(run[0], /--tarball "\$TARBALL"/);
  assert.match(run[0], /--release-yml \.github\/workflows\/release\.yml/);
  const cells = run[0].match(/--cells (\S+)/)[1].split(',');
  const script = readFileSync(join(repo, 'scripts', 'field', 'linux-install.sh'), 'utf8');
  const known = new Set(`${script.match(/^BINARY_CELLS=\(([^)]*)\)/m)[1]} ${script.match(/^NPM_CELLS=\(([^)]*)\)/m)[1]}`.split(/\s+/).filter(Boolean));
  for (const c of cells) assert.ok(known.has(c), `${c} is a linux-install.sh cell`);
  // ubuntu:22.04 is glibc 2.35 and bookworm 2.36 (floor 2.34), bullseye 2.31.
  for (const c of ['ubuntu:22.04', 'node:22-bookworm', 'node:20-bullseye', 'node:22-alpine']) {
    assert.ok(cells.includes(c), `cells include ${c}`);
  }
  assert.doesNotMatch(all.field, /id-token|contents: write/, 'the job running third-party images holds no write or OIDC token');
  assert.match(all.field, /name: npm-package/);

  // publish: that same file (checked by hash), not a re-pack.
  assert.doesNotMatch(all.publish, /npm pack/);
  assert.match(all.publish, /sha256sum -c -/);
  assert.match(all.publish, /npm publish "\$TARBALL" --access public --provenance/);
  assert.match(all.publish, /id-token: write/);
});

test('publish.yml skips the field run for a version already on npm', () => {
  // Every docs-only merge to main chains a publish.yml run for the version
  // already live; installing it in containers again tests nothing, and a
  // flaky cell would turn that run red.
  const all = jobs(workflow('publish.yml'));
  const [check] = steps(all.pack).filter((s) => /npm view "buildwithnexus@\$V" version/.test(s));
  assert.ok(check, 'pack checks npm for the version');
  assert.match(all.pack, /already: \$\{\{ steps\.published\.outputs\.already \}\}/);
  assert.match(all.field, /^ {4}if: needs\.pack\.outputs\.already == 'false'$/m);
  // Nor does it download and verify the binaries or pack a tarball.
  for (const name of ['Pack', 'Hand the tarball to the field and publish jobs']) {
    const [step] = steps(all.pack).filter((s) => s.includes(`name: ${name}`));
    assert.ok(step, `pack has the step "${name}"`);
    assert.match(step, /if: steps\.published\.outputs\.already == 'false'/, `"${name}" is skipped when already on npm`);
  }
  // publish runs only for a new version, and never after a field run that
  // did not pass. crates.io is release.yml's job, so nothing else is left.
  assert.match(all.publish, /^ {4}if: needs\.pack\.outputs\.already == 'false' && needs\.field\.result == 'success'$/m);
  assert.doesNotMatch(all.publish, /steps\.published/);
  assert.doesNotMatch(all.publish, /cargo publish|CARGO_REGISTRY_TOKEN/);
  assert.match(all.publish, /if: needs\.pack\.outputs\.already == 'false'\n/);
});
