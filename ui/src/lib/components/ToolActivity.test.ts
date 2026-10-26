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
    const group = container.querySelector<HTMLDetailsElement>('.tool-group')!;
    expect(group.open).toBe(false);
    expect(screen.getByText('2 tool calls')).toBeVisible();
    expect(screen.getByText('1 running · 1 failed')).toBeVisible();
    await user.click(screen.getByText('2 tool calls'));
    await user.click(screen.getByText('Read file'));
    const rows = container.querySelectorAll<HTMLDetailsElement>('.tool-row');
    expect(rows[1].open).toBe(true);
    await rerender({ tools: [instructions, { ...read, status: 'complete', output: 'text' }] });
    expect(container.querySelectorAll<HTMLDetailsElement>('.tool-row')[1].open).toBe(true);
    expect(container.querySelectorAll<HTMLDetailsElement>('.tool-row')[0].open).toBe(false);
    expect(group.open).toBe(true);
    expect(screen.getByText('text')).toBeVisible();
    expect(screen.getByText('{"path":"README.md"}')).toBeVisible();
    await user.click(screen.getByText('2 tool calls'));
    await rerender({ tools: [instructions, { ...read, status: 'stopped' }] });
    expect(group.open).toBe(false);
    expect(screen.getByText('1 failed · 1 stopped')).toBeVisible();
  });

  it('keeps the group keyboard focusable and labels historical activity', async () => {
    const user = userEvent.setup();
    const { container } = render(ToolActivity, {
      props: {
        historical: true,
        tools: [
          { id: 'search', name: 'web_search', arguments: '{"query":"Svelte"}', status: 'complete' },
        ],
      },
    });
    expect(screen.getByText('History · Complete')).toBeVisible();
    await user.tab();
    expect(container.querySelector('.group-summary')).toHaveFocus();
    await user.click(screen.getByText('1 tool call'));
    expect(container.querySelector<HTMLDetailsElement>('.tool-group')?.open).toBe(true);
    expect(screen.getByText('Svelte')).toBeVisible();
  });
});
