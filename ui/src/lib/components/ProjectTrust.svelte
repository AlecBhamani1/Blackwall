<script lang="ts">
  import { tick } from 'svelte';
  import { needsReview, projectClient, type ProjectStatus } from '../project';
  import Icon from './Icon.svelte';

  export let workspace: string;
  /** Changes when the status should be read again, such as after a run. */
  export let refresh: unknown = undefined;
  export let client = projectClient;

  let status: ProjectStatus | null = null;
  let dialog: HTMLDialogElement | undefined;
  let open = false;
  let busy = false;
  let error = '';
  /** The file hash the owner chose "Not now" for; the banner returns when the file changes. */
  let dismissed: string | null = null;
  let generation = 0;

  function message(cause: unknown, fallback: string) {
    return cause instanceof Error ? cause.message : typeof cause === 'string' ? cause : fallback;
  }
  async function load(folder: string) {
    const current = ++generation;
    try {
      const next = await client.status(folder);
      if (current === generation) status = next;
    } catch {
      if (current === generation) status = null;
    }
  }
  $: void refresh, load(workspace);
  $: shown = needsReview(status) && (status?.sha256 ?? status?.error ?? '') !== dismissed;

  async function review() {
    error = '';
    open = true;
    await tick();
    dialog?.showModal();
  }
  function notNow() {
    dismissed = status?.sha256 ?? status?.error ?? null;
    dialog?.close();
  }
  async function trust() {
    if (!status?.sha256) return;
    busy = true;
    error = '';
    try {
      status = await client.trust(workspace, status.sha256);
      dialog?.close();
    } catch (cause) {
      error = message(cause, 'The project could not be trusted.');
      // The file may have changed; show what is there now.
      await load(workspace);
    } finally {
      busy = false;
    }
  }
</script>

{#if shown && status}
  <div class="project-trust" role="status">
    <Icon name="shield" />
    <span>
      {#if status.error}This project's settings file cannot be used, so every action asks.
      {:else if status.trust.state === 'changed'}This project's settings changed since you
        trusted them. Only their deny and ask rules apply.
      {:else}This project has settings that are not trusted. Only their deny and ask rules apply.{/if}
    </span>
    <button onclick={review}>Review project settings</button>
  </div>
{/if}

{#if open && status}
  <dialog bind:this={dialog} onclose={() => (open = false)} aria-labelledby="project-trust-title">
    <header>
      <div>
        <h2 id="project-trust-title">Trust this project's settings?</h2>
        <p class="path">{status.path}</p>
      </div>
      <button class="icon-button" aria-label="Close project settings" onclick={notNow}
        ><Icon name="x" /></button
      >
    </header>
    <div class="content">
      {#if status.error}<p class="error" role="alert">{status.error}</p>{/if}
      <p class="hint">
        A project file comes with the folder, so anyone who can change the folder can change it.
        Trusting applies its allow rules and other settings. Any later edit makes it untrusted
        again.
      </p>
      <dl>
        <dt>Applies now</dt>
        <dd>{status.applies.length ? status.applies.join('; ') : 'Nothing'}</dd>
        {#if status.waiting.length}<dt>Applies after you trust it</dt>
          <dd>{status.waiting.join('; ')}</dd>{/if}
      </dl>
      {#if status.trust.state === 'changed'}
        <h3>Changes since you trusted it</h3>
        <pre class="diff" aria-label="Changes since you trusted it">{#each status.trust.diff.split('\n') as line}<span
              class:added={line.startsWith('+') && !line.startsWith('+++')}
              class:removed={line.startsWith('-') && !line.startsWith('---')}>{line}</span
            >{'\n'}{/each}</pre>
        <details>
          <summary>Whole file</summary>
          <pre>{status.content}</pre>
        </details>
      {:else if status.content !== null}
        <h3>File</h3>
        <pre aria-label="Project settings file">{status.content}</pre>
      {/if}
      {#if error}<p class="error" role="alert">{error}</p>{/if}
      <div class="actions">
        <span></span>
        <button class="secondary" onclick={notNow} disabled={busy}>Not now</button>
        {#if status.sha256 && !status.error}<button
            class="primary"
            onclick={trust}
            disabled={busy || status.trust.state === 'trusted'}>Trust</button
          >{/if}
      </div>
    </div>
  </dialog>
{/if}

<style>
  .project-trust {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
    padding: 12px 20px;
    border-bottom: 1px solid var(--warn);
    background: var(--bg-panel);
    color: var(--text-primary);
    font-size: 12px;
  }
  .project-trust span {
    flex: 1;
  }
  .project-trust button {
    flex: 0 0 auto;
    padding: 6px 10px;
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    background: var(--bg-elevated);
  }
  dialog {
    width: min(720px, calc(100vw - 32px));
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
    margin: 18px 0 6px;
    font-size: 13px;
    font-weight: 570;
  }
  .path,
  .hint {
    margin: 0;
    font-size: 12px;
    line-height: 1.6;
    color: var(--text-muted);
    overflow-wrap: anywhere;
  }
  .content {
    padding: 24px;
  }
  dl {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 6px 14px;
    margin: 16px 0 0;
    font-size: 12px;
  }
  dt {
    color: var(--text-muted);
  }
  dd {
    margin: 0;
  }
  pre {
    max-height: 320px;
    overflow: auto;
    margin: 0;
    padding: 12px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--bg-elevated);
    font-size: 11.5px;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .added {
    color: var(--ok);
  }
  .removed {
    color: var(--err);
  }
  details {
    margin-top: 10px;
    font-size: 12px;
  }
  .error {
    color: var(--err);
    font-size: 12px;
  }
  .icon-button {
    background: transparent;
    color: var(--text-muted);
    padding: 4px;
  }
  .actions {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 18px;
  }
  .actions > span {
    flex: 1;
  }
  .primary,
  .secondary {
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
</style>
