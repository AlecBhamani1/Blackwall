import { render, screen, waitFor } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import type { GuestShareClient } from '../ipc';
import SettingsDialog from './SettingsDialog.svelte';

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
      networkLabel: 'Tailscale network',
      requestCount: 0,
    }),
    stopShare: vi.fn().mockResolvedValue({ active: false }),
  };
}

describe('SettingsDialog', () => {
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

    expect(await screen.findByText('Update available')).toBeInTheDocument();
    expect(screen.getByText('v0.1.9')).toBeInTheDocument();
    expect(screen.getByText('A signed build from main.')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Install update and restart' }));
    expect(onInstallUpdate).toHaveBeenCalledOnce();
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

    const input = screen.getByLabelText('Endpoint URL');
    await user.clear(input);
    await user.type(input, 'http://100.76.24.116:11434');
    await user.click(screen.getByRole('button', { name: 'Save and reconnect' }));

    expect(onConfigureEndpoint).toHaveBeenCalledWith('http://100.76.24.116:11434');
    expect(await screen.findByText('Connected and loaded the available models.')).toBeInTheDocument();
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

    await screen.findByText('Link expires after');
    await user.click(screen.getByLabelText('8 hours'));
    await user.click(screen.getByRole('button', { name: 'Create guest link' }));

    expect(client.startShare).toHaveBeenCalledWith({
      model: MODEL,
      endpoint: 'http://model-host:11434/v1',
      expiresInMinutes: 480,
    });
    expect(await screen.findByRole('img', { name: 'QR code for guest chat link' })).toHaveAttribute(
      'src',
      QR_DATA_URL,
    );
    expect(screen.getByLabelText('Guest link')).toHaveValue(SHARE_URL);

    await user.click(screen.getByRole('button', { name: 'Copy guest link' }));
    expect(writeText).toHaveBeenCalledWith(SHARE_URL);
    expect(screen.getByText('Copied')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Stop sharing' }));
    await waitFor(() => expect(client.stopShare).toHaveBeenCalledOnce());
    expect(await screen.findByRole('button', { name: 'Create guest link' })).toBeInTheDocument();
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

    expect(await screen.findByText(/link and QR code are no longer available/i)).toBeInTheDocument();
    expect(screen.getByText('3 requests')).toBeInTheDocument();
  });

  it('gracefully reports that browser development cannot start guest sharing', async () => {
    render(SettingsDialog, {
      props: { open: true, model: MODEL, onClose: vi.fn() },
    });

    expect(await screen.findByRole('alert')).toHaveTextContent(
      'Guest sharing requires the Blackwall desktop app.',
    );
  });
});
