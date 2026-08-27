import { derived, get, writable } from 'svelte/store';
import { materializeAttachment, revokeAttachmentPreview } from './attachments';
import { createId } from './id';
import { localModelClient, type LocalModelClient } from './ipc';
import type {
  Attachment,
  ChatMessage,
  ConnectionState,
  ModelInfo,
  ModelMessage,
  PendingAttachment,
  RunState,
  SessionSummary,
} from './types';

const SESSION_STORAGE_KEY = 'blackwall.sessions.v1';
const MODEL_STORAGE_KEY = 'blackwall.model.v1';
const ENDPOINT_STORAGE_KEY = 'blackwall.endpoint.v1';
const MAX_SAVED_SESSIONS = 30;

function canUseStorage(): boolean {
  return typeof window !== 'undefined' && typeof window.localStorage !== 'undefined';
}

function storedSessions(): SessionSummary[] {
  if (!canUseStorage()) return [];

  try {
    const value = JSON.parse(window.localStorage.getItem(SESSION_STORAGE_KEY) ?? '[]') as unknown;
    if (!Array.isArray(value)) return [];
    return value
      .filter((session): session is SessionSummary => {
        if (!session || typeof session !== 'object') return false;
        const candidate = session as Partial<SessionSummary>;
        return (
          typeof candidate.id === 'string' &&
          typeof candidate.title === 'string' &&
          typeof candidate.updatedAt === 'number' &&
          Array.isArray(candidate.messages)
        );
      })
      .slice(0, MAX_SAVED_SESSIONS);
  } catch {
    return [];
  }
}

function persistedMessages(messages: ChatMessage[]): ChatMessage[] {
  return messages.map((message) => ({
    ...message,
    attachments: message.attachments.map((attachment) => ({
      id: attachment.id,
      name: attachment.name,
      mimeType: attachment.mimeType,
      sizeBytes: attachment.sizeBytes,
      kind: attachment.kind,
    })),
  }));
}

function writeSessions(sessions: SessionSummary[]): void {
  if (!canUseStorage()) return;
  try {
    window.localStorage.setItem(SESSION_STORAGE_KEY, JSON.stringify(sessions));
  } catch {
    // A full/disabled localStorage must never interrupt a model turn.
  }
}

function storedModel(): string {
  if (!canUseStorage()) return '';
  return window.localStorage.getItem(MODEL_STORAGE_KEY) ?? '';
}

function storedEndpoint(): string {
  if (!canUseStorage()) return '';
  return window.localStorage.getItem(ENDPOINT_STORAGE_KEY)?.trim() ?? '';
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === 'string') return error;
  if (error && typeof error === 'object' && 'message' in error) {
    const message = (error as { message?: unknown }).message;
    if (typeof message === 'string') return message;
  }
  return 'The configured model endpoint is unavailable.';
}

function sessionTitle(messages: ChatMessage[]): string {
  const firstUserMessage = messages.find((message) => message.role === 'user');
  if (!firstUserMessage) return 'New chat';
  const source = firstUserMessage.content.trim() || firstUserMessage.attachments[0]?.name || 'New chat';
  return source.length > 42 ? `${source.slice(0, 41).trimEnd()}…` : source;
}

function asModelMessages(messages: ChatMessage[]): ModelMessage[] {
  return messages
    .filter((message) => message.status !== 'error' && message.status !== 'sending')
    .map((message) => ({
      role: message.role,
      content: message.content,
      attachments: message.attachments.map((attachment) => ({
        id: attachment.id,
        name: attachment.name,
        mimeType: attachment.mimeType,
        sizeBytes: attachment.sizeBytes,
        kind: attachment.kind,
        dataUrl: attachment.dataUrl,
        textContent: attachment.textContent,
      })),
    }));
}

function releaseMessagePreviews(messages: ChatMessage[]): void {
  for (const message of messages) {
    for (const attachment of message.attachments) revokeAttachmentPreview(attachment);
  }
}

export function createChatController(client: LocalModelClient = localModelClient) {
  const initialSessionId = createId('session');
  const messages = writable<ChatMessage[]>([]);
  const sessions = writable<SessionSummary[]>(storedSessions());
  const activeSessionId = writable(initialSessionId);
  const runState = writable<RunState>('idle');
  const connectionState = writable<ConnectionState>('checking');
  const models = writable<ModelInfo[]>([]);
  const selectedModel = writable(storedModel() || import.meta.env.VITE_BLACKWALL_MODEL || '');
  const endpoint = writable(storedEndpoint());
  const notice = writable('');
  const connectionError = writable('');
  let activeRequest: AbortController | null = null;
  let connectionAttempt = 0;

  const contextPercent = derived(messages, ($messages) => {
    const characters = $messages.reduce((total, message) => total + message.content.length, 0);
    const estimatedTokens = Math.ceil(characters / 4);
    return Math.min(100, Math.round((estimatedTokens / 32_000) * 100));
  });

  function saveCurrentSession(nextMessages = get(messages)): void {
    if (nextMessages.length === 0) return;
    const session: SessionSummary = {
      id: get(activeSessionId),
      title: sessionTitle(nextMessages),
      updatedAt: Date.now(),
      messages: persistedMessages(nextMessages),
    };
    sessions.update((current) => {
      const next = [session, ...current.filter((item) => item.id !== session.id)].slice(
        0,
        MAX_SAVED_SESSIONS,
      );
      writeSessions(next);
      return next;
    });
  }

  async function initialize(): Promise<boolean> {
    const attempt = ++connectionAttempt;
    connectionState.set('checking');
    connectionError.set('');
    models.set([]);
    try {
      let configuredEndpoint = get(endpoint).trim();
      if (!configuredEndpoint) {
        configuredEndpoint = (await client.modelEndpoint()).trim();
        if (attempt !== connectionAttempt) return false;
        if (configuredEndpoint) endpoint.set(configuredEndpoint);
      }

      const catalog = await client.discoverModels(configuredEndpoint || undefined);
      if (attempt !== connectionAttempt) return false;
      models.set(catalog);
      if (catalog.length === 0) {
        connectionState.set('offline');
        const message = 'The endpoint responded, but it did not report any models.';
        connectionError.set(message);
        notice.set(message);
        return false;
      }

      const preferred = get(selectedModel);
      const choice = catalog.some((model) => model.id === preferred) ? preferred : catalog[0].id;
      selectedModel.set(choice);
      if (canUseStorage()) window.localStorage.setItem(MODEL_STORAGE_KEY, choice);
      connectionState.set('ready');
      connectionError.set('');
      notice.set('');
      return true;
    } catch (error) {
      if (attempt !== connectionAttempt) return false;
      const detail = errorMessage(error);
      connectionState.set('offline');
      connectionError.set(detail);
      notice.set(detail);
      return false;
    }
  }

  async function configureEndpoint(value: string): Promise<boolean> {
    const next = value.trim();
    if (!next) {
      const message = 'Enter the URL of an OpenAI-compatible model endpoint.';
      connectionError.set(message);
      notice.set(message);
      return false;
    }

    endpoint.set(next);
    if (canUseStorage()) window.localStorage.setItem(ENDPOINT_STORAGE_KEY, next);
    return initialize();
  }

  function chooseModel(modelId: string): void {
    if (!modelId) return;
    selectedModel.set(modelId);
    if (canUseStorage()) window.localStorage.setItem(MODEL_STORAGE_KEY, modelId);
  }

  async function send(text: string, pending: PendingAttachment[]): Promise<boolean> {
    const content = text.trim();
    if ((!content && pending.length === 0) || get(runState) !== 'idle') return false;
    if (!get(selectedModel)) {
      notice.set('Connect your model endpoint before sending a message.');
      return false;
    }

    runState.set('preparing');
    notice.set('');

    let attachmentPayloads;
    try {
      attachmentPayloads = await Promise.all(pending.map(materializeAttachment));
    } catch (error) {
      runState.set('error');
      notice.set(error instanceof Error ? error.message : 'One of the attachments could not be read.');
      window.setTimeout(() => runState.set('idle'), 0);
      return false;
    }

    const userMessage: ChatMessage = {
      id: createId('message'),
      role: 'user',
      content,
      attachments: attachmentPayloads.map((payload) => {
        const draft = pending.find((attachment) => attachment.id === payload.id);
        return { ...payload, previewUrl: draft?.previewUrl } satisfies Attachment;
      }),
      status: 'complete',
      createdAt: Date.now(),
    };
    const assistantId = createId('message');
    const assistantMessage: ChatMessage = {
      id: assistantId,
      role: 'assistant',
      content: '',
      attachments: [],
      status: 'streaming',
      createdAt: Date.now(),
    };
    const nextMessages = [...get(messages), userMessage, assistantMessage];
    messages.set(nextMessages);
    saveCurrentSession(nextMessages);
    runState.set('streaming');

    const controller = new AbortController();
    activeRequest = controller;
    const requestId = createId('request');

    void (async () => {
      try {
        await client.streamChat(
          {
            requestId,
            endpoint: get(endpoint) || undefined,
            model: get(selectedModel),
            messages: asModelMessages(nextMessages.filter((message) => message.id !== assistantId)),
          },
          {
            onDelta(delta) {
              messages.update((current) =>
                current.map((message) =>
                  message.id === assistantId
                    ? { ...message, content: message.content + delta, status: 'streaming' }
                    : message,
                ),
              );
            },
          },
          controller.signal,
        );

        messages.update((current) =>
          current.map((message) =>
            message.id === assistantId
              ? {
                  ...message,
                  content: message.content || 'The model completed without returning text.',
                  status: 'complete',
                }
              : message,
          ),
        );
        connectionState.set('ready');
      } catch (error) {
        const stopped = error instanceof DOMException && error.name === 'AbortError';
        messages.update((current) =>
          current.map((message) =>
            message.id === assistantId
              ? {
                  ...message,
                  content: message.content || (stopped ? 'Response stopped.' : ''),
                  status: stopped ? 'stopped' : 'error',
                  error: stopped
                    ? undefined
                    : error instanceof Error
                      ? error.message
                      : 'The model request failed.',
                }
              : message,
          ),
        );
        if (!stopped) {
          connectionState.set('offline');
          notice.set(error instanceof Error ? error.message : 'The model request failed.');
        }
      } finally {
        if (activeRequest === controller) activeRequest = null;
        runState.set('idle');
        saveCurrentSession();
      }
    })();

    return true;
  }

  function stop(): void {
    activeRequest?.abort();
  }

  function newChat(): void {
    stop();
    const current = get(messages);
    saveCurrentSession(current);
    releaseMessagePreviews(current);
    messages.set([]);
    activeSessionId.set(createId('session'));
    notice.set('');
    runState.set('idle');
  }

  function openSession(sessionId: string): void {
    if (sessionId === get(activeSessionId)) return;
    stop();
    releaseMessagePreviews(get(messages));
    const session = get(sessions).find((item) => item.id === sessionId);
    if (!session) return;
    messages.set(structuredClone(session.messages));
    activeSessionId.set(session.id);
    notice.set('');
    runState.set('idle');
  }

  function removeSession(sessionId: string): void {
    sessions.update((current) => {
      const next = current.filter((session) => session.id !== sessionId);
      writeSessions(next);
      return next;
    });
    if (sessionId === get(activeSessionId)) newChat();
  }

  function dismissNotice(): void {
    notice.set('');
  }

  function destroy(): void {
    stop();
    releaseMessagePreviews(get(messages));
  }

  return {
    messages,
    sessions,
    activeSessionId,
    runState,
    connectionState,
    models,
    selectedModel,
    endpoint,
    contextPercent,
    notice,
    connectionError,
    initialize,
    configureEndpoint,
    chooseModel,
    send,
    stop,
    newChat,
    openSession,
    removeSession,
    dismissNotice,
    destroy,
  };
}

export type ChatController = ReturnType<typeof createChatController>;
