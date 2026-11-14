import { render, screen, waitFor } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { KEYRING_NEEDED, LOCK_MISSING } from '../auth';
import LockScreen from './LockScreen.svelte';
describe('app lock', () => {
  it('clears the passphrase immediately after submission', async () => {
    const user = userEvent.setup();
    const onUnlock = vi.fn().mockResolvedValue(undefined);
    render(LockScreen, { props: { initialized: true, onUnlock, onRetry: vi.fn() } });
    await waitFor(() => expect(screen.getByLabelText('Passphrase')).toHaveFocus());
    await user.type(screen.getByLabelText('Passphrase'), 'a private passphrase');
    await user.click(screen.getByRole('button', { name: 'Unlock Blackwall' }));
    expect(onUnlock).toHaveBeenCalledWith('a private passphrase');
    expect(screen.getByLabelText('Passphrase')).toHaveValue('');
    await waitFor(() => expect(screen.getByLabelText('Passphrase')).toHaveFocus());
  });
  it('restores keyboard focus after a failed native unlock completes', async () => {
    const props = { initialized: true, onUnlock: vi.fn(), onRetry: vi.fn() };
    const { rerender } = render(LockScreen, { props: { ...props, loading: true } });
    await rerender({
      ...props,
      loading: false,
      error: 'That passphrase did not match. Try again.',
    });
    await waitFor(() => expect(screen.getByLabelText('Passphrase')).toHaveFocus());
    expect(screen.getByRole('alert')).toHaveTextContent('did not match');
  });
  it('keeps initialization failures recoverable without exposing the app', async () => {
    const retry = vi.fn();
    const user = userEvent.setup();
    render(LockScreen, {
      props: { error: 'Unlock your login keychain.', onUnlock: vi.fn(), onRetry: retry },
    });
    expect(screen.queryByLabelText('Passphrase')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Try again' }));
    expect(retry).toHaveBeenCalledOnce();
  });
  it('never shows the checking text together with an error', () => {
    render(LockScreen, {
      props: { error: 'Unlock your login keychain.', onUnlock: vi.fn(), onRetry: vi.fn() },
    });
    expect(screen.queryByText('Checking your app lock…')).not.toBeInTheDocument();
    expect(screen.getByRole('alert')).toHaveTextContent('Unlock your login keychain.');
    expect(screen.getByRole('button', { name: 'Try again' })).toBeVisible();
  });
  it('shows the start-your-keyring message when keyringNeeded', async () => {
    const retry = vi.fn();
    const user = userEvent.setup();
    render(LockScreen, {
      props: { initialized: true, keyringNeeded: true, onUnlock: vi.fn(), onRetry: retry },
    });
    expect(screen.getByText(KEYRING_NEEDED)).toBeVisible();
    expect(KEYRING_NEEDED).toBe(
      'Blackwall needs the system keyring to open this data folder. Start your keyring (GNOME Keyring or KWallet), then try again.',
    );
    expect(screen.queryByText('Enter your passphrase to open your conversations.')).toBeNull();
    expect(screen.queryByText('Checking your app lock…')).not.toBeInTheDocument();
    expect(screen.queryByLabelText('Passphrase')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Try again' }));
    expect(retry).toHaveBeenCalledOnce();
  });
  it('offers try again or a confirmed reset when the lock passphrase is missing', async () => {
    const retry = vi.fn();
    const reset = vi.fn().mockResolvedValue(undefined);
    const user = userEvent.setup();
    render(LockScreen, {
      props: { lockMissing: true, onUnlock: vi.fn(), onRetry: retry, onReset: reset },
    });
    expect(screen.getByRole('alert')).toHaveTextContent(LOCK_MISSING);
    expect(screen.queryByLabelText('Passphrase')).not.toBeInTheDocument();
    expect(screen.queryByText('Checking your app lock…')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Try again' }));
    expect(retry).toHaveBeenCalledOnce();
    // Reset asks first, and cancelling erases nothing.
    await user.click(screen.getByRole('button', { name: 'Reset' }));
    expect(reset).not.toHaveBeenCalled();
    expect(screen.getByText(/erases this device's conversations, history and saved memories/)).toBeVisible();
    expect(screen.queryByText(/recovered/)).not.toBeInTheDocument();
    expect(screen.getByText(/Settings, connections and saved keys stay/)).toBeVisible();
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(screen.queryByRole('button', { name: 'Erase and reset' })).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Reset' }));
    await user.click(screen.getByRole('button', { name: 'Erase and reset' }));
    expect(reset).toHaveBeenCalledOnce();
  });
  it('shows a failed reset without opening the app', () => {
    render(LockScreen, {
      props: {
        lockMissing: true,
        error: 'The app lock marker could not be updated in your data folder.',
        onUnlock: vi.fn(),
        onRetry: vi.fn(),
      },
    });
    expect(screen.getByText(/could not be updated/)).toBeVisible();
    expect(screen.queryByLabelText('Passphrase')).not.toBeInTheDocument();
  });
});
