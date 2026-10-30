import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { authStatus, NO_KEYRING_NOTE } from '../auth';
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
  it('fills the form from a suggestion and marks servers already added', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'mcpServers').mockResolvedValue([
      { name: 'git', command: ['git-mcp'], enabled: true },
    ]);
    const add = vi.spyOn(persistence, 'addMcpServer').mockResolvedValue();
    render(McpDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Add server' }));
    expect(screen.getByRole('button', { name: /^git/ })).toBeDisabled();
    expect(screen.getByText('Already added')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: /^context7/ }));
    expect(screen.getByLabelText('Name')).toHaveValue('context7');
    expect(screen.getByLabelText('Command')).toHaveValue('npx -y @upstash/context7-mcp@4.4.1');
    expect(add).not.toHaveBeenCalled();
    await user.click(screen.getByRole('button', { name: 'Add server' }));
    expect(add).toHaveBeenCalledWith('context7', 'npx -y @upstash/context7-mcp@4.4.1');
  });
  it.each([
    ['filesystem', 'Add the folder', '/tmp/notes', '@modelcontextprotocol/server-filesystem@1.0.0'],
    ['remote', 'Add the web address', 'https://mcp.example.com/mcp', 'mcp-remote@0.14.3'],
  ])('asks for what %s needs before adding it', async (name, message, value, pkg) => {
    const user = userEvent.setup();
    const add = vi.spyOn(persistence, 'addMcpServer').mockResolvedValue();
    render(McpDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Add server' }));
    await user.click(screen.getByRole('button', { name: new RegExp(`^${name}`) }));
    expect(screen.getByLabelText('Command')).toHaveFocus();
    await user.click(screen.getByRole('button', { name: 'Add server' }));
    expect(await screen.findByRole('alert')).toHaveTextContent(message);
    expect(add).not.toHaveBeenCalled();
    await user.type(screen.getByLabelText('Command'), value);
    await user.click(screen.getByRole('button', { name: 'Add server' }));
    expect(add).toHaveBeenCalledWith(name, `npx -y ${pkg} ${value}`);
  });
  it('does not offer suggestions that cannot run on Windows', async () => {
    const user = userEvent.setup();
    vi.spyOn(navigator, 'userAgent', 'get').mockReturnValue('Mozilla/5.0 (Windows NT 10.0)');
    render(McpDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Add server' }));
    expect(screen.queryByRole('button', { name: /^context7/ })).not.toBeInTheDocument();
    expect(screen.getByText(/cannot run on Windows yet/)).toBeInTheDocument();
  });
  it('forgets a suggestion when the form is cancelled or the command is retyped', async () => {
    const user = userEvent.setup();
    const add = vi.spyOn(persistence, 'addMcpServer').mockResolvedValue();
    render(McpDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Add server' }));
    await user.click(screen.getByRole('button', { name: /^filesystem/ }));
    await user.click(screen.getByRole('button', { name: 'Add server' }));
    expect(await screen.findByRole('alert')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Add server' }));
    expect(screen.getByLabelText('Name')).toHaveValue('');
    expect(screen.getByLabelText('Command')).toHaveValue('');
    await user.click(screen.getByRole('button', { name: /^filesystem/ }));
    const command = screen.getByLabelText('Command');
    await user.clear(command);
    await user.type(command, 'npx @modelcontextprotocol/server-filesystem@1.0.0');
    expect(screen.getByRole('button', { name: /^filesystem/ })).toHaveAttribute(
      'aria-pressed',
      'false',
    );
    expect(screen.queryByText(/Add the folder this server may use/)).not.toBeInTheDocument();
    await user.clear(command);
    await user.type(command, 'my-server --stdio');
    await user.click(screen.getByRole('button', { name: 'Add server' }));
    expect(add).toHaveBeenCalledWith('filesystem', 'my-server --stdio');
  });
  it('adds a server by URL with headers that name secrets', async () => {
    const user = userEvent.setup();
    const added = {
      name: 'docs',
      url: 'https://mcp.example.com/mcp',
      headers: { Authorization: '{secret:DOCS_TOKEN}' },
      enabled: true,
    };
    // Another server at the same address must not be the one that opens.
    const other = { ...added, name: 'docs-old', headers: {} };
    const add = vi.spyOn(persistence, 'addMcpHttpServer').mockImplementation(async () => {
      vi.spyOn(persistence, 'mcpServers').mockResolvedValue([other, added]);
      return 'docs';
    });
    const command = vi.spyOn(persistence, 'addMcpServer').mockResolvedValue();
    render(McpDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Add server' }));
    expect(screen.getByRole('radio', { name: 'Command' })).toHaveAttribute('aria-checked', 'true');
    await user.click(screen.getByRole('radio', { name: 'URL' }));
    expect(screen.queryByLabelText('Command')).not.toBeInTheDocument();
    expect(screen.getByText(/OAuth sign-in are not supported yet/)).toBeInTheDocument();
    await user.type(screen.getByLabelText('Name'), 'Docs');
    await user.type(screen.getByLabelText('URL'), 'https://mcp.example.com/mcp');
    await user.click(screen.getByRole('button', { name: 'Add header' }));
    await user.type(screen.getByLabelText('Header 1 name'), 'Authorization');
    await user.type(screen.getByLabelText('Header 1 secret'), 'DOCS_TOKEN');
    await user.click(screen.getByRole('button', { name: 'Add header' }));
    await user.click(screen.getByRole('button', { name: 'Remove header 2' }));
    expect(screen.queryByLabelText('Header 2 name')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Add server' }));
    expect(add).toHaveBeenCalledWith('Docs', 'https://mcp.example.com/mcp', [
      ['Authorization', 'DOCS_TOKEN'],
    ]);
    expect(command).not.toHaveBeenCalled();
    // The new server opens with its first unsaved header secret ready to save.
    expect(await screen.findByText(/not saved yet/)).toBeInTheDocument();
    expect(screen.getByLabelText('Secret name')).toHaveValue('DOCS_TOKEN');
    expect(screen.getByText('https://mcp.example.com/mcp')).toBeInTheDocument();
    expect(screen.getByText('docs')).toBeInTheDocument();
  });
  it('lists URL servers by address and counts unsaved header secrets', async () => {
    vi.spyOn(persistence, 'mcpServers').mockResolvedValue([
      {
        name: 'docs',
        url: 'https://mcp.example.com/mcp',
        headers: {
          Authorization: '{secret:DOCS_TOKEN}',
          'X-Team': '{secret:TEAM}',
          'X-Old': '{secret:OLD}',
          'X-Env': '{secret:ENV}',
        },
        secrets: ['TEAM', 'OLD', 'ENV'],
        secretOrigins: { TEAM: 'https://mcp.example.com', OLD: 'https://old.example.com' },
        enabled: true,
      },
    ]);
    const user = userEvent.setup();
    render(McpDialog, { props: { onClose: vi.fn() } });
    expect(await screen.findByText('https://mcp.example.com/mcp')).toBeInTheDocument();
    expect(screen.getByText(/3 secrets · 3 secrets to save/)).toBeInTheDocument();
    await user.click(screen.getByText('docs'));
    expect(screen.getByText(/not saved yet/)).toBeInTheDocument();
    expect(screen.getByText(/saved for https:\/\/old.example.com; save it again/)).toBeInTheDocument();
    expect(screen.getByText(/saved for a local command; save it again/)).toBeInTheDocument();
  });
  it('switches back to a command when a suggestion is picked', async () => {
    const user = userEvent.setup();
    render(McpDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Add server' }));
    await user.click(screen.getByRole('radio', { name: 'URL' }));
    await user.click(screen.getByRole('button', { name: /^filesystem/ }));
    expect(screen.getByRole('radio', { name: 'Command' })).toHaveAttribute('aria-checked', 'true');
    expect((screen.getByLabelText('Command') as HTMLInputElement).value).toContain(
      'server-filesystem',
    );
  });
  it('disables saving a secret without a keyring', async () => {
    const user = userEvent.setup();
    authStatus.update((status) => ({ ...status, keyring: false }));
    try {
      render(McpDialog, { props: { onClose: vi.fn() } });
      await user.click(await screen.findByText('jira'));
      expect(screen.getByLabelText('Environment variable')).toBeDisabled();
      expect(screen.getByLabelText('Value')).toBeDisabled();
      expect(screen.getByRole('button', { name: 'Save secret' })).toBeDisabled();
      expect(screen.getByText(NO_KEYRING_NOTE)).toBeVisible();
    } finally {
      authStatus.update((status) => ({ ...status, keyring: true }));
    }
  });
});
