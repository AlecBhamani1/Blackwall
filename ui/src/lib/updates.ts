import { get, writable } from 'svelte/store';
import type { DownloadEvent, Update } from '@tauri-apps/plugin-updater';

export interface AppUpdateInfo {
  currentVersion: string;
  version: string;
  date?: string;
  body?: string;
}

export interface AppUpdateProgress {
  downloadedBytes: number;
  totalBytes?: number;
}

export type AppUpdateState =
  | 'idle'
  | 'checking'
  | 'current'
  | 'available'
  | 'downloading'
  | 'error'
  | 'unsupported';

function isTauriRuntime(): boolean {
  return typeof window !== 'undefined' && Boolean(window.__TAURI_INTERNALS__);
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === 'string') return error;
  if (error && typeof error === 'object' && 'message' in error) {
    const message = (error as { message?: unknown }).message;
    if (typeof message === 'string') return message;
  }
  return 'Blackwall could not check for updates.';
}

let pendingUpdate: Update | null = null;

export const appUpdateClient = {
  supported(): boolean {
    return isTauriRuntime();
  },

  async currentVersion(): Promise<string> {
    if (!isTauriRuntime()) return 'development';
    const { getVersion } = await import('@tauri-apps/api/app');
    return getVersion();
  },

  async check(): Promise<AppUpdateInfo | null> {
    if (!isTauriRuntime()) return null;
    await pendingUpdate?.close();
    const { check } = await import('@tauri-apps/plugin-updater');
    pendingUpdate = await check({ timeout: 15_000 });
    if (!pendingUpdate) return null;
    return {
      currentVersion: pendingUpdate.currentVersion,
      version: pendingUpdate.version,
      date: pendingUpdate.date,
      body: pendingUpdate.body,
    };
  },

  async install(onProgress: (progress: AppUpdateProgress) => void): Promise<void> {
    if (!pendingUpdate) throw new Error('Check for updates before installing.');
    let downloadedBytes = 0;
    let totalBytes: number | undefined;
    const handleEvent = (event: DownloadEvent) => {
      if (event.event === 'Started') {
        totalBytes = event.data.contentLength;
        onProgress({ downloadedBytes, totalBytes });
      } else if (event.event === 'Progress') {
        downloadedBytes += event.data.chunkLength;
        onProgress({ downloadedBytes, totalBytes });
      } else {
        onProgress({ downloadedBytes: totalBytes ?? downloadedBytes, totalBytes });
      }
    };

    await pendingUpdate.downloadAndInstall(handleEvent);
    pendingUpdate = null;
    const { relaunch } = await import('@tauri-apps/plugin-process');
    await relaunch();
  },
};

export type AppUpdateClient = typeof appUpdateClient;

export function createAppUpdateController(client: AppUpdateClient = appUpdateClient) {
  const state = writable<AppUpdateState>('idle');
  const currentVersion = writable('');
  const update = writable<AppUpdateInfo | null>(null);
  const progress = writable<AppUpdateProgress>({ downloadedBytes: 0 });
  const error = writable('');
  let checkVersion = 0;

  async function checkForUpdates(): Promise<boolean> {
    if (!client.supported()) {
      state.set('unsupported');
      return false;
    }

    const version = ++checkVersion;
    state.set('checking');
    error.set('');
    try {
      const next = await client.check();
      if (version !== checkVersion) return false;
      update.set(next);
      state.set(next ? 'available' : 'current');
      return Boolean(next);
    } catch (cause) {
      if (version !== checkVersion) return false;
      update.set(null);
      error.set(errorMessage(cause));
      state.set('error');
      return false;
    }
  }

  async function initialize(): Promise<void> {
    try {
      currentVersion.set(await client.currentVersion());
    } catch (cause) {
      error.set(errorMessage(cause));
    }
    await checkForUpdates();
  }

  async function installUpdate(): Promise<void> {
    if (get(state) !== 'available') return;
    state.set('downloading');
    error.set('');
    progress.set({ downloadedBytes: 0 });
    try {
      await client.install((next) => progress.set(next));
    } catch (cause) {
      error.set(errorMessage(cause));
      state.set(get(update) ? 'available' : 'error');
    }
  }

  return {
    state,
    currentVersion,
    update,
    progress,
    error,
    initialize,
    checkForUpdates,
    installUpdate,
  };
}

export type AppUpdateController = ReturnType<typeof createAppUpdateController>;
