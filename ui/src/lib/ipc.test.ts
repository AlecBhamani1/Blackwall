import { afterEach, describe, expect, it, vi } from 'vitest';
const native = vi.hoisted(() => ({ invoke: vi.fn(), listen: vi.fn(), unlisten: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke }));
vi.mock('@tauri-apps/api/event', () => ({ listen: native.listen }));
import { localModelClient } from './ipc';
import { ModelRequestError, nativeModelError } from './modelError';

afterEach(() => {
  delete window.__TAURI_INTERNALS__;
  vi.resetAllMocks();
});

describe('native chat failure boundary', () => {
  it('pins Agent requests to the conversation project at the native boundary', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { value: {}, configurable: true });
    native.listen.mockResolvedValue(native.unlisten);
    native.invoke.mockResolvedValue({ requestId: 'agent-test' });
    await localModelClient.streamChat(
      {
        requestId: 'agent-test',
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
        options: { enabled: true, workspace: '/projects/current', webEnabled: false },
      }),
    );
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
