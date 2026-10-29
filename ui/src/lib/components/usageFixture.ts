import type { UsageGroupRow, UsageSummary, UsageTokens } from '../persistence';

const tokens = (input: number, cachedInput: number, output: number, reasoning: number) =>
  ({ input, cachedInput, output, reasoning }) satisfies UsageTokens;

function group(
  name: string | null,
  total: number,
  share: number,
  runs: number | null,
  sessions: number,
  calls: number,
): UsageGroupRow {
  const output = Math.round(total * 0.11);
  return {
    name,
    tokens: tokens(total - output, Math.round(total * 0.27), output, Math.round(total * 0.04)),
    total,
    share,
    runs,
    sessions,
    modelCalls: calls,
    failedCalls: 0,
    medianOutputSpeed: 41,
    p90Seconds: 6.4,
    modes: [
      { mode: 'agent', total: Math.round(total * 0.88) },
      { mode: 'subagent', total: Math.round(total * 0.09) },
      { mode: 'chat', total: Math.round(total * 0.03) },
    ],
    sessionRows: [
      { id: 'session_7c41e09a', models: ['qwen3.6'], modelCalls: 214, total: 612_000 },
      { id: 'session_19d2b7f3', models: ['qwen3.6'], modelCalls: 158, total: 447_000 },
      { id: 'session_a03e55c1', models: ['llama3.3', 'qwen3.6'], modelCalls: 96, total: 301_000 },
    ],
  };
}

const days = [
  ['2026-10-22', 520_000, 80_000, true],
  ['2026-10-23', 0, 0, false],
  ['2026-10-24', 0, 0, false],
  ['2026-10-25', 760_000, 110_000, true],
  ['2026-10-26', 950_000, 140_000, true],
  ['2026-10-27', 650_000, 90_000, true],
  ['2026-10-28', 850_000, 150_000, true],
] as const;

/** A week of usage, shaped like the owner's mockup. */
export function usageFixture(): UsageSummary {
  return {
    from: '2026-10-22',
    to: '2026-10-28',
    totals: {
      tokens: tokens(4_290_000, 1_300_000, 530_000, 190_000),
      total: 4_820_000,
      modelCalls: 1206,
      failedCalls: 9,
      runs: 143,
      completedRuns: 138,
      runsByOutcome: { complete: 138, error: 3, iterations: 2 },
      runSuccess: 138 / 143,
      sessions: 38,
      medianOutputSpeed: 41.2,
      topModel: 'qwen3.6',
      tokensSaved: 612_000,
      compactions: 9,
    },
    perDay: days.map(([day, input, output, recorded]) => ({
      day,
      tokens: tokens(input, Math.round(input * 0.3), output, Math.round(output * 0.3)),
      recorded,
    })),
    perProject: [
      group('blackwall', 2_310_000, 0.48, 61, 12, 520),
      group('house-finder', 1_040_000, 0.216, 28, 9, 260),
      group('jerkins-api', 702_000, 0.146, 22, 7, 190),
      group('mission-control', 418_000, 0.087, 17, 5, 140),
      group(null, 331_000, 0.069, 15, 5, 96),
    ],
    perModel: [
      group('qwen3.6', 3_900_000, 0.81, null, 33, 1000),
      group('llama3.3', 920_000, 0.19, null, 6, 206),
    ],
    perMode: [
      { mode: 'agent', total: 4_240_000, modelCalls: 1040, runs: 128 },
      { mode: 'subagent', total: 430_000, modelCalls: 120, runs: 0 },
      { mode: 'chat', total: 150_000, modelCalls: 46, runs: 15 },
    ],
    tools: [
      { name: 'read_file', calls: 612, errors: 2, denied: 0, timeouts: 0 },
      { name: 'shell', calls: 284, errors: 17, denied: 12, timeouts: 1 },
      { name: 'context7__query-docs', calls: 97, errors: 1, denied: 0, timeouts: 0 },
      { name: 'spawn_agent', calls: 41, errors: 1, denied: 0, timeouts: 0 },
    ],
    gaps: { firstDay: null, droppedRuns: 1 },
    warnings: [],
    telemetryEnabled: true,
    files: 5,
    newestUnixMs: Date.now() - 30_000,
    stale: false,
  };
}

export function emptyUsage(): UsageSummary {
  const summary = usageFixture();
  return {
    ...summary,
    totals: {
      ...summary.totals,
      tokens: tokens(0, 0, 0, 0),
      total: 0,
      modelCalls: 0,
      failedCalls: 0,
      runs: 0,
      completedRuns: 0,
      runsByOutcome: {},
      runSuccess: null,
      sessions: 0,
      medianOutputSpeed: null,
      topModel: null,
      tokensSaved: 0,
      compactions: 0,
    },
    perDay: summary.perDay.map((day) => ({ ...day, tokens: tokens(0, 0, 0, 0), recorded: false })),
    perProject: [],
    perModel: [],
    perMode: [],
    tools: [],
    gaps: { firstDay: null, droppedRuns: 0 },
    files: 0,
    newestUnixMs: null,
  };
}
