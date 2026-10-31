import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { persistence, type CommandToolSummary } from '../persistence';
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
    const save = vi.spyOn(persistence, 'saveCommandTool').mockResolvedValue();
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
        '--append-system-prompt',
        'Standard input is JSON; do the task in arguments.prompt.',
      ],
      true,
    );
    await vi.waitFor(() => expect(screen.getByRole('button', { name: 'Add tool' })).toBeEnabled());
  });
  it('suggests codex and copilot with the prompt as an argument, not for subagents', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'commandTools').mockResolvedValue([]);
    const save = vi.spyOn(persistence, 'saveCommandTool').mockResolvedValue();
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
  it('adds a tool of its own and refuses an unclosed quote or a name in use', async () => {
    const user = userEvent.setup();
    const save = vi.spyOn(persistence, 'saveCommandTool').mockResolvedValue();
    render(CommandToolsDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Add tool' }));
    await user.type(screen.getByLabelText('Name'), 'lint');
    await user.type(screen.getByLabelText('Description'), 'Lint');
    await user.type(screen.getByLabelText('Command'), 'eslint "a b');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('closing quote');
    await user.type(screen.getByLabelText('Command'), '"');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(screen.getByRole('alert')).toHaveTextContent('A tool named lint already exists');
    expect(save).not.toHaveBeenCalled();
  });
  it('edits only the description and command of a tool', async () => {
    const user = userEvent.setup();
    const save = vi.spyOn(persistence, 'saveCommandTool').mockResolvedValue();
    render(CommandToolsDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Edit review' }));
    expect(screen.getByLabelText('Name')).toBeDisabled();
    expect(screen.queryByLabelText('Subagents can run on this tool')).not.toBeInTheDocument();
    const command = screen.getByLabelText('Command');
    expect(command).toHaveValue('/Users/me/bin/review --root "my folder"');
    await user.clear(command);
    await user.type(command, 'review --strict');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(save).toHaveBeenCalledWith('review', 'Ask for a review', ['review', '--strict'], false);
  });
  it('shows the message from a refused save', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'saveCommandTool').mockRejectedValue('The tools file is not valid.');
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
