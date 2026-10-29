import { render, screen, waitFor } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import TrustedProjects from './TrustedProjects.svelte';

function client(paths: string[]) {
  return {
    status: vi.fn(),
    trust: vi.fn(),
    trusted: vi.fn(async () => paths.map((path) => ({ path, sha256: 'abcdef0123456789' }))),
    untrust: vi.fn(async () => undefined),
  };
}

describe('Trusted projects in Settings', () => {
  it('lists trusted projects and untrusts one', async () => {
    const user = userEvent.setup();
    const fake = client(['/work/app', '/work/site']);
    render(TrustedProjects, { client: fake });
    expect(await screen.findByText('/work/app')).toBeInTheDocument();
    expect(screen.getAllByText('abcdef012345')).toHaveLength(2);
    await user.click(screen.getByRole('button', { name: 'Untrust /work/app' }));
    expect(fake.untrust).toHaveBeenCalledWith('/work/app');
    await waitFor(() => expect(screen.queryByText('/work/app')).not.toBeInTheDocument());
    expect(screen.getByText('/work/site')).toBeInTheDocument();
  });

  it('says when nothing is trusted and shows a failure', async () => {
    const user = userEvent.setup();
    const fake = client(['/work/app']);
    fake.untrust.mockRejectedValueOnce('The trusted projects list could not be read or saved.');
    render(TrustedProjects, { client: fake });
    await user.click(await screen.findByRole('button', { name: 'Untrust /work/app' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('could not be read or saved');
    expect(screen.getByText('/work/app')).toBeInTheDocument();
    const empty = client([]);
    render(TrustedProjects, { client: empty });
    expect(await screen.findByText('No trusted projects.')).toBeInTheDocument();
  });
});
