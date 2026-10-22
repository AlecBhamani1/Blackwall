<script lang="ts">
  import type { SkillFinding } from '../persistence';

  /** Safety-scan results for one skill; the badge opens the list of findings. */
  export let findings: SkillFinding[];
  export let name: string;

  let open = false;
</script>

{#if findings.length === 0}
  <p class="badge clean">Scanned: no issues found</p>
{:else}
  <button class="badge warn" aria-expanded={open} onclick={() => (open = !open)}
    >Scanned: {findings.length}
    {findings.length === 1 ? 'warning' : 'warnings'}</button
  >
  {#if open}
    <ul class="findings" aria-label={`Scan warnings for ${name}`}>
      {#each findings as finding}
        <li>
          <span class={finding.severity}>{finding.severity}</span>
          <code>{finding.file}:{finding.line}</code>
          {finding.reason}
          <pre>{finding.excerpt}</pre>
        </li>
      {/each}
    </ul>
  {/if}
{/if}

<style>
  .badge {
    display: inline-block;
    font-size: 11.5px;
    margin-top: 8px;
    padding: 3px 8px;
    border-radius: var(--radius-sm);
    border: 1px solid var(--border);
  }
  .badge.clean {
    color: var(--ok);
  }
  .badge.warn {
    color: var(--warn);
    background: transparent;
  }
  .findings {
    list-style: none;
    margin: 8px 0 0;
    padding: 0;
    font-size: 12px;
  }
  .findings li {
    margin-bottom: 8px;
  }
  .findings span {
    text-transform: uppercase;
    font-size: 10.5px;
    font-weight: 700;
    margin-right: 6px;
  }
  .findings .high {
    color: var(--err);
  }
  .findings .medium {
    color: var(--warn);
  }
  .findings pre {
    margin: 4px 0 0;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    font-size: 11.5px;
    background: var(--bg-base);
    padding: 6px 8px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
  }
</style>
