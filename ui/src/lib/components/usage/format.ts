/** A compact count: 950, 12.4k, 612k, 4.82M. */
export function compact(value: number): string {
  if (value < 1000) return String(Math.round(value));
  const [scaled, unit] = value < 1_000_000 ? [value / 1e3, 'k'] : [value / 1e6, 'M'];
  const digits = scaled >= 100 ? 0 : scaled >= 10 ? 1 : 2;
  return `${Number(scaled.toFixed(digits))}${unit}`;
}

export function percent(share: number): string {
  return share >= 1 ? '100%' : `${(share * 100).toFixed(1)}%`;
}

export function plural(count: number, word: string, many = `${word}s`): string {
  return `${count.toLocaleString()} ${count === 1 ? word : many}`;
}

/** A UTC day (`YYYY-MM-DD`) as a short label: `Wed` within a week, else `Oct 22`. */
export function dayLabel(day: string, short: boolean): string {
  const date = new Date(`${day}T00:00:00Z`);
  return short
    ? date.toLocaleDateString(undefined, { weekday: 'short', timeZone: 'UTC' })
    : date.toLocaleDateString(undefined, { month: 'short', day: 'numeric', timeZone: 'UTC' });
}

export const modeNames: Record<string, string> = {
  agent: 'Agent',
  subagent: 'Subagent',
  chat: 'Chat',
};

/** Run outcomes other than `complete`, as the Run success tile words them. */
export const outcomeNames: Record<string, [string, string]> = {
  error: ['error', 'errors'],
  iterations: ['stopped at step limit', 'stopped at step limit'],
  cancelled: ['stopped', 'stopped'],
  denied: ['denied', 'denied'],
};
