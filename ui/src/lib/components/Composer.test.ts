import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import Composer from './Composer.svelte';

function renderComposer(onSend = vi.fn().mockResolvedValue(true)) {
  return {
    onSend,
    ...render(Composer, {
      props: {
        onSend,
        onStop: vi.fn(),
      },
    }),
  };
}

describe('Composer', () => {
  it('sends with Enter and clears only after the message is accepted', async () => {
    const user = userEvent.setup();
    const { onSend } = renderComposer();
    const textbox = screen.getByRole('textbox', { name: 'Message Blackwall' });

    await user.type(textbox, 'Hello from Blackwall{Enter}');

    await waitFor(() => expect(onSend).toHaveBeenCalledWith('Hello from Blackwall', []));
    await waitFor(() => expect(textbox).toHaveValue(''));
  });

  it('keeps the draft when the controller declines a send', async () => {
    const user = userEvent.setup();
    const { onSend } = renderComposer(vi.fn().mockResolvedValue(false));
    const textbox = screen.getByRole('textbox', { name: 'Message Blackwall' });

    await user.type(textbox, 'Keep this{Enter}');

    await waitFor(() => expect(onSend).toHaveBeenCalledOnce());
    expect(textbox).toHaveValue('Keep this');
  });

  it('uses Shift+Enter for a newline instead of sending', async () => {
    const user = userEvent.setup();
    const { onSend } = renderComposer();
    const textbox = screen.getByRole('textbox', { name: 'Message Blackwall' });

    await user.type(textbox, 'First{Shift>}{Enter}{/Shift}Second');

    expect(onSend).not.toHaveBeenCalled();
    expect(textbox).toHaveValue('First\nSecond');
  });

  it('adds and removes multiple picked files', async () => {
    const user = userEvent.setup();
    const { container } = renderComposer();
    const input = container.querySelector<HTMLInputElement>('input[type="file"]');
    expect(input).not.toBeNull();

    const image = new File(['image'], 'desk.png', { type: 'image/png' });
    const notes = new File(['notes'], 'notes.txt', { type: 'text/plain' });
    await user.upload(input as HTMLInputElement, [image, notes]);

    expect(screen.getByText('desk.png')).toBeInTheDocument();
    expect(screen.getByText('notes.txt')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Remove notes.txt' }));
    expect(screen.queryByText('notes.txt')).not.toBeInTheDocument();
  });

  it('accepts dropped files', async () => {
    renderComposer();
    const composer = screen.getByRole('group', { name: 'Message composer' });
    const dataTransfer = {
      files: [new File(['hello'], 'drop.md', { type: 'text/markdown' })],
      types: ['Files'],
      dropEffect: 'none',
    };

    await fireEvent.dragEnter(composer, { dataTransfer });
    expect(screen.getByText('Drop files here')).toBeInTheDocument();
    await fireEvent.drop(composer, { dataTransfer });
    expect(screen.getByText('drop.md')).toBeInTheDocument();
  });
});
