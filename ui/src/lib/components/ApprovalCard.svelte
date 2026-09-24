<script lang="ts">
  import type { PendingApproval } from '../types';
  export let approval: PendingApproval;
  export let onResolve: (decision: 'allow' | 'always_allow' | 'deny') => Promise<void>;
  let busy = false;
  let error = '';
  async function decide(decision: 'allow' | 'always_allow' | 'deny') {
    if (busy) return;
    busy = true;
    error = '';
    try {
      await onResolve(decision);
    } catch {
      error = 'This decision could not be sent. Try again, or stop the task.';
    } finally {
      busy = false;
    }
  }
</script>

<section class="approval" aria-label="Action needs your approval">
  <div class="title">
    <span></span><strong
      >{approval.kind === 'file'
        ? 'Review this file change'
        : approval.kind === 'network'
          ? 'Allow this web request?'
          : 'Allow this command?'}</strong
    ><small>Waiting for you</small>
  </div>
  <!-- Scrollable action details need a keyboard focus target. -->
  <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
  <div class="action-details" role="region" aria-label="Proposed action details" tabindex="0">
    <pre>{approval.detail}</pre>
  </div>
  <div class="actions">
    <button class="deny" disabled={busy} onclick={() => decide('deny')}>Deny</button><span
    ></span><button class="remember" disabled={busy} onclick={() => decide('always_allow')}
      >Allow identical action this run</button
    ><button class="allow" disabled={busy} onclick={() => decide('allow')}
      >{busy ? 'Sending…' : 'Allow once'}</button
    >
  </div>
  {#if error}<p role="alert">{error}</p>{/if}
</section>

<style>
  .approval {
    width: min(calc(100% - 36px), var(--composer-width));
    margin: 12px auto;
    padding: 16px;
    border: 1px solid var(--warn);
    border-radius: var(--radius-md);
    background: var(--bg-panel);
  }
  .title {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 12px;
  }
  .title > span {
    width: 6px;
    height: 6px;
    background: var(--warn);
    border-radius: 50%;
  }
  strong {
    font-size: 12.5px;
    font-weight: 600;
    flex: 1;
  }
  small {
    font-size: 10.5px;
    color: var(--text-muted);
  }
  .action-details {
    max-height: 240px;
    overflow: auto;
    margin: 0 0 14px;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    background: var(--bg-base);
    padding: 12px;
    border-radius: var(--radius-sm);
    font: 11.5px/1.6 var(--font-code);
  }
  pre {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    margin: 0;
    font: inherit;
  }
  .actions {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }
  .actions > span {
    flex: 1;
  }
  button {
    padding: 8px 10px;
    border-radius: var(--radius-sm);
    font-size: 11px;
  }
  button:disabled {
    opacity: 0.5;
  }
  .allow {
    background: var(--accent);
    color: var(--text-on-accent);
    font-weight: 600;
  }
  .deny,
  .remember {
    background: var(--bg-elevated);
    border: 1px solid var(--border-strong);
  }
  p {
    color: var(--err);
    font-size: 12px;
    margin-bottom: 0;
  }
  @media (max-width: 560px) {
    .actions {
      display: grid;
      grid-template-columns: 1fr;
    }
    .actions > span {
      display: none;
    }
    button {
      min-height: 38px;
    }
    .title {
      flex-wrap: wrap;
    }
  }
  @media (max-height: 650px) {
    .action-details {
      max-height: 130px;
    }
  }
</style>
