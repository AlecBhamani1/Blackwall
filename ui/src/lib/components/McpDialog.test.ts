import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { persistence } from '../persistence';
import McpDialog from './McpDialog.svelte';

describe('MCP server management', () => {
  beforeEach(() => {
    Object.defineProperty(HTMLDialogElement.prototype, 'showModal', {
      configurable: true,
      value: function (this: HTMLDialogElement) {
        this.open = true;
      },
    });
    vi.spyOn(persistence, 'available').mockReturnValue(true);
    vi.spyOn(persistence, 'mcpServers').mockResolvedValue([
      {
        name: 'jira',
        command: ['jira-mcp', '--stdio'],
        enabled: true,
        secrets: ['JIRA_API_TOKEN'],
      },
    ]);
    vi.spyOn(persistence, 'mcpToolRules').mockResolvedValue([]);
  });
  it('adds a server from a command line and toggles one', async () => {
    const user = userEvent.setup();
    const add = vi.spyOn(persistence, 'addMcpServer').mockResolvedValue();
    const toggle = vi.spyOn(persistence, 'setMcpServerEnabled').mockResolvedValue();
    render(McpDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Disable jira' }));
    expect(toggle).toHaveBeenCalledWith('jira', false);
    await user.click(screen.getByRole('button', { name: 'Add server' }));
    for (const field of ['Name', 'Command']) {
      expect(screen.getByLabelText(field)).toHaveAttribute('autocorrect', 'off');
      expect(screen.getByLabelText(field)).toHaveAttribute('autocapitalize', 'none');
    }
    await user.type(screen.getByLabelText('Name'), 'files');
    await user.type(screen.getByLabelText('Command'), 'npx -y server-filesystem /tmp');
    await user.click(screen.getByRole('button', { name: 'Add server' }));
    expect(add).toHaveBeenCalledWith('files', 'npx -y server-filesystem /tmp');
  });
  it('never carries a typed secret over to another server', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'mcpServers').mockResolvedValue([
      { name: 'jira', command: ['jira-mcp'], enabled: true },
      { name: 'gitlab', command: ['gitlab-mcp'], enabled: true },
    ]);
    render(McpDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByText('jira'));
    await user.type(screen.getByLabelText('Environment variable'), 'TOKEN');
    await user.type(screen.getByLabelText('Value'), 'jira-only');
    await user.click(screen.getByRole('button', { name: 'Back' }));
    await user.click(screen.getByText('gitlab'));
    expect(screen.getByLabelText('Environment variable')).toHaveValue('');
    expect(screen.getByLabelText('Value')).toHaveValue('');
  });
  it('saves a secret and clears the form', async () => {
    const user = userEvent.setup();
    const save = vi.spyOn(persistence, 'saveMcpSecret').mockResolvedValue();
    render(McpDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByText('jira'));
    expect(screen.getByText('JIRA_API_TOKEN')).toBeInTheDocument();
    await user.type(screen.getByLabelText('Environment variable'), 'JIRA_API_TOKEN');
    const value = screen.getByLabelText('Value');
    expect(value).toHaveAttribute('type', 'password');
    await user.type(value, 'secret-token');
    await user.click(screen.getByRole('button', { name: 'Save secret' }));
    expect(save).toHaveBeenCalledWith('jira', 'JIRA_API_TOKEN', 'secret-token');
    await vi.waitFor(() => expect(persistence.mcpServers).toHaveBeenCalledTimes(2));
    expect(screen.getByLabelText('Environment variable')).toHaveValue('');
    expect(screen.getByLabelText('Value')).toHaveValue('');
  });
  it('drops a secret value that failed to save', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'saveMcpSecret').mockRejectedValue('Keychain is locked.');
    render(McpDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByText('jira'));
    await user.type(screen.getByLabelText('Environment variable'), 'JIRA_API_TOKEN');
    await user.type(screen.getByLabelText('Value'), 'secret-token');
    await user.click(screen.getByRole('button', { name: 'Save secret' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Keychain is locked.');
    expect(screen.getByLabelText('Value')).toHaveValue('');
    expect(screen.getByLabelText('Environment variable')).toHaveValue('JIRA_API_TOKEN');
  });
  it('lists and removes the rules that allow a server without asking', async () => {
    const user = userEvent.setup();
    const rule = { tool: 'jira__search', project: '/work/app' };
    vi.spyOn(persistence, 'mcpToolRules').mockResolvedValue([
      { tool: 'jira', project: null },
      rule,
      { tool: 'docs', project: null },
    ]);
    const remove = vi.spyOn(persistence, 'removeMcpToolRule').mockResolvedValue();
    render(McpDialog, { props: { onClose: vi.fn() } });
    expect(await screen.findByText('2 allowed without asking · 1 secret')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Manage jira' }));
    expect(screen.getByText('Every tool')).toBeInTheDocument();
    expect(screen.getByText('In every project')).toBeInTheDocument();
    expect(screen.getByText('In /work/app')).toBeInTheDocument();
    expect(screen.queryByText('docs')).not.toBeInTheDocument();
    vi.spyOn(persistence, 'mcpToolRules').mockResolvedValue([{ tool: 'jira', project: null }]);
    await user.click(screen.getByRole('button', { name: 'Stop allowing jira__search in /work/app' }));
    expect(remove).toHaveBeenCalledWith(rule);
    await vi.waitFor(() => expect(screen.queryByText('In /work/app')).not.toBeInTheDocument());
  });
  it('still lists servers when the rules file cannot be read', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'mcpToolRules').mockRejectedValue('The policy file is not valid.');
    render(McpDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByText('jira'));
    expect(screen.getByText('The policy file is not valid.')).toBeInTheDocument();
  });
});
