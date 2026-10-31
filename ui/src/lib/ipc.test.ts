import { afterEach, describe, expect, it, vi } from 'vitest';
const native = vi.hoisted(() => ({ invoke: vi.fn(), listen: vi.fn(), unlisten: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke }));
vi.mock('@tauri-apps/api/event', () => ({ listen: native.listen }));
import { localModelClient } from './ipc';
import { ModelRequestError, nativeModelError } from './modelError';

afterEach(() => {
  delete window.__TAURI_INTERNALS__;
  vi.resetAllMocks();
  vi.unstubAllGlobals();
});

describe('native chat failure boundary', () => {
  it('pins Agent requests to the conversation project at the native boundary', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { value: {}, configurable: true });
    native.listen.mockResolvedValue(native.unlisten);
    native.invoke.mockResolvedValue({ requestId: 'agent-test' });
    await localModelClient.streamChat(
      {
        requestId: 'agent-test',
        sessionId: 'session-test',
        sourceMessageId: 'message-test',
        model: 'model',
        messages: [],
        agentMode: true,
        workspace: '/projects/current',
        webEnabled: false,
      },
      { onDelta: vi.fn() },
      new AbortController().signal,
    );
    expect(native.invoke).toHaveBeenCalledWith(
      'stream_chat',
      expect.objectContaining({
        options: {
          enabled: true,
          workspace: '/projects/current',
          webEnabled: false,
          executionProfile: 'read-only',
          commandNetwork: false,
          sessionId: 'session-test',
          sourceMessageId: 'message-test',
        },
      }),
    );
  });

  it('delivers validated instruction provenance through native streaming events', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { value: {}, configurable: true });
    native.listen.mockResolvedValue(native.unlisten);
    const onEvent = vi.fn();
    native.invoke.mockImplementation(async () => {
      const listener = native.listen.mock.calls[0][1];
      listener({
        payload: {
          type: 'instructions_loaded',
          requestId: 'guidance',
          sources: ['AGENTS.md'],
          warnings: ['Skipped malformed guidance.'],
        },
      });
      listener({
        payload: {
          type: 'instructions_loaded',
          requestId: 'guidance',
          sources: [123],
          warnings: [],
        },
      });
      listener({ payload: { type: 'turn_complete', requestId: 'guidance' } });
      return { requestId: 'guidance' };
    });
    await localModelClient.streamChat(
      { requestId: 'guidance', model: 'model', messages: [] },
      { onDelta: vi.fn(), onEvent },
      new AbortController().signal,
    );
    expect(onEvent).toHaveBeenCalledTimes(2);
    expect(onEvent).toHaveBeenCalledWith({
      type: 'instructions_loaded',
      requestId: 'guidance',
      sources: ['AGENTS.md'],
      warnings: ['Skipped malformed guidance.'],
    });
  });

  it('keeps well-formed server diagnostics on instruction events and drops malformed ones', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { value: {}, configurable: true });
    native.listen.mockResolvedValue(native.unlisten);
    const onEvent = vi.fn();
    native.invoke.mockImplementation(async () => {
      const listener = native.listen.mock.calls[0][1];
      listener({
        payload: {
          type: 'instructions_loaded',
          requestId: 'diag',
          sources: [],
          warnings: [],
          diagnostics: [
            { server: 'docs', stderr: 'boom', notPassed: ['TOKEN', 7] },
            { server: 'bad' },
            'nope',
          ],
        },
      });
      listener({ payload: { type: 'turn_complete', requestId: 'diag' } });
      return { requestId: 'diag' };
    });
    await localModelClient.streamChat(
      { requestId: 'diag', model: 'model', messages: [] },
      { onDelta: vi.fn(), onEvent },
      new AbortController().signal,
    );
    expect(onEvent).toHaveBeenCalledWith({
      type: 'instructions_loaded',
      requestId: 'diag',
      sources: [],
      warnings: [],
      diagnostics: [{ server: 'docs', stderr: 'boom', notPassed: ['TOKEN'] }],
    });
  });

  it('marks a prompt for a call written as text as one that cannot be remembered', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { value: {}, configurable: true });
    native.listen.mockResolvedValue(native.unlisten);
    const onEvent = vi.fn();
    const prompt = { type: 'approval_request', requestId: 'text', kind: 'file', detail: 'd' };
    native.invoke.mockImplementation(async () => {
      const listener = native.listen.mock.calls[0][1];
      listener({ payload: { ...prompt, approvalId: 'a1', remember: false } });
      listener({ payload: { ...prompt, approvalId: 'a2' } });
      listener({ payload: { type: 'turn_complete', requestId: 'text' } });
      return { requestId: 'text' };
    });
    await localModelClient.streamChat(
      { requestId: 'text', model: 'model', messages: [] },
      { onDelta: vi.fn(), onEvent },
      new AbortController().signal,
    );
    expect(onEvent).toHaveBeenCalledWith({ ...prompt, approvalId: 'a1', remember: false });
    expect(onEvent).toHaveBeenCalledWith({ ...prompt, approvalId: 'a2' });
  });

  it('keeps the off-device and outside-sandbox marks on prompts and the trimmed mark on tool results', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { value: {}, configurable: true });
    native.listen.mockResolvedValue(native.unlisten);
    const onEvent = vi.fn();
    const prompt = { type: 'approval_request', requestId: 'sub', kind: 'exec', detail: 'd' };
    const result = { type: 'tool_result', requestId: 'sub', toolCallId: 't', output: 'o' };
    native.invoke.mockImplementation(async () => {
      const listener = native.listen.mock.calls[0][1];
      listener({
        payload: {
          ...prompt,
          approvalId: 'a1',
          remember: false,
          offDevice: true,
          outsideSandbox: true,
        },
      });
      listener({ payload: { ...prompt, approvalId: 'a2', offDevice: 'yes', outsideSandbox: 1 } });
      listener({ payload: { ...result, success: true, truncated: true } });
      listener({ payload: { ...result, success: true } });
      listener({ payload: { type: 'turn_complete', requestId: 'sub' } });
      return { requestId: 'sub' };
    });
    await localModelClient.streamChat(
      { requestId: 'sub', model: 'model', messages: [] },
      { onDelta: vi.fn(), onEvent },
      new AbortController().signal,
    );
    expect(onEvent).toHaveBeenCalledWith({
      ...prompt,
      approvalId: 'a1',
      remember: false,
      offDevice: true,
      outsideSandbox: true,
    });
    expect(onEvent).toHaveBeenCalledWith({ ...prompt, approvalId: 'a2' });
    expect(onEvent).toHaveBeenCalledWith({ ...result, success: true, truncated: true });
    expect(onEvent).toHaveBeenCalledWith({ ...result, success: true });
  });

  it('ignores per-call model usage events without breaking the stream', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { value: {}, configurable: true });
    native.listen.mockResolvedValue(native.unlisten);
    const onEvent = vi.fn();
    const onDelta = vi.fn();
    const onComplete = vi.fn();
    const usage = { promptTokens: 120, completionTokens: 30, totalTokens: 150 };
    native.invoke.mockImplementation(async () => {
      const listener = native.listen.mock.calls[0][1];
      listener({ payload: { type: 'assistant_delta', requestId: 'usage', delta: 'Hello ' } });
      listener({ payload: { type: 'model_usage', requestId: 'usage', usage } });
      listener({ payload: { type: 'assistant_delta', requestId: 'usage', delta: 'world' } });
      listener({ payload: { type: 'turn_complete', requestId: 'usage', usage } });
      return { requestId: 'usage' };
    });
    await localModelClient.streamChat(
      { requestId: 'usage', model: 'model', messages: [] },
      { onDelta, onEvent, onComplete },
      new AbortController().signal,
    );
    expect(onDelta.mock.calls.map(([delta]) => delta).join('')).toBe('Hello world');
    expect(onComplete).toHaveBeenCalledTimes(1);
    expect(onEvent).not.toHaveBeenCalledWith(expect.objectContaining({ type: 'model_usage' }));
    expect(onEvent).toHaveBeenCalledWith(expect.objectContaining({ type: 'turn_complete', usage }));
  });

  it('passes reasoning and its duration through with the completed turn', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { value: {}, configurable: true });
    native.listen.mockResolvedValue(native.unlisten);
    const onEvent = vi.fn();
    native.invoke.mockImplementation(async () => {
      const listener = native.listen.mock.calls[0][1];
      listener({
        payload: {
          type: 'turn_complete',
          requestId: 'thinking',
          reasoning: 'Checked the plan.',
          reasoningMs: 2600,
        },
      });
      return { requestId: 'thinking' };
    });
    await localModelClient.streamChat(
      { requestId: 'thinking', model: 'model', messages: [] },
      { onDelta: vi.fn(), onEvent },
      new AbortController().signal,
    );
    expect(onEvent).toHaveBeenCalledWith(
      expect.objectContaining({ reasoning: 'Checked the plan.', reasoningMs: 2600 }),
    );
  });

  it('keeps the MCP tool an approval can be saved for and drops malformed ones', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { value: {}, configurable: true });
    native.listen.mockResolvedValue(native.unlisten);
    const onEvent = vi.fn();
    const approval = { type: 'approval_request', requestId: 'tools', kind: 'exec', detail: 'd' };
    native.invoke.mockImplementation(async () => {
      const listener = native.listen.mock.calls[0][1];
      listener({
        payload: {
          ...approval,
          approvalId: 'a1',
          tool: { server: 'context7', tool: 'context7__query-docs' },
        },
      });
      listener({
        payload: { ...approval, approvalId: 'a2', tool: { server: 'docs', tool: 'other__x' } },
      });
      listener({ payload: { type: 'turn_complete', requestId: 'tools' } });
      return { requestId: 'tools' };
    });
    await localModelClient.streamChat(
      { requestId: 'tools', model: 'model', messages: [] },
      { onDelta: vi.fn(), onEvent },
      new AbortController().signal,
    );
    expect(onEvent).toHaveBeenCalledWith({
      ...approval,
      approvalId: 'a1',
      tool: { server: 'context7', tool: 'context7__query-docs' },
    });
    expect(onEvent).toHaveBeenCalledWith({ ...approval, approvalId: 'a2' });
  });

  it('reports local storage failure without blaming the model or exposing native details', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { value: {}, configurable: true });
    native.listen.mockResolvedValue(native.unlisten);
    native.invoke.mockRejectedValue({ code: 'storage_error', message: '/private/user/data' });
    const result = localModelClient.streamChat(
      { requestId: 'storage-test', model: 'model', messages: [] },
      { onDelta: vi.fn() },
      new AbortController().signal,
    );
    await expect(result).rejects.toMatchObject({ code: 'storage_error', connectionLost: false });
    await expect(result).rejects.toThrow('disk space and folder permissions');
    await expect(result).rejects.not.toThrow('/private');
    expect(native.unlisten).toHaveBeenCalledOnce();
  });
  it('normalizes an IPC rejection even when the error event has not arrived', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { value: {}, configurable: true });
    native.listen.mockResolvedValue(native.unlisten);
    native.invoke.mockRejectedValue({
      code: 'model_unavailable',
      message: 'https://private/?key=secret',
    });
    const result = localModelClient.streamChat(
      { requestId: 'offline-test', model: 'model', messages: [] },
      { onDelta: vi.fn() },
      new AbortController().signal,
    );
    await expect(result).rejects.toBeInstanceOf(ModelRequestError);
    await expect(result).rejects.toMatchObject({ code: 'model_unavailable', connectionLost: true });
    await expect(result).rejects.toThrow('Open and unlock');
    expect(native.unlisten).toHaveBeenCalledOnce();
  });

  it('keeps busy models retryable without claiming connection loss and hides unknown native details', () => {
    expect(nativeModelError({ code: 'model_busy' }).connectionLost).toBe(false);
    expect(nativeModelError({ code: 'model_access_denied' }).connectionLost).toBe(true);
    expect(nativeModelError({ code: 'model_not_found' }).connectionLost).toBe(true);
    expect(nativeModelError({ code: 'credential_error' }).connectionLost).toBe(true);
    expect(nativeModelError({ code: 'toString', message: 'private key' }).message).not.toContain(
      'private',
    );
    expect(nativeModelError('private key').message).not.toContain('private');
  });

  it('shows what the model server said about a failure', () => {
    const memory =
      'Ollama: model requires more system memory (24.1 GiB) than is available (12.0 GiB)';
    expect(
      nativeModelError({ code: 'model_http_error', message: 'generic', detail: memory }).message,
    ).toBe(memory);
    expect(
      nativeModelError({ code: 'model_unavailable', detail: 'llama.cpp: Loading model' }).message,
    ).toMatch(/^The model computer is unavailable\..*\(llama\.cpp: Loading model\)$/);
    const noisy = nativeModelError({
      code: 'model_http_error',
      detail: `bad\u001b[31m\nline ${'x'.repeat(500)}`,
    }).message;
    expect(noisy).not.toMatch(/[\u0000-\u001f]/);
    expect(noisy.length).toBe(300);
    expect(nativeModelError({ code: 'model_http_error', detail: 42 }).message).toBe(
      'The model could not complete the request. Try again or check the model service.',
    );
  });
});

describe('browser model discovery', () => {
  it('lists each model id once', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) =>
        String(input).endsWith('/api/tags')
          ? Response.json({
              models: [
                { name: 'qwen3.6:latest', size: 2 },
                { name: 'qwen3.6:latest', size: 2 },
                { name: 'nomic-embed-text', size: 1 },
                { name: 'llama3.2:1b', size: 1 },
              ],
            })
          : Response.json({ data: [{ id: 'a' }, { id: 'a' }, { id: 'text-embedding-3' }, { id: 'b' }] }),
      ),
    );
    expect((await localModelClient.discoverModels()).map(({ id }) => id)).toEqual([
      'llama3.2:1b',
      'qwen3.6:latest',
    ]);
    expect(
      (await localModelClient.discoverModels('https://models.example/api')).map(({ id }) => id),
    ).toEqual(['a', 'b']);
  });
});

describe('browser model context transport', () => {
  it('caps generated output and retains separately reported server usage', async () => {
    const { webcrypto } = await import('node:crypto');
    vi.stubGlobal('crypto', webcrypto);
    const response =
      'data: {"choices":[{"delta":{"content":"Answer"},"finish_reason":"stop"}]}\n\ndata: {"choices":[],"usage":{"prompt_tokens":77,"completion_tokens":12,"total_tokens":89}}\n\ndata: [DONE]\n\n';
    const fetchMock = vi.fn(
      async (_input: RequestInfo | URL, _init?: RequestInit) =>
        new Response(response, { status: 200 }),
    );
    vi.stubGlobal('fetch', fetchMock);
    const event = vi.fn();
    const delta = vi.fn();
    await localModelClient.streamChat(
      {
        requestId: 'browser-budget',
        model: 'fixture',
        messages: [{ role: 'user', content: 'Task' }],
        contextBudget: { contextWindow: 8192, outputTokens: 1024, autoCompact: false },
      },
      { onDelta: delta, onEvent: event },
      new AbortController().signal,
    );
    const payload = JSON.parse(String(fetchMock.mock.calls[0][1]?.body));
    expect(payload.max_tokens).toBe(1024);
    expect(payload.stream_options).toEqual({ include_usage: true });
    expect(delta).toHaveBeenCalledWith('Answer');
    expect(event).toHaveBeenCalledWith(
      expect.objectContaining({
        type: 'context_report',
        report: expect.objectContaining({
          serverUsage: { promptTokens: 77, completionTokens: 12, totalTokens: 89 },
        }),
      }),
    );
    expect(event).toHaveBeenCalledWith(
      expect.objectContaining({
        type: 'context_updated',
        state: expect.objectContaining({ coveredMessages: 2, checkpoints: [] }),
      }),
    );
    vi.unstubAllGlobals();
  });
  it('rejects an unfinished stream without committing a history snapshot', async () => {
    const { webcrypto } = await import('node:crypto');
    vi.stubGlobal('crypto', webcrypto);
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response('data: {"choices":[{"delta":{"content":"Partial"}}]}\n\n', { status: 200 }),
      ),
    );
    const event = vi.fn();
    await expect(
      localModelClient.streamChat(
        {
          requestId: 'incomplete',
          model: 'fixture',
          messages: [{ role: 'user', content: 'Task' }],
        },
        { onDelta: vi.fn(), onEvent: event },
        new AbortController().signal,
      ),
    ).rejects.toThrow('incomplete');
    expect(event.mock.calls.some(([entry]) => entry.type === 'context_updated')).toBe(false);
    vi.unstubAllGlobals();
  });
});
