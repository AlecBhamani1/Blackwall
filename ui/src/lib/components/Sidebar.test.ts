import { fireEvent, render, screen, waitFor, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { expect, it, vi } from 'vitest';
import Sidebar from './Sidebar.svelte';

it('manages Memory, Skills, MCP servers, and Runs through Settings and returns focus after closing each panel', async () => {
  const user = userEvent.setup();
  Object.defineProperties(HTMLDialogElement.prototype, {
    showModal: {
      configurable: true,
      value: function (this: HTMLDialogElement) {
        this.open = true;
      },
    },
    close: {
      configurable: true,
      value: function (this: HTMLDialogElement) {
        this.open = false;
        this.dispatchEvent(new Event('close'));
      },
    },
  });
  const onPreferences = vi.fn();
  render(Sidebar, {
    props: {
      contextPreferences: { contextWindow: 8192, maxIterations: 30 },
      onPreferences,
      onNewChat: vi.fn(),
      onOpenSession: vi.fn(),
      onRemoveSession: vi.fn(),
      onConfigureEndpoint: vi.fn().mockResolvedValue(true),
      onCheckForUpdates: vi.fn().mockResolvedValue(false),
      onInstallUpdate: vi.fn().mockResolvedValue(undefined),
      onClose: vi.fn(),
    },
  });

  const sidebar = within(screen.getByRole('complementary', { name: 'Chat history' }));
  for (const name of ['Memory', 'Skills', 'MCP servers', 'Runs']) {
    expect(sidebar.queryByRole('button', { name: new RegExp(name) })).not.toBeInTheDocument();
  }
  await user.click(sidebar.getByRole('button', { name: 'Open settings' }));
  const settings = screen.getByRole('dialog', { name: 'Settings' });

  for (const panel of ['Memory', 'Skills', 'MCP servers', 'Runs']) {
    const button = within(settings).getByRole('button', { name: new RegExp(panel) });
    await user.click(button);
    const dialog = screen.getByRole('dialog', { name: panel });
    expect(settings.parentElement).toHaveProperty('inert', true);

    if (panel === 'Memory') {
      expect(within(dialog).getByLabelText('Model context limit (tokens)')).toHaveValue(8192);
      await user.click(within(dialog).getByText('Advanced · Context window'));
      await user.click(within(dialog).getByRole('button', { name: 'Save context limit' }));
      expect(onPreferences).toHaveBeenCalledWith(
        expect.objectContaining({ contextWindow: 8192, maxIterations: 30 }),
      );
    }

    // Native dialogs handle Escape themselves; it must not also close Settings.
    await fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(settings).toBeInTheDocument();
    await user.click(
      within(dialog).getByRole('button', {
        name: `Close ${panel === 'MCP servers' ? panel : panel.toLowerCase()}`,
      }),
    );
    await waitFor(() => expect(button).toHaveFocus());
    expect(screen.queryByRole('dialog', { name: panel })).not.toBeInTheDocument();
    expect(settings.parentElement).toHaveProperty('inert', false);
  }

  await user.keyboard('{Escape}');
  expect(screen.queryByRole('dialog', { name: 'Settings' })).not.toBeInTheDocument();
  expect(sidebar.getByRole('button', { name: 'Open settings' })).toHaveFocus();
});
