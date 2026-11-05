import { render, screen, waitFor } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import WebSettings from './WebSettings.svelte';

function client() {
  return {
    load: vi
      .fn()
      .mockResolvedValue({
        settings: { provider: 'duckduckgo', braveFallback: false, rendering: false },
        hasKey: true,
        browserSupported: true,
        browserInstalled: false,
      }),
    save: vi.fn().mockResolvedValue(undefined),
    saveKey: vi.fn().mockResolvedValue(undefined),
    check: vi.fn().mockResolvedValue(undefined),
    install: vi.fn().mockResolvedValue(undefined),
    open: vi.fn(),
  };
}
describe('Web setup and disclosure', () => {
  it('keeps paid fallback and rendering off until selected and saves no secret in preferences', async () => {
    const user = userEvent.setup();
    const api = client();
    render(WebSettings, { client: api });
    await waitFor(() => expect(screen.getByLabelText('Use for searches')).toBeEnabled());
    expect(screen.getByRole('checkbox', { name: /Allow Brave fallback/ })).not.toBeChecked();
    expect(screen.getByRole('checkbox', { name: /Render JavaScript/ })).not.toBeChecked();
    await user.click(screen.getByRole('checkbox', { name: /Allow Brave fallback/ }));
    await user.click(screen.getByRole('checkbox', { name: /Render JavaScript/ }));
    await user.click(screen.getByRole('button', { name: 'Save Web settings' }));
    expect(api.save).toHaveBeenCalledWith({
      provider: 'duckduckgo',
      braveFallback: true,
      rendering: true,
    });
    expect(api.install).not.toHaveBeenCalled();
    expect(api.check).not.toHaveBeenCalled();
    expect(screen.getByText(/requests may incur charges/)).toBeVisible();
  });
  it('writes keys separately and preserves provider choices while testing or installing', async () => {
    const user = userEvent.setup();
    const api = client();
    render(WebSettings, { client: api });
    await waitFor(() => expect(screen.getByLabelText('Use for searches')).toBeEnabled());
    await user.selectOptions(screen.getByLabelText('Use for searches'), 'brave');
    await user.type(screen.getByPlaceholderText('Enter a replacement key'), 'fixture-secret');
    await user.click(screen.getByRole('button', { name: 'Save key' }));
    await waitFor(() => expect(api.saveKey).toHaveBeenCalledWith('fixture-secret'));
    expect(screen.getByPlaceholderText('Enter a replacement key')).toHaveValue('');
    expect(screen.getByLabelText('Use for searches')).toHaveValue('brave');
    expect(api.save).not.toHaveBeenCalled();
    await user.click(screen.getByRole('button', { name: 'Check Brave connection' }));
    await waitFor(() => expect(api.check).toHaveBeenCalledOnce());
    expect(screen.getByText(/Blackwall connection check/)).toBeVisible();
  });
});
