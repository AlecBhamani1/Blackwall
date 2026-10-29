import { invoke } from '@tauri-apps/api/core';

/** Whether the owner trusts the project's current `.blackwall/project.json`. */
export type Trust = { state: 'none' } | { state: 'trusted' } | { state: 'changed'; diff: string };

export interface ProjectStatus {
  project: string;
  path: string;
  exists: boolean;
  trust: Trust;
  sha256: string | null;
  content: string | null;
  applies: string[];
  waiting: string[];
  error: string | null;
}

export interface TrustedProject {
  path: string;
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
