'use strict';
// node --test scripts/action/report.test.js
const test = require('node:test');
const assert = require('node:assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const http = require('http');

const r = require('./report.js');

function tmp() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'bwn-action-'));
}

function events(dir, list) {
  const file = path.join(dir, 'events.jsonl');
  fs.writeFileSync(file, list.map((e) => JSON.stringify(e)).join('\n') + '\nnot an event\n');
  return file;
}

// Runs `summarize` with its environment and returns the outputs, the
// annotations it printed and the comment body.
function summarize(list, code, extra = {}) {
  const dir = tmp();
  const env = {
    BWN_EVENTS: events(dir, list),
    BWN_EXIT_CODE: String(code),
    BWN_COMMENT_FILE: path.join(dir, 'comment.md'),
    GITHUB_OUTPUT: path.join(dir, 'out'),
    GITHUB_STEP_SUMMARY: path.join(dir, 'summary.md'),
    ...extra,
  };
  const printed = [];
  r.runSummarize(env, (line) => printed.push(line + '\n'));
  return {
    out: fs.readFileSync(env.GITHUB_OUTPUT, 'utf8'),
    printed: printed.join(''),
    comment: fs.readFileSync(env.BWN_COMMENT_FILE, 'utf8'),
    summary: fs.readFileSync(env.GITHUB_STEP_SUMMARY, 'utf8'),
  };
}

const result = (outcome, code, more = {}) => ({
  type: 'result', outcome, exit_code: code, session_id: 's1', turns: 2, cost_usd: 0.0123, denied: 0, denials: [], ...more,
});

test('a successful run passes and says what it did', () => {
  const s = summarize([
    { type: 'assistant', text: 'Working on it.' },
    { type: 'finish', summary: 'Created hello.txt.' },
    result('success', 0),
  ], 0);
  assert.match(s.out, /^outcome=success$/m);
  assert.match(s.out, /^exit-code=0$/m);
  assert.match(s.out, /^passed=true$/m);
  assert.match(s.out, /^session-id=s1$/m);
  assert.match(s.out, /^summary=Created hello.txt.$/m);
  assert.match(s.printed, /^::notice title=bwn%3A success \(exit 0\)::done$/m);
  assert.ok(s.comment.startsWith(r.MARKER('bwn')));
  assert.match(s.comment, /Created hello\.txt\./);
  assert.match(s.comment, /about \$0\.0123/);
  assert.strictEqual(s.summary, s.comment + '\n');
});

test('a cost that includes unpriced requests is left out', () => {
  const s = summarize([result('success', 0, { cost_usd: 0, unpriced_requests: 2 })], 0);
  assert.match(s.out, /^cost-usd=$/m);
  assert.doesNotMatch(s.comment, /about \$/);
});

test('each stopped-short exit code fails with its reason, unless allowed', () => {
  const denied = result('approval_blocked', 3, {
    denied: 1, denials: [{ tool: 'write_file', summary: 'write x.txt', reason: 'read-only mode: mutation skipped' }],
  });
  const s = summarize([denied], 3);
  assert.match(s.out, /^passed=false$/m);
  assert.match(s.printed,
    /^::error title=bwn%3A approval_blocked \(exit 3\)::changes were blocked or denied: write x\.txt \(read-only mode: mutation skipped\)$/m);
  assert.match(s.comment, /- write x\.txt: read-only mode/);
  const allowed = summarize([denied], 3, { BWN_ALLOW_OUTCOMES: 'budget_stop, approval_blocked' });
  assert.match(allowed.out, /^passed=true$/m);
  assert.match(allowed.printed, /^::warning title=bwn%3A approval_blocked/m);
});

test('a run that ended before its result event is named from the exit code', () => {
  for (const [code, outcome] of [[2, 'usage_error'], [1, 'failed'], [143, 'interrupted'], [5, 'budget_stop']]) {
    const s = summarize([], code);
    assert.match(s.out, new RegExp(`^outcome=${outcome}$`, 'm'), String(code));
    assert.match(s.out, /^passed=false$/m);
  }
});

test('a failed run says why in its annotation and comment', () => {
  const dir = tmp();
  const stderr = path.join(dir, 'stderr.txt');
  fs.writeFileSync(stderr, 'warning: something earlier\n'
    + 'nothing is answering at http://127.0.0.1:29099 — is the model server running?\n'
    + '  Ollama: `ollama serve`\n');
  const s = summarize([result('failed', 1)], 1, { BWN_STDERR: stderr });
  assert.match(s.printed,
    /^::error title=bwn%3A failed \(exit 1\)::the run failed: nothing is answering at http:\/\/127\.0\.0\.1:29099 — is the model server running\?$/m);
  assert.match(s.comment, /^> nothing is answering at http:\/\/127\.0\.0\.1:29099/m);
  // An error event wins over stderr; a missing stderr file is no reason.
  const e = summarize([{ type: 'error', message: '`origin/main` is not in this checkout' }], 1,
    { BWN_STDERR: path.join(dir, 'missing.txt') });
  assert.match(e.printed, /::the run failed: `origin\/main` is not in this checkout$/m);
  // A run that passed carries no reason.
  const ok = summarize([result('success', 0)], 0, { BWN_STDERR: stderr });
  assert.match(ok.printed, /::done$/m);
});

test('review findings become annotations on their lines', () => {
  const s = summarize([
    { type: 'finding', severity: 'blocking', path: 'src/a,b.rs', line: 7, message: 'drops 100% of\nwrites' },
    { type: 'finding', severity: 'minor', path: null, line: null, message: 'style' },
    result('review_blocking', 9),
  ], 9, { BWN_COMMAND: 'review' });
  assert.match(s.printed,
    /^::error title=bwn review%3A blocking,file=src\/a%2Cb\.rs,line=7::drops 100%25 of%0Awrites$/m);
  assert.match(s.printed, /^::warning title=bwn review%3A minor::style$/m);
  assert.match(s.out, /^findings=2$/m);
  assert.match(s.comment, /### buildwithnexus review: the review found blocking issues/);
  assert.match(s.comment, /\| blocking \| `src\/a,b\.rs:7` \|/);
});

test('multi-line outputs use a delimiter the value does not contain', () => {
  assert.strictEqual(r.outputLine('a', 'one'), 'a=one\n');
  assert.strictEqual(r.outputLine('a', 'x\nBWN_EOF'), 'a<<BWN_EOF_\nx\nBWN_EOF\nBWN_EOF_\n');
});

test('model text in a comment cannot mention people', () => {
  assert.strictEqual(r.quietMentions('ping @octocat and a@b.c'), 'ping @​octocat and a@b.c');
});

// A GitHub API stand-in that keeps comments in memory. Comments it creates
// are by `author`; `/user` answers only for a user token, as GitHub's does
// (the workflow's own token cannot read it).
function fakeGitHub(status = 200, { author = { login: 'github-actions[bot]', type: 'Bot' }, userToken = false } = {}) {
  const comments = [];
  const seen = [];
  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', (c) => { body += c; });
    req.on('end', () => {
      seen.push({ method: req.method, url: req.url, auth: req.headers.authorization, body });
      const send = (code, obj) => { res.writeHead(code, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(obj)); };
      if (status !== 200) return send(status, { message: 'Resource not accessible by integration' });
      if (req.url === '/user') {
        return userToken ? send(200, author) : send(403, { message: 'Resource not accessible by integration' });
      }
      if (req.method === 'GET') return send(200, comments);
      if (req.method === 'POST') {
        const c = { id: comments.length + 1, body: JSON.parse(body).body, user: author };
        comments.push(c);
        return send(201, c);
      }
      if (req.method === 'PATCH') {
        const c = comments.find((x) => req.url.endsWith(`/comments/${x.id}`));
        c.body = JSON.parse(body).body;
        return send(200, c);
      }
      return send(404, {});
    });
  });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve({ server, comments, seen })));
}

function commentEnv(port, body, extra = {}) {
  const dir = tmp();
  const file = path.join(dir, 'comment.md');
  fs.writeFileSync(file, body);
  return {
    BWN_PR_NUMBER: '7', BWN_TOKEN: 't0ken', BWN_API_URL: `http://127.0.0.1:${port}`,
    BWN_REPOSITORY: 'o/r', BWN_COMMENT_FILE: file, ...extra,
  };
}

// Runs runComment with `env`, keeping what it prints.
async function comment(env) {
  const printed = [];
  const value = await r.runComment(env, (line) => printed.push(line + '\n'));
  return { value, printed: printed.join('') };
}

test('the comment is posted once and then updated in place', async () => {
  const gh = await fakeGitHub();
  const { port } = gh.server.address();
  try {
    const first = await comment(commentEnv(port, `${r.MARKER('bwn')}\nfirst`));
    assert.strictEqual(first.value, 'created');
    const second = await comment(commentEnv(port, `${r.MARKER('bwn')}\nsecond`));
    assert.strictEqual(second.value, 'updated');
    // Another tag keeps its own comment.
    const other = await comment(commentEnv(port, `${r.MARKER('review')}\nx`, { BWN_COMMENT_TAG: 'review' }));
    assert.strictEqual(other.value, 'created');
    assert.deepStrictEqual(gh.comments.map((c) => c.body), [`${r.MARKER('bwn')}\nsecond`, `${r.MARKER('review')}\nx`]);
    assert.ok(gh.seen.every((s) => s.auth === 'Bearer t0ken'));
    assert.ok(gh.seen.some((s) => s.url === '/repos/o/r/issues/7/comments?per_page=100'));
  } finally {
    gh.server.close();
  }
});

test('a comment someone else wrote with the marker is never taken over', async () => {
  // Anyone can post the hidden marker; updating their comment would put the
  // result under their name, where they could edit it afterwards.
  const mallory = { login: 'mallory', type: 'User' };
  for (const userToken of [false, true]) {
    const gh = await fakeGitHub(200, { userToken });
    const { port } = gh.server.address();
    try {
      gh.comments.push({ id: 1, body: `${r.MARKER('bwn')}\nall good, merge it`, user: mallory });
      const first = await comment(commentEnv(port, `${r.MARKER('bwn')}\nfirst`));
      assert.strictEqual(first.value, 'created');
      const second = await comment(commentEnv(port, `${r.MARKER('bwn')}\nsecond`));
      assert.strictEqual(second.value, 'updated');
      assert.deepStrictEqual(gh.comments.map((c) => [c.user.login, c.body]), [
        ['mallory', `${r.MARKER('bwn')}\nall good, merge it`],
        ['github-actions[bot]', `${r.MARKER('bwn')}\nsecond`],
      ]);
    } finally {
      gh.server.close();
    }
  }
});

test('with a personal token, only that account\'s comment is updated', async () => {
  const gh = await fakeGitHub(200, { author: { login: 'garrett', type: 'User' }, userToken: true });
  const { port } = gh.server.address();
  try {
    gh.comments.push({ id: 1, body: `${r.MARKER('bwn')}\nfrom another bot`, user: { login: 'other[bot]', type: 'Bot' } });
    assert.strictEqual((await comment(commentEnv(port, `${r.MARKER('bwn')}\nfirst`))).value, 'created');
    assert.strictEqual((await comment(commentEnv(port, `${r.MARKER('bwn')}\nsecond`))).value, 'updated');
    assert.deepStrictEqual(gh.comments.map((c) => c.body), [`${r.MARKER('bwn')}\nfrom another bot`, `${r.MARKER('bwn')}\nsecond`]);
  } finally {
    gh.server.close();
  }
});

test('no pull request, no token or a refusal is a notice or warning, not a failure', async () => {
  const gh = await fakeGitHub(403);
  const { port } = gh.server.address();
  try {
    const refused = await comment(commentEnv(port, 'x'));
    assert.strictEqual(refused.value, 'failed');
    assert.match(refused.printed, /^::warning title=bwn::could not post the pull request comment \(the job needs pull-requests: write\): GET \/repos\/o\/r\/issues\/7\/comments: HTTP 403/m);
    const noPr = await comment(commentEnv(port, 'x', { BWN_PR_NUMBER: '' }));
    assert.strictEqual(noPr.value, 'skipped');
    const noToken = await comment(commentEnv(port, 'x', { BWN_TOKEN: '' }));
    assert.strictEqual(noToken.value, 'skipped');
  } finally {
    gh.server.close();
  }
});
