<script lang="ts">
  import { onMount } from 'svelte';
  import {
    persistence,
    instructionFileValid,
    MAX_INSTRUCTION_FILES,
    SUBAGENT_TOOLS,
    type CommandToolsIsolation,
    type SubagentTool,
    type MemoryEntry,
    type MemoryProposal,
    type Preferences,
  } from '../persistence';
  import { createId } from '../id';
  import Icon from './Icon.svelte';
  import { platform } from '../platform';
  export let contextPreferences: Preferences = {};
  export let onPreferences: (preferences: {
    memoryEnabled: boolean;
    contextWindow: number;
    outputTokens: number;
    autoCompact: boolean;
    maxIterations: number;
    commandTimeoutSeconds: number;
    maxOutputBytes: number;
  }) => void = () => {};
  export let onClose: () => void;
  export let embedded = false;
  export let section: 'memory' | 'limits' | 'subagents' | 'instructions' = 'memory';
  let dialog: HTMLDialogElement;
  let entries: MemoryEntry[] = [];
  let content = '';
  let editingId = '';
  let editingEntry: MemoryEntry | undefined;
  let proposals: MemoryProposal[] = [];
  let drafts: Record<string, string> = {};
  let sourceMessages: Record<string, string> = {};
  let query = '';
  let error = '';
  let busy = false;
  let enabled = false;
  let contextWindow = 32000;
  let outputTokens = 4096;
  let autoCompact = false;
  let maxIterations = 20;
  let commandTimeoutSeconds = 120;
  let maxOutputBytes = 65536;
  let subagentMaxSteps = 8;
  let subagentTools: SubagentTool[] = ['read_file', 'list_files'];
  let subagentRunsOn: 'local' | 'tool' = 'local';
  let subagentTool = '';
  let commandToolsIsolation: CommandToolsIsolation = 'outside';
  const toolLabels: Record<SubagentTool, string> = {
    read_file: 'Read files',
    list_files: 'List folders',
    search_files: 'Search files',
    write_file: 'Propose file edits (asks)',
    shell: 'Run commands (asks)',
    web_fetch: 'Read web pages (asks)',
    web_search: 'Search the web (asks)',
  };
  $: subagentToolValid = /^[a-z0-9_]{1,64}$/.test(subagentTool);
  $: subagentsValid =
    Number.isInteger(Number(subagentMaxSteps)) &&
    subagentMaxSteps >= 1 &&
    subagentMaxSteps <= 50 &&
    (subagentRunsOn === 'local' || subagentToolValid);
  function toggleTool(tool: SubagentTool, on: boolean) {
    subagentTools = SUBAGENT_TOOLS.filter((name) =>
      name === tool ? on : subagentTools.includes(name),
    );
  }
  async function saveSubagents() {
    busy = true;
    error = '';
    try {
      await persistence.savePreferences({
        subagentMaxSteps: Number(subagentMaxSteps),
        subagentTools,
        subagentRunsOn: subagentRunsOn === 'local' ? 'local' : { tool: subagentTool },
        commandToolsIsolation,
      });
    } catch (cause) {
      error = typeof cause === 'string' ? cause : 'These settings could not be saved. Try again.';
    } finally {
      busy = false;
    }
  }
  let instructionMaxFileKib = 8;
  let instructionMaxTotalKib = 32;
  let instructionFiles = '';
  let maxSkillSizeMode: 'auto' | 'manual' = 'auto';
  let maxSkillSizeKib = 32;
  $: instructionFileList = instructionFiles
    .split('\n')
    .map((name) => name.trim())
    .filter((name) => name !== '');
  $: instructionsValid =
    Number.isInteger(Number(instructionMaxFileKib)) &&
    instructionMaxFileKib >= 1 &&
    instructionMaxFileKib <= 64 &&
    Number.isInteger(Number(instructionMaxTotalKib)) &&
    instructionMaxTotalKib >= 8 &&
    instructionMaxTotalKib <= 256 &&
    (maxSkillSizeMode === 'auto' ||
      (Number.isInteger(Number(maxSkillSizeKib)) &&
        maxSkillSizeKib >= 32 &&
        maxSkillSizeKib <= 256)) &&
    instructionFileList.length <= MAX_INSTRUCTION_FILES &&
    new Set(instructionFileList).size === instructionFileList.length &&
    instructionFileList.every(instructionFileValid);
  async function saveInstructions() {
    busy = true;
    error = '';
    try {
      await persistence.savePreferences({
        instructionMaxFileKib: Number(instructionMaxFileKib),
        instructionMaxTotalKib: Number(instructionMaxTotalKib),
        instructionExtraFiles: instructionFileList,
        maxSkillSizeKib: maxSkillSizeMode === 'auto' ? null : Number(maxSkillSizeKib),
      });
    } catch (cause) {
      error = typeof cause === 'string' ? cause : 'These settings could not be saved. Try again.';
    } finally {
      busy = false;
    }
  }
  let loaded = false;
  let savedPreferences: Awaited<ReturnType<typeof persistence.preferences>> = {};
  const desktop = persistence.available();
  let searchVersion = 0;
  async function refresh() {
    const version = ++searchVersion;
    try {
      const [result, pending] = await Promise.all([
        persistence.memories(query),
        persistence.memoryProposals(),
      ]);
      if (version === searchVersion) {
        entries = result;
        proposals = pending;
      }
    } catch {
      error = 'Your memories could not be loaded. Existing data has been left in place.';
    }
  }
  async function save(event: SubmitEvent) {
    event.preventDefault();
    busy = true;
    error = '';
    try {
      await persistence.saveMemory({
        ...editingEntry,
        id: editingId || createId('memory'),
        content: content.trim(),
        updatedAt: Date.now(),
      });
      content = '';
      editingId = '';
      editingEntry = undefined;
      await refresh();
    } catch (cause) {
      error =
        typeof cause === 'string'
          ? cause
          : 'This memory could not be saved. Check disk space and try again.';
    } finally {
      busy = false;
    }
  }
  async function remove(id: string) {
    busy = true;
    error = '';
    try {
      await persistence.removeMemory(id);
      if (editingId === id) {
        editingId = '';
        editingEntry = undefined;
        content = '';
      }
      await refresh();
    } catch {
      error = 'This memory could not be deleted. Try again.';
    } finally {
      busy = false;
    }
  }
  async function showSource(id: string, source: { sessionId: string; messageId: string }) {
    try {
      const session = await persistence.load(source.sessionId);
      const message = session?.messages.find((message) => message.id === source.messageId);
      sourceMessages[id] = message
        ? message.content.slice(0, 2000) + (message.content.length > 2000 ? '\n[excerpt]' : '')
        : 'The source conversation or message is no longer available.';
    } catch {
      sourceMessages[id] = 'The source conversation could not be opened.';
    }
  }
  async function review(proposal: MemoryProposal, approve: boolean) {
    busy = true;
    error = '';
    try {
      if (approve)
        await persistence.approveMemory(proposal.id, drafts[proposal.id] ?? proposal.entry.content);
      else await persistence.rejectMemory(proposal.id);
      delete drafts[proposal.id];
      await refresh();
    } catch (cause) {
      error =
        typeof cause === 'string'
          ? cause
          : 'This proposal could not be reviewed. Refresh and try again; a saved memory may have changed.';
    } finally {
      busy = false;
    }
  }
  async function saveSettings() {
    busy = true;
    error = '';
    try {
      if (desktop)
        await persistence.savePreferences({
          memoryEnabled: enabled,
          contextWindow: Number(contextWindow),
          outputTokens: Number(outputTokens),
          autoCompact,
          maxIterations: Number(maxIterations),
          commandTimeoutSeconds: Number(commandTimeoutSeconds),
          maxOutputBytes: Number(maxOutputBytes),
        });
      onPreferences({
        memoryEnabled: enabled,
        contextWindow: Number(contextWindow),
        outputTokens: Number(outputTokens),
        autoCompact,
        maxIterations: Number(maxIterations),
        commandTimeoutSeconds: Number(commandTimeoutSeconds),
        maxOutputBytes: Number(maxOutputBytes),
      });
    } catch {
      error = 'These settings could not be saved. Try again.';
    } finally {
      busy = false;
    }
  }
  onMount(() => {
    if (embedded) dialog.setAttribute('open', '');
    else dialog.showModal();
    if (!desktop) {
      contextWindow = contextPreferences.contextWindow ?? 32000;
      outputTokens =
        contextPreferences.outputTokens ?? Math.min(4096, Math.floor(contextWindow / 4));
      autoCompact = contextPreferences.autoCompact ?? false;
      maxIterations = contextPreferences.maxIterations ?? 20;
      commandTimeoutSeconds = contextPreferences.commandTimeoutSeconds ?? 120;
      maxOutputBytes = contextPreferences.maxOutputBytes ?? 65536;
      loaded = true;
      return;
    }
    void (async () => {
      try {
        savedPreferences = await persistence.preferences();
        enabled = savedPreferences.memoryEnabled ?? false;
        contextWindow = savedPreferences.contextWindow ?? 32000;
        outputTokens =
          savedPreferences.outputTokens ?? Math.min(4096, Math.floor(contextWindow / 4));
        autoCompact = savedPreferences.autoCompact ?? false;
        maxIterations = savedPreferences.maxIterations ?? 20;
        commandTimeoutSeconds = savedPreferences.commandTimeoutSeconds ?? 120;
        maxOutputBytes = savedPreferences.maxOutputBytes ?? 65536;
        subagentMaxSteps = savedPreferences.subagentMaxSteps ?? 8;
        subagentTools = savedPreferences.subagentTools ?? ['read_file', 'list_files'];
        const runsOn = savedPreferences.subagentRunsOn ?? 'local';
        subagentRunsOn = runsOn === 'local' ? 'local' : 'tool';
        subagentTool = runsOn === 'local' ? '' : runsOn.tool;
        commandToolsIsolation = savedPreferences.commandToolsIsolation ?? 'outside';
        instructionMaxFileKib = savedPreferences.instructionMaxFileKib ?? 8;
        instructionMaxTotalKib = savedPreferences.instructionMaxTotalKib ?? 32;
        maxSkillSizeMode = savedPreferences.maxSkillSizeKib == null ? 'auto' : 'manual';
        maxSkillSizeKib = savedPreferences.maxSkillSizeKib ?? 32;
        instructionFiles = (savedPreferences.instructionExtraFiles ?? []).join('\n');
        await refresh();
        loaded = true;
      } catch {
        error = 'Your memory settings could not be opened.';
      }
    })();
  });
</script>

<dialog class:embedded bind:this={dialog} onclose={onClose} aria-labelledby="memory-title">
  {#if !embedded}<header>
    <div>
      <h2 id="memory-title">Memory</h2>
      <p>A little context, carried into your next conversation.</p>
    </div>
    <button class="icon-button" aria-label="Close memory" onclick={() => dialog.close()}
      ><Icon name="x" /></button
    >
    </header>{/if}
  <div class="content">
    {#if embedded}<h2 id="memory-title">
        {section === 'memory'
          ? 'Memory'
          : section === 'limits'
            ? 'Limits'
            : section === 'subagents'
              ? 'Subagents'
              : 'Instructions'}
      </h2>{/if}
    {#if section === 'memory'}
    {#if !desktop}<p>Memory is available in the installed Blackwall desktop app.</p>
    {:else}
      <div class="preference">
        <div>
          <strong>Use saved memories in chat</strong>
          <p>
            Only saved or approved memories are included. Turning this off also disables agent
            learning. Shared guest chats never receive them.
          </p>
        </div>
        <input
          aria-label="Use saved memories in chat"
          type="checkbox"
          bind:checked={enabled}
          disabled={busy || !loaded}
          onchange={saveSettings}
        />
      </div>
      {#if proposals.length}
        <section aria-label="Pending memory proposals" class="proposals">
          <h3>Pending review · {proposals.length}</h3>
          <p class="hint">
            The agent suggested these facts. Review or edit them before they enter future context.
          </p>
          {#each proposals as proposal (proposal.id)}
            <article>
              <strong>{proposal.entry.scope} · {proposal.entry.key}</strong>
              {#if proposal.entry.workspace}<p class="hint">
                  Project: {proposal.entry.workspace}
                </p>{/if}
              <p class="hint">
                Source session: {proposal.source.sessionId}<br />Message: {proposal.source
                  .messageId}
              </p>
              <button
                class="secondary"
                disabled={busy}
                  onclick={() => showSource(proposal.id, proposal.source)}
                  >View source message</button
              >
              {#if sourceMessages[proposal.id]}<pre>{sourceMessages[proposal.id]}</pre>{/if}
              <label for={`before-${proposal.id}`}>Saved text (−)</label>
              <pre id={`before-${proposal.id}`}>{proposal.before?.content ?? '(new fact)'}</pre>
              <label for={`proposal-${proposal.id}`}>Proposed text (+) · editable</label>
              <textarea
                id={`proposal-${proposal.id}`}
                aria-label={`Edit proposal: ${proposal.entry.key}`}
                value={drafts[proposal.id] ?? proposal.entry.content}
                oninput={(event) => {
                  drafts[proposal.id] = event.currentTarget.value;
                }}
                rows="3"
                maxlength="8000"
                disabled={busy}></textarea>
              <div class="review-actions">
                <button
                  class="primary"
                  disabled={busy || !(drafts[proposal.id] ?? proposal.entry.content).trim()}
                  onclick={() => review(proposal, true)}>Approve</button
                >
                <button class="secondary" disabled={busy} onclick={() => review(proposal, false)}
                  >Reject</button
                >
              </div>
            </article>
          {/each}
        </section>
      {/if}
      <form onsubmit={save} class="editor">
        <label for="memory-content">{editingId ? 'Edit memory' : 'Remember something'}</label
        ><textarea
          id="memory-content"
          bind:value={content}
          maxlength="8000"
          placeholder="For example: I prefer concise answers with practical examples."
          rows="3"
          disabled={busy || !loaded}></textarea>
        <div class="editor-actions">
            <span>Saved on this {platform.device}. Editable at any time.</span
            >{#if editingId}<button
              type="button"
              class="secondary"
              onclick={() => {
                editingId = '';
                editingEntry = undefined;
                content = '';
              }}>Cancel</button
            >{/if}<button class="primary" disabled={busy || !loaded || !content.trim()}
            >{busy ? 'Saving…' : editingId ? 'Save changes' : 'Save memory'}</button
          >
        </div>
      </form>
      <label for="memory-search" class="sr-only">Search memories</label><input
        id="memory-search"
        type="search"
        placeholder="Search your memories…"
        bind:value={query}
        oninput={refresh}
        disabled={!loaded}
      />
      {#if !loaded && !error}<p class="empty" role="status">
          Loading your memories…
        </p>{:else if entries.length === 0}<p class="empty">
          {query
            ? 'No memories match this search.'
            : 'No memories yet. Add a preference or useful detail above.'}
        </p>{:else}
        <div class="memories">
          {#each entries as entry (entry.id)}<article>
              <strong>{(entry.scope ?? 'user') + (entry.key ? ' · ' + entry.key : '')}</strong>
              {#if entry.workspace}<p class="hint">Project: {entry.workspace}</p>{/if}
              {#if entry.source}<p class="hint">
                  Source session: {entry.source.sessionId}<br />Message: {entry.source.messageId}
                </p>{/if}
              {#if entry.source}
                <button
                  class="secondary"
                  disabled={busy}
                  onclick={() => showSource(entry.id, entry.source!)}>View source message</button
                >
                {#if sourceMessages[entry.id]}<pre>{sourceMessages[entry.id]}</pre>{/if}
              {/if}
              <p>{entry.content}</p>
              <div>
                <button
                  class="secondary"
                  disabled={busy}
                  onclick={() => {
                    editingId = entry.id;
                    editingEntry = entry;
                    content = entry.content;
                  }}>Edit</button
                ><button
                  class="delete"
                  aria-label={`Delete memory: ${entry.content.slice(0, 40)}`}
                  disabled={busy}
                  onclick={() => remove(entry.id)}><Icon name="trash" size={14} />Forget</button
                >
              </div>
            </article>{/each}
        </div>
      {/if}
    {/if}
    {/if}
    {#if section === 'limits'}<details open>
      <summary>Advanced · Context window</summary><label for="context-window"
        >Model context limit (tokens)</label
      ><input
        id="context-window"
        type="number"
        min="2048"
        max="1000000"
        step="1024"
        bind:value={contextWindow}
        disabled={busy || !loaded}
      />
      <p class="hint">
          Use the limit configured in your model service. Requests reserve room for tools, output,
          and a 5% safety margin. Token counts are estimates.
      </p>
      <label for="output-tokens">Output reserve (tokens)</label>
      <input
        id="output-tokens"
        type="number"
        min="1"
        max="32768"
        bind:value={outputTokens}
        disabled={busy || !loaded}
      />
      <label
        ><input type="checkbox" bind:checked={autoCompact} disabled={busy || !loaded} /> Automatically
        compact older context near the limit</label
      >
      <p class="hint">
        Use /context to see usage and /compact to summarize older turns. Original transcripts are
        retained.
      </p>
      <label for="max-iterations">Agent iteration limit</label>
      <input
        id="max-iterations"
        type="number"
        min="1"
        max="100"
        bind:value={maxIterations}
        disabled={busy || !loaded}
      />
      <label for="command-timeout">Command time limit (seconds)</label>
      <input
        id="command-timeout"
        type="number"
        min="10"
        max="3600"
        bind:value={commandTimeoutSeconds}
        disabled={busy || !loaded}
      />
      <label for="output-cap">Command output cap (bytes)</label>
      <input
        id="output-cap"
        type="number"
        min="4096"
        max="1048576"
        step="4096"
        bind:value={maxOutputBytes}
        disabled={busy || !loaded}
      />
      <p class="hint">
        Longer command output keeps its first 40% and last 60%. File reads stay limited to 64 KiB.
      </p>
      <button
        class="secondary"
        disabled={busy ||
          !loaded ||
          contextWindow < 2048 ||
          contextWindow > 1000000 ||
          outputTokens < 1 ||
          outputTokens > 32768 ||
          Number(outputTokens) + Math.floor(contextWindow / 20) >= contextWindow ||
          !Number.isInteger(Number(maxIterations)) ||
          maxIterations < 1 ||
          maxIterations > 100 ||
          !Number.isInteger(Number(commandTimeoutSeconds)) ||
          commandTimeoutSeconds < 10 ||
          commandTimeoutSeconds > 3600 ||
          !Number.isInteger(Number(maxOutputBytes)) ||
          maxOutputBytes < 4096 ||
          maxOutputBytes > 1048576}
        onclick={saveSettings}>Save context limit</button
      >
      </details>{/if}
    {#if desktop && section === 'subagents'}
      <details open>
        <summary>Advanced · Subagents</summary>
        <label for="subagent-steps">Subagent step limit</label>
        <input
          id="subagent-steps"
          type="number"
          min="1"
          max="50"
          bind:value={subagentMaxSteps}
          disabled={busy || !loaded}
        />
        <fieldset disabled={busy || !loaded}>
          <legend>Subagents run on</legend>
          <label
            ><input type="radio" bind:group={subagentRunsOn} value="local" /> This model, with the tools
            below</label
          >
          <label
            ><input type="radio" bind:group={subagentRunsOn} value="tool" /> A command tool</label
          >
        </fieldset>
        {#if subagentRunsOn === 'local'}
          <fieldset disabled={busy || !loaded}>
            <legend>Subagent tools</legend>
            {#each SUBAGENT_TOOLS as tool (tool)}
              <label
                ><input
                  type="checkbox"
                  checked={subagentTools.includes(tool)}
                  onchange={(event) => toggleTool(tool, event.currentTarget.checked)}
                />
                {toolLabels[tool]}</label
              >
            {/each}
          </fieldset>
        {:else}
          <label for="subagent-tool">Command tool name</label>
          <input
            id="subagent-tool"
            bind:value={subagentTool}
            placeholder="review"
            autocomplete="off"
            disabled={busy || !loaded}
          />
          <p class="off-device" role="note">
            Sends data off this device. The subagent's whole prompt goes to this tool as JSON on
            standard input, and you are asked every time. The tool needs "stdin": "json" and a
            "prompt" parameter in tools.json.
          </p>
        {/if}
        <fieldset disabled={busy || !loaded}>
          <legend>Command tools</legend>
          <label
            ><input type="radio" bind:group={commandToolsIsolation} value="outside" /> Outside the sandbox
            (asks every time)</label
          >
          <label
            ><input type="radio" bind:group={commandToolsIsolation} value="sandboxed" /> Inside the Commands
            profile</label
          >
        </fieldset>
        {#if commandToolsIsolation === 'outside'}
          <p class="hint">Command tools run with your account permissions and always ask first.</p>
        {/if}
        <p class="hint">Projects can lower these limits but never choose where subagents run.</p>
        <button
          class="secondary"
          disabled={busy || !loaded || !subagentsValid}
          onclick={saveSubagents}>Save subagent settings</button
        >
      </details>
    {/if}
    {#if desktop && section === 'instructions'}
      <details open>
        <summary>Advanced · Instructions</summary>
        <label for="instruction-file-kib">Instruction file limit (KiB)</label>
        <input
          id="instruction-file-kib"
          type="number"
          min="1"
          max="64"
          bind:value={instructionMaxFileKib}
          disabled={busy || !loaded}
        />
        <label for="instruction-total-kib">Total instruction limit (KiB)</label>
        <input
          id="instruction-total-kib"
          type="number"
          min="8"
          max="256"
          bind:value={instructionMaxTotalKib}
          disabled={busy || !loaded}
        />
        <label for="instruction-files">Extra instruction files, one per line</label>
        <textarea
          id="instruction-files"
          rows="3"
          bind:value={instructionFiles}
          placeholder="CLAUDE.md"
          autocomplete="off"
          spellcheck="false"
          disabled={busy || !loaded}></textarea>
        <p class="hint">
          Read after AGENTS.md in each folder, up to 8 paths inside the project, such as CLAUDE.md
          or .github/copilot-instructions.md. A longer file is cut at the limit with a warning.
          Projects can lower these limits but never add files.
        </p>
        <fieldset disabled={busy || !loaded}>
          <legend>Max skill size</legend>
          <label
            ><input type="radio" bind:group={maxSkillSizeMode} value="auto" /> Auto (10% of the configured
            context window, 32–256 KiB)</label
          >
          <label><input type="radio" bind:group={maxSkillSizeMode} value="manual" /> Manual</label>
        </fieldset>
        {#if maxSkillSizeMode === 'manual'}
          <label for="max-skill-size-kib">Max skill size (KiB)</label>
          <input
            id="max-skill-size-kib"
            type="number"
            min="32"
            max="256"
            bind:value={maxSkillSizeKib}
            disabled={busy || !loaded}
          />
        {/if}
        <p class="hint">
          Auto uses the same configured-window limit for importing and every chat path. Installed
          skills over the current limit stay available to remove but are skipped during chats.
        </p>
        <button
          class="secondary"
          disabled={busy || !loaded || !instructionsValid}
          onclick={saveInstructions}>Save instruction limits</button
        >
      </details>
    {/if}
    {#if error}<p role="alert" class="error">{error}</p>{/if}
  </div>
</dialog>

<style>
  fieldset {
    border: 0;
    margin: 8px 0;
    padding: 0;
  }
  legend {
    font-size: 12px;
    margin-bottom: 4px;
  }
  .off-device {
    font-weight: 600;
    color: var(--warn);
  }
  .proposals {
    margin-bottom: 24px;
  }
  h3 {
    font-size: 14px;
    margin: 0 0 8px;
  }
  pre {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    font-size: 12px;
    background: var(--bg-base);
    padding: 12px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
  }
  .proposals label {
    margin-top: 12px;
  }
  .review-actions {
    margin-top: 10px;
  }
  dialog {
    width: min(620px, calc(100vw - 32px));
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
    display: block;
    width: 100%;
    max-height: none;
    margin: 0;
    border: 0;
    border-radius: 0;
    background: transparent;
    box-shadow: none;
  }
  dialog.embedded .content {
    padding: 0;
  }
  dialog.embedded h2 {
    margin-bottom: 20px;
  }
  dialog.embedded details {
    border-top: 0;
    padding-top: 0;
  }
  dialog.embedded summary {
    display: none;
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
  header p {
    margin: 0;
    font-size: 12px;
    color: var(--text-muted);
  }
  .icon-button {
    background: transparent;
    color: var(--text-muted);
    padding: 4px;
  }
  .content {
    padding: 24px;
  }
  .preference {
    display: flex;
    align-items: center;
    gap: 20px;
    margin-bottom: 24px;
  }
  .preference div {
    flex: 1;
  }
  strong {
    font-size: 13px;
    font-weight: 580;
  }
  .preference p,
  .hint {
    color: var(--text-muted);
    font-size: 12px;
    line-height: 1.6;
    margin: 5px 0 0;
  }
  input[type='checkbox'] {
    accent-color: var(--accent);
    width: 17px;
    height: 17px;
    vertical-align: middle;
  }
  input[type='radio'] {
    vertical-align: middle;
  }
  label {
    display: block;
    font-size: 12px;
    font-weight: 550;
    margin-bottom: 9px;
  }
  textarea,
  input:not([type='checkbox']):not([type='radio']) {
    width: 100%;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-sm);
    background: var(--bg-base);
    padding: 11px 12px;
    font-size: 12px;
  }
  textarea {
    resize: vertical;
    min-height: 85px;
    max-height: 220px;
  }
  .editor-actions {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 10px 0 26px;
  }
  .editor-actions span {
    flex: 1;
    color: var(--text-muted);
    font-size: 10.5px;
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
    display: flex;
    align-items: center;
    gap: 6px;
    color: var(--text-muted);
    background: transparent;
  }
  .delete:hover {
    color: var(--err);
  }
  button:disabled {
    opacity: 0.5;
  }
  .empty {
    font-size: 12px;
    color: var(--text-muted);
    padding: 22px 0;
    text-align: center;
  }
  .memories {
    margin: 15px 0 24px;
  }
  article {
    padding: 15px 0;
    border-bottom: 1px solid var(--border);
  }
  article p {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    font-size: 13px;
    margin: 0 0 12px;
  }
  article > div {
    display: flex;
    gap: 5px;
  }
  details {
    border-top: 1px solid var(--border);
    padding-top: 18px;
  }
  summary {
    cursor: pointer;
    color: var(--text-muted);
    font-size: 12px;
    margin-bottom: 16px;
  }
  .hint {
    margin: 8px 0 12px;
  }
  .error {
    color: var(--err);
    font-size: 12px;
    line-height: 1.6;
    margin-top: 18px;
  }
</style>
