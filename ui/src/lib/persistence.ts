import { invoke } from '@tauri-apps/api/core';
import type { SessionSummary } from './types';
import { isDesktop } from './setup';
export interface ConnectionProfile {
  endpoint: string;
  name: string;
  model?: string;
  /** "This model writes tool calls as text". Off when absent; such calls always ask. */
  textToolCalls?: boolean;
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
  tracesEnabled?: boolean;
  traceContent?: boolean;
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
  /** Runs only when the user types /name; the model never picks it on its own. */
  userOnly?: boolean;
}
export interface SkillProposal {
  id: string;
  skill: Skill;
  before: Skill | null;
  source: { sessionId: string; messageId: string };
}
export interface TraceSummary {
  id: string;
  startedMs: number;
  durationMs: number;
  error: string | null;
  modelCalls: number;
  toolCalls: number;
  inputTokens: number;
  outputTokens: number;
  model: string | null;
}
export interface TraceSpan {
  id: string;
  parent: string | null;
  name: string;
  startMs: number;
  durationMs: number;
  error: string | null;
  attributes: Record<string, unknown>;
  events: string[];
}
/** Token counts; `cachedInput` is part of `input` and `reasoning` part of `output`. */
export interface UsageTokens {
  input: number;
  cachedInput: number;
  output: number;
  reasoning: number;
}
export interface UsageSessionRow {
  id: string;
  models: string[];
  modelCalls: number;
  total: number;
}
export interface UsageGroupRow {
  /** Project folder name or model; `null` is Chat with no project. */
  name: string | null;
  tokens: UsageTokens;
  total: number;
  share: number;
  /** Projects only. */
  runs: number | null;
  sessions: number;
  modelCalls: number;
  failedCalls: number;
  medianOutputSpeed: number | null;
  p90Seconds: number | null;
  modes: { mode: string; total: number }[];
  sessionRows: UsageSessionRow[];
}
export interface UsageSummary {
  from: string;
  to: string;
  totals: {
    tokens: UsageTokens;
    total: number;
    modelCalls: number;
    failedCalls: number;
    runs: number;
    completedRuns: number;
    runsByOutcome: Record<string, number>;
    runSuccess: number | null;
    sessions: number;
    medianOutputSpeed: number | null;
    topModel: string | null;
    tokensSaved: number;
    compactions: number;
  };
  perDay: { day: string; tokens: UsageTokens; recorded: boolean }[];
  perProject: UsageGroupRow[];
  perModel: UsageGroupRow[];
  perMode: { mode: string; total: number; modelCalls: number; runs: number }[];
  tools: { name: string; calls: number; errors: number; denied: number; timeouts: number }[];
  gaps: { firstDay: string | null; droppedRuns: number };
  warnings: string[];
  telemetryEnabled: boolean;
  files: number;
  newestUnixMs: number | null;
  stale: boolean;
}
export interface TelemetryExport {
  folder: string;
  metricsFiles: number;
  traceFiles: number;
}
export interface SkillFinding {
  severity: 'medium' | 'high';
  reason: string;
  file: string;
  line: number;
  excerpt: string;
}
export interface SkillImportCandidate {
  origin: string;
  source: string;
  skill: Skill | null;
  error: string | null;
  notes: string[];
  exists: boolean;
  files: string[];
  license: string | null;
  findings: SkillFinding[];
}
export interface SuggestedSkill {
  name: string;
  description: string;
  repository: string;
  path: string;
  commit: string;
  license: string;
  saved: boolean;
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
/** Minutes east of UTC, as the usage commands count local days. */
export function utcOffsetMinutes(): number {
  return -new Date().getTimezoneOffset();
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
  skills: () =>
    command<{
      skills: Skill[];
      warnings: string[];
      /** Safety-scan findings per skill name, rescanned on every listing. */
      scans?: Record<string, SkillFinding[]>;
    }>('list_skills'),
  saveSkill: (skill: Skill) => command<void>('save_skill', { skill }),
  removeSkill: (name: string) => command<void>('delete_skill', { name }),
  skillProposals: () =>
    command<{
      proposals: SkillProposal[];
      warnings: string[];
      /** Safety-scan findings per proposal ID. */
      scans?: Record<string, SkillFinding[]>;
    }>('list_skill_proposals'),
  approveSkillProposal: (id: string, description: string, body: string) =>
    command<void>('approve_skill_proposal', { id, description, body }),
  rejectSkillProposal: (id: string) => command<void>('reject_skill_proposal', { id }),
  findSkillImports: () => command<SkillImportCandidate[]>('find_skill_imports'),
  chooseSkillImport: (folder: boolean) =>
    command<SkillImportCandidate[] | null>('choose_skill_import', { folder }),
  importSkill: (source: string, replace: boolean, allow: boolean) =>
    command<Skill>('import_skill', { source, replace, allow }),
  suggestedSkills: () => command<SuggestedSkill[]>('list_suggested_skills'),
  getSuggestedSkill: (name: string) =>
    command<SkillImportCandidate>('get_suggested_skill', { name }),
  discardSkillDownloads: (sources: string[]) =>
    command<void>('discard_skill_downloads', { sources }),
  traces: () => command<TraceSummary[]>('list_traces'),
  trace: (id: string) => command<TraceSpan[]>('read_trace', { id }),
  /** The last `days` local days, in this device's current time zone. */
  usage: (days: number) =>
    command<UsageSummary>('usage_summary', { days, offsetMinutes: utcOffsetMinutes() }),
  /** Opens a native folder picker; `null` when it is cancelled. */
  exportTelemetry: (days: number) =>
    command<TelemetryExport | null>('export_telemetry', {
      days,
      offsetMinutes: utcOffsetMinutes(),
    }),
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
