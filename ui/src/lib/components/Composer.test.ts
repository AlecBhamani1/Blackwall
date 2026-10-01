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

  it('selects a chat file with the keyboard before Enter sends the message', async () => {
    const user = userEvent.setup();
    const onSend = vi.fn().mockResolvedValue(true);
    render(Composer, {
      onSend,
      onStop: vi.fn(),
      chatAttachments: [
        { id: 'one', name: 'notes.md', kind: 'text', mimeType: 'text/markdown', sizeBytes: 5 },
        {
          id: 'two',
          name: 'Screenshot 1.png',
          kind: 'image',
          mimeType: 'image/png',
          sizeBytes: 10,
        },
      ],
    });
    const textbox = screen.getByRole('textbox', { name: 'Message Blackwall' });
    await user.type(textbox, 'Read @');
    expect(screen.getByRole('listbox', { name: 'Files in this chat' })).toBeInTheDocument();
    await user.keyboard('{ArrowDown}{Enter}');
    expect(onSend).not.toHaveBeenCalled();
    expect(textbox).toHaveValue('Read @"Screenshot 1.png" ');
    expect(screen.queryByRole('listbox')).not.toBeInTheDocument();
    await user.keyboard('{Enter}');
    expect(onSend).toHaveBeenCalledWith('Read @"Screenshot 1.png" ', []);
  });

  it('filters pending files and supports clicking and dismissing suggestions', async () => {
    const user = userEvent.setup();
    const { container, onSend } = renderComposer();
    await user.upload(container.querySelector<HTMLInputElement>('input[type="file"]')!, [
      new File(['one'], 'notes.md'),
      new File(['two'], 'report.txt'),
    ]);
    const textbox = screen.getByRole('textbox', { name: 'Message Blackwall' });
    await user.type(textbox, 'Read @note');
    expect(screen.getAllByRole('option')).toHaveLength(1);
    await user.click(screen.getByRole('option', { name: 'notes.md' }));
    expect(textbox).toHaveValue('Read @notes.md ');
    await user.type(textbox, '@');
    await user.keyboard('{Escape}');
    expect(screen.queryByRole('listbox')).not.toBeInTheDocument();
    expect(onSend).not.toHaveBeenCalled();
  });

  it('pastes a screenshot as an image and suppresses the clipboard fallback text', async () => {
    renderComposer();
    const textbox = screen.getByRole('textbox', { name: 'Message Blackwall' });
    const screenshot = new File(['pixels'], 'Screenshot.png', { type: 'image/png' });
    expect(
      await fireEvent.paste(textbox, {
        clipboardData: {
          items: [{ kind: 'file', type: 'image/png', getAsFile: () => screenshot }],
        },
      }),
    ).toBe(false);
    expect(screen.getByAltText('Preview of Screenshot.png')).toBeInTheDocument();
  });

  it('clears file suggestions and the draft when changing conversations', async () => {
    const user = userEvent.setup();
    const { rerender } = render(Composer, {
      onSend: vi.fn(),
      onStop: vi.fn(),
      sessionId: 'first',
      chatAttachments: [
        { id: 'one', name: 'private.txt', kind: 'text', mimeType: 'text/plain', sizeBytes: 5 },
      ],
    });
    const textbox = screen.getByRole('textbox', { name: 'Message Blackwall' });
    await user.type(textbox, '@');
    expect(screen.getByRole('option', { name: 'private.txt' })).toBeInTheDocument();
    await rerender({ sessionId: 'second', chatAttachments: [] });
    expect(screen.queryByRole('listbox')).not.toBeInTheDocument();
    expect(textbox).toHaveValue('');
  });
});
