import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { writable } from 'svelte/store';
import { describe, expect, it, vi } from 'vitest';
import PairingPanel from './PairingPanel.svelte';
import type { PairingSnapshot } from '../pairing';
function client(initial: Partial<PairingSnapshot> = {}) {
  return {
    snapshot: writable<PairingSnapshot>({
      devices: [],
      host: null,
      client: null,
      warnings: [],
      ...initial,
    }),
    refresh: vi.fn().mockResolvedValue(undefined),
    create: vi.fn().mockResolvedValue({
      invitation: 'https://relay.example/pair/id#key=secret',
      expiresAt: Date.now() + 300000,
    }),
    join: vi.fn().mockResolvedValue(undefined),
    approve: vi.fn().mockResolvedValue(undefined),
    cancel: vi.fn().mockResolvedValue(undefined),
    remove: vi.fn().mockResolvedValue(undefined),
    retry: vi.fn().mockResolvedValue(undefined),
  };
}
const candidate = { id: 'device-a', name: 'Travel laptop', salt: 'salt', hash: 'hash' };
const host = {
  state: 'review',
  hostName: 'Home Mac',
  model: 'model-a',
  expiresAt: Date.now() + 300000,
  candidate,
  confirmation: 'AABB CCDD EEFF',
  sessionId: null,
};
describe('persistent computer pairing', () => {
  it('shows a failed client receipt and offers a status retry without clearing its identity', async () => {
    const api = client({ client: host, warnings: ['Local database could not be saved.'] });
    render(PairingPanel, { desktop: true, client: api });
    expect(screen.getByText('Local database could not be saved.')).toBeVisible();
    expect(screen.getByText(/Pairing needs attention/)).toBeVisible();
    await fireEvent.click(screen.getByRole('button', { name: 'Check again' }));
    await waitFor(() => expect(api.refresh).toHaveBeenCalled());
    expect(api.cancel).not.toHaveBeenCalled();
    expect(screen.getByLabelText('Confirmation code')).toHaveTextContent('AABB CCDD EEFF');
  });
  it('refreshes a partial removal and keeps an explicit retry available without Connect', async () => {
    const device = {
      id: 'partial',
      role: 'client' as const,
      name: 'Partial removal',
      model: 'model',
      endpoint: 'https://relay.example/s/partial/v1',
      state: 'active',
      online: false,
    };
    const api = client({ devices: [device] });
    const onForget = vi.fn();
    api.remove.mockImplementationOnce(async () => {
      api.refresh.mockImplementation(async () => {
        api.snapshot.update((current) => ({
          ...current,
          devices: [{ ...device, state: 'revoking' }],
        }));
      });
      throw new Error('Database deletion failed.');
    });
    render(PairingPanel, { desktop: true, client: api, onForget });
    await fireEvent.click(screen.getByRole('button', { name: 'Remove Partial removal' }));
    await fireEvent.click(screen.getByRole('button', { name: 'Remove computer' }));
    await waitFor(() =>
      expect(screen.getByText('Removal incomplete · retry removal')).toBeVisible(),
    );
    expect(
      screen.queryByRole('button', { name: 'Connect to Partial removal' }),
    ).not.toBeInTheDocument();
    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Remove Partial removal' })).toBeEnabled(),
    );
    expect(onForget).not.toHaveBeenCalled();
    api.refresh.mockImplementation(async () => {
      api.snapshot.update((current) => ({ ...current, devices: [] }));
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Remove computer' }));
    await waitFor(() => expect(onForget).toHaveBeenCalledWith(device.endpoint));
  });
  it('moves from the submitted invitation to review, then to the newly saved computer', async () => {
    const api = client();
    const user = userEvent.setup();
    api.join.mockImplementation(async () => {
      api.snapshot.update((value) => ({ ...value, client: host }));
    });
    render(PairingPanel, { desktop: true, client: api });
    await user.type(screen.getByLabelText('Pairing invitation'), 'private-invitation');
    await user.click(screen.getByRole('button', { name: 'Request pairing' }));
    await waitFor(() =>
      expect(screen.getByRole('group', { name: 'Pairing confirmation' })).toHaveFocus(),
    );
    api.snapshot.update((value) => ({
      ...value,
      client: { ...host, state: 'saved', sessionId: 'a' },
      devices: [
        {
          id: 'older',
          role: 'client',
          name: 'Older Mac',
          model: 'model-a',
          endpoint: 'https://relay.example/s/older/v1',
          state: 'active',
          online: false,
        },
        {
          id: 'a',
          role: 'client',
          name: 'Home Mac',
          model: 'model-a',
          endpoint: 'https://relay.example/s/a/v1',
          state: 'active',
          online: false,
        },
      ],
    }));
    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Connect to Home Mac' })).toHaveFocus(),
    );
  });
  it('moves host approval focus to Done and shows its result once', async () => {
    const api = client({ host });
    const user = userEvent.setup();
    api.approve.mockImplementation(async () => {
      api.snapshot.update((value) => ({
        ...value,
        host: { ...host, state: 'approved', sessionId: 'a' },
        devices: [
          {
            id: 'a',
            role: 'host',
            name: 'Travel laptop',
            model: 'model-a',
            endpoint: 'https://relay.example/s/a/v1',
            state: 'active',
            online: true,
          },
        ],
      }));
    });
    render(PairingPanel, { mode: 'host', desktop: true, client: api });
    await user.click(screen.getByRole('checkbox'));
    await user.click(screen.getByRole('button', { name: 'Approve computer' }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Done' })).toHaveFocus());
    expect(
      screen.getAllByText('Computer approved. Finish connecting on the other screen.'),
    ).toHaveLength(1);
  });
  it('keeps relay settings open while typing and moves focus through invitation creation', async () => {
    const api = client();
    api.create.mockResolvedValue({
      invitation: 'https://relay.example/pair/id#key=secret',
      expiresAt: Date.now() + 300500,
    });
    const user = userEvent.setup();
    render(PairingPanel, {
      mode: 'host',
      desktop: true,
      client: api,
      model: 'model-a',
      endpoint: 'http://127.0.0.1:11434/v1',
    });
    await user.click(screen.getByRole('button', { name: 'Pair another computer' }));
    expect(screen.getByLabelText('Name this model computer')).toHaveFocus();
    const relay = screen.getByLabelText('Relay address');
    await user.type(relay, 'https://relay.example');
    expect(relay).toHaveFocus();
    expect(relay.closest('details')).toHaveAttribute('open');
    await user.click(screen.getByRole('button', { name: 'Create pairing invitation' }));
    expect(api.create).toHaveBeenCalledWith(
      expect.objectContaining({ relayUrl: 'https://relay.example' }),
    );
    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Copy invitation' })).toHaveFocus(),
    );
    expect(screen.getByText(/Expires in 5 minutes/)).toBeVisible();
  });
  it('requires matching-code confirmation and resets it when the candidate changes', async () => {
    const api = client({ host });
    render(PairingPanel, { mode: 'host', desktop: true, client: api });
    const approve = screen.getByRole('button', { name: 'Approve computer' });
    expect(approve).toBeDisabled();
    const keyboard = userEvent.setup();
    screen.getByRole('checkbox').focus();
    await keyboard.keyboard('[Space]');
    expect(approve).toBeEnabled();
    api.snapshot.set({
      devices: [],
      host: {
        ...host,
        candidate: { ...candidate, hash: 'changed-credential' },
        confirmation: '0011 2233 4455',
      },
      client: null,
      warnings: [],
    });
    await waitFor(() => expect(approve).toBeDisabled());
    await fireEvent.click(screen.getByRole('checkbox'));
    await fireEvent.click(approve);
    await waitFor(() =>
      expect(api.approve).toHaveBeenCalledWith({ ...candidate, hash: 'changed-credential' }),
    );
  });
  it('clears the invitation draft when sending it to native code and shows a rejected request', async () => {
    const api = client();
    api.join.mockRejectedValue(new Error('Invitation expired.'));
    render(PairingPanel, { desktop: true, client: api });
    const input = screen.getByLabelText('Pairing invitation');
    await fireEvent.input(input, { target: { value: 'private-invitation' } });
    await fireEvent.submit(input.closest('form')!);
    expect(input).toHaveValue('');
    expect(await screen.findByRole('alert')).toHaveTextContent('Invitation expired.');
    expect(api.join).toHaveBeenCalledWith('private-invitation', 'My laptop');
  });
  it('requires explicit removal and preserves another saved computer', async () => {
    const device = {
      id: 'a',
      name: 'Home Mac',
      model: 'model-a',
      endpoint: 'https://relay.example/s/a/v1',
      role: 'client' as const,
      state: 'active',
      online: false,
    };
    const api = client({ devices: [device, { ...device, id: 'b', name: 'Studio Mac' }] });
    const forget = vi.fn();
    render(PairingPanel, { desktop: true, client: api, onForget: forget });
    await fireEvent.click(screen.getByRole('button', { name: 'Remove Home Mac' }));
    expect(api.remove).not.toHaveBeenCalled();
    await fireEvent.click(screen.getByRole('button', { name: 'Keep computer' }));
    expect(api.remove).not.toHaveBeenCalled();
    await fireEvent.click(screen.getByRole('button', { name: 'Remove Home Mac' }));
    await fireEvent.click(screen.getByRole('button', { name: 'Remove computer' }));
    await waitFor(() => expect(api.remove).toHaveBeenCalledWith('a'));
    expect(forget).toHaveBeenCalledWith(device.endpoint);
    expect(screen.getByText('Studio Mac')).toBeVisible();
  });
  it('keeps a failed connection available to retry without claiming success', async () => {
    const api = client({
      devices: [
        {
          id: 'a',
          name: 'Home Mac',
          model: 'model',
          endpoint: 'https://relay.example/s/a/v1',
          role: 'client',
          state: 'active',
          online: false,
        },
      ],
    });
    const connect = vi.fn().mockResolvedValue(false);
    render(PairingPanel, { desktop: true, client: api, onConnect: connect });
    await fireEvent.click(screen.getByRole('button', { name: 'Connect to Home Mac' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('offline');
    expect(screen.getByRole('button', { name: 'Connect to Home Mac' })).toBeEnabled();
    expect(screen.getByText('Home Mac')).toBeVisible();
  });
});
