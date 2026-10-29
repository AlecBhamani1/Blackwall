import { invoke } from '@tauri-apps/api/core';

/** Whether the owner trusts the project's current `.blackwall/project.json`. */
export type Trust = { state: 'none' } | { state: 'trusted' } | { state: 'changed'; diff: string };

export interface RuleView {
  decision: 'allow' | 'ask' | 'deny';
  kind: 'exec' | 'file' | 'network' | 'subagent';
  subject: string;
  /** "matches every file", "any host", "any subdomain", or "your MCP server". */
  badges: string[];
}

export interface ProjectStatus {
  project: string;
  path: string;
  exists: boolean;
  trust: Trust;
  sha256: string | null;
  content: string | null;
  rules: RuleView[];
  mode: 'strictest' | 'ordered';
  applies: string[];
  waiting: string[];
  error: string | null;
}

export interface TrustedProject {
  /** The canonical folder, as untrust takes it. */
  path: string;
  /** The folder for display. */
  label: string;
  sha256: string;
}

export const projectClient = {
  status: (workspace: string) => invoke<ProjectStatus>('project_status', { workspace }),
  /** Trusts the file only if it still has `sha256`, the hash of the file that was shown. */
  trust: (workspace: string, sha256: string) =>
    invoke<ProjectStatus>('trust_project', { workspace, sha256 }),
  trusted: () => invoke<TrustedProject[]>('list_trusted_projects'),
  untrust: (path: string) => invoke<void>('untrust_project', { path }),
};
export type ProjectClient = typeof projectClient;

/** The banner shows when the file has settings waiting for trust, changed, or cannot be used. */
export function needsReview(status: ProjectStatus | null): boolean {
  if (!status?.exists) return false;
  if (status.error) return true;
  if (status.trust.state === 'changed') return true;
  return status.trust.state === 'none' && status.waiting.length > 0;
}

/** Show invisible format characters (bidi controls, zero-width, BOM: Unicode Cf) as \u{XXXX}. */
export function escapeInvisible(text: string): string {
  return text.replace(
    /\p{Cf}/gu,
    (character) => `\\u{${(character.codePointAt(0) ?? 0).toString(16).toUpperCase().padStart(4, '0')}}`,
  );
}

/** Text for review: invisible characters escaped and runs of more than two blank lines collapsed. */
export function displayText(text: string): string {
  const lines: string[] = [];
  let blank = 0;
  const flush = () => {
    if (blank > 2) lines.push(`[${blank - 2} more blank lines not shown]`);
    blank = 0;
  };
  for (const line of escapeInvisible(text).split('\n')) {
    if (line.trim() === '') {
      blank += 1;
      if (blank <= 2) lines.push(line);
      continue;
    }
    flush();
    lines.push(line);
  }
  flush();
  return lines.join('\n');
}

const DISMISSED_KEY = 'blackwall.project-trust-dismissed.v1';

/** The file version (hash, or the problem) the owner chose "Not now" for, per project. */
function dismissals(): Record<string, string> {
  try {
    const saved = JSON.parse(window.localStorage.getItem(DISMISSED_KEY) ?? '{}');
    return saved && typeof saved === 'object' ? saved : {};
  } catch {
    return {};
  }
}
function version(status: ProjectStatus): string {
  return status.sha256 ?? status.error ?? '';
}
export function isDismissed(status: ProjectStatus): boolean {
  return dismissals()[status.project] === version(status);
}
export function dismiss(status: ProjectStatus) {
  try {
    window.localStorage.setItem(
      DISMISSED_KEY,
      JSON.stringify({ ...dismissals(), [status.project]: version(status) }),
    );
  } catch {
    // Without storage the banner simply returns next time.
  }
}
