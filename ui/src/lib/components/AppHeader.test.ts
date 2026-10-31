import { render, screen, within } from '@testing-library/svelte';
import { describe, expect, it, vi } from 'vitest';
import AppHeader from './AppHeader.svelte';

function header(sidebarVisible: boolean) {
  return render(AppHeader, {
    props: {
      sidebarVisible,
      onToggleSidebar: vi.fn(),
      onChooseWorkspace: vi.fn(),
      onFolderPulseEnd: vi.fn(),
      onToggleFiles: vi.fn(),
      onExport: vi.fn(),
      onSearch: vi.fn(),
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

  it('keeps the Chat header to one row without Agent-only or model controls', () => {
    const { container } = header(true);
    const appHeader = screen.getByRole('banner', { name: 'Conversation header' });
    expect(container.querySelectorAll('header')).toHaveLength(1);
    expect(within(appHeader).getByText('New chat')).toBeInTheDocument();
    expect(within(appHeader).getByRole('button', { name: 'Search history' })).toBeInTheDocument();
    expect(within(appHeader).queryByRole('combobox', { name: 'Model' })).not.toBeInTheDocument();
    expect(
      within(appHeader).queryByRole('button', { name: 'Choose project folder' }),
    ).not.toBeInTheDocument();
    expect(within(appHeader).queryByRole('button', { name: 'Hide files' })).not.toBeInTheDocument();
  });

  it('puts the project and files controls in the Agent header', () => {
    render(AppHeader, {
      props: {
        sidebarVisible: true,
        agentMode: true,
        canExport: true,
        onToggleSidebar: vi.fn(),
        onChooseWorkspace: vi.fn(),
        onFolderPulseEnd: vi.fn(),
        onToggleFiles: vi.fn(),
        onExport: vi.fn(),
        onSearch: vi.fn(),
      },
    });
    const appHeader = screen.getByRole('banner', { name: 'Conversation header' });
    expect(
      within(appHeader).getByRole('button', { name: 'Choose project folder' }),
    ).toBeInTheDocument();
    expect(within(appHeader).getByRole('button', { name: 'Export chat' })).toBeInTheDocument();
    expect(within(appHeader).getByRole('button', { name: 'Hide files' })).toBeInTheDocument();
  });
});
