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

export interface ChatRequest {
  requestId: string;
  agentMode?: boolean;
  webEnabled?: boolean;
  endpoint?: string;
  model?: string;
  messages: ModelMessage[];
}

export type AgentEvent =
  | { type: 'assistant_delta'; requestId: string; delta: string }
  | { type: 'turn_complete'; requestId: string; finishReason?: string }
  | { type: 'error'; requestId: string; message: string; code?: string }
  | { type: 'subagent_status'; requestId: string; agentId: string; state: string; summary?: string }
  | { type: 'tool_call'; requestId: string; toolCallId: string; name: string; arguments: string }
  | { type: 'tool_result'; requestId: string; toolCallId: string; output: string; success: boolean }
  | {
      type: 'approval_request';
      requestId: string;
      approvalId: string;
      kind: string;
      detail: string;
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
  output?: string;
  status: 'running' | 'complete' | 'error' | 'stopped';
}
export interface PendingApproval {
  requestId: string;
  approvalId: string;
  kind: string;
  detail: string;
}

export interface StreamCallbacks {
  onDelta: (delta: string) => void;
  onComplete?: () => void;
  onEvent?: (event: AgentEvent) => void;
}

export interface SessionSummary {
  id: string;
  title: string;
  updatedAt: number;
  messages: ChatMessage[];
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
