import { describe, expect, it } from 'vitest';
import { connectionName, connectionProfiles } from './connections';

describe('saved connection profiles', () => {
  it('keeps the text tool call setting only when it is on, so older profiles load unchanged', () => {
    expect(
      connectionProfiles([
        { endpoint: 'http://a.example/v1', name: 'A', textToolCalls: true },
        { endpoint: 'http://b.example/v1', name: 'B', textToolCalls: 'yes' },
        { endpoint: 'http://c.example/v1', name: 'C' },
      ]),
    ).toEqual([
      { endpoint: 'http://a.example/v1', name: 'A', textToolCalls: true },
      { endpoint: 'http://b.example/v1', name: 'B' },
      { endpoint: 'http://c.example/v1', name: 'C' },
    ]);
  });

  it('normalizes addresses, deduplicates aliases and strips secrets and invalid addresses', () => {
    expect(
      connectionProfiles([
        { endpoint: 'https://MODEL.example:443/', name: 'Service', model: 'first', key: 'private' },
        { endpoint: 'https://model.example/v1', name: 'Alias', model: 'second' },
        { endpoint: 'https://user:password@other.example', name: 'Unsafe' },
        { endpoint: 'https://other.example?key=private', name: 'Unsafe' },
        { endpoint: 'https://other.example/custom/v1', name: 'Other' },
        null,
      ]),
    ).toEqual([
      { endpoint: 'https://model.example/v1', name: 'Service', model: 'first' },
      { endpoint: 'https://other.example/custom/v1', name: 'Other' },
    ]);
  });

  it('keeps names within the native UTF-8 byte limit', () => {
    const name = connectionName('https://model.example', '🤖'.repeat(80));
    expect(new TextEncoder().encode(name).length).toBe(120);
    expect(name).toBe('🤖'.repeat(30));
  });
});
