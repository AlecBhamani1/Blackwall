import { get } from 'svelte/store';
import { agentClient } from './agent';
import { describe, expect, it, vi } from 'vitest';
import type { LocalModelClient } from './ipc';
import { createChatController } from './store';
import { nativeModelError } from './modelError';
import { prepareAttachments, MAX_ATTACHMENT_TOTAL_BYTES } from './attachments';
import type { Attachment, SessionSummary } from './types';

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
  it.each(['chat', 'agent'] as const)(
    'locks %s mode and its project after sending',
    async (mode) => {
      const pick = vi.spyOn(agentClient, 'chooseWorkspace').mockResolvedValue('/projects/first');
      const client = mockClient();
      const controller = createChatController(client);
      await controller.initialize();
      controller.chooseMode(mode);
      if (mode === 'agent') await controller.chooseWorkspace();
      await controller.send('Keep this mode', []);
      await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
      controller.chooseMode(mode === 'agent' ? 'chat' : 'agent');
      await controller.chooseWorkspace();
      expect(get(controller.sessionLocked)).toBe(true);
      expect(get(controller.agentMode)).toBe(mode === 'agent');
      expect(get(controller.workspace)).toBe(mode === 'agent' ? '/projects/first' : '');
      expect(pick).toHaveBeenCalledTimes(mode === 'agent' ? 1 : 0);
      expect(get(controller.sessions)[0]).toMatchObject({ mode });
      expect(vi.mocked(client.streamChat).mock.calls[0][0]).toMatchObject({
        agentMode: mode === 'agent',
        workspace: mode === 'agent' ? '/projects/first' : undefined,
      });
      controller.destroy();
      pick.mockRestore();
    },
  );

  it('clears projects on new chats and restores each saved Agent conversation after reload', async () => {
    const pick = vi
      .spyOn(agentClient, 'chooseWorkspace')
      .mockResolvedValueOnce('/projects/first')
      .mockResolvedValueOnce('/projects/second');
    const controller = createChatController(mockClient());
    await controller.initialize();
    controller.chooseMode('agent');
    expect(await controller.send('Needs a project', [])).toBe(false);
    expect(get(controller.sessionLocked)).toBe(false);
    await controller.chooseWorkspace();
    await controller.send('First agent task', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    const first = get(controller.activeSessionId);
    controller.webEnabled.set(true);
    await controller.newChat();
    expect(get(controller.agentMode)).toBe(true);
    expect(get(controller.workspace)).toBe('');
    expect(get(controller.webEnabled)).toBe(false);
    expect(get(controller.sessionLocked)).toBe(false);
    await controller.chooseWorkspace();
    await controller.send('Second agent task', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    const second = get(controller.activeSessionId);
    await controller.newChat('chat');
    await controller.send('Plain chat', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    const chat = get(controller.activeSessionId);
    expect(get(controller.visibleSessions).map((session) => session.id)).toEqual([chat]);
    await controller.openSession(first);
    expect(get(controller.activeSessionId)).toBe(chat);
    await controller.newChat('agent');
    expect(get(controller.visibleSessions).map((session) => session.id)).toEqual([second, first]);
    await controller.openSession(first);
    expect(get(controller.workspace)).toBe('/projects/first');
    await controller.openSession(second);
    expect(get(controller.workspace)).toBe('/projects/second');
    controller.destroy();
    const reloaded = createChatController(mockClient());
    reloaded.chooseMode('agent');
    await reloaded.openSession(first);
    expect(get(reloaded.workspace)).toBe('/projects/first');
    expect(get(reloaded.sessionLocked)).toBe(true);
    reloaded.removeSession(first);
    expect(get(reloaded.workspace)).toBe('');
    expect(get(reloaded.sessionLocked)).toBe(false);
    reloaded.destroy();
    pick.mockRestore();
  });

  it('treats legacy conversations as Chat with no project', async () => {
    window.localStorage.setItem(
      'blackwall.sessions.v1',
      JSON.stringify([{ id: 'legacy', title: 'Old chat', updatedAt: 1, messages: [] }]),
    );
    const controller = createChatController(mockClient());
    expect(get(controller.visibleSessions)[0].id).toBe('legacy');
    controller.chooseMode('agent');
    expect(get(controller.visibleSessions)).toEqual([]);
    await controller.newChat('chat');
    await controller.openSession('legacy');
    expect(get(controller.agentMode)).toBe(false);
    expect(get(controller.workspace)).toBe('');
    controller.destroy();
  });

  it('ignores a project picker that finishes after starting another conversation', async () => {
    let finish!: (path: string) => void;
    const pick = vi.spyOn(agentClient, 'chooseWorkspace').mockImplementation(
      () =>
        new Promise((resolve) => {
          finish = resolve;
        }),
    );
    const controller = createChatController(mockClient());
    controller.chooseMode('agent');
    const selection = controller.chooseWorkspace();
    await controller.newChat('chat');
    finish('/projects/stale');
    await selection;
    expect(get(controller.agentMode)).toBe(false);
    expect(get(controller.workspace)).toBe('');
    controller.destroy();
    pick.mockRestore();
  });

  it('resends the contents of a referenced screenshot without reuploading it', async () => {
    const client = mockClient();
    const controller = createChatController(client);
    await controller.initialize();
    const pending = prepareAttachments([
      new File(['pixels'], 'Screenshot 1.png', { type: 'image/png' }),
    ]).accepted;
    await controller.send('Describe this', pending);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    const original = get(controller.messages)[0].attachments[0];
    expect(original.dataUrl).toMatch(/^data:image\/png;base64,/);
    await controller.send('Look again at @"Screenshot 1.png"', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    const request = vi.mocked(client.streamChat).mock.calls[1][0];
    expect(request.messages.at(-1)?.attachments).toEqual([
      expect.objectContaining({ id: original.id, dataUrl: original.dataUrl }),
    ]);
    expect(get(controller.messages)[2].attachments[0].id).toBe(original.id);
    controller.destroy();
  });

  it('counts both new attachments and references against the message limits', async () => {
    const client = mockClient();
    const controller = createChatController(client);
    await controller.initialize();
    const attachments: Attachment[] = Array.from({ length: 8 }, (_, i) => ({
      id: `file-${i}`,
      name: `file-${i}.txt`,
      kind: 'text',
      mimeType: 'text/plain',
      sizeBytes: 1,
      textContent: 'x',
    }));
    controller.messages.set([
      {
        id: 'previous',
        role: 'user',
        content: '',
        attachments,
        status: 'complete',
        createdAt: 1,
      },
    ]);
    const pending = prepareAttachments([new File(['extra'], 'extra.txt')]).accepted;
    expect(
      await controller.send(attachments.map((file) => `@${file.name}`).join(' '), pending),
    ).toBe(false);
    expect(get(controller.notice)).toContain('8 files');
    controller.messages.update((messages) => [
      {
        ...messages[0],
        attachments: attachments.slice(0, 3).map((attachment) => ({
          ...attachment,
          sizeBytes: MAX_ATTACHMENT_TOTAL_BYTES / 3,
        })),
      },
    ]);
    expect(await controller.send('@file-0.txt @file-1.txt @file-2.txt', pending)).toBe(false);
    expect(get(controller.notice)).toContain('30 MB');
    expect(client.streamChat).not.toHaveBeenCalled();
    controller.destroy();
  });

  it('keeps references out of another conversation and explains missing retained contents', async () => {
    const client = mockClient();
    const controller = createChatController(client);
    await controller.initialize();
    controller.messages.set([
      {
        id: 'previous',
        role: 'user',
        content: '',
        status: 'complete',
        createdAt: 1,
        attachments: [
          { id: 'file', name: 'legacy.txt', kind: 'text', mimeType: 'text/plain', sizeBytes: 3 },
        ],
      },
    ]);
    expect(await controller.send('Read @legacy.txt', [])).toBe(false);
    expect(get(controller.notice)).toContain('Reattach legacy.txt');
    expect(client.streamChat).not.toHaveBeenCalled();
    await controller.newChat();
    expect(await controller.send('Read @legacy.txt', [])).toBe(true);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    expect(vi.mocked(client.streamChat).mock.calls[0][0].messages[0].attachments).toEqual([]);
    controller.destroy();
  });

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
    const endpoint = 'http://192.0.2.10:11434';

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

  it('shows instruction sources and warnings and replaces them when nested guidance loads', async () => {
    const client = mockClient();
    client.streamChat = vi.fn(async (request, callbacks) => {
      callbacks.onEvent?.({
        type: 'instructions_loaded',
        requestId: request.requestId,
        sources: ['AGENTS.md'],
        warnings: [],
      });
      callbacks.onEvent?.({
        type: 'instructions_loaded',
        requestId: request.requestId,
        sources: ['AGENTS.md', 'src/AGENTS.override.md'],
        warnings: ['Skipped oversized guidance.'],
      });
      callbacks.onEvent?.({
        type: 'instructions_loaded',
        requestId: request.requestId,
        agentId: 'child',
        sources: ['AGENTS.md', 'nested/AGENTS.md'],
        warnings: [],
      });
      callbacks.onDelta('Applied project conventions.');
    });
    const controller = createChatController(client);
    await controller.initialize();
    await controller.send('Inspect this project', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    const tools = get(controller.messages).at(-1)?.tools;
    expect(tools).toHaveLength(2);
    expect(tools?.[1]).toMatchObject({ id: 'instructions_child', status: 'complete' });
    expect(tools?.[1].output).toContain('nested/AGENTS.md');
    expect(tools?.[0]).toMatchObject({ name: 'project_instructions', status: 'complete' });
    expect(tools?.[0].output).toContain('AGENTS.md, src/AGENTS.override.md');
    expect(tools?.[0].output).toContain('Warning: Skipped oversized guidance.');
    controller.destroy();
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
  it('restores native Agent projects and retains the current chat if a project is unavailable', async () => {
    const storage = await nativeStorage();
    const first: SessionSummary = {
      id: 'agent-first',
      title: 'First',
      mode: 'agent',
      workspace: '/projects/first',
      updatedAt: 1,
      messages: [
        {
          id: 'user',
          role: 'user',
          content: 'Task',
          status: 'complete',
          createdAt: 1,
          attachments: [],
        },
      ],
    };
    const second: SessionSummary = { ...first, id: 'agent-second', workspace: '/projects/second' };
    storage.list.mockResolvedValue([first, second]);
    storage.load.mockImplementation(async (id: string) => (id === first.id ? first : second));
    const restore = vi
      .spyOn(agentClient, 'restoreWorkspace')
      .mockResolvedValueOnce('/projects/first')
      .mockRejectedValueOnce(new Error('Folder missing'));
    const controller = createChatController(mockClient(), storage);
    await controller.initialize();
    controller.chooseMode('agent');
    await controller.openSession(first.id);
    expect(restore).toHaveBeenCalledWith(first.id);
    expect(get(controller.workspace)).toBe('/projects/first');
    expect(get(controller.sessionLocked)).toBe(true);
    await controller.openSession(second.id);
    expect(get(controller.activeSessionId)).toBe(first.id);
    expect(get(controller.workspace)).toBe('/projects/first');
    expect(get(controller.persistenceError)).toContain('could not be opened');
    controller.destroy();
    restore.mockRestore();
  });

  it('references retained files after reopening a saved native conversation', async () => {
    const storage = await nativeStorage();
    const saved: SessionSummary = {
      id: 'saved',
      title: 'Files',
      updatedAt: 1,
      messages: [
        {
          id: 'previous',
          role: 'user',
          content: 'Two files',
          status: 'complete',
          createdAt: 1,
          attachments: [
            {
              id: 'first',
              name: 'notes.md',
              kind: 'text',
              mimeType: 'text/markdown',
              sizeBytes: 3,
              textContent: 'one',
            },
            {
              id: 'second',
              name: 'notes.md',
              kind: 'text',
              mimeType: 'text/markdown',
              sizeBytes: 3,
              textContent: 'two',
            },
          ],
        },
      ],
    };
    storage.list.mockResolvedValue([saved]);
    storage.load.mockResolvedValue(saved);
    const client = mockClient();
    const controller = createChatController(client, storage);
    await controller.initialize();
    await controller.openSession(saved.id);
    await controller.send('Read @"notes.md (2)"', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    expect(vi.mocked(client.streamChat).mock.calls[0][0].messages.at(-1)?.attachments).toEqual([
      expect.objectContaining({ id: 'second', textContent: 'two' }),
    ]);
    controller.destroy();
  });

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

describe('context commands', () => {
  it('sends the configured budget and keeps compaction checkpoints separate from the transcript', async () => {
    const client = mockClient();
    const controller = createChatController(client);
    await controller.initialize();
    controller.savePreferences({ contextWindow: 8192, outputTokens: 1024, autoCompact: true });
    await controller.send('Initial task', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    expect(vi.mocked(client.streamChat).mock.calls[0][0].contextBudget).toEqual({
      contextWindow: 8192,
      outputTokens: 1024,
      autoCompact: true,
    });
    const original = structuredClone(get(controller.messages));
    const snapshot = {
      transcript: [{ role: 'user', content: 'Initial task' }],
      checkpoints: [],
      coveredMessages: 1,
      sourceHash: 'fixture',
      serverUsage: { promptTokens: 77, completionTokens: 12, totalTokens: 89 },
    };
    vi.mocked(client.streamChat).mockImplementationOnce(async (request, callbacks) => {
      expect(request.compact).toBe(true);
      callbacks.onEvent?.({
        type: 'context_updated',
        requestId: request.requestId,
        state: snapshot,
      });
      callbacks.onEvent?.({
        type: 'context_report',
        requestId: request.requestId,
        report: {
          estimatedPromptTokens: 101,
          toolTokens: 20,
          outputTokens: 1024,
          safetyTokens: 409,
          contextWindow: 8192,
          serverUsage: snapshot.serverUsage,
        },
      });
    });
    expect(await controller.send('/compact', [])).toBe(true);
    expect(get(controller.messages)).toEqual(original);
    expect(get(controller.sessions)[0].contextState).toEqual(snapshot);
    await controller.send('/context', []);
    expect(get(controller.notice)).toContain('101 tokens');
    expect(get(controller.notice)).toContain('77 prompt, 12 generated');
    expect(client.streamChat).toHaveBeenCalledTimes(2);
    await controller.newChat();
    await controller.send('New task', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    expect(vi.mocked(client.streamChat).mock.calls[2][0].contextState).toBeUndefined();
    controller.destroy();
  });
  it('does not commit a failed or cancelled compaction candidate', async () => {
    const client = mockClient();
    const controller = createChatController(client);
    await controller.initialize();
    await controller.send('Initial task', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    const original = structuredClone(get(controller.sessions)[0]);
    const candidate = {
      transcript: [],
      checkpoints: [],
      coveredMessages: 0,
      sourceHash: 'candidate',
    };
    vi.mocked(client.streamChat).mockImplementationOnce(async (request, callbacks) => {
      callbacks.onEvent?.({
        type: 'context_updated',
        requestId: request.requestId,
        state: candidate,
      });
      throw new Error('Invalid summary');
    });
    expect(await controller.send('/compact', [])).toBe(false);
    expect(get(controller.sessions)[0]).toEqual(original);
    let release!: () => void;
    vi.mocked(client.streamChat).mockImplementationOnce(async (request, callbacks) => {
      callbacks.onEvent?.({
        type: 'context_updated',
        requestId: request.requestId,
        state: candidate,
      });
      await new Promise<void>((resolve) => {
        release = resolve;
      });
    });
    const work = controller.send('/compact', []);
    await vi.waitFor(() => expect(release).toBeTypeOf('function'));
    controller.stop();
    release();
    expect(await work).toBe(false);
    expect(get(controller.sessions)[0].contextState).toBeUndefined();
    expect(get(controller.messages)).toEqual(original.messages);
    controller.destroy();
  });
});
