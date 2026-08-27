import { get } from 'svelte/store';
import { describe, expect, it, vi } from 'vitest';
import type { AppUpdateClient } from './updates';
import { createAppUpdateController } from './updates';

function mockClient(): AppUpdateClient {
  return {
    supported: vi.fn().mockReturnValue(true),
    currentVersion: vi.fn().mockResolvedValue('0.1.0'),
    check: vi.fn().mockResolvedValue(null),
    install: vi.fn().mockResolvedValue(undefined),
  };
}

describe('app update controller', () => {
  it('checks automatically and reports that the installed version is current', async () => {
    const client = mockClient();
    const controller = createAppUpdateController(client);

    await controller.initialize();

    expect(get(controller.currentVersion)).toBe('0.1.0');
    expect(get(controller.state)).toBe('current');
    expect(client.check).toHaveBeenCalledOnce();
  });

  it('downloads an available signed update and forwards progress', async () => {
    const client = mockClient();
    client.check = vi.fn().mockResolvedValue({ currentVersion: '0.1.0', version: '0.1.4' });
    client.install = vi.fn(async (onProgress) => {
      onProgress({ downloadedBytes: 25, totalBytes: 100 });
      onProgress({ downloadedBytes: 100, totalBytes: 100 });
    });
    const controller = createAppUpdateController(client);

    await controller.initialize();
    expect(get(controller.state)).toBe('available');
    expect(get(controller.update)?.version).toBe('0.1.4');

    await controller.installUpdate();
    expect(client.install).toHaveBeenCalledOnce();
    expect(get(controller.progress)).toEqual({ downloadedBytes: 100, totalBytes: 100 });
  });

  it('keeps update failures inside the settings status', async () => {
    const client = mockClient();
    client.check = vi.fn().mockRejectedValue(new Error('release endpoint returned 404'));
    const controller = createAppUpdateController(client);

    await controller.initialize();

    expect(get(controller.state)).toBe('error');
    expect(get(controller.error)).toBe('release endpoint returned 404');
  });
});
