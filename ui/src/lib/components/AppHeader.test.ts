import { render, screen } from '@testing-library/svelte';
import { describe, expect, it, vi } from 'vitest';
import AppHeader from './AppHeader.svelte';

function header(sidebarVisible: boolean) {
  return render(AppHeader, {
    props: {
      sidebarVisible,
      onToggleSidebar: vi.fn(),
      onModelChange: vi.fn(),
      onReconnect: vi.fn(),
    },
  });
}

describe('AppHeader', () => {
  it('toggles the sidebar with a panel icon, filled while the sidebar is open', () => {
    const open = header(true);
    const hide = screen.getByRole('button', { name: 'Hide sidebar' });
    expect(hide.querySelector('rect')).not.toBeNull();
    expect(hide.querySelector('path[fill="currentColor"]')).not.toBeNull();
    open.unmount();

    header(false);
    const show = screen.getByRole('button', { name: 'Show sidebar' });
    expect(show.querySelector('rect')).not.toBeNull();
    expect(show.querySelector('path[fill="currentColor"]')).toBeNull();
  });
});
