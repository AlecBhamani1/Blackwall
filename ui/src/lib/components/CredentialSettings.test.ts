import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, expect, it, vi } from 'vitest';
import CredentialSettings from './CredentialSettings.svelte';

function client() {
  return {
    available: () => true,
    status: vi.fn().mockResolvedValue(true),
    remove: vi.fn().mockResolvedValue(undefined),
    saveModel: vi.fn().mockResolvedValue(undefined),
  };
}

describe('saved access keys', () => {
  it('keeps a pending removal unconfirmed, then preserves the key after denial and permits retry', async () => {
    const api = client();
    let reject!: (reason: unknown) => void;
    api.remove.mockImplementationOnce(
      () =>
        new Promise((_, fail) => {
          reject = fail;
        }),
    );
    render(CredentialSettings, { endpoint: 'https://model.example/v1', client: api });
    await screen.findByText('Saved in Keychain');
    await fireEvent.click(screen.getByRole('button', { name: 'Remove key…' }));
    expect(screen.getByRole('button', { name: 'Keep key' })).toHaveFocus();
    await fireEvent.click(screen.getByRole('button', { name: 'Remove saved key' }));
    expect(screen.getByRole('status')).toHaveTextContent('wait for confirmation');
    expect(screen.getByText('Saved in Keychain')).toBeVisible();
    expect(screen.getByRole('button', { name: 'Remove saved key' })).toBeDisabled();
    expect(screen.queryByText('Saved key removed from Keychain.')).not.toBeInTheDocument();
    reject('Keychain permission was denied.');
    expect(await screen.findByRole('alert')).toHaveTextContent('permission was denied');
    await waitFor(() => expect(screen.getByRole('alert')).toHaveFocus());
    expect(screen.getByText('Saved in Keychain')).toBeVisible();
    expect(api.remove).toHaveBeenCalledTimes(1);
    await fireEvent.click(screen.getByRole('button', { name: 'Remove saved key' }));
    expect(await screen.findByText('No saved key')).toBeVisible();
    await waitFor(() => expect(screen.getByRole('status')).toHaveFocus());
    expect(api.remove).toHaveBeenCalledTimes(2);
  });

  it('keeps a pending replacement draft and only clears it after confirmed completion', async () => {
    const api = client();
    let resolve!: () => void;
    api.saveModel.mockImplementationOnce(
      () =>
        new Promise<void>((done) => {
          resolve = done;
        }),
    );
    render(CredentialSettings, { endpoint: 'https://model.example/v1', client: api });
    await screen.findByText('Saved in Keychain');
    const input = screen.getByLabelText('New access key');
    await fireEvent.input(input, { target: { value: 'replacement' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Verify and save key' }));
    expect(input).toHaveValue('replacement');
    expect(input).toBeDisabled();
    expect(screen.getByRole('status')).toHaveTextContent('macOS Keychain prompt');
    expect(
      screen.queryByText('Access key verified and saved in Keychain.'),
    ).not.toBeInTheDocument();
    resolve();
    await screen.findByText('Access key verified and saved in Keychain.');
    expect(input).toHaveValue('');
    expect(input).toBeEnabled();
    await waitFor(() => expect(screen.getByRole('status')).toHaveFocus());
  });

  it('ignores a late mutation result after switching services', async () => {
    const api = client();
    let reject!: (reason: unknown) => void;
    api.remove.mockImplementationOnce(
      () =>
        new Promise((_, fail) => {
          reject = fail;
        }),
    );
    const view = render(CredentialSettings, { endpoint: 'https://old.example/v1', client: api });
    await screen.findByText('Saved in Keychain');
    await fireEvent.click(screen.getByRole('button', { name: 'Remove key…' }));
    await fireEvent.click(screen.getByRole('button', { name: 'Remove saved key' }));
    api.status.mockResolvedValue(false);
    await view.rerender({ endpoint: 'https://new.example/v1', client: api });
    await screen.findByText('No saved key');
    reject('Denied on old service.');
    await Promise.resolve();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(screen.queryByRole('status')).not.toBeInTheDocument();
    expect(screen.getByText('No saved key')).toBeVisible();
  });
  it('keeps a rejected replacement draft and never claims the existing key was removed', async () => {
    const api = client();
    api.saveModel.mockRejectedValue(new Error('Previous key kept.'));
    render(CredentialSettings, { endpoint: 'https://model.example/v1', client: api });
    await screen.findByText('Saved in Keychain');
    const input = screen.getByLabelText('New access key');
    await fireEvent.input(input, { target: { value: 'replacement' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Verify and save key' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Previous key kept.');
    expect(input).toHaveValue('replacement');
    expect(screen.getByText('Saved in Keychain')).toBeVisible();
    expect(api.remove).not.toHaveBeenCalled();
    api.saveModel.mockResolvedValue(undefined);
    await fireEvent.click(screen.getByRole('button', { name: 'Verify and save key' }));
    await screen.findByText('Access key verified and saved in Keychain.');
    expect(input).toHaveValue('');
  });

  it('requires the explicit removal action and keeps model and relay scopes separate', async () => {
    const api = client();
    render(CredentialSettings, { endpoint: 'https://relay.example', kind: 'relay', client: api });
    await screen.findByText('Saved in Keychain');
    await fireEvent.click(screen.getByRole('button', { name: 'Remove key…' }));
    expect(api.remove).not.toHaveBeenCalled();
    await fireEvent.click(screen.getByRole('button', { name: 'Keep key' }));
    expect(api.remove).not.toHaveBeenCalled();
    expect(screen.getByRole('button', { name: 'Remove key…' })).toHaveFocus();
    await fireEvent.click(screen.getByRole('button', { name: 'Remove key…' }));
    await fireEvent.click(screen.getByRole('button', { name: 'Remove saved key' }));
    await screen.findByText('No saved key');
    expect(api.remove).toHaveBeenCalledWith('relay', 'https://relay.example');
    expect(api.saveModel).not.toHaveBeenCalled();
  });

  it('ignores a late Keychain result from the previous service', async () => {
    const api = client();
    let resolve!: (value: boolean) => void;
    api.status
      .mockImplementationOnce(
        () =>
          new Promise<boolean>((done) => {
            resolve = done;
          }),
      )
      .mockResolvedValue(false);
    const view = render(CredentialSettings, { endpoint: 'https://old.example/v1', client: api });
    await waitFor(() => expect(api.status).toHaveBeenCalledTimes(1));
    await view.rerender({ endpoint: 'https://new.example/v1', client: api });
    await screen.findByText('No saved key');
    resolve(true);
    await Promise.resolve();
    expect(screen.queryByText('Saved in Keychain')).not.toBeInTheDocument();
    expect(screen.getByText('No saved key')).toBeVisible();
  });
});
