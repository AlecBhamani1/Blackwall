import { fireEvent, render, screen, waitFor, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { GuestShareClient } from '../ipc';
import { persistence } from '../persistence';
import SettingsDialog from './SettingsDialog.svelte';
import { credentialClient } from '../credentials';

const MODEL = 'qwen3:27b';
const SHARE_URL = 'https://blackwall.test/chat/guest-token';
const QR_DATA_URL = 'data:image/svg+xml,%3Csvg%3E%3C/svg%3E';

function mockClient(): GuestShareClient {
  return {
    shareStatus: vi.fn().mockResolvedValue({ active: false }),
    startShare: vi.fn().mockResolvedValue({
      active: true,
      shareUrl: SHARE_URL,
      qrDataUrl: QR_DATA_URL,
      model: MODEL,
      expiresAt: Date.now() + 60 * 60 * 1_000,
      networkLabel: 'Hosted relay',
      requestCount: 0,
    }),
    stopShare: vi.fn().mockResolvedValue({ active: false }),
  };
}

describe('SettingsDialog', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('keeps the owner-defined navigation order and has no sort control', () => {
    const client = mockClient();
    render(SettingsDialog, { props: { open: true, onClose: vi.fn(), client } });

    const menu = within(screen.getByRole('navigation', { name: 'Settings menu' }));
    expect(
      within(menu.getByRole('region', { name: 'General' })).getByRole('button', {
        name: /Model connection/,
      }),
    ).toBeInTheDocument();
    const groups = menu.getAllByRole('region').map((group) => group.getAttribute('aria-label'));
    expect(groups).toEqual(['General', 'Agent', 'Memory', 'Workflows']);
    expect(
      within(menu.getByRole('region', { name: 'Agent' }))
        .getAllByRole('button')
        .map((button) => button.textContent?.trim()),
    ).toEqual(['Limits', 'MCP servers', 'Skills']);
    expect(menu.queryByRole('button', { name: /Subagents/ })).not.toBeInTheDocument();
    expect(menu.queryByRole('button', { name: /Instructions/ })).not.toBeInTheDocument();
    expect(
      within(menu.getByRole('region', { name: 'Workflows' }))
        .getAllByRole('button')
        .map((button) => button.textContent?.trim()),
    ).toEqual(['Sharing & pairing', 'Runs']);
    expect(client.shareStatus).not.toHaveBeenCalled();
    expect(screen.queryByLabelText('Settings order')).not.toBeInTheDocument();
  });

  it('places rules, telemetry, and lock under Privacy & security on desktop', async () => {
    const user = userEvent.setup();
    vi.stubGlobal('__TAURI_INTERNALS__', {});
    render(SettingsDialog, { props: { open: true, onClose: vi.fn(), client: mockClient() } });
    const privacy = within(screen.getByRole('region', { name: 'Privacy & security' }));
    expect(privacy.getAllByRole('button').map((button) => button.textContent?.trim())).toEqual([
      'Rules & sandbox',
      'Telemetry',
      'Lock',
    ]);
    await user.click(privacy.getByRole('button', { name: 'Rules & sandbox' }));
    expect(screen.getByText('Access keys', { selector: 'summary' })).toBeInTheDocument();
    await user.click(privacy.getByRole('button', { name: 'Lock' }));
    expect(screen.getByRole('heading', { name: 'App lock' })).toBeInTheDocument();
  });

  it('lists Usage next to Runs on desktop and opens it', async () => {
    const user = userEvent.setup();
    vi.stubGlobal('__TAURI_INTERNALS__', {});
    const usage = vi.spyOn(persistence, 'usage').mockReturnValue(new Promise(() => {}));
    Object.defineProperty(HTMLDialogElement.prototype, 'showModal', {
      configurable: true,
      value: function (this: HTMLDialogElement) {
        this.open = true;
      },
    });
    render(SettingsDialog, { props: { open: true, onClose: vi.fn(), client: mockClient() } });
    const workflows = within(screen.getByRole('region', { name: 'Workflows' }));
    const names = workflows
      .getAllByRole('button')
      .map((button) => button.querySelector('strong')?.textContent);
    expect(names.slice(names.indexOf('Runs'))).toEqual(['Runs', 'Usage']);
    await user.click(workflows.getByRole('button', { name: /Usage/ }));
    expect(screen.getByRole('heading', { name: 'Usage' })).toBeInTheDocument();
    expect(usage).toHaveBeenCalledWith(7);
    usage.mockRestore();
  });

  it('keeps every embedded panel in the content pane flow', async () => {
    vi.stubGlobal('__TAURI_INTERNALS__', {});
    vi.spyOn(persistence, 'preferences').mockResolvedValue({});
    vi.spyOn(persistence, 'memories').mockResolvedValue([]);
    vi.spyOn(persistence, 'memoryProposals').mockResolvedValue([]);
    vi.spyOn(persistence, 'usage').mockReturnValue(new Promise(() => {}));
    render(SettingsDialog, { props: { open: true, onClose: vi.fn(), client: mockClient() } });
    for (const name of ['Limits', 'Subagents', 'Instructions', 'Saved memories', 'MCP servers', 'Skills', 'Runs', 'Usage']) {
      await fireEvent.click(screen.getByRole('button', { name: new RegExp(name) }));
      const embedded = document.querySelector('dialog.embedded');
      expect(embedded).not.toBeNull();
      expect(getComputedStyle(embedded!).position).toBe('static');
    }
  });

  it('filters by related terms and category, and explains an empty result', async () => {
    const user = userEvent.setup();
    render(SettingsDialog, { props: { open: true, onClose: vi.fn(), client: mockClient() } });
    const search = screen.getByRole('searchbox', { name: 'Search settings' });
    const menu = within(screen.getByRole('navigation', { name: 'Settings menu' }));
    await user.type(search, '  TOOLS  ');
    expect(menu.getAllByRole('button')).toHaveLength(1);
    expect(menu.getByRole('button', { name: /MCP servers/ })).toBeInTheDocument();
    await user.clear(search);
    await user.type(search, 'workflows');
    expect(menu.getAllByRole('button')).toHaveLength(3);
    await user.clear(search);
    await user.type(search, 'nothing matches');
    expect(menu.queryAllByRole('button')).toHaveLength(0);
    expect(menu.getByRole('status')).toHaveTextContent('No settings match');
    await user.clear(search);
    expect(menu.getByRole('button', { name: /Runs/ })).toBeInTheDocument();
  });

  it('polls guest access only while the sharing page is open', async () => {
    vi.useFakeTimers();
    try {
      const client = mockClient();
      const invites = { list: vi.fn().mockResolvedValue([]), revoke: vi.fn() };
      const view = render(SettingsDialog, {
        props: { open: true, onClose: vi.fn(), client, invites },
      });
      await vi.advanceTimersByTimeAsync(10_000);
      expect(client.shareStatus).not.toHaveBeenCalled();
      await fireEvent.click(screen.getByRole('button', { name: 'Sharing & pairing' }));
      await vi.advanceTimersByTimeAsync(10_000);
      expect(client.shareStatus).toHaveBeenCalledTimes(2);
      await fireEvent.click(screen.getByRole('button', { name: 'Model connection' }));
      await vi.advanceTimersByTimeAsync(30_000);
      expect(client.shareStatus).toHaveBeenCalledTimes(2);
      await fireEvent.click(screen.getByRole('button', { name: 'Sharing & pairing' }));
      await vi.advanceTimersByTimeAsync(0);
      expect(client.shareStatus).toHaveBeenCalledTimes(3);
      await view.rerender({ open: false });
      await vi.advanceTimersByTimeAsync(30_000);
      expect(client.shareStatus).toHaveBeenCalledTimes(3);
    } finally {
      vi.useRealTimers();
    }
  });

  it('keeps the navigation and stable content pane while changing sections', async () => {
    const user = userEvent.setup();
    const onClose = vi.fn();
    const view = render(SettingsDialog, { props: { open: true, onClose, client: mockClient() } });
    await user.type(screen.getByRole('searchbox'), 'beta');
    await user.click(screen.getByRole('button', { name: /App updates/ }));
    const pane = screen.getByTestId('settings-content-pane');
    expect(screen.getByRole('navigation', { name: 'Settings menu' })).toBeInTheDocument();
    expect(screen.getByLabelText('Update channel')).toBeInTheDocument();
    await user.clear(screen.getByRole('searchbox'));
    await user.click(screen.getByRole('button', { name: /Model connection/ }));
    expect(screen.getByTestId('settings-content-pane')).toBe(pane);
    expect(screen.getByRole('searchbox')).toHaveValue('');
    await user.keyboard('{Escape}');
    expect(onClose).toHaveBeenCalledOnce();
    await view.rerender({ open: false });
    await view.rerender({ open: true });
    expect(screen.getByRole('searchbox')).toHaveValue('');
    expect(screen.getByRole('button', { name: /Model connection/ })).toBeInTheDocument();
  });

  it('offers an available signed update and installs it on request', async () => {
    const user = userEvent.setup();
    const onInstallUpdate = vi.fn().mockResolvedValue(undefined);

    render(SettingsDialog, {
      props: {
        open: true,
        model: MODEL,
        currentVersion: '0.1.0',
        updateState: 'available',
        availableUpdate: {
          currentVersion: '0.1.0',
          version: '0.1.9',
          body: 'A signed build from main.',
        },
        onInstallUpdate,
        onClose: vi.fn(),
        client: mockClient(),
      },
    });

    await user.click(screen.getByRole('button', { name: /App updates/ }));
    expect(await screen.findByText('Update available')).toBeInTheDocument();
    expect(screen.getByText('v0.1.9')).toBeInTheDocument();
    expect(screen.getByText('A signed build from main.')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Install update and restart' }));
    expect(onInstallUpdate).toHaveBeenCalledOnce();
  });

  it('offers beta and prevents switching channels during installation', async () => {
    const user = userEvent.setup();
    const onUpdateChannelChange = vi.fn().mockResolvedValue(undefined);
    const view = render(SettingsDialog, {
      props: {
        open: true,
        model: MODEL,
        onClose: vi.fn(),
        client: mockClient(),
        updateChannel: 'stable',
        onUpdateChannelChange,
      },
    });
    await user.click(screen.getByRole('button', { name: /App updates/ }));
    await user.selectOptions(screen.getByLabelText('Update channel'), 'beta');
    expect(onUpdateChannelChange).toHaveBeenCalledExactlyOnceWith('beta');
    await view.rerender({ updateChannel: 'beta', updateState: 'downloading' });
    expect(screen.getByLabelText('Update channel')).toBeDisabled();
    expect(screen.getByText(/Beta builds may have bugs/)).toBeInTheDocument();
  });

  it('saves and tests a model endpoint from settings', async () => {
    const user = userEvent.setup();
    const onConfigureEndpoint = vi.fn().mockResolvedValue(true);

    render(SettingsDialog, {
      props: {
        open: true,
        model: MODEL,
        endpoint: 'http://localhost:11434/v1',
        connectionState: 'offline',
        connectionError: 'Connection refused.',
        onConfigureEndpoint,
        onClose: vi.fn(),
        client: mockClient(),
      },
    });

    await user.click(screen.getByRole('button', { name: /Model connection/ }));
    // The address field is in plain view, not behind an Advanced toggle.
    const input = screen.getByLabelText('Server address');
    expect(input).toBeVisible();
    await user.clear(input);
    await user.type(input, 'http://192.0.2.10:11434');
    await user.click(screen.getByRole('button', { name: 'Connect' }));

    expect(onConfigureEndpoint).toHaveBeenCalledWith('http://192.0.2.10:11434');
    expect(
      await screen.findByText('Connected and loaded the available models.'),
    ).toBeInTheDocument();
  });

  it('switches and forgets saved endpoints in Settings and keeps failed switches visible', async () => {
    const user = userEvent.setup();
    const connections = [
      { endpoint: 'https://first.example/v1', name: 'First service' },
      { endpoint: 'https://second.example/v1', name: 'Second service' },
    ];
    let complete!: (value: boolean) => void;
    const onConfigureEndpoint = vi.fn(
      () =>
        new Promise<boolean>((resolve) => {
          complete = resolve;
        }),
    );
    const onPreferences = vi.fn();
    const view = render(SettingsDialog, {
      props: {
        open: true,
        onClose: vi.fn(),
        client: mockClient(),
        endpoint: 'https://first.example/v1',
        connectionState: 'ready',
        contextPreferences: { connections },
        onConfigureEndpoint,
        onPreferences,
      },
    });
    await user.click(screen.getByRole('button', { name: /Model connection/ }));
    await user.click(screen.getByRole('button', { name: 'Connect to Second service' }));
    expect(onConfigureEndpoint).toHaveBeenCalledWith('https://second.example/v1');
    expect(screen.getByRole('button', { name: 'Forget First service' })).toBeDisabled();
    await view.rerender({ connectionError: 'This service needs an access key.' });
    complete(false);
    expect(await screen.findByRole('alert')).toHaveTextContent('access key');
    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Connect to First service' })).toBeEnabled(),
    );
    await user.click(screen.getByRole('button', { name: 'Forget Second service' }));
    expect(onPreferences).toHaveBeenCalledWith({ connections: [connections[0]] });
  });

  it('leads Model connection with setup and found servers, then the address, then saved ones', async () => {
    const user = userEvent.setup();
    const onConfigureEndpoint = vi.fn().mockResolvedValue(true);
    const discover = vi.fn().mockResolvedValue([
      {
        name: 'llama.cpp',
        endpoint: 'http://127.0.0.1:18040/v1',
        available: true,
        models: ['qwen3.8-flash-next-iq2_xs'],
        supportsDownload: false,
        kind: 'llama.cpp',
        loaded: ['qwen3.8-flash-next-iq2_xs'],
      },
    ]);
    render(SettingsDialog, {
      props: {
        open: true,
        onClose: vi.fn(),
        client: mockClient(),
        endpoint: 'https://first.example/v1',
        connectionState: 'ready',
        contextPreferences: {
          connections: [{ endpoint: 'https://first.example/v1', name: 'First service' }],
        },
        onConfigureEndpoint,
        discover,
        desktop: true,
      },
    });
    await user.click(screen.getByRole('button', { name: /Model connection/ }));
    const connect = await screen.findByRole('button', {
      name: 'Connect to llama.cpp at 127.0.0.1:18040',
    });
    const order = [
      screen.getByRole('button', { name: 'Set up a connection' }),
      screen.getByRole('region', { name: 'Found on this computer' }),
      screen.getByLabelText('Server address'),
      screen.getByRole('region', { name: 'Saved connections' }),
      screen.getByRole('checkbox', { name: /writes tool calls as text/ }),
    ];
    for (let index = 1; index < order.length; index += 1) {
      expect(
        order[index - 1].compareDocumentPosition(order[index]) & Node.DOCUMENT_POSITION_FOLLOWING,
      ).toBeTruthy();
    }
    expect(screen.getByLabelText('Server address')).toBeVisible();
    await user.click(connect);
    expect(onConfigureEndpoint).toHaveBeenCalledWith(
      'http://127.0.0.1:18040/v1',
      'llama.cpp :18040',
    );
    expect(discover).toHaveBeenCalledOnce();
  });

  it('shows the detected server kind and its context window for the active connection', async () => {
    const user = userEvent.setup();
    const detectServer = vi.fn().mockResolvedValue({ kind: 'llama.cpp', contextWindow: 131072 });
    render(SettingsDialog, {
      props: {
        open: true,
        onClose: vi.fn(),
        client: mockClient(),
        endpoint: 'http://127.0.0.1:18040/v1',
        model: 'qwen',
        connectionState: 'ready',
        detectServer,
      },
    });
    await user.click(screen.getByRole('button', { name: /Model connection/ }));
    expect(await screen.findByText(/Detected llama\.cpp/)).toHaveTextContent(
      'Detected llama.cpp · 131,072-token context',
    );
    expect(detectServer).toHaveBeenCalledWith('http://127.0.0.1:18040/v1', 'qwen');
  });

  it('says when the server kind is unknown', async () => {
    const user = userEvent.setup();
    render(SettingsDialog, {
      props: {
        open: true,
        onClose: vi.fn(),
        client: mockClient(),
        endpoint: 'https://api.example/v1',
        model: 'm',
        connectionState: 'ready',
        detectServer: vi.fn().mockResolvedValue({ kind: 'unknown' }),
      },
    });
    await user.click(screen.getByRole('button', { name: /Model connection/ }));
    expect(
      await screen.findByText('Server type not detected · using your context setting'),
    ).toBeInTheDocument();
  });

  it('turns on text tool calls for the active connection only, off by default', async () => {
    const user = userEvent.setup();
    const connections = [
      { endpoint: 'http://127.0.0.1:18040/v1', name: 'Strata' },
      { endpoint: 'https://other.example/v1', name: 'Other', textToolCalls: true },
    ];
    const onPreferences = vi.fn();
    render(SettingsDialog, {
      props: {
        open: true,
        onClose: vi.fn(),
        client: mockClient(),
        endpoint: 'http://127.0.0.1:18040/v1',
        connectionState: 'ready',
        contextPreferences: { connections },
        onPreferences,
        detectServer: vi.fn().mockResolvedValue(null),
      },
    });
    await user.click(screen.getByRole('button', { name: /Model connection/ }));
    const toggle = screen.getByRole('checkbox', { name: /This model writes tool calls as text/ });
    expect(toggle).not.toBeChecked();
    expect(screen.getByText(/always asks before running one/)).toBeInTheDocument();
    await user.click(toggle);
    expect(onPreferences).toHaveBeenCalledWith({
      connections: [{ ...connections[0], textToolCalls: true }, connections[1]],
    });
  });

  it('manages a saved service key without changing the active connection', async () => {
    const user = userEvent.setup();
    vi.stubGlobal('__TAURI_INTERNALS__', {});
    const status = vi.spyOn(credentialClient, 'status').mockResolvedValue(true);
    const save = vi.spyOn(credentialClient, 'saveModel').mockResolvedValue({ saved: true });
    const onConfigureEndpoint = vi.fn();
    render(SettingsDialog, {
      props: {
        open: true,
        onClose: vi.fn(),
        client: mockClient(),
        onConfigureEndpoint,
        endpoint: 'https://first.example',
        contextPreferences: {
          connections: [
            { endpoint: 'https://first.example/v1', name: 'First service' },
            { endpoint: 'https://second.example/v1', name: 'Second service' },
          ],
        },
      },
    });
    await user.click(screen.getByRole('button', { name: 'Rules & sandbox' }));
    await user.click(screen.getByText('Access keys', { selector: 'summary' }));
    expect(screen.getByLabelText('Model service').querySelectorAll('option')).toHaveLength(2);
    await user.selectOptions(screen.getByLabelText('Model service'), 'https://second.example/v1');
    await waitFor(() =>
      expect(status).toHaveBeenLastCalledWith('model', 'https://second.example/v1'),
    );
    await user.type(screen.getByLabelText('New access key'), 'second-service-key');
    await user.click(screen.getByRole('button', { name: 'Verify and save key' }));
    expect(save).toHaveBeenCalledExactlyOnceWith('https://second.example/v1', 'second-service-key');
    expect(onConfigureEndpoint).not.toHaveBeenCalled();
  });

  it('creates a temporary share and exposes both its QR code and copyable link', async () => {
    const user = userEvent.setup();
    const client = mockClient();
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: { writeText },
    });

    render(SettingsDialog, {
      props: {
        open: true,
        model: MODEL,
        endpoint: 'http://model-host:11434/v1',
        onClose: vi.fn(),
        client,
      },
    });

    await user.click(screen.getByRole('button', { name: /Sharing & pairing/ }));
    await screen.findByText('Link expires after');
    await user.type(screen.getByLabelText('Hosted relay URL'), 'https://relay.example.com');
    await user.type(screen.getByLabelText(/Relay token/), 'relay-secret');
    await user.click(screen.getByLabelText('8 hours'));
    await user.click(screen.getByRole('button', { name: 'Create guest link' }));

    expect(client.startShare).toHaveBeenCalledWith({
      model: MODEL,
      endpoint: 'http://model-host:11434/v1',
      relayUrl: 'https://relay.example.com',
      relayToken: 'relay-secret',
      expiresInMinutes: 480,
    });
    expect(window.localStorage.getItem('blackwall.relay-url.v1')).toBe('https://relay.example.com');
    expect(await screen.findByRole('img', { name: 'QR code for guest chat link' })).toHaveAttribute(
      'src',
      QR_DATA_URL,
    );
    expect(screen.getByLabelText('Guest link')).toHaveValue(SHARE_URL);
    await waitFor(() => expect(screen.getByLabelText('Guest link')).toHaveFocus());

    await user.click(screen.getByRole('button', { name: 'Copy guest link' }));
    expect(writeText).toHaveBeenCalledWith(SHARE_URL);
    expect(screen.getByText('Copied')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Stop sharing' }));
    await waitFor(() => expect(client.stopShare).toHaveBeenCalledOnce());
    expect(await screen.findByRole('button', { name: 'Create guest link' })).toBeInTheDocument();
    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Create guest link' })).toHaveFocus(),
    );
  });

  it('explains when an active share secret is unavailable after reopening the app', async () => {
    const client = mockClient();
    client.shareStatus = vi.fn().mockResolvedValue({
      active: true,
      model: MODEL,
      expiresAt: Date.now() + 15 * 60 * 1_000,
      networkLabel: 'Local network',
      requestCount: 3,
    });

    render(SettingsDialog, {
      props: { open: true, model: MODEL, onClose: vi.fn(), client },
    });

    await userEvent.click(screen.getByRole('button', { name: /Sharing & pairing/ }));
    expect(await screen.findByText(/access code is no longer available/i)).toBeInTheDocument();
    expect(screen.getByText('3 requests')).toBeInTheDocument();
  });

  it('gracefully reports that browser development cannot start guest sharing', async () => {
    render(SettingsDialog, {
      props: { open: true, model: MODEL, onClose: vi.fn() },
    });

    await userEvent.click(screen.getByRole('button', { name: /Sharing & pairing/ }));
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'Guest sharing requires the Blackwall desktop app.',
    );
  });
  it('revokes one named link while keeping the other active', async () => {
    const user = userEvent.setup();
    const client = mockClient();
    const first = { id: 'first', name: 'Alex’s phone', active: true, model: MODEL };
    const second = { id: 'second', name: 'Taylor’s laptop', active: true, model: MODEL };
    client.shareStatus = vi.fn().mockResolvedValue(second);
    const invites = {
      list: vi.fn().mockResolvedValue([first, second]),
      revoke: vi.fn().mockResolvedValue(undefined),
    };
    render(SettingsDialog, {
      props: { open: true, model: MODEL, onClose: vi.fn(), client, invites },
    });
    await user.click(screen.getByRole('button', { name: /Sharing & pairing/ }));
    const revoke = await screen.findByRole('button', { name: 'Revoke Alex’s phone' });
    invites.list.mockResolvedValue([second]);
    await user.click(revoke);
    expect(invites.revoke).toHaveBeenCalledExactlyOnceWith('first');
    expect(client.stopShare).not.toHaveBeenCalled();
    expect(
      await screen.findByRole('button', { name: 'Revoke Taylor’s laptop' }),
    ).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Revoke Alex’s phone' })).not.toBeInTheDocument();
  });
});
