<script lang="ts">
  import { normalizeEndpoint, serviceAddress, type LocalService } from '../setup';

  export let services: LocalService[] = [];
  export let currentEndpoint = '';
  export let busy = false;
  export let scanning = false;
  export let onConnect: (service: LocalService) => void;
  export let onRescan: (() => void) | undefined = undefined;

  function current(service: LocalService): boolean {
    try {
      return !!currentEndpoint && normalizeEndpoint(currentEndpoint) === service.endpoint;
    } catch {
      return false;
    }
  }
  function modelSummary(service: LocalService): string {
    if (!service.models.length) return 'No models installed yet';
    const shown = service.models.slice(0, 3).join(' · ');
    return service.models.length > 3 ? `${shown} · +${service.models.length - 3} more` : shown;
  }
  function loadedLabel(service: LocalService): string {
    const context = service.contextWindow
      ? ` · ${service.contextWindow.toLocaleString('en-US')}-token context`
      : '';
    return `Loaded: ${(service.loaded ?? []).join(', ')}${context}`;
  }
  $: available = services.filter((service) => service.available);
</script>

<section class="found" aria-label="Found on this computer">
  <div class="found-heading">
    <p class="found-label">Found on this computer</p>
    {#if onRescan}<button class="rescan" disabled={busy || scanning} onclick={onRescan}
        >{scanning ? 'Looking…' : 'Look again'}</button
      >{/if}
  </div>
  {#if scanning && !available.length}
    <p class="empty" role="status">Looking for model servers on this computer…</p>
  {:else if !available.length}
    <p class="empty">No model servers are running on this computer.</p>
  {/if}
  {#each available as service (service.endpoint)}
    <div class="found-row">
      <div class="server">
        <strong
          >{service.name}{#if current(service)}<small>Current</small>{/if}</strong
        >
        <span class="address">{serviceAddress(service)}</span>
        <span class="models">{modelSummary(service)}</span>
        {#if service.loaded?.length}
          <span class="loaded"><span class="dot"></span>{loadedLabel(service)}</span>
        {/if}
      </div>
      <button
        class="connect"
        aria-label={`Connect to ${service.name} at ${serviceAddress(service)}`}
        disabled={busy}
        onclick={() => onConnect(service)}>{current(service) ? 'Reconnect' : 'Connect'}</button
      >
    </div>
  {/each}
</section>

<style>
  .found-heading {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    gap: 12px;
  }
  .found-label {
    margin: 0 0 8px;
    color: var(--text-muted);
    font-size: 12px;
  }
  .rescan {
    padding: 0;
    background: transparent;
    color: var(--text-muted);
    font-size: 11.5px;
  }
  .rescan:hover:not(:disabled) {
    color: var(--text-primary);
  }
  .empty {
    color: var(--text-muted);
    font-size: 12px;
    margin: 4px 0 8px;
  }
  .found-row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 12px 0;
    border-bottom: 1px solid var(--border);
  }
  .server {
    display: grid;
    gap: 4px;
    flex: 1;
    min-width: 0;
  }
  strong {
    color: var(--text-primary);
    font-size: 13px;
    overflow-wrap: anywhere;
  }
  strong small {
    margin-left: 8px;
    color: var(--text-muted);
    font-size: 10px;
    font-weight: 400;
  }
  .address,
  .models,
  .loaded {
    color: var(--text-muted);
    font-size: 11px;
    overflow-wrap: anywhere;
  }
  .address,
  .models {
    font-family: var(--font-code);
  }
  .loaded {
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }
  .dot {
    flex: 0 0 5px;
    width: 5px;
    height: 5px;
    border-radius: 50%;
    background: var(--ok);
  }
  .connect {
    min-height: 34px;
    padding: 7px 14px;
    border-radius: var(--radius-sm);
    background: var(--accent);
    color: var(--text-on-accent);
    font-size: 12px;
    font-weight: 580;
  }
  .connect:hover:not(:disabled) {
    background: var(--accent-hover);
  }
  .connect:disabled {
    opacity: 0.55;
    cursor: wait;
  }
</style>
