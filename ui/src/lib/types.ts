export type AttachmentKind = 'image' | 'text' | 'file';

export interface Attachment {
  id: string;
  name: string;
  mimeType: string;
  sizeBytes: number;
  kind: AttachmentKind;
  previewUrl?: string;
  dataUrl?: string;
  textContent?: string;
}

export interface PendingAttachment extends Attachment {
  file: File;
  fingerprint: string;
}

export type MessageRole = 'user' | 'assistant';
export type MessageStatus = 'sending' | 'streaming' | 'complete' | 'error' | 'stopped';

export interface ChatMessage {
  id: string;
  role: MessageRole;
  content: string;
  attachments: Attachment[];
  status: MessageStatus;
  createdAt: number;
  error?: string;
  tools?: ToolEntry[];
  children?: ChildEntry[];
  /** Restored tool activity is inert history, never live approval state. */
  historical?: boolean;
  /** The model's reasoning, shown collapsed; never sent back to the model. */
  reasoning?: string;
  reasoningMs?: number;
}

export interface ModelInfo {
  id: string;
  name: string;
  sizeBytes?: number;
  ownedBy?: string;
}

export type ConnectionState = 'checking' | 'ready' | 'offline';
export type RunState = 'idle' | 'preparing' | 'streaming' | 'error';

export interface AttachmentPayload {
  id: string;
  name: string;
  mimeType: string;
  sizeBytes: number;
  kind: AttachmentKind;
  dataUrl?: string;
  textContent?: string;
}

export interface ModelMessage {
  role: MessageRole;
  content: string;
  attachments?: AttachmentPayload[];
}

export interface TokenUsage {
  promptTokens: number;
  completionTokens: number;
  totalTokens: number;
}
export interface ContextBudget {
  contextWindow: number;
  outputTokens: number;
  autoCompact: boolean;
  /** Omitted limits use the runtime defaults of 20 iterations and 120 seconds. */
  maxIterations?: number;
  commandTimeoutSeconds?: number;
  /** Command output over this many bytes is trimmed to its start and end; 65,536 by default. */
  maxOutputBytes?: number;
}
export interface ContextReport {
  estimatedPromptTokens: number;
  toolTokens: number;
  outputTokens: number;
  safetyTokens: number;
  contextWindow: number;
  serverUsage?: TokenUsage;
}
export interface ContextSummary {
  taskRequirements: string[];
  corrections: string[];
  decisions: string[];
  changedFiles: string[];
  checks: string[];
  unfinishedWork: string[];
  relevantContext: string[];
}
export interface ContextState {
  transcript: Record<string, unknown>[];
  checkpoints: Array<{ through: number; sourceHash: string; summary: ContextSummary }>;
  coveredMessages: number;
  sourceHash: string;
  serverUsage?: TokenUsage;
}

export interface ChatRequest {
  requestId: string;
  sessionId?: string;
  sourceMessageId?: string;
  agentMode?: boolean;
  workspace?: string;
  webEnabled?: boolean;
  executionProfile?: 'read-only' | 'workspace-write' | 'unsandboxed';
  commandNetwork?: boolean;
  endpoint?: string;
  model?: string;
  messages: ModelMessage[];
  contextBudget?: ContextBudget;
  contextState?: ContextState;
  compact?: boolean;
  /** The active connection reads tool calls the model writes as text. */
  textToolCalls?: boolean;
}

/** Why an MCP server did not start. Shown to the owner only: never sent, saved or traced. */
export interface ServerDiagnostic {
  server: string;
  stderr: string;
  notPassed?: string[];
}

export type AgentEvent =
  | {
      type: 'instructions_loaded';
      requestId: string;
      agentId?: string;
      sources: string[];
      warnings: string[];
      diagnostics?: ServerDiagnostic[];
    }
  | { type: 'context_report'; requestId: string; report: ContextReport }
  | { type: 'context_updated'; requestId: string; state: ContextState }
  | { type: 'assistant_delta'; requestId: string; delta: string }
  | {
      type: 'turn_complete';
      requestId: string;
      finishReason?: string;
      usage?: TokenUsage;
      reasoning?: string;
      reasoningMs?: number;
    }
  | { type: 'error'; requestId: string; message: string; code?: string; detail?: string }
  | { type: 'subagent_status'; requestId: string; agentId: string; state: string; summary?: string }
  | { type: 'tool_call'; requestId: string; toolCallId: string; name: string; arguments: string }
  | {
      type: 'tool_result';
      requestId: string;
      toolCallId: string;
      output: string;
      success: boolean;
      /** Command output over the cap was trimmed to its start and end. */
      truncated?: boolean;
      /** A rule or the user denied the action, so it did not run. */
      denied?: boolean;
    }
  | {
      type: 'approval_request';
      requestId: string;
      approvalId: string;
      kind: string;
      detail: string;
      tool?: ApprovalTool;
      remember?: boolean;
      offDevice?: boolean;
      outsideSandbox?: boolean;
    };

export interface ChildEntry {
  id: string;
  state: string;
  summary: string;
}
export interface ToolEntry {
  id: string;
  name: string;
  arguments: string;
  /** Position in the response text when called; absent in older saved conversations. */
  contentOffset?: number;
  output?: string;
  /** The output was trimmed to fit the command output cap. */
  truncated?: boolean;
  /** Owner-only server details; dropped before a conversation is saved or exported. */
  diagnostics?: ServerDiagnostic[];
  status: 'running' | 'complete' | 'error' | 'stopped';
}
/** The MCP tool an approval is for; `tool` is the namespaced `server__tool` name. */
export interface ApprovalTool {
  server: string;
  tool: string;
}

export type ApprovalDecision =
  | 'allow'
  | 'always_allow'
  | 'deny'
  | 'allow_tool_in_project'
  | 'allow_tool'
  | 'allow_server_in_project'
  | 'allow_server';

export interface PendingApproval {
  requestId: string;
  approvalId: string;
  kind: string;
  detail: string;
  tool?: ApprovalTool;
  /** False for a tool call the model wrote as text: it can only be allowed once. */
  remember?: boolean;
  /** The action hands data to a program that may send it off this device. */
  offDevice?: boolean;
  /** The program runs outside the sandbox with the owner's account permissions. */
  outsideSandbox?: boolean;
}

export interface StreamCallbacks {
  onDelta: (delta: string) => void;
  onComplete?: () => void;
  onEvent?: (event: AgentEvent) => void;
}

export type ConversationMode = 'chat' | 'agent';

export interface SessionSummary {
  // Older conversations did not record their mode or project. Treat them as Chat.
  mode?: ConversationMode;
  workspace?: string;
  id: string;
  title: string;
  updatedAt: number;
  messages: ChatMessage[];
  contextState?: ContextState;
  contextReport?: ContextReport;
}

export interface AttachmentRejection {
  fileName: string;
  reason: string;
}

export type ShareExpiryMinutes = 15 | 60 | 480;

export interface StartShareRequest {
  name?: string;
  model: string;
  endpoint?: string;
  relayUrl?: string;
  relayToken?: string;
  expiresInMinutes: ShareExpiryMinutes;
}

export interface ShareStatus {
  id?: string;
  name?: string;
  active: boolean;
  shareUrl?: string;
  qrDataUrl?: string;
  model?: string;
  /** Unix epoch milliseconds. */
  expiresAt?: number;
  networkLabel?: string;
  requestCount?: number;
}
