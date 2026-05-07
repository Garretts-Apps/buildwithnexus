import * as readline from 'readline';
import chalk from 'chalk';
import { tui, type Mode } from './tui.js';
import { classifyIntent } from './intent-classifier.js';
import { hasAnyKey, loadApiKeys, validateBackendUrl } from '../core/config.js';
import { loadKeys } from '../core/secrets.js';
import { parseSSEStream } from '../core/sse-parser.js';
import { startBackend } from '../core/docker.js';
import { resolvedVersion } from '../core/version.js';
import { buildRunPayload, checkServerHealth } from '../core/api.js';

const appVersion = resolvedVersion;

// When the backend stops sending SSE events for this long, surface a hint to the
// user so a silent stall doesn't look like a frozen terminal. The 120s
// AbortSignal.timeout still bounds the total wait — this just prints a
// breadcrumb sooner.
const SSE_STALL_WARNING_MS = 8000;

async function reportBackendError(label: string, response: Response): Promise<void> {
  let bodySnippet = '';
  try {
    const text = await response.text();
    bodySnippet = text.slice(0, 300);
  } catch {
    bodySnippet = '(could not read response body)';
  }
  console.error(chalk.red(`${label}: HTTP ${response.status} ${response.statusText}`));
  if (bodySnippet) {
    console.error(chalk.gray(`  body: ${bodySnippet}`));
  }
  console.error(chalk.gray('  Tip: stream backend logs with `buildwithnexus logs -f`'));
}

export async function interactiveMode() {
  const backendUrl = process.env.BACKEND_URL || 'http://localhost:4200';

  // Validate backend URL security before transmitting API keys
  const urlCheck = validateBackendUrl(backendUrl);
  if (!urlCheck.valid) {
    console.error(chalk.red(`\n${urlCheck.error}`));
    process.exit(1);
  }

  // Load keys from ~/.buildwithnexus/.env.keys into process.env if not already set
  if (!hasAnyKey()) {
    try {
      const storedKeys = loadKeys();
      if (storedKeys) {
        if (storedKeys.ANTHROPIC_API_KEY) process.env.ANTHROPIC_API_KEY = storedKeys.ANTHROPIC_API_KEY;
        if (storedKeys.OPENAI_API_KEY) process.env.OPENAI_API_KEY = storedKeys.OPENAI_API_KEY;
        if (storedKeys.GOOGLE_API_KEY) process.env.GOOGLE_API_KEY = storedKeys.GOOGLE_API_KEY;
      }
    } catch {
      // tampered keys — will fall through to re-prompt below
    }
  }

  // Only prompt for API keys if none are configured yet
  if (!hasAnyKey()) {
    console.log(chalk.cyan('\n🔧 Configure API Keys\n'));
    const { deepAgentsInitCommand } = await import('./init-command.js');
    await deepAgentsInitCommand();

    // Verify keys are loaded after init
    if (!hasAnyKey()) {
      console.error('Error: At least one API key is required to use buildwithnexus.');
      process.exit(1);
    }
  }

  // Show configured keys
  const keys = loadApiKeys();
  console.log(chalk.green('\n✓ Keys configured!'));
  console.log(chalk.gray(`  Anthropic: ${keys.anthropic ? '✓' : '✗'}`));
  console.log(chalk.gray(`  OpenAI: ${keys.openai ? '✓' : '✗'}`));
  console.log(chalk.gray(`  Google: ${keys.google ? '✓' : '✗'}`));
  console.log(chalk.gray(`  (Run 'da-init' to reconfigure)\n`));

  // Check backend health; auto-start if not running
  async function waitForBackend(): Promise<boolean> {
    for (let i = 0; i < 5; i++) {
      await new Promise((resolve) => setTimeout(resolve, 2000));
      if (await checkServerHealth(backendUrl)) return true;
    }
    return false;
  }

  if (!(await checkServerHealth(backendUrl))) {
    console.log(chalk.yellow('⚠️  Backend not accessible, attempting to start...'));
    await startBackend();
    const ready = await waitForBackend();
    if (!ready) {
      console.error(chalk.red('❌ Backend failed to start. Run: buildwithnexus server'));
      process.exit(1);
    }
  }

  const rl = readline.createInterface({
    input: process.stdin,
    output: process.stdout,
  });

  const ask = (question: string): Promise<string> =>
    new Promise((resolve) => {
      rl.question(question, resolve);
    });

  console.clear();
  console.log(chalk.gray('Welcome! Describe what you want the AI agents to do.'));
  console.log(chalk.gray('Type "exit" to quit.\n'));

  while (true) {
    const task = await ask(chalk.bold.blue('📝 Task: '));

    if (task.toLowerCase() === 'exit') {
      console.log(chalk.yellow('\nGoodbye! 👋\n'));
      rl.close();
      process.exit(0);
    }

    if (!task.trim()) {
      console.log(chalk.red('Please enter a task.\n'));
      continue;
    }

    // Classify intent and suggest a mode
    const suggestedMode = classifyIntent(task).toUpperCase() as Mode;
    tui.displaySuggestedMode(suggestedMode, task);

    // Let user confirm or override mode
    const currentMode = await selectMode(suggestedMode, ask);

    // Enter the mode loop
    await runModeLoop(currentMode, task, backendUrl, ask);
    console.log('');
  }
}

async function selectMode(suggested: Mode, ask: (q: string) => Promise<string>): Promise<Mode> {
  const modeColor: Record<Mode, (s: string) => string> = {
    PLAN: chalk.cyan,
    BUILD: chalk.green,
    BRAINSTORM: chalk.blue,
  };

  console.log('');
  console.log(
    chalk.gray('Press ') +
      chalk.bold('Enter') +
      chalk.gray(' to use ') +
      modeColor[suggested](suggested) +
      chalk.gray(' or choose a mode:')
  );
  console.log(
    chalk.gray('  ') +
      chalk.cyan.bold('[1] PLAN') + chalk.gray('   design & break down steps') +
      chalk.gray('\n  ') +
      chalk.green.bold('[2] BUILD') + chalk.gray('  execute with live streaming') +
      chalk.gray('\n  ') +
      chalk.blue.bold('[3] BRAINSTORM') + chalk.gray('  free-form explore & Q&A')
  );

  const answer = await ask(chalk.gray('> '));
  const lower = answer.trim().toLowerCase();

  if (lower === '1' || lower === 'p' || lower === 'plan') return 'PLAN';
  if (lower === '2' || lower === 'b' || lower === 'build') return 'BUILD';
  if (lower === '3' || lower === 'bs' || lower === 'br' || lower === 'brainstorm') return 'BRAINSTORM';

  return suggested;
}

async function runModeLoop(
  mode: Mode,
  task: string,
  backendUrl: string,
  ask: (q: string) => Promise<string>
): Promise<void> {
  let currentMode = mode;

  while (true) {
    console.clear();
    printAppHeader();
    tui.displayModeBar(currentMode);
    tui.displayModeHeader(currentMode);

    if (currentMode === 'PLAN') {
      const next = await planModeLoop(task, backendUrl, currentMode, ask);
      if (next === 'BUILD') {
        currentMode = 'BUILD';
        continue;
      }
      if (next === 'switch') {
        currentMode = await promptModeSwitch(currentMode, ask);
        continue;
      }
      // cancelled or done
      return;
    }

    if (currentMode === 'BUILD') {
      const next = await buildModeLoop(task, backendUrl, currentMode, ask);
      if (next === 'switch') {
        currentMode = await promptModeSwitch(currentMode, ask);
        continue;
      }
      return;
    }

    if (currentMode === 'BRAINSTORM') {
      const next = await brainstormModeLoop(task, backendUrl, currentMode, ask);
      if (next === 'switch') {
        currentMode = await promptModeSwitch(currentMode, ask);
        continue;
      }
      return;
    }
  }
}

function printAppHeader() {
  console.log(chalk.cyan('╔════════════════════════════════════════════════════════════╗'));
  console.log(
    chalk.cyan('║') +
      chalk.bold.white('        Nexus - Autonomous Agent Orchestration                ') +
      chalk.cyan('║')
  );
  const versionLine = `        v${appVersion}`;
  console.log(
    chalk.cyan('║') +
      chalk.dim(versionLine.padEnd(60)) +
      chalk.cyan('║')
  );
  console.log(chalk.cyan('╚════════════════════════════════════════════════════════════╝'));
  console.log('');
}

async function promptModeSwitch(current: Mode, ask: (q: string) => Promise<string>): Promise<Mode> {
  const others: Mode[] = (['PLAN', 'BUILD', 'BRAINSTORM'] as Mode[]).filter((m) => m !== current);
  console.log('');
  console.log(
    chalk.gray('Switch to: ') +
      others.map((m, i) => chalk.bold(`[${i + 1}] ${m}`)).join(chalk.gray('  ')) +
      chalk.gray('  [Enter to stay in ') +
      chalk.bold(current) +
      chalk.gray(']')
  );
  const answer = await ask(chalk.gray('> '));
  const n = parseInt(answer.trim(), 10);
  if (n === 1) return others[0];
  if (n === 2) return others[1];
  return current;
}

// ---------------------------------------------------------------------------
// PLAN MODE
// ---------------------------------------------------------------------------
async function planModeLoop(
  task: string,
  backendUrl: string,
  currentMode: Mode,
  ask: (q: string, m?: Mode) => Promise<string>
): Promise<'BUILD' | 'switch' | 'cancel' | 'done'> {
  console.log(chalk.bold('Task:'), chalk.white(task));
  console.log('');
  console.log(chalk.yellow('⏳ Fetching plan from backend...'));

  let steps: string[] = [];

  try {
    const response = await fetch(`${backendUrl}/api/run`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(buildRunPayload(task, 'engineer', '')),
      signal: AbortSignal.timeout(120000),
    });

    if (!response.ok) {
      await reportBackendError('Backend error fetching plan', response);
      return 'cancel';
    }

    const planText = await response.text();
    let planParsed: unknown;
    try {
      planParsed = JSON.parse(planText);
    } catch {
      console.error(chalk.red(`Backend returned invalid JSON: ${planText.slice(0, 200)}`));
      return 'cancel';
    }
    const { run_id: planRunId } = planParsed as { run_id?: string };
    if (!planRunId || typeof planRunId !== 'string') {
      console.error(chalk.red('Backend did not return a valid run ID'));
      return 'cancel';
    }
    if (!/^[a-zA-Z0-9_-]+$/.test(planRunId)) {
      console.error(chalk.red('Backend returned run ID with invalid characters'));
      return 'cancel';
    }
    const run_id = planRunId;
    tui.displayConnected(run_id);

    const streamResponse = await fetch(`${backendUrl}/api/stream/${run_id}`, { signal: AbortSignal.timeout(120000) });
    if (!streamResponse.ok) {
      await reportBackendError('Stream endpoint error', streamResponse);
      return 'cancel';
    }
    const reader = streamResponse.body?.getReader();

    if (!reader) throw new Error('No response body');

    let planReceived = false;
    const stallTimer = setTimeout(() => {
      console.log(chalk.gray(`(no events from backend after ${SSE_STALL_WARNING_MS / 1000}s — backend may be stalled; check \`buildwithnexus logs -f\`)`));
    }, SSE_STALL_WARNING_MS);

    try {
      for await (const parsed of parseSSEStream(reader)) {
        if (parsed.type === 'plan') {
          steps = (parsed.data['steps'] as string[]) || [];
          planReceived = true;
          reader.cancel();
          break;
        } else if (parsed.type === 'error') {
          const errorMsg = (parsed.data['error'] as string) || (parsed.data['content'] as string) || 'Unknown error';
          tui.displayError(errorMsg);
          reader.cancel();
          return 'cancel';
        }
      }
    } finally {
      clearTimeout(stallTimer);
    }

    if (!planReceived || steps.length === 0) {
      console.log(chalk.yellow('No plan received from backend.'));
      console.log(chalk.gray('  This usually means the backend exited without emitting a plan event.'));
      console.log(chalk.gray('  Check `buildwithnexus logs -f` for the underlying error.'));
      steps = ['(no steps returned — execute anyway?)'];
    }
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    console.error(chalk.red('Error: ' + msg));
    return 'cancel';
  }

  // Display the plan
  displayPlanSteps(steps);

  // Approval loop
  while (true) {
    console.log(chalk.gray('Options: ') + chalk.bold('[Y]') + chalk.gray(' Execute  ') + chalk.bold('[e]') + chalk.gray(' Edit step  ') + chalk.bold('[n]') + chalk.gray(' Cancel'));
    const answer = (await ask(tui.displayPermissionPrompt('Execute this plan?'))).trim().toLowerCase();

    if (answer === '' || answer === 'y') {
      return 'BUILD';
    }
    if (answer === 'n' || answer === '') {
      console.log(chalk.yellow('\nExecution cancelled.\n'));
      return 'cancel';
    }
    if (answer === 'e' || answer === 'edit') {
      steps = await editPlanSteps(steps, currentMode, ask);
      displayPlanSteps(steps);
      continue;
    }
    if (answer === 's' || answer === 'switch') {
      return 'switch';
    }
  }
}

function displayPlanSteps(steps: string[]) {
  console.log('');
  console.log(chalk.bold.cyan('┌─────────────────────────────────────────────────────────┐'));
  console.log(chalk.bold.cyan('│') + chalk.bold.white('  📋 Execution Plan                                      ') + chalk.bold.cyan('│'));
  console.log(chalk.bold.cyan('├─────────────────────────────────────────────────────────┤'));
  steps.forEach((step, i) => {
    const label = `  Step ${i + 1}: `;
    const maxContentWidth = 57 - label.length;
    const truncated = step.length > maxContentWidth ? step.substring(0, maxContentWidth - 3) + '...' : step;
    const line = label + truncated;
    const padded = line.padEnd(57);
    console.log(chalk.bold.cyan('│') + chalk.white(padded) + chalk.bold.cyan('│'));
  });
  console.log(chalk.bold.cyan('└─────────────────────────────────────────────────────────┘'));
  console.log('');
}

async function editPlanSteps(steps: string[], currentMode: Mode, ask: (q: string) => Promise<string>): Promise<string[]> {
  console.log(chalk.gray('Enter step number to edit, or press Enter to finish editing:'));
  const numStr = await ask(chalk.bold('Step #: '));
  const n = parseInt(numStr.trim(), 10);
  if (!isNaN(n) && n >= 1 && n <= steps.length) {
    console.log(chalk.gray(`Current: ${steps[n - 1]}`));
    const updated = await ask(chalk.bold('New text: '));
    if (updated.trim()) steps[n - 1] = updated.trim();
  }
  return steps;
}

// ---------------------------------------------------------------------------
// BUILD MODE
// ---------------------------------------------------------------------------
async function buildModeLoop(
  task: string,
  backendUrl: string,
  currentMode: Mode,
  ask: (q: string, m?: Mode) => Promise<string>
): Promise<'switch' | 'done'> {
  console.log(chalk.bold('Task:'), chalk.white(task));
  tui.displayConnecting();

  try {
    const response = await fetch(`${backendUrl}/api/run`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(buildRunPayload(task, 'engineer', '')),
      signal: AbortSignal.timeout(120000),
    });

    if (!response.ok) {
      await reportBackendError('Backend error starting run', response);
      return 'done';
    }

    const buildText = await response.text();
    let buildParsed: unknown;
    try {
      buildParsed = JSON.parse(buildText);
    } catch {
      console.error(chalk.red(`Backend returned invalid JSON: ${buildText.slice(0, 200)}`));
      return 'done';
    }
    const { run_id: buildRunId } = buildParsed as { run_id?: string };
    if (!buildRunId || typeof buildRunId !== 'string') {
      console.error(chalk.red('Backend did not return a valid run ID'));
      return 'done';
    }
    if (!/^[a-zA-Z0-9_-]+$/.test(buildRunId)) {
      console.error(chalk.red('Backend returned run ID with invalid characters'));
      return 'done';
    }
    const run_id = buildRunId;
    tui.displayConnected(run_id);

    console.log(chalk.bold.green('⚙️  Executing...'));
    tui.displayStreamStart();

    const streamResponse = await fetch(`${backendUrl}/api/stream/${run_id}`, { signal: AbortSignal.timeout(120000) });
    if (!streamResponse.ok) {
      await reportBackendError('Stream endpoint error', streamResponse);
      return 'done';
    }
    const reader = streamResponse.body?.getReader();

    if (!reader) throw new Error('No response body');

    let sawTerminal = false;
    const stallTimer = setTimeout(() => {
      console.log(chalk.gray(`(no events from backend after ${SSE_STALL_WARNING_MS / 1000}s — backend may be stalled; check \`buildwithnexus logs -f\`)`));
    }, SSE_STALL_WARNING_MS);

    try {
      for await (const parsed of parseSSEStream(reader)) {
        const type = parsed.type;

        if (type === 'execution_complete') {
          const summary = (parsed.data['summary'] as string) || '';
          const count = (parsed.data['todos_completed'] as number) || 0;
          tui.displayResults(summary, count);
          tui.displayComplete(tui.getElapsedTime());
          sawTerminal = true;
          break;
        } else if (type === 'done') {
          tui.displayEvent(type, { content: 'Task completed successfully' });
          tui.displayComplete(tui.getElapsedTime());
          sawTerminal = true;
          break;
        } else if (type === 'error') {
          const errorMsg = (parsed.data['error'] as string) || (parsed.data['content'] as string) || 'Unknown error';
          tui.displayError(errorMsg);
          sawTerminal = true;
          break;
        } else if (type !== 'plan') {
          tui.displayEvent(type, parsed.data);
        }
      }
    } finally {
      clearTimeout(stallTimer);
    }

    if (!sawTerminal) {
      console.log(chalk.yellow('Stream ended without a terminal event (no execution_complete / done / error).'));
      console.log(chalk.gray('  The backend likely crashed mid-run. Check `buildwithnexus logs -f`.'));
    }
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    console.error(chalk.red('Error: ' + msg));
  }

  // Post-execution options
  console.log('');
  console.log(
    chalk.gray('Options: ') +
      chalk.bold('[Enter]') +
      chalk.gray(' Done')
  );
  const answer = (await ask(chalk.bold('> '))).trim().toLowerCase();
  if (answer === '/switch' || answer === '/mode') return 'switch';
  return 'done';
}

// ---------------------------------------------------------------------------
// BRAINSTORM MODE
// ---------------------------------------------------------------------------
async function brainstormModeLoop(
  task: string,
  backendUrl: string,
  currentMode: Mode,
  ask: (q: string, m?: Mode) => Promise<string>
): Promise<'switch' | 'done'> {
  console.log(chalk.bold('Starting topic:'), chalk.white(task));
  console.log(chalk.gray('Ask follow-up questions. Type "done" to exit, "switch" to change mode.\n'));

  let currentQuestion = task;

  while (true) {
    try {
      const response = await fetch(`${backendUrl}/api/run`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(buildRunPayload(
          currentQuestion,
          'brainstorm',
          'Generate ideas, considerations, and suggestions. Be concise and helpful.',
        )),
        signal: AbortSignal.timeout(120000),
      });

      if (!response.ok) {
        await reportBackendError('Backend error in brainstorm', response);
      } else {
        const brainstormText = await response.text();
        let brainstormParsed: unknown;
        try {
          brainstormParsed = JSON.parse(brainstormText);
        } catch {
          console.error(chalk.red(`Backend returned invalid JSON: ${brainstormText.slice(0, 200)}`));
          continue;
        }
        const { run_id: brainstormRunId } = brainstormParsed as { run_id?: string };
        if (!brainstormRunId || typeof brainstormRunId !== 'string') {
          console.error(chalk.red('Backend did not return a valid run ID'));
          continue;
        }
        if (!/^[a-zA-Z0-9_-]+$/.test(brainstormRunId)) {
          console.error(chalk.red('Backend returned run ID with invalid characters'));
          continue;
        }
        const run_id = brainstormRunId;
        const streamResponse = await fetch(`${backendUrl}/api/stream/${run_id}`, { signal: AbortSignal.timeout(120000) });
        if (!streamResponse.ok) {
          await reportBackendError('Stream endpoint error', streamResponse);
          continue;
        }
        const reader = streamResponse.body?.getReader();

        if (!reader) {
          console.error(chalk.red('No response body from agent'));
          continue;
        }

        let responseText = '';
        let firstEvent = true;
        const stallTimer = setTimeout(() => {
          console.log(chalk.gray(`(no events from backend after ${SSE_STALL_WARNING_MS / 1000}s — backend may be stalled; check \`buildwithnexus logs -f\`)`));
        }, SSE_STALL_WARNING_MS);

        try {
          for await (const parsed of parseSSEStream(reader)) {
            const type = parsed.type;
            const data = parsed.data;

            // Show thinking indicator on first event
            if (firstEvent && type !== 'done' && type !== 'error') {
              console.log(chalk.bold.blue('💭 Thinking...\n'));
              firstEvent = false;
            }

            if (type === 'done' || type === 'execution_complete' || type === 'final_result') {
              const summary = (data['summary'] as string) || (data['result'] as string) || '';
              if (summary) responseText = summary;
              break;
            } else if (type === 'error') {
              const errorMsg = (data['error'] as string) || (data['content'] as string) || 'Unknown error';
              responseText += errorMsg + '\n';
              break;
            } else if (type === 'thought' || type === 'observation') {
              const content = (data['content'] as string) || '';
              if (content) {
                console.log(chalk.gray('→ ' + content));
                responseText += content + '\n';
              }
            } else if (type === 'agent_response' || type === 'agent_result') {
              // Handle agent response events
              const content = (data['content'] as string) || (data['result'] as string) || '';
              if (content) responseText += content + '\n';
            } else if (type === 'action') {
              const content = (data['content'] as string) || '';
              if (content) {
                console.log(chalk.cyan('⚙️  ' + content));
                responseText += content + '\n';
              }
            } else if (type === 'agent_working' || type === 'started') {
              // Skip intermediate agent_working and started events in brainstorm mode
            } else if (type !== 'plan') {
              // Catch-all for any other event types
              const content = (data['content'] as string) || (data['response'] as string) || '';
              if (content) responseText += content + '\n';
            }
          }
        } finally {
          clearTimeout(stallTimer);
        }
        console.log('');

        if (responseText.trim()) {
          tui.displayBrainstormResponse(responseText.trim());
        } else {
          console.log(chalk.gray('(No response received from agent — check `buildwithnexus logs -f`)'));
        }
      }
    } catch (err: unknown) {
      const msg = err instanceof Error ? err.message : String(err);
      console.error(chalk.red('Error: ' + msg));
    }

    const followUp = await ask(chalk.bold.blue('💬 You: '));
    const lower = followUp.trim().toLowerCase();

    if (lower === 'done' || lower === 'exit') return 'done';
    if (lower === 'switch') return 'switch';
    if (!followUp.trim()) continue;

    currentQuestion = followUp.trim();
  }
}
