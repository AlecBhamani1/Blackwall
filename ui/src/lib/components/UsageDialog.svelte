<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { persistence, type UsageGroupRow, type UsageSummary } from '../persistence';
  import Icon from './Icon.svelte';
  import ExportCard from './usage/ExportCard.svelte';
  import StatTiles from './usage/StatTiles.svelte';
  import TokensChart from './usage/TokensChart.svelte';
  import UsageTable from './usage/UsageTable.svelte';
  import { ago, compact, dayLabel, modeNames, percent, plural } from './usage/format';
  export let onClose: () => void;
  export let embedded = false;

  /** The page re-reads the metrics files this often while it is visible. */
  const REFRESH_MS = 120_000;
  const ranges = [
    [1, 'Today'],
    [7, '7 days'],
    [30, '30 days'],
    [90, '90 days'],
  ] as const;
  const desktop = persistence.available();
  const timeZone = Intl.DateTimeFormat().resolvedOptions().timeZone;
  let dialog: HTMLDialogElement;
  let days = 7;
  let summary: UsageSummary | null = null;
  let lastRead: Date | null = null;
  let loading = false;
  let error = '';
  let exported = '';
  type View = 'projects' | 'models';
  let view: View = 'projects';
  let selected: Record<View, string | null> = { projects: null, models: null };
  let request = 0;
  let timer: ReturnType<typeof setInterval> | undefined;

  const keyOf = (row: UsageGroupRow) => row.name ?? '';
  const nameOf = (row: UsageGroupRow) =>
    row.name ?? (view === 'projects' ? 'No project' : 'Unknown model');

  async function refresh() {
    const current = ++request;
    loading = true;
    error = '';
    try {
      const next = await persistence.usage(days);
      if (current !== request) return;
      summary = next;
      lastRead = new Date();
    } catch (cause) {
      // The previous numbers stay on screen; only the alert is new.
      if (current === request)
        error = typeof cause === 'string' ? cause : 'Usage could not be read. Try again.';
    } finally {
      if (current === request) loading = false;
    }
  }
  function pick(next: number) {
    days = next;
    exported = '';
    void refresh();
  }
  async function exportFiles() {
    exported = '';
    error = '';
    try {
      const report = await persistence.exportTelemetry(days);
      if (report)
        exported = `Exported ${plural(report.metricsFiles, 'metrics file')} and ${plural(
          report.traceFiles,
          'trace',
        )} to ${report.folder}.`;
    } catch (cause) {
      error = typeof cause === 'string' ? cause : 'The export could not be written.';
    }
  }
  function visible() {
    return typeof document === 'undefined' || document.visibilityState !== 'hidden';
  }
  function onVisibility() {
    if (visible() && (!lastRead || Date.now() - lastRead.getTime() >= REFRESH_MS)) void refresh();
  }

  $: empty =
    summary !== null &&
    summary.totals.total === 0 &&
    summary.totals.runs === 0 &&
    summary.totals.modelCalls === 0;
  $: rows = summary ? (view === 'projects' ? summary.perProject : summary.perModel) : [];
  $: largest = Math.max(1, ...rows.map((row) => row.total));
  $: chosen =
    rows.find((row) => selected[view] !== null && keyOf(row) === selected[view]) ?? rows[0];
  $: modes = chosen ? modeShares(chosen) : [];
  $: gaps = summary ? gapParts(summary) : [];
  $: mix = summary ? mixParts(summary) : [];

  function modeShares(row: UsageGroupRow) {
    const sum = row.modes.reduce((total, mode) => total + mode.total, 0);
    return row.modes
      .map((mode) => [modeNames[mode.mode] ?? mode.mode, Math.round((mode.total / sum) * 100)])
      .filter(([, share]) => (share as number) > 0)
      .map(([name, share]) => `${name} ${share}%`);
  }
  /** Calls, tokens in and out, median speed, p90 duration, and failed calls for a model. */
  function modelDetail(row: UsageGroupRow) {
    return [
      plural(row.modelCalls, 'call'),
      `${compact(row.tokens.input)} in / ${compact(row.tokens.output)} out`,
      row.medianOutputSpeed !== null ? `${Math.round(row.medianOutputSpeed)} tok/s` : null,
      row.p90Seconds !== null ? `p90 ${row.p90Seconds.toFixed(1)} s` : null,
      `${row.modelCalls ? percent(row.failedCalls / row.modelCalls) : '0.0%'} errors`,
    ]
      .filter(Boolean)
      .join(' · ');
  }
  function moment(ms: number) {
    return new Date(ms).toLocaleString([], {
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
    });
  }
  function gapParts(summary: UsageSummary) {
    const parts: string[] = [];
    const periods = summary.gaps.offPeriods ?? [];
    const stillOff = periods.some((period) => period.surface === 'desktop' && !period.toUnixMs);
    if (!summary.telemetryEnabled && !stillOff) parts.push('recording is off now');
    for (const period of periods) {
      const who = period.surface === 'cli' ? 'bw recording' : 'recording';
      if (period.fromUnixMs && period.toUnixMs)
        parts.push(`${who} off ${moment(period.fromUnixMs)} – ${moment(period.toUnixMs)}`);
      else if (period.fromUnixMs) parts.push(`${who} off since ${moment(period.fromUnixMs)}`);
      else if (period.toUnixMs) parts.push(`${who} off until ${moment(period.toUnixMs)}`);
    }
    if (summary.gaps.droppedRuns > 0)
      parts.push(`${plural(summary.gaps.droppedRuns, 'run')} dropped (writer queue full)`);
    if (summary.gaps.firstDay)
      parts.push(
        `nothing before ${dayLabel(summary.gaps.firstDay, false)}, when usage tracking started`,
      );
    return parts;
  }
  function mixParts(summary: UsageSummary) {
    const tokens = summary.totals.tokens;
    return [
      ['Input', tokens.input - tokens.cachedInput, 'input'],
      ['Cached', tokens.cachedInput, 'cached'],
      ['Output', tokens.output - tokens.reasoning, 'output'],
      ['Reasoning', tokens.reasoning, 'reasoning'],
    ]
      .map(([label, value, kind]) => ({
        label: label as string,
        value: Math.max(0, value as number),
        kind: kind as string,
      }))
      .filter((part) => part.value > 0);
  }

  onMount(() => {
    if (embedded) dialog.setAttribute('open', '');
    else dialog.showModal();
    // Start on the dialog itself, not the first range option, so no focus ring shows on open.
    dialog.focus();
    if (!desktop) return;
    void refresh();
    timer = setInterval(() => {
      if (visible()) void refresh();
    }, REFRESH_MS);
    document.addEventListener('visibilitychange', onVisibility);
  });
  onDestroy(() => {
    clearInterval(timer);
    if (typeof document !== 'undefined')
      document.removeEventListener('visibilitychange', onVisibility);
  });
</script>

<dialog
  class:embedded
  bind:this={dialog}
  onclose={onClose}
  aria-labelledby="usage-title"
  tabindex="-1"
>
  <header>
    <h2 id="usage-title">Usage</h2>
    <fieldset class="segmented" aria-label="Range">
      {#each ranges as [value, label] (value)}
        <label class:on={days === value}
          ><input
            type="radio"
            name="usage-range"
            {value}
            checked={days === value}
            onchange={() => pick(value)}
          />{label}</label
        >
      {/each}
    </fieldset>
    <span class="spacer"></span>
    <div class="actions">
      <button class="secondary" onclick={exportFiles} disabled={!desktop}>Export…</button>
      <button class="secondary" onclick={refresh} disabled={!desktop || loading}
        ><Icon name="refresh" size={13} /> Refresh</button
      >
      <button class="icon-button" aria-label="Close usage" onclick={() => dialog.close()}
        ><Icon name="x" /></button
      >
    </div>
  </header>
  {#if desktop}
    <div class="fresh">
      <span
        ><span class="dot" class:stale={summary?.stale}>●</span>
        <b
          >{lastRead
            ? `Updated ${lastRead.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}`
            : 'Reading…'}</b
        >
        · auto every 2 min while open
        {#if summary?.stale && summary.newestUnixMs}<span
            class="tag"
            title={`The newest data was recorded ${ago(summary.newestUnixMs)}.`}>stale</span
          >{/if}</span
      >
      <span
        >{new Date().toLocaleDateString([], {
          weekday: 'short',
          day: 'numeric',
          month: 'short',
          year: 'numeric',
        })} · {timeZone}</span
      >
      <span class="spacer"></span>
      <span>metrics/ · {plural(summary?.files ?? 0, 'file')} · OTLP JSON</span>
    </div>
  {/if}
  <div class="body" class:loading>
    {#if !desktop}
      <p class="empty">Usage is available in the installed Blackwall desktop app.</p>
    {:else}
      {#if summary && !summary.telemetryEnabled}
        <p class="banner" role="status">
          Telemetry is off, so new runs are not recorded. Turn on <b>Record agent runs</b> in Runs to
          count them again.
        </p>
      {/if}
      {#if error}<p role="alert" class="error">{error}</p>{/if}
      {#if exported}<p class="note" aria-live="polite">{exported}</p>{/if}
      {#if summary && empty}
        <div class="empty">
          <strong>No usage yet</strong>
          <p>Tokens, runs, and tools appear here after your first Chat or Agent run.</p>
        </div>
      {:else if summary}
        <StatTiles {summary} />
        {#if gaps.length}
          <p class="gaps"><b>Missing from this picture</b> · {gaps.join(' · ')}</p>
        {/if}
        <div class="two">
          <section class="card" aria-labelledby="where-title">
            <div class="card-head">
              <h3 id="where-title">Where it went</h3>
              <fieldset class="segmented small" aria-label="Group by">
                {#each [['projects', 'Projects'], ['models', 'Models']] as [value, label] (value)}
                  <label class:on={view === value}
                    ><input
                      type="radio"
                      name="usage-view"
                      {value}
                      checked={view === value}
                      onchange={() => (view = value as View)}
                    />{label}</label
                  >
                {/each}
              </fieldset>
            </div>
            <ul class="bars">
              {#each rows as row (keyOf(row))}
                <li>
                  <button
                    class="bar-row"
                    class:selected={chosen === row}
                    aria-pressed={chosen === row}
                    onclick={() => (selected = { ...selected, [view]: keyOf(row) })}
                  >
                    <span class="row-head"
                      ><span class="row-name">{nameOf(row)}</span><span class="row-value"
                        >{compact(row.total)}{row.runs !== null
                          ? ` · ${plural(row.runs, 'run')}`
                          : ''}</span
                      ></span
                    >
                    <span class="track"
                      ><i style={`width: ${Math.max(1, (row.total / largest) * 100)}%`}></i></span
                    >
                    {#if view === 'models'}<span class="row-detail">{modelDetail(row)}</span>{/if}
                  </button>
                </li>
              {/each}
            </ul>
            {#if summary.perMode.length}
              <ul class="modes" aria-label="By mode">
                {#each summary.perMode as mode (mode.mode)}
                  <li>
                    <b>{modeNames[mode.mode] ?? mode.mode}</b>
                    {[
                      compact(mode.total),
                      mode.runs > 0 ? plural(mode.runs, 'run') : null,
                      plural(mode.modelCalls, 'call'),
                    ]
                      .filter(Boolean)
                      .join(' · ')}
                  </li>
                {/each}
              </ul>
            {/if}
          </section>
          {#if chosen}
            <section class="card detail" aria-label={nameOf(chosen)}>
              <span class="label">{percent(chosen.share)} of this range</span>
              <h3 class="detail-name">{nameOf(chosen)}</h3>
              <p class="sub">
                {[
                  `${compact(chosen.total)} tokens`,
                  chosen.runs !== null ? plural(chosen.runs, 'run') : null,
                  plural(chosen.sessions, 'session'),
                  ...modes,
                ]
                  .filter(Boolean)
                  .join(' · ')}
              </p>
              <ul class="sessions" aria-label="Sessions">
                {#each chosen.sessionRows as session (session.id)}
                  <li>
                    <code title={session.id}>{session.id}</code>
                    <span>{session.models.join(', ')} · {plural(session.modelCalls, 'call')}</span>
                    <b>{compact(session.total)}</b>
                  </li>
                {:else}
                  <li><span>No session IDs were recorded for this range.</span></li>
                {/each}
              </ul>
              <p class="sub foot">
                {chosen.sessions > chosen.sessionRows.length
                  ? `The ${chosen.sessionRows.length} busiest of ${chosen.sessions} sessions. `
                  : ''}Session IDs are neutral; no titles or prompts are kept.
              </p>
            </section>
          {/if}
        </div>
        <div class="two">
          <section class="card" aria-labelledby="days-title">
            <h3 id="days-title">Tokens per day</h3>
            <TokensChart days={summary.perDay} />
          </section>
          <section class="card">
            <h3 id="mix-title">What the tokens contain</h3>
            <div class="mix" role="img" aria-labelledby="mix-title mix-legend">
              {#each mix as part (part.kind)}
                <i class={part.kind} style={`flex-grow: ${part.value}`}></i>
              {/each}
            </div>
            <ul class="legend" id="mix-legend">
              {#each mix as part (part.kind)}
                <li><i class={`swatch ${part.kind}`}></i>{part.label} {compact(part.value)}</li>
              {/each}
            </ul>
            {#if summary.totals.tokens.cachedInput === 0 && summary.totals.tokens.reasoning === 0}
              <p class="sub">The model server did not report cached or reasoning tokens.</p>
            {/if}
            <h3 class="tools-title">Top tools</h3>
            <UsageTable tools={summary.tools} />
          </section>
        </div>
        {#each summary.warnings as warning (warning)}<p class="sub warning">{warning}</p>{/each}
      {/if}
      {#if summary}<ExportCard refreshed={lastRead} />{/if}
    {/if}
  </div>
</dialog>

<style>
  dialog {
    width: min(1180px, calc(100vw - 32px));
    max-height: calc(100vh - 48px);
    padding: 0;
    margin: auto;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-lg);
    background: var(--bg-panel);
    color: var(--text-primary);
    box-shadow: var(--shadow-menu);
  }
  dialog:focus-visible {
    outline: none;
  }
  dialog::backdrop {
    background: var(--bg-scrim);
    backdrop-filter: blur(3px);
  }
  dialog.embedded {
    display: block;
    width: 100%;
    max-height: none;
    margin: 0;
    border: 0;
    border-radius: 0;
    background: transparent;
    box-shadow: none;
  }
  dialog.embedded header {
    padding: 0 0 20px;
  }
  dialog.embedded header .icon-button {
    display: none;
  }
  header {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px 14px;
    padding: 16px 22px;
    border-bottom: 1px solid var(--border);
  }
  h2 {
    margin: 0;
    font-size: 17px;
    font-weight: 600;
  }
  h3 {
    margin: 0 0 10px;
    font-size: 14px;
    font-weight: 600;
  }
  .spacer {
    flex: 1;
  }
  .actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .segmented {
    display: flex;
    margin: 0;
    padding: 0;
    border: 1px solid var(--border);
    border-radius: var(--radius-pill);
    overflow: hidden;
  }
  .segmented label {
    padding: 4px 12px;
    color: var(--text-muted);
    font-size: 12.5px;
    cursor: pointer;
    white-space: nowrap;
  }
  .segmented label.on {
    background: var(--accent-muted);
    color: var(--accent);
  }
  .segmented label:has(input:focus-visible) {
    outline: 2px solid var(--focus-ring);
    outline-offset: -2px;
  }
  .segmented input {
    position: absolute;
    opacity: 0;
    width: 1px;
    height: 1px;
  }
  .segmented.small label {
    padding: 3px 11px;
  }
  .secondary {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 4px 11px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-muted);
    font-size: 12.5px;
  }
  .icon-button {
    background: transparent;
    color: var(--text-muted);
    padding: 4px;
  }
  .fresh {
    display: flex;
    flex-wrap: wrap;
    gap: 4px 18px;
    padding: 8px 22px;
    font: 11.5px var(--font-code);
    color: var(--text-faint);
    border-bottom: 1px solid var(--border);
  }
  .fresh b {
    color: var(--text-muted);
    font-weight: 500;
  }
  .dot {
    color: var(--ok);
  }
  .dot.stale {
    color: var(--warn);
  }
  .tag {
    margin-left: 6px;
    padding: 0 7px;
    border: 1px solid var(--warn);
    border-radius: var(--radius-pill);
    color: var(--warn);
  }
  .body {
    display: grid;
    gap: 16px;
    padding: 18px 22px 22px;
    transition: opacity var(--transition-fast);
  }
  .body.loading {
    opacity: 0.6;
  }
  .card {
    min-width: 0;
    padding: 14px 16px;
    background: var(--bg-elevated);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
  }
  .card-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    margin-bottom: 4px;
  }
  .card-head h3 {
    margin: 0;
  }
  .two {
    display: grid;
    grid-template-columns: 1.25fr 1fr;
    gap: 12px;
  }
  .label {
    font: 11px var(--font-code);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--text-faint);
  }
  .sub {
    margin: 0;
    font-size: 12px;
    color: var(--text-muted);
    overflow-wrap: anywhere;
  }
  .gaps {
    margin: 0;
    padding: 8px 12px;
    font-size: 12px;
    color: var(--text-muted);
    border: 1px dashed var(--border-strong);
    border-radius: var(--radius-sm);
  }
  .gaps b {
    color: var(--warn);
    font-weight: 500;
  }
  .banner {
    margin: 0;
    padding: 10px 12px;
    font-size: 12.5px;
    color: var(--text-primary);
    background: var(--bg-elevated);
    border: 1px solid var(--warn);
    border-radius: var(--radius-sm);
  }
  .bars {
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .bar-row {
    display: block;
    width: calc(100% + 16px);
    padding: 8px;
    margin: 0 -8px;
    text-align: left;
    background: transparent;
    border-radius: var(--radius-sm);
  }
  .bars li + li .bar-row {
    border-top: 1px solid var(--border);
    border-radius: 0;
  }
  .bar-row.selected,
  .bars li + li .bar-row.selected {
    background: var(--bg-hover);
    border-radius: var(--radius-sm);
  }
  .row-head {
    display: flex;
    justify-content: space-between;
    gap: 10px;
  }
  .row-name {
    font-weight: 500;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .row-value {
    flex: none;
    font: 12px var(--font-code);
    color: var(--text-muted);
  }
  .row-detail {
    display: block;
    margin-top: 5px;
    font: 11.5px var(--font-code);
    color: var(--text-faint);
    overflow-wrap: anywhere;
  }
  .modes {
    display: flex;
    flex-wrap: wrap;
    gap: 4px 18px;
    margin: 10px 0 0;
    padding: 10px 0 0;
    list-style: none;
    border-top: 1px solid var(--border);
    font-size: 12px;
    color: var(--text-muted);
  }
  .modes b {
    color: var(--text-primary);
    font-weight: 500;
    margin-right: 4px;
  }
  .track {
    display: block;
    height: 5px;
    margin-top: 6px;
    background: var(--border);
    border-radius: 3px;
  }
  .track i {
    display: block;
    height: 100%;
    border-radius: 3px;
    background: var(--text-muted);
  }
  .bar-row.selected .track i {
    background: var(--accent);
  }
  .detail {
    background: var(--subagent-bg);
    border-color: var(--accent-muted);
  }
  .detail-name {
    margin: 4px 0 2px;
    font-size: 20px;
    overflow-wrap: anywhere;
  }
  .sessions {
    margin: 10px 0 0;
    padding: 0;
    list-style: none;
  }
  .sessions li {
    display: grid;
    grid-template-columns: minmax(0, 1.1fr) minmax(0, 1.3fr) auto;
    gap: 10px;
    padding: 6px 0;
    border-bottom: 1px solid var(--accent-muted);
    font-size: 12.5px;
  }
  .sessions code {
    font: 12px var(--font-code);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .sessions span {
    color: var(--text-muted);
    overflow-wrap: anywhere;
  }
  .sessions b {
    font-variant-numeric: tabular-nums;
  }
  .foot {
    margin-top: 8px;
  }
  .mix {
    display: flex;
    gap: 2px;
    height: 12px;
    margin: 8px 0;
    border-radius: 6px;
    overflow: hidden;
  }
  .mix i {
    flex-basis: 0;
    min-width: 2px;
  }
  .input {
    background: var(--text-muted);
  }
  .cached {
    background: var(--text-faint);
  }
  .output {
    background: var(--accent);
  }
  .reasoning {
    background: color-mix(in srgb, var(--accent) 55%, var(--bg-elevated));
  }
  .legend {
    display: flex;
    flex-wrap: wrap;
    gap: 4px 16px;
    margin: 0 0 4px;
    padding: 0;
    list-style: none;
    font: 11.5px var(--font-code);
    color: var(--text-muted);
  }
  .swatch {
    display: inline-block;
    width: 8px;
    height: 8px;
    margin-right: 5px;
    border-radius: 2px;
  }
  .tools-title {
    margin-top: 16px;
  }
  .empty {
    padding: 32px 0;
    text-align: center;
    color: var(--text-muted);
  }
  .empty strong {
    display: block;
    color: var(--text-primary);
    font-size: 15px;
    margin-bottom: 4px;
  }
  .empty p {
    margin: 0;
    font-size: 12.5px;
  }
  .error {
    margin: 0;
    font-size: 12px;
    color: var(--err);
  }
  .note {
    margin: 0;
    font-size: 12px;
    color: var(--text-muted);
    overflow-wrap: anywhere;
  }
  .warning {
    color: var(--warn);
  }
  @media (max-width: 760px) {
    header,
    .fresh {
      padding-left: 16px;
      padding-right: 16px;
    }
    .body {
      padding: 14px 16px 18px;
    }
    .two {
      grid-template-columns: 1fr;
    }
    .actions {
      width: 100%;
    }
    .actions .icon-button {
      margin-left: auto;
    }
  }
</style>
