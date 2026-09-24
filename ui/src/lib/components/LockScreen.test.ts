import { render, screen, waitFor } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
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
});
