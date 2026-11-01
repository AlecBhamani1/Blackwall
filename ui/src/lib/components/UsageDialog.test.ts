import { render, screen, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { persistence } from '../persistence';
import UsageDialog from './UsageDialog.svelte';
import { emptyUsage, usageFixture } from './usageFixture';

describe('usage dialog', () => {
  beforeEach(() => {
    Object.defineProperty(HTMLDialogElement.prototype, 'showModal', {
      configurable: true,
      value: function (this: HTMLDialogElement) {
        this.open = true;
      },
    });
    vi.spyOn(persistence, 'available').mockReturnValue(true);
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it('renders tiles from a fixture', async () => {
    vi.spyOn(persistence, 'usage').mockResolvedValue(usageFixture());
    render(UsageDialog, { props: { onClose: vi.fn() } });
    const tiles = within(await screen.findByRole('list', { name: 'Usage totals' }));
    expect(tiles.getByText('4.82M')).toBeInTheDocument();
    expect(tiles.getByText('143 runs · 38 sessions · 1,206 model calls')).toBeInTheDocument();
    expect(tiles.getByText('96.5%')).toBeInTheDocument();
    expect(tiles.getByText('3 errors · 2 hit the step limit')).toBeInTheDocument();
    expect(tiles.getByText('41 tok/s')).toBeInTheDocument();
    expect(tiles.getByText('median · qwen3.6')).toBeInTheDocument();
    expect(tiles.getByText('612k')).toBeInTheDocument();
    expect(tiles.getByText('compaction · 9 compactions')).toBeInTheDocument();
    // Token mix legend, and tools with error and denied rates.
    expect(screen.getByText('Cached 1.3M')).toBeInTheDocument();
    const shell = screen.getByRole('row', { name: /shell/ });
    expect(within(shell).getByText('6.0%')).toBeInTheDocument();
    expect(within(shell).getByText('4.2%')).toBeInTheDocument();
  });

  it('range switch calls usage(7)', async () => {
    const user = userEvent.setup();
    const usage = vi.spyOn(persistence, 'usage').mockResolvedValue(usageFixture());
    render(UsageDialog, { props: { onClose: vi.fn() } });
    await screen.findByText('4.82M');
    expect(usage).toHaveBeenLastCalledWith(7);
    const range = screen.getByRole('group', { name: 'Range' });
    expect(within(range).getAllByRole('radio')).toHaveLength(5);
    expect(
      within(range)
        .getAllByRole('radio')
        .filter((option) => option.matches(':checked')),
    ).toHaveLength(1);
    await user.click(screen.getByRole('radio', { name: '30 days' }));
    expect(usage).toHaveBeenLastCalledWith(30);
    await user.click(screen.getByRole('radio', { name: '7 days' }));
    expect(usage).toHaveBeenLastCalledWith(7);
    expect(screen.getByRole('radio', { name: '7 days' })).toBeChecked();
  });

  it('uses the inclusive local calendar days since January 1 for YTD', async () => {
    vi.useFakeTimers({ now: new Date(2026, 9, 31, 18, 30) });
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    const usage = vi.spyOn(persistence, 'usage').mockResolvedValue(usageFixture());
    render(UsageDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('radio', { name: 'Year to date' }));
    expect(usage).toHaveBeenLastCalledWith(304);
  });

  it('shows the empty state text "No usage yet"', async () => {
    vi.spyOn(persistence, 'usage').mockResolvedValue(emptyUsage());
    render(UsageDialog, { props: { onClose: vi.fn() } });
    expect(await screen.findByText('No usage yet')).toBeInTheDocument();
    expect(screen.queryByRole('list', { name: 'Usage totals' })).not.toBeInTheDocument();
  });

  it('shows a banner when telemetry is off', async () => {
    vi.spyOn(persistence, 'usage').mockResolvedValue({
      ...usageFixture(),
      telemetryEnabled: false,
    });
    render(UsageDialog, { props: { onClose: vi.fn() } });
    expect(await screen.findByRole('status')).toHaveTextContent(/Telemetry is off/);
    expect(screen.getByText(/recording is off now/)).toBeInTheDocument();
  });

  it('gives the chart an accessible table fallback', async () => {
    vi.spyOn(persistence, 'usage').mockResolvedValue(usageFixture());
    render(UsageDialog, { props: { onClose: vi.fn() } });
    const chart = await screen.findByRole('img', { name: /Tokens per day/ });
    expect(chart.tagName.toLowerCase()).toBe('svg');
    const table = screen.getByRole('table', { name: 'Tokens per day' });
    const rows = within(table).getAllByRole('row');
    expect(rows).toHaveLength(8);
    expect(within(rows[1]).getByText('520,000')).toBeInTheDocument();
    expect(within(rows[2]).getByText('No data')).toBeInTheDocument();
  });

  it('lists projects or models and drills into sessions', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'usage').mockResolvedValue(usageFixture());
    render(UsageDialog, { props: { onClose: vi.fn() } });
    const detail = await screen.findByRole('region', { name: 'blackwall' });
    expect(within(detail).getByText('48.0% of this range')).toBeInTheDocument();
    expect(within(detail).getByText(/61 runs · 12 sessions · Agent 88%/)).toBeInTheDocument();
    expect(within(detail).getByText('session_7c41e09a')).toBeInTheDocument();
    expect(within(detail).getByText('llama3.3, qwen3.6 · 96 calls')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: /house-finder/ }));
    expect(screen.getByRole('region', { name: 'house-finder' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /No project/ })).toBeInTheDocument();
    expect(screen.queryByText(/tok\/s · p90/)).not.toBeInTheDocument();
    await user.click(screen.getByRole('radio', { name: 'Models' }));
    expect(screen.getByRole('region', { name: 'qwen3.6' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /llama3.3/ })).toBeInTheDocument();
  });

  it('shows calls, tokens, speed, p90 and errors per model', async () => {
    const user = userEvent.setup();
    const summary = usageFixture();
    summary.perModel[0].failedCalls = 9;
    vi.spyOn(persistence, 'usage').mockResolvedValue(summary);
    render(UsageDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('radio', { name: 'Models' }));
    const qwen = screen.getByRole('button', { name: /qwen3.6/ });
    expect(qwen).toHaveTextContent(
      '1,000 calls · 3.47M in / 429k out · 41 tok/s · p90 6.4 s · 0.9% errors',
    );
  });

  it('splits the range by Chat, Agent, and Subagent', async () => {
    vi.spyOn(persistence, 'usage').mockResolvedValue(usageFixture());
    render(UsageDialog, { props: { onClose: vi.fn() } });
    const modes = within(await screen.findByRole('list', { name: 'By mode' }));
    const items = modes.getAllByRole('listitem').map((item) => item.textContent?.trim());
    expect(items).toEqual([
      'Agent 4.24M · 128 runs · 1,040 calls',
      'Subagent 430k · 120 calls',
      'Chat 150k · 15 runs · 46 calls',
    ]);
  });

  it('does not claim an export status it does not know', async () => {
    vi.spyOn(persistence, 'usage').mockResolvedValue(usageFixture());
    render(UsageDialog, { props: { onClose: vi.fn() } });
    await screen.findByText('4.82M');
    expect(screen.queryByText(/Export:/)).not.toBeInTheDocument();
  });

  it('lists gaps and marks stale data', async () => {
    vi.spyOn(persistence, 'usage').mockResolvedValue({
      ...usageFixture(),
      gaps: { firstDay: '2026-10-22', droppedRuns: 1 },
      stale: true,
    });
    render(UsageDialog, { props: { onClose: vi.fn() } });
    const gaps = await screen.findByText(/Missing from this picture/);
    expect(gaps.parentElement).toHaveTextContent(
      /1 run dropped \(writer queue full\) · nothing before Oct 22, when usage tracking started/,
    );
    expect(screen.getByText('stale')).toBeInTheDocument();
  });

  it('lists past recording-off periods in the gaps line', async () => {
    const from = new Date(2026, 9, 24, 9, 5).getTime();
    const to = new Date(2026, 9, 24, 17, 40).getTime();
    vi.spyOn(persistence, 'usage').mockResolvedValue({
      ...usageFixture(),
      gaps: {
        firstDay: null,
        droppedRuns: 0,
        offPeriods: [
          { surface: 'desktop', fromUnixMs: from, toUnixMs: to },
          { surface: 'cli', fromUnixMs: to, toUnixMs: null },
        ],
      },
    });
    render(UsageDialog, { props: { onClose: vi.fn() } });
    const gaps = await screen.findByText(/Missing from this picture/);
    expect(gaps.parentElement).toHaveTextContent(
      /recording off Oct 24, 09:05 AM – Oct 24, 05:40 PM/,
    );
    expect(gaps.parentElement).toHaveTextContent(/bw recording off since Oct 24, 05:40 PM/);
  });

  it('shows the export card', async () => {
    vi.spyOn(persistence, 'usage').mockResolvedValue(usageFixture());
    const { telemetryClient } = await import('../persistence');
    vi.spyOn(telemetryClient, 'status').mockResolvedValue({
      endpoint: 'http://127.0.0.1:4318/',
      protocol: 'http/protobuf',
      traces: true,
      metrics: true,
      lastSuccessUnixMs: null,
      lastError: null,
      sentSpans: 0,
      droppedSpans: 0,
      sentMetrics: 0,
      droppedMetrics: 0,
    });
    render(UsageDialog, { props: { onClose: vi.fn() } });
    expect(await screen.findByText('http://127.0.0.1:4318/')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Send test span' })).toBeInTheDocument();
  });

  it('refreshes every 2 minutes while visible', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const usage = vi.spyOn(persistence, 'usage').mockResolvedValue(usageFixture());
    render(UsageDialog, { props: { onClose: vi.fn() } });
    await screen.findByText('4.82M');
    expect(usage).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(120_000);
    expect(usage).toHaveBeenCalledTimes(2);
    Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'hidden' });
    await vi.advanceTimersByTimeAsync(120_000);
    expect(usage).toHaveBeenCalledTimes(2);
    Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'visible' });
    document.dispatchEvent(new Event('visibilitychange'));
    await vi.advanceTimersByTimeAsync(0);
    expect(usage).toHaveBeenCalledTimes(3);
  });

  it('exports through the native folder picker and keeps the page on errors', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'usage').mockResolvedValue(usageFixture());
    const exported = vi.spyOn(persistence, 'exportTelemetry').mockResolvedValue({
      folder: '/Users/owner/Desktop/blackwall-telemetry-2026-10-22-to-2026-10-28',
      metricsFiles: 5,
      traceFiles: 12,
    });
    render(UsageDialog, { props: { onClose: vi.fn() } });
    await screen.findByText('4.82M');
    await user.click(screen.getByRole('button', { name: 'Export…' }));
    expect(exported).toHaveBeenCalledWith(7);
    expect(await screen.findByText(/Exported 5 metrics files and 12 traces/)).toBeInTheDocument();
    vi.spyOn(persistence, 'usage').mockRejectedValue('Usage could not be read.');
    await user.click(screen.getByRole('button', { name: 'Refresh' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Usage could not be read.');
    expect(screen.getByText('4.82M')).toBeInTheDocument();
  });
});
