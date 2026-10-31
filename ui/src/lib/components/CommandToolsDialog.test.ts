import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { persistence, type CommandToolSummary } from '../persistence';
import { COMMAND_TOOL_SUGGESTIONS } from '../commandToolSuggestions';
import CommandToolsDialog from './CommandToolsDialog.svelte';

const review: CommandToolSummary = {
  name: 'review',
  description: 'Ask for a review',
  kind: 'command',
  command: ['/Users/me/bin/review', '--root', 'my folder'],
  subagentProblem: null,
};
const lint: CommandToolSummary = {
  name: 'lint',
  description: 'Lint a file',
  kind: 'command',
  command: ['eslint', '{file}'],
  subagentProblem: 'A subagent tool must set "stdin": "json", so the prompt is sent on standard input.',
};
const issues: CommandToolSummary = {
  name: 'issues',
  description: 'Search issues',
  kind: 'http',
  command: [],
  subagentProblem: 'A subagent can only run on a command tool, not an HTTP tool.',
};

describe('command tools screen', () => {
  beforeEach(() => {
    Object.defineProperty(HTMLDialogElement.prototype, 'showModal', {
      configurable: true,
      value: function (this: HTMLDialogElement) {
        this.open = true;
      },
    });
    vi.spyOn(persistence, 'available').mockReturnValue(true);
    vi.spyOn(persistence, 'commandTools').mockResolvedValue([review, lint, issues]);
    vi.spyOn(persistence, 'commandClis').mockResolvedValue([
      { name: 'claude', installed: true, signedIn: true },
      { name: 'codex', installed: true, signedIn: false },
      { name: 'copilot', installed: false, signedIn: false },
    ]);
  });
  it('lists tools with why a subagent cannot use them, and leaves HTTP tools to the file', async () => {
    render(CommandToolsDialog, { props: { onClose: vi.fn() } });
    expect(await screen.findByText('/Users/me/bin/review --root "my folder"')).toBeInTheDocument();
    expect(screen.getByText('Subagents can run on this tool.')).toBeInTheDocument();
    expect(screen.getByText(/Not for subagents: A subagent tool must set "stdin"/)).toBeInTheDocument();
    expect(screen.getByText(/A subagent can only run on a command tool/)).toBeInTheDocument();
    expect(screen.getByText('Edit in tools.json')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Remove issues' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Edit issues' })).not.toBeInTheDocument();
  });
  it('removes a command tool', async () => {
    const user = userEvent.setup();
    const remove = vi.spyOn(persistence, 'removeCommandTool').mockResolvedValue();
    render(CommandToolsDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Remove lint' }));
    expect(remove).toHaveBeenCalledWith('lint');
    await vi.waitFor(() => expect(persistence.commandTools).toHaveBeenCalledTimes(2));
  });
  it('shows what was found for each suggestion without claiming more', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'commandTools').mockResolvedValue([]);
    render(CommandToolsDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Add tool' }));
    expect(await screen.findByText('Installed · Sign-in found')).toBeInTheDocument();
    expect(
      screen.getByText('Installed · Not detected (may use the system keychain)'),
    ).toBeInTheDocument();
    expect(screen.getByText('Not found on your PATH')).toBeInTheDocument();
  });
  it('adds claude so a subagent can run on it', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'commandTools').mockResolvedValue([]);
    const save = vi.spyOn(persistence, 'addCommandTool').mockResolvedValue();
    render(CommandToolsDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Add tool' }));
    await user.click(screen.getByRole('button', { name: /claude/ }));
    expect(screen.getByLabelText('Name')).toHaveValue('claude');
    expect(screen.getByLabelText('Subagents can run on this tool')).toBeChecked();
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(save).toHaveBeenCalledWith(
      'claude',
      'Ask Claude Code for a read-only answer. Subagents can run on it.',
      [
        'claude',
        '-p',
        '--permission-mode',
        'plan',
        '--setting-sources',
        'user',
        '--strict-mcp-config',
        '--append-system-prompt',
        'Standard input is JSON; do the task in arguments.prompt.',
      ],
      true,
    );
    await vi.waitFor(() => expect(screen.getByRole('button', { name: 'Add tool' })).toBeEnabled());
  });
  it('keeps the project from configuring the suggested claude', () => {
    const claude = COMMAND_TOOL_SUGGESTIONS.find((item) => item.name === 'claude');
    // A cloned repository's .claude/settings.json and .mcp.json are never loaded.
    expect(claude?.command).toContain('--strict-mcp-config');
    const sources = claude?.command.indexOf('--setting-sources') ?? -1;
    expect(claude?.command.slice(sources, sources + 2)).toEqual(['--setting-sources', 'user']);
  });
  it('suggests codex and copilot with the prompt as an argument, not for subagents', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'commandTools').mockResolvedValue([]);
    const save = vi.spyOn(persistence, 'addCommandTool').mockResolvedValue();
    render(CommandToolsDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Add tool' }));
    await user.click(screen.getByRole('button', { name: /codex/ }));
    expect(screen.getByLabelText('Subagents can run on this tool')).not.toBeChecked();
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(save).toHaveBeenCalledWith(
      'codex',
      expect.any(String),
      ['codex', 'exec', '--sandbox', 'read-only', '{prompt}'],
      false,
    );
  });
  it('adds a tool of its own, refuses an unclosed quote, and shows a refused name', async () => {
    const user = userEvent.setup();
    const save = vi
      .spyOn(persistence, 'addCommandTool')
      .mockRejectedValue('A tool named lint already exists. Edit it instead.');
    render(CommandToolsDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Add tool' }));
    await user.type(screen.getByLabelText('Name'), 'lint');
    await user.type(screen.getByLabelText('Description'), 'Lint');
    await user.type(screen.getByLabelText('Command'), 'eslint "a b');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('closing quote');
    expect(save).not.toHaveBeenCalled();
    await user.type(screen.getByLabelText('Command'), '"');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(await screen.findByText(/A tool named lint already exists/)).toBeInTheDocument();
    expect(save).toHaveBeenCalledWith('lint', 'Lint', ['eslint', 'a b'], false);
  });
  it('edits only the description and command of a tool', async () => {
    const user = userEvent.setup();
    const save = vi.spyOn(persistence, 'updateCommandTool').mockResolvedValue();
    render(CommandToolsDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Edit review' }));
    expect(screen.getByLabelText('Name')).toBeDisabled();
    expect(screen.queryByLabelText('Subagents can run on this tool')).not.toBeInTheDocument();
    const command = screen.getByLabelText('Command');
    expect(command).toHaveValue('/Users/me/bin/review --root "my folder"');
    await user.clear(command);
    await user.type(command, 'review --strict');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(save).toHaveBeenCalledWith('review', 'Ask for a review', ['review', '--strict']);
  });
  it('shows the message from a refused save', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'updateCommandTool').mockRejectedValue('The tools file is not valid.');
    render(CommandToolsDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Edit review' }));
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('The tools file is not valid.');
  });
  it('still lists tools when detection fails', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'commandClis').mockRejectedValue('nope');
    render(CommandToolsDialog, { props: { onClose: vi.fn() } });
    expect(await screen.findByText('review')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Add tool' }));
    expect(screen.queryByText(/Installed/)).not.toBeInTheDocument();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });
});
