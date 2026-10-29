import type { ConnectionProfile } from './persistence';
import { normalizeEndpoint } from './setup';

export const MAX_CONNECTIONS = 20;

export function connectionName(endpoint: string, name?: string): string {
  let label = Array.from(name?.trim() || new URL(normalizeEndpoint(endpoint)).host)
    .slice(0, 120)
    .join('');
  // Native preferences bound names by UTF-8 bytes, rather than JS string length.
  while (new TextEncoder().encode(label).length > 120)
    label = Array.from(label).slice(0, -1).join('');
  return label;
}

/** Read only non-secret profile fields and use one identity for equivalent addresses. */
export function connectionProfiles(value: unknown): ConnectionProfile[] {
  if (!Array.isArray(value)) return [];
  const profiles: ConnectionProfile[] = [];
  for (const item of value) {
    if (!item || typeof item.endpoint !== 'string') continue;
    try {
      const endpoint = normalizeEndpoint(item.endpoint);
      if (profiles.some((profile) => profile.endpoint === endpoint)) continue;
      profiles.push({
        endpoint,
        name: connectionName(endpoint, typeof item.name === 'string' ? item.name : undefined),
        ...(typeof item.model === 'string' && item.model.length <= 2048
          ? { model: item.model }
          : {}),
        ...(item.textToolCalls === true ? { textToolCalls: true } : {}),
      });
      if (profiles.length === MAX_CONNECTIONS) break;
    } catch {
      // Leave invalid legacy addresses out of the switcher.
    }
  }
  return profiles;
}
