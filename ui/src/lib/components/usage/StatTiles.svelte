<script lang="ts">
  import type { UsageSummary } from '../../persistence';
  import { compact, outcomeNames, percent, plural } from './format';
  export let summary: UsageSummary;

  $: totals = summary.totals;
  $: outcomes = Object.entries(totals.runsByOutcome)
    .filter(([outcome, count]) => outcome !== 'complete' && count > 0)
    .map(([outcome, count]) => {
      const [one, many] = outcomeNames[outcome] ?? [outcome, outcome];
      return `${count.toLocaleString()} ${count === 1 ? one : many}`;
    });
</script>

<ul class="tiles" aria-label="Usage totals">
  <li class="tile hero">
    <span class="label">Tokens recorded</span>
    <strong class="value">{compact(totals.total)}</strong>
    <span class="sub"
      >{plural(totals.runs, 'run')} · {plural(totals.sessions, 'session')} · {plural(
        totals.modelCalls,
        'model call',
      )}</span
    >
  </li>
  <li class="tile">
    <span class="label">Run success</span>
    <strong class="value">{totals.runSuccess === null ? '—' : percent(totals.runSuccess)}</strong>
    <span class="sub"
      >{totals.runSuccess === null
        ? 'No finished runs'
        : outcomes.length
          ? outcomes.join(' · ')
          : 'Every run completed'}</span
    >
  </li>
  <li class="tile">
    <span class="label">Output speed</span>
    <strong class="value"
      >{totals.medianOutputSpeed === null
        ? '—'
        : `${Math.round(totals.medianOutputSpeed)} tok/s`}</strong
    >
    <span class="sub"
      >{totals.medianOutputSpeed === null
        ? 'No timed output yet'
        : `median${totals.topModel ? ` · ${totals.topModel}` : ''}`}</span
    >
  </li>
  <li class="tile">
    <span class="label">Tokens saved</span>
    <strong class="value">{compact(totals.tokensSaved)}</strong>
    <span class="sub">compaction · {plural(totals.compactions, 'compaction')}</span>
  </li>
</ul>

<style>
  .tiles {
    display: grid;
    grid-template-columns: 1.4fr 1fr 1fr 1fr;
    gap: 12px;
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .tile {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
    padding: 14px 16px;
    background: var(--bg-elevated);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
  }
  .label {
    font: 11px var(--font-code);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--text-faint);
  }
  .value {
    font-size: 22px;
    font-weight: 600;
    letter-spacing: -0.01em;
  }
  .hero .value {
    font-size: 32px;
    letter-spacing: -0.02em;
  }
  .sub {
    font-size: 12px;
    color: var(--text-muted);
    overflow-wrap: anywhere;
  }
  @media (max-width: 760px) {
    .tiles {
      grid-template-columns: 1fr 1fr;
    }
    .hero {
      grid-column: 1 / -1;
    }
  }
</style>
