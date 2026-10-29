import { render, screen, waitFor } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import type { ExportSettings, SavedHeader, TelemetryView } from '../persistence';
import TelemetrySettings from './TelemetrySettings.svelte';

const ORIGIN = 'https://otel.example.com:443';

/** A fake backend that keeps state like the desktop commands: headers bound to the origin. */
function client(saved: Partial<ExportSettings> = {}, extra: Partial<TelemetryView> = {}) {
  let view: TelemetryView = {
    export: {
      endpoint: null,
      protocol: 'http/protobuf',
      traces: true,
      metrics: true,
      headers: [],
      includeProject: false,
      ...saved,
    },
    retention: 400,
    origin: saved.endpoint ? ORIGIN : null,
    contentCapture: false,
    ...extra,
  };
  return {
    available: () => true,
    load: vi.fn(async () => view),
    save: vi.fn(async (settings: { export: ExportSettings; retention: number | 'forever' }) => {
      view = {
        ...view,
        export: { ...settings.export, headers: view.export.headers },
        retention: settings.retention,
        origin: settings.export.endpoint
          ? `${new URL(settings.export.endpoint).protocol}//${new URL(settings.export.endpoint).hostname}:${new URL(settings.export.endpoint).port || 443}`
          : null,
      };
      return view;
    }),
    saveHeader: vi.fn(async (name: string) => {
      const header = { name: name.toLowerCase(), origin: view.origin as string };
      view = {
        ...view,
        export: {
          ...view.export,
          headers: [...view.export.headers.filter((h) => h.name !== header.name), header],
        },
      };
      return view;
    }),
    deleteHeader: vi.fn(async (header: SavedHeader) => {
      view = {
        ...view,
        export: {
          ...view.export,
          headers: view.export.headers.filter(
            (h) => h.name !== header.name || h.origin !== header.origin,
          ),
        },
      };
      return view;
    }),
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
    expect(screen.getByLabelText('Header value')).toBeDisabled();
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
    expect(screen.getByRole('button', { name: 'Send test span' })).toBeEnabled();
  });

  it('refuses retention outside 30 to 3,650 days', async () => {
    const user = userEvent.setup();
    const api = client();
    api.load.mockResolvedValueOnce({ ...(await api.load()), retention: 90 });
    render(TelemetrySettings, { client: api });
    const days = await screen.findByRole('spinbutton');
    await waitFor(() => expect(days).toHaveValue(90));
    await user.clear(days);
    await user.type(days, '10');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(screen.getByRole('alert')).toHaveTextContent('30 to 3,650 days');
    expect(api.save).not.toHaveBeenCalled();
  });

  it('stores a header value once, for the saved origin, and lists only masked names', async () => {
    const user = userEvent.setup();
    const api = client({
      endpoint: 'https://otel.example.com/',
      headers: [{ name: 'x-team', origin: ORIGIN }],
    });
    render(TelemetrySettings, { client: api });
    expect(await screen.findByText('x-team')).toBeInTheDocument();
    expect(screen.getByText(`Saved for ${ORIGIN} only.`)).toBeInTheDocument();
    await user.type(screen.getByLabelText('Header name'), 'X-Api-Key');
    await user.type(screen.getByLabelText('Header value'), 'very-secret-value');
    await user.click(screen.getByRole('button', { name: 'Add header' }));
    expect(api.saveHeader).toHaveBeenCalledWith('X-Api-Key', 'very-secret-value');
    await screen.findByText('x-api-key');
    expect(screen.getByLabelText('Header value')).toHaveValue('');
    expect(document.body.textContent).not.toContain('very-secret-value');
    await user.click(screen.getByRole('button', { name: 'Remove x-team' }));
    expect(api.deleteHeader).toHaveBeenCalledWith({ name: 'x-team', origin: ORIGIN });
    await waitFor(() => expect(screen.queryByText('x-team')).not.toBeInTheDocument());
    // Editing the endpoint blocks new headers until it is saved.
    await user.type(screen.getByLabelText('Endpoint'), 'x');
    expect(screen.getByLabelText('Header value')).toBeDisabled();
    expect(screen.getByText(/Save an endpoint first/)).toBeInTheDocument();
  });

  it('shows headers saved for another origin and offers to re-enter or remove them', async () => {
    const user = userEvent.setup();
    const api = client({
      endpoint: 'https://otel.example.com/',
      headers: [{ name: 'authorization', origin: 'https://old.example:443' }],
    });
    render(TelemetrySettings, { client: api });
    expect(
      await screen.findByText('Saved for https://old.example:443. Not sent; export is paused.'),
    ).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Enter authorization again' }));
    expect(screen.getByLabelText('Header name')).toHaveValue('authorization');
    expect(screen.getByLabelText('Header value')).toHaveFocus();
    await user.type(screen.getByLabelText('Header value'), 'new-value');
    await user.click(screen.getByRole('button', { name: 'Add header' }));
    await waitFor(() =>
      expect(screen.queryByText(/Saved for https:\/\/old/)).not.toBeInTheDocument(),
    );
    expect(screen.getByText('authorization')).toBeInTheDocument();
  });

  it('asks before export sends prompts and outputs when content capture is on', async () => {
    const user = userEvent.setup();
    const api = client({}, { contentCapture: true });
    render(TelemetrySettings, { client: api });
    const endpoint = await screen.findByLabelText('Endpoint');
    await waitFor(() => expect(api.load).toHaveBeenCalled());
    await user.type(endpoint, 'https://otel.example.com');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    const confirm = screen.getByRole('alertdialog', { name: 'Send prompts and outputs?' });
    expect(confirm).toHaveTextContent('prompts, outputs, and tool data from new runs will be sent');
    expect(api.save).not.toHaveBeenCalled();
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(api.save).not.toHaveBeenCalled();
    await user.click(screen.getByRole('button', { name: 'Save' }));
    await user.click(screen.getByRole('button', { name: 'Save and send content' }));
    expect(api.save).toHaveBeenCalledTimes(1);
    expect(await screen.findByText('Saved. New runs are exported.')).toBeInTheDocument();
    // Saving again without moving the endpoint does not ask again.
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument();
    expect(api.save).toHaveBeenCalledTimes(2);
  });

  it('tests only the saved settings and shows the HTTP status or the error', async () => {
    const user = userEvent.setup();
    const api = client({ endpoint: 'https://otel.example.com/' });
    render(TelemetrySettings, { client: api });
    const send = await screen.findByRole('button', { name: 'Send test span' });
    await waitFor(() => expect(send).toBeEnabled());
    await user.click(send);
    expect(await screen.findByText('Test span sent · HTTP 200')).toBeInTheDocument();
    expect(api.test).toHaveBeenCalledWith();
    api.test.mockRejectedValueOnce('The collector answered HTTP 401.');
    await user.click(send);
    expect(await screen.findByText('The collector answered HTTP 401.')).toBeInTheDocument();
    await user.type(screen.getByLabelText('Endpoint'), 'x');
    expect(send).toBeDisabled();
    expect(screen.getByText(/Save first to test changes/)).toBeInTheDocument();
  });
});
