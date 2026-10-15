<script lang="ts">
  import type { RecallQuery, RecallResults } from '../persistence';
  export let initialQuery = '';
  export let enabled = false;
  export let search: (query: RecallQuery) => Promise<RecallResults>;
  export let onEnabled: (enabled: boolean) => Promise<unknown> | void;
  export let onOpen: (id: string, index: number) => Promise<void>;
  export let onClose: () => void;
  let query = initialQuery;
  $: query = initialQuery;
  let project = '';
  let after = '';
  let before = '';
  let results: RecallResults | null = null;
  let error = '';
  let busy = false;
  async function find(event: SubmitEvent) {
    event.preventDefault();
    busy = true;
    error = '';
    results = null;
    try {
      results = await search({
        query,
        project: project || undefined,
        after: after ? Date.parse(`${after}T00:00:00Z`) : undefined,
        before: before ? Date.parse(`${before}T23:59:59.999Z`) : undefined,
        limit: 10,
      });
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause);
    } finally {
      busy = false;
    }
  }
  async function toggle(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    try {
      await onEnabled(input.checked);
    } catch (cause) {
      input.checked = enabled;
      error = String(cause);
    }
  }
</script>

<section class="recall" aria-label="Search saved conversations">
  <div class="heading">
    <strong>Search saved conversations</strong><button onclick={onClose}>Close</button>
  </div>
  <form onsubmit={find}>
    <label
      >Words<input
        bind:value={query}
        required
        maxlength="512"
        placeholder="Search message text"
      /></label
    >
    <label>Project<input bind:value={project} maxlength="2048" placeholder="Any project" /></label>
    <label>Updated after (UTC)<input type="date" bind:value={after} /></label>
    <label>Updated before (UTC)<input type="date" bind:value={before} /></label>
    <button type="submit" disabled={busy}>{busy ? 'Searching…' : 'Search'}</button>
  </form>
  <label class="toggle"
    ><input type="checkbox" checked={enabled} onchange={toggle} />Allow the agent to recall this
    project's messages</label
  >
  <p class="hint">
    Saved text is history. Opening a conversation preserves its mode; past tools and approvals are
    never replayed.
  </p>
  {#if error}<p role="alert">{error}</p>{/if}
  {#if results}
    {#if results.remainingSessions}<p role="status">
        {results.remainingSessions} older conversations still need indexing. Search again to continue.
      </p>{/if}
    {#if !results.results.length}<p role="status">No matching saved messages.</p>{/if}
    <ul>
      {#each results.results as hit}
        <li>
          <button onclick={() => onOpen(hit.sessionId, hit.messageIndex)}
            >{hit.title} · {hit.mode} · message {hit.messageIndex + 1}</button
          >
          <small
            >{new Date(hit.updatedAt).toLocaleDateString()} · {hit.project || 'No project'} · {hit.role}</small
          >
          <p>{hit.excerpt}</p>
          <small>{hit.sessionId} / {hit.messageId}</small>
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .recall {
    padding: 18px;
    border-bottom: 1px solid var(--border);
    background: var(--bg-panel);
    max-height: 60vh;
    overflow: auto;
  }
  .heading,
  form {
    display: flex;
    gap: 12px;
    align-items: center;
    flex-wrap: wrap;
  }
  .heading {
    justify-content: space-between;
    margin-bottom: 12px;
  }
  label {
    display: flex;
    flex-direction: column;
    gap: 6px;
    font-size: 12px;
  }
  input,
  button {
    padding: 8px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--bg-base);
    color: var(--text-primary);
  }
  button {
    cursor: pointer;
  }
  .toggle {
    flex-direction: row;
    align-items: center;
    margin-top: 12px;
  }
  .hint,
  small {
    font-size: 11px;
    color: var(--text-muted);
  }
  small {
    display: block;
    margin-top: 6px;
    overflow-wrap: anywhere;
  }
  ul {
    list-style: none;
    padding: 0;
  }
  li {
    border-top: 1px solid var(--border);
    padding: 12px 0;
  }
  li p {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
</style>
