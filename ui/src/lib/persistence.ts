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
  outputTokens?: number;
  autoCompact?: boolean;
  maxIterations?: number;
  commandTimeoutSeconds?: number;
  memoryEnabled?: boolean;
  recallEnabled?: boolean;
  workspace?: string;
}
export interface RecallQuery {
  query: string;
  project?: string;
  after?: number;
  before?: number;
  limit?: number;
}
export interface RecallHit {
  sessionId: string;
  messageId: string;
  messageIndex: number;
  title: string;
  mode: string;
  project: string | null;
  updatedAt: number;
  role: string;
  excerpt: string;
}
export interface RecallResults {
  results: RecallHit[];
  remainingSessions: number;
}
export interface Skill {
  name: string;
  description: string;
  body: string;
  enabled: boolean;
}
export interface McpServer {
  name: string;
  command: string[];
  enabled: boolean;
  allowTools?: string[];
  denyTools?: string[];
  secrets?: string[];
}
export interface McpToolRule {
  tool: string;
  project: string | null;
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
  search: (query: RecallQuery) => command<RecallResults>('search_sessions', { query }),
  load: (id: string) => command<SessionSummary | null>('load_session', { id }),
  save: (session: SessionSummary) => command<void>('save_session', { session }),
  remove: (id: string) => command<void>('delete_session', { id }),
  migrate: (sessions: SessionSummary[]) => command<void>('migrate_sessions', { sessions }),
  preferences: () => command<Preferences>('load_preferences'),
  savePreferences: (preferences: Preferences) => command<void>('save_preferences', { preferences }),
  skills: () => command<{ skills: Skill[]; warnings: string[] }>('list_skills'),
  saveSkill: (skill: Skill) => command<void>('save_skill', { skill }),
  removeSkill: (name: string) => command<void>('delete_skill', { name }),
  mcpServers: () => command<McpServer[]>('list_mcp_servers'),
  addMcpServer: (name: string, commandLine: string) =>
    command<void>('add_mcp_server', { name, command: commandLine }),
  setMcpServerEnabled: (name: string, enabled: boolean) =>
    command<void>('set_mcp_server_enabled', { name, enabled }),
  removeMcpServer: (name: string) => command<void>('remove_mcp_server', { name }),
  saveMcpSecret: (name: string, key: string, value: string) =>
    command<void>('save_mcp_secret', { name, key, value }),
  removeMcpSecret: (name: string, key: string) => command<void>('delete_mcp_secret', { name, key }),
  mcpToolRules: () => command<McpToolRule[]>('list_mcp_tool_rules'),
  removeMcpToolRule: (rule: McpToolRule) => command<void>('remove_mcp_tool_rule', { rule }),
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
