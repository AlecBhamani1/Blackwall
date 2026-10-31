import { fireEvent, render, screen, waitFor, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { persistence } from '../persistence';
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
  it('holds the model and Chat controls in its footer without the old filler', () => {
    render(Composer, {
      props: {
        onSend: vi.fn(),
        onStop: vi.fn(),
        models: [{ id: 'model', name: 'Test model' }],
      },
    });
    const composer = screen.getByRole('group', { name: 'Message composer' });
    expect(within(composer).getByRole('combobox', { name: 'Model' })).toBeInTheDocument();
    expect(within(composer).getByRole('checkbox', { name: 'Web access' })).toBeInTheDocument();
    expect(
      within(composer).getByRole('button', { name: 'Add photos or files' }),
    ).toBeInTheDocument();
    expect(within(composer).getByRole('button', { name: 'Send message' })).toBeInTheDocument();
    expect(within(composer).queryByText('Sent only to your model')).not.toBeInTheDocument();
    expect(
      within(composer).queryByRole('combobox', { name: 'Command execution profile' }),
    ).not.toBeInTheDocument();
  });

  it('adds Agent command controls to the composer footer', () => {
    render(Composer, { props: { onSend: vi.fn(), onStop: vi.fn(), agentMode: true } });
    const composer = screen.getByRole('group', { name: 'Message composer' });
    expect(
      within(composer).getByRole('combobox', { name: 'Command execution profile' }),
    ).toBeInTheDocument();
    expect(within(composer).getByRole('checkbox', { name: 'Command network' })).toBeInTheDocument();
  });

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
  it('lists commands and enabled skills after / and inserts the chosen one', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'skills').mockResolvedValue({
      skills: [
        { name: 'pdf', description: 'Fill PDFs.', body: 'b', enabled: true, userOnly: true },
        { name: 'off', description: 'Off.', body: 'b', enabled: false },
      ],
      warnings: [],
    });
    const { onSend } = renderComposer();
    const textbox = screen.getByRole('textbox', { name: 'Message Blackwall' });
    await user.type(textbox, '/');
    expect(await screen.findByRole('option', { name: /\/pdf/ })).toBeInTheDocument();
    expect(screen.queryByRole('option', { name: /\/off/ })).not.toBeInTheDocument();
    await user.type(textbox, 'pd');
    expect(screen.getAllByRole('option')).toHaveLength(1);
    await user.keyboard('{Enter}');
    expect(textbox).toHaveValue('/pdf ');
    expect(onSend).not.toHaveBeenCalled();
    await user.type(textbox, 'fill this{Enter}');
    expect(onSend).toHaveBeenCalledWith('/pdf fill this', []);
  });

  it('makes every command reachable and scrolls keyboard selections into view', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'skills').mockResolvedValue({
      skills: Array.from({ length: 12 }, (_, index) => ({
        name: `skill-${String(index).padStart(2, '0')}`,
        description: `Workflow ${index}`,
        body: 'Instructions',
        enabled: true,
      })),
      warnings: [],
    });
    const { onSend } = renderComposer();
    const textbox = screen.getByRole('textbox', { name: 'Message Blackwall' });
    await user.type(textbox, '/');
    await waitFor(() => expect(screen.getAllByRole('option')).toHaveLength(18));
    const last = screen.getByRole('option', { name: '/skill-11 Workflow 11' });
    const scrollIntoView = vi.fn();
    Object.defineProperty(last, 'scrollIntoView', { configurable: true, value: scrollIntoView });

    await user.keyboard('{ArrowUp}');

    expect(last).toHaveAttribute('aria-selected', 'true');
    expect(textbox).toHaveAttribute('aria-activedescendant', last.id);
    expect(scrollIntoView).toHaveBeenCalledWith({ block: 'nearest' });
    await user.keyboard('{Tab}');
    expect(textbox).toHaveValue('/skill-11 ');
    expect(textbox).toHaveFocus();
    expect(screen.queryByRole('listbox')).not.toBeInTheDocument();
    expect(onSend).not.toHaveBeenCalled();
  });

  it('narrows skill names while typing and inserts a clicked command', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'skills').mockResolvedValue({
      skills: ['skill-creator', 'skill-review', 'pdf'].map((name) => ({
        name,
        description: `Run ${name}`,
        body: 'Instructions',
        enabled: true,
      })),
      warnings: [],
    });
    const { onSend } = renderComposer();
    const textbox = screen.getByRole('textbox', { name: 'Message Blackwall' });
    await user.type(textbox, '/sk');
    await waitFor(() => expect(screen.getAllByRole('option')).toHaveLength(2));
    await user.type(textbox, 'ill-c');
    expect(screen.getAllByRole('option')).toHaveLength(1);
    await user.click(screen.getByRole('option', { name: '/skill-creator Run skill-creator' }));
    expect(textbox).toHaveValue('/skill-creator ');
    expect(textbox).toHaveFocus();
    expect(onSend).not.toHaveBeenCalled();
  });

  it('keeps built-in commands available when saved skills cannot be loaded', async () => {
    const user = userEvent.setup();
    const skills = vi.spyOn(persistence, 'skills').mockRejectedValue(new Error('Unavailable'));
    renderComposer();
    await user.type(screen.getByRole('textbox', { name: 'Message Blackwall' }), '/');
    await waitFor(() => expect(skills).toHaveBeenCalledOnce());
    expect(screen.getAllByRole('option')).toHaveLength(6);
    for (const name of ['compact', 'context', 'init', 'learn', 'recall', 'review']) {
      expect(screen.getByRole('option', { name: new RegExp(`^/${name} `) })).toBeInTheDocument();
    }
  });

  it('shows an empty result and lets Escape dismiss it without sending', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'skills').mockResolvedValue({ skills: [], warnings: [] });
    const { onSend } = renderComposer();
    const textbox = screen.getByRole('textbox', { name: 'Message Blackwall' });
    await user.type(textbox, '/unknown');
    expect(screen.getByRole('status')).toHaveTextContent('No matching commands');
    expect(textbox).not.toHaveAttribute('aria-activedescendant');
    await user.keyboard('{Escape}');
    expect(screen.queryByRole('listbox')).not.toBeInTheDocument();
    expect(textbox).toHaveValue('/unknown');
    expect(onSend).not.toHaveBeenCalled();
  });

  it.each(['Read /re', 'https://example.com', '/review explain changes', '/tmp/file'])(
    'does not suggest commands inside prose, URLs, arguments, or paths: %s',
    async (draft) => {
      const user = userEvent.setup();
      vi.spyOn(persistence, 'skills').mockResolvedValue({ skills: [], warnings: [] });
      renderComposer();
      await user.type(screen.getByRole('textbox', { name: 'Message Blackwall' }), draft);
      expect(screen.queryByRole('listbox')).not.toBeInTheDocument();
    },
  );

  it('replaces the whole command when editing its name and preserves arguments', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'skills').mockResolvedValue({ skills: [], warnings: [] });
    const { onSend } = renderComposer();
    const textbox = screen.getByRole('textbox', {
      name: 'Message Blackwall',
    }) as HTMLTextAreaElement;
    await user.type(textbox, '/review explain changes');
    textbox.setSelectionRange(1, 1);
    await fireEvent.click(textbox);
    await user.click(screen.getByRole('option', { name: /^\/recall / }));
    expect(textbox).toHaveValue('/recall explain changes');
    expect(textbox.selectionStart).toBe('/recall '.length);
    expect(onSend).not.toHaveBeenCalled();
  });

  it('clears command suggestions when switching conversations', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'skills').mockResolvedValue({ skills: [], warnings: [] });
    const { rerender } = render(Composer, {
      onSend: vi.fn(),
      onStop: vi.fn(),
      sessionId: 'first',
    });
    const textbox = screen.getByRole('textbox', { name: 'Message Blackwall' });
    await user.type(textbox, '/');
    await user.keyboard('{ArrowDown}');
    expect(screen.getByRole('listbox')).toBeInTheDocument();
    await rerender({ sessionId: 'second' });
    expect(textbox).toHaveValue('');
    expect(screen.queryByRole('listbox')).not.toBeInTheDocument();
    expect(textbox).not.toHaveAttribute('aria-activedescendant');
  });
});
