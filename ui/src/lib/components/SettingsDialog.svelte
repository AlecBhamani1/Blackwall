<script lang="ts">
  import { onDestroy, tick } from 'svelte';
  import { guestShareClient, type GuestShareClient } from '../ipc';
  import type { ShareExpiryMinutes, ShareStatus } from '../types';
  import Icon from './Icon.svelte';

  export let open = false;
  export let model = '';
  export let onClose: () => void;
  export let client: GuestShareClient = guestShareClient;

  const expiryOptions: Array<{ value: ShareExpiryMinutes; label: string }> = [
    { value: 15, label: '15 min' },
    { value: 60, label: '1 hour' },
    { value: 480, label: '8 hours' },
  ];

  let expiry: ShareExpiryMinutes = 60;
  let status: ShareStatus = { active: false };
  let busy: 'loading' | 'starting' | 'stopping' | '' = '';
  let error = '';
  let copied = false;
  let dialog: HTMLElement;
  let previouslyOpen = false;
  let pollTimer: number | undefined;
  let operationVersion = 0;

  function errorMessage(value: unknown): string {
    return value instanceof Error ? value.message : 'Blackwall could not update guest sharing.';
  }

  function mergeStatus(next: ShareStatus): void {
    status = next.active
      ? {
          ...status,
          ...next,
          shareUrl: next.shareUrl ?? status.shareUrl,
          qrDataUrl: next.qrDataUrl ?? status.qrDataUrl,
        }
      : { active: false };
  }

  async function refresh(silent = false): Promise<void> {
    const version = operationVersion;
    if (!silent) busy = 'loading';
    try {
      const next = await client.shareStatus();
      if (version !== operationVersion) return;
      mergeStatus(next);
      error = '';
    } catch (cause) {
      if (!silent) error = errorMessage(cause);
    } finally {
      if (!silent) busy = '';
    }
  }

  async function start(): Promise<void> {
    if (!model || busy) return;
    operationVersion += 1;
    busy = 'starting';
    error = '';
    copied = false;
    try {
      mergeStatus(await client.startShare({ model, expiresInMinutes: expiry }));
    } catch (cause) {
      error = errorMessage(cause);
    } finally {
      busy = '';
    }
  }

  async function stop(): Promise<void> {
    if (busy) return;
    operationVersion += 1;
    busy = 'stopping';
    error = '';
    try {
      mergeStatus(await client.stopShare());
      copied = false;
    } catch (cause) {
      error = errorMessage(cause);
    } finally {
      busy = '';
    }
  }

  async function copyLink(): Promise<void> {
    if (!status.shareUrl) return;
    try {
      await navigator.clipboard.writeText(status.shareUrl);
      copied = true;
      window.setTimeout(() => (copied = false), 2_000);
    } catch {
      error = 'The link could not be copied. Select it and copy it manually.';
    }
  }

  function expiresLabel(value?: number): string {
    if (!value) return 'Temporary access';
    const parsed = new Date(value);
    if (Number.isNaN(parsed.getTime())) return 'Temporary access';
    return `Expires ${new Intl.DateTimeFormat(undefined, {
      hour: 'numeric',
      minute: '2-digit',
    }).format(parsed)}`;
  }

  function handleKeydown(event: KeyboardEvent): void {
    if (open && event.key === 'Escape') {
      event.preventDefault();
      onClose();
    }
  }

  function trapFocus(event: KeyboardEvent): void {
    if (event.key !== 'Tab') return;
    const focusable = Array.from(
      dialog.querySelectorAll<HTMLElement>(
        'button:not(:disabled), input:not(:disabled), [href], [tabindex]:not([tabindex="-1"])',
      ),
    );
    const first = focusable[0];
    const last = focusable.at(-1);
    if (!first || !last) {
      event.preventDefault();
      return;
    }

    if (event.shiftKey && (document.activeElement === first || document.activeElement === dialog)) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && (document.activeElement === last || document.activeElement === dialog)) {
      event.preventDefault();
      first.focus();
    }
  }

  $: if (open !== previouslyOpen) {
    previouslyOpen = open;
    if (open) {
      copied = false;
      void refresh();
      void tick().then(() => dialog?.focus());
      pollTimer = window.setInterval(() => void refresh(true), 10_000);
    } else if (pollTimer !== undefined) {
      window.clearInterval(pollTimer);
      pollTimer = undefined;
    }
  }

  onDestroy(() => {
    if (pollTimer !== undefined) window.clearInterval(pollTimer);
  });
</script>

<svelte:window onkeydown={handleKeydown} />

{#if open}
  <div class="modal-layer">
    <button class="backdrop" aria-label="Close settings" onclick={onClose}></button>
    <div
      class="settings-dialog"
      role="dialog"
      aria-modal="true"
      aria-labelledby="settings-title"
      tabindex="-1"
      bind:this={dialog}
      onkeydown={trapFocus}
    >
      <header>
        <div>
          <h2 id="settings-title">Settings</h2>
          <p>Manage guest access to your model.</p>
        </div>
        <button class="icon-button" aria-label="Close settings" onclick={onClose}>
          <Icon name="x" size={18} />
        </button>
      </header>

      <div class="dialog-content">
        <section class="share-section" aria-labelledby="share-heading">
          <div class="section-heading">
            <div>
              <h3 id="share-heading">Share your model</h3>
              <p>Create a temporary browser link for someone you trust.</p>
            </div>
            {#if status.active}
              <span class="live-badge"><span></span>Live</span>
            {/if}
          </div>

          {#if busy === 'loading'}
            <div class="loading-row" role="status">
              <span class="spinner"></span>
              Checking guest access…
            </div>
          {:else if status.active}
            <div class="active-share">
              <div class="share-summary">
                <div>
                  <span class="summary-label">Sharing</span>
                  <strong>{status.model || model}</strong>
                </div>
                <div>
                  <span class="summary-label">Access</span>
                  <strong>{status.networkLabel || 'Private network'}</strong>
                </div>
              </div>

              {#if status.qrDataUrl && status.shareUrl}
                <div class="qr-wrap">
                  <img src={status.qrDataUrl} alt="QR code for guest chat link" />
                  <div>
                    <strong>Scan to open</strong>
                    <p>Guests can scan this code with their phone camera.</p>
                  </div>
                </div>

                <label class="link-label" for="guest-link">Guest link</label>
                <div class="link-field">
                  <input id="guest-link" readonly value={status.shareUrl} onclick={(event) => event.currentTarget.select()} />
                  <button class:copied aria-label="Copy guest link" onclick={copyLink}>
                    <Icon name={copied ? 'check' : 'copy'} size={15} />
                    <span aria-live="polite">{copied ? 'Copied' : 'Copy'}</span>
                  </button>
                </div>
              {:else}
                <div class="notice warning">
                  This share is active, but its link and QR code are no longer available here. Stop sharing and create a new link to display them again.
                </div>
              {/if}

              <div class="share-meta">
                <span>{expiresLabel(status.expiresAt)}</span>
                <span>{status.requestCount ?? 0} {(status.requestCount ?? 0) === 1 ? 'request' : 'requests'}</span>
              </div>

              <div class="notice warning">
                Anyone with this link can send prompts and files to your model until it expires or you stop sharing.
              </div>

              <button class="stop-button" disabled={busy === 'stopping'} onclick={stop}>
                {busy === 'stopping' ? 'Stopping…' : 'Stop sharing'}
              </button>
            </div>
          {:else}
            <div class="inactive-share">
              <div class="model-row">
                <span>Model</span>
                <strong>{model || 'No model selected'}</strong>
              </div>

              <fieldset disabled={busy === 'starting'}>
                <legend>Link expires after</legend>
                <div class="expiry-options">
                  {#each expiryOptions as option}
                    <label class:checked={expiry === option.value}>
                      <input type="radio" name="share-expiry" value={option.value} bind:group={expiry} />
                      <span>{option.label}</span>
                    </label>
                  {/each}
                </div>
              </fieldset>

              <div class="notice">
                Blackwall must stay open and connected while guests are chatting. You can end access at any time.
              </div>

              <button class="start-button" disabled={!model || busy === 'starting'} onclick={start}>
                {busy === 'starting' ? 'Creating link…' : 'Create guest link'}
              </button>
            </div>
          {/if}

          {#if error}
            <p class="error" role="alert">{error}</p>
          {/if}

          <p class="privacy-note">
            Guest chats are routed through this Blackwall app to your configured model endpoint.
          </p>
        </section>
      </div>
    </div>
  </div>
{/if}

<style>
  .modal-layer {
    position: fixed;
    z-index: 100;
    inset: 0;
    display: grid;
    place-items: center;
    padding: 20px;
  }

  .backdrop {
    position: absolute;
    inset: 0;
    background: var(--bg-scrim);
    backdrop-filter: blur(3px);
  }

  .settings-dialog {
    position: relative;
    display: flex;
    width: min(520px, 100%);
    max-height: min(740px, calc(100vh - 40px));
    flex-direction: column;
    overflow: hidden;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-lg);
    background: var(--bg-panel);
    box-shadow: var(--shadow-menu);
  }

  header {
    display: flex;
    min-height: 72px;
    align-items: center;
    justify-content: space-between;
    border-bottom: 1px solid var(--border);
    padding: 14px 16px 14px 20px;
  }

  h2,
  h3,
  p {
    margin: 0;
  }

  h2 {
    font-size: 16px;
    font-weight: 650;
    letter-spacing: -0.015em;
  }

  header p,
  .section-heading p,
  .qr-wrap p {
    color: var(--text-muted);
    font-size: 12px;
  }

  .icon-button {
    display: grid;
    width: 32px;
    height: 32px;
    place-items: center;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-muted);
  }

  .icon-button:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }

  .dialog-content {
    overflow-y: auto;
    padding: 20px;
  }

  .share-section {
    display: flex;
    flex-direction: column;
    gap: 17px;
  }

  .section-heading {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 16px;
  }

  h3 {
    margin-bottom: 3px;
    font-size: 14px;
    font-weight: 630;
  }

  .live-badge {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    border: 1px solid #4ade8033;
    border-radius: var(--radius-pill);
    padding: 3px 9px;
    background: #4ade800c;
    color: var(--ok);
    font-size: 10.5px;
    font-weight: 650;
    text-transform: uppercase;
  }

  .live-badge span {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--ok);
  }

  .loading-row {
    display: flex;
    min-height: 180px;
    align-items: center;
    justify-content: center;
    gap: 9px;
    color: var(--text-muted);
    font-size: 12px;
  }

  .spinner {
    width: 14px;
    height: 14px;
    border: 2px solid var(--border-strong);
    border-top-color: var(--accent);
    border-radius: 50%;
    animation: spin 700ms linear infinite;
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  .inactive-share,
  .active-share {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .model-row,
  .share-summary {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 20px;
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 12px 13px;
    background: var(--bg-elevated);
  }

  .model-row span,
  .summary-label,
  .link-label {
    color: var(--text-faint);
    font-size: 10.5px;
    font-weight: 650;
    letter-spacing: 0.06em;
    text-transform: uppercase;
  }

  .model-row strong {
    overflow: hidden;
    color: var(--text-muted);
    font-size: 12px;
    font-weight: 550;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  fieldset {
    min-width: 0;
    margin: 0;
    border: 0;
    padding: 0;
  }

  legend {
    margin-bottom: 8px;
    color: var(--text-muted);
    font-size: 11.5px;
  }

  .expiry-options {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    gap: 7px;
  }

  .expiry-options label {
    display: grid;
    min-height: 38px;
    place-items: center;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--bg-elevated);
    color: var(--text-muted);
    cursor: pointer;
    font-size: 12px;
    transition: all var(--transition-fast);
  }

  .expiry-options label:hover {
    border-color: var(--border-strong);
    color: var(--text-primary);
  }

  .expiry-options label.checked {
    border-color: var(--accent);
    background: var(--accent-muted);
    color: var(--accent);
  }

  .expiry-options input {
    position: absolute;
    width: 1px;
    height: 1px;
    opacity: 0;
  }

  .notice {
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 10px 11px;
    background: #ffffff03;
    color: var(--text-muted);
    font-size: 11.5px;
    line-height: 1.5;
  }

  .notice.warning {
    border-color: #fbbf2429;
    background: #fbbf2408;
    color: #d6bd7c;
  }

  .start-button,
  .stop-button {
    min-height: 40px;
    border-radius: var(--radius-sm);
    font-weight: 620;
  }

  .start-button {
    background: var(--accent);
    color: var(--text-on-accent);
  }

  .start-button:hover:not(:disabled) {
    background: var(--accent-hover);
  }

  .start-button:disabled {
    opacity: 0.45;
  }

  .stop-button {
    border: 1px solid #f8717133;
    background: #f871710c;
    color: var(--err);
  }

  .stop-button:hover:not(:disabled) {
    background: #f8717117;
  }

  .share-summary {
    align-items: stretch;
  }

  .share-summary > div {
    display: flex;
    min-width: 0;
    flex: 1;
    flex-direction: column;
    gap: 2px;
  }

  .share-summary > div + div {
    border-left: 1px solid var(--border);
    padding-left: 14px;
  }

  .share-summary strong {
    overflow: hidden;
    color: var(--text-muted);
    font-size: 11.5px;
    font-weight: 540;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .qr-wrap {
    display: flex;
    align-items: center;
    gap: 14px;
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 13px;
    background: var(--bg-elevated);
  }

  .qr-wrap img {
    width: 112px;
    height: 112px;
    flex: 0 0 auto;
    border: 7px solid white;
    border-radius: 8px;
    background: white;
  }

  .qr-wrap strong {
    display: block;
    margin-bottom: 3px;
    font-size: 13px;
  }

  .link-label {
    margin-bottom: -8px;
  }

  .link-field {
    display: flex;
    min-width: 0;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--bg-base);
  }

  .link-field:focus-within {
    border-color: var(--accent);
    box-shadow: 0 0 0 2px var(--focus-ring);
  }

  .link-field input {
    min-width: 0;
    height: 40px;
    flex: 1;
    border: 0;
    outline: 0;
    padding: 0 10px;
    background: transparent;
    color: var(--text-muted);
    font-family: var(--font-code);
    font-size: 10.5px;
  }

  .link-field button {
    display: flex;
    min-width: 73px;
    align-items: center;
    justify-content: center;
    gap: 6px;
    margin: 4px;
    border-radius: 5px;
    background: var(--bg-hover);
    color: var(--text-muted);
    font-size: 11.5px;
  }

  .link-field button:hover,
  .link-field button.copied {
    color: var(--accent);
  }

  .share-meta {
    display: flex;
    justify-content: space-between;
    color: var(--text-faint);
    font-size: 10.5px;
  }

  .error {
    border-radius: var(--radius-sm);
    padding: 9px 11px;
    background: #f871710d;
    color: var(--err);
    font-size: 11.5px;
  }

  .privacy-note {
    border-top: 1px solid var(--border);
    padding-top: 14px;
    color: var(--text-faint);
    font-size: 10.5px;
    line-height: 1.5;
  }

  @media (max-width: 560px) {
    .modal-layer {
      place-items: end center;
      padding: 0;
    }

    .settings-dialog {
      width: 100%;
      max-height: calc(100vh - 20px);
      border-right: 0;
      border-bottom: 0;
      border-left: 0;
      border-radius: var(--radius-lg) var(--radius-lg) 0 0;
    }
  }
</style>
