<script lang="ts">
  import { modelLabel } from '../format';
  import type { ConnectionState, RunState } from '../types';

  export let connectionState: ConnectionState = 'checking';
  export let runState: RunState = 'idle';
  export let model = '';
  export let contextPercent = 0;

  $: stateLabel =
    runState === 'streaming'
      ? 'Generating'
      : runState === 'preparing'
        ? 'Reading files'
        : connectionState === 'ready'
          ? 'Ready'
          : connectionState === 'checking'
            ? 'Connecting'
            : 'Offline';
</script>

<footer>
  <div class="status-group">
    <span
      class:ready={connectionState === 'ready' && runState === 'idle'}
      class:working={runState === 'streaming' || runState === 'preparing'}
      class:offline={connectionState === 'offline'}
      class="state-dot"
    ></span>
    <span>{stateLabel}</span>
  </div>
  <div class="status-group center">
    <span>{model ? modelLabel(model) : 'no model'}</span>
    <span class="separator">·</span>
    <span>context {contextPercent}% <span class="estimate">est</span></span>
  </div>
  <div class="status-group right"><span>private endpoint</span></div>
</footer>

<style>
  footer {
    display: grid;
    height: var(--statusbar-height);
    min-height: var(--statusbar-height);
    grid-template-columns: 1fr auto 1fr;
    align-items: center;
    border-top: 1px solid var(--border);
    padding: 0 11px;
    background: var(--bg-panel);
    color: var(--text-faint);
    font-family: var(--font-code);
    font-size: 9.5px;
  }

  .status-group {
    display: flex;
    min-width: 0;
    align-items: center;
    gap: 6px;
    white-space: nowrap;
  }

  .status-group.right {
    justify-content: flex-end;
  }

  .state-dot {
    width: 5px;
    height: 5px;
    border-radius: 50%;
    background: var(--warn);
  }

  .state-dot.ready {
    background: var(--ok);
  }

  .state-dot.working {
    background: var(--accent);
    box-shadow: 0 0 0 3px var(--focus-ring);
  }

  .state-dot.offline {
    background: var(--err);
  }

  .separator,
  .estimate {
    color: var(--text-faint);
  }

  @media (max-width: 560px) {
    footer {
      grid-template-columns: 1fr auto;
    }

    .status-group.center {
      justify-content: flex-end;
    }

    .status-group.right {
      display: none;
    }
  }
</style>
