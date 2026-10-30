import userEvent from '@testing-library/user-event';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/svelte';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import type { ChatRequest, StreamCallbacks, SessionSummary } from './lib/types';
import type { Preferences } from './lib/persistence';

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
import { KEYRING_NEEDED } from './lib/auth';

let saved: Map<string, SessionSummary>;
beforeEach(() => {
  Object.defineProperty(window, '__TAURI_INTERNALS__', { value: {}, configurable: true });
  saved = new Map();
  native.invoke.mockReset().mockImplementation(async (command, args) => {
    switch (command) {
      case 'auth_status':
        return { locked: false, enabled: false, keyring: true, keyringNeeded: false };
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

it('lets Chat opt into web research without a project and resets access for a new chat', async () => {
  const user = userEvent.setup();
  render(App);
  const input = await screen.findByRole('textbox', { name: 'Message Blackwall' });
  await screen.findByRole('combobox', { name: 'Model' });
  const web = screen.getByRole('checkbox', { name: 'Web access' });
  expect(web).not.toBeChecked();
  expect(screen.queryByRole('button', { name: 'Choose project folder' })).not.toBeInTheDocument();
  expect(
    screen.queryByRole('combobox', { name: 'Command execution profile' }),
  ).not.toBeInTheDocument();
  await user.click(web);
  await user.type(input, 'Find current information{Enter}');
  await waitFor(() =>
    expect(vi.mocked(localModelClient.streamChat).mock.calls.at(-1)?.[0]).toMatchObject({
      agentMode: false,
      webEnabled: true,
      workspace: undefined,
    }),
  );
  await waitFor(() => expect(web).toBeEnabled());
  await fireEvent.click(screen.getByRole('button', { name: /New chat/ }));
  expect(web).not.toBeChecked();
});

it('uses only sidebar modes, separates histories, and keeps and restores Agent projects', async () => {
  const user = userEvent.setup();
  render(App);
  const input = await screen.findByRole('textbox', { name: 'Message Blackwall' });
  const historyModes = within(screen.getByLabelText('History mode'));
  await screen.findByRole('combobox', { name: 'Model' });
  expect(screen.getAllByRole('button', { name: 'Chat' })).toHaveLength(1);
  expect(screen.getAllByRole('button', { name: 'Agent' })).toHaveLength(1);
  expect(screen.queryByRole('button', { name: 'Choose project folder' })).not.toBeInTheDocument();
  await user.type(input, 'Plain conversation{Enter}');
  await screen.findByRole('button', { name: /Plain conversation/ });
  await fireEvent.click(historyModes.getByRole('button', { name: 'Agent' }));
  await screen.findByRole('button', { name: 'Choose project folder' });
  expect(screen.queryByRole('button', { name: /Plain conversation/ })).not.toBeInTheDocument();
  await fireEvent.click(screen.getByRole('button', { name: 'Choose project folder' }));
  await user.type(input, 'Project task{Enter}');
  await waitFor(() => expect(screen.getByRole('button', { name: 'demo' })).toBeDisabled());
  expect(historyModes.getByRole('button', { name: 'Agent' })).toHaveAttribute(
    'aria-pressed',
    'true',
  );
  expect(vi.mocked(localModelClient.streamChat).mock.calls.at(-1)?.[0]).toMatchObject({
    agentMode: true,
    workspace: '/projects/demo',
  });
  await fireEvent.click(screen.getByRole('button', { name: /New chat/ }));
  expect(await screen.findByRole('button', { name: 'demo' })).toBeEnabled();
  expect(historyModes.getByRole('button', { name: 'Agent' })).toHaveClass('active');
  await fireEvent.click(screen.getByRole('button', { name: /Project task/ }));
  await waitFor(() => expect(screen.getByRole('button', { name: 'demo' })).toBeDisabled());
  await fireEvent.click(historyModes.getByRole('button', { name: 'Chat' }));
  await screen.findByRole('button', { name: /Plain conversation/ });
  expect(screen.queryByRole('button', { name: /Project task/ })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: 'demo' })).not.toBeInTheDocument();
});

it('sends the selected command boundary and resets full access on a new session', async () => {
  const user = userEvent.setup();
  render(App);
  const input = await screen.findByRole('textbox', { name: 'Message Blackwall' });
  await screen.findByRole('combobox', { name: 'Model' });
  await fireEvent.click(
    within(screen.getByLabelText('History mode')).getByRole('button', { name: 'Agent' }),
  );
  const profile = screen.getByRole('combobox', { name: 'Command execution profile' });
  const network = screen.getByRole('checkbox', { name: 'Command network' });
  expect(profile).toHaveValue('read-only');
  expect(network).not.toBeChecked();
  await user.selectOptions(profile, 'workspace-write');
  await user.click(network);
  await user.click(screen.getByRole('button', { name: 'Choose project folder' }));
  await user.type(input, 'Check project{Enter}');
  await waitFor(() =>
    expect(vi.mocked(localModelClient.streamChat).mock.calls.at(-1)?.[0]).toMatchObject({
      executionProfile: 'workspace-write',
      commandNetwork: true,
    }),
  );
  await waitFor(() => expect(profile).toBeEnabled());
  await user.selectOptions(profile, 'unsandboxed');
  expect(screen.getByText(/Unsandboxed commands have full account/)).toBeInTheDocument();
  expect(network).toBeDisabled();
  expect(network).toBeChecked();
  await user.click(screen.getByRole('button', { name: /New chat/ }));
  expect(profile).toHaveValue('read-only');
  expect(network).not.toBeChecked();
});

it('keeps Read-only, disables Workspace-write and shows the bubblewrap note when unavailable', async () => {
  const user = userEvent.setup();
  const note =
    "Sandboxed commands on Linux need Bubblewrap (bwrap) 0.12 or newer. Install Blackwall's .deb or AppImage, which include it, or a newer bwrap, then restart Blackwall.";
  const fallback = native.invoke.getMockImplementation();
  native.invoke.mockImplementation(async (command, args) =>
    command === 'sandbox_status'
      ? { available: false, note, workspaceWrite: false }
      : fallback?.(command, args),
  );
  render(App);
  await screen.findByRole('combobox', { name: 'Model' });
  expect(screen.queryByText(note)).not.toBeInTheDocument();
  await fireEvent.click(
    within(screen.getByLabelText('History mode')).getByRole('button', { name: 'Agent' }),
  );
  const profile = screen.getByRole('combobox', { name: 'Command execution profile' });
  expect(await screen.findByText(note)).toBeInTheDocument();
  // Read-only is the safe default and stays on offer; commands fail closed until it works.
  expect(within(profile).getByRole('option', { name: 'Read-only' })).toBeEnabled();
  expect(profile).toHaveValue('read-only');
  expect(within(profile).getByRole('option', { name: 'Workspace-write' })).toBeDisabled();
  expect(within(profile).getByRole('option', { name: 'Unsandboxed (full access)' })).toBeEnabled();
  await user.click(screen.getByRole('button', { name: 'Learn more' }));
  expect(native.invoke).toHaveBeenCalledWith('open_setup_link', {
    kind: 'sandbox-docs',
    invitation: null,
  });
  await user.selectOptions(profile, 'unsandboxed');
  await user.selectOptions(profile, 'read-only');
  expect(profile).toHaveValue('read-only');
});

it('checks the sandbox again on focus and about 30 seconds after it was unavailable', async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  try {
    const note = "Bubblewrap didn't start just now. Try again in a moment.";
    let available = false;
    const fallback = native.invoke.getMockImplementation();
    native.invoke.mockImplementation(async (command, args) =>
      command === 'sandbox_status'
        ? { available, note: available ? null : note, workspaceWrite: false }
        : fallback?.(command, args),
    );
    const checks = () =>
      native.invoke.mock.calls.filter(([command]) => command === 'sandbox_status').length;
    render(App);
    await screen.findByRole('combobox', { name: 'Model' });
    expect(checks()).toBe(0);
    await fireEvent.click(
      within(screen.getByLabelText('History mode')).getByRole('button', { name: 'Agent' }),
    );
    expect(await screen.findByText(note)).toBeInTheDocument();
    expect(checks()).toBe(1);
    // Still failing 30 seconds later: asked once more, and not again after that.
    await vi.advanceTimersByTimeAsync(30_000);
    expect(checks()).toBe(2);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(checks()).toBe(2);
    // Coming back to the window asks again, and a working sandbox clears the note.
    available = true;
    await fireEvent.focus(window);
    await waitFor(() => expect(screen.queryByText(note)).not.toBeInTheDocument());
    expect(checks()).toBe(3);
    await vi.advanceTimersByTimeAsync(30_000);
    expect(checks()).toBe(3);
  } finally {
    vi.useRealTimers();
  }
});

it('applies only the latest sandbox answer and stops rechecking outside Agent mode', async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  try {
    const note = "Bubblewrap didn't start just now. Try again in a moment.";
    const replies: ((status: unknown) => void)[] = [];
    const fallback = native.invoke.getMockImplementation();
    native.invoke.mockImplementation(async (command, args) =>
      command === 'sandbox_status'
        ? new Promise((resolve) => replies.push(resolve))
        : fallback?.(command, args),
    );
    render(App);
    await screen.findByRole('combobox', { name: 'Model' });
    await fireEvent.click(
      within(screen.getByLabelText('History mode')).getByRole('button', { name: 'Agent' }),
    );
    await waitFor(() => expect(replies).toHaveLength(1));
    await fireEvent.focus(window);
    await waitFor(() => expect(replies).toHaveLength(2));
    // The newer answer arrives first; the older one, arriving late, is ignored.
    replies[1]({ available: true, note: null, workspaceWrite: false });
    replies[0]({ available: false, note, workspaceWrite: false });
    await vi.advanceTimersByTimeAsync(0);
    expect(screen.queryByText(note)).not.toBeInTheDocument();
    // An unavailable answer arms the 30-second recheck, which is skipped once Agent mode is left.
    await fireEvent.focus(window);
    await waitFor(() => expect(replies).toHaveLength(3));
    replies[2]({ available: false, note, workspaceWrite: false });
    expect(await screen.findByText(note)).toBeInTheDocument();
    await fireEvent.click(
      within(screen.getByLabelText('History mode')).getByRole('button', { name: 'Chat' }),
    );
    await vi.advanceTimersByTimeAsync(30_000);
    expect(replies).toHaveLength(3);
  } finally {
    vi.useRealTimers();
  }
});

it('says what command network reaches when it is on', async () => {
  const user = userEvent.setup();
  const hint =
    "With command network on, sandboxed commands share this computer's network, so they can reach services on this computer (localhost) and your local network.";
  render(App);
  await screen.findByRole('combobox', { name: 'Model' });
  await fireEvent.click(
    within(screen.getByLabelText('History mode')).getByRole('button', { name: 'Agent' }),
  );
  const network = screen.getByRole('checkbox', { name: 'Command network' });
  expect(screen.queryByText(hint)).not.toBeInTheDocument();
  await user.click(network);
  expect(screen.getByText(hint)).toBeInTheDocument();
  // Unsandboxed has its own, stronger notice.
  await user.selectOptions(
    screen.getByRole('combobox', { name: 'Command execution profile' }),
    'unsandboxed',
  );
  expect(screen.queryByText(hint)).not.toBeInTheDocument();
  await user.selectOptions(
    screen.getByRole('combobox', { name: 'Command execution profile' }),
    'read-only',
  );
  await user.click(network);
  expect(screen.queryByText(hint)).not.toBeInTheDocument();
});

it('offers Workspace-write only where its overlay works, and says why when it does not', async () => {
  const note =
    "Workspace-write isn't available on this computer: it needs overlay mounts inside the sandbox, which this system doesn't allow (they need Linux 5.11 or newer). Read-only commands still run in the sandbox.";
  let status = { available: true, note: note as string | null, workspaceWrite: false };
  const fallback = native.invoke.getMockImplementation();
  native.invoke.mockImplementation(async (command, args) =>
    command === 'sandbox_status' ? status : fallback?.(command, args),
  );
  render(App);
  await screen.findByRole('combobox', { name: 'Model' });
  await fireEvent.click(
    within(screen.getByLabelText('History mode')).getByRole('button', { name: 'Agent' }),
  );
  const profile = screen.getByRole('combobox', { name: 'Command execution profile' });
  await waitFor(() =>
    expect(within(profile).getByRole('option', { name: 'Workspace-write' })).toBeDisabled(),
  );
  expect(within(profile).getByRole('option', { name: 'Read-only' })).toBeEnabled();
  expect(await screen.findByText(note)).toBeInTheDocument();
  // Where the overlay works too, Workspace-write is on offer and there is nothing to explain.
  status = { available: true, note: null, workspaceWrite: true };
  await fireEvent.focus(window);
  await waitFor(() =>
    expect(within(profile).getByRole('option', { name: 'Workspace-write' })).toBeEnabled(),
  );
  expect(screen.queryByText(note)).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: 'Learn more' })).not.toBeInTheDocument();
});

it('asks which server to use on a first launch instead of connecting on its own', async () => {
  const user = userEvent.setup();
  const discoverModels = vi.spyOn(localModelClient, 'discoverModels');
  discoverModels.mockClear();
  const found = [
    {
      name: 'Ollama',
      endpoint: 'http://127.0.0.1:11434/v1',
      available: true,
      models: ['qwen3.6:35b-a3b-nvfp4'],
      supportsDownload: true,
      kind: 'ollama',
    },
    {
      name: 'llama.cpp',
      endpoint: 'http://127.0.0.1:18040/v1',
      available: true,
      models: ['qwen3.8-flash-next-iq2_xs'],
      supportsDownload: false,
      kind: 'llama.cpp',
      loaded: ['qwen3.8-flash-next-iq2_xs'],
    },
  ];
  const fallback = native.invoke.getMockImplementation();
  native.invoke.mockImplementation(async (command, args) =>
    command === 'discover_local_services' ? found : fallback?.(command, args),
  );
  render(App);
  const list = within(await screen.findByRole('region', { name: 'Found on this computer' }));
  expect(list.getByRole('button', { name: 'Connect to Ollama at 127.0.0.1:11434' })).toBeVisible();
  expect(
    list.getByRole('button', { name: 'Connect to llama.cpp at 127.0.0.1:18040' }),
  ).toBeVisible();
  expect(discoverModels).not.toHaveBeenCalled();
  await user.click(list.getByRole('button', { name: 'Connect to llama.cpp at 127.0.0.1:18040' }));
  expect(await screen.findByRole('heading', { name: 'You’re connected.' })).toBeVisible();
  expect(discoverModels).toHaveBeenCalledWith('http://127.0.0.1:18040/v1');
});

it('reconnects a saved connection at startup without asking again', async () => {
  const fallback = native.invoke.getMockImplementation();
  native.invoke.mockImplementation(async (command, args) => {
    if (command === 'load_preferences')
      return {
        endpoint: 'https://model.example/v1',
        connections: [{ endpoint: 'https://model.example/v1', name: 'Saved' }],
      };
    if (command === 'discover_local_services') throw new Error('discovery must not run');
    return fallback?.(command, args);
  });
  render(App);
  expect(await screen.findByRole('textbox', { name: 'Message Blackwall' })).toBeVisible();
  expect(screen.queryByRole('region', { name: 'Found on this computer' })).not.toBeInTheDocument();
});

it('persists text tool calls and forgotten connections across a desktop restart', async () => {
  const user = userEvent.setup();
  let preferences: Preferences = {
    endpoint: 'https://model.example/v1',
    connections: [
      { endpoint: 'https://model.example/v1', name: 'Current' },
      { endpoint: 'https://other.example/v1', name: 'Other' },
    ],
  };
  const fallback = native.invoke.getMockImplementation();
  native.invoke.mockImplementation(async (command, args) => {
    if (command === 'load_preferences') return structuredClone(preferences);
    if (command === 'save_preferences') {
      preferences = { ...preferences, ...structuredClone(args.preferences) };
      return;
    }
    return fallback?.(command, args);
  });

  const first = render(App);
  await screen.findByRole('textbox', { name: 'Message Blackwall' });
  await user.click(screen.getByRole('button', { name: 'Open settings' }));
  await user.click(screen.getByRole('button', { name: /Model connection/ }));
  await user.click(screen.getByRole('checkbox', { name: /writes tool calls as text/ }));
  await user.click(screen.getByRole('button', { name: 'Forget Other' }));
  await waitFor(() =>
    expect(preferences.connections).toEqual([
      { endpoint: 'https://model.example/v1', name: 'Current', textToolCalls: true },
    ]),
  );
  first.unmount();

  render(App);
  await screen.findByRole('textbox', { name: 'Message Blackwall' });
  await user.click(screen.getByRole('button', { name: 'Open settings' }));
  await user.click(screen.getByRole('button', { name: /Model connection/ }));
  expect(screen.getByRole('checkbox', { name: /writes tool calls as text/ })).toBeChecked();
  expect(screen.queryByRole('button', { name: 'Forget Other' })).not.toBeInTheDocument();
});

it('shows the real error after a keyring retry fails instead of the keyring message', async () => {
  const user = userEvent.setup();
  const failure = 'Blackwall could not read its app lock from the system keyring.';
  const fallback = native.invoke.getMockImplementation();
  let reads = 0;
  native.invoke.mockImplementation(async (command, args) => {
    if (command !== 'auth_status') return fallback?.(command, args);
    reads += 1;
    if (reads === 1) return { locked: true, enabled: true, keyring: false, keyringNeeded: true };
    throw failure;
  });
  render(App);
  expect(await screen.findByText(KEYRING_NEEDED)).toBeVisible();
  await user.click(screen.getByRole('button', { name: 'Try again' }));
  expect(await screen.findByText(failure)).toBeVisible();
  expect(screen.getByRole('button', { name: 'Try again' })).toBeEnabled();
  expect(screen.queryByText(KEYRING_NEEDED)).not.toBeInTheDocument();
});
