import { fireEvent, render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { instructionFileValid, persistence, type MemoryProposal } from '../persistence';
import MemoryDialog from './MemoryDialog.svelte';

describe('memory management', () => {
  beforeEach(() => {
    Object.defineProperty(HTMLDialogElement.prototype, 'showModal', {
      configurable: true,
      value: function (this: HTMLDialogElement) {
        this.open = true;
      },
    });
    vi.spyOn(persistence, 'available').mockReturnValue(true);
    vi.spyOn(persistence, 'preferences').mockResolvedValue({ memoryEnabled: false });
    vi.spyOn(persistence, 'memories').mockResolvedValue([]);
    vi.spyOn(persistence, 'memoryProposals').mockResolvedValue([]);
  });
  it('saves only the user-entered fact and refreshes the list', async () => {
    const user = userEvent.setup();
    const save = vi.spyOn(persistence, 'saveMemory').mockResolvedValue();
    render(MemoryDialog, { props: { onClose: vi.fn() } });
    const input = await screen.findByLabelText('Remember something');
    await vi.waitFor(() => expect(input).toBeEnabled());
    await user.type(input, 'I prefer practical examples.');
    await user.click(screen.getByRole('button', { name: 'Save memory' }));
    expect(save).toHaveBeenCalledWith(
      expect.objectContaining({ content: 'I prefer practical examples.' }),
    );
    expect(input).toHaveValue('');
    expect(persistence.memories).toHaveBeenCalledTimes(2);
  });
  it('saves agent limits only within their ranges', async () => {
    const user = userEvent.setup();
    const save = vi.spyOn(persistence, 'savePreferences').mockResolvedValue();
    render(MemoryDialog, { props: { onClose: vi.fn(), embedded: true, section: 'limits' } });
    const iterations = await screen.findByLabelText('Agent iteration limit');
    await vi.waitFor(() => expect(iterations).toBeEnabled());
    const timeout = screen.getByLabelText('Command time limit (seconds)');
    const button = screen.getByRole('button', { name: 'Save context limit' });
    await user.clear(iterations);
    await user.type(iterations, '0');
    expect(button).toBeDisabled();
    await user.clear(iterations);
    await user.type(iterations, '40');
    await user.clear(timeout);
    await user.type(timeout, '600');
    await user.click(button);
    expect(save).toHaveBeenCalledWith(
      expect.objectContaining({ maxIterations: 40, commandTimeoutSeconds: 600 }),
    );
  });
  it('cancels an invalid context limit edit', async () => {
    const user = userEvent.setup();
    render(MemoryDialog, { props: { onClose: vi.fn(), embedded: true, section: 'limits' } });
    const context = await screen.findByLabelText('Model context limit (tokens)');
    await vi.waitFor(() => expect(context).toBeEnabled());
    await user.clear(context);
    await user.type(context, '1000');
    expect(screen.getByRole('button', { name: 'Save context limit' })).toBeDisabled();
    const cancel = screen.getByRole('button', { name: 'Cancel' });
    expect(cancel).toBeEnabled();
    await user.click(cancel);
    expect(context).toHaveValue(32000);
  });
  it('saves the command output cap only within its range', async () => {
    const user = userEvent.setup();
    const save = vi.spyOn(persistence, 'savePreferences').mockResolvedValue();
    render(MemoryDialog, { props: { onClose: vi.fn(), embedded: true, section: 'limits' } });
    const cap = await screen.findByLabelText('Command output cap (bytes)');
    await vi.waitFor(() => expect(cap).toBeEnabled());
    expect(cap).toHaveValue(65536);
    const button = screen.getByRole('button', { name: 'Save context limit' });
    await user.clear(cap);
    await user.type(cap, '1024');
    expect(button).toBeDisabled();
    await user.clear(cap);
    await user.type(cap, '8192');
    await user.click(button);
    expect(save).toHaveBeenCalledWith(expect.objectContaining({ maxOutputBytes: 8192 }));
  });
  it('saves subagent steps and tools, and marks a command tool as off-device', async () => {
    const user = userEvent.setup();
    const save = vi.spyOn(persistence, 'savePreferences').mockResolvedValue();
    render(MemoryDialog, { props: { onClose: vi.fn(), embedded: true, section: 'subagents' } });
    const steps = await screen.findByLabelText('Subagent step limit');
    await vi.waitFor(() => expect(steps).toBeEnabled());
    expect(steps).toHaveValue(8);
    expect(screen.getByLabelText('Read files')).toBeChecked();
    expect(screen.getByLabelText('Run commands (asks)')).not.toBeChecked();
    expect(screen.queryByText(/Sends data off this device/)).not.toBeInTheDocument();
    const button = screen.getByRole('button', { name: 'Save subagent settings' });
    await user.clear(steps);
    await user.type(steps, '51');
    expect(button).toBeDisabled();
    await user.clear(steps);
    await user.type(steps, '12');
    await user.click(screen.getByLabelText('Run commands (asks)'));
    await user.click(screen.getByLabelText('List folders'));
    await user.click(button);
    expect(save).toHaveBeenLastCalledWith({
      subagentMaxSteps: 12,
      subagentTools: ['read_file', 'shell'],
      subagentRunsOn: 'local',
      commandToolsIsolation: 'outside',
    });
    expect(screen.getByText('Saved ✓')).toBeInTheDocument();
    await user.click(screen.getByLabelText('A command tool'));
    expect(screen.getByText(/Sends data off this device/)).toBeVisible();
    expect(button).toBeDisabled();
    await user.type(screen.getByLabelText('Command tool name'), 'review');
    await user.click(button);
    expect(save).toHaveBeenLastCalledWith({
      subagentMaxSteps: 12,
      subagentTools: ['read_file', 'shell'],
      subagentRunsOn: { tool: 'review' },
      commandToolsIsolation: 'outside',
    });
  });
  it('shows command tool settings synchronously from local state', async () => {
    render(MemoryDialog, { props: { onClose: vi.fn(), embedded: true, section: 'subagents' } });
    const option = await screen.findByLabelText('A command tool');
    await vi.waitFor(() => expect(option).toBeEnabled());
    fireEvent.click(option);
    expect(screen.getByLabelText('Command tool name')).toBeInTheDocument();
  });
  it('saves the memory toggle without committing a draft context limit', async () => {
    const user = userEvent.setup();
    let stored = { memoryEnabled: false, contextWindow: 32000 };
    vi.spyOn(persistence, 'preferences').mockImplementation(async () => ({ ...stored }));
    vi.spyOn(persistence, 'savePreferences').mockImplementation(async (patch) => {
      stored = { ...stored, ...patch };
    });
    const view = render(MemoryDialog, {
      props: { onClose: vi.fn(), embedded: true, section: 'limits' },
    });
    const context = await screen.findByLabelText('Model context limit (tokens)');
    await vi.waitFor(() => expect(context).toBeEnabled());
    await user.clear(context);
    await user.type(context, '64000');
    view.unmount();
    render(MemoryDialog, { props: { onClose: vi.fn(), embedded: true, section: 'memory' } });
    const toggle = await screen.findByLabelText('Use saved memories in chat');
    await vi.waitFor(() => expect(toggle).toBeEnabled());
    await user.click(toggle);
    expect(stored).toEqual({ memoryEnabled: true, contextWindow: 32000 });
  });
  it('loads saved subagent settings that run on a command tool', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'preferences').mockResolvedValue({
      subagentMaxSteps: 3,
      subagentRunsOn: { tool: 'review' },
    });
    render(MemoryDialog, { props: { onClose: vi.fn(), embedded: true, section: 'subagents' } });
    const name = await screen.findByLabelText('Command tool name');
    await vi.waitFor(() => expect(name).toBeEnabled());
    expect(name).toHaveValue('review');
    expect(screen.getByLabelText('Subagent step limit')).toHaveValue(3);
    expect(screen.getByText(/Sends data off this device/)).toBeVisible();
  });
  const isolationHint = 'Command tools run with your account permissions and always ask first.';
  it('saves command tools isolation and shows the account-permissions label', async () => {
    const user = userEvent.setup();
    const save = vi.spyOn(persistence, 'savePreferences').mockResolvedValue();
    render(MemoryDialog, { props: { onClose: vi.fn(), embedded: true, section: 'subagents' } });
    const outside = await screen.findByLabelText('Outside the sandbox (asks every time)');
    await vi.waitFor(() => expect(outside).toBeEnabled());
    const sandboxed = screen.getByLabelText('Inside the Commands profile');
    expect(screen.getByRole('group', { name: 'Command tools' })).toBeInTheDocument();
    expect(outside).toHaveAttribute('value', 'outside');
    expect(sandboxed).toHaveAttribute('value', 'sandboxed');
    expect(outside).toBeChecked();
    expect(screen.getByText(isolationHint)).toBeVisible();
    await user.click(sandboxed);
    expect(screen.queryByText(isolationHint)).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Save subagent settings' }));
    expect(save).toHaveBeenLastCalledWith(
      expect.objectContaining({ commandToolsIsolation: 'sandboxed' }),
    );
  });
  it('keeps command tools isolation through a save, a reload and an unrelated save', async () => {
    const user = userEvent.setup();
    // The desktop merges each save into the stored preferences (patch_preferences).
    let stored: Record<string, unknown> = { memoryEnabled: false };
    vi.spyOn(persistence, 'preferences').mockImplementation(async () => ({ ...stored }));
    vi.spyOn(persistence, 'savePreferences').mockImplementation(async (patch) => {
      stored = { ...stored, ...patch };
    });
    const first = render(MemoryDialog, {
      props: { onClose: vi.fn(), embedded: true, section: 'subagents' },
    });
    const sandboxed = await screen.findByLabelText('Inside the Commands profile');
    await vi.waitFor(() => expect(sandboxed).toBeEnabled());
    await user.click(sandboxed);
    await user.click(screen.getByRole('button', { name: 'Save subagent settings' }));
    expect(stored.commandToolsIsolation).toBe('sandboxed');
    first.unmount();

    // Reopened: the saved choice is shown, and the other saves do not reset it.
    const second = render(MemoryDialog, {
      props: { onClose: vi.fn(), embedded: true, section: 'subagents' },
    });
    const reloaded = await screen.findByLabelText('Inside the Commands profile');
    await vi.waitFor(() => expect(reloaded).toBeEnabled());
    expect(reloaded).toBeChecked();
    expect(screen.queryByText(isolationHint)).not.toBeInTheDocument();
    second.unmount();
    const limits = render(MemoryDialog, {
      props: { onClose: vi.fn(), embedded: true, section: 'limits' },
    });
    const limit = await screen.findByLabelText('Model context limit (tokens)');
    await vi.waitFor(() => expect(limit).toBeEnabled());
    await user.clear(limit);
    await user.type(limit, '33000');
    await user.click(screen.getByRole('button', { name: 'Save context limit' }));
    expect(stored.commandToolsIsolation).toBe('sandboxed');
    limits.unmount();
    const instructions = render(MemoryDialog, {
      props: { onClose: vi.fn(), embedded: true, section: 'instructions' },
    });
    const instructionFile = await screen.findByLabelText('Instruction file limit (KiB)');
    await vi.waitFor(() => expect(instructionFile).toBeEnabled());
    await user.type(screen.getByLabelText('Extra instruction files, one per line'), 'CLAUDE.md');
    await user.click(screen.getByRole('button', { name: 'Save instruction limits' }));
    expect(stored.commandToolsIsolation).toBe('sandboxed');
    expect(stored.instructionMaxFileKib).toBe(8);
    instructions.unmount();

    // Choosing the default again is saved too.
    render(MemoryDialog, { props: { onClose: vi.fn(), embedded: true, section: 'subagents' } });
    const outside = await screen.findByLabelText('Outside the sandbox (asks every time)');
    await vi.waitFor(() => expect(outside).toBeEnabled());
    await user.click(outside);
    await user.click(screen.getByRole('button', { name: 'Save subagent settings' }));
    expect(stored.commandToolsIsolation).toBe('outside');
  });
  it('loads a saved sandboxed choice', async () => {
    vi.spyOn(persistence, 'preferences').mockResolvedValue({ commandToolsIsolation: 'sandboxed' });
    render(MemoryDialog, { props: { onClose: vi.fn(), embedded: true, section: 'subagents' } });
    const sandboxed = await screen.findByLabelText('Inside the Commands profile');
    await vi.waitFor(() => expect(sandboxed).toBeEnabled());
    expect(sandboxed).toBeChecked();
  });
  it('saves instruction limits and extra files only within their ranges', async () => {
    const user = userEvent.setup();
    const save = vi.spyOn(persistence, 'savePreferences').mockResolvedValue();
    render(MemoryDialog, { props: { onClose: vi.fn(), embedded: true, section: 'instructions' } });
    const file = await screen.findByLabelText('Instruction file limit (KiB)');
    await vi.waitFor(() => expect(file).toBeEnabled());
    const total = screen.getByLabelText('Total instruction limit (KiB)');
    const files = screen.getByLabelText('Extra instruction files, one per line');
    expect(file).toHaveValue(8);
    expect(total).toHaveValue(32);
    expect(files).toHaveValue('');
    const button = screen.getByRole('button', { name: 'Save instruction limits' });
    for (const [input, value] of [
      [file, '0'],
      [file, '65'],
      [total, '7'],
      [total, '257'],
    ] as const) {
      await user.clear(input);
      await user.type(input, value);
      expect(button).toBeDisabled();
      await user.clear(input);
      await user.type(input, input === file ? '4' : '64');
      expect(button).toBeEnabled();
    }
    for (const bad of ['../CLAUDE.md', '/etc/passwd', 'C:\\CLAUDE.md', 'AGENTS.md']) {
      await user.clear(files);
      await user.type(files, bad);
      expect(button).toBeDisabled();
    }
    await user.clear(files);
    await user.type(files, 'CLAUDE.md{Enter}  .github/copilot-instructions.md  {Enter}');
    await user.click(button);
    expect(save).toHaveBeenLastCalledWith({
      instructionMaxFileKib: 4,
      instructionMaxTotalKib: 64,
      instructionExtraFiles: ['CLAUDE.md', '.github/copilot-instructions.md'],
      maxSkillSizeKib: null,
    });
    await user.click(screen.getByLabelText('Manual'));
    const skill = screen.getByLabelText('Max skill size (KiB)');
    for (const value of ['31', '257']) {
      await user.clear(skill);
      await user.type(skill, value);
      expect(button).toBeDisabled();
    }
    await user.clear(skill);
    await user.type(skill, '96');
    await user.click(button);
    expect(save).toHaveBeenLastCalledWith(expect.objectContaining({ maxSkillSizeKib: 96 }));
  });
  it('loads saved instruction settings', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'preferences').mockResolvedValue({
      instructionMaxFileKib: 2,
      instructionMaxTotalKib: 128,
      instructionExtraFiles: ['CLAUDE.md', 'GEMINI.md'],
    });
    render(MemoryDialog, { props: { onClose: vi.fn(), embedded: true, section: 'instructions' } });
    const file = await screen.findByLabelText('Instruction file limit (KiB)');
    await vi.waitFor(() => expect(file).toBeEnabled());
    expect(file).toHaveValue(2);
    expect(screen.getByLabelText('Total instruction limit (KiB)')).toHaveValue(128);
    expect(screen.getByLabelText('Extra instruction files, one per line')).toHaveValue(
      'CLAUDE.md\nGEMINI.md',
    );
  });
  it('accepts only relative extra instruction file paths inside the project', () => {
    for (const good of ['CLAUDE.md', '.github/copilot-instructions.md', 'docs/AGENTS.md'])
      expect(instructionFileValid(good)).toBe(true);
    for (const bad of [
      '',
      '../CLAUDE.md',
      'a/../b.md',
      './CLAUDE.md',
      '/CLAUDE.md',
      'C:CLAUDE.md',
      '\\\\server\\share',
      '.git/CLAUDE.md',
      'a//b.md',
      'agents.override.md',
      'x'.repeat(257),
      'bad\u0000name',
    ])
      expect(instructionFileValid(bad)).toBe(false);
  });
  it('keeps the draft available when saving fails', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'saveMemory').mockRejectedValue(new Error('disk full'));
    render(MemoryDialog, { props: { onClose: vi.fn() } });
    const input = await screen.findByLabelText('Remember something');
    await vi.waitFor(() => expect(input).toBeEnabled());
    await user.type(input, 'Keep this draft.');
    await user.click(screen.getByRole('button', { name: 'Save memory' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('could not be saved');
    expect(input).toHaveValue('Keep this draft.');
  });
  const proposal: MemoryProposal = {
    id: 'proposal-one',
    entry: {
      id: 'fact-one',
      content: 'Run offline tests.',
      updatedAt: 1,
      scope: 'project',
      workspace: '/project',
      key: 'tests',
      revision: 1,
    },
    before: { id: 'fact-one', content: 'Run tests.', updatedAt: 1, revision: 1 },
    source: { sessionId: 'session-example', messageId: 'message-example' },
    createdAt: 2,
  };
  it('shows provenance, scope and replacement text and approves the edited proposal', async () => {
    const user = userEvent.setup();
    vi.mocked(persistence.memoryProposals).mockResolvedValue([proposal]);
    const approve = vi.spyOn(persistence, 'approveMemory').mockResolvedValue();
    render(MemoryDialog, { props: { onClose: vi.fn() } });
    const draft = await screen.findByLabelText('Edit proposal: tests');
    vi.spyOn(persistence, 'load').mockResolvedValue({
      id: 'session-example',
      title: 'Example',
      updatedAt: 1,
      messages: [
        {
          id: 'message-example',
          role: 'user',
          content: 'Please use offline tests for this project.',
          attachments: [],
          status: 'complete',
          createdAt: 1,
        },
      ],
    });
    await user.click(screen.getByRole('button', { name: 'View source message' }));
    expect(
      await screen.findByText('Please use offline tests for this project.'),
    ).toBeInTheDocument();

    expect(screen.getByText('project · tests')).toBeInTheDocument();
    expect(screen.getByText(/session-example/)).toHaveTextContent('message-example');
    expect(screen.getByText('Run tests.')).toBeInTheDocument();
    await user.clear(draft);
    await user.type(draft, 'Run the fast offline suite.');
    vi.mocked(persistence.memoryProposals).mockResolvedValue([]);
    await user.click(screen.getByRole('button', { name: 'Approve' }));
    expect(approve).toHaveBeenCalledWith('proposal-one', 'Run the fast offline suite.');
    await vi.waitFor(() =>
      expect(screen.queryByLabelText('Edit proposal: tests')).not.toBeInTheDocument(),
    );
  });
  it('keeps a stale proposal and the edited draft available and lets the owner reject it', async () => {
    const user = userEvent.setup();
    vi.mocked(persistence.memoryProposals).mockResolvedValue([proposal]);
    vi.spyOn(persistence, 'approveMemory').mockRejectedValue(
      'This memory changed or was forgotten. Refresh and review a new proposal.',
    );
    const reject = vi.spyOn(persistence, 'rejectMemory').mockResolvedValue();
    render(MemoryDialog, { props: { onClose: vi.fn() } });
    const draft = await screen.findByLabelText('Edit proposal: tests');
    await user.clear(draft);
    await user.type(draft, 'Keep this edit.');
    await user.click(screen.getByRole('button', { name: 'Approve' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('changed or was forgotten');
    expect(draft).toHaveValue('Keep this edit.');
    vi.mocked(persistence.memoryProposals).mockResolvedValue([]);
    await user.click(screen.getByRole('button', { name: 'Reject' }));
    expect(reject).toHaveBeenCalledWith('proposal-one');
  });
  it('preserves fact identity and revision when editing and supports forgetting', async () => {
    const user = userEvent.setup();
    const entry = { ...proposal.entry, source: proposal.source, revision: 7 };
    vi.mocked(persistence.memories).mockResolvedValue([entry]);
    const save = vi.spyOn(persistence, 'saveMemory').mockResolvedValue();
    const forget = vi.spyOn(persistence, 'removeMemory').mockResolvedValue();
    render(MemoryDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Edit' }));
    const content = screen.getByLabelText('Edit memory');
    await user.clear(content);
    await user.type(content, 'User-edited convention.');
    await user.click(screen.getByRole('button', { name: 'Save changes' }));
    expect(save).toHaveBeenCalledWith(
      expect.objectContaining({
        id: entry.id,
        scope: 'project',
        workspace: '/project',
        key: 'tests',
        revision: 7,
        content: 'User-edited convention.',
      }),
    );
    await user.click(screen.getByRole('button', { name: /Delete memory/ }));
    expect(forget).toHaveBeenCalledWith(entry.id);
  });
});
