<script lang="ts">
  import { onMount } from 'svelte';
  import { persistence, type McpServer } from '../persistence';
  import Icon from './Icon.svelte';
  export let onClose: () => void;
  let dialog: HTMLDialogElement;
  let servers: McpServer[] = [];
  let selected: McpServer | null = null;
  let adding = false;
  let name = '';
  let commandLine = '';
  let secretName = '';
  let secretValue = '';
  let busy = false;
  let error = '';
  const desktop = persistence.available();
  // Secret fields belong to one server and are cleared whenever the view changes.
  function open(server: McpServer | null) {
    selected = server;
    secretName = '';
    secretValue = '';
  }
  async function load() {
    servers = await persistence.mcpServers();
    if (selected) open(servers.find((server) => server.name === selected?.name) ?? null);
  }
  async function run(action: () => Promise<void>, failure: string) {
    busy = true;
    error = '';
    try {
      await action();
    } catch (cause) {
      error = typeof cause === 'string' ? cause : failure;
      busy = false;
      return false;
    }
    try {
      await load();
    } catch {
      error = 'The change was saved, but the server list could not be refreshed.';
    }
    busy = false;
    return true;
  }
  async function add(event: SubmitEvent) {
    event.preventDefault();
    if (
      await run(
        () => persistence.addMcpServer(name, commandLine),
        'This server could not be added.',
      )
    ) {
      adding = false;
      name = '';
      commandLine = '';
    }
  }
  async function saveSecret(event: SubmitEvent) {
    event.preventDefault();
    if (!selected) return;
    const server = selected.name;
    const saved = await run(
      () => persistence.saveMcpSecret(server, secretName, secretValue),
      'This secret could not be saved.',
    );
    // The value is cleared either way, so it never stays in the page longer than needed.
    secretValue = '';
    if (saved) secretName = '';
  }
  onMount(() => {
    dialog.showModal();
    if (desktop)
      void load().catch(
        () => (error = 'Your MCP servers could not be opened. The file has been left in place.'),
      );
  });
</script>

<dialog bind:this={dialog} onclose={onClose} aria-labelledby="mcp-title">
  <header>
    <div>
      <h2 id="mcp-title">MCP servers</h2>
      <p>Local programs that give agent runs more tools.</p>
    </div>
    <button class="icon-button" aria-label="Close MCP servers" onclick={() => dialog.close()}
      ><Icon name="x" /></button
    >
  </header>
  <div class="content">
    {#if !desktop}<p>MCP servers are available in the installed Blackwall desktop app.</p>
    {:else if adding}
      <form onsubmit={add}>
        <label for="mcp-name">Name</label><input
          id="mcp-name"
          autocapitalize="none"
          autocorrect="off"
          spellcheck="false"
          bind:value={name}
          maxlength="32"
          required
          disabled={busy}
          placeholder="jira"
        />
        <p class="field-hint">
          Saved in lowercase with hyphens for spaces and underscores, so My Jira becomes my-jira.
        </p>
        <label for="mcp-command">Command</label><input
          id="mcp-command"
          autocapitalize="none"
          autocorrect="off"
          spellcheck="false"
          class="code"
          bind:value={commandLine}
          required
          disabled={busy}
          placeholder="npx -y @modelcontextprotocol/server-filesystem /path/to/folder"
        />
        <p class="hint">
          Words are separated by spaces; use <code>bw mcp add</code> or edit mcp.json for an argument
          that contains a space. Servers start without a shell in the Blackwall data folder, so pass a
          project path as an argument when a server needs one.
        </p>
        <div class="actions">
          <button type="button" class="secondary" disabled={busy} onclick={() => (adding = false)}
            >Cancel</button
          ><span></span><button class="primary" disabled={busy}
            >{busy ? 'Saving…' : 'Add server'}</button
          >
        </div>
      </form>
    {:else if selected}
      {@const server = selected}
      <div class="intro">
        <strong>{server.name}</strong><button
          class="secondary"
          disabled={busy}
          onclick={() => open(null)}>Back</button
        >
      </div>
      <code>{JSON.stringify(server.command)}</code>
      <h3>Secrets</h3>
      {#each server.secrets ?? [] as secret (secret)}<article>
          <span class="secret">{secret}</span><button
            class="delete"
            aria-label={`Remove ${secret}`}
            disabled={busy}
            onclick={() =>
              run(
                () => persistence.removeMcpSecret(server.name, secret),
                'This secret could not be removed.',
              )}>Remove</button
          >
        </article>{:else}<p class="empty">
          No secrets. Add one if this server needs a token.
        </p>{/each}
      <form onsubmit={saveSecret}>
        <label for="mcp-secret-name">Environment variable</label><input
          id="mcp-secret-name"
          autocapitalize="none"
          autocorrect="off"
          spellcheck="false"
          class="code"
          bind:value={secretName}
          pattern="[A-Z_][A-Z0-9_]*"
          maxlength="64"
          required
          disabled={busy}
          placeholder="JIRA_API_TOKEN"
        /><label for="mcp-secret-value">Value</label><input
          id="mcp-secret-value"
          type="password"
          autocomplete="new-password"
          bind:value={secretValue}
          maxlength="16384"
          required
          disabled={busy}
        />
        <p class="hint">
          Saved values are kept only in the system credential store and given to this server when it
          starts. They are never shown again.
        </p>
        <div class="actions">
          <button
            type="button"
            class="delete"
            disabled={busy}
            onclick={() =>
              run(
                () => persistence.removeMcpServer(server.name),
                'This server could not be removed.',
              )}>Remove server</button
          ><span></span><button class="primary" disabled={busy}
            >{busy ? 'Saving…' : 'Save secret'}</button
          >
        </div>
      </form>
    {:else}
      <div class="intro">
        <span>Stored in mcp.json in the Blackwall data folder.</span><button
          class="primary"
          disabled={busy}
          onclick={() => (adding = true)}>Add server</button
        >
      </div>
      {#if servers.length === 0}<p class="empty">
          No servers yet. Add one to offer its tools in agent tasks.
        </p>{/if}
      {#each servers as server (server.name)}<article>
          <button class="server" disabled={busy} onclick={() => open(server)}
            ><strong>{server.name}</strong>
            <p class="code">{server.command.join(' ')}</p></button
          ><button
            class:enabled={server.enabled}
            class="toggle"
            disabled={busy}
            aria-label={`${server.enabled ? 'Disable' : 'Enable'} ${server.name}`}
            onclick={() =>
              run(
                () => persistence.setMcpServerEnabled(server.name, !server.enabled),
                'This server could not be updated.',
              )}>{server.enabled ? 'Enabled' : 'Disabled'}</button
          >
        </article>{/each}
      <p class="hint">
        Tools are named <code>server__tool</code>. Every call asks for approval unless an owner rule
        allows it; server descriptions never grant permission. Changes apply from the next agent
        run.
      </p>
    {/if}
    {#if error}<p role="alert" class="error">{error}</p>{/if}
  </div>
</dialog>

<style>
  dialog {
    width: min(660px, calc(100vw - 32px));
    max-height: calc(100vh - 48px);
    padding: 0;
    margin: auto;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-lg);
    background: var(--bg-panel);
    color: var(--text-primary);
    box-shadow: var(--shadow-menu);
  }
  dialog::backdrop {
    background: var(--bg-scrim);
    backdrop-filter: blur(3px);
  }
  header {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 15px;
    padding: 24px;
    border-bottom: 1px solid var(--border);
  }
  h2 {
    margin: 0 0 5px;
    font-size: 18px;
    font-weight: 620;
  }
  h3 {
    margin: 22px 0 6px;
    font-size: 13px;
    font-weight: 570;
  }
  header p,
  .hint {
    font-size: 12px;
    color: var(--text-muted);
    line-height: 1.6;
    margin: 0;
  }
  .hint {
    margin: 18px 0;
  }
  .content {
    padding: 24px;
  }
  .icon-button {
    background: transparent;
    color: var(--text-muted);
    padding: 4px;
  }
  .intro,
  .actions {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .intro {
    justify-content: space-between;
    margin-bottom: 18px;
  }
  .intro > span {
    color: var(--text-muted);
    font-size: 12px;
  }
  .actions > span {
    flex: 1;
  }
  .primary,
  .secondary,
  .delete,
  .toggle {
    padding: 8px 12px;
    border-radius: var(--radius-sm);
    font-size: 11.5px;
  }
  .primary {
    background: var(--accent);
    color: var(--text-on-accent);
    font-weight: 600;
  }
  .secondary {
    background: var(--bg-elevated);
    border: 1px solid var(--border);
  }
  .delete {
    background: transparent;
    color: var(--err);
  }
  article {
    display: flex;
    gap: 15px;
    align-items: center;
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 16px;
    margin: 10px 0;
  }
  .server {
    flex: 1;
    min-width: 0;
    text-align: left;
    background: transparent;
    padding: 0;
  }
  .server strong {
    font-size: 13px;
    font-weight: 570;
  }
  .server p {
    color: var(--text-muted);
    font-size: 12px;
    line-height: 1.6;
    margin: 5px 0 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .secret {
    flex: 1;
  }
  .code,
  code,
  .secret {
    font-family: var(--font-code);
    font-size: 12px;
  }
  code {
    display: block;
    color: var(--text-muted);
    overflow-wrap: anywhere;
  }
  .hint code {
    display: inline;
  }
  .toggle {
    color: var(--text-muted);
    background: var(--bg-elevated);
    border: 1px solid var(--border);
  }
  .toggle.enabled {
    color: var(--ok);
  }
  label {
    display: block;
    font-size: 12px;
    margin: 16px 0 8px;
  }
  input {
    width: 100%;
    background: var(--bg-base);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-sm);
    padding: 11px 12px;
    font-size: 12px;
  }
  .field-hint {
    color: var(--text-muted);
    font-size: 11.5px;
    margin: 6px 0 0;
  }
  .empty {
    color: var(--text-muted);
    font-size: 12px;
    padding: 12px 0;
  }
  .error {
    color: var(--err);
    font-size: 12px;
  }
  button:disabled {
    opacity: 0.5;
  }
</style>
