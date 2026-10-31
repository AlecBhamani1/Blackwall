import { render, screen, waitFor, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import ConnectionSetup from './ConnectionSetup.svelte';
import { persistence } from '../persistence';
import { pairingClient } from '../pairing';
import type { SetupClient } from '../setup';
function client(): SetupClient {
  return {
    detectServer: vi.fn().mockResolvedValue(null),
    discover: vi.fn().mockResolvedValue([
      {
        name: 'Ollama',
        endpoint: 'http://127.0.0.1:11434/v1',
        available: true,
        models: ['test-model'],
        supportsDownload: true,
      },
    ]),
    openLink: vi.fn().mockResolvedValue(undefined),
    download: vi.fn().mockResolvedValue(undefined),
  };
}
describe('guided model setup', () => {
  it('retains a rejected key draft and distinguishes a Keychain save failure from a network failure', async () => {
    const user = userEvent.setup();
    let reject!: (reason: unknown) => void;
    const saveKey = vi
      .spyOn(persistence, 'saveKey')
      .mockImplementationOnce(
        () =>
          new Promise<{ saved: boolean }>((_, fail) => {
            reject = fail;
          }),
      )
      .mockResolvedValue({ saved: true });
    const onConnect = vi.fn().mockResolvedValue(true);
    render(ConnectionSetup, { props: { desktop: true, onConnect, models: ['test-model'] } });
    await user.click(screen.getByRole('button', { name: 'Enter a server address' }));
    await user.type(screen.getByLabelText('Model service address'), 'https://model.example/v1');
    await user.click(screen.getByText('Access key (if your service requires one)'));
    const input = screen.getByLabelText('Access key');
    await user.type(input, 'replacement');
    await user.click(screen.getByRole('button', { name: 'Connect and continue' }));
    expect(screen.getByRole('status')).toHaveTextContent('wait for confirmation');
    expect(screen.getByRole('button', { name: 'Verifying and saving key…' })).toBeDisabled();
    expect(onConnect).not.toHaveBeenCalled();
    reject('Blackwall could not save the access key in Keychain.');
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'could not save the access key in Keychain',
    );
    await waitFor(() => expect(screen.getByRole('alert')).toHaveFocus());
    expect(input).toHaveValue('replacement');
    expect(onConnect).not.toHaveBeenCalled();
    await user.click(screen.getByRole('button', { name: 'Connect and continue' }));
    await user.click(await screen.findByRole('button', { name: 'Use this model' }));
    expect(await screen.findByRole('heading', { name: 'You’re connected.' })).toBeVisible();
    expect(saveKey).toHaveBeenCalledTimes(2);
    expect(onConnect).toHaveBeenCalledOnce();
    saveKey.mockRestore();
  });
  it('shows the automatic startup connection failure when setup first opens', () => {
    render(ConnectionSetup, {
      props: {
        onConnect: vi.fn(),
        currentEndpoint: 'https://model.example/v1',
        connections: [{ endpoint: 'https://model.example/v1', name: 'Saved' }],
        connectionError: 'Check the macOS Keychain prompt, then reconnect.',
      },
    });
    expect(screen.getByRole('alert')).toHaveTextContent('Keychain');
    expect(
      screen
        .getByRole('alert')
        .compareDocumentPosition(screen.getByRole('button', { name: /Use this computer/ })) &
        Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Enter a server address' })).toBeEnabled();
  });
  it('shows a neutral first-run message before any connection is configured', () => {
    render(ConnectionSetup, {
      props: {
        onConnect: vi.fn(),
        currentEndpoint: 'http://localhost:11434/v1',
        connectionError: 'Blackwall could not reach your model.',
        found: [],
      },
    });
    expect(screen.getByText('No model server found on this computer.')).toBeVisible();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });
  it('waits for the local scan before showing the neutral first-run message', () => {
    const mock = client();
    mock.discover = vi.fn().mockReturnValue(new Promise(() => {}));
    render(ConnectionSetup, {
      props: {
        client: mock,
        desktop: true,
        onConnect: vi.fn(),
        currentEndpoint: 'http://localhost:11434/v1',
        connectionError: 'Blackwall could not reach your model.',
      },
    });
    expect(screen.queryByText('No model server found on this computer.')).not.toBeInTheDocument();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });
  it('starts a new server address empty with the localhost example as its placeholder', async () => {
    const user = userEvent.setup();
    render(ConnectionSetup, {
      props: { onConnect: vi.fn(), currentEndpoint: 'http://localhost:11434/v1' },
    });
    await user.click(screen.getByRole('button', { name: 'Enter a server address' }));
    expect(screen.getByLabelText('Model service address')).toHaveValue('');
    expect(screen.getByLabelText('Model service address')).toHaveAttribute(
      'placeholder',
      'http://localhost:11434/v1',
    );
  });
  it('prefills an equivalent normalized saved connection', async () => {
    const user = userEvent.setup();
    render(ConnectionSetup, {
      props: {
        onConnect: vi.fn(),
        currentEndpoint: 'http://host:11434',
        connections: [{ endpoint: 'http://host:11434/v1', name: 'Saved' }],
      },
    });
    await user.click(screen.getByRole('button', { name: 'Enter a server address' }));
    expect(screen.getByLabelText('Model service address')).toHaveValue('http://host:11434');
  });
  it('accepts a paired connection that advances to model selection', async () => {
    const user = userEvent.setup();
    pairingClient.snapshot.set({
      devices: [
        {
          id: 'paired',
          name: 'Home Mac',
          model: 'qwen3.6',
          endpoint: 'https://relay.example/s/paired/v1',
          role: 'client',
          state: 'active',
          online: false,
        },
      ],
      host: null,
      client: null,
      warnings: [],
    });
    render(ConnectionSetup, {
      props: {
        desktop: true,
        onConnect: vi.fn().mockResolvedValue(true),
        models: ['qwen3.6'],
      },
    });
    await user.click(screen.getByRole('button', { name: /Use another computer/ }));
    await user.click(screen.getByRole('button', { name: 'Connect to Home Mac' }));
    expect(await screen.findByRole('heading', { name: 'Choose a model.' })).toBeVisible();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });
  it('offers a way back when the connected server has no chat models', async () => {
    const user = userEvent.setup();
    render(ConnectionSetup, {
      props: { onConnect: vi.fn().mockResolvedValue(true), models: [] },
    });
    await user.click(screen.getByRole('button', { name: 'Enter a server address' }));
    await user.type(screen.getByLabelText('Model service address'), 'https://model.example/v1');
    await user.click(screen.getByRole('button', { name: 'Connect and continue' }));
    expect(await screen.findByText('This server has no chat models yet.')).toBeVisible();
    expect(screen.queryByRole('radio')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Back' }));
    expect(screen.getByRole('heading', { name: 'Connect a model service.' })).toBeVisible();
  });
  it('requires a model choice and saves the selected model', async () => {
    const user = userEvent.setup();
    const choose = vi.fn();
    render(ConnectionSetup, {
      props: {
        onConnect: vi.fn().mockResolvedValue(true),
        onChooseModel: choose,
        models: ['qwen3.6', 'llama3.2'],
      },
    });
    await user.click(screen.getByRole('button', { name: 'Enter a server address' }));
    await user.type(screen.getByLabelText('Model service address'), 'https://model.example/v1');
    await user.click(screen.getByRole('button', { name: 'Connect and continue' }));
    expect(await screen.findByRole('heading', { name: 'Choose a model.' })).toBeVisible();
    expect(screen.getByRole('button', { name: 'Use this model' })).toBeDisabled();
    await user.click(screen.getByRole('radio', { name: 'llama3.2' }));
    await user.click(screen.getByRole('button', { name: 'Use this model' }));
    expect(choose).toHaveBeenCalledWith('llama3.2');
    expect(screen.getByRole('heading', { name: 'You’re connected.' })).toBeVisible();
  });
  it('preselects the only chat model', async () => {
    const user = userEvent.setup();
    render(ConnectionSetup, {
      props: {
        onConnect: vi.fn().mockResolvedValue(true),
        models: ['qwen3.6'],
      },
    });
    await user.click(screen.getByRole('button', { name: 'Enter a server address' }));
    await user.type(screen.getByLabelText('Model service address'), 'https://model.example/v1');
    await user.click(screen.getByRole('button', { name: 'Connect and continue' }));
    expect(await screen.findByRole('radio', { name: 'qwen3.6' })).toBeChecked();
    expect(screen.getByRole('button', { name: 'Use this model' })).toBeEnabled();
  });
  it('preselects the saved model when reconnecting to a multi-model server', async () => {
    const user = userEvent.setup();
    render(ConnectionSetup, {
      props: {
        onConnect: vi.fn().mockResolvedValue(true),
        models: ['qwen3.6', 'llama3.2'],
        currentModel: 'llama3.2',
        connections: [{ endpoint: 'https://model.example/v1', name: 'Saved' }],
      },
    });
    await user.click(screen.getByRole('button', { name: 'Connect to Saved' }));
    expect(await screen.findByRole('radio', { name: 'llama3.2' })).toBeChecked();
    expect(screen.getByRole('heading', { name: 'Choose a model.' })).toBeVisible();
  });
  it('focuses Keychain recovery guidance and allows an explicit connection retry', async () => {
    const user = userEvent.setup();
    const onConnect = vi
      .fn()
      .mockRejectedValueOnce({ code: 'credential_error' })
      .mockResolvedValue(true);
    render(ConnectionSetup, { props: { client: client(), desktop: true, onConnect } });
    await user.click(screen.getByRole('button', { name: /Use this computer/ }));
    await user.click(await screen.findByRole('button', { name: 'Use these models' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Keychain');
    expect(screen.getByRole('alert')).toHaveFocus();
    expect(screen.getByRole('button', { name: 'Use these models' })).toBeEnabled();
    expect(onConnect).toHaveBeenCalledTimes(1);
    await user.click(screen.getByRole('button', { name: 'Use these models' }));
    await user.click(await screen.findByRole('button', { name: 'Use this model' }));
    expect(await screen.findByRole('heading', { name: 'You’re connected.' })).toHaveFocus();
    expect(onConnect).toHaveBeenCalledTimes(2);
  });
  it('connects a detected local service without an address field', async () => {
    const user = userEvent.setup();
    const onConnect = vi.fn().mockResolvedValue(true);
    const onDone = vi.fn();
    render(ConnectionSetup, { props: { client: client(), desktop: true, onConnect, onDone } });
    expect(screen.queryByLabelText('Model service address')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: /Use this computer/ }));
    await user.click(await screen.findByRole('button', { name: 'Use these models' }));
    expect(onConnect).toHaveBeenCalledWith('http://127.0.0.1:11434/v1', 'This computer');
    await user.click(await screen.findByRole('button', { name: 'Use this model' }));
    expect(await screen.findByRole('heading', { name: 'You’re connected.' })).toHaveFocus();
    await user.click(screen.getByRole('button', { name: /Start chatting/ }));
    expect(onDone).toHaveBeenCalledOnce();
  });
  it('does not apply an abandoned manual access key to a saved computer', async () => {
    const user = userEvent.setup();
    const saveKey = vi.spyOn(persistence, 'saveKey').mockResolvedValue({ saved: true });
    const onConnect = vi.fn().mockResolvedValue(true);
    render(ConnectionSetup, {
      props: {
        desktop: true,
        onConnect,
        connections: [{ endpoint: 'https://relay.example/s/saved/v1', name: 'Home Mac' }],
      },
    });
    await user.click(screen.getByRole('button', { name: 'Enter a server address' }));
    await user.click(screen.getByText('Access key (if your service requires one)'));
    await user.type(screen.getByLabelText('Access key'), 'unfinished-manual-key');
    await user.click(screen.getByRole('button', { name: 'All connection options' }));
    await user.click(screen.getByRole('button', { name: 'Connect to Home Mac' }));
    expect(onConnect).toHaveBeenCalledWith('https://relay.example/s/saved/v1', 'Home Mac');
    expect(saveKey).not.toHaveBeenCalled();
    saveKey.mockRestore();
  });
  it('offers installation and recheck when no service is available', async () => {
    const mock = client();
    mock.discover = vi.fn().mockResolvedValue([]);
    const user = userEvent.setup();
    render(ConnectionSetup, { props: { client: mock, desktop: true, onConnect: vi.fn() } });
    await user.click(screen.getByRole('button', { name: /Use this computer/ }));
    await user.click(await screen.findByRole('button', { name: /Get Ollama/ }));
    expect(mock.openLink).toHaveBeenCalledWith('download');
    await user.click(screen.getByRole('button', { name: /Check again/ }));
    // Once when setup opened, once for this step, and once more for the recheck.
    expect(mock.discover).toHaveBeenCalledTimes(3);
  });
  it('only downloads after explicit action and connects the chosen model', async () => {
    const mock = client();
    mock.discover = vi.fn().mockResolvedValue([
      {
        name: 'Ollama',
        endpoint: 'http://127.0.0.1:11434/v1',
        available: true,
        models: [],
        supportsDownload: true,
      },
    ]);
    const user = userEvent.setup();
    const choose = vi.fn();
    render(ConnectionSetup, {
      props: {
        client: mock,
        desktop: true,
        onConnect: vi.fn().mockResolvedValue(true),
        onChooseModel: choose,
      },
    });
    await user.click(screen.getByRole('button', { name: /Use this computer/ }));
    const download = await screen.findByRole('button', { name: 'Download and connect' });
    expect(mock.download).not.toHaveBeenCalled();
    await user.click(download);
    expect(mock.download).toHaveBeenCalledWith(
      'llama3.2:1b',
      expect.any(Function),
      expect.any(AbortSignal),
    );
    expect(choose).toHaveBeenCalledWith('llama3.2:1b');
  });
  it('lists every server found on this computer with its own Connect button', async () => {
    const user = userEvent.setup();
    const onConnect = vi.fn().mockResolvedValue(true);
    const found = [
      {
        name: 'Ollama',
        endpoint: 'http://127.0.0.1:11434/v1',
        available: true,
        models: ['qwen3.6:35b-a3b-nvfp4'],
        supportsDownload: true,
        kind: 'ollama' as const,
        loaded: ['qwen3.6:35b-a3b-nvfp4'],
        contextWindow: 65_536,
      },
      {
        name: 'llama.cpp',
        endpoint: 'http://127.0.0.1:18040/v1',
        available: true,
        models: ['qwen3.8-flash-next-iq2_xs'],
        supportsDownload: false,
        kind: 'llama.cpp' as const,
        loaded: ['qwen3.8-flash-next-iq2_xs'],
      },
    ];
    const mock = client();
    render(ConnectionSetup, { props: { client: mock, desktop: true, onConnect, found } });
    const list = within(screen.getByRole('region', { name: 'Found on this computer' }));
    expect(list.getByText('127.0.0.1:11434')).toBeVisible();
    expect(list.getByText(/Loaded: qwen3\.6:35b-a3b-nvfp4 · 65,536-token context/)).toBeVisible();
    expect(list.getByText('127.0.0.1:18040')).toBeVisible();
    expect(list.getByText(/Loaded: qwen3\.8-flash-next-iq2_xs/)).toBeVisible();
    // Servers found at startup are not looked up again.
    expect(mock.discover).not.toHaveBeenCalled();
    await user.click(list.getByRole('button', { name: 'Connect to llama.cpp at 127.0.0.1:18040' }));
    expect(onConnect).toHaveBeenCalledWith('http://127.0.0.1:18040/v1', 'llama.cpp :18040');
    await user.click(await screen.findByRole('button', { name: 'Use this model' }));
    expect(await screen.findByRole('heading', { name: 'You’re connected.' })).toBeVisible();
  });
  it('looks for servers when it opens and sends an empty Ollama to the download step', async () => {
    const user = userEvent.setup();
    const mock = client();
    mock.discover = vi.fn().mockResolvedValue([
      {
        name: 'Ollama',
        endpoint: 'http://127.0.0.1:11434/v1',
        available: true,
        models: [],
        supportsDownload: true,
      },
    ]);
    const onConnect = vi.fn();
    render(ConnectionSetup, { props: { client: mock, desktop: true, onConnect } });
    await user.click(
      await screen.findByRole('button', { name: 'Connect to Ollama at 127.0.0.1:11434' }),
    );
    expect(await screen.findByRole('button', { name: 'Download and connect' })).toBeVisible();
    expect(onConnect).not.toHaveBeenCalled();
  });
  it('keeps invalid invitation secrets out of navigation', async () => {
    const user = userEvent.setup();
    const mock = client();
    render(ConnectionSetup, { props: { client: mock, onConnect: vi.fn() } });
    await user.click(screen.getByRole('button', { name: /I have an invitation/ }));
    await user.type(screen.getByLabelText('Invitation link'), 'javascript:alert(1)');
    await user.click(screen.getByRole('button', { name: 'Open invitation' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('complete Blackwall invitation');
    expect(mock.openLink).not.toHaveBeenCalled();
  });
  it('says an access key was not saved when there is no keyring', async () => {
    const user = userEvent.setup();
    const saveKey = vi.spyOn(persistence, 'saveKey').mockResolvedValue({ saved: false });
    render(ConnectionSetup, {
      props: {
        desktop: true,
        onConnect: vi.fn().mockResolvedValue(true),
        models: ['test-model'],
      },
    });
    await user.click(screen.getByRole('button', { name: 'Enter a server address' }));
    await user.type(screen.getByLabelText('Model service address'), 'https://model.example/v1');
    await user.click(screen.getByText('Access key (if your service requires one)'));
    await user.type(screen.getByLabelText('Access key'), 'session-key');
    await user.click(screen.getByRole('button', { name: 'Connect and continue' }));
    await user.click(await screen.findByRole('button', { name: 'Use this model' }));
    expect(await screen.findByRole('heading', { name: 'You’re connected.' })).toBeVisible();
    expect(screen.getByText('Not saved: no system keyring')).toBeVisible();
    saveKey.mockRestore();
  });
});
