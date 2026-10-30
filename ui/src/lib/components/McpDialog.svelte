<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { MCP_SUGGESTIONS, type McpSuggestion } from '../mcpSuggestions';
  import { persistence, type McpServer, type McpToolRule } from '../persistence';
  import { authStatus, NO_KEYRING_NOTE } from '../auth';
  import { detectPlatform } from '../platform';
  import Icon from './Icon.svelte';
  export let onClose: () => void;
  let dialog: HTMLDialogElement;
  let servers: McpServer[] = [];
  let rules: McpToolRule[] = [];
  let rulesError = '';
  let selected: McpServer | null = null;
  let adding = false;
  let name = '';
  let commandLine = '';
  let commandInput: HTMLInputElement;
  // A server is a local command or a streamable HTTP address with secret-backed headers.
  let mode: 'command' | 'url' = 'command';
  let url = '';
  let headerRows: { header: string; secret: string }[] = [];
  let suggestion: McpSuggestion | null = null;
  // Suggestions start with npx, a batch file on Windows, which servers cannot run there.
  const windows = detectPlatform() === 'windows';
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
  function secretOf(reference: string) {
    return reference.replace(/^\{secret:(.*)\}$/, '$1');
  }
  function originOf(url: string) {
    try {
      return new URL(url).origin;
    } catch {
      return '';
    }
  }
  // Why a header secret will not be sent: not saved, or saved for another address or for a
  // local command. Secrets are only ever sent to the origin they were saved for.
  function secretProblem(server: McpServer, secret: string) {
    if (!(server.secrets ?? []).includes(secret)) return 'not saved yet';
    const saved = server.secretOrigins?.[secret];
    if (!saved) return 'saved for a local command; save it again';
    if (saved !== originOf(server.url ?? '')) return `saved for ${saved}; save it again`;
    return '';
  }
  function missingSecrets(server: McpServer) {
    return Object.values(server.headers ?? {})
      .map(secretOf)
      .filter((secret) => secretProblem(server, secret));
  }
  async function load() {
    servers = await persistence.mcpServers();
    if (selected) open(servers.find((server) => server.name === selected?.name) ?? null);
    // Rules live in policy.json, so a problem there does not hide the servers.
    try {
      rules = await persistence.mcpToolRules();
      rulesError = '';
    } catch (cause) {
      rules = [];
      rulesError =
        typeof cause === 'string' ? cause : 'Saved rules could not be read from policy.json.';
    }
  }
  // Saved Windows paths keep their verbatim prefix for matching; it is hidden when shown.
  function folder(path: string) {
    return path.replace(/^\\\\\?\\/, '');
  }
  // Rules are passed in, so the markup updates when they reload.
  function serverRules(rules: McpToolRule[], server: string) {
    return rules.filter((rule) => rule.tool === server || rule.tool.startsWith(`${server}__`));
  }
  function summary(rules: McpToolRule[], server: McpServer) {
    const allowed = serverRules(rules, server.name).length;
    const secrets = server.secrets?.length ?? 0;
    const missing = missingSecrets(server).length;
    return [
      allowed ? `${allowed} allowed without asking` : 'Asks before every call',
      secrets ? `${secrets} secret${secrets === 1 ? '' : 's'}` : '',
      missing ? `${missing} secret${missing === 1 ? '' : 's'} to save` : '',
    ]
      .filter(Boolean)
      .join(' · ');
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
  async function suggest(item: McpSuggestion) {
    mode = 'command';
    suggestion = item;
    name = item.name;
    commandLine = item.command.join(' ') + (item.needs ? ' ' : '');
    error = '';
    if (!item.needs) return;
    await tick();
    commandInput.focus();
    commandInput.setSelectionRange(commandLine.length, commandLine.length);
  }
  function cancelAdd() {
    adding = false;
    suggestion = null;
    name = '';
    commandLine = '';
    url = '';
    headerRows = [];
    mode = 'command';
    error = '';
  }
  // A command retyped by the owner is no longer the suggestion, so its hint and check stop.
  function commandEdited(event: Event) {
    const value = (event.currentTarget as HTMLInputElement).value.trim();
    if (suggestion && !value.startsWith(suggestion.command.join(' '))) suggestion = null;
  }
  async function add(event: SubmitEvent) {
    event.preventDefault();
    if (mode === 'url') return addUrl();
    const words = commandLine.trim().split(/\s+/);
    if (suggestion?.needs && words.length <= suggestion.command.length) {
      error =
        suggestion.needs === 'folder'
          ? 'Add the folder this server may use at the end of the command.'
          : 'Add the web address of the server at the end of the command.';
      return;
    }
    if (
      await run(
        () => persistence.addMcpServer(name, commandLine),
        'This server could not be added.',
      )
    ) {
      adding = false;
      suggestion = null;
      name = '';
      commandLine = '';
    }
  }
  async function addUrl() {
    const headers = headerRows
      .filter((row) => row.header.trim() || row.secret.trim())
      .map((row): [string, string] => [row.header.trim(), row.secret.trim()]);
    let saved = '';
    if (
      await run(async () => {
        saved = await persistence.addMcpHttpServer(name, url, headers);
      }, 'This server could not be added.')
    ) {
      // The new server opens so its header secrets can be saved next.
      const added = servers.find((server) => server.name === saved);
      cancelAdd();
      if (added) {
        open(added);
        secretName = missingSecrets(added)[0] ?? '';
      }
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
      <p>Local programs and HTTP servers that give agent runs more tools.</p>
    </div>
    <button class="icon-button" aria-label="Close MCP servers" onclick={() => dialog.close()}
      ><Icon name="x" /></button
    >
  </header>
  <div class="content">
    {#if !desktop}<p>MCP servers are available in the installed Blackwall desktop app.</p>
    {:else if adding}
      <form onsubmit={add}>
        <h3 class="first">Suggested</h3>
        {#if windows}<p class="hint">
            Suggested servers start with <code>npx</code>, which Blackwall cannot run on Windows yet.
            Add a server whose program is an <code>.exe</code> instead.
          </p>
        {:else}<div class="suggestions">
            {#each MCP_SUGGESTIONS as item (item.name)}{@const added = servers.some(
                (server) => server.name === item.name,
              )}<button
                type="button"
                class="suggestion"
                class:chosen={suggestion?.name === item.name}
                disabled={busy || added}
                aria-pressed={suggestion?.name === item.name}
                onclick={() => suggest(item)}
                ><strong>{item.name}</strong><small
                  >{added ? 'Already added' : item.description}</small
                ></button
              >{/each}
          </div>
          <p class="hint">
            Pick one to fill in the form, then review it and choose Add server. The first agent run
            after adding one downloads that exact version from npm, so it needs a network connection.
          </p>
        {/if}
        <div class="modes" role="radiogroup" aria-label="Server type">
          {#each [['command', 'Command'], ['url', 'URL']] as [value, label] (value)}<button
              type="button"
              role="radio"
              aria-checked={mode === value}
              class:chosen={mode === value}
              disabled={busy}
              onclick={() => {
                mode = value as 'command' | 'url';
                suggestion = null;
                error = '';
              }}>{label}</button
            >{/each}
        </div>
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
        {#if mode === 'url'}
          <label for="mcp-url">URL</label><input
            id="mcp-url"
            type="url"
            autocapitalize="none"
            autocorrect="off"
            spellcheck="false"
            class="code"
            bind:value={url}
            maxlength="2048"
            required
            disabled={busy}
            placeholder="https://mcp.example.com/mcp"
          />
          <p class="field-hint">
            A streamable HTTP endpoint. Use https, or http only on 127.0.0.1 or ::1.
          </p>
          <h3>Headers</h3>
          {#each headerRows as row, index (index)}<div class="header-row">
              <input
                aria-label={`Header ${index + 1} name`}
                autocapitalize="none"
                autocorrect="off"
                spellcheck="false"
                class="code"
                bind:value={row.header}
                maxlength="64"
                required
                disabled={busy}
                placeholder="Authorization"
              /><input
                aria-label={`Header ${index + 1} secret`}
                autocapitalize="none"
                autocorrect="off"
                spellcheck="false"
                class="code"
                bind:value={row.secret}
                pattern="[A-Z_][A-Z0-9_]*"
                maxlength="64"
                required
                disabled={busy}
                placeholder="API_TOKEN"
              /><button
                type="button"
                class="delete"
                aria-label={`Remove header ${index + 1}`}
                disabled={busy}
                onclick={() => (headerRows = headerRows.filter((_, other) => other !== index))}
                >Remove</button
              >
            </div>{/each}
          <button
            type="button"
            class="secondary"
            disabled={busy || headerRows.length >= 16}
            onclick={() => (headerRows = [...headerRows, { header: '', secret: '' }])}
            >Add header</button
          >
          <p class="hint">
            Each header is sent with the value of a secret saved for this server, never a value typed
            here. After adding the server, save each secret's full header value, such as
            <code>Bearer &lt;token&gt;</code> for Authorization. Servers that need OAuth sign-in are not
            supported yet.
          </p>
        {:else}
        <label for="mcp-command">Command</label><input
          id="mcp-command"
          autocapitalize="none"
          autocorrect="off"
          spellcheck="false"
          class="code"
          bind:this={commandInput}
          bind:value={commandLine}
          oninput={commandEdited}
          required
          disabled={busy}
          placeholder="npx -y @modelcontextprotocol/server-filesystem /path/to/folder"
        />
        {#if suggestion?.needs}<p class="field-hint">
            {suggestion.needs === 'folder'
              ? 'Add the folder this server may use at the end, such as /Users/me/projects.'
              : 'Add the web address of the server at the end, such as https://mcp.example.com/mcp.'}
          </p>{/if}
        <p class="hint">
          Words are separated by spaces; use <code>bw mcp add</code> or edit mcp.json for an argument
          that contains a space. Servers start without a shell in the Blackwall data folder, so pass a
          project path as an argument when a server needs one.
        </p>
        {/if}
        <div class="actions">
          <button type="button" class="secondary" disabled={busy} onclick={cancelAdd}
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
      {#if server.url}<code>{server.url}</code>
        {#each Object.entries(server.headers ?? {}) as [header, reference] (header)}{@const problem =
            secretProblem(server, secretOf(reference))}<p class="header-line">
            <code>{header}</code> from secret <code>{secretOf(reference)}</code>{#if problem}<span
                class="missing">{` · ${problem}`}</span
              >{/if}
          </p>{/each}
      {:else}<code>{JSON.stringify(server.command ?? [])}</code>{/if}
      <h3>Allowed without asking</h3>
      {#each serverRules(rules, server.name) as rule (`${rule.tool} ${rule.project}`)}<article>
          <span class="rule"
            ><strong class:tool={rule.tool !== server.name}
              >{rule.tool === server.name
                ? 'Every tool'
                : rule.tool.slice(server.name.length + 2)}</strong
            >
            <small>{rule.project ? `In ${folder(rule.project)}` : 'In every project'}</small></span
          ><button
            class="delete"
            aria-label={`Stop allowing ${rule.tool}${rule.project ? ` in ${rule.project}` : ''}`}
            disabled={busy}
            onclick={() =>
              run(
                () => persistence.removeMcpToolRule(rule),
                'This rule could not be removed.',
              )}>Remove</button
          >
        </article>{:else}<p class="empty">
          {rulesError || 'Every call asks for approval. Choose Always allow on an approval to skip it.'}
        </p>{/each}
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
        <label for="mcp-secret-name">{server.url ? 'Secret name' : 'Environment variable'}</label><input
          id="mcp-secret-name"
          autocapitalize="none"
          autocorrect="off"
          spellcheck="false"
          class="code"
          bind:value={secretName}
          pattern="[A-Z_][A-Z0-9_]*"
          maxlength="64"
          required
          disabled={busy || !$authStatus.keyring}
          placeholder={server.url ? 'API_TOKEN' : 'JIRA_API_TOKEN'}
        /><label for="mcp-secret-value">Value</label><input
          id="mcp-secret-value"
          type="password"
          autocomplete="new-password"
          bind:value={secretValue}
          maxlength="16384"
          required
          disabled={busy || !$authStatus.keyring}
        />
        {#if !$authStatus.keyring}<p class="hint">{NO_KEYRING_NOTE}</p>{/if}
        <p class="hint">
          Saved values are kept only in the system credential store and given to this server when it
          {server.url ? 'connects, in the headers that name them' : 'starts'}. They are never shown
          again.
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
          ><span></span><button class="primary" disabled={busy || !$authStatus.keyring}
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
            <p class="code">{server.url ?? (server.command ?? []).join(' ')}</p>
            <small>{summary(rules, server)}</small></button
          ><button
            class="secondary manage"
            disabled={busy}
            aria-label={`Manage ${server.name}`}
            onclick={() => open(server)}>Manage <Icon name="chevron-right" size={13} /></button
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
  h3.first {
    margin-top: 0;
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
  .server small {
    display: block;
    color: var(--text-muted);
    font-size: 11.5px;
    margin-top: 6px;
  }
  .suggestions {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
    gap: 8px;
  }
  .suggestion {
    display: grid;
    gap: 4px;
    text-align: left;
    padding: 10px 12px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--bg-elevated);
  }
  .suggestion strong {
    font-family: var(--font-code);
    font-size: 12px;
  }
  .suggestion small {
    color: var(--text-muted);
    font-size: 11px;
    line-height: 1.4;
  }
  .suggestion.chosen {
    border-color: var(--accent);
  }
  .suggestion:disabled {
    opacity: 0.55;
  }
  .modes {
    display: inline-flex;
    margin: 18px 0 0;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    overflow: hidden;
  }
  .modes button {
    padding: 7px 14px;
    font-size: 11.5px;
    background: var(--bg-elevated);
    color: var(--text-muted);
  }
  .modes button.chosen {
    background: var(--accent);
    color: var(--text-on-accent);
    font-weight: 600;
  }
  .header-row {
    display: grid;
    grid-template-columns: 1fr 1fr auto;
    gap: 8px;
    align-items: center;
    margin: 0 0 8px;
  }
  .header-line {
    margin: 6px 0 0;
    font-size: 12px;
    color: var(--text-muted);
  }
  .header-line code {
    display: inline;
  }
  .missing {
    color: var(--err);
  }
  .manage {
    display: flex;
    align-items: center;
    gap: 4px;
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
  .rule {
    flex: 1;
    display: grid;
    gap: 2px;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .rule strong {
    font-size: 12px;
  }
  .rule .tool {
    font-family: var(--font-code);
  }
  .rule small {
    color: var(--text-muted);
    font-size: 11px;
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
