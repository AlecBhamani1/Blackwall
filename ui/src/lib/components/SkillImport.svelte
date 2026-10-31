<script lang="ts">
  import ScanBadge from './ScanBadge.svelte';
  import { onDestroy, onMount } from 'svelte';
  import {
    persistence,
    type Skill,
    type SkillImportCandidate,
    type SuggestedSkill,
  } from '../persistence';
  import SettingsFooter from './SettingsFooter.svelte';
  export let onDone: (imported: number) => void;
  export let onCancel: () => void;
  export let installedSkills: Skill[] = [];
  export let onRemove: (name: string) => void = () => {};
  let candidates: SkillImportCandidate[] = [];
  let suggested: SuggestedSkill[] = [];
  let selected: Record<string, boolean> = {};
  let replace: Record<string, boolean> = {};
  /** The owner chose to import despite scan findings. */
  let allowed: Record<string, boolean> = {};
  let getting: Record<string, boolean> = {};
  let downloads: string[] = [];
  let loaded = false;
  let busy = false;
  let error = '';
  let failures: string[] = [];

  /** New candidates replace any with the same source. Ready, unsaved, unflagged ones start selected. */
  function add(found: SkillImportCandidate[]) {
    const sources = new Set(found.map((candidate) => candidate.source));
    candidates = [...candidates.filter((candidate) => !sources.has(candidate.source)), ...found];
    for (const candidate of found) {
      selected[candidate.source] =
        Boolean(candidate.skill) && !candidate.exists && candidate.findings.length === 0;
      replace[candidate.source] = false;
      allowed[candidate.source] = false;
    }
  }
  /** The skill's folder and file name; the full path is in the tooltip. */
  function shortPath(source: string) {
    return source.split(/[\\/]/).filter(Boolean).slice(-2).join('/');
  }
  function files(candidate: SkillImportCandidate) {
    const shown = candidate.files.slice(0, 6).join(', ');
    const more = candidate.files.length > 6 ? `, and ${candidate.files.length - 6} more` : '';
    return `${candidate.files.length} bundled ${candidate.files.length === 1 ? 'file' : 'files'}: ${shown}${more}`;
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
  async function get(skill: SuggestedSkill) {
    error = '';
    getting[skill.name] = true;
    try {
      const candidate = await persistence.getSuggestedSkill(skill.name);
      // The panel may have closed while this downloaded.
      if (destroyed) {
        void persistence.discardSkillDownloads([candidate.source]);
        return;
      }
      downloads = [...downloads, candidate.source];
      add([candidate]);
    } catch (cause) {
      error = typeof cause === 'string' ? cause : `${skill.name} could not be downloaded.`;
    } finally {
      getting[skill.name] = false;
    }
  }
  function removeInstalled(name: string) {
    if (window.confirm(`Remove ${name}?`)) onRemove(name);
  }
  function allow(candidate: SkillImportCandidate) {
    allowed[candidate.source] = true;
    selected[candidate.source] = true;
  }
  function block(candidate: SkillImportCandidate) {
    candidates = candidates.filter((item) => item.source !== candidate.source);
    selected[candidate.source] = false;
    if (downloads.includes(candidate.source))
      void persistence.discardSkillDownloads([candidate.source]);
  }
  async function importSelected() {
    busy = true;
    error = '';
    failures = [];
    let imported = 0;
    for (const candidate of candidates.filter(ready)) {
      try {
        await persistence.importSkill(
          candidate.source,
          replace[candidate.source] ?? false,
          allowed[candidate.source] ?? false,
        );
        imported += 1;
        selected[candidate.source] = false;
        downloads = downloads.filter((source) => source !== candidate.source);
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
  /** A saved skill is only overwritten when its Replace box is ticked, and a flagged one only
   * after Allow anyway. */
  function ready(candidate: SkillImportCandidate) {
    return (
      selected[candidate.source] &&
      (!candidate.exists || replace[candidate.source]) &&
      (candidate.findings.length === 0 || allowed[candidate.source])
    );
  }
  $: count = candidates.filter(
    (candidate) => selected && replace && allowed && ready(candidate),
  ).length;
  onMount(() => {
    void persistence
      .suggestedSkills()
      .then((list) => (suggested = list))
      .catch(() => (suggested = []));
    void persistence
      .findSkillImports()
      .then(add)
      .catch(() => (error = 'Skills from other harnesses could not be listed.'))
      .finally(() => (loaded = true));
  });
  // Downloads that were reviewed but not imported are removed.
  let destroyed = false;
  onDestroy(() => {
    destroyed = true;
    if (downloads.length) void persistence.discardSkillDownloads(downloads);
  });
</script>

<section aria-label="Import skills" class="import">
  <h3>Import skills</h3>
  <p class="hint">
    Skills in the SKILL.md format used by Claude Code, Codex, and most published skills, with their
    scripts and reference files. Every skill is scanned for hidden instructions and data leaks
    before you import it. Settings such as allowed tools are ignored; skills never grant
    permissions, and their scripts still ask before running.
  </p>
  {#if suggested.length}
    <h4>Suggested</h4>
    <ul class="suggested" aria-label="Suggested skills">
      {#each suggested as skill (skill.name)}
        <li>
          <div>
            <strong>{skill.name}</strong>
            <p>{skill.description}</p>
            <small>{skill.repository} · {skill.license}</small>
          </div>
          {#if installedSkills.some((installed) => installed.name === skill.name)}<div
              class="installed-actions"
              ><button class="installed" disabled>Installed ✓</button><button
                class="remove-suggestion"
                aria-label={`Remove ${skill.name}`}
                disabled={busy}
                onclick={() => removeInstalled(skill.name)}>Remove</button
              ></div
            >{:else}<button
              class="secondary"
              aria-label={`Get ${skill.name}`}
              disabled={busy ||
                getting[skill.name] ||
                candidates.some((candidate) => candidate.skill?.name === skill.name)}
              onclick={() => get(skill)}>{getting[skill.name] ? 'Getting…' : 'Get'}</button
            >{/if}
        </li>
      {/each}
    </ul>
    <h4>On this {candidates.length ? 'computer and downloaded' : 'computer'}</h4>
  {/if}
  <div class="choose">
    <button class="secondary" disabled={busy} onclick={() => choose(true)}>Choose folder…</button>
    <button class="secondary" disabled={busy} onclick={() => choose(false)}>Choose file…</button>
  </div>
  {#if loaded && candidates.length === 0}<p class="empty">
      No skills found in ~/.claude/skills, ~/.codex/skills, or ~/.agents/skills. Get a suggested
      skill, or choose a folder or file.
    </p>{/if}
  {#each candidates as candidate (candidate.source)}
    <article class:unavailable={!candidate.skill}>
      {#if candidate.skill}
        <input
          type="checkbox"
          aria-label={`Import ${candidate.skill.name}`}
          bind:checked={selected[candidate.source]}
          disabled={busy || (candidate.findings.length > 0 && !allowed[candidate.source])}
        />
      {/if}
      <div class="details">
        <strong>{candidate.skill?.name ?? 'Not importable'}</strong>
        <small>{candidate.origin}</small>
        {#if candidate.skill}<p>{candidate.skill.description}</p>{/if}
        <p class="source" title={candidate.source}>{shortPath(candidate.source)}</p>
        {#if candidate.skill}
          <p class="meta">
            {candidate.license ? `License: ${candidate.license}` : 'No license file'}{candidate
              .files.length
              ? ` · ${files(candidate)}`
              : ''}
          </p>
          <ScanBadge findings={candidate.findings} name={candidate.skill.name} />
          {#if candidate.findings.length > 0}
            {#if allowed[candidate.source]}
              <p class="note">Allowed by you despite the warnings.</p>
            {:else}
              <div class="decide">
                <button
                  class="secondary"
                  disabled={busy}
                  aria-label={`Allow ${candidate.skill.name} anyway`}
                  onclick={() => allow(candidate)}>Allow anyway</button
                ><button
                  class="block"
                  disabled={busy}
                  aria-label={`Block ${candidate.skill.name}`}
                  onclick={() => block(candidate)}>Block</button
                >
              </div>
            {/if}
          {/if}
        {/if}
        {#if candidate.error}<p class="problem">{candidate.error}</p>{/if}
        {#each candidate.notes as note}<p class="notice warning"><span aria-hidden="true">⚠</span>{note}</p>{/each}
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
  {#if error}<p role="alert" class="problem">{error}</p>{/if}
  <SettingsFooter
    dirty
    canSave={count > 0}
    {busy}
    saveLabel={count === 1 ? 'Import 1 skill' : `Import ${count} skills`}
    feedback=""
    error={failures.join(' ')}
    onSave={importSelected}
    onCancel={onCancel}
  />
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
  .choose {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .notice.warning {
    display: flex;
    gap: 8px;
    border-left: 2px solid var(--warn);
    padding: 8px 10px;
    background: color-mix(in srgb, var(--warn) 7%, transparent);
    color: var(--text-muted);
    font-size: 12px;
    font-weight: 400;
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
  .secondary {
    padding: 8px 12px;
    border-radius: var(--radius-sm);
    font-size: 11.5px;
  }
  .secondary {
    background: var(--bg-elevated);
    border: 1px solid var(--border);
  }
  h4 {
    font-size: 12px;
    color: var(--text-muted);
    font-weight: 600;
    margin: 18px 0 8px;
  }
  .suggested {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 8px;
  }
  .suggested li {
    display: flex;
    gap: 12px;
    align-items: center;
    justify-content: space-between;
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 10px 14px;
  }
  .suggested p {
    font-size: 12px;
    color: var(--text-muted);
    margin: 3px 0;
  }
  .suggested small {
    margin: 0;
  }
  .installed-actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .installed {
    padding: 6px 10px;
    border-radius: var(--radius-sm);
    background: var(--accent);
    color: var(--text-on-accent);
    font-size: 11.5px;
    font-weight: 600;
  }
  .remove-suggestion {
    padding: 6px 0;
    background: transparent;
    color: var(--err);
    font-size: 11.5px;
  }
  .meta {
    color: var(--text-muted);
  }
  .decide {
    display: flex;
    gap: 8px;
    margin-top: 8px;
  }
  .block {
    padding: 8px 12px;
    border-radius: var(--radius-sm);
    font-size: 11.5px;
    background: transparent;
    color: var(--err);
    border: 1px solid var(--border);
  }
  button:disabled {
    opacity: 0.5;
  }
</style>
