<script lang="ts">
  import type { ToolEntry } from '../types';
  import Icon from './Icon.svelte';
  export let tools: ToolEntry[] = [];
  const labels: Record<string, string> = {
    read_file: 'Read file',
    search_files: 'Search files',
    list_files: 'List files',
    write_file: 'Edit file',
    shell: 'Run command',
    web_fetch: 'Read web page',
    web_search: 'Search the web',
    spawn_agent: 'Delegate task',
  };
  function detail(tool: ToolEntry): string {
    try {
      const args = JSON.parse(tool.arguments);
      return String(args.path ?? args.command ?? args.url ?? args.goal ?? '').slice(0, 160);
    } catch {
      return '';
    }
  }
</script>

<div class="tool-activity" aria-label="Agent actions">
  {#each tools as tool (tool.id)}
    <details class:failed={tool.status === 'error'}>
      <summary
        ><span class="state" class:running={tool.status === 'running'}
          >{#if tool.status === 'complete'}<Icon
              name="check"
              size={13}
            />{:else if tool.status === 'error'}<Icon
              name="x"
              size={13}
            />{:else if tool.status === 'stopped'}<Icon name="stop" size={11} />{:else}<Icon
              name="refresh"
              size={12}
            />{/if}</span
        ><strong>{labels[tool.name] ?? tool.name}</strong><span class="detail">{detail(tool)}</span
        ><small>{tool.status}</small></summary
      >
      <pre>{tool.output ?? tool.arguments}</pre>
    </details>
  {/each}
</div>

<style>
  .tool-activity {
    display: grid;
    gap: 7px;
    margin: 14px 0;
  }
  details {
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--bg-panel);
  }
  summary {
    display: flex;
    align-items: center;
    gap: 8px;
    cursor: pointer;
    padding: 10px 12px;
    list-style: none;
  }
  summary::-webkit-details-marker {
    display: none;
  }
  strong {
    font-size: 11.5px;
    font-weight: 550;
    white-space: nowrap;
  }
  .state {
    color: var(--ok);
    display: flex;
  }
  .failed .state {
    color: var(--err);
  }
  .running {
    color: var(--warn);
  }
  .detail {
    font: 10.5px var(--font-code);
    color: var(--text-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    flex: 1;
  }
  small {
    font-size: 10px;
    color: var(--text-muted);
  }
  pre {
    max-height: 300px;
    overflow: auto;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    font: 11px/1.6 var(--font-code);
    margin: 0;
    padding: 12px;
    border-top: 1px solid var(--border);
  }
</style>
