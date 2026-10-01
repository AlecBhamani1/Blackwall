export type Platform = 'macos' | 'windows' | 'linux';

export interface PlatformCopy {
  /** Noun for this device, as in "Saved on this Mac". */
  device: string;
  /** Short credential-store name, as in "Saved in Keychain". */
  store: string;
  /** Full credential-store name for explanatory text. */
  storeName: string;
  /** Recovery step when the credential store does not respond. */
  storeHelp: string;
  /** Operating-system account that can still read local files. */
  account: string;
  /** Disk encryption that protects local files at rest. */
  diskEncryption: string;
  ollamaDownload: string;
}

const COPY: Record<Platform, PlatformCopy> = {
  macos: {
    device: 'Mac',
    store: 'Keychain',
    storeName: 'macOS Keychain',
    storeHelp: 'Respond to any macOS Keychain prompt, unlock your login keychain if needed',
    account: 'macOS account',
    diskEncryption: 'FileVault',
    ollamaDownload: 'https://ollama.com/download/mac',
  },
  windows: {
    device: 'PC',
    store: 'Credential Manager',
    storeName: 'Windows Credential Manager',
    storeHelp: 'Make sure you are signed in to Windows and Credential Manager is available',
    account: 'Windows account',
    diskEncryption: 'BitLocker or device encryption',
    ollamaDownload: 'https://ollama.com/download/windows',
  },
  linux: {
    device: 'computer',
    store: 'keyring',
    storeName: 'your system keyring',
    storeHelp:
      'Respond to any keyring unlock prompt and make sure GNOME Keyring, KWallet, or another Secret Service provider is running',
    account: 'Linux account',
    diskEncryption: 'full-disk encryption such as LUKS',
    ollamaDownload: 'https://ollama.com/download/linux',
  },
};

export function detectPlatform(userAgent = globalThis.navigator?.userAgent ?? ''): Platform {
  if (/Windows/i.test(userAgent)) return 'windows';
  if (/Macintosh|Mac OS X/i.test(userAgent)) return 'macos';
  return 'linux';
}

export function platformCopy(platform: Platform = detectPlatform()): PlatformCopy {
  return COPY[platform];
}

/** Copy for the platform running this webview. */
export const platform = platformCopy();

/** Matches native credential-store errors on every supported platform. */
export const credentialStoreError = /keychain|keyring|credential manager/i;
