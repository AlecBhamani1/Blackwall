import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';
import ToolActivity from './ToolActivity.svelte';

describe('agent actions', () => {
  it('shows startup warnings without expanding and keeps rows as the owner left them', async () => {
    const user = userEvent.setup();
    const instructions = {
      id: 'instructions',
      name: 'project_instructions',
      arguments: '{"path":"AGENTS.md"}',
      output:
        'Loaded instructions: AGENTS.md\nWarning: MCP server context7 did not start: npx was not found.',
      status: 'error' as const,
    };
    const read = {
      id: 'read',
      name: 'read_file',
      arguments: '{"path":"README.md"}',
      status: 'running' as const,
    };
    const { container, rerender } = render(ToolActivity, {
      props: { tools: [instructions, read] },
    });
    expect(screen.getByText('MCP server context7 did not start: npx was not found.')).toBeVisible();
    await user.click(screen.getByText('Read file'));
    const rows = container.querySelectorAll('details');
    expect(rows[1].open).toBe(true);
    await rerender({ tools: [instructions, { ...read, status: 'complete', output: 'text' }] });
    expect(container.querySelectorAll('details')[1].open).toBe(true);
    expect(container.querySelectorAll('details')[0].open).toBe(false);
  });
});
