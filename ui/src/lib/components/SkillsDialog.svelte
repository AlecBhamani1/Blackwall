<script lang="ts">
  import { onMount } from 'svelte';
  import { persistence, type Skill } from '../persistence';
  import Icon from './Icon.svelte';
  export let onClose: () => void;
  let dialog: HTMLDialogElement;
  let skills: Skill[] = [];
  let warnings: string[] = [];
  let editing: Skill | null = null;
  let creating = false;
  let busy = false;
  let error = '';
  const desktop = persistence.available();
  async function load() {
    const result = await persistence.skills();
    skills = result.skills;
    warnings = result.warnings;
  }
  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (!editing) return;
    busy = true;
    error = '';
    try {
      await persistence.saveSkill(editing);
      editing = null;
      await load();
    } catch (cause) {
      error =
        typeof cause === 'string'
          ? cause
          : 'This skill could not be saved. Check its name and try again.';
    } finally {
      busy = false;
    }
  }
  async function toggle(skill: Skill) {
    busy = true;
    error = '';
    try {
      await persistence.saveSkill({ ...skill, enabled: !skill.enabled });
      await load();
    } catch {
      error = 'This skill could not be updated.';
    } finally {
      busy = false;
    }
  }
  async function remove(skill: Skill) {
    busy = true;
    error = '';
    try {
      await persistence.removeSkill(skill.name);
      editing = null;
      await load();
    } catch {
      error = 'This skill could not be deleted.';
    } finally {
      busy = false;
    }
  }
  onMount(() => {
    dialog.showModal();
    if (desktop)
      void load().catch(
        () => (error = 'Your skills could not be opened. Existing files have been left in place.'),
      );
  });
</script>

<dialog bind:this={dialog} onclose={onClose} aria-labelledby="skills-title">
  <header>
    <div>
      <h2 id="skills-title">Skills</h2>
      <p>Reusable instructions for the way you like to work.</p>
    </div>
    <button class="icon-button" aria-label="Close skills" onclick={() => dialog.close()}
      ><Icon name="x" /></button
    >
  </header>
  <div class="content">
    {#if !desktop}<p>Skills are available in the installed Blackwall desktop app.</p>
    {:else if editing}
      <form onsubmit={save}>
        <label for="skill-name">Name</label><input
          id="skill-name"
          bind:value={editing.name}
          pattern="[a-z0-9-]+"
          maxlength="64"
          required
          disabled={!creating || busy}
          placeholder="my-workflow"
        /><label for="skill-description">When should Blackwall use it?</label><input
          id="skill-description"
          bind:value={editing.description}
          maxlength="500"
          required
          disabled={busy}
          placeholder="A short description of this workflow"
        /><label for="skill-body">Instructions</label><textarea
          id="skill-body"
          bind:value={editing.body}
          rows="10"
          maxlength="30000"
          required
          disabled={busy}
          placeholder="Describe the steps Blackwall should follow…"></textarea>
        <p class="hint">
          Enabled skills guide agent tasks. They never bypass approval for file changes, commands,
          or web access.
        </p>
        <div class="actions">
          <button type="button" class="secondary" disabled={busy} onclick={() => (editing = null)}
            >Cancel</button
          >{#if !creating}<button
              type="button"
              class="delete"
              disabled={busy}
              onclick={() => editing && remove(editing)}>Delete skill</button
            >{/if}<span></span><button class="primary" disabled={busy}
            >{busy ? 'Saving…' : 'Save skill'}</button
          >
        </div>
      </form>
    {:else}
      <div class="intro">
        <span>Stored as Markdown files on this Mac.</span><button
          class="primary"
          onclick={() => {
            creating = true;
            editing = { name: '', description: '', body: '', enabled: true };
          }}>New skill</button
        >
      </div>
      {#if skills.length === 0}<p class="empty">
          No skills yet. Add a workflow to use in future agent tasks.
        </p>{/if}
      {#each skills as skill (skill.name)}<article>
          <button
            class="skill"
            onclick={() => {
              creating = false;
              editing = { ...skill };
            }}
            ><strong>{skill.name}</strong>
            <p>{skill.description}</p></button
          ><button
            class:enabled={skill.enabled}
            class="toggle"
            disabled={busy}
            aria-label={`${skill.enabled ? 'Disable' : 'Enable'} ${skill.name}`}
            onclick={() => toggle(skill)}>{skill.enabled ? 'Enabled' : 'Disabled'}</button
          >
        </article>{/each}
      <p class="hint">
        Blackwall includes enabled skills when they fit its context budget. Keep instructions
        focused and use a model that supports tools.
      </p>
    {/if}
    {#each warnings as warning}<p class="warning">{warning}</p>{/each}
    {#if error}<p role="alert" class="error">{error}</p>{/if}
  </div>
</dialog>

<style>
  dialog {
    width: min(660px, calc(100vw - 32px));
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
  .hint {
    font-size: 12px;
    color: var(--text-muted);
    line-height: 1.6;
    margin: 0;
  }
  .hint {
    margin: 18px 0;
  }
  .content {
    padding: 24px;
  }
  .icon-button {
    background: transparent;
    color: var(--text-muted);
    padding: 4px;
  }
  .intro,
  .actions {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .intro {
    justify-content: space-between;
    margin-bottom: 18px;
  }
  .intro > span {
    color: var(--text-muted);
    font-size: 12px;
  }
  .actions > span {
    flex: 1;
  }
  .primary,
  .secondary,
  .delete,
  .toggle {
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
  .delete {
    background: transparent;
    color: var(--err);
  }
  article {
    display: flex;
    gap: 15px;
    align-items: center;
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 16px;
    margin: 10px 0;
  }
  .skill {
    flex: 1;
    min-width: 0;
    text-align: left;
    background: transparent;
    padding: 0;
  }
  .skill strong {
    font-size: 13px;
    font-weight: 570;
  }
  .skill p {
    color: var(--text-muted);
    font-size: 12px;
    line-height: 1.6;
    margin: 5px 0 0;
  }
  .toggle {
    color: var(--text-muted);
    background: var(--bg-elevated);
    border: 1px solid var(--border);
  }
  .toggle.enabled {
    color: var(--ok);
  }
  label {
    display: block;
    font-size: 12px;
    margin: 16px 0 8px;
  }
  input,
  textarea {
    width: 100%;
    background: var(--bg-base);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-sm);
    padding: 11px 12px;
    font-size: 12px;
  }
  textarea {
    resize: vertical;
    font-family: var(--font-code);
    line-height: 1.6;
    min-height: 130px;
  }
  .empty {
    color: var(--text-muted);
    font-size: 12px;
    padding: 25px 0;
  }
  .warning {
    color: var(--warn);
    font-size: 12px;
  }
  .error {
    color: var(--err);
    font-size: 12px;
  }
  button:disabled {
    opacity: 0.5;
  }
</style>
