<script lang="ts">
  import { onMount } from 'svelte';
  import { persistence, type TraceSpan, type TraceSummary } from '../persistence';
  import { relativeTime } from '../format';
  import Icon from './Icon.svelte';
  import { platform } from '../platform';
  export let onClose: () => void;
  export let embedded = false;
  let dialog: HTMLDialogElement;
  let traces: TraceSummary[] = [];
  let open: TraceSummary | null = null;
  let rows: { span: TraceSpan; depth: number }[] = [];
  let enabled = true;
  let content = false;
  let saved = { enabled, content };
  let loaded = false;
  let busy = false;
  let error = '';
  const desktop = persistence.available();
  const contentKeys = [
    ['gen_ai.tool.call.arguments', 'Arguments'],
    ['gen_ai.tool.call.result', 'Result'],
    ['gen_ai.output.messages', 'Answer'],
  ] as const;

  function seconds(ms: number) {
    return `${(ms / 1000).toFixed(1)}s`;
  }
  function plural(count: number, word: string) {
    return `${count} ${word}${count === 1 ? '' : 's'}`;
  }
  function tokens(span: TraceSpan) {
    const input = span.attributes['gen_ai.usage.input_tokens'];
    const output = span.attributes['gen_ai.usage.output_tokens'];
    return typeof input === 'number' && typeof output === 'number'
      ? `${input} in / ${output} out tokens`
      : '';
  }
  function text(span: TraceSpan, key: string) {
    const value = span.attributes[key];
    if (typeof value !== 'string') return '';
    if (key !== 'gen_ai.output.messages') return value;
    // Answers are stored as OpenTelemetry output messages; show their text.
    try {
      const messages = JSON.parse(value) as { parts?: { content?: unknown }[] }[];
      return messages
        .flatMap((message) => message.parts ?? [])
        .map((part) => (typeof part.content === 'string' ? part.content : ''))
        .join('\n');
    } catch {
      return value;
    }
  }
  /** Children follow their parent in start order, so the list reads as a tree. */
  function tree(spans: TraceSpan[]) {
    const ordered: { span: TraceSpan; depth: number }[] = [];
    const placed = new Set<TraceSpan>();
    const expanded = new Set<string>();
    const byStart = [...spans].sort((a, b) => a.startMs - b.startMs);
    const add = (span: TraceSpan, depth: number) => {
      placed.add(span);
      ordered.push({ span, depth });
      // Each ID's children are listed once, so repeated IDs or cycles cannot loop.
      if (expanded.has(span.id)) return;
      expanded.add(span.id);
      for (const child of byStart.filter((child) => child.parent === span.id && !placed.has(child)))
        add(child, depth + 1);
    };
    for (const span of byStart) if (span.parent === null && !placed.has(span)) add(span, 0);
    // Spans whose parent is missing are shown at the top level rather than dropped.
    for (const span of byStart) if (!placed.has(span)) add(span, 0);
    return ordered;
  }
  async function load() {
    traces = await persistence.traces();
  }
  let opening = 0;
  async function show(trace: TraceSummary) {
    error = '';
    const request = ++opening;
    try {
      const spans = await persistence.trace(trace.id);
      // Only the run opened last is shown.
      if (request !== opening) return;
      rows = tree(spans);
      open = trace;
    } catch (cause) {
      if (request === opening)
        error = typeof cause === 'string' ? cause : 'This run could not be opened.';
    }
  }
  async function saveSettings() {
    busy = true;
    error = '';
    try {
      await persistence.savePreferences({ tracesEnabled: enabled, traceContent: content });
      saved = { enabled, content };
    } catch {
      ({ enabled, content } = saved);
      error = 'These settings could not be saved. Try again.';
    } finally {
      busy = false;
    }
  }
  onMount(() => {
    if (embedded) dialog.setAttribute('open', '');
    else dialog.showModal();
    if (!desktop) return;
    void (async () => {
      try {
        const preferences = await persistence.preferences();
        enabled = preferences.tracesEnabled ?? true;
        content = preferences.traceContent ?? false;
        saved = { enabled, content };
        loaded = true;
        await load();
      } catch {
        error = 'Your runs could not be opened.';
      }
    })();
  });
</script>

<dialog class:embedded bind:this={dialog} onclose={onClose} aria-labelledby="runs-title">
  <header>
    <div>
      <h2 id="runs-title">Runs</h2>
      <p>What each agent run did: model calls, tools, tokens, and failures.</p>
    </div>
    <button class="icon-button" aria-label="Close runs" onclick={() => dialog.close()}
      ><Icon name="x" /></button
    >
  </header>
  <div class="content">
    {#if !desktop}<p>Runs are available in the installed Blackwall desktop app.</p>
    {:else if open}
      <div class="intro">
        <button class="secondary" onclick={() => (open = null)}>All runs</button>
        <span
          >{new Date(open.startedMs).toLocaleString()} · {seconds(open.durationMs)}{open.model
            ? ` · ${open.model}`
            : ''}</span
        >
      </div>
      <ol class="tree" aria-label="Run steps">
        {#each rows as { span, depth }, index (index)}
          <li style={`--depth: ${depth}`} aria-level={depth + 1} class:failed={span.error}>
            <div class="step">
              <span class="status" role="img" aria-label={span.error ? 'Failed' : 'Succeeded'}
                >{span.error ? '✗' : '✓'}</span
              >
              <strong>{span.name}</strong>
              <span class="meta"
                >{[
                  seconds(span.durationMs),
                  tokens(span),
                  ...span.events.map((e) => e.replace(/_/g, ' ')),
                ]
                  .filter(Boolean)
                  .join(' · ')}</span
              >
            </div>
            {#if span.error}<p class="span-error">{span.error}</p>{/if}
            {#each contentKeys as [key, label]}
              {#if text(span, key)}<details>
                  <summary>{label}</summary>
                  <pre>{text(span, key)}</pre>
                </details>{/if}
            {/each}
          </li>
        {/each}
      </ol>
    {:else}
      <div class="preference">
        <div>
          <strong>Record agent runs</strong>
          <p>
            Keeps a local trace of the newest 200 agent runs on this {platform.device}. Traces are
            never sent anywhere. The CLI uses <code>bw config set traces</code>.
          </p>
        </div>
        <input
          aria-label="Record agent runs"
          type="checkbox"
          bind:checked={enabled}
          disabled={busy || !loaded}
          onchange={saveSettings}
        />
      </div>
      <div class="preference">
        <div>
          <strong>Include content</strong>
          <p>
            Also records tool arguments, tool results, and model answers, with secrets redacted
            where they are recognized. They can still hold private file contents, so turn this on
            only while looking into a run.
          </p>
        </div>
        <input
          aria-label="Include content"
          type="checkbox"
          bind:checked={content}
          disabled={busy || !loaded || !enabled}
          onchange={saveSettings}
        />
      </div>
      {#if loaded && traces.length === 0}<p class="empty">
          No runs yet. Agent tasks appear here after they finish.
        </p>{/if}
      {#each traces as trace (trace.id)}<button
          class="run"
          title={new Date(trace.startedMs).toLocaleString()}
          onclick={() => show(trace)}
          ><span class="when">{relativeTime(trace.startedMs)}</span>
          <span class="summary"
            ><strong
              >{plural(trace.modelCalls, 'model call')} · {plural(
                trace.toolCalls,
                'tool call',
              )}</strong
            >
            <small
              >{seconds(trace.durationMs)} · {trace.inputTokens} in / {trace.outputTokens} out tokens{trace.model
                ? ` · ${trace.model}`
                : ''}</small
            >
            {#if trace.error}<small class="failed">Failed: {trace.error}</small>{/if}</span
          ></button
        >{/each}
    {/if}
    {#if error}<p role="alert" class="error">{error}</p>{/if}
  </div>
</dialog>

<style>
  dialog {
    width: min(720px, calc(100vw - 32px));
    max-height: calc(100vh - 48px);
    padding: 0;
    margin: auto;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-lg);
    background: var(--bg-panel);
    color: var(--text-primary);
    box-shadow: var(--shadow-menu);
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
  dialog.embedded .content {
    padding: 0;
  }
  header {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 15px;
    padding: 24px;
    border-bottom: 1px solid var(--border);
  }
  h2 {
    margin: 0 0 5px;
    font-size: 18px;
    font-weight: 620;
  }
  header p,
  .preference p {
    font-size: 12px;
    color: var(--text-muted);
    line-height: 1.6;
    margin: 0;
  }
  .preference p {
    margin-top: 5px;
  }
  .content {
    padding: 24px;
  }
  .icon-button {
    background: transparent;
    color: var(--text-muted);
    padding: 4px;
  }
  .preference {
    display: flex;
    align-items: center;
    gap: 20px;
    margin-bottom: 20px;
  }
  .preference div {
    flex: 1;
  }
  strong {
    font-size: 13px;
    font-weight: 570;
  }
  input[type='checkbox'] {
    accent-color: var(--accent);
    width: 17px;
    height: 17px;
  }
  .intro {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-bottom: 18px;
  }
  .intro > span {
    color: var(--text-muted);
    font-size: 12px;
  }
  .secondary {
    padding: 8px 12px;
    border-radius: var(--radius-sm);
    font-size: 11.5px;
    background: var(--bg-elevated);
    border: 1px solid var(--border);
  }
  .run {
    display: flex;
    gap: 15px;
    align-items: flex-start;
    width: 100%;
    text-align: left;
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 14px 16px;
    margin: 10px 0;
    background: transparent;
  }
  .when {
    min-width: 34px;
    color: var(--text-muted);
    font-size: 12px;
  }
  .summary {
    display: flex;
    flex-direction: column;
    gap: 4px;
    min-width: 0;
  }
  small {
    color: var(--text-muted);
    font-size: 12px;
    overflow-wrap: anywhere;
  }
  .failed,
  .span-error,
  .error {
    color: var(--err);
  }
  .tree {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .tree li {
    margin-left: calc(var(--depth) * 20px);
    padding: 9px 0;
    border-bottom: 1px solid var(--border);
  }
  .step {
    display: flex;
    align-items: baseline;
    gap: 8px;
    flex-wrap: wrap;
  }
  .status {
    color: var(--ok);
    width: 14px;
  }
  .tree li.failed .status {
    color: var(--err);
  }
  .meta {
    color: var(--text-muted);
    font-size: 12px;
  }
  .span-error {
    font-size: 12px;
    margin: 6px 0 0 22px;
    overflow-wrap: anywhere;
  }
  details {
    margin: 6px 0 0 22px;
    font-size: 12px;
  }
  summary {
    color: var(--text-muted);
    cursor: pointer;
  }
  pre {
    margin: 6px 0 0;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    max-height: 240px;
    overflow: auto;
    font-size: 12px;
    background: var(--bg-base);
    padding: 10px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
  }
  .empty {
    color: var(--text-muted);
    font-size: 12px;
    padding: 20px 0;
  }
  .error {
    font-size: 12px;
  }
</style>
