<script lang="ts">
  import { onDestroy, tick } from 'svelte';
  import { credentialClient, type CredentialKind } from '../credentials';

  export let endpoint: string;
  export let kind: CredentialKind = 'model';
  export let client = credentialClient;
  let saved: boolean | null = null;
  let key = '';
  let busy = false;
  let mutating = false;
  let error = '';
  let feedback = '';
  let confirming = false;
  let previous = '';
  let version = 0;
  let resultMessage: HTMLParagraphElement | undefined;
  let keepButton: HTMLButtonElement | undefined;
  let removeButton: HTMLButtonElement | undefined;
  $: scope = `${kind}:${endpoint}`;
  $: if (scope !== previous) {
    previous = scope;
    key = '';
    confirming = false;
    feedback = '';
    void refresh();
  }
  function message(cause: unknown) {
    return cause instanceof Error
      ? cause.message
      : typeof cause === 'string'
        ? cause
        : 'Keychain could not be updated. Try again.';
  }
  async function refresh() {
    const operation = ++version;
    saved = null;
    error = '';
    busy = true;
    mutating = false;
    try {
      const next = await client.status(kind, endpoint);
      if (operation === version) saved = next;
    } catch (cause) {
      if (operation === version) error = message(cause);
    } finally {
      if (operation === version) busy = false;
    }
  }
  async function confirmRemoval(confirm: boolean) {
    confirming = confirm;
    const operation = version;
    await tick();
    if (operation === version) (confirm ? keepButton : removeButton)?.focus();
  }
  async function update(remove = false) {
    if (busy || (!remove && !key.trim())) return;
    const operation = ++version;
    busy = true;
    mutating = true;
    error = '';
    feedback = '';
    try {
      if (remove) await client.remove(kind, endpoint);
      else await client.saveModel(endpoint, key.trim());
      if (operation !== version) return;
      saved = !remove;
      key = '';
      confirming = false;
      feedback = remove
        ? 'Saved key removed from Keychain.'
        : 'Access key verified and saved in Keychain.';
    } catch (cause) {
      if (operation === version) error = message(cause);
    } finally {
      if (operation === version) {
        busy = false;
        mutating = false;
        await tick();
        if (operation === version) resultMessage?.focus();
      }
    }
  }
  onDestroy(() => {
    version += 1;
    key = '';
  });
</script>

<section aria-label={kind === 'model' ? 'Model access key' : 'Relay access key'}>
  <div class="key-heading">
    <strong>{kind === 'model' ? 'Model access key' : 'Relay access key'}</strong>
    <span
      >{saved === null
        ? busy
          ? 'Checking…'
          : 'Status unavailable'
        : saved
          ? 'Saved in Keychain'
          : 'No saved key'}</span
    >
  </div>
  <p class="address">{endpoint}</p>
  {#if kind === 'model'}
    <form
      onsubmit={(event) => {
        event.preventDefault();
        void update();
      }}
    >
      <label
        >New access key
        <input
          type="password"
          autocomplete="new-password"
          maxlength={8192}
          bind:value={key}
          disabled={busy}
          placeholder="Paste a replacement key"
        />
      </label>
      <button disabled={busy || !key.trim()}>{busy ? 'Working…' : 'Verify and save key'}</button>
    </form>
    <p>A replacement is checked with this service before your saved key is changed.</p>
  {:else}
    <p>
      To replace this key, enter a new relay token when creating a guest link. It is saved after the
      relay accepts it.
    </p>
  {/if}
  {#if saved}
    {#if confirming}
      <div class="confirmation">
        <p>
          Remove the saved {kind} key for this service? Future connections may require it again. Active
          requests and guest links continue; revoke guest links separately.
        </p>
        <div class="actions">
          <button class="remove" disabled={busy} onclick={() => update(true)}
            >Remove saved key</button
          >
          <button bind:this={keepButton} disabled={busy} onclick={() => confirmRemoval(false)}
            >Keep key</button
          >
        </div>
      </div>
    {:else}
      <button
        bind:this={removeButton}
        class="subtle"
        disabled={busy}
        onclick={() => confirmRemoval(true)}>Remove key…</button
      >
    {/if}
  {/if}
  {#if mutating}
    <p role="status">
      Working on your saved key. Respond to any macOS Keychain prompt and wait for confirmation
      before retrying.
    </p>
  {/if}
  {#if error}<p role="alert" class="error" tabindex="-1" bind:this={resultMessage}>{error}</p>
    {#if saved === null}<button disabled={busy} onclick={refresh}>Retry Keychain</button>{/if}{/if}
  {#if feedback}<p role="status" class="feedback" tabindex="-1" bind:this={resultMessage}>
      {feedback}
    </p>{/if}
</section>

<style>
  section {
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 14px;
    margin-top: 12px;
  }
  .key-heading {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px 16px;
    justify-content: space-between;
    font-size: 12px;
  }
  .key-heading span,
  p {
    color: var(--text-muted);
    font-size: 12px;
    line-height: 1.6;
  }
  .address {
    overflow-wrap: anywhere;
    font-family: var(--font-mono);
  }
  form,
  label {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  label {
    font-size: 12px;
    color: var(--text-muted);
  }
  input {
    width: 100%;
    min-width: 0;
    padding: 10px;
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    background: var(--bg-elevated);
    color: var(--text-primary);
  }
  button {
    align-self: flex-start;
    padding: 9px 12px;
    border-radius: 6px;
    background: var(--bg-hover);
    color: var(--text-primary);
    font-size: 12px;
  }
  form button {
    background: var(--accent);
    color: var(--text-on-accent);
  }
  button:disabled {
    opacity: 0.5;
  }
  .subtle {
    padding-left: 0;
    background: transparent;
    color: var(--text-muted);
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }
  .confirmation {
    margin-top: 12px;
    border-top: 1px solid var(--border);
  }
  .error,
  .remove {
    color: var(--err);
  }
  .feedback {
    color: var(--ok);
  }
</style>
