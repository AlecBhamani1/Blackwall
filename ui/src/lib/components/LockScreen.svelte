<script lang="ts">
  import { tick } from 'svelte';
  import LogoMark from './LogoMark.svelte';
  export let loading = false;
  export let error = '';
  export let onUnlock: (passphrase: string) => Promise<void>;
  export let onRetry: () => Promise<void>;
  export let initialized = false;
  let passphrase = '';
  let input: HTMLInputElement | undefined;
  async function focusPassphrase(node: HTMLInputElement) {
    await tick();
    if (node.isConnected && !node.disabled) node.focus();
  }
  $: if (initialized && !loading && input) void focusPassphrase(input);
  async function unlock(event: SubmitEvent) {
    event.preventDefault();
    const value = passphrase;
    passphrase = '';
    try {
      await onUnlock(value);
    } finally {
      if (input) await focusPassphrase(input);
    }
  }
</script>

<main class="lock-screen">
  <section aria-labelledby="lock-title">
    <LogoMark size={44} />
    <h1 id="lock-title">{initialized ? 'Welcome back' : 'Opening Blackwall'}</h1>
    <p>
      {initialized
        ? 'Enter your passphrase to open your conversations.'
        : 'Checking your app lock…'}
    </p>
    {#if initialized}
      <form onsubmit={unlock}>
        <label for="unlock-passphrase">Passphrase</label>
        <input
          id="unlock-passphrase"
          type="password"
          autocomplete="current-password"
          bind:value={passphrase}
          bind:this={input}
          disabled={loading}
          required
          maxlength={1024}
        />
        <button disabled={loading || !passphrase}
          >{loading ? 'Unlocking…' : 'Unlock Blackwall'}</button
        >
      </form>
    {:else if error}
      <button disabled={loading} onclick={onRetry}>Try again</button>
    {/if}
    {#if error}<p class="error" role="alert">{error}</p>{/if}
  </section>
</main>

<style>
  .lock-screen {
    display: grid;
    place-items: center;
    width: 100%;
    height: 100%;
    padding: 24px;
    background: var(--bg-base);
  }
  section {
    width: 100%;
    max-width: 360px;
  }
  h1 {
    margin: 24px 0 8px;
    font-size: 26px;
    letter-spacing: -0.03em;
  }
  p {
    color: var(--text-muted);
    line-height: 1.6;
    font-size: 13px;
  }
  form {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-top: 28px;
  }
  label {
    font-size: 12px;
    color: var(--text-muted);
  }
  input {
    width: 100%;
    padding: 12px;
    border: 1px solid var(--border-strong);
    border-radius: 8px;
    color: var(--text-primary);
    background: var(--bg-elevated);
  }
  button {
    padding: 12px 18px;
    border-radius: 8px;
    background: var(--accent);
    color: var(--text-on-accent);
    font-weight: 600;
  }
  button:disabled {
    opacity: 0.5;
  }
  .error {
    color: var(--err);
  }
</style>
