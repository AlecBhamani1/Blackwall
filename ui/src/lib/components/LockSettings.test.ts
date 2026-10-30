import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { afterEach, expect, it, vi } from 'vitest';
import { authStatus, NO_KEYRING_NOTE } from '../auth';
import LockSettings from './LockSettings.svelte';
vi.mock('../setup', () => ({ isDesktop: () => true }));
const status = { locked: false, enabled: false, keyring: true, keyringNeeded: false };
afterEach(() => authStatus.set({ ...status, locked: true }));
it('shows and focuses a lock failure in settings and permits an explicit retry', async () => {
  authStatus.set({ ...status, enabled: true });
  const onLock = vi
    .fn()
    .mockRejectedValueOnce(
      new Error('Retry saving or export your conversation before locking Blackwall.'),
    )
    .mockResolvedValue(undefined);
  render(LockSettings, { onLock });
  await fireEvent.click(screen.getByRole('button', { name: 'Lock now' }));
  const alert = await screen.findByRole('alert');
  expect(alert).toHaveTextContent('Retry saving or export');
  await waitFor(() => expect(alert).toHaveFocus());
  await fireEvent.click(screen.getByRole('button', { name: 'Lock now' }));
  await waitFor(() => expect(onLock).toHaveBeenCalledTimes(2));
  expect(screen.queryByRole('alert')).not.toBeInTheDocument();
});
it('disables lock setup without a keyring', () => {
  authStatus.set({ ...status, keyring: false });
  render(LockSettings);
  expect(screen.getByText(NO_KEYRING_NOTE)).toBeVisible();
  expect(screen.getByLabelText('Passphrase')).toBeDisabled();
  expect(screen.getByLabelText('Confirm passphrase')).toBeDisabled();
  expect(screen.getByRole('button', { name: 'Enable app lock' })).toBeDisabled();
});
