import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { persistence, type MemoryProposal } from '../persistence';
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
