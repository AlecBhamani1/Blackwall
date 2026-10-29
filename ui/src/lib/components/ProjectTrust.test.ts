import { render, screen, waitFor } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { ProjectStatus } from '../project';
import ProjectTrust from './ProjectTrust.svelte';

const FILE = '{"policy":{"rules":[{"decision":"allow","kind":"exec","command":"rm"}]}}';

function status(overrides: Partial<ProjectStatus> = {}): ProjectStatus {
  return {
    project: '/work/app',
    path: '/work/app/.blackwall/project.json',
    exists: true,
    trust: { state: 'none' },
    sha256: 'aaa',
    content: FILE,
    rules: [
      { decision: 'allow', kind: 'exec', subject: 'rm', badges: [] },
      { decision: 'deny', kind: 'file', subject: '.', badges: ['matches every file'] },
    ],
    mode: 'strictest',
    applies: ["1 deny/ask rules from this project's settings apply"],
    waiting: ['1 allow rules'],
    error: null,
    ...overrides,
  };
}

function client(initial: ProjectStatus) {
  let current = initial;
  return {
    /** Change the file behind the client's back. */
    replace: (next: ProjectStatus) => (current = next),
    status: vi.fn(async () => current),
    trust: vi.fn(async (_workspace: string, sha256: string) => {
      if (sha256 !== current.sha256) throw new Error('changed');
      current = { ...current, trust: { state: 'trusted' }, waiting: [] };
      return current;
    }),
    trusted: vi.fn(async () => []),
    untrust: vi.fn(async () => undefined),
  };
}

describe('Project settings trust', () => {
  beforeEach(() => {
    window.localStorage.clear();
    Object.defineProperty(HTMLDialogElement.prototype, 'showModal', {
      configurable: true,
      value: function (this: HTMLDialogElement) {
        this.open = true;
      },
    });
    Object.defineProperty(HTMLDialogElement.prototype, 'close', {
      configurable: true,
      value: function (this: HTMLDialogElement) {
        this.open = false;
        this.dispatchEvent(new Event('close'));
      },
    });
  });

  it('shows a banner for an untrusted file and trusts exactly the file shown', async () => {
    const user = userEvent.setup();
    const fake = client(status());
    render(ProjectTrust, { workspace: '/work/app', client: fake });
    expect(await screen.findByRole('status')).toHaveTextContent(
      'Only their deny and ask rules apply',
    );
    await user.click(screen.getByRole('button', { name: 'Review project settings' }));
    const dialog = screen.getByRole('dialog', { name: "Trust this project's settings?" });
    expect(dialog).toHaveTextContent('/work/app/.blackwall/project.json');
    expect(screen.getByLabelText('Project settings file')).toHaveTextContent('"command":"rm"');
    expect(dialog).toHaveTextContent('Applies after you trust it');
    expect(dialog).toHaveTextContent('1 allow rules');
    await user.click(screen.getByRole('button', { name: 'Trust' }));
    expect(fake.trust).toHaveBeenCalledWith('/work/app', 'aaa');
    await waitFor(() => expect(screen.queryByRole('status')).not.toBeInTheDocument());
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });

  it('shows the diff for a changed file and hides the banner on Not now', async () => {
    const user = userEvent.setup();
    const diff = '--- trusted\n+++ current\n@@ -1 +1 @@\n-{"policy":{}}\n+' + FILE + '\n';
    const fake = client(status({ trust: { state: 'changed', diff }, sha256: 'bbb' }));
    render(ProjectTrust, { workspace: '/work/app', client: fake });
    expect(await screen.findByRole('status')).toHaveTextContent('changed since you trusted them');
    await user.click(screen.getByRole('button', { name: 'Review project settings' }));
    const changes = screen.getByLabelText('Changes since you trusted it');
    expect(changes.querySelector('.added')).toHaveTextContent('+{"policy":{"rules"');
    expect(changes.querySelector('.removed')).toHaveTextContent('-{"policy":{}}');
    await user.click(screen.getByRole('button', { name: 'Not now' }));
    expect(fake.trust).not.toHaveBeenCalled();
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(screen.queryByRole('status')).not.toBeInTheDocument();
  });

  it('stays quiet for a trusted file or one with only deny and ask rules', async () => {
    for (const quiet of [
      status({ trust: { state: 'trusted' }, waiting: [] }),
      status({ waiting: [] }),
      status({ exists: false, content: null, sha256: null }),
    ]) {
      const fake = client(quiet);
      const { unmount } = render(ProjectTrust, { workspace: '/work/app', client: fake });
      await waitFor(() => expect(fake.status).toHaveBeenCalled());
      expect(screen.queryByRole('status')).not.toBeInTheDocument();
      unmount();
    }
  });

  it('offers no Trust button for a file that cannot be used', async () => {
    const user = userEvent.setup();
    const fake = client(
      status({
        sha256: null,
        content: null,
        applies: [],
        waiting: [],
        error: 'The project settings file is larger than 64 KiB. Every action will ask for approval.',
      }),
    );
    render(ProjectTrust, { workspace: '/work/app', client: fake });
    expect(await screen.findByRole('status')).toHaveTextContent('every action asks');
    await user.click(screen.getByRole('button', { name: 'Review project settings' }));
    expect(screen.getByRole('alert')).toHaveTextContent('larger than 64 KiB');
    expect(screen.queryByRole('button', { name: 'Trust' })).not.toBeInTheDocument();
  });

  it('reports a file that changed after it was shown and shows the new one', async () => {
    const user = userEvent.setup();
    const fake = client(status());
    fake.trust.mockRejectedValueOnce(
      'The project settings file changed after it was shown. Review it again.',
    );
    render(ProjectTrust, { workspace: '/work/app', client: fake });
    await user.click(await screen.findByRole('button', { name: 'Review project settings' }));
    await user.click(screen.getByRole('button', { name: 'Trust' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Review it again');
    expect(fake.status).toHaveBeenCalledTimes(3);
    expect(screen.getByRole('dialog')).toBeInTheDocument();
  });

  it('freezes what is shown when the file changes during review and offers a reload', async () => {
    const user = userEvent.setup();
    const fake = client(status());
    const view = render(ProjectTrust, { workspace: '/work/app', client: fake, refresh: 0 });
    await user.click(await screen.findByRole('button', { name: 'Review project settings' }));
    fake.replace(
      status({
        sha256: 'swapped',
        content: '{"policy":{"rules":[{"decision":"allow","kind":"exec","command":"curl"}]}}',
      }),
    );
    await view.rerender({ workspace: '/work/app', client: fake, refresh: 1 });
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent('The file changed while you were reviewing it. Review it again.');
    // The reviewed file is still what is shown, and it cannot be trusted any more.
    expect(screen.getByLabelText('Project settings file')).toHaveTextContent('"command":"rm"');
    expect(screen.getByRole('button', { name: 'Trust' })).toBeDisabled();
    await user.click(screen.getByRole('button', { name: 'Reload' }));
    expect(screen.getByLabelText('Project settings file')).toHaveTextContent('"command":"curl"');
    await user.click(screen.getByRole('button', { name: 'Trust' }));
    expect(fake.trust).toHaveBeenCalledWith('/work/app', 'swapped');
  });

  it('lists the parsed rules with badges and escapes invisible characters', async () => {
    const user = userEvent.setup();
    const hidden = `{"policy":{"rules":[]}}${'\n'.repeat(6)}x\u202Ey`;
    const fake = client(
      status({
        content: hidden,
        rules: [
          { decision: 'allow', kind: 'exec', subject: 'cargo\u200Btest', badges: [] },
          { decision: 'ask', kind: 'network', subject: '*.com', badges: ['any host'] },
          { decision: 'deny', kind: 'exec', subject: 'MCP github', badges: ['your MCP server'] },
        ],
      }),
    );
    render(ProjectTrust, { workspace: '/work/app', client: fake });
    await user.click(await screen.findByRole('button', { name: 'Review project settings' }));
    const rows = screen.getAllByRole('row').slice(1);
    expect(rows.map((row) => row.textContent)).toEqual([
      'allowexeccargo\\u{200B}test',
      'asknetwork*.comany host',
      'denyexecMCP githubyour MCP server',
    ]);
    const file = screen.getByLabelText('Project settings file').textContent ?? '';
    expect(file).toContain('x\\u{202E}y');
    expect(file).toContain('[3 more blank lines not shown]');
    expect(file).not.toMatch(/\p{Cf}/u);
  });

  it('remembers Not now per project and file version', async () => {
    const user = userEvent.setup();
    const first = client(status());
    const view = render(ProjectTrust, { workspace: '/work/app', client: first });
    await user.click(await screen.findByRole('button', { name: 'Review project settings' }));
    await user.click(screen.getByRole('button', { name: 'Not now' }));
    view.unmount();
    // Still dismissed after a restart, for the same file.
    const again = client(status());
    const second = render(ProjectTrust, { workspace: '/work/app', client: again });
    await waitFor(() => expect(again.status).toHaveBeenCalled());
    expect(screen.queryByRole('status')).not.toBeInTheDocument();
    second.unmount();
    // A different file version brings the banner back.
    render(ProjectTrust, { workspace: '/work/app', client: client(status({ sha256: 'new' })) });
    expect(await screen.findByRole('status')).toBeInTheDocument();
  });

  it('still works when storage is unavailable', async () => {
    const user = userEvent.setup();
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('blocked');
    });
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('blocked');
    });
    render(ProjectTrust, { workspace: '/work/app', client: client(status()) });
    await user.click(await screen.findByRole('button', { name: 'Review project settings' }));
    await user.click(screen.getByRole('button', { name: 'Not now' }));
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    vi.restoreAllMocks();
  });
});
