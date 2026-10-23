<script lang="ts">
  import type { ConnectionProfile } from '../persistence';
  import { normalizeEndpoint } from '../setup';
  import Icon from './Icon.svelte';

  export let connections: ConnectionProfile[] = [];
  export let currentEndpoint = '';
  export let busy = false;
  export let onConnect: (endpoint: string) => void;
  export let onForget: (endpoint: string) => void;

  function current(address: string): boolean {
    try {
      return normalizeEndpoint(currentEndpoint) === address;
    } catch {
      return false;
    }
  }
</script>

{#if connections.length}
  <section aria-label="Saved connections">
    <p class="saved-label">Saved connections</p>
    {#each connections as profile (profile.endpoint)}
      <div class="saved-row">
        <button
          class="connect"
          aria-label={`Connect to ${profile.name}`}
          disabled={busy}
          onclick={() => onConnect(profile.endpoint)}
        >
          <span class="profile">
            <strong
              >{profile.name}{#if current(profile.endpoint)}<small>Current</small>{/if}</strong
            >
            <span class="address">{profile.endpoint}</span>
          </span>
          <span class="action">{current(profile.endpoint) ? 'Reconnect' : 'Connect'}</span>
        </button>
        <button
          class="forget"
          aria-label={`Forget ${profile.name}`}
          title="Forget connection; its saved access key is kept"
          disabled={busy}
          onclick={() => onForget(profile.endpoint)}><Icon name="x" size={14} /></button
        >
      </div>
    {/each}
    <p class="hint">Forgetting a connection keeps its saved access key.</p>
  </section>
{/if}

<style>
  .saved-label {
    margin-bottom: 8px;
    color: var(--text-muted);
    font-size: 12px;
  }
  .saved-row {
    display: flex;
    gap: 8px;
    border-bottom: 1px solid var(--border);
  }
  .connect {
    display: flex;
    min-width: 0;
    align-items: center;
    gap: 12px;
    flex: 1;
    padding: 14px 0;
    background: transparent;
    text-align: left;
  }
  .profile {
    display: grid;
    min-width: 0;
    flex: 1;
    gap: 5px;
  }
  strong {
    overflow-wrap: anywhere;
    color: var(--text-primary);
    font-size: 13px;
  }
  strong small {
    margin-left: 8px;
    color: var(--text-muted);
    font-size: 10px;
    font-weight: 400;
  }
  .address {
    overflow-wrap: anywhere;
    color: var(--text-muted);
    font-size: 11px;
  }
  .action {
    color: var(--accent);
    font-size: 12px;
  }
  .forget {
    padding: 10px;
    background: transparent;
    color: var(--text-muted);
  }
  .hint {
    margin-top: 10px;
    color: var(--text-muted);
    font-size: 11px;
  }
</style>
