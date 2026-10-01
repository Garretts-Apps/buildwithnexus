'use strict';
// The GitHub Action's reporting half (action.yml runs bwn, then this):
//   node report.js summarize   outcome, outputs, annotations, step summary
//   node report.js comment     post or update the pull request comment
// Node builtins only, like the launcher. Inputs arrive as environment
// variables, never interpolated into a shell script.
const fs = require('fs');

// The result event names the outcome; these are the headless exit codes
// (README "Headless and CI"). A run that ended before its result event
// (usage error, no provider) is named from its exit code.
const OUTCOMES = {
  success: 'done',
  failed: 'the run failed',
  usage_error: 'usage error (see the step log)',
  approval_blocked: 'changes were blocked or denied',
  hook_blocked: 'a hook blocked the task',
  budget_stop: 'stopped at the budget limit',
  step_limit: 'ran out of steps',
  check_work_failed: "finished, but the project's checks still fail",
  verification_failed: 'finished, but verification still fails',
  review_blocking: 'the review found blocking issues',
  interrupted: 'interrupted',
};

const BY_EXIT_CODE = {
  0: 'success', 1: 'failed', 2: 'usage_error', 3: 'approval_blocked', 4: 'hook_blocked',
  5: 'budget_stop', 6: 'step_limit', 7: 'check_work_failed', 8: 'verification_failed',
  9: 'review_blocking', 130: 'interrupted', 143: 'interrupted',
};

const MARKER = (tag) => `<!-- bwn-action:${tag} -->`;
const MAX_COMMENT = 60000;

function readEvents(file) {
  let text = '';
  try {
    text = fs.readFileSync(file, 'utf8');
  } catch {
    return [];
  }
  const events = [];
  for (const line of text.split('\n')) {
    try {
      const v = JSON.parse(line);
      if (v && typeof v === 'object') events.push(v);
    } catch {
      // Not an event line.
    }
  }
  return events;
}

// Why a failed run failed: its last error event, or the last unindented
// line bwn wrote to stderr (indented lines are hints under it).
function failureReason(events, stderr) {
  const err = [...events].reverse().find((e) => e.type === 'error' && String(e.message || '').trim());
  if (err) return String(err.message).trim();
  const lines = String(stderr || '').split('\n').filter((l) => l.trim() && !/^\s/.test(l));
  return lines.length ? lines[lines.length - 1].trim().slice(0, 500) : '';
}

// What the run came to, from its events, exit code and stderr.
function summarize(events, exitCode, allowed, stderr = '') {
  const result = [...events].reverse().find((e) => e.type === 'result') || null;
  const outcome = (result && result.outcome) || BY_EXIT_CODE[exitCode] || 'failed';
  const finish = [...events].reverse().find((e) => e.type === 'finish');
  const reply = [...events].reverse().find((e) => e.type === 'assistant' && String(e.text || '').trim());
  const findings = events.filter((e) => e.type === 'finding');
  const ok = outcome === 'success' || allowed.includes(outcome);
  return {
    outcome,
    label: OUTCOMES[outcome] || outcome,
    exitCode,
    passed: ok,
    sessionId: (result && result.session_id) || '',
    // Requests to a model with no known price are counted, never guessed.
    costUsd: result && typeof result.cost_usd === 'number' && !(result.unpriced_requests > 0)
      ? result.cost_usd : null,
    turns: result && typeof result.turns === 'number' ? result.turns : null,
    denials: (result && Array.isArray(result.denials)) ? result.denials : [],
    summary: finish ? String(finish.summary || '') : reply ? String(reply.text) : '',
    findings,
    reason: ok ? '' : failureReason(events, stderr),
  };
}

// Workflow command escaping, as @actions/core does it.
function escapeData(s) {
  return String(s).replace(/%/g, '%25').replace(/\r/g, '%0D').replace(/\n/g, '%0A');
}

function escapeProperty(s) {
  return escapeData(s).replace(/:/g, '%3A').replace(/,/g, '%2C');
}

function annotation(level, message, props = {}) {
  const p = Object.entries(props)
    .filter(([, v]) => v !== undefined && v !== null && v !== '')
    .map(([k, v]) => `${k}=${escapeProperty(v)}`)
    .join(',');
  return `::${level}${p ? ' ' + p : ''}::${escapeData(message)}`;
}

// One annotation for the outcome, one per finding (blocking ones as errors).
function annotations(r) {
  const out = [];
  for (const f of r.findings) {
    const level = f.severity === 'blocking' ? 'error' : f.severity === 'nit' ? 'notice' : 'warning';
    out.push(annotation(level, f.message, {
      title: `bwn review: ${f.severity}`, file: f.path, line: f.line,
    }));
  }
  let detail = r.label;
  if (r.denials.length) {
    detail += ': ' + r.denials.map((d) => `${d.summary} (${d.reason})`).join('; ');
  } else if (r.reason) {
    detail += ': ' + r.reason;
  }
  const level = r.outcome === 'success' ? 'notice' : r.passed ? 'warning' : 'error';
  out.push(annotation(level, detail, { title: `bwn: ${r.outcome} (exit ${r.exitCode})` }));
  return out;
}

// Model text in a comment must not ping people.
function quietMentions(s) {
  return s.replace(/(^|[^\w`])@(?=[A-Za-z0-9])/g, '$1@​');
}

function markdown(r, { command, artifact, tag }) {
  const lines = [MARKER(tag), `### buildwithnexus ${command}: ${r.label}`, ''];
  if (r.summary.trim()) lines.push(quietMentions(r.summary.trim()), '');
  if (r.reason && !r.denials.length) lines.push('> ' + quietMentions(r.reason), '');
  if (r.findings.length) {
    lines.push('| severity | where | finding |', '|---|---|---|');
    for (const f of r.findings) {
      const where = f.path ? `\`${f.path}${f.line ? ':' + f.line : ''}\`` : '';
      lines.push(`| ${f.severity} | ${where} | ${quietMentions(String(f.message)).replace(/\|/g, '\\|')} |`);
    }
    lines.push('');
  }
  if (r.denials.length) {
    lines.push('Refused calls:', ...r.denials.map((d) => `- ${d.summary}: ${d.reason}`), '');
  }
  const facts = [`outcome \`${r.outcome}\``, `exit code ${r.exitCode}`];
  if (r.costUsd !== null) facts.push(`about $${r.costUsd.toFixed(4)}`);
  if (r.turns !== null) facts.push(`${r.turns} model request${r.turns === 1 ? '' : 's'}`);
  if (artifact) facts.push(`events: artifact \`${artifact}\``);
  lines.push(`<sub>${facts.join(' · ')}</sub>`);
  let body = lines.join('\n');
  if (body.length > MAX_COMMENT) body = body.slice(0, MAX_COMMENT) + '\n\n… (cut)';
  return body;
}

// `name<<delimiter` form, so multi-line values survive.
function outputLine(name, value) {
  const v = String(value);
  if (!v.includes('\n')) return `${name}=${v}\n`;
  let delim = 'BWN_EOF';
  while (v.includes(delim)) delim += '_';
  return `${name}<<${delim}\n${v}\n${delim}\n`;
}

const stdout = (line) => process.stdout.write(line + '\n');

function runSummarize(env, print = stdout) {
  const exitCode = Number.parseInt(env.BWN_EXIT_CODE || '1', 10);
  const allowed = (env.BWN_ALLOW_OUTCOMES || '').split(/[\s,]+/).filter(Boolean);
  let stderr = '';
  try {
    stderr = env.BWN_STDERR ? fs.readFileSync(env.BWN_STDERR, 'utf8') : '';
  } catch {
    // No stderr file: the reason comes from the events alone.
  }
  const r = summarize(readEvents(env.BWN_EVENTS || ''), exitCode, allowed, stderr);
  for (const a of annotations(r)) print(a);
  const body = markdown(r, {
    command: env.BWN_COMMAND || 'run', artifact: env.BWN_ARTIFACT || '', tag: env.BWN_COMMENT_TAG || 'bwn',
  });
  if (env.BWN_COMMENT_FILE) fs.writeFileSync(env.BWN_COMMENT_FILE, body);
  if (env.GITHUB_STEP_SUMMARY) fs.appendFileSync(env.GITHUB_STEP_SUMMARY, body + '\n');
  if (env.GITHUB_OUTPUT) {
    fs.appendFileSync(env.GITHUB_OUTPUT, [
      outputLine('outcome', r.outcome),
      outputLine('exit-code', r.exitCode),
      outputLine('passed', r.passed),
      outputLine('session-id', r.sessionId),
      outputLine('cost-usd', r.costUsd === null ? '' : r.costUsd),
      outputLine('findings', r.findings.length),
      outputLine('summary', r.summary),
    ].join(''));
  }
  return r;
}

async function api(url, method, token, body) {
  const res = await fetch(url, {
    method,
    headers: {
      Accept: 'application/vnd.github+json',
      Authorization: `Bearer ${token}`,
      'X-GitHub-Api-Version': '2022-11-28',
      ...(body ? { 'Content-Type': 'application/json' } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!res.ok) {
    const text = await res.text().catch(() => '');
    throw new Error(`${method} ${new URL(url).pathname}: HTTP ${res.status} ${text.slice(0, 200)}`);
  }
  return res.status === 204 ? null : res.json();
}

// The account a user token belongs to. The workflow's own token and app
// tokens cannot read /user: null, and their comments are by a bot.
async function tokenLogin(root, token) {
  try {
    const user = await api(`${root}/user`, 'GET', token);
    return (user && typeof user.login === 'string' && user.login) || null;
  } catch {
    return null;
  }
}

function isAuthor(user, login) {
  if (!user) return false;
  return login ? user.login === login : user.type === 'Bot';
}

// Updates this action's earlier comment on the pull request, or adds one.
// Returns what it did; a failure is a warning, never a failed step.
async function runComment(env, print = stdout) {
  const pr = String(env.BWN_PR_NUMBER || '').trim();
  if (!/^\d+$/.test(pr)) {
    print(annotation('notice', 'not a pull request, so no comment was posted', { title: 'bwn' }));
    return 'skipped';
  }
  if (!env.BWN_TOKEN) {
    print(annotation('warning', 'no github-token, so no comment was posted', { title: 'bwn' }));
    return 'skipped';
  }
  const root = (env.BWN_API_URL || 'https://api.github.com').replace(/\/$/, '');
  const base = `${root}/repos/${env.BWN_REPOSITORY}`;
  const body = fs.readFileSync(env.BWN_COMMENT_FILE, 'utf8');
  const marker = MARKER(env.BWN_COMMENT_TAG || 'bwn');
  try {
    const comments = await api(`${base}/issues/${pr}/comments?per_page=100`, 'GET', env.BWN_TOKEN);
    // Anyone can write the marker, so only a comment this token's account
    // wrote is ours: updating someone else's would put the result under
    // their name, for them to edit.
    const login = await tokenLogin(root, env.BWN_TOKEN);
    const mine = (comments || []).find((c) => typeof c.body === 'string' && c.body.startsWith(marker)
      && isAuthor(c.user, login));
    if (mine) {
      await api(`${base}/issues/comments/${mine.id}`, 'PATCH', env.BWN_TOKEN, { body });
      return 'updated';
    }
    await api(`${base}/issues/${pr}/comments`, 'POST', env.BWN_TOKEN, { body });
    return 'created';
  } catch (e) {
    print(annotation('warning',
      `could not post the pull request comment (the job needs pull-requests: write): ${e.message}`,
      { title: 'bwn' }));
    return 'failed';
  }
}

if (require.main === module) {
  const cmd = process.argv[2];
  if (cmd === 'summarize') {
    runSummarize(process.env);
  } else if (cmd === 'comment') {
    runComment(process.env).then((what) => stdout(`comment: ${what}`));
  } else {
    process.stderr.write('usage: report.js summarize|comment\n');
    process.exit(2);
  }
}

module.exports = {
  summarize, annotations, annotation, escapeData, escapeProperty, markdown, outputLine,
  quietMentions, readEvents, runSummarize, runComment, MARKER,
};
