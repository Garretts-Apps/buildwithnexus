import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

import { README_BUDGET, checkReadme } from './check-published-readme.mjs';

test('anchors and absolute urls pass', () => {
  const text = [
    '# Title',
    '',
    'See [Permissions](#permissions) and [the site](https://buildwithnexus.dev).',
    '![badge](https://img.shields.io/npm/v/buildwithnexus)',
    '',
  ].join('\n');
  assert.deepEqual(checkReadme(text), []);
});

test('a relative file link is rejected, including inside a label', () => {
  const problems = checkReadme('See [BENCHMARKS.md](BENCHMARKS.md) and [docs](./docs/VERSIONING.md).\n');
  assert.equal(problems.length, 2);
  assert.match(problems[0], /BENCHMARKS\.md/);
  assert.match(problems[1], /docs\/VERSIONING\.md/);
});

test('a relative link inside a code fence is not a published link', () => {
  const text = '```md\n[file](BENCHMARKS.md)\n```\n';
  assert.deepEqual(checkReadme(text), []);
});

test('a readme npm would cut off is rejected', () => {
  const problems = checkReadme(`${'x'.repeat(README_BUDGET + 1)}\n`);
  assert.equal(problems.length, 1);
  assert.match(problems[0], /UTF-16/);
});

test('the repository README publishes whole, with working file links', () => {
  const text = readFileSync(new URL('../README.md', import.meta.url), 'utf8');
  assert.deepEqual(checkReadme(text), []);
});
