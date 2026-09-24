import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { persistence } from '../persistence';
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
});
