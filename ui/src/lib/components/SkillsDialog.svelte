<script lang="ts">
  import { onMount } from 'svelte';
  import { persistence, type Skill, type SkillProposal } from '../persistence';
  import Icon from './Icon.svelte';
  import SkillImport from './SkillImport.svelte';
  import { platform } from '../platform';
  export let onClose: () => void;
  let dialog: HTMLDialogElement;
  let skills: Skill[] = [];
  let warnings: string[] = [];
  let proposals: SkillProposal[] = [];
  let drafts: Record<string, { description: string; body: string }> = {};
  let sourceMessages: Record<string, string> = {};
  let editing: Skill | null = null;
  let creating = false;
  let importing = false;
  let notice = '';
  let busy = false;
  let error = '';
  const desktop = persistence.available();
  async function load() {
    const [result, pending] = await Promise.all([
      persistence.skills(),
      persistence.skillProposals().catch(() => ({
        proposals: [],
        warnings: ['Proposed skills could not be read. Review them with bw skills.'],
      })),
    ]);
    skills = result.skills;
    proposals = pending.proposals;
    warnings = [...result.warnings, ...pending.warnings];
    // Edits in progress are kept; new proposals start from the proposed text.
    drafts = Object.fromEntries(
      proposals.map((proposal) => [
        proposal.id,
        drafts[proposal.id] ?? {
          description: proposal.skill.description,
          body: proposal.skill.body,
        },
      ]),
    );
  }
  function saved(skill: Skill) {
    return `${skill.description}\n\n${skill.body}`;
  }
  async function showSource(proposal: SkillProposal) {
    try {
      const session = await persistence.load(proposal.source.sessionId);
      const message = session?.messages.find((message) => message.id === proposal.source.messageId);
      sourceMessages[proposal.id] = message
        ? message.content.slice(0, 2000) + (message.content.length > 2000 ? '\n[excerpt]' : '')
        : 'The source conversation or message is no longer available.';
    } catch {
      sourceMessages[proposal.id] = 'The source conversation could not be opened.';
    }
  }
  async function review(proposal: SkillProposal, approve: boolean) {
    busy = true;
    error = '';
    try {
      if (approve) {
        const edited = drafts[proposal.id];
        await persistence.approveSkillProposal(proposal.id, edited.description, edited.body);
      } else await persistence.rejectSkillProposal(proposal.id);
    } catch (cause) {
      error =
        typeof cause === 'string'
          ? cause
          : 'This proposal could not be reviewed. Refresh and try again; the skill may have changed.';
      busy = false;
      return;
    }
    // The review is done; a failed refresh must not read as a failed review.
    proposals = proposals.filter((pending) => pending.id !== proposal.id);
    delete drafts[proposal.id];
    delete sourceMessages[proposal.id];
    try {
      await load();
    } catch {
      error = 'The proposal was reviewed, but your skills could not be refreshed.';
    } finally {
      busy = false;
    }
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
    {:else if importing}
      <SkillImport
        onCancel={() => {
          importing = false;
          // Some skills may have been imported before one failed.
          void load().catch(() => (error = 'Your skills could not be refreshed.'));
        }}
        onDone={(imported) => {
          importing = false;
          notice = imported === 1 ? 'Imported 1 skill.' : `Imported ${imported} skills.`;
          void load().catch(() => (error = 'Your skills could not be refreshed.'));
        }}
      />
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
        <label class="check"
          ><input type="checkbox" bind:checked={editing.userOnly} disabled={busy} /> Run only when I
          type /{editing.name || 'name'}</label
        >
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
        <span>Stored as Markdown files on this {platform.device}.</span><span class="buttons"
          ><button
            class="secondary"
            onclick={() => {
              notice = '';
              importing = true;
            }}>Import skills</button
          ><button
            class="primary"
            onclick={() => {
              creating = true;
              editing = { name: '', description: '', body: '', enabled: true };
            }}>New skill</button
          ></span
        >
      </div>
      {#if notice}<p role="status" class="notice">{notice}</p>{/if}
      {#if proposals.length}
        <section aria-label="Proposed skills" class="proposals">
          <h3>Pending review · {proposals.length}</h3>
          <p class="hint">
            The agent proposed these after <code>/learn</code>. Nothing is used until you approve
            it.
          </p>
          {#each proposals as proposal (proposal.id)}
            <article class="proposal">
              <strong>{proposal.skill.name}</strong>
              <small>{proposal.before ? 'Updates this skill' : 'New skill'}</small>
              <p class="hint">
                Source session: {proposal.source.sessionId}<br />Message: {proposal.source
                  .messageId}
              </p>
              <button class="secondary" disabled={busy} onclick={() => showSource(proposal)}
                >View source message</button
              >
              {#if sourceMessages[proposal.id]}<pre>{sourceMessages[proposal.id]}</pre>{/if}
              {#if proposal.before}<label for={`before-${proposal.id}`}>Saved skill (−)</label>
                <pre id={`before-${proposal.id}`}>{saved(proposal.before)}</pre>{/if}
              <label for={`description-${proposal.id}`}>Proposed description (+) · editable</label
              ><input
                id={`description-${proposal.id}`}
                aria-label={`Proposed description for ${proposal.skill.name}`}
                bind:value={drafts[proposal.id].description}
                maxlength="500"
                disabled={busy}
              /><label for={`body-${proposal.id}`}>Proposed instructions (+) · editable</label
              ><textarea
                id={`body-${proposal.id}`}
                aria-label={`Proposed instructions for ${proposal.skill.name}`}
                bind:value={drafts[proposal.id].body}
                rows="8"
                maxlength="30000"
                disabled={busy}></textarea>
              <div class="review-actions">
                <button
                  class="primary"
                  aria-label={`Approve ${proposal.skill.name}`}
                  disabled={busy ||
                    !drafts[proposal.id]?.description.trim() ||
                    !drafts[proposal.id]?.body.trim()}
                  onclick={() => review(proposal, true)}>Approve</button
                ><button
                  class="secondary"
                  aria-label={`Reject ${proposal.skill.name}`}
                  disabled={busy}
                  onclick={() => review(proposal, false)}>Reject</button
                >
              </div>
            </article>
          {/each}
        </section>
      {/if}
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
            ><strong>{skill.name}</strong>{#if skill.userOnly}{' '}<span class="typed"
                >Runs only when you type /{skill.name}</span
              >{/if}
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
        Agent runs list enabled skills by name and description and load one when it is relevant.
        Keep descriptions specific and use a model that supports tools. Type <code>/name</code> to
        run a skill directly; a skill set to run only when typed waits for that. Type
        <code>/learn</code> after a task that worked to have the agent propose a skill from it.
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
  .buttons {
    display: flex;
    gap: 8px;
  }
  .notice {
    color: var(--ok);
    font-size: 12px;
    margin: 0 0 12px;
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
  .typed {
    color: var(--text-muted);
    font-size: 11px;
    margin-left: 8px;
  }
  label.check {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .check input {
    width: auto;
    margin: 0;
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
  .proposals {
    margin-bottom: 24px;
  }
  .proposals h3 {
    font-size: 14px;
    margin: 0 0 8px;
  }
  .proposal {
    display: block;
  }
  .proposal small {
    margin-left: 8px;
    color: var(--text-muted);
    font-size: 11.5px;
  }
  .proposals label {
    margin-top: 12px;
  }
  .proposals pre {
    margin: 10px 0 0;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    max-height: 240px;
    overflow: auto;
    font-size: 12px;
    background: var(--bg-base);
    padding: 12px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
  }
  .review-actions {
    display: flex;
    gap: 8px;
    margin-top: 10px;
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
