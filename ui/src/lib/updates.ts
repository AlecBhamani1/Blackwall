import { get, writable } from 'svelte/store';
import type { DownloadEvent, Update } from '@tauri-apps/plugin-updater';

export type AppUpdateChannel = 'stable' | 'beta';
const CHANNEL_STORAGE_KEY = 'blackwall.update-channel.v1';

function savedChannel(): AppUpdateChannel | null {
  try {
    const value = window.localStorage.getItem(CHANNEL_STORAGE_KEY);
    return value === 'stable' || value === 'beta' ? value : null;
  } catch {
    return null;
  }
}

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
  'idle' | 'checking' | 'current' | 'available' | 'downloading' | 'error' | 'unsupported';

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
let latestCheck = 0;

export const appUpdateClient = {
  supported(): boolean {
    return isTauriRuntime();
  },

  async currentVersion(): Promise<string> {
    if (!isTauriRuntime()) return 'development';
    const { getVersion } = await import('@tauri-apps/api/app');
    return getVersion();
  },

  async check(channel: AppUpdateChannel): Promise<AppUpdateInfo | null> {
    if (!isTauriRuntime()) return null;
    const request = ++latestCheck;
    const previous = pendingUpdate;
    pendingUpdate = null;
    await previous?.close();
    const [{ invoke }, { Update }] = await Promise.all([
      import('@tauri-apps/api/core'),
      import('@tauri-apps/plugin-updater'),
    ]);
    if (request !== latestCheck) return null;
    const metadata = await invoke<ConstructorParameters<typeof Update>[0] | null>(
      'check_app_update',
      { channel },
    );
    const next = metadata ? new Update(metadata) : null;
    if (request !== latestCheck) {
      await next?.close();
      return null;
    }
    pendingUpdate = next;
    if (!next) return null;
    return {
      currentVersion: next.currentVersion,
      version: next.version,
      date: next.date,
      body: next.body,
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
    await pendingUpdate.close();
    pendingUpdate = null;
    const { relaunch } = await import('@tauri-apps/plugin-process');
    await relaunch();
  },
};

export type AppUpdateClient = typeof appUpdateClient;

export function createAppUpdateController(client: AppUpdateClient = appUpdateClient) {
  const channel = writable<AppUpdateChannel>(savedChannel() ?? 'stable');
  const state = writable<AppUpdateState>('idle');
  const currentVersion = writable('');
  const update = writable<AppUpdateInfo | null>(null);
  const progress = writable<AppUpdateProgress>({ downloadedBytes: 0 });
  const error = writable('');
  let checkVersion = 0;

  async function checkForUpdates(): Promise<boolean> {
    if (get(state) === 'downloading') return false;
    if (!client.supported()) {
      state.set('unsupported');
      return false;
    }

    const version = ++checkVersion;
    state.set('checking');
    update.set(null);
    error.set('');
    try {
      const next = await client.check(get(channel));
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
      const installed = await client.currentVersion();
      currentVersion.set(installed);
      if (!savedChannel() && installed.includes('-beta.')) channel.set('beta');
    } catch (cause) {
      error.set(errorMessage(cause));
    }
    await checkForUpdates();
  }

  async function setChannel(next: AppUpdateChannel): Promise<void> {
    if (get(state) === 'downloading' || (next !== 'stable' && next !== 'beta')) return;
    try {
      window.localStorage.setItem(CHANNEL_STORAGE_KEY, next);
    } catch {
      ++checkVersion;
      update.set(null);
      state.set('error');
      error.set('Blackwall could not save your update channel. Try again.');
      return;
    }
    channel.set(next);
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
    channel,
    setChannel,
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
