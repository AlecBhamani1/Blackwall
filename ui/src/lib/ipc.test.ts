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
