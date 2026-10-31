<script lang="ts">
  import { onMount } from 'svelte';
  import {
    persistence,
    type CommandToolsIsolation,
    type OwnerRule,
    type OwnerRules,
    type PolicyAuditEntry,
    type RuleVerdict,
  } from '../persistence';

  export let isolation: CommandToolsIsolation = 'outside';

  type Kind = 'exec' | 'file' | 'network';
  type Field = 'command' | 'path' | 'host' | 'tool';
  const FIELDS: Record<Kind, Field> = { exec: 'command', file: 'path', network: 'host' };
  const LABELS: Record<Field, string> = {
    command: 'Command',
    path: 'Path',
    host: 'Host',
    tool: 'Tool',
  };

  let saved: OwnerRules | null = null;
  let audit: PolicyAuditEntry[] = [];
  let loaded = false;
  let busy = false;
  let loadError = '';
  let error = '';
  /** `index` is the rule being edited, or `null` for a new one. */
  let draft: {
    index: number | null;
    decision: OwnerRule['decision'];
    kind: Kind;
    field: Field;
    value: string;
  } | null = null;
  let testKind: Kind = 'exec';
  let testValue = '';
  let verdict: RuleVerdict | null = null;

  $: rules = saved?.rules ?? [];

  function message(cause: unknown, fallback: string) {
    return cause instanceof Error ? cause.message : typeof cause === 'string' ? cause : fallback;
  }
  function subject(rule: OwnerRule) {
    return rule.command ?? rule.path ?? rule.host ?? rule.tool ?? '';
  }
  function describe(rule: OwnerRule) {
    return `${rule.decision} ${rule.kind} ${subject(rule)}${rule.project ? ` (project ${rule.project})` : ''}`;
  }
  async function loadRules() {
    try {
      saved = await persistence.ownerRules();
      loadError = '';
    } catch (cause) {
      loadError = message(cause, 'The rules could not be loaded.');
    }
  }
  async function load() {
    await loadRules();
    try {
      audit = await persistence.policyAudit();
    } catch (cause) {
      error = message(cause, 'The audit log could not be loaded.');
    } finally {
      loaded = true;
    }
  }
  async function save(next: OwnerRule[]) {
    busy = true;
    error = '';
    try {
      await persistence.saveOwnerRules(saved ?? { rules }, next);
      draft = null;
    } catch (cause) {
      error = message(cause, 'The rules could not be saved.');
    } finally {
      const shown = JSON.stringify(saved);
      await loadRules();
      // The editor's index would point at another rule.
      if (JSON.stringify(saved) !== shown) draft = null;
      busy = false;
    }
  }
  function add() {
    draft = { index: null, decision: 'ask', kind: 'exec', field: 'command', value: '' };
  }
  function edit(index: number) {
    const rule = rules[index];
    const field = (['command', 'path', 'host', 'tool'] as const).find(
      (name) => rule[name] !== undefined,
    );
    draft = {
      index,
      decision: rule.decision,
      kind: rule.kind as Kind,
      field: field ?? 'command',
      value: field ? (rule[field] ?? '') : '',
    };
  }
  function submit() {
    if (!draft) return;
    const { index, decision, kind, value } = draft;
    const rule: OwnerRule =
      index === null
        ? { decision, kind, [FIELDS[kind]]: value }
        : { ...rules[index], decision, [draft.field]: value };
    save(index === null ? [...rules, rule] : rules.map((item, at) => (at === index ? rule : item)));
  }
  async function test() {
    error = '';
    try {
      verdict = await persistence.testOwnerRule(testKind, testValue);
    } catch (cause) {
      verdict = null;
      error = message(cause, 'The rule could not be tested.');
    }
  }
  function verdictText(result: RuleVerdict) {
    const rule = result.matched ? rules[result.matched.index] : undefined;
    return result.matched
      ? `${result.decision} by rule ${result.matched.index + 1}${rule ? `: ${describe(rule)}` : ''}`
      : `${result.decision}, no rule matched${result.projectOnly ? ' outside a project: a rule limited to a project would match' : ''}`;
  }
  onMount(load);
</script>

<p>
  Your own allow, ask and deny rules from <code>policy.json</code>. Command tools run
  {isolation === 'sandboxed' ? 'inside the Commands profile' : 'outside the sandbox'}.
</p>
{#if error}<p class="error" role="alert">{error}</p>{/if}
{#if loadError}
  <p class="error" role="alert">{loadError}</p>
{:else if loaded}
  <p>
    {saved?.mode === 'ordered'
      ? 'Ordered: the first matching rule decides.'
      : 'Strictest: deny outranks ask, and ask outranks allow.'}
  </p>
  {#if !rules.length}<p>No rules.</p>{/if}
  <ol>
    {#each rules as rule, index}
      <li>
        <span>{describe(rule)}</span>
        <button
          onclick={() => edit(index)}
          disabled={busy}
          aria-label={`Edit rule ${index + 1}: ${subject(rule)}`}>Edit</button
        >
        <button
          onclick={() => save(rules.filter((_, at) => at !== index))}
          disabled={busy}
          aria-label={`Delete rule ${index + 1}: ${subject(rule)}`}>Delete</button
        >
      </li>
    {/each}
  </ol>
  {#if draft}
    <form onsubmit={(event) => (event.preventDefault(), submit())}>
      <label
        >Decision
        <select bind:value={draft.decision}>
          <option value="allow">allow</option>
          <option value="ask">ask</option>
          <option value="deny">deny</option>
        </select>
      </label>
      <label
        >Kind
        <select bind:value={draft.kind} disabled={draft.index !== null}>
          <option value="exec">exec</option>
          <option value="file">file</option>
          <option value="network">network</option>
        </select>
      </label>
      <label
        >{LABELS[draft.index === null ? FIELDS[draft.kind] : draft.field]}
        <input bind:value={draft.value} autocomplete="off" autocapitalize="none" />
      </label>
      <button type="submit" disabled={busy || !draft.value}>Save rule</button>
      <button type="button" onclick={() => (draft = null)}>Cancel</button>
    </form>
  {:else}
    <button onclick={add} disabled={busy}>Add rule</button>
  {/if}
  <fieldset>
    <legend>Test a rule</legend>
    <div class="row">
      <label
        >Test kind
        <select
          bind:value={testKind}
          onchange={() => ((testValue = ''), (verdict = null))}
        >
          <option value="exec">exec</option>
          <option value="file">file</option>
          <option value="network">network</option>
        </select>
      </label>
      <label
        >{LABELS[FIELDS[testKind]]}
        <input bind:value={testValue} autocomplete="off" autocapitalize="none" />
      </label>
      <button onclick={test} disabled={!testValue}>Test</button>
    </div>
    {#if verdict}<p role="status">{verdictText(verdict)}</p>{/if}
  </fieldset>
{/if}
{#if loaded}
  <section aria-label="Audit log">
    <h4>Audit log</h4>
    {#if !audit.length}<p>Nothing has been recorded yet.</p>{/if}
    <ul>
      {#each audit as item}
        <li>
          <small>{new Date(item.time * 1000).toLocaleString()}</small>
          <span
            >{item.kind} {item.subject} {item.decision}
            {item.rule ? `by rule ${item.rule.index + 1}` : 'no rule'}{item.decision === 'ask'
              ? item.allowed
                ? ', allowed'
                : ', not allowed'
              : ''}</span
          >
        </li>
      {/each}
    </ul>
  </section>
{/if}

<style>
  ol,
  ul {
    margin: 0;
    padding: 0;
  }
  ol {
    padding-left: 20px;
  }
  ul {
    list-style: none;
  }
  li {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 8px 0;
    border-top: 1px solid var(--border);
    font-size: 12px;
  }
  li span {
    flex: 1;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  small {
    color: var(--text-muted);
    font-family: var(--font-mono);
  }
  button {
    padding: 6px 10px;
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    background: var(--bg-elevated);
  }
  form,
  .row {
    display: flex;
    flex-wrap: wrap;
    align-items: end;
    gap: 10px;
  }
  form {
    margin: 12px 0;
    font-size: 12px;
  }
  fieldset {
    margin: 20px 0 0;
    padding: 0;
    border: 0;
    font-size: 12px;
  }
  label {
    display: flex;
    flex-direction: column;
    gap: 8px;
    color: var(--text-muted);
  }
  input,
  select {
    min-width: 0;
    padding: 10px;
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    background-color: var(--bg-elevated);
    color: var(--text-primary);
  }
  select {
    padding-right: 30px;
  }
  input {
    min-width: 200px;
  }
  legend {
    padding: 0;
    margin-bottom: 10px;
    font-size: 12px;
    font-weight: 600;
  }
  h4 {
    margin: 20px 0 10px;
    font-size: 12px;
  }
  .error {
    color: var(--err);
  }
</style>
