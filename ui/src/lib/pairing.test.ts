import { afterEach, describe, expect, it, vi } from 'vitest';
import { get } from 'svelte/store';
const native = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke }));
import { pairingClient, pairingSnapshot, stopPairing } from './pairing';

afterEach(() => {
  stopPairing();
  delete window.__TAURI_INTERNALS__;
  vi.resetAllMocks();
});

describe('pairing status recovery', () => {
  it('surfaces polling failures while retaining the pending identity, then clears them on retry', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { value: {}, configurable: true });
    const progress = {
      state: 'review',
      hostName: 'Home Mac',
      model: 'model',
      expiresAt: 1,
      candidate: null,
      confirmation: 'code',
      sessionId: 'same-device',
    };
    pairingSnapshot.set({ devices: [], host: null, client: progress, warnings: [] });
    native.invoke.mockRejectedValueOnce('Local database could not be saved.');
    await expect(pairingClient.refresh()).rejects.toBe('Local database could not be saved.');
    expect(get(pairingSnapshot).client).toEqual(progress);
    expect(get(pairingSnapshot).warnings).toEqual(['Local database could not be saved.']);
    native.invoke.mockResolvedValue({
      devices: [],
      host: null,
      client: { ...progress, state: 'saved' },
      warnings: [],
    });
    await pairingClient.refresh();
    expect(get(pairingSnapshot).client?.state).toBe('saved');
    expect(get(pairingSnapshot).warnings).toEqual([]);
  });
  it('does not restore private state when a stale request fails after locking', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { value: {}, configurable: true });
    let reject!: (reason: string) => void;
    native.invoke.mockImplementation(
      () =>
        new Promise((_resolve, fail) => {
          reject = fail;
        }),
    );
    const request = pairingClient.refresh();
    stopPairing();
    reject('Previous device failed.');
    await expect(request).rejects.toBe('Previous device failed.');
    expect(get(pairingSnapshot)).toEqual({ devices: [], host: null, client: null, warnings: [] });
  });
});
