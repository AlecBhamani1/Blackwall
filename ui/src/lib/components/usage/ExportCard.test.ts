import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import type { ExportStatus } from '../../persistence';
import ExportCard from './ExportCard.svelte';

function status(overrides: Partial<ExportStatus> = {}): ExportStatus {
  return {
    endpoint: 'https://otel.example.com/',
    protocol: 'http/protobuf',
    traces: true,
    metrics: true,
    lastSuccessUnixMs: Date.now() - 4 * 60_000,
    lastError: null,
    sentSpans: 1240,
    droppedSpans: 0,
    sentMetrics: 12,
    droppedMetrics: 0,
    ...overrides,
  };
}

function client(next: ExportStatus) {
  return {
    available: () => true,
    load: vi.fn().mockResolvedValue({ export: { endpoint: next.endpoint }, retention: 400 }),
    save: vi.fn(),
    saveHeader: vi.fn(),
    deleteHeader: vi.fn(),
    status: vi.fn().mockResolvedValue(next),
    test: vi.fn().mockResolvedValue(200),
  };
}

describe('export card', () => {
  it('shows the endpoint, last success, counts, and no error', async () => {
    render(ExportCard, { client: client(status()) });
    expect(await screen.findByText('https://otel.example.com/')).toBeInTheDocument();
    expect(screen.getByText('4 min ago')).toBeInTheDocument();
    expect(screen.getByText('none')).toBeInTheDocument();
    expect(screen.getByText(/1,240 spans and 12 metric batches\s+sent/)).toBeInTheDocument();
  });

  it('shows the last error as its kind and HTTP status only', async () => {
    const api = client(
      status({
        lastSuccessUnixMs: null,
        lastError: { kind: 'http_status', status: 401, signal: 'traces', unixMs: Date.now() },
        droppedSpans: 3,
      }),
    );
    render(ExportCard, { client: api });
    expect(
      await screen.findByText(/collector refused \(HTTP 401\) · just now/),
    ).toBeInTheDocument();
    expect(screen.getByText('nothing sent yet')).toBeInTheDocument();
    expect(screen.getByText(/3 spans and 0 metric batches dropped/)).toBeInTheDocument();
  });

  it('points to Settings when export is off', async () => {
    render(ExportCard, { client: client(status({ endpoint: null })) });
    expect(await screen.findByText(/Settings → Privacy & security → Telemetry/)).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Send test span' })).not.toBeInTheDocument();
  });

  it('sends a test span with the saved settings and refreshes', async () => {
    const user = userEvent.setup();
    const api = client(status());
    render(ExportCard, { client: api });
    await user.click(await screen.findByRole('button', { name: 'Send test span' }));
    expect(await screen.findByRole('status')).toHaveTextContent('Test span sent · HTTP 200');
    expect(api.test).toHaveBeenCalledWith();
    expect(api.status).toHaveBeenCalledTimes(2);
    api.test.mockRejectedValueOnce('The collector could not be reached.');
    await user.click(screen.getByRole('button', { name: 'Send test span' }));
    expect(await screen.findByRole('status')).toHaveTextContent('could not be reached');
  });
});
