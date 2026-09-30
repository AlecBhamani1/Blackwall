import userEvent from '@testing-library/user-event';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/svelte';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import type { ChatRequest, StreamCallbacks, SessionSummary } from './lib/types';

const native = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke }));
vi.mock('./lib/ipc', async (importOriginal) => ({
  ...(await importOriginal<typeof import('./lib/ipc')>()),
  localModelClient: {
    modelEndpoint: async () => 'https://model.example/v1',
    discoverModels: async () => [{ id: 'test-model', name: 'Test model' }],
    streamChat: vi.fn(async (_request: ChatRequest, callbacks: StreamCallbacks) =>
      callbacks.onDelta('Task complete.'),
    ),
  },
}));
import App from './App.svelte';
import { localModelClient } from './lib/ipc';

let saved: Map<string, SessionSummary>;
beforeEach(() => {
  Object.defineProperty(window, '__TAURI_INTERNALS__', { value: {}, configurable: true });
  saved = new Map();
  native.invoke.mockReset().mockImplementation(async (command, args) => {
    switch (command) {
      case 'auth_status':
        return { locked: false, enabled: false };
      case 'load_preferences':
        return {};
      case 'list_sessions':
        return [...saved.values()];
      case 'save_session':
        saved.set(args.session.id, args.session);
        return;
      case 'load_session':
        return saved.get(args.id);
      case 'restore_workspace':
        return saved.get(args.sessionId)?.workspace;
      case 'choose_workspace':
        return '/projects/demo';
      case 'browse_workspace':
        return { entries: [], truncated: false };
      case 'pairing_status':
        return { devices: [], host: null, client: null, warnings: [] };
      case 'plugin:app|version':
        return '0.1.0';
      default:
        return null;
    }
  });
});
afterEach(() => {
  delete window.__TAURI_INTERNALS__;
});

it('keeps mode and project locked, separates histories, and resets and restores Agent projects', async () => {
  const user = userEvent.setup();
  render(App);
  const input = await screen.findByRole('textbox', { name: 'Message Blackwall' });
  const modes = within(screen.getByLabelText('Conversation mode'));
  const historyModes = within(screen.getByLabelText('History mode'));
  await screen.findByRole('combobox', { name: 'Model' });
  expect(screen.queryByRole('button', { name: 'Choose project folder' })).not.toBeInTheDocument();
  await user.type(input, 'Plain conversation{Enter}');
  await waitFor(() => expect(modes.getByRole('button', { name: 'Agent' })).toBeDisabled());
  await fireEvent.click(historyModes.getByRole('button', { name: 'Agent' }));
  await screen.findByRole('button', { name: 'Choose project folder' });
  expect(screen.queryByRole('button', { name: /Plain conversation/ })).not.toBeInTheDocument();
  await fireEvent.click(screen.getByRole('button', { name: 'Choose project folder' }));
  await user.type(input, 'Project task{Enter}');
  await waitFor(() => expect(screen.getByRole('button', { name: 'demo' })).toBeDisabled());
  expect(modes.getByRole('button', { name: 'Chat' })).toBeDisabled();
  expect(vi.mocked(localModelClient.streamChat).mock.calls.at(-1)?.[0]).toMatchObject({
    agentMode: true,
    workspace: '/projects/demo',
  });
  await fireEvent.click(screen.getByRole('button', { name: /New chat/ }));
  expect(await screen.findByRole('button', { name: 'Choose project folder' })).toBeEnabled();
  expect(modes.getByRole('button', { name: 'Agent' })).toHaveClass('active');
  await fireEvent.click(screen.getByRole('button', { name: /Project task/ }));
  expect(await screen.findByRole('button', { name: 'demo' })).toBeDisabled();
  await fireEvent.click(historyModes.getByRole('button', { name: 'Chat' }));
  await screen.findByRole('button', { name: /Plain conversation/ });
  expect(screen.queryByRole('button', { name: /Project task/ })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: 'demo' })).not.toBeInTheDocument();
});
