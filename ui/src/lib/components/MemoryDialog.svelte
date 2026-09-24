<script lang="ts">
  import { onMount } from 'svelte';
  import { persistence, type MemoryEntry } from '../persistence';
  import { createId } from '../id';
  import Icon from './Icon.svelte';
  export let onPreferences: (preferences: {
    memoryEnabled: boolean;
    contextWindow: number;
  }) => void = () => {};
  export let onClose: () => void;
  let dialog: HTMLDialogElement;
  let entries: MemoryEntry[] = [];
  let content = '';
  let editingId = '';
  let query = '';
  let error = '';
  let busy = false;
  let enabled = false;
  let contextWindow = 32000;
  let loaded = false;
  let savedPreferences: Awaited<ReturnType<typeof persistence.preferences>> = {};
  const desktop = persistence.available();
  let searchVersion = 0;
  async function refresh() {
    const version = ++searchVersion;
    try {
      const result = await persistence.memories(query);
      if (version === searchVersion) entries = result;
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
        id: editingId || createId('memory'),
        content: content.trim(),
        updatedAt: Date.now(),
      });
      content = '';
      editingId = '';
      await refresh();
    } catch {
      error = 'This memory could not be saved. Check disk space and try again.';
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
        content = '';
      }
      await refresh();
    } catch {
      error = 'This memory could not be deleted. Try again.';
    } finally {
      busy = false;
    }
  }
  async function saveSettings() {
    busy = true;
    error = '';
    try {
      await persistence.savePreferences({
        memoryEnabled: enabled,
        contextWindow: Number(contextWindow),
      });
      onPreferences({ memoryEnabled: enabled, contextWindow: Number(contextWindow) });
    } catch {
      error = 'These settings could not be saved. Try again.';
    } finally {
      busy = false;
    }
  }
  onMount(() => {
    dialog.showModal();
    if (!desktop) return;
    void (async () => {
      try {
        savedPreferences = await persistence.preferences();
        enabled = savedPreferences.memoryEnabled ?? false;
        contextWindow = savedPreferences.contextWindow ?? 32000;
        await refresh();
        loaded = true;
      } catch {
        error = 'Your memory settings could not be opened.';
      }
    })();
  });
</script>

<dialog bind:this={dialog} onclose={onClose} aria-labelledby="memory-title">
  <header>
    <div>
      <h2 id="memory-title">Memory</h2>
      <p>A little context, carried into your next conversation.</p>
    </div>
    <button class="icon-button" aria-label="Close memory" onclick={() => dialog.close()}
      ><Icon name="x" /></button
    >
  </header>
  <div class="content">
    {#if !desktop}<p>Memory is available in the installed Blackwall desktop app.</p>
    {:else}
      <div class="preference">
        <div>
          <strong>Use saved memories in chat</strong>
          <p>Only memories you save here are included. Shared guest chats never receive them.</p>
        </div>
        <input
          aria-label="Use saved memories in chat"
          type="checkbox"
          bind:checked={enabled}
          disabled={busy || !loaded}
          onchange={saveSettings}
        />
      </div>
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
          <span>Saved on this Mac. Editable at any time.</span>{#if editingId}<button
              type="button"
              class="secondary"
              onclick={() => {
                editingId = '';
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
              <p>{entry.content}</p>
              <div>
                <button
                  class="secondary"
                  disabled={busy}
                  onclick={() => {
                    editingId = entry.id;
                    content = entry.content;
                  }}>Edit</button
                ><button
                  class="delete"
                  aria-label={`Delete memory: ${entry.content.slice(0, 40)}`}
                  disabled={busy}
                  onclick={() => remove(entry.id)}><Icon name="trash" size={14} />Delete</button
                >
              </div>
            </article>{/each}
        </div>
      {/if}
      <details>
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
          Use the limit configured in your model service. The status bar shows an estimate.
        </p>
        <button
          class="secondary"
          disabled={busy || !loaded || contextWindow < 2048 || contextWindow > 1000000}
          onclick={saveSettings}>Save context limit</button
        >
      </details>
    {/if}
    {#if error}<p role="alert" class="error">{error}</p>{/if}
  </div>
</dialog>

<style>
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
  }
  label {
    display: block;
    font-size: 12px;
    font-weight: 550;
    margin-bottom: 9px;
  }
  textarea,
  input:not([type='checkbox']) {
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
