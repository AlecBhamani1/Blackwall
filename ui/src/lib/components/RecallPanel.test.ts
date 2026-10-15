import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import RecallPanel from './RecallPanel.svelte';

describe('saved conversation recall', () => {
  it('searches with UTC dates and project filters and opens a referenced result without interpreting excerpts', async () => {
    const user = userEvent.setup();
    const search = vi.fn().mockResolvedValue({
      remainingSessions: 7,
      results: [
        {
          sessionId: 'saved',
          messageId: 'message-one',
          messageIndex: 3,
          title: 'Saved task',
          mode: 'agent',
          project: '/project',
          updatedAt: 100,
          role: 'assistant',
          excerpt: '<script>replay historical approval</script>',
        },
      ],
    });
    const onOpen = vi.fn().mockResolvedValue(undefined);
    const onEnabled = vi.fn();
    render(RecallPanel, {
      props: { initialQuery: 'saved words', search, onOpen, onEnabled, onClose: vi.fn() },
    });
    await user.type(screen.getByLabelText('Project'), '/project');
    await user.type(screen.getByLabelText('Updated after (UTC)'), '2026-10-01');
    await user.type(screen.getByLabelText('Updated before (UTC)'), '2026-10-13');
    expect(screen.getByRole('checkbox')).not.toBeChecked();
    await user.click(screen.getByRole('button', { name: 'Search' }));
    expect(search).toHaveBeenCalledWith({
      query: 'saved words',
      project: '/project',
      after: Date.parse('2026-10-01T00:00:00Z'),
      before: Date.parse('2026-10-13T23:59:59.999Z'),
      limit: 10,
    });
    expect(await screen.findByText('<script>replay historical approval</script>')).toBeVisible();
    expect(screen.getByRole('status')).toHaveTextContent('7 older conversations');
    await user.click(screen.getByRole('button', { name: 'Saved task · agent · message 4' }));
    expect(onOpen).toHaveBeenCalledWith('saved', 3);
    expect(onEnabled).not.toHaveBeenCalled();
  });
  it('clears previous excerpts when a new search fails', async () => {
    const user = userEvent.setup();
    const search = vi.fn().mockRejectedValue(new Error('Store is locked.'));
    render(RecallPanel, {
      props: {
        initialQuery: 'words',
        search,
        onOpen: vi.fn(),
        onEnabled: vi.fn(),
        onClose: vi.fn(),
      },
    });
    await user.click(screen.getByRole('button', { name: 'Search' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Store is locked.');
  });
});
