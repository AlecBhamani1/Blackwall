<script lang="ts">
  import Icon from './Icon.svelte';

  export let command = '';
  export let duration = '';
  export let output = '';
</script>

<details>
  <summary>
    <span class="tool-label">shell</span>
    <code>{command}</code>
    <span class="duration">{duration}</span>
    <span class="success"><Icon name="check" size={13} /></span>
  </summary>
  <pre>{output}</pre>
</details>

<style>
  details {
    overflow: hidden;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--bg-elevated);
    color: var(--text-muted);
  }

  summary {
    display: flex;
    min-height: 35px;
    align-items: center;
    gap: 9px;
    padding: 0 10px;
    cursor: pointer;
    list-style: none;
  }

  summary::-webkit-details-marker {
    display: none;
  }

  .tool-label {
    color: var(--text-faint);
    font-size: 10px;
    font-weight: 650;
    letter-spacing: 0.07em;
    text-transform: uppercase;
  }

  code,
  pre {
    font-family: var(--font-code);
    font-size: 11.5px;
  }

  code {
    min-width: 0;
    flex: 1;
    overflow: hidden;
    color: var(--text-primary);
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .duration {
    color: var(--text-faint);
    font-size: 10.5px;
  }

  .success {
    display: grid;
    color: var(--ok);
  }

  pre {
    max-height: 180px;
    overflow: auto;
    margin: 0;
    border-top: 1px solid var(--border);
    padding: 10px;
    color: var(--text-muted);
    white-space: pre-wrap;
  }
</style>
