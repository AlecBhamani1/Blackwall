import { invoke } from '@tauri-apps/api/core';
import { writable, type Writable } from 'svelte/store';
import { isDesktop } from './setup';
export interface PairingCandidate {
  id: string;
  name: string;
  salt: string;
  hash: string;
}
export interface PairingProgress {
  state: string;
  hostName: string;
  model: string;
  expiresAt: number;
  candidate: PairingCandidate | null;
  confirmation: string | null;
  sessionId: string | null;
}
export interface PairedDevice {
  id: string;
  role: 'host' | 'client';
  name: string;
  model: string;
  endpoint: string;
  state: string;
  online: boolean;
}
export interface PairingSnapshot {
  devices: PairedDevice[];
  host: PairingProgress | null;
  client: PairingProgress | null;
  warnings: string[];
}
const empty = (): PairingSnapshot => ({ devices: [], host: null, client: null, warnings: [] });
export interface PairingClient {
  snapshot: Writable<PairingSnapshot>;
  refresh(): Promise<void>;
  create(input: {
    relayUrl: string;
    relayKey: string;
    name: string;
    model: string;
    endpoint: string;
  }): Promise<{ invitation: string; expiresAt: number }>;
  join(invitation: string, name: string): Promise<void>;
  approve(candidate: PairingCandidate): Promise<void>;
  cancel(role: 'host' | 'client'): Promise<void>;
  remove(id: string): Promise<void>;
  retry(id: string): Promise<void>;
}
export const pairingSnapshot = writable<PairingSnapshot>(empty());
let polling: ReturnType<typeof setInterval> | undefined;
let generation = 0;
let refreshing = false;
async function refresh() {
  if (!isDesktop() || refreshing) return;
  const version = generation;
  refreshing = true;
  try {
    const next = await invoke<PairingSnapshot>('pairing_status');
    if (version === generation) pairingSnapshot.set(next);
  } catch (cause) {
    if (version === generation) {
      pairingSnapshot.update((current) => ({ ...current, warnings: [pairingError(cause)] }));
    }
    throw cause;
  } finally {
    refreshing = false;
  }
}
export const pairingClient: PairingClient = {
  snapshot: pairingSnapshot,
  refresh,
  create: (input) => invoke('create_pairing', input),
  join: (invitation, name) => invoke('join_pairing', { invitation, name }),
  approve: (candidate) => invoke('approve_pairing', { candidate }),
  cancel: (role) => invoke('cancel_pairing', { role }),
  remove: (id) => invoke('remove_paired_device', { id }),
  retry: (id) => invoke('retry_paired_device', { id }),
};
export function startPairing() {
  if (!isDesktop() || polling) return;
  const update = () => {
    void refresh().catch(() => {
      /* Explicit controls surface retryable errors. */
    });
  };
  update();
  polling = setInterval(update, 4000);
}
export function stopPairing() {
  generation += 1;
  if (polling) clearInterval(polling);
  polling = undefined;
  pairingSnapshot.set(empty());
}
export function pairingError(cause: unknown) {
  return typeof cause === 'string'
    ? cause
    : cause instanceof Error
      ? cause.message
      : 'Pairing could not finish. Please try again.';
}

export function isPairedEndpoint(endpoint: string): boolean {
  try {
    return /^\/s\/bws_[A-Za-z0-9_-]{43}\/v1\/?$/.test(new URL(endpoint).pathname);
  } catch {
    return false;
  }
}
