<script lang="ts">
  import { tick } from 'svelte';
  import { authClient, authError, authStatus } from '../auth';
  import { isDesktop } from '../setup';
  export let onLock: () => Promise<void> = async () => {};
  let current = '';
  let passphrase = '';
  let confirmation = '';
  let busy = false;
  let error = '';
  let saved = '';
  let currentInput: HTMLInputElement | undefined;
  let passphraseInput: HTMLInputElement | undefined;
  let errorMessage: HTMLParagraphElement | undefined;
  async function lock() {
    if (busy) return;
    busy = true;
    error = '';
    saved = '';
    try {
      await onLock();
    } catch (cause) {
      error = authError(cause);
      await tick();
      errorMessage?.focus();
    } finally {
      busy = false;
    }
  }
  async function save(remove = false) {
    if (busy) return;
    error = '';
    saved = '';
    if (!remove && (passphrase.length < 10 || passphrase !== confirmation)) {
      error = 'Use at least 10 characters and enter the same passphrase twice.';
      return;
    }
    busy = true;
    try {
      authStatus.set(await authClient.configure(current, remove ? '' : passphrase));
      saved = remove
        ? 'App lock removed.'
        : 'Passphrase saved. Blackwall will lock when you reopen it.';
      current = '';
      passphrase = '';
      confirmation = '';
    } catch (cause) {
      error = authError(cause);
    } finally {
      busy = false;
      await tick();
      ($authStatus.enabled ? currentInput : passphraseInput)?.focus();
    }
  }
</script>

{#if isDesktop()}
  <section aria-labelledby="app-lock-heading">
    <h3 id="app-lock-heading">App lock</h3>
    <p>
      Require a passphrase when Blackwall opens. Locking stops active work and disconnects guests
      and paired computers.
    </p>
    <p class="limit">
      This locks the desktop app. Saved files remain readable by your macOS account; use FileVault
      to protect your disk.
    </p>
    <form
      onsubmit={(event) => {
        event.preventDefault();
        void save();
      }}
    >
      {#if $authStatus.enabled}<label
          >Current passphrase<input
            type="password"
            autocomplete="current-password"
            bind:value={current}
            bind:this={currentInput}
            required
            maxlength={1024}
            disabled={busy}
          /></label
        >{/if}
      <label
        >{$authStatus.enabled ? 'New passphrase' : 'Passphrase'}<input
          type="password"
          autocomplete="new-password"
          bind:value={passphrase}
          bind:this={passphraseInput}
          minlength={10}
          maxlength={1024}
          required
          disabled={busy}
        /></label
      >
      <label
        >Confirm passphrase<input
          type="password"
          autocomplete="new-password"
          bind:value={confirmation}
          minlength={10}
          maxlength={1024}
          required
          disabled={busy}
        /></label
      >
      <div class="actions">
        <button disabled={busy}
          >{busy
            ? 'Saving…'
            : $authStatus.enabled
              ? 'Change passphrase'
              : 'Enable app lock'}</button
        >{#if $authStatus.enabled}<button
            type="button"
            disabled={busy || !current}
            onclick={() => save(true)}>Remove lock</button
          ><button type="button" disabled={busy} onclick={lock}>Lock now</button>{/if}
      </div>
    </form>
    {#if error}<p bind:this={errorMessage} class="error" role="alert" tabindex="-1">{error}</p>{/if}
    {#if saved}<p class="saved" role="status">{saved}</p>{/if}
  </section>
{/if}

<style>
  section {
    border-bottom: 1px solid var(--border);
    padding-bottom: 20px;
    margin-bottom: 20px;
  }
  h3 {
    font-size: 14px;
    margin: 0 0 6px;
  }
  p {
    color: var(--text-muted);
    font-size: 12px;
    line-height: 1.6;
  }
  .limit {
    color: var(--text-faint);
  }
  form,
  label {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  form {
    gap: 12px;
  }
  label {
    font-size: 12px;
    color: var(--text-muted);
  }
  input {
    width: 100%;
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    padding: 10px;
    color: var(--text-primary);
    background: var(--bg-elevated);
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }
  button {
    padding: 10px 12px;
    border-radius: 6px;
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  button:first-child {
    background: var(--accent);
    color: var(--text-on-accent);
  }
  button:disabled {
    opacity: 0.5;
  }
  .error {
    color: var(--err);
  }
  .saved {
    color: var(--ok);
  }
</style>
