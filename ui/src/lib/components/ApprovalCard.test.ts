import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import ApprovalCard from './ApprovalCard.svelte';

const approval = {
  requestId: 'run',
  approvalId: 'approval',
  kind: 'file',
  detail: 'Update src/main.ts\n-old\n+new',
};
describe('action approval', () => {
  it('offers no run-wide allow for a call the model wrote as text', () => {
    render(ApprovalCard, {
      props: { approval: { ...approval, remember: false }, onResolve: vi.fn() },
    });
    expect(screen.getByText('Review this file change')).toBeInTheDocument();
    expect(
      screen.queryByRole('button', { name: 'Allow identical action this run' }),
    ).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Allow once' })).toBeInTheDocument();
  });
  it('marks a subagent delegation as sending data off this device and allows it only once', () => {
    render(ApprovalCard, {
      props: {
        approval: {
          ...approval,
          kind: 'exec',
          detail: 'Tool: review\nPrompt (40 bytes, on standard input):\nTask: Review the diff',
          remember: false,
          offDevice: true,
        },
        onResolve: vi.fn(),
      },
    });
    expect(screen.getByText(/Sends data off this device/)).toBeVisible();
    expect(screen.getByText(/Task: Review the diff/)).toBeVisible();
    expect(
      screen.queryByRole('button', { name: 'Allow identical action this run' }),
    ).not.toBeInTheDocument();
  });
  it('still offers a run-wide allow for ordinary prompts', () => {
    render(ApprovalCard, { props: { approval, onResolve: vi.fn() } });
    expect(
      screen.getByRole('button', { name: 'Allow identical action this run' }),
    ).toBeInTheDocument();
    expect(screen.queryByText(/Sends data off this device/)).not.toBeInTheDocument();
  });
  it('shows the exact proposed action and sends only one decision while pending', async () => {
    const user = userEvent.setup();
    const onResolve = vi.fn(() => new Promise<void>(() => {}));
    render(ApprovalCard, { props: { approval, onResolve } });
    expect(screen.getByRole('region', { name: 'Proposed action details' })).toHaveTextContent(
      'Update src/main.ts',
    );
    await user.click(screen.getByRole('button', { name: 'Allow once' }));
    await user.click(screen.getByRole('button', { name: 'Deny' }));
    expect(onResolve).toHaveBeenCalledExactlyOnceWith('allow');
    expect(screen.getByRole('button', { name: 'Deny' })).toBeDisabled();
  });
  it('offers to always allow an MCP tool or its whole server', async () => {
    const user = userEvent.setup();
    const onResolve = vi.fn().mockResolvedValue(undefined);
    const tool = {
      requestId: 'run',
      approvalId: 'tool',
      kind: 'exec',
      detail: 'MCP server: context7\nTool: query-docs',
      tool: { server: 'context7', tool: 'context7__query-docs' },
    };
    const { rerender } = render(ApprovalCard, { props: { approval: tool, onResolve } });
    expect(screen.getByText('Allow this tool?')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Always allow' }));
    expect(onResolve).toHaveBeenLastCalledWith('allow_tool_in_project');
    await user.selectOptions(screen.getByLabelText('What to allow'), 'every context7 tool');
    await user.selectOptions(screen.getByLabelText('Where to allow it'), 'in every project');
    await user.click(screen.getByRole('button', { name: 'Always allow' }));
    expect(onResolve).toHaveBeenLastCalledWith('allow_server');
    await rerender({ approval: { ...tool, approvalId: 'next' }, onResolve });
    await user.click(screen.getByRole('button', { name: 'Always allow' }));
    expect(onResolve).toHaveBeenLastCalledWith('allow_tool_in_project');
    await rerender({ approval, onResolve });
    expect(screen.queryByRole('button', { name: 'Always allow' })).not.toBeInTheDocument();
  });
  it('offers recovery after a decision cannot be delivered', async () => {
    const user = userEvent.setup();
    const onResolve = vi.fn().mockRejectedValue(new Error('expired'));
    render(ApprovalCard, { props: { approval, onResolve } });
    await user.click(screen.getByRole('button', { name: 'Deny' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('stop the task');
    expect(screen.getByRole('button', { name: 'Deny' })).toBeEnabled();
  });
});
