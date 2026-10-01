import { describe, expect, it } from 'vitest';
import { credentialStoreError, detectPlatform, platformCopy } from './platform';

describe('platform copy', () => {
  it('detects each desktop webview platform', () => {
    expect(
      detectPlatform('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15'),
    ).toBe('macos');
    expect(
      detectPlatform('Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Edg/120.0'),
    ).toBe('windows');
    expect(detectPlatform('Mozilla/5.0 (X11; Linux aarch64) AppleWebKit/605.1.15')).toBe('linux');
  });

  it('names the native credential store and matches its errors', () => {
    expect(platformCopy('macos').storeName).toBe('macOS Keychain');
    expect(platformCopy('windows').storeName).toBe('Windows Credential Manager');
    expect(platformCopy('linux').storeHelp).toContain('Secret Service');
    for (const message of [
      'Blackwall could not save the access key in Keychain.',
      'Blackwall could not save the access key in Windows Credential Manager.',
      'No system keyring is available.',
    ])
      expect(message).toMatch(credentialStoreError);
    expect('The service could not be reached.').not.toMatch(credentialStoreError);
  });

  it('offers the Ollama download for the running platform', () => {
    expect(platformCopy('windows').ollamaDownload).toBe('https://ollama.com/download/windows');
    expect(platformCopy('linux').ollamaDownload).toBe('https://ollama.com/download/linux');
  });
});
