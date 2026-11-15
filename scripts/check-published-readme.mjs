#!/usr/bin/env node
// The repository README is what npm and crates.io render for buildwithnexus.
//
// npm's registry stores 65536 UTF-16 code units of it and drops the rest
// mid-sentence. 0.20.0's page ends at "/doctor", so Editors, platform
// support, the proxy, Safety and the license never appear. Stay under
// README_BUDGET, which leaves room for a later edit before that cut.
//
// crates.io resolves a relative link from harness/, the crate directory
// (cargo copies ../README.md into the crate and records that path). 
// BENCHMARKS.md becomes harness/BENCHMARKS.md and 404s. A file link has to
// be an absolute URL. Same-page anchors (#permissions) are fine on both.

import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

/** npm's registry cuts the readme field here, in UTF-16 code units. */
export const NPM_README_LIMIT = 65536;

/** Fail before the registry does, so one more paragraph still fits. */
export const README_BUDGET = 62000;

const FILE_LINK = /!?\[[^\]]*\]\(([^)\s]+)\)/g;

/**
 * @param {string} text
 * @returns {string[]} problems, empty when the readme is safe to publish
 */
export function checkReadme(text) {
  const problems = [];
  if (text.length > README_BUDGET) {
    problems.push(
      `README.md is ${text.length} UTF-16 code units. Keep it at or under ${README_BUDGET} ` +
        `(npm's registry stores ${NPM_README_LIMIT} and drops the rest mid-sentence).`,
    );
  }
  let inFence = false;
  for (const [index, line] of text.split('\n').entries()) {
    if (/^(`{3,}|~{3,})/.test(line)) {
      inFence = !inFence;
      continue;
    }
    if (inFence) continue;
    for (const match of line.matchAll(FILE_LINK)) {
      const url = match[1];
      if (url.startsWith('#') || url.startsWith('mailto:') || /^https?:\/\//.test(url)) continue;
      problems.push(
        `README.md:${index + 1}: "${url}" is a relative file link. crates.io resolves it ` +
          'from harness/ and the page 404s. Use an absolute https:// URL.',
      );
    }
  }
  return problems;
}

function main() {
  const file = process.argv[2] ?? 'README.md';
  const problems = checkReadme(readFileSync(file, 'utf8'));
  if (problems.length === 0) return;
  for (const problem of problems) console.error(problem);
  process.exitCode = 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main();
