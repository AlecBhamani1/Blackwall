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
    expect(tiles.getByText('3 errors · 2 stopped at step limit')).toBeInTheDocument();
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
    await user.click(screen.getByRole('radio', { name: '30 days' }));
    expect(usage).toHaveBeenLastCalledWith(30);
    await user.click(screen.getByRole('radio', { name: '7 days' }));
    expect(usage).toHaveBeenLastCalledWith(7);
    expect(screen.getByRole('radio', { name: '7 days' })).toBeChecked();
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
    await user.click(screen.getByRole('radio', { name: 'Models' }));
    expect(screen.getByRole('region', { name: 'qwen3.6' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /llama3.3/ })).toBeInTheDocument();
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
