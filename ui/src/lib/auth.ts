import { invoke } from '@tauri-apps/api/core';
import { writable } from 'svelte/store';

export interface AuthStatus {
  locked: boolean;
  enabled: boolean;
  /** False when the app opened without a system keyring. */
  keyring: boolean;
  /** The data has a lock, or a lock state that can't be confirmed, and the keyring is unavailable. */
  keyringNeeded: boolean;
}
/** The message the backend returns when a keyring is needed to open the app. */
export const KEYRING_NEEDED =
  'Blackwall needs the system keyring to open this data folder. Start your keyring (GNOME Keyring or KWallet), then try again.';
/** Shown on every setting that can only be saved in a keyring. */
export const NO_KEYRING_NOTE =
  "No system keyring is available, so this can't be saved. Start GNOME Keyring or KWallet, then restart Blackwall.";
/** Shown beside an access key that was used but not saved. */
export const KEY_NOT_SAVED = 'Not saved: no system keyring';
export const authStatus = writable<AuthStatus>({
  locked: true,
  enabled: false,
  keyring: true,
  keyringNeeded: false,
});
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
