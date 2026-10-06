import { invoke } from '@tauri-apps/api/core';
import type { SessionSummary } from './types';
import { isDesktop } from './setup';
export interface ConnectionProfile {
  endpoint: string;
  name: string;
  model?: string;
}
export interface Preferences {
  connections?: ConnectionProfile[];
  endpoint?: string;
  model?: string;
  connectionName?: string;
  relayUrl?: string;
  contextWindow?: number;
  memoryEnabled?: boolean;
  workspace?: string;
}
export interface Skill {
  name: string;
  description: string;
  body: string;
  enabled: boolean;
}
export interface MemoryEntry {
  id: string;
  content: string;
  updatedAt: number;
  scope?: 'user' | 'project' | 'environment';
  workspace?: string;
  key?: string;
  revision?: number;
  source?: { sessionId: string; messageId: string } | null;
}
export interface MemoryProposal {
  id: string;
  entry: MemoryEntry;
  before: MemoryEntry | null;
  source: { sessionId: string; messageId: string };
  createdAt: number;
}
async function command<T>(name: string, args?: Record<string, unknown>): Promise<T> {
  return invoke<T>(name, args);
}
export const persistence = {
  available: isDesktop,
  list: () => command<SessionSummary[]>('list_sessions'),
  load: (id: string) => command<SessionSummary | null>('load_session', { id }),
  save: (session: SessionSummary) => command<void>('save_session', { session }),
  remove: (id: string) => command<void>('delete_session', { id }),
  migrate: (sessions: SessionSummary[]) => command<void>('migrate_sessions', { sessions }),
  preferences: () => command<Preferences>('load_preferences'),
  savePreferences: (preferences: Preferences) => command<void>('save_preferences', { preferences }),
  skills: () => command<{ skills: Skill[]; warnings: string[] }>('list_skills'),
  saveSkill: (skill: Skill) => command<void>('save_skill', { skill }),
  removeSkill: (name: string) => command<void>('delete_skill', { name }),
  memories: (query = '') => command<MemoryEntry[]>('list_memories', { query }),
  memoryProposals: () => command<MemoryProposal[]>('list_memory_proposals'),
  approveMemory: (id: string, content: string | null = null) =>
    command<void>('approve_memory_proposal', { id, content }),
  rejectMemory: (id: string) => command<void>('reject_memory_proposal', { id }),
  saveMemory: (entry: MemoryEntry) => command<void>('save_memory', { entry }),
  removeMemory: (id: string) => command<void>('delete_memory', { id }),
  saveKey: (endpoint: string, key: string) => command<void>('save_model_key', { endpoint, key }),
};
export type Persistence = typeof persistence;
