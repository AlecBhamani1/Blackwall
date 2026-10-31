<script lang="ts">
  import { onMount } from 'svelte';
  import { COMMAND_TOOL_SUGGESTIONS, type CommandToolSuggestion } from '../commandToolSuggestions';
  import { formatCommand, parseCommand } from '../mcpCommand';
  import {
    persistence,
    type CommandCli,
    type CommandToolSummary,
  } from '../persistence';
  import Icon from './Icon.svelte';
  export let onClose: () => void;
  export let embedded = false;
  let dialog: HTMLDialogElement;
  let tools: CommandToolSummary[] = [];
  let clis: CommandCli[] = [];
  let form = false;
  // The tool being edited; empty when adding one.
  let editing = '';
  let name = '';
  let description = '';
  let commandLine = '';
  let promptOnStdin = false;
  let suggestion: CommandToolSuggestion | null = null;
  let busy = false;
  let error = '';
  const desktop = persistence.available();
  const UNCLOSED_QUOTE = 'The command has a quote without a closing quote.';
  async function load() {
    tools = await persistence.commandTools();
  }
  // What is known about a suggested program without running it.
  function found(item: CommandToolSuggestion) {
    const cli = clis.find((candidate) => candidate.name === item.name);
    if (!cli) return '';
    if (!cli.installed) return 'Not found on your PATH';
    return `Installed · ${cli.signedIn ? 'Sign-in found' : 'Not detected (may use the system keychain)'}`;
  }
  function suggest(item: CommandToolSuggestion) {
    suggestion = item;
    name = item.name;
    description = item.description;
    commandLine = formatCommand(item.command);
    promptOnStdin = item.promptOnStdin;
    error = '';
  }
  function close() {
    form = false;
    editing = '';
    suggestion = null;
    name = '';
    description = '';
    commandLine = '';
    promptOnStdin = false;
    error = '';
  }
  function edit(tool: CommandToolSummary) {
    close();
    editing = tool.name;
    name = tool.name;
    description = tool.description;
    commandLine = formatCommand(tool.command);
    form = true;
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
      error = 'The change was saved, but the tool list could not be refreshed.';
    }
    busy = false;
    return true;
  }
  async function save(event: SubmitEvent) {
    event.preventDefault();
    const words = parseCommand(commandLine);
    if (!words) {
      error = UNCLOSED_QUOTE;
      return;
    }
    if (!editing && tools.some((tool) => tool.name === name)) {
      error = `A tool named ${name} already exists. Edit it instead.`;
      return;
    }
    if (
      await run(
        () => persistence.saveCommandTool(name, description, words, promptOnStdin),
        'This tool could not be saved.',
      )
    )
      close();
  }
  onMount(() => {
    if (embedded) dialog.setAttribute('open', '');
    else dialog.showModal();
    if (!desktop) return;
    void load().catch(
      () => (error = 'Your command tools could not be opened. The file has been left in place.'),
    );
    // Detection only decorates the suggestions, so a failure shows nothing.
    void persistence
      .commandClis()
      .then((detected) => (clis = detected))
      .catch(() => {});
  });
</script>

<dialog class:embedded bind:this={dialog} onclose={onClose} aria-labelledby="tools-title">
  <header>
    <div>
      <h2 id="tools-title">Command tools</h2>
      <p>Programs such as Claude, Codex, or Copilot that agents, and subagents, can run.</p>
    </div>
    <button class="icon-button" aria-label="Close command tools" onclick={() => dialog.close()}
      ><Icon name="x" /></button
    >
  </header>
  <div class="content">
    {#if !desktop}<p>Command tools are available in the installed Blackwall desktop app.</p>
    {:else if form}
      <form onsubmit={save}>
        {#if !editing}
          <h3 class="first">Suggested</h3>
          <div class="suggestions">
            {#each COMMAND_TOOL_SUGGESTIONS as item (item.name)}{@const added = tools.some(
                (tool) => tool.name === item.name,
              )}<button
                type="button"
                class="suggestion"
                class:chosen={suggestion?.name === item.name}
                disabled={busy || added}
                aria-pressed={suggestion?.name === item.name}
                onclick={() => suggest(item)}
                ><strong>{item.name}</strong><small
                  >{added ? 'Already added' : item.description}</small
                >{#if !added && found(item)}<small class="found">{found(item)}</small>{/if}</button
              >{/each}
          </div>
          <p class="hint">
            Pick one to fill in the form, then review it and choose Save. Each call asks for your
            approval, and the program runs with your account's permissions.
          </p>
        {/if}
        <label for="tool-name">Name</label><input
          id="tool-name"
          autocapitalize="none"
          autocorrect="off"
          spellcheck="false"
          class="code"
          bind:value={name}
          pattern="[a-z0-9_]+"
          maxlength="64"
          required
          disabled={busy || Boolean(editing)}
          placeholder="review"
        />
        <p class="field-hint">Lowercase letters, digits, and underscores.</p>
        <label for="tool-description">Description</label><input
          id="tool-description"
          bind:value={description}
          maxlength="1024"
          required
          disabled={busy}
          placeholder="Ask another assistant for a read-only review"
        />
        <p class="field-hint">The model reads this to decide when to call the tool.</p>
        <label for="tool-command">Command</label><input
          id="tool-command"
          autocapitalize="none"
          autocorrect="off"
          spellcheck="false"
          class="code"
          bind:value={commandLine}
          required
          disabled={busy}
          placeholder="claude -p --permission-mode plan"
        />
        <p class="hint">
          Words are separated by spaces; put an argument that contains a space in quotes. A whole
          argument written <code>{'{prompt}'}</code> is filled in by the model. The program starts without
          a shell in the project folder.
        </p>
        {#if !editing}<label class="check"
            ><input type="checkbox" bind:checked={promptOnStdin} disabled={busy} /> Subagents can run
            on this tool</label
          >
          <p class="field-hint">
            The prompt is sent as JSON on standard input, so the command needs no
            <code>{'{prompt}'}</code>.
          </p>{/if}
        <div class="actions">
          <button type="button" class="secondary" disabled={busy} onclick={close}>Cancel</button
          ><span></span><button class="primary" disabled={busy}>{busy ? 'Saving…' : 'Save'}</button
          >
        </div>
      </form>
    {:else}
      <div class="intro">
        <span>Stored in tools.json in the Blackwall data folder.</span><button
          class="primary"
          disabled={busy}
          onclick={() => (form = true)}>Add tool</button
        >
      </div>
      {#if tools.length === 0}<p class="empty">
          No command tools yet. Add one to let agents call a program.
        </p>{/if}
      {#each tools as tool (tool.name)}<article>
          <span class="tool">
            <strong>{tool.name}</strong>
            <p class="code">{tool.kind === 'http' ? 'HTTP tool' : formatCommand(tool.command)}</p>
            <small>{tool.description}</small>
            <small
              >{tool.subagentProblem === null
                ? 'Subagents can run on this tool.'
                : `Not for subagents: ${tool.subagentProblem}`}</small
            >
          </span>
          {#if tool.kind === 'http'}<small>Edit in tools.json</small>{:else}<button
              class="secondary"
              disabled={busy}
              aria-label={`Edit ${tool.name}`}
              onclick={() => edit(tool)}>Edit</button
            ><button
              class="delete"
              disabled={busy}
              aria-label={`Remove ${tool.name}`}
              onclick={() =>
                run(
                  () => persistence.removeCommandTool(tool.name),
                  'This tool could not be removed.',
                )}>Remove</button
            >{/if}
        </article>{/each}
      <p class="hint">
        Changes apply from the next agent run. Choose a tool for subagents under Subagents.
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
  dialog.embedded {
    position: static;
    display: block;
    width: 100%;
    max-height: none;
    margin: 0;
    border: 0;
    border-radius: 0;
    background: transparent;
    box-shadow: none;
  }
  dialog.embedded header {
    padding: 0 0 20px;
  }
  dialog.embedded header .icon-button {
    display: none;
  }
  dialog.embedded .content {
    padding: 0;
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
  .delete {
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
    gap: 10px;
    align-items: center;
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 16px;
    margin: 10px 0;
  }
  article > small {
    color: var(--text-muted);
    font-size: 11.5px;
  }
  .tool {
    flex: 1;
    min-width: 0;
  }
  .tool strong {
    font-size: 13px;
    font-weight: 570;
  }
  .tool small {
    display: block;
    color: var(--text-muted);
    font-size: 11.5px;
    margin-top: 6px;
  }
  .tool p {
    color: var(--text-muted);
    font-size: 12px;
    line-height: 1.6;
    margin: 5px 0 0;
    overflow-wrap: anywhere;
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
  .suggestion .found {
    color: var(--text-primary);
  }
  .suggestion.chosen {
    border-color: var(--accent);
  }
  .suggestion:disabled {
    opacity: 0.55;
  }
  .code,
  code {
    font-family: var(--font-code);
    font-size: 12px;
  }
  code {
    color: var(--text-muted);
  }
  label {
    display: block;
    font-size: 12px;
    margin: 16px 0 8px;
  }
  label.check {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  input:not([type='checkbox']) {
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
