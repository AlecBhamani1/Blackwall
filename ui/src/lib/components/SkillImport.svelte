<script lang="ts">
  import { onMount } from 'svelte';
  import { persistence, type SkillImportCandidate } from '../persistence';
  export let onDone: (imported: number) => void;
  export let onCancel: () => void;
  let candidates: SkillImportCandidate[] = [];
  let selected: Record<string, boolean> = {};
  let replace: Record<string, boolean> = {};
  let loaded = false;
  let busy = false;
  let error = '';
  let failures: string[] = [];

  /** New candidates replace any with the same source; ready ones that are not saved start selected. */
  function add(found: SkillImportCandidate[]) {
    const sources = new Set(found.map((candidate) => candidate.source));
    candidates = [...candidates.filter((candidate) => !sources.has(candidate.source)), ...found];
    for (const candidate of found) {
      selected[candidate.source] = Boolean(candidate.skill) && !candidate.exists;
      replace[candidate.source] = false;
    }
  }
  /** The skill's folder and file name; the full path is in the tooltip. */
  function shortPath(source: string) {
    return source.split(/[\\/]/).filter(Boolean).slice(-2).join('/');
  }
  async function choose(folder: boolean) {
    error = '';
    try {
      const found = await persistence.chooseSkillImport(folder);
      if (found === null) return;
      if (found.length === 0)
        error = 'No skills were found there. A skill is a SKILL.md file or a folder with one.';
      add(found);
    } catch (cause) {
      error = typeof cause === 'string' ? cause : 'That location could not be read.';
    }
  }
  async function importSelected() {
    busy = true;
    error = '';
    failures = [];
    let imported = 0;
    for (const candidate of candidates.filter(ready)) {
      try {
        await persistence.importSkill(candidate.source, replace[candidate.source] ?? false);
        imported += 1;
        selected[candidate.source] = false;
        // It is saved now, so importing it again needs Replace.
        candidates = candidates.map((item) =>
          item.source === candidate.source ? { ...item, exists: true } : item,
        );
        replace[candidate.source] = false;
      } catch (cause) {
        failures = [
          ...failures,
          `${candidate.skill?.name ?? candidate.source}: ${typeof cause === 'string' ? cause : 'It could not be imported.'}`,
        ];
      }
    }
    busy = false;
    if (failures.length === 0) onDone(imported);
  }
  /** A saved skill is only overwritten when its Replace box is ticked. */
  function ready(candidate: SkillImportCandidate) {
    return selected[candidate.source] && (!candidate.exists || replace[candidate.source]);
  }
  $: count = candidates.filter((candidate) => selected && replace && ready(candidate)).length;
  onMount(() => {
    void persistence
      .findSkillImports()
      .then(add)
      .catch(() => (error = 'Skills from other harnesses could not be listed.'))
      .finally(() => (loaded = true));
  });
</script>

<section aria-label="Import skills" class="import">
  <h3>Import skills</h3>
  <p class="hint">
    Skills in the SKILL.md format used by Claude Code, Codex, and most published skills. Only the
    instructions are imported; bundled files are not copied, and settings such as allowed tools are
    ignored because skills never grant permissions.
  </p>
  <div class="choose">
    <button class="secondary" disabled={busy} onclick={() => choose(true)}>Choose folder…</button>
    <button class="secondary" disabled={busy} onclick={() => choose(false)}>Choose file…</button>
  </div>
  {#if loaded && candidates.length === 0}<p class="empty">
      No skills found in ~/.claude/skills, ~/.codex/skills, or ~/.agents/skills. Choose a folder or
      file instead.
    </p>{/if}
  {#each candidates as candidate (candidate.source)}
    <article class:unavailable={!candidate.skill}>
      {#if candidate.skill}
        <input
          type="checkbox"
          aria-label={`Import ${candidate.skill.name}`}
          bind:checked={selected[candidate.source]}
          disabled={busy}
        />
      {/if}
      <div class="details">
        <strong>{candidate.skill?.name ?? 'Not importable'}</strong>
        <small>{candidate.origin}</small>
        {#if candidate.skill}<p>{candidate.skill.description}</p>{/if}
        <p class="source" title={candidate.source}>{shortPath(candidate.source)}</p>
        {#if candidate.error}<p class="problem">{candidate.error}</p>{/if}
        {#each candidate.notes as note}<p class="note">{note}</p>{/each}
        {#if candidate.exists}
          <label class="replace"
            ><input
              type="checkbox"
              bind:checked={replace[candidate.source]}
              disabled={busy || !selected[candidate.source]}
            /> A saved skill has this name. Replace it.</label
          >
        {/if}
      </div>
    </article>
  {/each}
  {#each failures as failure}<p role="alert" class="problem">{failure}</p>{/each}
  {#if error}<p role="alert" class="problem">{error}</p>{/if}
  <div class="actions">
    <button class="secondary" disabled={busy} onclick={onCancel}>Cancel</button><span></span><button
      class="primary"
      disabled={busy || count === 0}
      onclick={importSelected}
      >{busy ? 'Importing…' : count === 1 ? 'Import 1 skill' : `Import ${count} skills`}</button
    >
  </div>
</section>

<style>
  h3 {
    font-size: 14px;
    margin: 0 0 8px;
  }
  .hint,
  .empty {
    font-size: 12px;
    color: var(--text-muted);
    line-height: 1.6;
    margin: 0 0 14px;
  }
  .choose,
  .actions {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .actions {
    margin-top: 16px;
  }
  .actions > span {
    flex: 1;
  }
  article {
    display: flex;
    gap: 12px;
    align-items: flex-start;
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 14px 16px;
    margin: 10px 0;
  }
  article.unavailable {
    opacity: 0.75;
  }
  .details {
    min-width: 0;
    flex: 1;
  }
  strong {
    font-size: 13px;
    font-weight: 570;
  }
  small {
    margin-left: 8px;
    color: var(--text-muted);
    font-size: 11.5px;
  }
  .details p {
    font-size: 12px;
    line-height: 1.6;
    margin: 5px 0 0;
    overflow-wrap: anywhere;
  }
  .details p:not(.problem, .note) {
    color: var(--text-muted);
  }
  .source {
    font-family: var(--font-code);
    font-size: 11px !important;
  }
  .note {
    color: var(--warn);
  }
  .problem {
    color: var(--err);
    font-size: 12px;
  }
  .replace {
    display: flex;
    gap: 8px;
    align-items: center;
    font-size: 12px;
    margin-top: 8px;
  }
  input[type='checkbox'] {
    accent-color: var(--accent);
    width: 16px;
    height: 16px;
    margin-top: 2px;
  }
  .primary,
  .secondary {
    padding: 8px 12px;
    border-radius: var(--radius-sm);
    font-size: 11.5px;
  }
  .primary {
    background: var(--accent);
    color: var(--text-on-accent);
    font-weight: 600;
  }
  .secondary {
    background: var(--bg-elevated);
    border: 1px solid var(--border);
  }
  button:disabled {
    opacity: 0.5;
  }
</style>
