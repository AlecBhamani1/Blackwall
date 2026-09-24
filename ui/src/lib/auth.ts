import { invoke } from '@tauri-apps/api/core';
import { writable } from 'svelte/store';

export interface AuthStatus {
  locked: boolean;
  enabled: boolean;
}
export const authStatus = writable<AuthStatus>({ locked: true, enabled: false });
export const authClient = {
  status: () => invoke<AuthStatus>('auth_status'),
  unlock: (passphrase: string) => invoke<AuthStatus>('unlock_app', { passphrase }),
  configure: (current: string, passphrase: string) =>
    invoke<AuthStatus>('set_passphrase', { current, passphrase }),
  lock: () => invoke<AuthStatus>('lock_app'),
};
export function authError(cause: unknown): string {
  return typeof cause === 'string'
    ? cause
    : cause instanceof Error
      ? cause.message
      : 'Blackwall could not update the app lock. Try again.';
}
