import { render, screen, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { expect, it, vi } from 'vitest';
import Sidebar from './Sidebar.svelte';

it('manages settings sections on one page and returns focus after closing Settings', async () => {
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
  const settings = screen.getByRole('region', { name: 'Settings' });
  const pane = within(settings).getByTestId('settings-content-pane');

  for (const panel of ['Saved memories', 'Skills', 'MCP servers', 'Runs']) {
    const button = within(settings).getByRole('button', { name: new RegExp(panel) });
    await user.click(button);
    expect(within(settings).getByTestId('settings-content-pane')).toBe(pane);
    expect(
      within(pane).getByRole('heading', { name: panel === 'Saved memories' ? 'Memory' : panel }),
    ).toBeInTheDocument();
  }

  await user.click(within(settings).getByRole('button', { name: 'Limits' }));
  expect(within(pane).getByLabelText('Model context limit (tokens)')).toHaveValue(8192);
  await user.click(within(pane).getByRole('button', { name: 'Save context limit' }));
      expect(onPreferences).toHaveBeenCalledWith(
        expect.objectContaining({ contextWindow: 8192, maxIterations: 30 }),
      );

  await user.keyboard('{Escape}');
  expect(screen.queryByRole('region', { name: 'Settings' })).not.toBeInTheDocument();
  expect(sidebar.getByRole('button', { name: 'Open settings' })).toHaveFocus();
});
