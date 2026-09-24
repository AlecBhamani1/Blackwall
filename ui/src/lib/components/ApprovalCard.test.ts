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
  it('offers recovery after a decision cannot be delivered', async () => {
    const user = userEvent.setup();
    const onResolve = vi.fn().mockRejectedValue(new Error('expired'));
    render(ApprovalCard, { props: { approval, onResolve } });
    await user.click(screen.getByRole('button', { name: 'Deny' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('stop the task');
    expect(screen.getByRole('button', { name: 'Deny' })).toBeEnabled();
  });
});
