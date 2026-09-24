import { invoke } from '@tauri-apps/api/core';
import { createId } from './id';
import { nativeModelError } from './modelError';

export interface LocalService {
  name: string;
  endpoint: string;
  available: boolean;
  models: string[];
  supportsDownload: boolean;
}
export interface DownloadProgress {
  status: string;
  completed: number;
  total: number;
}
export const STARTER_MODELS = [
  {
    id: 'llama3.2:1b',
    name: 'Llama 3.2 · Small',
    size: 'About 1.3 GB',
    description: 'A light starting point for writing and everyday questions.',
  },
  {
    id: 'llama3.2:3b',
    name: 'Llama 3.2 · Standard',
    size: 'About 2 GB',
    description: 'More capable answers, with a little more memory needed.',
  },
];
export function isDesktop(): boolean {
  return typeof window !== 'undefined' && Boolean(window.__TAURI_INTERNALS__);
}

export function normalizeEndpoint(value: string): string {
  const trimmed = value.trim();
  if (!trimmed || trimmed.length > 2048)
    throw new Error('Enter the address of your model service.');
  let url: URL;
  try {
    url = new URL(trimmed.includes('://') ? trimmed : `http://${trimmed}`);
  } catch {
    throw new Error('Check the computer address and try again.');
  }
  if (!['http:', 'https:'].includes(url.protocol) || !url.hostname)
    throw new Error('Use an HTTP or HTTPS address.');
  if (url.username || url.password || url.search || url.hash)
    throw new Error('Keep passwords and access keys out of the computer address.');
  if (url.pathname === '/') url.pathname = '/v1';
  return url.toString().replace(/\/$/, '');
}

export function invitationUrl(value: string): string {
  let url: URL;
  try {
    url = new URL(value.trim());
  } catch {
    throw new Error('Paste the complete invitation link you received.');
  }
  if (
    url.protocol !== 'https:' ||
    url.username ||
    url.password ||
    url.search ||
    !/^\/s\/bws_[A-Za-z0-9_-]{43}\/guest$/.test(url.pathname) ||
    !/^#key=bw1_[A-Za-z0-9_-]{43}$/.test(url.hash)
  ) {
    throw new Error(
      'This does not look like a complete Blackwall invitation. Ask the sender for a new link.',
    );
  }
  return url.toString();
}

export function connectionHelp(cause: unknown): string {
  const record =
    cause && typeof cause === 'object' ? (cause as { code?: string; message?: string }) : {};
  const detail =
    cause instanceof Error
      ? cause.message
      : (record.message ?? (typeof cause === 'string' ? cause : ''));
  if (record.code === 'credential_error') return nativeModelError(cause).message;
  if (record.code === 'model_access_denied' || /401|403|unauthor|forbidden/i.test(detail))
    return 'This service needs an access key, or the saved key is no longer valid. Check its connection settings.';
  if (record.code === 'model_not_found')
    return 'This model connection is unavailable. Open and unlock the model computer. If access was removed, pair again.';
  if (/no models|did not report any models/i.test(detail))
    return 'The service is running, but no models are installed. Add a model to continue.';
  if (/timeout|timed out/i.test(detail) || record.code === 'model_timeout')
    return 'Your model computer is taking too long to respond. Make sure it is awake, then try again.';
  if (
    [
      'Enter the address of your model service.',
      'Check the computer address and try again.',
      'Use an HTTP or HTTPS address.',
      'Keep passwords and access keys out of the computer address.',
    ].includes(detail)
  )
    return detail;
  return 'Blackwall could not reach your model. Make sure the model service is open and the computer is awake, then try again.';
}

export const setupClient = {
  async discover(): Promise<LocalService[]> {
    if (!isDesktop()) return [];
    return invoke<LocalService[]>('discover_local_services');
  },
  async openLink(kind: 'download' | 'invitation', invitation?: string): Promise<void> {
    const target =
      kind === 'invitation' ? invitationUrl(invitation ?? '') : 'https://ollama.com/download/mac';
    if (!isDesktop()) {
      const opened = window.open(target, '_blank', 'noopener,noreferrer');
      // noopener may return null even when the browser successfully opens the tab.
      void opened;
      return;
    }
    await invoke('open_setup_link', { kind, invitation: kind === 'invitation' ? target : null });
  },
  async download(
    model: string,
    onProgress: (progress: DownloadProgress) => void,
    signal: AbortSignal,
  ): Promise<void> {
    if (!isDesktop())
      throw new Error('Model downloads are available in the installed Blackwall app.');
    const { listen } = await import('@tauri-apps/api/event');
    const requestId = createId('download');
    const unlisten = await listen<DownloadProgress & { requestId: string }>(
      'blackwall://download',
      ({ payload }) => {
        if (payload.requestId === requestId && !signal.aborted) onProgress(payload);
      },
    );
    const cancel = () => {
      void invoke('cancel_request', { requestId }).catch(() => {});
    };
    signal.addEventListener('abort', cancel, { once: true });
    try {
      if (signal.aborted) throw new DOMException('Download stopped.', 'AbortError');
      await invoke('download_local_model', { requestId, model });
      if (signal.aborted) throw new DOMException('Download stopped.', 'AbortError');
    } catch (cause) {
      if (signal.aborted) throw new DOMException('Download stopped.', 'AbortError');
      throw cause;
    } finally {
      signal.removeEventListener('abort', cancel);
      unlisten();
    }
  },
};
export type SetupClient = typeof setupClient;
