#!/usr/bin/env node

import { program } from 'commander';
import { PlanningREPL } from './deep-agents/ui/planning-repl.js';
import { StreamFormatter } from './deep-agents/ui/stream-formatter.js';
import { buildRunPayload } from './core/api.js';
import { MODELS } from './core/models.js';
import dotenv from 'dotenv';
import path from 'path';
import os from 'os';

dotenv.config({ path: path.join(os.homedir(), '.env.local') });

const backendUrl = process.env.BACKEND_URL || 'http://localhost:4200';

async function runCommand(task: string, options: { agent: string; goal?: string; model: string }) {
  const repl = new PlanningREPL();
  const formatter = new StreamFormatter();

  console.log(`\nStarting Nexus Workflow\n`);
  console.log(`  Task: ${task}`);
  console.log(`  Agent: ${options.agent}`);
  console.log(`  Backend: ${backendUrl}\n`);

  try {
    // POST to backend /api/run
    const payload = buildRunPayload(task, options.agent, options.goal || '');
    const response = await fetch(`${backendUrl}/api/run`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    });

    const { run_id } = (await response.json()) as { run_id: string };
    console.log(`Run ID: ${run_id}\n`);

    // Connect to WebSocket for streaming
    const wsUrl = backendUrl.replace('http', 'ws') + '/ws/stream';
    const ws = new WebSocket(wsUrl);

    ws.onopen = () => {
      console.log('Connected to backend\n');
    };

    ws.onmessage = (event) => {
      try {
        const data = JSON.parse(event.data as string) as {
          type: string;
          data: Record<string, unknown>;
        };

        switch (data.type) {
          case 'plan_created':
            repl.displayPlan(data.data as unknown as Parameters<PlanningREPL['displayPlan']>[0]);
            break;
          case 'thought':
            repl.displayReAct(data.data['content'] as string, 'thought');
            break;
          case 'action':
            repl.displayReAct(data.data['content'] as string, 'action');
            break;
          case 'observation':
            repl.displayReAct(data.data['content'] as string, 'observation');
            break;
          case 'checkpoint_saved':
            console.log(`  Checkpoint saved: ${JSON.stringify(data.data)}`);
            break;
          case 'metrics_update':
            repl.displayMetrics(data.data as unknown as Parameters<PlanningREPL['displayMetrics']>[0]);
            break;
          case 'done':
            console.log('\nWorkflow complete!\n');
            repl.close();
            ws.close();
            process.exit(0);
            break;
          case 'error':
            console.error(`\nError: ${data.data['error']}\n`);
            repl.close();
            ws.close();
            process.exit(1);
            break;
        }
      } catch (e) {
        console.error('Parse error:', e);
      }
    };

    ws.onerror = (error) => {
      console.error('WebSocket error:', error);
      repl.close();
      process.exit(1);
    };
  } catch (error) {
    console.error('Error starting workflow:', error);
    repl.close();
    process.exit(1);
  }
}

program
  .name('deep-agents')
  .description('Run Deep Agents workflows')
  .version('0.5.17');

program
  .command('run <task>')
  .description('Run a task with Deep Agents')
  .option('-a, --agent <name>', 'Agent role (engineer, researcher, etc)', 'engineer')
  .option('-g, --goal <goal>', 'Agent goal')
  .option('-m, --model <model>', 'LLM model', MODELS.DEFAULT)
  .action(runCommand);

program.parse();
