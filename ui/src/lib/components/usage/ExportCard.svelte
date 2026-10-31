<script lang="ts">
  import { telemetryClient, type ExportStatus } from '../../persistence';
  import { ago, exportError, plural } from './format';

  export let client = telemetryClient;
  /** Changes when the Usage page re-reads its data, so the card refreshes with it. */
  export let refreshed: unknown = null;

  let status: ExportStatus | null = null;
  let unavailable = false;
  let testing = false;
  let result = '';
  let failed = false;
  let request = 0;

  async function refresh() {
    const current = ++request;
    try {
      const next = await client.status();
      if (current !== request) return;
      status = next;
      unavailable = false;
    } catch {
      if (current === request) unavailable = true;
    }
  }
  async function sendTest() {
    testing = true;
    result = '';
    failed = false;
    try {
      const code = await client.test();
      result = `Test span sent · HTTP ${code}`;
    } catch (cause) {
      failed = true;
      result = typeof cause === 'string' ? cause : 'The test span could not be sent.';
    } finally {
      testing = false;
      void refresh();
    }
  }

  function reload(_: unknown) {
    void refresh();
  }
  $: reload(refreshed);
  $: signals = status
    ? [status.traces ? 'traces' : null, status.metrics ? 'metrics' : null]
        .filter(Boolean)
        .join(', ')
    : '';
</script>

<section class="card export" aria-labelledby="export-title">
  <div class="card-head">
    <h3 id="export-title">Export</h3>
    {#if status?.endpoint}
      <button class="secondary" onclick={sendTest} disabled={testing}
        >{testing ? 'Sending…' : 'Send test span'}</button
      >
    {/if}
  </div>
  {#if unavailable}
    <p class="sub">Export status is unavailable.</p>
  {:else if !status}
    <p class="sub">Reading…</p>
  {:else if !status.endpoint}
    <p class="sub">
      Off. Send these traces and metrics to an OpenTelemetry collector from Settings → Privacy &amp;
      security → Telemetry.
    </p>
  {:else}
    <p class="endpoint"><code>{status.endpoint}</code></p>
    <dl>
      <div>
        <dt>Sends</dt>
        <dd>{signals || 'nothing'} · {status.protocol}</dd>
      </div>
      <div>
        <dt>Last sent</dt>
        <dd>{status.lastSuccessUnixMs ? ago(status.lastSuccessUnixMs) : 'nothing sent yet'}</dd>
      </div>
      <div>
        <dt>Last error</dt>
        <dd class:bad={status.lastError}>
          {status.lastError
            ? `${exportError(status.lastError.kind, status.lastError.status)} · ${ago(
                status.lastError.unixMs,
              )}`
            : 'none'}
        </dd>
      </div>
      <div>
        <dt>Counts</dt>
        <dd>
          {plural(status.sentSpans, 'span')} and {plural(
            status.sentMetrics,
            'metric batch',
            'metric batches',
          )}
          sent{status.droppedSpans || status.droppedMetrics
            ? ` · ${plural(status.droppedSpans, 'span')} and ${plural(
                status.droppedMetrics,
                'metric batch',
                'metric batches',
              )} dropped`
            : ''}
        </dd>
      </div>
    </dl>
    <p class="sub">Since the app started. Export never slows or stops a run.</p>
  {/if}
  {#if result}<p class="result" class:bad={failed} role="status">{result}</p>{/if}
</section>

<style>
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
    margin-bottom: 8px;
  }
  h3 {
    margin: 0;
    font-size: 14px;
    font-weight: 600;
  }
  .secondary {
    padding: 4px 11px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-muted);
    font-size: 12.5px;
  }
  .secondary:disabled {
    opacity: 0.6;
  }
  .endpoint {
    margin: 0 0 8px;
    overflow-wrap: anywhere;
  }
  code {
    font: 12px var(--font-code);
  }
  dl {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
    gap: 8px 18px;
    margin: 0 0 8px;
  }
  dt {
    font: 11px var(--font-code);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--text-faint);
  }
  dd {
    margin: 2px 0 0;
    font-size: 12.5px;
    color: var(--text-primary);
    overflow-wrap: anywhere;
  }
  .sub,
  .result {
    margin: 0;
    font-size: 12px;
    color: var(--text-muted);
  }
  .result {
    margin-top: 6px;
    color: var(--ok);
  }
  .bad,
  .result.bad {
    color: var(--err);
  }
</style>
