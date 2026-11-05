import type { ToolEntry } from './types';

const labels: Record<string, string> = {
  invalid_url: 'Unsupported URL',
  network_policy: 'Network policy blocked access',
  dns: 'Address lookup failed',
  tls: 'Secure connection failed',
  connectivity: 'Connection failed',
  timeout: 'Website timed out',
  redirect_limit: 'Too many redirects',
  redirect_loop: 'Redirect loop',
  site_denied: 'Website refused access',
  authentication: 'Website requires login',
  rate_limited: 'Website rate limit',
  http: 'Website error',
  challenge: 'Website requires human verification',
  unsupported_document: 'Unsupported document',
  encoding: 'Unsupported page encoding',
  size_limit: 'Page exceeds download limit',
  rendering_required: 'Page needs browser rendering',
  search_unavailable: 'Search unavailable',
  search_authentication: 'Search key needs attention',
  search_quota: 'Search quota exhausted',
  renderer_unavailable: 'Browser rendering unavailable',
};

export function webFailure(
  tool: ToolEntry,
): { label: string; message: string; source: string } | null {
  if (!['web_fetch', 'web_search'].includes(tool.name) || tool.status !== 'error') return null;
  if (tool.denied) {
    let source = '';
    try {
      const url = new URL(JSON.parse(tool.arguments).url);
      if (['https:', 'http:'].includes(url.protocol)) {
        url.username = '';
        url.password = '';
        url.search = '';
        url.hash = '';
        source = url.href;
      }
    } catch {
      /* Search calls have no source page yet. */
    }
    return {
      label: 'Owner or policy denied access',
      message:
        'The request did not run. Review your web permission or host rules before trying again.',
      source,
    };
  }
  try {
    const failure = JSON.parse(tool.output ?? '').webError;
    if (
      failure &&
      Object.hasOwn(labels, failure.category) &&
      typeof failure.message === 'string' &&
      typeof failure.source === 'string'
    ) {
      return { label: labels[failure.category], message: failure.message, source: failure.source };
    }
  } catch {
    /* Older tool results remain visible in the expanded details. */
  }
  return null;
}
