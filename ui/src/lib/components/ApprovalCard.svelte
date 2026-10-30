<script lang="ts">
  import type { ApprovalDecision, PendingApproval } from '../types';
  export let approval: PendingApproval;
  export let onResolve: (decision: ApprovalDecision) => Promise<void>;
  let busy = false;
  let error = '';
  // MCP approvals can be saved as an owner rule for one tool or the whole server.
  let scope: 'tool' | 'server' = 'tool';
  let place: 'project' | 'everywhere' = 'project';
  // A new approval starts from the narrowest choice, even when the card is reused.
  let shown = '';
  $: if (approval.approvalId !== shown) {
    shown = approval.approvalId;
    scope = 'tool';
    place = 'project';
  }
  const OUTSIDE_NOTE = 'Runs outside the sandbox with your account permissions.';
  // The label already says it, so the details do not repeat it; the event keeps it for others.
  $: shownDetail =
    approval.outsideSandbox && approval.detail.startsWith(OUTSIDE_NOTE)
      ? approval.detail.slice(OUTSIDE_NOTE.length).replace(/^\n+/, '')
      : approval.detail;
  $: toolName = approval.tool?.tool.slice(approval.tool.server.length + 2) ?? '';
  $: saved = (
    place === 'project' ? `allow_${scope}_in_project` : `allow_${scope}`
  ) as ApprovalDecision;
  async function decide(decision: ApprovalDecision) {
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
          : approval.tool
            ? 'Allow this tool?'
            : 'Allow this command?'}</strong
    ><small>Waiting for you</small>
  </div>
  {#if approval.offDevice}<p class="off-device">
      Sends data off this device. The program below may pass this prompt to an online service.
      Blackwall asks every time.
    </p>{/if}
  {#if approval.outsideSandbox}<p class="outside">{OUTSIDE_NOTE}</p>{/if}
  <!-- Scrollable action details need a keyboard focus target. -->
  <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
  <div class="action-details" role="region" aria-label="Proposed action details" tabindex="0">
    <pre>{shownDetail}</pre>
  </div>
  <div class="actions">
    <button class="deny" disabled={busy} onclick={() => decide('deny')}>Deny</button><span
    ></span>{#if approval.remember !== false && !approval.outsideSandbox}<button
        class="remember"
        disabled={busy}
        onclick={() => decide('always_allow')}>Allow identical action this run</button
      >{/if}<button class="allow" disabled={busy} onclick={() => decide('allow')}
      >{busy ? 'Sending…' : 'Allow once'}</button
    >
  </div>
  {#if approval.tool && !approval.outsideSandbox}
    <div class="always">
      <label
        >Always allow <select bind:value={scope} disabled={busy} aria-label="What to allow"
          ><option value="tool">{toolName}</option><option value="server"
            >every {approval.tool.server} tool</option
          ></select
        ></label
      ><label
        ><select bind:value={place} disabled={busy} aria-label="Where to allow it"
          ><option value="project">in this project</option><option value="everywhere"
            >in every project</option
          ></select
        ></label
      ><button class="remember" disabled={busy} onclick={() => decide(saved)}>Always allow</button>
    </div>
    <small class="note">Saved as an owner rule. Remove it in MCP servers.</small>
  {/if}
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
  .always {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
    margin-top: 12px;
    padding-top: 12px;
    border-top: 1px solid var(--border);
    font-size: 11.5px;
  }
  .always label {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  select {
    padding: 6px 8px;
    border-radius: var(--radius-sm);
    border: 1px solid var(--border-strong);
    background: var(--bg-base);
    font: inherit;
  }
  .note {
    display: block;
    margin-top: 6px;
  }
  .off-device,
  .outside {
    margin: 0 0 8px;
    font-weight: 600;
    color: var(--warn);
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
    .always,
    .always label {
      display: grid;
      grid-template-columns: 1fr;
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
