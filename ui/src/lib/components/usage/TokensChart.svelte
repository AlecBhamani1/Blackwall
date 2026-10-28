<script lang="ts">
  import type { UsageSummary } from '../../persistence';
  import { compact, dayLabel } from './format';
  export let days: UsageSummary['perDay'];

  let width = 0;
  let active: number | null = null;
  let showTable = false;
  const height = 168;
  const top = 10;
  const baseline = 128;
  const left = 40;
  const right = 4;

  /** A rounded y-axis maximum made of two clean steps. */
  function niceMax(value: number) {
    if (value <= 0) return 2;
    const half = value / 2;
    const power = 10 ** Math.floor(Math.log10(half));
    const step =
      [1, 1.2, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10].map((m) => m * power).find((s) => s >= half) ??
      10 * power;
    return step * 2;
  }
  /** A bar segment with 4px rounded corners on its data end only. */
  function segment(x: number, y: number, w: number, h: number, rounded: boolean) {
    const r = rounded ? Math.min(4, w / 2, h) : 0;
    return `M${x},${y + h}V${y + r}${r ? `Q${x},${y} ${x + r},${y}` : ''}H${x + w - r}${
      r ? `Q${x + w},${y} ${x + w},${y + r}` : ''
    }V${y + h}Z`;
  }

  $: plotWidth = Math.max((width || 560) - left - right, 40);
  $: max = niceMax(Math.max(0, ...days.map((day) => day.tokens.input + day.tokens.output)));
  $: scale = (value: number) => ((baseline - top) * value) / max;
  $: slot = plotWidth / Math.max(days.length, 1);
  $: barWidth = Math.max(1, Math.min(24, slot * 0.62));
  $: labelEvery = Math.max(1, Math.ceil(days.length / Math.max(1, Math.floor(plotWidth / 52))));
  $: short = days.length <= 7;
  $: bars = days.map((day, index) => {
    const x = left + index * slot + (slot - barWidth) / 2;
    const inputHeight = scale(day.tokens.input);
    const outputHeight = scale(day.tokens.output);
    // A 2px surface gap separates the stacked segments.
    const gap = inputHeight > 0 && outputHeight > 0 ? 2 : 0;
    return {
      day,
      index,
      x,
      input: { y: baseline - inputHeight, h: inputHeight },
      output: { y: baseline - inputHeight - gap - outputHeight, h: outputHeight },
    };
  });
  $: missing = days.some((day) => !day.recorded);
  $: tip = active === null ? null : bars[active];
</script>

<div class="chart" bind:clientWidth={width}>
  <svg
    role="img"
    aria-label={`Tokens per day, ${days[0]?.day ?? ''} to ${days.at(-1)?.day ?? ''}. The table below lists each day.`}
    width="100%"
    {height}
    onpointerleave={() => (active = null)}
  >
    {#each [0, max / 2, max] as tick (tick)}
      <line
        class="grid"
        x1={left}
        x2={left + plotWidth}
        y1={baseline - scale(tick)}
        y2={baseline - scale(tick)}
      />
      <text class="axis" x={left - 6} y={baseline - scale(tick) + 3} text-anchor="end"
        >{compact(tick)}</text
      >
    {/each}
    {#each bars as bar (bar.day.day)}
      <g class:dim={active !== null && active !== bar.index}>
        {#if bar.input.h > 0}<path
            class="input"
            d={segment(bar.x, bar.input.y, barWidth, bar.input.h, bar.output.h <= 0)}
          />{/if}
        {#if bar.output.h > 0}<path
            class="output"
            d={segment(bar.x, bar.output.y, barWidth, bar.output.h, true)}
          />{/if}
        {#if !bar.day.recorded}<text
            class="axis"
            x={bar.x + barWidth / 2}
            y={baseline - 6}
            text-anchor="middle">–</text
          >{/if}
        {#if bar.index % labelEvery === 0}<text
            class="axis"
            x={bar.x + barWidth / 2}
            y={baseline + 16}
            text-anchor="middle">{dayLabel(bar.day.day, short)}</text
          >{/if}
        <!-- The hit area is the whole day slot, larger than the bar. -->
        <rect
          role="presentation"
          class="hit"
          x={left + bar.index * slot}
          y={top}
          width={slot}
          height={baseline - top}
          onpointerenter={() => (active = bar.index)}
        />
      </g>
    {/each}
    <g class="legend" transform={`translate(${left}, ${height - 14})`}>
      <rect class="input" width="8" height="8" rx="2" />
      <text class="axis" x="12" y="8">input</text>
      <rect class="output" x="52" width="8" height="8" rx="2" />
      <text class="axis" x="64" y="8">output</text>
      {#if missing}<text class="axis" x="112" y="8">– no data recorded</text>{/if}
    </g>
  </svg>
  {#if tip}
    <div
      class="tooltip"
      style={`left: ${Math.min(Math.max(tip.x + barWidth / 2, 70), (width || 560) - 70)}px; top: ${Math.max(tip.output.y - 8, 0)}px`}
    >
      <strong>{(tip.day.tokens.input + tip.day.tokens.output).toLocaleString()}</strong>
      <span>{dayLabel(tip.day.day, false)}{tip.day.recorded ? '' : ' · no data'}</span>
      <span><i class="key input"></i>{tip.day.tokens.input.toLocaleString()} input</span>
      <span><i class="key output"></i>{tip.day.tokens.output.toLocaleString()} output</span>
    </div>
  {/if}
</div>
<button class="link" aria-expanded={showTable} onclick={() => (showTable = !showTable)}
  >{showTable ? 'Hide table' : 'Show as table'}</button
>
<!-- Always in the accessibility tree; shown on request. -->
<div class:sr-only={!showTable}>
  <table aria-label="Tokens per day">
    <thead>
      <tr><th scope="col">Day (UTC)</th><th scope="col">Input</th><th scope="col">Output</th></tr>
    </thead>
    <tbody>
      {#each days as day (day.day)}
        <tr>
          <th scope="row">{day.day}</th>
          {#if day.recorded}
            <td>{day.tokens.input.toLocaleString()}</td>
            <td>{day.tokens.output.toLocaleString()}</td>
          {:else}
            <td colspan="2">No data</td>
          {/if}
        </tr>
      {/each}
    </tbody>
  </table>
</div>

<style>
  .chart {
    position: relative;
    width: 100%;
  }
  svg {
    display: block;
    overflow: visible;
  }
  .grid {
    stroke: var(--border);
    stroke-width: 1;
    shape-rendering: crispEdges;
  }
  .axis {
    fill: var(--text-faint);
    font: 10px var(--font-code);
    font-variant-numeric: tabular-nums;
  }
  .input {
    fill: var(--text-muted);
  }
  .output {
    fill: var(--accent);
  }
  g.dim path {
    opacity: 0.45;
  }
  .hit {
    fill: transparent;
  }
  .tooltip {
    position: absolute;
    transform: translate(-50%, -100%);
    display: flex;
    flex-direction: column;
    gap: 1px;
    padding: 7px 10px;
    background: var(--bg-elevated);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-sm);
    box-shadow: var(--shadow-menu);
    font-size: 11.5px;
    color: var(--text-muted);
    white-space: nowrap;
    pointer-events: none;
  }
  .tooltip strong {
    color: var(--text-primary);
    font-size: 13px;
  }
  .key {
    display: inline-block;
    width: 10px;
    height: 2px;
    margin: 0 6px 3px 0;
    vertical-align: middle;
  }
  .key.input {
    background: var(--text-muted);
  }
  .key.output {
    background: var(--accent);
  }
  .link {
    background: transparent;
    color: var(--text-muted);
    font-size: 11.5px;
    padding: 2px 0;
    text-decoration: underline;
    text-underline-offset: 2px;
  }
  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 12px;
    margin-top: 6px;
  }
  th,
  td {
    text-align: right;
    padding: 4px 0;
    border-top: 1px solid var(--border);
    font-variant-numeric: tabular-nums;
    font-weight: 400;
  }
  th:first-child {
    text-align: left;
  }
  thead th {
    border-top: 0;
    color: var(--text-faint);
    font: 11px var(--font-code);
  }
  td[colspan] {
    color: var(--text-faint);
  }
</style>
