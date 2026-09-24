import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { afterEach, expect, it, vi } from 'vitest';
import { authStatus } from '../auth';
import LockSettings from './LockSettings.svelte';
vi.mock('../setup', () => ({ isDesktop: () => true }));
afterEach(() => authStatus.set({ locked: true, enabled: false }));
it('shows and focuses a lock failure in settings and permits an explicit retry', async () => {
  authStatus.set({ locked: false, enabled: true });
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
