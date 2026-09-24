import { get } from 'svelte/store';
import { describe, expect, it, vi } from 'vitest';
import type { LocalModelClient } from './ipc';
import { createChatController } from './store';
import { nativeModelError } from './modelError';

function mockClient(): LocalModelClient {
  return {
    modelEndpoint: vi.fn().mockResolvedValue('http://localhost:11434/v1'),
    discoverModels: vi
      .fn()
      .mockResolvedValue([{ id: 'remote-model:latest', name: 'remote-model:latest' }]),
    streamChat: vi.fn(async (_request, callbacks) => {
      callbacks.onDelta('Blackwall ');
      callbacks.onDelta('is ready.');
      callbacks.onComplete?.();
    }),
  };
}

describe('chat controller', () => {
  it('preserves saved pairing after a Keychain failure and reconnects without replay', async () => {
    const client = mockClient();
    const controller = createChatController(client);
    const endpoint = 'https://relay.example/s/bws_saved/v1';
    await controller.configureEndpoint(endpoint, 'My paired computer');
    vi.mocked(client.streamChat).mockRejectedValueOnce(
      nativeModelError({ code: 'credential_error' }),
    );
    await controller.send('Keep this failed turn', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    expect(get(controller.connectionState)).toBe('offline');
    expect(get(controller.notice)).toContain('Keychain');
    vi.mocked(client.discoverModels).mockRejectedValueOnce({
      code: 'credential_error',
      message: 'private key',
    });
    expect(await controller.configureEndpoint(endpoint)).toBe(false);
    expect(get(controller.connectionError)).toContain('Keychain');
    expect(get(controller.preferences).connections).toContainEqual(
      expect.objectContaining({ endpoint, name: 'My paired computer' }),
    );
    expect(get(controller.messages).at(-1)?.status).toBe('error');
    expect(await controller.configureEndpoint(endpoint)).toBe(true);
    expect(client.streamChat).toHaveBeenCalledTimes(1);
    await controller.send('Explicit next request', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    expect(get(controller.messages).at(-1)?.status).toBe('complete');
    expect(client.streamChat).toHaveBeenCalledTimes(2);
    controller.destroy();
  });
  it.each(['', ' \n'])(
    'keeps an empty completion recoverable without replay (%j)',
    async (delta) => {
      const client = mockClient();
      vi.mocked(client.streamChat).mockImplementationOnce(async (_request, callbacks) => {
        callbacks.onDelta(delta);
        callbacks.onComplete?.();
      });
      const controller = createChatController(client);
      await controller.initialize();
      await controller.send('Read these attachments', []);
      await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
      expect(get(controller.messages).at(-1)?.status).toBe('error');
      expect(get(controller.notice)).toContain('Try a shorter prompt');
      expect(get(controller.connectionState)).toBe('ready');
      expect(client.streamChat).toHaveBeenCalledTimes(1);
      await controller.send('Try this explicit request', []);
      await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
      expect(get(controller.messages).at(-1)?.status).toBe('complete');
      expect(client.streamChat).toHaveBeenCalledTimes(2);
      controller.destroy();
    },
  );
  it('keeps a failed saved reconnect offline even with cached models', async () => {
    const client = mockClient();
    const controller = createChatController(client);
    await controller.configureEndpoint('http://paired:11434/v1', 'My computer');
    vi.mocked(client.streamChat).mockRejectedValueOnce(
      nativeModelError({ code: 'model_unavailable' }),
    );
    await controller.send('Interrupt this response', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    vi.mocked(client.discoverModels).mockRejectedValue(new Error('Connection refused'));
    expect(await controller.configureEndpoint('http://paired:11434/v1')).toBe(false);
    expect(get(controller.connectionState)).toBe('offline');
    expect(await controller.send('Must not send while locked', [])).toBe(false);
    expect(client.streamChat).toHaveBeenCalledTimes(1);
    controller.destroy();
  });
  it('marks the current connection offline when its own health check fails', async () => {
    const client = mockClient();
    const controller = createChatController(client);
    await controller.configureEndpoint('http://paired:11434/v1');
    vi.mocked(client.discoverModels).mockRejectedValue(new Error('Connection refused'));
    expect(await controller.configureEndpoint('http://paired:11434/v1/')).toBe(false);
    expect(get(controller.connectionState)).toBe('offline');
    controller.destroy();
  });
  it('marks a lost model connection offline and reconnects without replaying the failed turn', async () => {
    const client = mockClient();
    vi.mocked(client.streamChat).mockRejectedValueOnce(
      nativeModelError({ code: 'model_unavailable' }),
    );
    const controller = createChatController(client);
    await controller.initialize();
    await controller.send('Only send this once', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    expect(get(controller.connectionState)).toBe('offline');
    expect(get(controller.connectionError)).toContain('Open and unlock');
    expect(get(controller.messages).at(-1)?.status).toBe('error');
    await controller.initialize();
    expect(get(controller.connectionState)).toBe('ready');
    expect(client.streamChat).toHaveBeenCalledTimes(1);
    controller.destroy();
  });
  it('discovers the configured endpoint and streams a complete turn', async () => {
    const client = mockClient();
    const controller = createChatController(client);

    await controller.initialize();
    expect(get(controller.connectionState)).toBe('ready');
    expect(get(controller.selectedModel)).toBe('remote-model:latest');

    await expect(controller.send('Hello', [])).resolves.toBe(true);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));

    const messages = get(controller.messages);
    expect(messages).toHaveLength(2);
    expect(messages[0]).toMatchObject({ role: 'user', content: 'Hello' });
    expect(messages[1]).toMatchObject({
      role: 'assistant',
      content: 'Blackwall is ready.',
      status: 'complete',
    });
    expect(get(controller.sessions)[0]?.title).toBe('Hello');
  });

  it('persists an endpoint override and uses it for discovery and chat', async () => {
    const client = mockClient();
    const controller = createChatController(client);
    const endpoint = 'http://100.76.24.116:11434';

    await expect(controller.configureEndpoint(endpoint)).resolves.toBe(true);
    expect(client.discoverModels).toHaveBeenCalledWith(endpoint);
    expect(window.localStorage.getItem('blackwall.endpoint.v1')).toBe(endpoint);

    await controller.send('Use the configured host', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    expect(client.streamChat).toHaveBeenCalledWith(
      expect.objectContaining({ endpoint }),
      expect.any(Object),
      expect.any(AbortSignal),
    );
  });

  it('explains how to recover from an unavailable model', async () => {
    const client = mockClient();
    client.discoverModels = vi
      .fn()
      .mockRejectedValue(new Error('connection refused at 127.0.0.1:11434'));
    const controller = createChatController(client);

    await expect(controller.initialize()).resolves.toBe(false);
    expect(get(controller.connectionState)).toBe('offline');
    expect(get(controller.connectionError)).toContain('Make sure the model service is open');
    expect(get(controller.notice)).toBe(get(controller.connectionError));
  });

  it('finishes visible tool and child activity when the parent request fails', async () => {
    const client = mockClient();
    client.streamChat = vi.fn(async (request, callbacks) => {
      callbacks.onEvent?.({
        type: 'tool_call',
        requestId: request.requestId,
        toolCallId: 'read',
        name: 'read_file',
        arguments: '{}',
      });
      callbacks.onEvent?.({
        type: 'subagent_status',
        requestId: request.requestId,
        agentId: 'child',
        state: 'running',
      });
      throw new Error('The model stream ended unexpectedly.');
    });
    const controller = createChatController(client);
    await controller.initialize();
    await controller.send('Inspect this project', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    expect(get(controller.messages).at(-1)).toMatchObject({
      status: 'error',
      tools: [{ id: 'read', status: 'error' }],
      children: [{ id: 'child', state: 'interrupted' }],
    });
    expect(get(controller.connectionState)).toBe('ready');
    expect(get(controller.approval)).toBeNull();
  });

  it('does not send whitespace or send while disconnected', async () => {
    const client = mockClient();
    const controller = createChatController(client);

    await expect(controller.send('   ', [])).resolves.toBe(false);
    await expect(controller.send('Hello', [])).resolves.toBe(false);
    expect(client.streamChat).not.toHaveBeenCalled();
  });

  it('marks an interrupted response as stopped', async () => {
    const client = mockClient();
    client.streamChat = vi.fn(
      (_request, _callbacks, signal) =>
        new Promise<void>((_resolve, reject) => {
          signal.addEventListener('abort', () => reject(new DOMException('Stopped', 'AbortError')));
        }),
    );
    const controller = createChatController(client);
    await controller.initialize();
    await controller.send('Keep it short', []);

    controller.stop();
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    expect(get(controller.messages).at(-1)).toMatchObject({
      status: 'stopped',
      content: 'Response stopped.',
    });
  });
  it('remembers the selected model separately for each named connection', async () => {
    const client = mockClient();
    client.discoverModels = vi.fn().mockResolvedValue([
      { id: 'small', name: 'Small' },
      { id: 'large', name: 'Large' },
    ]);
    const controller = createChatController(client);
    await controller.configureEndpoint('http://first:11434', 'First computer');
    controller.chooseModel('large');
    await controller.configureEndpoint('http://second:11434', 'Second computer');
    controller.chooseModel('small');
    await controller.configureEndpoint('http://first:11434', 'First computer');
    expect(get(controller.selectedModel)).toBe('large');
    await controller.configureEndpoint('http://second:11434', 'Second computer');
    expect(get(controller.selectedModel)).toBe('small');
  });

  it('keeps the working connection when a replacement fails', async () => {
    const client = mockClient();
    const controller = createChatController(client);
    await controller.configureEndpoint('http://working:11434');
    client.discoverModels = vi.fn().mockRejectedValue(new Error('Connection refused'));
    expect(await controller.configureEndpoint('http://unreachable:11434')).toBe(false);
    expect(get(controller.endpoint)).toBe('http://working:11434');
    expect(window.localStorage.getItem('blackwall.endpoint.v1')).toBe('http://working:11434');
    expect(get(controller.connectionState)).toBe('ready');
    expect(get(controller.models)).toHaveLength(1);
  });

  it('does not resurrect a deleted active conversation', async () => {
    const controller = createChatController(mockClient());
    await controller.initialize();
    await controller.send('Delete this chat', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    const id = get(controller.activeSessionId);
    controller.removeSession(id);
    expect(get(controller.sessions).some((session) => session.id === id)).toBe(false);
    expect(JSON.parse(window.localStorage.getItem('blackwall.sessions.v1') ?? '[]')).toHaveLength(
      0,
    );
    expect(get(controller.messages)).toHaveLength(0);
  });

  it('ignores late events and completion from a stopped conversation', async () => {
    const client = mockClient();
    const turns: Array<{ delta: (text: string) => void; finish: () => void }> = [];
    client.streamChat = vi.fn(
      (_request, callbacks) =>
        new Promise<void>((resolve) => {
          turns.push({ delta: callbacks.onDelta, finish: resolve });
        }),
    );
    const controller = createChatController(client);
    await controller.initialize();
    await controller.send('First', []);
    controller.newChat();
    await controller.send('Second', []);
    turns[0].delta('Stale answer');
    turns[0].finish();
    await Promise.resolve();
    expect(get(controller.runState)).toBe('streaming');
    expect(get(controller.messages).some((message) => message.content.includes('Stale'))).toBe(
      false,
    );
    turns[1].delta('Current answer');
    turns[1].finish();
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    expect(get(controller.messages).at(-1)?.content).toBe('Current answer');
  });
});

describe('native conversation persistence', () => {
  async function nativeStorage() {
    const { persistence } = await import('./persistence');
    return {
      ...persistence,
      available: () => true,
      migrate: vi.fn().mockResolvedValue(undefined),
      list: vi.fn().mockResolvedValue([]),
      preferences: vi.fn().mockResolvedValue({}),
      save: vi.fn().mockResolvedValue(undefined),
      savePreferences: vi.fn().mockResolvedValue(undefined),
      remove: vi.fn().mockResolvedValue(undefined),
      load: vi.fn().mockResolvedValue(null),
    };
  }
  it('blocks new turns when the native database cannot open', async () => {
    const storage = await nativeStorage();
    storage.list.mockRejectedValue(new Error('disk unavailable'));
    const client = mockClient();
    const controller = createChatController(client, storage);
    await controller.initialize();
    expect(await controller.send('Keep this safe', [])).toBe(false);
    expect(client.streamChat).not.toHaveBeenCalled();
    expect(get(controller.persistenceError)).toContain('unavailable');
  });
  it('flushes active work before clearing private state for the app lock', async () => {
    const storage = await nativeStorage();
    const client = mockClient();
    const controller = createChatController(client, storage);
    await controller.initialize();
    await controller.send('Remember this chat', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    await controller.suspend();
    expect(storage.save).toHaveBeenCalledWith(
      expect.objectContaining({ title: 'Remember this chat' }),
    );
    expect(get(controller.messages)).toEqual([]);
    expect(get(controller.sessions)).toEqual([]);
    await controller.initialize();
    expect(storage.list).toHaveBeenCalledTimes(2);
  });
  it('does not let a slow session load replace a newer navigation', async () => {
    const storage = await nativeStorage();
    const first = { id: 'first', title: 'First', updatedAt: 1, messages: [] };
    const second = { id: 'second', title: 'Second', updatedAt: 2, messages: [] };
    storage.list.mockResolvedValue([first, second]);
    let finish: (value: typeof first) => void = () => {};
    storage.load.mockImplementation((id: string) =>
      id === 'first'
        ? new Promise((resolve) => {
            finish = resolve;
          })
        : Promise.resolve(second),
    );
    const controller = createChatController(mockClient(), storage);
    await controller.initialize();
    const earlier = controller.openSession('first');
    await vi.waitFor(() => expect(storage.load).toHaveBeenCalledWith('first'));
    await controller.openSession('second');
    finish(first);
    await earlier;
    expect(get(controller.activeSessionId)).toBe('second');
  });
  it('retries failed writes in order so a later deletion stays deleted', async () => {
    const storage = await nativeStorage();
    const calls: string[] = [];
    storage.save.mockImplementation(async () => {
      calls.push('save');
      throw new Error('disk full');
    });
    storage.remove.mockImplementation(async () => {
      calls.push('delete');
    });
    const controller = createChatController(mockClient(), storage);
    await controller.initialize();
    await controller.send('Transient disk failure', []);
    await vi.waitFor(() =>
      expect(get(controller.persistenceError)).toContain('could not be saved'),
    );
    controller.removeSession(get(controller.activeSessionId));
    expect(storage.remove).not.toHaveBeenCalled();
    storage.save.mockImplementation(async () => {
      calls.push('save');
    });
    await controller.retryPersistence();
    expect(calls.at(-1)).toBe('delete');
    expect(get(controller.persistenceError)).toBe('');
    expect(get(controller.sessions)).toEqual([]);
  });
  it('retains unsaved content if locking would discard it', async () => {
    const storage = await nativeStorage();
    storage.save.mockRejectedValue(new Error('disk full'));
    const controller = createChatController(mockClient(), storage);
    await controller.initialize();
    await controller.send('Do not lose this', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    await expect(controller.suspend()).rejects.toThrow('before locking');
    expect(get(controller.messages)[0].content).toBe('Do not lose this');
  });
  it('keeps a failed save visible and exportable when starting a new chat', async () => {
    const storage = await nativeStorage();
    storage.save.mockRejectedValue(new Error('read-only database'));
    const client = mockClient();
    const controller = createChatController(client, storage);
    const { agentClient } = await import('./agent');
    const exported = vi.spyOn(agentClient, 'export').mockResolvedValue(true);
    await controller.initialize();
    await controller.send('Keep this unsaved conversation', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    const id = get(controller.activeSessionId);
    const contents = structuredClone(get(controller.messages));
    expect(await controller.newChat()).toBe(false);
    expect(get(controller.activeSessionId)).toBe(id);
    expect(get(controller.messages)).toEqual(contents);
    expect(get(controller.persistenceError)).toContain('Keep Blackwall open');
    await controller.exportConversation();
    expect(exported).toHaveBeenCalledWith(expect.objectContaining({ id, messages: contents }));
    storage.save.mockResolvedValue(undefined);
    await controller.retryPersistence();
    expect(get(controller.persistenceError)).toBe('');
    expect(await controller.newChat()).toBe(true);
    expect(get(controller.messages)).toEqual([]);
    expect(get(controller.activeSessionId)).not.toBe(id);
    expect(client.streamChat).toHaveBeenCalledTimes(1);
    exported.mockRestore();
    controller.destroy();
  });
  it('waits for a pending save and retains the conversation if that write fails', async () => {
    const storage = await nativeStorage();
    let rejectWrite!: (reason: Error) => void;
    storage.save.mockImplementationOnce(
      () =>
        new Promise<void>((_resolve, reject) => {
          rejectWrite = reject;
        }),
    );
    const controller = createChatController(mockClient(), storage);
    await controller.initialize();
    await controller.send('Pending disk write', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    const id = get(controller.activeSessionId);
    const navigation = controller.newChat();
    expect(get(controller.activeSessionId)).toBe(id);
    expect(get(controller.messages)[0].content).toBe('Pending disk write');
    rejectWrite(new Error('disk full'));
    expect(await navigation).toBe(false);
    expect(get(controller.activeSessionId)).toBe(id);
    expect(get(controller.messages)[0].content).toBe('Pending disk write');
    controller.destroy();
  });
  it('does not clear a newer turn when an earlier new-chat save finishes', async () => {
    const storage = await nativeStorage();
    let finishWrite!: () => void;
    storage.save.mockImplementationOnce(
      () =>
        new Promise<void>((resolve) => {
          finishWrite = resolve;
        }),
    );
    const controller = createChatController(mockClient(), storage);
    await controller.initialize();
    await controller.send('First turn', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    const navigation = controller.newChat();
    await controller.send('Newer turn while saving', []);
    finishWrite();
    expect(await navigation).toBe(false);
    expect(
      get(controller.messages).some((message) => message.content === 'Newer turn while saving'),
    ).toBe(true);
    controller.destroy();
  });
});
