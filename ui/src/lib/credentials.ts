import { invoke } from '@tauri-apps/api/core';
import { isDesktop } from './setup';

export type CredentialKind = 'model' | 'relay';
export const credentialClient = {
  available: isDesktop,
  status: (kind: CredentialKind, endpoint: string) =>
    invoke<boolean>('has_saved_credential', { kind, endpoint }),
  remove: (kind: CredentialKind, endpoint: string) =>
    invoke<void>('remove_saved_credential', { kind, endpoint }),
  saveModel: (endpoint: string, key: string) => invoke<void>('save_model_key', { endpoint, key }),
};
