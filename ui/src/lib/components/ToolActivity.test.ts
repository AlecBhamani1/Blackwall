import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';
import ToolActivity from './ToolActivity.svelte';

describe('agent actions', () => {
  it("shows a failed server's output and unpassed variables in its details", async () => {
    const user = userEvent.setup();
    const instructions = {
      id: 'instructions',
      name: 'project_instructions',
      arguments: '{}',
      output: 'Loaded instructions: None\nWarning: MCP server docs did not start.',
      diagnostics: [{ server: 'docs', stderr: 'no module named x', notPassed: ['TOKEN'] }],
      status: 'error' as const,
    };
    render(ToolActivity, { props: { tools: [instructions] } });
    await user.click(screen.getByText('1 tool call'));
    await user.click(screen.getByText('Project instructions'));
    expect(screen.getByText('no module named x')).toBeVisible();
    expect(screen.getByText(/TOKEN/)).toBeVisible();
  });
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

  it('says when command output was trimmed, outside the output itself', async () => {
    const user = userEvent.setup();
    const { container } = render(ToolActivity, {
      props: {
        tools: [
          {
            id: 'big',
            name: 'shell',
            arguments: '{"command":"cat big.log"}',
            output: 'Exit: 0\nstart\n[… 900 bytes omitted …]\nend',
            truncated: true,
            status: 'complete' as const,
          },
          {
            id: 'small',
            name: 'shell',
            arguments: '{"command":"ls"}',
            output: 'Exit: 0\nfile',
            status: 'complete' as const,
          },
        ],
      },
    });
    await user.click(screen.getByText('2 tool calls'));
    for (const row of container.querySelectorAll<HTMLDetailsElement>('.tool-row')) row.open = true;
    const notes = screen.getAllByText(/Output trimmed to its start and end/);
    expect(notes).toHaveLength(1);
    expect(notes[0].closest('pre')).toBeNull();
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
