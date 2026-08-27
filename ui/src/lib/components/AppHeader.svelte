<script lang="ts">
  import { modelLabel } from '../format';
  import type { ConnectionState, ModelInfo } from '../types';
  import Icon from './Icon.svelte';
  import LogoMark from './LogoMark.svelte';

  export let sidebarVisible = true;
  export let models: ModelInfo[] = [];
  export let selectedModel = '';
  export let connectionState: ConnectionState = 'checking';
  export let onToggleSidebar: () => void;
  export let onModelChange: (modelId: string) => void;
  export let onReconnect: () => void;

  function changeModel(event: Event) {
    onModelChange((event.currentTarget as HTMLSelectElement).value);
  }
</script>

<header>
  <div class="header-side">
    <button class="icon-button" aria-label={sidebarVisible ? 'Hide sidebar' : 'Show sidebar'} onclick={onToggleSidebar}>
      <Icon name="menu" size={18} />
    </button>
    {#if !sidebarVisible}
      <div class="compact-brand"><LogoMark size={25} /><span>Blackwall</span></div>
    {/if}
  </div>

  <div class="model-control">
    <span
      class:ready={connectionState === 'ready'}
      class:offline={connectionState === 'offline'}
      class="state-dot"
      aria-hidden="true"
    ></span>
    {#if models.length > 0}
      <label class="sr-only" for="model-select">Model</label>
      <select id="model-select" value={selectedModel} onchange={changeModel}>
        {#each models as model (model.id)}
          <option value={model.id}>{modelLabel(model.name)}</option>
        {/each}
      </select>
      <span class="select-chevron"><Icon name="chevron-down" size={14} /></span>
    {:else}
      <span class="model-placeholder">
        {connectionState === 'checking' ? 'Finding your model…' : 'Model unavailable'}
      </span>
    {/if}
  </div>

  <div class="header-side right">
    {#if connectionState === 'offline'}
      <button class="reconnect" onclick={onReconnect}>
        <Icon name="refresh" size={14} />
        <span>Reconnect</span>
      </button>
    {:else}
      <div class="privacy" title="Connected only to your configured model endpoint">
        <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
          <rect width="16" height="11" x="4" y="11" rx="2" />
          <path d="M8 11V7a4 4 0 0 1 8 0v4" />
        </svg>
        <span>Private</span>
      </div>
    {/if}
  </div>
</header>

<style>
  header {
    position: relative;
    z-index: 5;
    display: grid;
    height: var(--topbar-height);
    min-height: var(--topbar-height);
    grid-template-columns: 1fr auto 1fr;
    align-items: center;
    border-bottom: 1px solid var(--border);
    padding: 0 14px;
    background: var(--bg-base);
  }

  .header-side {
    display: flex;
    min-width: 0;
    align-items: center;
    gap: 9px;
  }

  .header-side.right {
    justify-content: flex-end;
  }

  .icon-button {
    display: grid;
    width: 32px;
    height: 32px;
    flex: 0 0 auto;
    place-items: center;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-muted);
  }

  .icon-button:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }

  .compact-brand {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
    font-weight: 650;
  }

  .model-control {
    position: relative;
    display: flex;
    max-width: min(42vw, 380px);
    align-items: center;
    gap: 8px;
    border: 1px solid transparent;
    border-radius: var(--radius-sm);
    padding: 4px 7px 4px 8px;
    color: var(--text-muted);
    transition:
      border-color var(--transition-fast),
      background var(--transition-fast);
  }

  .model-control:hover,
  .model-control:focus-within {
    border-color: var(--border);
    background: var(--bg-panel);
  }

  .state-dot {
    width: 6px;
    height: 6px;
    flex: 0 0 auto;
    border-radius: 50%;
    background: var(--warn);
  }

  .state-dot.ready {
    background: var(--ok);
  }

  .state-dot.offline {
    background: var(--err);
  }

  select {
    max-width: 330px;
    overflow: hidden;
    appearance: none;
    border: 0;
    padding: 0 18px 0 0;
    background: transparent;
    color: var(--text-primary);
    font-size: 12.5px;
    font-weight: 550;
    outline: none;
    text-overflow: ellipsis;
  }

  .select-chevron {
    position: absolute;
    right: 6px;
    pointer-events: none;
    color: var(--text-faint);
  }

  .model-placeholder {
    overflow: hidden;
    font-size: 12.5px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .privacy,
  .reconnect {
    display: flex;
    align-items: center;
    gap: 6px;
    border-radius: var(--radius-sm);
    color: var(--text-faint);
    font-size: 11.5px;
  }

  .reconnect {
    min-height: 30px;
    padding: 0 9px;
    background: var(--bg-elevated);
    color: var(--text-muted);
  }

  .reconnect:hover {
    color: var(--text-primary);
  }

  @media (max-width: 620px) {
    header {
      grid-template-columns: auto 1fr auto;
      gap: 7px;
      padding: 0 9px;
    }

    .compact-brand span,
    .privacy span,
    .reconnect span {
      display: none;
    }

    .model-control {
      min-width: 0;
      justify-self: center;
    }

    select {
      max-width: 48vw;
    }
  }
</style>
