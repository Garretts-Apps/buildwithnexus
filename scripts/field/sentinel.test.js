'use strict';

// Drives scripts/field/sentinel.sh against an in-process fake of the GitHub
// REST API, the npm registry and the docs site, and checks the issue and
// dispatch writes it makes. Needs bash, curl and jq (all on ubuntu runners).
// Run: node --test scripts/field/sentinel.test.js

const test = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFile } = require('node:child_process');

const SCRIPT = path.join(__dirname, 'sentinel.sh');
const NOW = Date.parse('2026-10-01T12:00:00Z') / 1000;
const iso = (t) => new Date(t * 1000).toISOString().replace(/\.\d{3}Z$/, 'Z');
const SHA = 'a'.repeat(40);

function makeState(over = {}) {
  return {
    head: { sha: SHA, date: iso(NOW - 30 * 60), message: 'v0.15.0: things (#120)' },
    version: '0.15.0',
    distTags: { latest: '0.14.9' },
    time: { '0.14.9': iso(NOW - 3 * 86400) },
    releaseRuns: [],
    publishRuns: [],
    ciRunsForHead: 1,
    ciRuns: [{ id: 11, run_number: 7, head_sha: SHA, event: 'push', status: 'completed', conclusion: 'success', run_attempt: 1, html_url: 'https://gh/run/11' }],
    ciJobs: [],
    site: '<script type="application/ld+json">{ "softwareVersion": "0.14.9" }</script><a><span class="ver">v0.14.9</span></a>',
    labels: ['dependencies', 'npm', 'rust', 'github-actions', 'sentinel'],
    pulls: [],
    checkRuns: {},
    statuses: {},
    issues: [],
    comments: [],
    dispatches: [],
    writes: [],
    // Paths that fail: a string fails with HTTP 500 every time;
    // { match, code, times, method } fails with `code`, only the first `times`
    // requests, only for `method`.
    broken: [],
    ...over,
  };
}

function startServer(state) {
  const server = http.createServer((req, res) => {
    let raw = '';
    req.on('data', (c) => (raw += c));
    req.on('end', () => {
      const url = new URL(req.url, 'http://x');
      const p = url.pathname;
      const q = url.searchParams;
      const body = raw ? JSON.parse(raw) : null;
      const send = (code, obj) => {
        res.writeHead(code, { 'content-type': 'application/json' });
        res.end(obj === undefined ? '' : typeof obj === 'string' ? obj : JSON.stringify(obj));
      };
      if (req.method !== 'GET') state.writes.push({ method: req.method, path: p, body });
      for (const b of state.broken) {
        const rule = typeof b === 'string' ? { match: b, code: 500 } : b;
        if (!p.includes(rule.match) || (rule.method && rule.method !== req.method)) continue;
        if (rule.times === undefined) return send(rule.code, { message: 'boom' });
        if (rule.times > 0) { rule.times -= 1; return send(rule.code, { message: 'boom' }); }
      }
      const page = Number(q.get('page') || 1);
      const paged = (arr) => (page === 1 ? arr : []);
      const R = '/repos/o/r';
      let m;
      if (req.method === 'GET' && p === '/npm/buildwithnexus') return send(200, { 'dist-tags': state.distTags, time: state.time });
      if (req.method === 'GET' && p === '/site') return send(200, state.site);
      if (req.method === 'GET' && p === `${R}/commits/main`) return send(200, { sha: state.head.sha, commit: { message: state.head.message, committer: { date: state.head.date } } });
      if (req.method === 'GET' && p === `${R}/contents/package.json`) return send(200, { name: 'buildwithnexus', version: state.version });
      const runs = (arr) => ({ total_count: arr.length, workflow_runs: arr.map((r) => ({ created_at: iso(NOW - 600), ...r })) });
      if (req.method === 'GET' && p === `${R}/actions/workflows/release.yml/runs`) return send(200, runs(state.releaseRuns));
      if (req.method === 'GET' && p === `${R}/actions/workflows/publish.yml/runs`) return send(200, runs(state.publishRuns));
      if (req.method === 'GET' && p === `${R}/actions/workflows/ci.yml/runs`) {
        if (q.get('head_sha')) return send(200, runs(Array.from({ length: state.ciRunsForHead }, () => ({}))));
        return send(200, runs(state.ciRuns));
      }
      if (req.method === 'GET' && (m = p.match(/\/actions\/runs\/(\d+)\/jobs$/))) return send(200, { jobs: state.ciJobs });
      if (req.method === 'GET' && p === `${R}/labels`) return send(200, paged(state.labels.map((name) => ({ name }))));
      if (req.method === 'POST' && p === `${R}/labels`) { state.labels.push(body.name); return send(201, body); }
      if (req.method === 'GET' && p === `${R}/pulls`) return send(200, paged(state.pulls));
      if (req.method === 'GET' && (m = p.match(/\/commits\/(\w+)\/check-runs$/))) return send(200, { check_runs: state.checkRuns[m[1]] || [] });
      if (req.method === 'GET' && (m = p.match(/\/commits\/(\w+)\/status$/))) return send(200, { statuses: state.statuses[m[1]] || [] });
      if (req.method === 'GET' && p === `${R}/issues`) return send(200, paged(state.issues));
      if (req.method === 'POST' && p === `${R}/issues`) {
        const issue = { number: 100 + state.issues.length, state: 'open', state_reason: null, ...body };
        state.issues.unshift(issue);
        return send(201, issue);
      }
      if (req.method === 'PATCH' && (m = p.match(/\/issues\/(\d+)$/))) {
        const issue = state.issues.find((i) => i.number === Number(m[1]));
        if (!issue) return send(404, { message: 'Not Found' });
        Object.assign(issue, body);
        return send(200, issue);
      }
      if (req.method === 'POST' && (m = p.match(/\/issues\/(\d+)\/comments$/))) {
        state.comments.push({ number: Number(m[1]), body: body.body });
        return send(201, {});
      }
      if (req.method === 'POST' && (m = p.match(/\/actions\/workflows\/([\w.-]+)\/dispatches$/))) {
        state.dispatches.push({ workflow: m[1], ...body });
        return send(204);
      }
      return send(404, { message: `no route: ${req.method} ${p}` });
    });
  });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve(server)));
}

function fixtureRoot() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'sentinel-'));
  fs.mkdirSync(path.join(root, '.github/workflows'), { recursive: true });
  fs.mkdirSync(path.join(root, 'field'));
  fs.writeFileSync(path.join(root, 'package.json'), '{"name":"buildwithnexus","version":"0.15.0"}\n');
  fs.writeFileSync(path.join(root, '.github/dependabot.yml'), [
    'version: 2',
    'updates:',
    '  - package-ecosystem: cargo',
    '    labels:',
    '      - dependencies',
    '      - rust # shipped product',
    '  - package-ecosystem: npm',
    '    labels: [dependencies, "npm", tooling]',
    '',
  ].join('\n'));
  fs.writeFileSync(path.join(root, '.github/workflows/ci.yml'), [
    'jobs:',
    '  a:',
    '    # macos-14 in a comment does not count',
    '    runs-on: ${{ matrix.os }}',
    '    strategy:',
    '      matrix:',
    '        os: [macos-14-large, windows-2022]',
    '  b:',
    '    runs-on: ubuntu-20.04',
    '',
  ].join('\n'));
  fs.writeFileSync(path.join(root, 'field/deprecated-labels.tsv'),
    '# label\tdate\tnote\nubuntu-20.04\t2025-04-15\tRetired.\nmacos-14\t2026-11-02\tBrownouts.\nmacos-14-large\t2026-11-02\tSame.\n');
  return root;
}

async function run(t, state, args, envOver = {}) {
  const server = await startServer(state);
  t.after(() => server.close());
  const base = `http://127.0.0.1:${server.address().port}`;
  const root = fixtureRoot();
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  for (const [f, text] of Object.entries(state.files || {})) fs.writeFileSync(path.join(root, f), text);
  const env = {
    PATH: process.env.PATH,
    HOME: process.env.HOME,
    GITHUB_API_URL: base,
    SENTINEL_NPM_REGISTRY: `${base}/npm`,
    SENTINEL_SITE_URL: `${base}/site`,
    SENTINEL_NOW: String(NOW),
    SENTINEL_RETRY_DELAY: '0',
    GH_TOKEN: 'test-token',
    NO_PROXY: '127.0.0.1,localhost',
    no_proxy: '127.0.0.1,localhost',
    ...envOver,
  };
  for (const [k, v] of Object.entries(env)) if (v === undefined) delete env[k];
  return new Promise((resolve) => {
    execFile('bash', [SCRIPT, '--repo', 'o/r', '--root', root, ...args], { env }, (err, stdout, stderr) => {
      resolve({ code: err ? err.code : 0, stdout, stderr });
    });
  });
}

const sentinelIssue = (number, sig, check, streak, extra = {}) => ({
  number,
  state: 'open',
  state_reason: null,
  title: `sentinel: ${sig}`,
  body: `<!-- sentinel check="${check}" sig="${sig}" streak="${streak}" first="2026-09-30T00:00Z" dispatched="" -->\n\n<!-- sentinel:details -->\nold details\n<!-- /sentinel:details -->\n`,
  ...extra,
});

test('release-gap opens one issue and dispatches release.yml once per commit', async (t) => {
  const state = makeState();
  let r = await run(t, state, ['--checks', 'release-gap']);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /FAIL\s+release-gap\s+release-gap: v0\.15\.0 is on main but release\.yml never ran/);
  assert.equal(state.dispatches.length, 1);
  assert.deepEqual(state.dispatches[0], { workflow: 'release.yml', ref: 'main' });
  assert.equal(state.issues.length, 1);
  assert.deepEqual(state.issues[0].labels, ['sentinel']);
  assert.match(state.issues[0].body, /sig="release-gap" streak="0"/);
  assert.match(state.issues[0].body, new RegExp(`dispatched="release\\.yml:${SHA}@`));

  // The next hour: still no run for the commit. No second dispatch, no new issue.
  r = await run(t, state, ['--checks', 'release-gap']);
  assert.equal(r.code, 0, r.stderr);
  assert.equal(state.dispatches.length, 1);
  assert.equal(state.issues.length, 1);
  assert.match(r.stdout, /already dispatched/);
});

test('release-gap waits 10 minutes, then accepts a running release', async (t) => {
  let state = makeState({ head: { sha: SHA, date: iso(NOW - 5 * 60) } });
  let r = await run(t, state, ['--checks', 'release-gap']);
  assert.match(r.stdout, /PASS\s+release-gap\s+main aaaaaaa \(v0\.15\.0\) is 5 min old/);
  assert.equal(state.writes.length, 0);

  state = makeState({ releaseRuns: [{ status: 'in_progress', conclusion: null }] });
  r = await run(t, state, ['--checks', 'release-gap']);
  assert.match(r.stdout, /PASS\s+release-gap\s+release\.yml is running/);
  assert.equal(state.dispatches.length, 0);
});

test('runs created after SENTINEL_NOW are ignored, so backtests see the past', async (t) => {
  const state = makeState({ releaseRuns: [{ status: 'completed', conclusion: 'success', created_at: iso(NOW + 60) }] });
  const r = await run(t, state, ['--checks', 'release-gap', '--dry-run']);
  assert.match(r.stdout, /FAIL\s+release-gap\s+release-gap: /);
});

test('release-gap reports a failed release without re-dispatching', async (t) => {
  const state = makeState({
    releaseRuns: [{ status: 'completed', conclusion: 'failure', event: 'push', run_attempt: 2, html_url: 'https://gh/run/9' }],
  });
  const r = await run(t, state, ['--checks', 'release-gap']);
  assert.match(r.stdout, /FAIL\s+release-gap\s+release-gap\/failed/);
  assert.equal(state.dispatches.length, 0);
  assert.equal(state.issues.length, 1);
});

test('an issue closes after three passing runs in a row, with one comment', async (t) => {
  const state = makeState({ distTags: { latest: '0.15.0' }, issues: [sentinelIssue(7, 'release-gap', 'release-gap', 0)] });
  for (const want of ['1', '2']) {
    const r = await run(t, state, ['--checks', 'release-gap']);
    assert.equal(r.code, 0, r.stderr);
    assert.equal(state.issues[0].state, 'open');
    assert.match(state.issues[0].body, new RegExp(`streak="${want}"`));
    assert.match(state.issues[0].body, /old details/);
  }
  assert.equal(state.comments.length, 0);
  await run(t, state, ['--checks', 'release-gap']);
  assert.equal(state.issues[0].state, 'closed');
  assert.equal(state.issues[0].state_reason, 'completed');
  assert.equal(state.comments.length, 1);
});

test('a failure resets the streak and comments once; a muted signature stays closed', async (t) => {
  let state = makeState({ issues: [sentinelIssue(7, 'release-gap', 'release-gap', 2, { body: sentinelIssue(7, 'release-gap', 'release-gap', 2).body.replace('dispatched=""', `dispatched="release.yml:${SHA}@2026-10-01T11:00Z"`) })] });
  let r = await run(t, state, ['--checks', 'release-gap']);
  assert.equal(r.code, 0, r.stderr);
  assert.match(state.issues[0].body, /streak="0"/);
  assert.equal(state.comments.length, 1);
  assert.match(state.comments[0].body, /Failing again/);
  assert.equal(state.dispatches.length, 0, 'dispatch recorded in the issue is not repeated');

  state = makeState({
    releaseRuns: [{ status: 'completed', conclusion: 'failure', event: 'push', run_attempt: 1, html_url: 'u' }],
    issues: [sentinelIssue(8, 'release-gap/failed', 'release-gap', 0, { state: 'closed', state_reason: 'not_planned' })],
  });
  r = await run(t, state, ['--checks', 'release-gap']);
  assert.match(r.stdout, /muted: #8/);
  assert.equal(state.writes.length, 0);

  state.issues[0].state_reason = 'completed';
  r = await run(t, state, ['--checks', 'release-gap']);
  assert.equal(state.issues[0].state, 'open');
  assert.equal(state.issues.length, 1);
  assert.match(state.comments.at(-1).body, /reopened/);
});

test('--dry-run and a missing token write nothing', async (t) => {
  let state = makeState();
  let r = await run(t, state, ['--checks', 'frequent', '--dry-run']);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /\[dry-run\] would dispatch release\.yml/);
  assert.match(r.stdout, /\[dry-run\] would open issue "sentinel: v0\.15\.0 is on main/);
  assert.equal(state.writes.length, 0);

  state = makeState();
  r = await run(t, state, ['--checks', 'frequent'], { GH_TOKEN: undefined });
  assert.match(r.stderr, /running as --dry-run/);
  assert.equal(state.writes.length, 0);
});

test('main-red names the failing jobs; a missing push run is its own signature', async (t) => {
  const state = makeState({
    ciRunsForHead: 0,
    ciRuns: [
      { id: 12, run_number: 8, head_sha: SHA, event: 'push', status: 'completed', conclusion: 'cancelled', run_attempt: 1, html_url: 'u' },
      { id: 11, run_number: 7, head_sha: SHA, event: 'push', status: 'completed', conclusion: 'failure', run_attempt: 2, html_url: 'https://gh/run/11' },
    ],
    ciJobs: [
      { name: 'Cargo audit', conclusion: 'failure', html_url: 'https://gh/job/1' },
      { name: 'Lint workflows', conclusion: 'success', html_url: 'https://gh/job/2' },
    ],
  });
  const r = await run(t, state, ['--checks', 'main-red', '--dry-run']);
  assert.match(r.stdout, /FAIL\s+main-red\s+main-red\/no-run/);
  assert.match(r.stdout, /FAIL\s+main-red\s+main-red: CI is red on main/);
  assert.match(r.stdout, /\[run 7\]\(https:\/\/gh\/run\/11\) for `aaaaaaa` \(attempt 2\), concluded \*\*failure\*\*/, 'cancelled runs are skipped');
  assert.match(r.stdout, /\[Cargo audit\]\(https:\/\/gh\/job\/1\): failure/);
  assert.doesNotMatch(r.stdout, /Lint workflows/);
  assert.doesNotMatch(r.stdout, /PASS\s+main-red/);
});

test('site-version allows 2 hours after publish, then fails', async (t) => {
  const site = '<script>{"softwareVersion": "0.14.8"}</script><span class="ver">v0.14.8</span>';
  let state = makeState({ site, time: { '0.14.9': iso(NOW - 30 * 60) } });
  let r = await run(t, state, ['--checks', 'site-version', '--dry-run']);
  assert.match(r.stdout, /PASS\s+site-version\s+site lags npm v0\.14\.9, which was published 30 min ago/);

  state = makeState({ site });
  r = await run(t, state, ['--checks', 'site-version', '--dry-run']);
  assert.match(r.stdout, /FAIL\s+site-version\s+site-version: .* does not show npm latest \(0\.14\.9\)/);
  assert.match(r.stdout, /header badge: \*\*v0\.14\.8\*\*/);
});

test('site-version accepts a site ahead of npm only with the version on main or next', async (t) => {
  // The docs merged before publish.yml finished: main is at 0.15.0, npm latest
  // is still 0.14.9 (published 3 days ago, so no grace applies).
  const site = (v) => `<script>{"softwareVersion": "${v}"}</script><span class="ver">v${v}</span>`;
  let state = makeState({ site: site('0.15.0') });
  let r = await run(t, state, ['--checks', 'site-version', '--dry-run']);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /PASS\s+site-version\s+.* shows v0\.15\.0, ahead of npm latest \(0\.14\.9\)/);

  state = makeState({ site: site('0.15.1'), distTags: { latest: '0.14.9', next: '0.15.1' } });
  r = await run(t, state, ['--checks', 'site-version', '--dry-run']);
  assert.match(r.stdout, /PASS\s+site-version\s+.* shows v0\.15\.1, ahead/);

  // A version that is neither latest, next nor main's is still reported.
  state = makeState({ site: site('0.15.2') });
  r = await run(t, state, ['--checks', 'site-version', '--dry-run']);
  assert.match(r.stdout, /FAIL\s+site-version\s+site-version: .* does not show npm latest \(0\.14\.9\)/);
  assert.match(r.stdout, /JSON-LD `softwareVersion`: \*\*0\.15\.2\*\*/);
});

test('labels, runner labels and bot PRs read the checkout and the API', async (t) => {
  const old = iso(NOW - 5 * 86400);
  const state = makeState({
    labels: ['Dependencies', 'npm', 'sentinel'],
    pulls: [
      { number: 81, title: 'Bump actions', html_url: 'https://gh/pr/81', created_at: old, user: { login: 'dependabot[bot]' }, head: { sha: 'b1' } },
      { number: 82, title: 'Bump rustls', html_url: 'https://gh/pr/82', created_at: iso(NOW - 3600), user: { login: 'dependabot[bot]' }, head: { sha: 'b2' } },
      { number: 83, title: 'Human PR', html_url: 'https://gh/pr/83', created_at: old, user: { login: 'someone' }, head: { sha: 'b3' } },
    ],
    checkRuns: { b2: [{ name: 'Rust lint + test', conclusion: 'failure' }, { name: 'CodeQL', conclusion: 'success' }] },
  });
  const r = await run(t, state, ['--checks', 'labels,runner-labels,stale-bot-prs', '--dry-run']);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /FAIL\s+labels\s+labels: Dependabot labels missing: rust, tooling/);
  assert.match(r.stdout, /runner-labels\/ubuntu-20\.04: .*\n(.*\n)*.*ci\.yml:9: `runs-on: ubuntu-20\.04`/);
  assert.match(r.stdout, /runner-labels\/macos-14-large/);
  assert.doesNotMatch(r.stdout, /runner-labels\/macos-14:/, 'macos-14 appears only in a comment and as a prefix');
  assert.match(r.stdout, /stale-bot-prs: 2 Dependabot PRs need attention/);
  assert.match(r.stdout, /#81.*open 5 days/);
  assert.match(r.stdout, /#82.*failing checks: Rust lint \+ test/);
  assert.doesNotMatch(r.stdout, /#83/);
});

test('labels dispatches labels.yml once for the labels it lists, and names the rest', async (t) => {
  const state = makeState({
    labels: ['dependencies', 'npm', 'sentinel'],
    files: { '.github/workflows/labels.yml': 'on:\n  workflow_dispatch:\njobs:\n  sync:\n    steps:\n      - run: |\n          create() {\n            gh label create "$1" --force\n          }\n          create dependencies 0366d6 "Deps"\n          create Rust dea584 "Rust (cargo) dependencies"\n' },
  });
  let r = await run(t, state, ['--checks', 'labels']);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /FAIL\s+labels\s+labels: Dependabot labels missing: rust, tooling/);
  assert.match(r.stdout, /dispatches \.github\/workflows\/labels\.yml on main once for this commit, which creates \*\*rust\*\*\. Add \*\*tooling\*\* to/);
  assert.deepEqual(state.dispatches, [{ workflow: 'labels.yml', ref: 'main' }]);
  assert.match(state.issues[0].body, new RegExp(`dispatched="labels\\.yml:${SHA}@`));
  r = await run(t, state, ['--checks', 'labels']);
  assert.equal(state.dispatches.length, 1, 'once per commit');
  assert.match(r.stdout, /labels\.yml was already dispatched/);

  // Nothing labels.yml lists is missing: no dispatch, and the fix is a PR.
  const other = makeState({ labels: ['dependencies', 'npm', 'rust', 'sentinel'], files: state.files });
  r = await run(t, other, ['--checks', 'labels']);
  assert.match(r.stdout, /\n\s+Add \*\*tooling\*\* to \.github\/workflows\/labels\.yml; once that is on main, the sentinel dispatches it\./);
  assert.deepEqual(other.dispatches, []);
});

test('main-red dispatches ci.yml once when HEAD has no run, and ignores [skip ci] commits', async (t) => {
  const state = makeState({ ciRunsForHead: 0 });
  let r = await run(t, state, ['--checks', 'main-red']);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /FAIL\s+main-red\s+main-red\/no-run: ci\.yml never ran for main aaaaaaa/);
  assert.match(r.stdout, /The sentinel dispatches ci\.yml on main once for this commit/);
  assert.deepEqual(state.dispatches, [{ workflow: 'ci.yml', ref: 'main' }]);
  assert.match(state.issues[0].body, new RegExp(`sig="main-red/no-run" .*dispatched="ci\\.yml:${SHA}@`));
  assert.match(state.issues[0].body, /The sentinel dispatched ci\.yml for `aaaaaaa`/);

  r = await run(t, state, ['--checks', 'main-red']);
  assert.equal(state.dispatches.length, 1, 'not dispatched twice for one commit');
  assert.equal(state.issues.length, 1);

  // publish.yml's manual bump commits "chore(release): vX [skip ci]": no run is expected.
  const skip = makeState({ ciRunsForHead: 0, head: { sha: SHA, date: iso(NOW - 3600), message: 'chore(release): v0.15.1 [skip ci]' } });
  r = await run(t, skip, ['--checks', 'main-red']);
  assert.match(r.stdout, /PASS\s+main-red\s+ci\.yml run 7 on aaaaaaa: success/);
  assert.equal(skip.writes.length, 0);
});

test('main-red reads push and dispatched runs, not a fork PR from a branch named main', async (t) => {
  const state = makeState({
    ciRuns: [
      { id: 13, run_number: 9, head_sha: 'f'.repeat(40), event: 'pull_request', status: 'completed', conclusion: 'failure', run_attempt: 1, html_url: 'u' },
      { id: 12, run_number: 8, head_sha: SHA, event: 'workflow_dispatch', status: 'completed', conclusion: 'success', run_attempt: 1, html_url: 'u' },
    ],
  });
  const r = await run(t, state, ['--checks', 'main-red', '--dry-run']);
  assert.match(r.stdout, /PASS\s+main-red\s+ci\.yml run 8 on aaaaaaa: success/);
});

const BOT = { login: 'github-actions[bot]' };
const releaseOk = (extra = {}) => ({
  run_number: 30, status: 'completed', conclusion: 'success', event: 'push', run_attempt: 1,
  html_url: 'https://gh/release/30', created_at: iso(NOW - 3600), updated_at: iso(NOW - 40 * 60), ...extra,
});

test('after the sentinel dispatched release.yml, it dispatches publish.yml itself and keeps one issue open', async (t) => {
  // Run 1 found no release run: one issue, release.yml dispatched.
  const state = makeState();
  await run(t, state, ['--checks', 'release-gap']);
  assert.equal(state.issues.length, 1);

  // The dispatched release is running: still the same failing issue, no pass tick.
  state.releaseRuns = [{ run_number: 30, status: 'in_progress', conclusion: null, event: 'workflow_dispatch', triggering_actor: BOT, html_url: 'https://gh/release/30' }];
  let r = await run(t, state, ['--checks', 'release-gap']);
  assert.match(r.stdout, /FAIL\s+release-gap\s+release-gap: v0\.15\.0 is not on npm yet: release\.yml is running/);
  assert.match(state.issues[0].body, /streak="0"/);

  // It succeeded 2 minutes ago. workflow_run never fires for a GITHUB_TOKEN
  // dispatch, so the sentinel dispatches publish.yml at once, with no bump.
  state.releaseRuns = [releaseOk({ event: 'workflow_dispatch', triggering_actor: BOT, updated_at: iso(NOW - 120) })];
  r = await run(t, state, ['--checks', 'release-gap']);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /release\.yml succeeded, publish\.yml has not run/);
  assert.deepEqual(state.dispatches.map((d) => d.workflow), ['release.yml', 'publish.yml']);
  assert.deepEqual(state.dispatches[1], { workflow: 'publish.yml', ref: 'main', inputs: { version_bump: 'none' } });
  assert.equal(state.issues.length, 1, 'the publish dispatch is recorded on the same issue');
  assert.match(state.issues[0].body, new RegExp(`dispatched="release\\.yml:${SHA}@\\S+ publish\\.yml:${SHA}@\\S+"`));
  assert.match(state.comments.at(-1).body, /Dispatched publish\.yml on main/);

  // Next run: publish.yml has not shown up yet. No second dispatch.
  r = await run(t, state, ['--checks', 'release-gap']);
  assert.equal(state.dispatches.length, 2);
  assert.match(r.stdout, /publish\.yml was already dispatched/);

  // publish.yml running, then npm has it: the issue starts its pass streak.
  state.publishRuns = [{ run_number: 50, status: 'in_progress', conclusion: null, event: 'workflow_dispatch', created_at: iso(NOW - 60), html_url: 'https://gh/publish/50' }];
  r = await run(t, state, ['--checks', 'release-gap']);
  assert.match(r.stdout, /FAIL\s+release-gap\s+release-gap: v0\.15\.0 is not on npm yet: publish\.yml is running/);
  state.distTags = { latest: '0.15.0' };
  await run(t, state, ['--checks', 'release-gap']);
  assert.match(state.issues[0].body, /streak="1"/);
  assert.equal(state.dispatches.length, 2);
});

test('a push release whose publish.yml never starts gets publish.yml dispatched after 20 minutes', async (t) => {
  let state = makeState({ releaseRuns: [releaseOk({ updated_at: iso(NOW - 5 * 60) })] });
  let r = await run(t, state, ['--checks', 'release-gap']);
  assert.match(r.stdout, /PASS\s+release-gap\s+release\.yml finished for v0\.15\.0 5 min ago; publish\.yml has 20 min/);
  assert.equal(state.writes.length, 0);

  state = makeState({ releaseRuns: [releaseOk()] });
  r = await run(t, state, ['--checks', 'release-gap']);
  assert.match(r.stdout, /FAIL\s+release-gap\s+release-gap\/unpublished: v0\.15\.0 was released but publish\.yml never started/);
  assert.deepEqual(state.dispatches, [{ workflow: 'publish.yml', ref: 'main', inputs: { version_bump: 'none' } }]);

  // A publish.yml run that chained normally and is running is fine.
  state = makeState({ releaseRuns: [releaseOk()], publishRuns: [{ status: 'in_progress', conclusion: null, event: 'workflow_run', created_at: iso(NOW - 39 * 60) }] });
  r = await run(t, state, ['--checks', 'release-gap']);
  assert.match(r.stdout, /PASS\s+release-gap\s+publish\.yml is running for v0\.15\.0/);

  // A failed publish is reported, never re-dispatched. A run from before the
  // release started does not count.
  state = makeState({
    releaseRuns: [releaseOk()],
    publishRuns: [
      { run_number: 51, status: 'completed', conclusion: 'failure', event: 'workflow_run', run_attempt: 1, created_at: iso(NOW - 39 * 60), html_url: 'https://gh/publish/51' },
      { run_number: 49, status: 'completed', conclusion: 'success', event: 'workflow_run', run_attempt: 1, created_at: iso(NOW - 2 * 3600), html_url: 'u' },
    ],
  });
  r = await run(t, state, ['--checks', 'release-gap']);
  assert.match(r.stdout, /FAIL\s+release-gap\s+release-gap\/unpublished: publish\.yml failed for v0\.15\.0/);
  assert.match(r.stdout, /\[workflow_run run, attempt 1\]\(https:\/\/gh\/publish\/51\) concluded \*\*failure\*\*/);
  assert.equal(state.dispatches.length, 0);
});

test('a transient API error is retried once, then reported without failing the run', async (t) => {
  // Two 502s on the jobs call (curl retries once by itself): the check's
  // retry succeeds and the result is normal.
  const failing = {
    ciRuns: [{ id: 11, run_number: 7, head_sha: SHA, event: 'push', status: 'completed', conclusion: 'failure', run_attempt: 1, html_url: 'u' }],
    ciJobs: [{ name: 'Cargo audit', conclusion: 'failure', html_url: 'https://gh/job/1' }],
  };
  let state = makeState({ ...failing, broken: [{ match: '/jobs', code: 502, times: 2 }] });
  let r = await run(t, state, ['--checks', 'main-red']);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /RETRY main-red in 0s: GET repos\/o\/r\/actions\/runs\/11\/jobs.*HTTP 502/);
  assert.match(r.stdout, /FAIL\s+main-red\s+main-red: CI is red on main/);
  assert.equal(state.issues.length, 1);

  // It keeps failing: an ERROR row, no issue changes for that check, exit 0.
  // The partial main-red/no-run result from the dead attempts is dropped.
  state = makeState({ ...failing, ciRunsForHead: 0, broken: ['/jobs'], issues: [sentinelIssue(9, 'main-red', 'main-red', 1)] });
  r = await run(t, state, ['--checks', 'main-red,site-version']);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /ERROR\s+main-red\s+transient, retried once: GET repos\/o\/r\/actions\/runs\/11\/jobs\S* HTTP 500 boom/);
  assert.match(r.stdout, /::warning::sentinel: main-red: transient/);
  assert.doesNotMatch(r.stdout, /FAIL\s+main-red/);
  assert.match(r.stdout, /PASS\s+site-version/);
  assert.match(state.issues[0].body, /streak="1"/, 'no pass tick for a check that did not run');
  assert.equal(state.dispatches.length, 0, 'no dispatch from a check that did not finish');
  assert.equal(state.writes.length, 0);

  // The issue list or an issue write failing is logged; the run still exits 0.
  state = makeState({ ...failing, broken: [{ match: '/issues', code: 503, method: 'GET' }] });
  r = await run(t, state, ['--checks', 'main-red']);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /could not list sentinel issues, so no issue changes this run/);
  assert.equal(state.writes.length, 0);

  state = makeState({ ...failing, broken: [{ match: '/issues', code: 503, method: 'POST' }] });
  r = await run(t, state, ['--checks', 'main-red']);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /FAILED \(transient\): open issue "sentinel: CI is red on main": POST repos\/o\/r\/issues: HTTP 503/);
  assert.equal(state.writes.filter((w) => w.path.endsWith('/issues')).length, 1, 'a write is not retried');
});

test('only a sentinel bug fails the run: a request GitHub rejects, or a crash', async (t) => {
  let state = makeState({ broken: [{ match: '/actions/workflows/ci.yml/runs', code: 404 }] });
  let r = await run(t, state, ['--checks', 'main-red,site-version']);
  assert.equal(r.code, 1);
  assert.match(r.stdout, /ERROR\s+main-red\s+sentinel bug: exit 1: GET repos\/o\/r\/actions\/workflows\/ci\.yml\/runs\S* HTTP 404 boom/);
  assert.doesNotMatch(r.stdout, /RETRY/, 'a 4xx is not retried');
  assert.match(r.stdout, /PASS\s+site-version/);

  // So does a write GitHub rejects.
  state = makeState({ ciRunsForHead: 0, broken: [{ match: '/dispatches', code: 422 }] });
  r = await run(t, state, ['--checks', 'main-red']);
  assert.equal(r.code, 1);
  assert.match(r.stdout, /FAILED \(sentinel bug\): dispatch ci\.yml on main: POST .*HTTP 422/);
  assert.doesNotMatch(state.issues[0].body, /dispatched="ci/, 'a failed dispatch is not recorded, so the next run tries again');

  // Too many requests is a 4xx too, but transient.
  state = makeState({ broken: [{ match: '/actions/workflows/ci.yml/runs', code: 429 }] });
  r = await run(t, state, ['--checks', 'main-red']);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /ERROR\s+main-red\s+transient, retried once: .*HTTP 429/);

  // Data the check cannot handle (package.json without a version).
  state = makeState({ version: undefined });
  r = await run(t, state, ['--checks', 'release-gap']);
  assert.equal(r.code, 1);
  assert.match(r.stdout, /ERROR\s+release-gap\s+sentinel bug: exit/);
  assert.match(r.stdout, /::error::sentinel: 1 error/);
});
