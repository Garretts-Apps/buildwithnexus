import chalk, { ChalkInstance } from 'chalk';
import stringWidth from 'string-width';

export type Mode = 'PLAN' | 'BUILD' | 'BRAINSTORM';

// Semantic color roles (respects terminal light/dark themes)
const colors = {
  accent: chalk.hex('#7D56F4'),    // Brand purple
  success: chalk.hex('#00FF87'),   // Bright green
  warning: chalk.hex('#FFB86C'),   // Amber
  info: chalk.hex('#8BE9FD'),      // Cyan
  muted: chalk.gray,               // Adaptive gray
  error: chalk.red,
};

// Spinner frames (Braille characters for smooth animation)
const SPINNER_FRAMES = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const STATUS_SYMBOLS = {
  done: colors.success('✔'),
  active: colors.info('◉'),
  pending: colors.muted('○'),
  error: colors.error('✖'),
};

export class TUI {
  private taskStartTime: number = 0;
  private eventCount: number = 0;
  private spinnerIndex: number = 0;

  private getSpinner(): string {
    const frame = SPINNER_FRAMES[this.spinnerIndex % SPINNER_FRAMES.length];
    this.spinnerIndex++;
    return frame;
  }

  displayHeader(task: string, agent: string) {
    console.clear();

    // Header with rounded borders (friendly, modern)
    const headerBox = this.makeRoundedBox(
      colors.accent('🚀 NEXUS - Autonomous Agent Orchestration'),
      62,
      colors.accent
    );
    console.log(headerBox);
    console.log('');

    console.log(colors.muted('Task') + colors.muted(':  ') + chalk.white(task));
    console.log(colors.muted('Agent') + colors.muted(': ') + colors.info(agent));
    console.log('');
    this.taskStartTime = Date.now();
  }

  displayConnecting() {
    console.log(`${this.getSpinner()} ${colors.warning('Connecting to backend...')}`);
  }

  displayConnected(runId: string) {
    console.log(`${STATUS_SYMBOLS.done} Connected • ${colors.muted(`run: ${runId}`)}`);
    console.log('');
  }

  displayStreamStart() {
    console.log(chalk.bold(colors.accent('📡 Streaming Events')));
    console.log('');
  }

  displayPlan(task: string, steps: string[]) {
    console.log('');

    // Plan box with double borders (authority, structure)
    const planHeader = colors.accent('Plan Breakdown');
    const planLines: string[] = [];

    steps.forEach((step, i) => {
      planLines.push(`  ${STATUS_SYMBOLS.pending}  ${chalk.white(`Step ${i + 1}:`)} ${step}`);
    });

    const planBox = this.makeDoubleBox(planHeader, planLines.join('\n'), 62, colors.accent);
    console.log(planBox);
    console.log('');
  }

  displayEvent(type: string, data: Record<string, unknown>) {
    this.eventCount++;
    const content = (data['content'] as string) || '';

    if (type === 'agent_working') {
      const agent = (data['agent'] as string) || 'Agent';
      const agentTask = (data['task'] as string) || '';
      console.log('');
      console.log(`  ${colors.info('👤')} ${colors.info(chalk.bold(agent))}`);
      console.log(`     ${colors.muted('→')} ${agentTask}`);
      return;
    }

    if (type === 'agent_result') {
      const result = (data['result'] as string) || '';
      let displayResult = result;
      if (displayResult.length > 100) {
        displayResult = displayResult.substring(0, 97) + '...';
      }
      console.log(`  ${STATUS_SYMBOLS.done} ${chalk.white(displayResult)}`);
      return;
    }

    // Event icons with semantic meaning
    const eventConfig: Record<string, { icon: string; color: (s: string) => string }> = {
      thought: { icon: '💭', color: colors.info },
      action: { icon: '⚡', color: colors.warning },
      observation: { icon: '✓', color: colors.success },
      started: { icon: '▶', color: colors.info },
      thinking: { icon: '💭', color: colors.info },
      plan: { icon: '🎯', color: colors.info },
      progress: { icon: '→', color: colors.muted },
      done: { icon: '✨', color: colors.success },
      execution_complete: { icon: '✨', color: colors.success },
      error: { icon: '✖', color: colors.error },
    };

    const config = eventConfig[type] || { icon: '●', color: colors.muted };
    let displayContent = content;
    if (displayContent.length > 100) {
      displayContent = displayContent.substring(0, 97) + '...';
    }

    console.log(`  ${config.icon} ${config.color(displayContent)}`);
  }

  displayResults(summary: string, todosCompleted: number) {
    console.log('');
    console.log(colors.success('━'.repeat(60)));
    console.log(colors.success.bold('✨ Execution Complete'));
    console.log('');

    const lines = summary.split('\n');
    for (const line of lines) {
      console.log(`  ${colors.success('│')} ${chalk.white(line)}`);
    }

    console.log('');
    console.log(`  ${colors.muted(`${todosCompleted} subtask(s) completed`)}`);
    console.log('');
  }

  displayError(error: string) {
    console.log('');
    console.log(colors.error('❌ Error'));
    console.log(colors.error(error));
    console.log('');
  }

  displayComplete(duration: number) {
    const seconds = (duration / 1000).toFixed(1);
    console.log('');
    console.log(colors.success.bold(`✨ Complete in ${seconds}s`));
    console.log(colors.muted(`${this.eventCount} event(s) streamed`));
    console.log('');
  }

  displayBox(title: string, content: string) {
    const box = this.makeRoundedBox(title, 60, colors.accent);
    console.log(box);
    const lines = content.split('\n');
    for (const line of lines) {
      const padded = line.substring(0, 56).padEnd(56);
      console.log(`  ${padded}`);
    }
    console.log('');
  }

  getElapsedTime(): number {
    return Date.now() - this.taskStartTime;
  }

  displayModeBar(current: Mode) {
    const modes: Mode[] = ['PLAN', 'BUILD', 'BRAINSTORM'];
    const modeConfig: Record<Mode, { icon: string; color: ChalkInstance }> = {
      PLAN: { icon: '📋', color: colors.info },
      BUILD: { icon: '⚙️', color: colors.success },
      BRAINSTORM: { icon: '💡', color: chalk.blue },
    };

    const parts = modes.map((m) => {
      const config = modeConfig[m];
      const label = `${config.icon} ${m}`;
      if (m === current) {
        return config.color.bold(label);
      }
      return colors.muted(label);
    });

    console.log(parts.join(colors.muted('  •  ')));
    console.log('');
  }

  displayModeHeader(mode: Mode) {
    const config: Record<Mode, { icon: string; desc: string; color: ChalkInstance }> = {
      PLAN: {
        icon: '📋',
        desc: 'Break down and review steps',
        color: colors.info,
      },
      BUILD: {
        icon: '⚙️',
        desc: 'Execute with live streaming',
        color: colors.success,
      },
      BRAINSTORM: {
        icon: '💡',
        desc: 'Free-form Q&A and exploration',
        color: chalk.blue,
      },
    };

    const c = config[mode];
    console.log('');
    console.log(c.color.bold(`${c.icon} ${mode}`));
    console.log(colors.muted(c.desc));
    console.log('');
  }

  displaySuggestedMode(mode: Mode, task: string) {
    const modeColor: Record<Mode, ChalkInstance> = {
      PLAN: colors.info,
      BUILD: colors.success,
      BRAINSTORM: chalk.blue,
    };

    const taskPreview = task.length > 45 ? task.substring(0, 42) + '...' : task;
    console.log('');
    console.log(
      colors.muted('Suggested: ') +
        modeColor[mode].bold(mode) +
        colors.muted(` for "${taskPreview}"`)
    );
  }

  displayBrainstormResponse(response: string) {
    console.log('');

    const lines = response.split('\n');
    for (const line of lines) {
      if (line.trim()) {
        console.log(`  ${chalk.white(line)}`);
      } else {
        console.log('');
      }
    }
    console.log('');
  }

  displayPermissionPrompt(message: string): string {
    return colors.accent.bold(message) + colors.muted(' (y/n)  ');
  }

  displayInputBox(mode: Mode): string {
    const width = 60;
    const innerWidth = width - 4;
    const modeName = mode === 'PLAN' ? '📋 Planning' : mode === 'BUILD' ? '⚙️  Building' : '💡 Brainstorming';

    // Top border
    const top = colors.accent('┌' + '─'.repeat(innerWidth) + '┐');
    // Bottom border with mode indicator
    const bottom = colors.accent('└' + '─'.repeat(innerWidth) + '┘');
    const modeDisplay = colors.muted(`Mode: ${modeName}`);

    console.log(top);
    // Input area will be typed here by readline
    return colors.accent('│ ') + colors.muted('> ');
  }

  displayModeIndicator(mode: Mode): void {
    const modeName = mode === 'PLAN' ? '📋 Planning' : mode === 'BUILD' ? '⚙️  Building' : '💡 Brainstorming';
    console.log(colors.muted(`Mode: ${modeName}\n`));
  }

  private padToWidth(text: string, targetWidth: number): string {
    const visibleWidth = stringWidth(text);
    const padding = Math.max(0, targetWidth - visibleWidth);
    return text + ' '.repeat(padding);
  }

  private truncateToWidth(text: string, maxWidth: number): string {
    let result = '';
    let width = 0;
    for (const char of text) {
      const charWidth = stringWidth(char);
      if (width + charWidth > maxWidth) break;
      result += char;
      width += charWidth;
    }
    return result;
  }

  private makeRoundedBox(
    title: string,
    width: number,
    borderColor: (s: string) => string
  ): string {
    const lines: string[] = [];
    const innerWidth = width - 4;
    const titleText = ` ${title} `;
    const titleTruncated = this.truncateToWidth(titleText, innerWidth);
    const titlePadded = this.padToWidth(titleTruncated, innerWidth);

    lines.push(borderColor('╭' + '─'.repeat(innerWidth) + '╮'));
    lines.push(borderColor('│') + chalk.bold(titlePadded) + borderColor('│'));
    lines.push(borderColor('╰' + '─'.repeat(innerWidth) + '╯'));

    return lines.join('\n');
  }

  private makeDoubleBox(
    title: string,
    content: string,
    width: number,
    borderColor: (s: string) => string
  ): string {
    const lines: string[] = [];
    const innerWidth = width - 4;
    const titleText = ` ${title} `;
    const titleTruncated = this.truncateToWidth(titleText, innerWidth);
    const titlePadded = this.padToWidth(titleTruncated, innerWidth);

    lines.push(borderColor('╔' + '═'.repeat(innerWidth) + '╗'));
    lines.push(borderColor('║') + chalk.bold(titlePadded) + borderColor('║'));
    lines.push(borderColor('╠' + '═'.repeat(innerWidth) + '╣'));

    const contentLines = content.split('\n');
    for (const line of contentLines) {
      const contentWidth = innerWidth - 2;
      const truncated = this.truncateToWidth(line, contentWidth);
      const padded = this.padToWidth(truncated, contentWidth);
      lines.push(borderColor('║') + '  ' + padded + borderColor('║'));
    }

    lines.push(borderColor('╚' + '═'.repeat(innerWidth) + '╝'));

    return lines.join('\n');
  }
}

export const tui = new TUI();
