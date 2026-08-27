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
  model?: string;
  messages: ModelMessage[];
}

export type AgentEvent =
  | { type: 'assistant_delta'; requestId: string; delta: string }
  | { type: 'turn_complete'; requestId: string; finishReason?: string }
  | { type: 'error'; requestId: string; message: string; code?: string };

export interface StreamCallbacks {
  onDelta: (delta: string) => void;
  onComplete?: () => void;
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
  model: string;
  expiresInMinutes: ShareExpiryMinutes;
}

export interface ShareStatus {
  active: boolean;
  shareUrl?: string;
  qrDataUrl?: string;
  model?: string;
  /** Unix epoch milliseconds. */
  expiresAt?: number;
  networkLabel?: string;
  requestCount?: number;
}
