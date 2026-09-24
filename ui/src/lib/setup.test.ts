import { describe, expect, it } from 'vitest';
import { normalizeEndpoint, invitationUrl, connectionHelp } from './setup';

describe('connection boundaries', () => {
  it('normalizes supported addresses and rejects embedded credentials', () => {
    expect(normalizeEndpoint('my-computer.local:11434')).toBe('http://my-computer.local:11434/v1');
    for (const value of [
      'javascript:alert(1)',
      'file:///etc/passwd',
      'https://user:secret@host',
      'https://host?token=secret',
      'https://host#secret',
    ]) {
      expect(() => normalizeEndpoint(value)).toThrow();
    }
  });
  it('accepts only complete HTTPS guest invitations', () => {
    const good = `https://relay.test/s/bws_${'a'.repeat(43)}/guest#key=bw1_${'b'.repeat(43)}`;
    expect(invitationUrl(good)).toBe(good);
    for (const bad of [
      good.replace('https:', 'http:'),
      good.split('#')[0],
      'javascript:alert(1)',
      good.replace('/guest', '/other'),
    ])
      expect(() => invitationUrl(bad)).toThrow();
  });
  it('maps errors to recovery actions without echoing private server responses', () => {
    expect(connectionHelp(new Error('HTTP 401 private details'))).toContain('access key');
    expect(connectionHelp(new Error('HTTP 500 secret body'))).not.toContain('secret');
    expect(connectionHelp({ code: 'model_timeout' })).toContain('awake');
    expect(connectionHelp({ code: 'model_not_found' })).toContain('pair again');
    expect(connectionHelp({ code: 'model_access_denied' })).toContain('access key');
    expect(connectionHelp({ code: 'credential_error', message: 'private credential' })).toContain(
      'Keychain',
    );
    expect(
      connectionHelp({ code: 'credential_error', message: 'private credential' }),
    ).not.toContain('private');
    expect(connectionHelp(new Error('did not report any models'))).toContain('Add a model');
  });
});
