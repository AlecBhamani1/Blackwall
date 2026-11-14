import { get } from 'svelte/store';
import { describe, expect, it, vi } from 'vitest';
import type { AppUpdateChannel, AppUpdateInfo, AppUpdateClient } from './updates';
import { appUpdateClient, createAppUpdateController } from './updates';

const native = vi.hoisted(() => ({
  invoke: vi.fn(),
  relaunch: vi.fn(),
  steps: [] as string[],
  resources: [] as Array<{ rid: number; close: ReturnType<typeof vi.fn> }>,
}));
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke }));
vi.mock('@tauri-apps/plugin-process', () => ({ relaunch: native.relaunch }));
vi.mock('@tauri-apps/plugin-updater', () => ({
  Update: class {
    close = vi.fn().mockResolvedValue(undefined);
    download = vi.fn(async () => void native.steps.push('download'));
    install = vi.fn(async () => void native.steps.push('install'));
    constructor(metadata: { rid: number }) {
      Object.assign(this, metadata);
      native.resources.push(this as unknown as (typeof native.resources)[number]);
    }
  },
}));

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
    expect(client.check).toHaveBeenCalledExactlyOnceWith('stable');
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

  it('does not install when the app is not ready to restart', async () => {
    const client = mockClient();
    client.check = vi.fn().mockResolvedValue({ currentVersion: '0.1.0', version: '0.1.4' });
    const controller = createAppUpdateController(client);
    await controller.initialize();

    const release = vi.fn();
    await controller.installUpdate({
      prepare: async () => {
        throw new Error('Stop the current task before updating.');
      },
      release,
    });

    expect(client.install).not.toHaveBeenCalled();
    expect(release).toHaveBeenCalledOnce();
    expect(get(controller.state)).toBe('available');
    expect(get(controller.error)).toBe('Stop the current task before updating.');
  });

  it('re-checks readiness after the download and releases the app if install fails', async () => {
    const client = mockClient();
    client.check = vi.fn().mockResolvedValue({ currentVersion: '0.1.0', version: '0.1.4' });
    client.install = vi.fn(async (_onProgress, beforeInstall) => {
      await beforeInstall?.();
    });
    const controller = createAppUpdateController(client);
    await controller.initialize();
    const prepare = vi
      .fn()
      .mockResolvedValueOnce(undefined)
      .mockRejectedValueOnce(
        new Error('Retry saving or export your conversation before updating.'),
      );
    const release = vi.fn();

    await controller.installUpdate({ prepare, release });

    expect(prepare).toHaveBeenCalledTimes(2);
    expect(release).toHaveBeenCalledOnce();
    expect(get(controller.state)).toBe('available');
    expect(get(controller.error)).toContain('Retry saving');
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

describe('update channels', () => {
  it('saves beta, checks immediately and restores it after restart', async () => {
    const client = mockClient();
    const controller = createAppUpdateController(client);
    await controller.initialize();
    await controller.setChannel('beta');
    expect(get(controller.channel)).toBe('beta');
    expect(client.check).toHaveBeenLastCalledWith('beta');
    expect(window.localStorage.getItem('blackwall.update-channel.v1')).toBe('beta');
    const restarted = createAppUpdateController(client);
    await restarted.initialize();
    expect(get(restarted.channel)).toBe('beta');
    expect(client.check).toHaveBeenLastCalledWith('beta');
  });

  it('defaults a manually installed beta to beta and honors an explicit stable preference', async () => {
    const client = mockClient();
    client.currentVersion = vi.fn().mockResolvedValue('0.1.5-beta.42');
    const controller = createAppUpdateController(client);
    await controller.initialize();
    expect(client.check).toHaveBeenLastCalledWith('beta');
    await controller.setChannel('stable');
    const restarted = createAppUpdateController(client);
    await restarted.initialize();
    expect(client.check).toHaveBeenLastCalledWith('stable');
  });

  it('discards stale checks when the user changes channel', async () => {
    const client = mockClient();
    let resolveStable!: (value: { currentVersion: string; version: string }) => void;
    client.check = vi.fn((channel: AppUpdateChannel) =>
      channel === 'stable'
        ? new Promise<AppUpdateInfo | null>((resolve) => {
            resolveStable = resolve;
          })
        : Promise.resolve({ currentVersion: '0.1.0', version: '0.1.5-beta.42' }),
    );
    const controller = createAppUpdateController(client);
    const stale = controller.checkForUpdates();
    await controller.setChannel('beta');
    resolveStable({ currentVersion: '0.1.0', version: '0.1.4' });
    await stale;
    expect(get(controller.update)?.version).toBe('0.1.5-beta.42');
    expect(get(controller.state)).toBe('available');
  });

  it('clears a previous update while checking a new channel and after failure', async () => {
    const client = mockClient();
    client.check = vi.fn().mockResolvedValue({ currentVersion: '0.1.0', version: '0.1.4' });
    const controller = createAppUpdateController(client);
    await controller.initialize();
    client.check = vi.fn().mockRejectedValue(new Error('Beta is unavailable'));
    const checking = controller.setChannel('beta');
    expect(get(controller.update)).toBeNull();
    await checking;
    expect(get(controller.state)).toBe('error');
    expect(get(controller.channel)).toBe('beta');
    await controller.installUpdate();
    expect(client.install).not.toHaveBeenCalled();
  });

  it('blocks channel changes and checks during installation', async () => {
    const client = mockClient();
    client.check = vi.fn().mockResolvedValue({ currentVersion: '0.1.0', version: '0.1.4' });
    let finish!: () => void;
    client.install = vi.fn(
      () =>
        new Promise<void>((resolve) => {
          finish = resolve;
        }),
    );
    const controller = createAppUpdateController(client);
    await controller.initialize();
    const installing = controller.installUpdate();
    await controller.setChannel('beta');
    await controller.checkForUpdates();
    expect(get(controller.channel)).toBe('stable');
    expect(client.check).toHaveBeenCalledOnce();
    finish();
    await installing;
  });

  it('keeps the existing channel if its preference cannot be saved', async () => {
    const controller = createAppUpdateController(mockClient());
    const storage = vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('Storage unavailable');
    });
    await controller.setChannel('beta');
    expect(get(controller.channel)).toBe('stable');
    expect(get(controller.error)).toContain('could not save');
    expect(get(controller.state)).toBe('error');
    storage.mockRestore();
  });

  it('re-checks readiness between the native download and install', async () => {
    vi.stubGlobal('__TAURI_INTERNALS__', {});
    native.steps.length = 0;
    native.invoke.mockResolvedValue({ rid: 7, currentVersion: '0.1.0', version: '0.1.4' });
    await appUpdateClient.check('stable');
    const blocked = vi.fn(async () => {
      native.steps.push('check');
      throw new Error('Stop the current task before updating.');
    });
    await expect(appUpdateClient.install(vi.fn(), blocked)).rejects.toThrow('Stop the current');
    expect(native.steps).toEqual(['download', 'check']);
    expect(native.relaunch).not.toHaveBeenCalled();

    native.steps.length = 0;
    await appUpdateClient.install(vi.fn(), async () => void native.steps.push('check'));
    expect(native.steps).toEqual(['download', 'check', 'install']);
    expect(native.relaunch).toHaveBeenCalledOnce();
    native.invoke.mockReset();
  });

  it('freezes settings saves from the start of an install until relaunch', async () => {
    const { persistence } = await import('./persistence');
    const { createChatController } = await import('./store');
    let finishSave = () => {};
    native.invoke.mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          finishSave = resolve;
        }),
    );
    const inFlight = persistence.savePreferences({ memoryEnabled: true });
    const chat = createChatController();
    let ready = false;
    const preparing = chat.prepareForRestart().then(() => (ready = true));
    await new Promise((resolve) => setTimeout(resolve));
    expect(ready).toBe(false);
    await expect(persistence.savePreferences({ memoryEnabled: false })).rejects.toBe(
      'Blackwall is installing an update.',
    );
    chat.savePreferences({ contextWindow: 64000 });
    expect(get(chat.preferences).contextWindow).not.toBe(64000);
    finishSave();
    await inFlight;
    await preparing;
    expect(native.invoke).toHaveBeenCalledOnce();

    chat.releaseRestart();
    native.invoke.mockResolvedValue(undefined);
    await persistence.savePreferences({ memoryEnabled: false });
    expect(native.invoke).toHaveBeenCalledTimes(2);
    chat.destroy();
    native.invoke.mockReset();
  });

  it('routes native checks by channel and closes resources from out-of-order replies', async () => {
    vi.stubGlobal('__TAURI_INTERNALS__', {});
    let reply!: (metadata: object) => void;
    native.invoke.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          reply = resolve;
        }),
    );
    const stale = appUpdateClient.check('stable');
    await vi.waitFor(() =>
      expect(native.invoke).toHaveBeenCalledWith('check_app_update', { channel: 'stable' }),
    );
    native.invoke.mockResolvedValueOnce({
      rid: 2,
      currentVersion: '0.1.0',
      version: '0.1.5-beta.42',
    });
    expect((await appUpdateClient.check('beta'))?.version).toBe('0.1.5-beta.42');
    reply({ rid: 1, currentVersion: '0.1.0', version: '0.1.4' });
    expect(await stale).toBeNull();
    expect(native.resources.find((resource) => resource.rid === 1)?.close).toHaveBeenCalledOnce();
    native.invoke.mockResolvedValueOnce(null);
    await appUpdateClient.check('beta');
    expect(native.resources.find((resource) => resource.rid === 2)?.close).toHaveBeenCalledOnce();
  });
});
