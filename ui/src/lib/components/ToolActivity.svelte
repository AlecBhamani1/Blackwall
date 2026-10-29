<script lang="ts">
  import type { ToolEntry } from '../types';
  import Icon from './Icon.svelte';
  export let historical = false;
  export let tools: ToolEntry[] = [];
  $: running = tools.filter((tool) => tool.status === 'running').length;
  $: failed = tools.filter((tool) => tool.status === 'error').length;
  $: stopped = tools.filter((tool) => tool.status === 'stopped').length;
  $: status =
    [
      running ? `${running} running` : '',
      failed ? `${failed} failed` : '',
      stopped ? `${stopped} stopped` : '',
    ]
      .filter(Boolean)
      .join(' · ') || 'Complete';
  const labels: Record<string, string> = {
    project_instructions: 'Project instructions',
    read_file: 'Read file',
    search_files: 'Search files',
    list_files: 'List files',
    write_file: 'Edit file',
    shell: 'Run command',
    web_fetch: 'Read web page',
    web_search: 'Search the web',
    spawn_agent: 'Delegate task',
  };
  // Startup warnings, such as an MCP server that did not start, are shown without expanding.
  function warnings(tool: ToolEntry): string[] {
    if (tool.name !== 'project_instructions') return [];
    return (tool.output ?? '')
      .split('\n')
      .filter((line) => line.startsWith('Warning: '))
      .map((line) => line.slice('Warning: '.length));
  }
  function detail(tool: ToolEntry): string {
    try {
      const args = JSON.parse(tool.arguments);
      return String(args.path ?? args.command ?? args.url ?? args.query ?? args.goal ?? '').slice(
        0,
        160,
      );
    } catch {
      return '';
    }
  }
</script>

<div class="tool-activity" aria-label={historical ? 'Historical agent actions' : 'Agent actions'}>
  <details class="tool-group">
    <summary class="group-summary">
      <span class="chevron"><Icon name="chevron-right" size={13} /></span>
      <strong>{tools.length} tool {tools.length === 1 ? 'call' : 'calls'}</strong>
      <small class:running={running > 0} class:has-errors={failed > 0}>
        {historical ? 'History · ' : ''}{status}
      </small>
    </summary>
    <div class="tool-list">
      {#each tools as tool (tool.id)}
        <details class="tool-row" class:failed={tool.status === 'error'}>
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
            ><strong>{labels[tool.name] ?? tool.name}</strong><span class="detail"
              >{detail(tool)}</span
            ><small>{historical ? 'History · ' : ''}{tool.status}</small></summary
          >
          <div class="tool-content">
            <small>Arguments</small>
            <pre>{tool.arguments}</pre>
            {#if tool.output !== undefined}
              <small>Result</small>
              {#if tool.truncated}<small class="trimmed"
                  >Output trimmed to its start and end to fit the output cap.</small
                >{/if}
              <pre>{tool.output}</pre>
            {/if}
          </div>
        </details>
      {/each}
    </div>
  </details>
  {#each tools as tool (tool.id)}
    {#each warnings(tool) as warning}
      <p class="warning">{warning}</p>
    {/each}
  {/each}
</div>

<style>
  .tool-activity {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: 7px;
    margin: 14px 0;
  }
  .tool-list {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: 7px;
    padding: 0 10px 10px;
  }
  .group-summary small {
    margin-left: auto;
    text-align: right;
  }
  .chevron {
    display: flex;
    color: var(--text-muted);
  }
  .tool-group[open] > .group-summary .chevron {
    transform: rotate(90deg);
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
  .has-errors {
    color: var(--err);
  }
  .running {
    color: var(--warn);
  }
  .detail {
    min-width: 0;
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
  .trimmed {
    display: block;
    color: var(--text-muted);
  }
  .warning {
    margin: 0;
    padding: 0 12px;
    font-size: 11.5px;
    color: var(--err);
    overflow-wrap: anywhere;
  }
  pre {
    max-height: 300px;
    overflow: auto;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    font: 11px/1.6 var(--font-code);
    margin: 0;
    padding: 4px 0 8px;
  }
  .tool-content {
    padding: 8px 12px 0;
    border-top: 1px solid var(--border);
  }
</style>
