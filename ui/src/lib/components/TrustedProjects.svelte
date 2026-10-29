<script lang="ts">
  import { onMount } from 'svelte';
  import { projectClient, type TrustedProject } from '../project';

  export let client = projectClient;

  let projects: TrustedProject[] = [];
  let loaded = false;
  let busy = false;
  let error = '';

  function message(cause: unknown, fallback: string) {
    return cause instanceof Error ? cause.message : typeof cause === 'string' ? cause : fallback;
  }
  async function load() {
    try {
      projects = await client.trusted();
    } catch (cause) {
      error = message(cause, 'Trusted projects could not be loaded.');
    } finally {
      loaded = true;
    }
  }
  async function untrust(project: TrustedProject) {
    busy = true;
    error = '';
    try {
      await client.untrust(project.path);
      projects = projects.filter((item) => item.path !== project.path);
    } catch (cause) {
      error = message(cause, 'The project could not be untrusted.');
    } finally {
      busy = false;
    }
  }
  onMount(load);
</script>

<p>
  Projects whose <code>.blackwall/project.json</code> you trusted. Untrusting one keeps only its deny and
  ask rules until you trust it again.
</p>
{#if error}<p class="error" role="alert">{error}</p>{/if}
{#if loaded && !projects.length && !error}<p>No trusted projects.</p>{/if}
<ul>
  {#each projects as project (project.path)}
    <li>
      <span title={project.label}>{project.label}</span>
      <small>{project.sha256.slice(0, 12)}</small>
      <button
        onclick={() => untrust(project)}
        disabled={busy}
        aria-label={`Untrust ${project.label}`}>Untrust</button
      >
    </li>
  {/each}
</ul>

<style>
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
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
  .error {
    color: var(--err);
  }
</style>
