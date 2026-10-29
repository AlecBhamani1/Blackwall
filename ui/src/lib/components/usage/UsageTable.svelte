<script lang="ts">
  import type { UsageSummary } from '../../persistence';
  export let tools: UsageSummary['tools'];

  function rate(count: number, calls: number) {
    return count === 0 ? '—' : `${((count / Math.max(calls, 1)) * 100).toFixed(1)}%`;
  }
</script>

<table aria-label="Top tools">
  <thead>
    <tr>
      <th scope="col">Tool</th>
      <th scope="col">Calls</th>
      <th scope="col">Errors</th>
      <th scope="col">Denied</th>
    </tr>
  </thead>
  <tbody>
    {#each tools as tool (tool.name)}
      <tr>
        <th scope="row" title={tool.name}>{tool.name}</th>
        <td>{tool.calls.toLocaleString()}</td>
        <td>{rate(tool.errors, tool.calls)}</td>
        <td>{rate(tool.denied, tool.calls)}</td>
      </tr>
    {:else}
      <tr><td colspan="4" class="none">No tool calls in this range.</td></tr>
    {/each}
  </tbody>
</table>

<style>
  table {
    width: 100%;
    border-collapse: collapse;
    table-layout: fixed;
    font-size: 12.5px;
  }
  th,
  td {
    padding: 5px 0;
    border-top: 1px solid var(--border);
    text-align: right;
    font-variant-numeric: tabular-nums;
    font-weight: 400;
  }
  td {
    font-family: var(--font-code);
    color: var(--text-muted);
  }
  tbody th {
    text-align: left;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    padding-right: 8px;
  }
  thead th {
    border-top: 0;
    font: 11px var(--font-code);
    color: var(--text-faint);
  }
  thead th:first-child {
    text-align: left;
    width: auto;
  }
  thead th:not(:first-child) {
    width: 7.5ch;
  }
  .none {
    text-align: left;
    font-family: inherit;
  }
</style>
