import { invoke } from '@tauri-apps/api/core';
import { writable } from 'svelte/store';

export interface AuthStatus {
  locked: boolean;
  enabled: boolean;
  /** False when the app opened without a system keyring. */
  keyring: boolean;
  /** A lock or saved data exists but its keyring is unavailable, so it cannot be opened yet. */
  keyringNeeded: boolean;
}
/** The message the backend returns when a keyring is needed to open the app. */
export const KEYRING_NEEDED =
  "Your app lock is stored in the system keyring, which isn't available. Start your keyring (GNOME Keyring or KWallet), then try again.";
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
