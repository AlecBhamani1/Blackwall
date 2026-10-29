import { render, screen, waitFor } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import type { TelemetryPreferences } from '../persistence';
import TelemetrySettings from './TelemetrySettings.svelte';

function client(
  saved: Partial<TelemetryPreferences['export']> = {},
  retention: number | 'forever' = 400,
) {
  return {
    available: () => true,
    load: vi.fn().mockResolvedValue({
      export: {
        protocol: 'http/protobuf',
        traces: true,
        metrics: true,
        headers: [],
        includeProject: false,
        ...saved,
      },
      retention,
    }),
    save: vi.fn().mockResolvedValue(undefined),
    saveHeader: vi.fn(async (name: string) => name.toLowerCase()),
    deleteHeader: vi.fn().mockResolvedValue(undefined),
    status: vi.fn(),
    test: vi.fn().mockResolvedValue(200),
  };
}

describe('telemetry settings', () => {
  it('starts off and saves an endpoint, protocol, signals, project names, and retention', async () => {
    const user = userEvent.setup();
    const api = client();
    render(TelemetrySettings, { client: api });
    const endpoint = await screen.findByLabelText('Endpoint');
    await waitFor(() => expect(api.load).toHaveBeenCalled());
    expect(endpoint).toHaveValue('');
    expect(screen.getByRole('checkbox', { name: /Include project names/ })).not.toBeChecked();
    expect(screen.getByRole('spinbutton')).toHaveValue(400);
    expect(screen.getByRole('button', { name: 'Send test span' })).toBeDisabled();
    await user.type(endpoint, 'http://127.0.0.1:4318');
    await user.selectOptions(screen.getByLabelText('Protocol'), 'http/json');
    await user.click(screen.getByRole('checkbox', { name: 'Metrics' }));
    await user.click(screen.getByRole('checkbox', { name: /Include project names/ }));
    await user.click(screen.getByRole('checkbox', { name: 'Forever' }));
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(api.save).toHaveBeenCalledWith({
      export: {
        endpoint: 'http://127.0.0.1:4318',
        protocol: 'http/json',
        traces: true,
        metrics: false,
        headers: [],
        includeProject: true,
      },
      retention: 'forever',
    });
    expect(await screen.findByText('Saved. New runs are exported.')).toBeInTheDocument();
  });

  it('refuses retention outside 30 to 3,650 days', async () => {
    const user = userEvent.setup();
    const api = client({}, 90);
    render(TelemetrySettings, { client: api });
    const days = await screen.findByRole('spinbutton');
    await waitFor(() => expect(days).toHaveValue(90));
    await user.clear(days);
    await user.type(days, '10');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(screen.getByRole('alert')).toHaveTextContent('30 to 3,650 days');
    expect(api.save).not.toHaveBeenCalled();
  });

  it('stores a header value once and lists only masked names', async () => {
    const user = userEvent.setup();
    const api = client({ endpoint: 'https://otel.example.com/', headers: ['x-team'] });
    render(TelemetrySettings, { client: api });
    expect(await screen.findByText('x-team')).toBeInTheDocument();
    await user.type(screen.getByLabelText('Header name'), 'X-Api-Key');
    await user.type(screen.getByLabelText('Header value'), 'very-secret-value');
    await user.click(screen.getByRole('button', { name: 'Add header' }));
    expect(api.saveHeader).toHaveBeenCalledWith('X-Api-Key', 'very-secret-value');
    await screen.findByText('x-api-key');
    expect(api.save).toHaveBeenLastCalledWith(
      expect.objectContaining({
        export: expect.objectContaining({ headers: ['x-team', 'x-api-key'] }),
      }),
    );
    expect(JSON.stringify(api.save.mock.calls)).not.toContain('very-secret-value');
    expect(screen.getByLabelText('Header value')).toHaveValue('');
    expect(document.body.textContent).not.toContain('very-secret-value');
    await user.click(screen.getByRole('button', { name: 'Remove x-team' }));
    expect(api.deleteHeader).toHaveBeenCalledWith('x-team');
    await waitFor(() => expect(screen.queryByText('x-team')).not.toBeInTheDocument());
    expect(api.save).toHaveBeenLastCalledWith(
      expect.objectContaining({ export: expect.objectContaining({ headers: ['x-api-key'] }) }),
    );
  });

  it('sends a test span and shows the HTTP status or the error', async () => {
    const user = userEvent.setup();
    const api = client({ endpoint: 'https://otel.example.com/' });
    render(TelemetrySettings, { client: api });
    const send = await screen.findByRole('button', { name: 'Send test span' });
    await waitFor(() => expect(send).toBeEnabled());
    await user.click(send);
    expect(await screen.findByText('Test span sent · HTTP 200')).toBeInTheDocument();
    expect(api.test).toHaveBeenCalledWith(
      expect.objectContaining({ endpoint: 'https://otel.example.com/' }),
    );
    api.test.mockRejectedValueOnce('The collector answered HTTP 401.');
    await user.click(send);
    expect(await screen.findByText('The collector answered HTTP 401.')).toBeInTheDocument();
  });
});
