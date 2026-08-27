<script lang="ts">
  import Icon from './Icon.svelte';

  export let title = '';
  export let result = '';
</script>

<details>
  <summary>
    <span class="branch" aria-hidden="true"></span>
    <span class="title">{title}</span>
    <span class="status"><Icon name="check" size={12} /> complete</span>
  </summary>
  <p>{result}</p>
</details>

<style>
  details {
    border: 1px solid var(--border);
    border-left: 2px solid var(--accent-muted);
    border-radius: 0 var(--radius-sm) var(--radius-sm) 0;
    background: var(--subagent-bg);
  }

  summary {
    display: flex;
    min-height: 39px;
    align-items: center;
    gap: 9px;
    padding: 0 11px;
    cursor: pointer;
    list-style: none;
  }

  summary::-webkit-details-marker {
    display: none;
  }

  .branch {
    width: 8px;
    height: 8px;
    border-right: 1px solid var(--text-faint);
    border-bottom: 1px solid var(--text-faint);
  }

  .title {
    min-width: 0;
    flex: 1;
    color: var(--text-primary);
    font-size: 12px;
    font-weight: 550;
  }

  .status {
    display: flex;
    align-items: center;
    gap: 4px;
    color: var(--ok);
    font-size: 10.5px;
  }

  p {
    margin: 0;
    border-top: 1px solid var(--border);
    padding: 10px 12px;
    color: var(--text-muted);
    font-size: 12px;
  }
</style>
