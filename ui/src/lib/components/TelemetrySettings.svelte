<script lang="ts">
  import { onDestroy } from 'svelte';
  import {
    telemetryClient,
    type ExportSettings,
    type Retention,
    type TelemetryPreferences,
  } from '../persistence';
  import { platform } from '../platform';

  export let client = telemetryClient;

  const blank: ExportSettings = {
    endpoint: null,
    protocol: 'http/protobuf',
    traces: true,
    metrics: true,
    headers: [],
    includeProject: false,
  };
  let saved: TelemetryPreferences | null = null;
  let endpoint = '';
  let protocol: ExportSettings['protocol'] = 'http/protobuf';
  let traces = true;
  let metrics = true;
  let includeProject = false;
  let headers: string[] = [];
  let forever = false;
  let days = 400;
  let headerName = '';
  let headerValue = '';
  let busy = false;
  let error = '';
  let feedback = '';
  let testResult = '';
  let testFailed = false;
  let closed = false;

  function message(cause: unknown, fallback: string) {
    return cause instanceof Error ? cause.message : typeof cause === 'string' ? cause : fallback;
  }
  function apply(settings: TelemetryPreferences) {
    saved = settings;
    const exported = { ...blank, ...settings.export };
    endpoint = exported.endpoint ?? '';
    protocol = exported.protocol;
    traces = exported.traces;
    metrics = exported.metrics;
    includeProject = exported.includeProject;
    headers = [...exported.headers];
    forever = settings.retention === 'forever';
    days = settings.retention === 'forever' ? 400 : settings.retention;
  }
  function draft(): ExportSettings {
    return {
      endpoint: endpoint.trim() || null,
      protocol,
      traces,
      metrics,
      headers,
      includeProject,
    };
  }
  function retention(): Retention {
    return forever ? 'forever' : Math.round(days);
  }
  async function load() {
    try {
      const settings = await client.load();
      if (!closed) apply(settings);
    } catch (cause) {
      if (!closed) error = message(cause, 'Telemetry settings could not be read.');
    }
  }
  async function persist(settings: TelemetryPreferences, done: string) {
    await client.save(settings);
    if (closed) return;
    saved = settings;
    feedback = done;
  }
  async function save() {
    if (busy) return;
    error = '';
    feedback = '';
    if (!forever && !(Number.isInteger(days) && days >= 30 && days <= 3650)) {
      error = 'Keep usage history for 30 to 3,650 days, or forever.';
      return;
    }
    if (endpoint.trim() && !traces && !metrics) {
      error = 'Choose traces, metrics, or both, or clear the endpoint to turn export off.';
      return;
    }
    busy = true;
    try {
      await persist(
        { export: draft(), retention: retention() },
        endpoint.trim() ? 'Saved. New runs are exported.' : 'Saved. Export is off.',
      );
    } catch (cause) {
      if (!closed) error = message(cause, 'Telemetry settings could not be saved.');
    } finally {
      busy = false;
    }
  }
  /** Header changes are saved at once, on top of the last saved settings. */
  async function addHeader() {
    if (busy || !headerName.trim() || !headerValue.trim()) return;
    busy = true;
    error = '';
    feedback = '';
    try {
      const name = await client.saveHeader(headerName.trim(), headerValue);
      headerValue = '';
      headerName = '';
      if (closed) return;
      const base = saved ?? { export: blank, retention: 400 };
      const names = base.export.headers.includes(name)
        ? base.export.headers
        : [...base.export.headers, name];
      await persist(
        { ...base, export: { ...base.export, headers: names } },
        `Saved the ${name} header in ${platform.store}.`,
      );
      if (!headers.includes(name)) headers = [...headers, name];
    } catch (cause) {
      if (!closed) error = message(cause, `${platform.store} could not be updated. Try again.`);
    } finally {
      busy = false;
    }
  }
  async function removeHeader(name: string) {
    if (busy) return;
    busy = true;
    error = '';
    feedback = '';
    try {
      await client.deleteHeader(name);
      const base = saved ?? { export: blank, retention: 400 };
      await persist(
        {
          ...base,
          export: { ...base.export, headers: base.export.headers.filter((h) => h !== name) },
        },
        `Removed the ${name} header.`,
      );
      headers = headers.filter((header) => header !== name);
    } catch (cause) {
      if (!closed) error = message(cause, `${platform.store} could not be updated. Try again.`);
    } finally {
      busy = false;
    }
  }
  async function sendTest() {
    testResult = '';
    testFailed = false;
    busy = true;
    try {
      const status = await client.test(draft());
      if (!closed) testResult = `Test span sent · HTTP ${status}`;
    } catch (cause) {
      if (!closed) {
        testFailed = true;
        testResult = message(cause, 'The test span could not be sent.');
      }
    } finally {
      busy = false;
    }
  }

  void load();
  onDestroy(() => {
    closed = true;
    headerValue = '';
  });
</script>

<section class="telemetry" aria-label="Telemetry">
  <h4>Export to an OpenTelemetry collector</h4>
  <p>
    Sends the same traces and usage metrics that are saved on this device, over OTLP/HTTP. Off until
    you set an endpoint. Prompts and outputs are included only when <b>Include content</b>
    is on in Runs.
  </p>
  <form
    novalidate
    onsubmit={(event) => {
      event.preventDefault();
      void save();
    }}
  >
    <label
      >Endpoint
      <input
        type="url"
        bind:value={endpoint}
        placeholder="https://otel.example.com or http://127.0.0.1:4318"
        spellcheck="false"
        autocomplete="off"
        maxlength={2048}
        disabled={busy}
      />
    </label>
    <p class="hint">
      The base URL: <code>/v1/traces</code> and <code>/v1/metrics</code> are added. Plain http only for
      127.0.0.1, ::1, or localhost. Redirects are not followed.
    </p>
    <div class="row">
      <label class="inline"
        >Protocol
        <select bind:value={protocol} disabled={busy}>
          <option value="http/protobuf">HTTP/protobuf</option>
          <option value="http/json">HTTP/JSON</option>
        </select>
      </label>
      <fieldset class="signals">
        <legend>Send</legend>
        <label class="check"
          ><input type="checkbox" bind:checked={traces} disabled={busy} /> Traces</label
        >
        <label class="check"
          ><input type="checkbox" bind:checked={metrics} disabled={busy} /> Metrics</label
        >
      </fieldset>
    </div>
    <label class="check"
      ><input type="checkbox" bind:checked={includeProject} disabled={busy} /> Include project names and
      session IDs in exported metrics</label
    >
    <p class="hint">Off by default: they stay in the local files and on the Usage page.</p>

    <h4>Usage history</h4>
    <div class="row">
      <label class="inline"
        >Keep
        <input
          class="days"
          type="number"
          min="30"
          max="3650"
          bind:value={days}
          disabled={busy || forever}
        />
        days</label
      >
      <label class="check"
        ><input type="checkbox" bind:checked={forever} disabled={busy} /> Forever</label
      >
    </div>
    <p class="hint">Older metrics files are deleted when the app starts. Default 400 days.</p>
    <div class="actions">
      <button type="submit" class="primary" disabled={busy}>Save</button>
      <button type="button" onclick={sendTest} disabled={busy || !endpoint.trim()}
        >Send test span</button
      >
    </div>
  </form>
  {#if testResult}<p class="result" class:bad={testFailed} role="status">{testResult}</p>{/if}

  <h4>Headers</h4>
  <p>
    Such as an API key. Values are saved only in {platform.store} and are never shown again.
  </p>
  {#if headers.length}
    <ul class="headers" aria-label="Saved headers">
      {#each headers as name (name)}
        <li>
          <code>{name}</code><span class="masked" aria-label="Value hidden">••••••••</span>
          <button
            type="button"
            class="subtle"
            aria-label={`Remove ${name}`}
            onclick={() => removeHeader(name)}
            disabled={busy}>Remove</button
          >
        </li>
      {/each}
    </ul>
  {/if}
  <form
    class="add"
    onsubmit={(event) => {
      event.preventDefault();
      void addHeader();
    }}
  >
    <label
      >Header name
      <input
        bind:value={headerName}
        placeholder="x-api-key"
        autocomplete="off"
        maxlength={64}
        disabled={busy}
      />
    </label>
    <label
      >Header value
      <input
        type="password"
        bind:value={headerValue}
        autocomplete="new-password"
        maxlength={8192}
        disabled={busy}
      />
    </label>
    <button type="submit" disabled={busy || !headerName.trim() || !headerValue.trim()}
      >Add header</button
    >
  </form>
  {#if error}<p class="error" role="alert">{error}</p>{/if}
  {#if feedback}<p class="feedback" role="status">{feedback}</p>{/if}
</section>

<style>
  section {
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 14px;
    margin-top: 12px;
  }
  h4 {
    margin: 14px 0 4px;
    font-size: 12.5px;
    font-weight: 600;
  }
  h4:first-child {
    margin-top: 0;
  }
  p {
    margin: 0 0 8px;
    color: var(--text-muted);
    font-size: 12px;
    line-height: 1.6;
  }
  .hint {
    margin-top: -2px;
    color: var(--text-faint);
  }
  form,
  label {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  label {
    font-size: 12px;
    color: var(--text-muted);
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    align-items: flex-end;
    gap: 10px 22px;
  }
  .inline {
    flex-direction: row;
    align-items: center;
  }
  .check {
    flex-direction: row;
    align-items: center;
    gap: 6px;
  }
  fieldset {
    display: flex;
    gap: 14px;
    margin: 0;
    padding: 0;
    border: 0;
  }
  legend {
    float: left;
    margin-right: 10px;
    font-size: 12px;
    color: var(--text-muted);
  }
  input:not([type='checkbox']),
  select {
    width: 100%;
    min-width: 0;
    padding: 9px 10px;
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    background: var(--bg-elevated);
    color: var(--text-primary);
  }
  select,
  .days {
    width: auto;
  }
  .days {
    max-width: 90px;
  }
  code {
    font-family: var(--font-mono);
    font-size: 11.5px;
  }
  button {
    align-self: flex-start;
    padding: 9px 12px;
    border-radius: 6px;
    background: var(--bg-hover);
    color: var(--text-primary);
    font-size: 12px;
  }
  .primary {
    background: var(--accent);
    color: var(--text-on-accent);
  }
  button:disabled {
    opacity: 0.5;
  }
  .subtle {
    margin-left: auto;
    padding: 4px 6px;
    background: transparent;
    color: var(--err);
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }
  .headers {
    margin: 0 0 10px;
    padding: 0;
    list-style: none;
  }
  .headers li {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 6px 0;
    border-bottom: 1px solid var(--border);
  }
  .masked {
    color: var(--text-faint);
    letter-spacing: 0.1em;
  }
  .add {
    padding-top: 4px;
  }
  .error,
  .result.bad {
    color: var(--err);
  }
  .feedback,
  .result {
    color: var(--ok);
  }
</style>
