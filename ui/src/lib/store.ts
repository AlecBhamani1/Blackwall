import { derived, get, writable } from 'svelte/store';
import { materializeAttachment, revokeAttachmentPreview } from './attachments';
import { createId } from './id';
import { normalizeEndpoint, connectionHelp } from './setup';
import { agentClient } from './agent';
import { persistence, type Persistence, type Preferences } from './persistence';
import { localModelClient, type LocalModelClient } from './ipc';
import { ModelRequestError, nativeModelError } from './modelError';
import type {
  Attachment,
  ChatMessage,
  ConnectionState,
  ModelInfo,
  ModelMessage,
  PendingAttachment,
  RunState,
  PendingApproval,
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

function persistedMessages(messages: ChatMessage[], retainContents = false): ChatMessage[] {
  return messages.map((message) => ({
    ...message,
    attachments: message.attachments.map((attachment) => ({
      id: attachment.id,
      name: attachment.name,
      mimeType: attachment.mimeType,
      sizeBytes: attachment.sizeBytes,
      kind: attachment.kind,
      ...(retainContents
        ? { dataUrl: attachment.dataUrl, textContent: attachment.textContent }
        : {}),
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
  try {
    return window.localStorage.getItem(MODEL_STORAGE_KEY) ?? '';
  } catch {
    return '';
  }
}

function storedEndpoint(): string {
  if (!canUseStorage()) return '';
  try {
    return window.localStorage.getItem(ENDPOINT_STORAGE_KEY)?.trim() ?? '';
  } catch {
    return '';
  }
}

function sessionTitle(messages: ChatMessage[]): string {
  const firstUserMessage = messages.find((message) => message.role === 'user');
  if (!firstUserMessage) return 'New chat';
  const source =
    firstUserMessage.content.trim() || firstUserMessage.attachments[0]?.name || 'New chat';
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

export function createChatController(
  client: LocalModelClient = localModelClient,
  storage: Persistence = persistence,
) {
  const native = storage.available();
  const initialSessionId = createId('session');
  const messages = writable<ChatMessage[]>([]);
  const sessions = writable<SessionSummary[]>(storedSessions());
  const activeSessionId = writable(initialSessionId);
  const runState = writable<RunState>('idle');
  const agentMode = writable(false);
  const webEnabled = writable(false);
  const workspace = writable('');
  const approval = writable<PendingApproval | null>(null);
  async function chooseWorkspace() {
    if (get(runState) !== 'idle') return;
    try {
      const selected = await agentClient.chooseWorkspace();
      if (selected) {
        workspace.set(selected);
        agentMode.set(true);
      }
    } catch {
      notice.set('The project folder could not be opened. Try choosing it again.');
    }
  }
  async function resolveApproval(decision: 'allow' | 'always_allow' | 'deny') {
    const pending = get(approval);
    if (!pending) return;
    await agentClient.resolve(pending, decision);
    if (get(approval)?.approvalId === pending.approvalId) approval.set(null);
  }
  async function exportConversation() {
    if (!get(messages).length) return;
    const session = {
      id: get(activeSessionId),
      title: sessionTitle(get(messages)),
      updatedAt: Date.now(),
      messages: persistedMessages(get(messages), true),
    };
    try {
      if (native) {
        await agentClient.export(session);
      } else {
        const url = URL.createObjectURL(
          new Blob([JSON.stringify(session, null, 2)], { type: 'application/json' }),
        );
        const anchor = document.createElement('a');
        anchor.href = url;
        anchor.download = 'Blackwall conversation.json';
        anchor.click();
        window.setTimeout(() => URL.revokeObjectURL(url), 1000);
      }
    } catch {
      persistenceError.set(
        'The conversation could not be exported. Check the destination folder and available space.',
      );
    }
  }
  const connectionState = writable<ConnectionState>('checking');
  const models = writable<ModelInfo[]>([]);
  const selectedModel = writable(storedModel() || import.meta.env.VITE_BLACKWALL_MODEL || '');
  const endpoint = writable(storedEndpoint());
  const notice = writable('');
  const persistenceError = writable('');
  const preferences = writable<Preferences>({ contextWindow: 32000, memoryEnabled: false });
  let storageReady = !native;
  let storageInitialization: Promise<void> | null = null;
  let saves = Promise.resolve();
  let sessionNavigation = 0;
  const pendingWrites: Array<() => Promise<void>> = [];
  let drainingWrites = false;
  let writeFailed = false;
  async function drainWrites() {
    drainingWrites = true;
    try {
      while (pendingWrites.length) {
        await pendingWrites[0]();
        pendingWrites.shift();
      }
      writeFailed = false;
      persistenceError.set('');
    } catch {
      writeFailed = true;
      persistenceError.set(
        'Your latest changes could not be saved. Check disk space and folder permissions, then retry. Keep Blackwall open to retry saving or export this conversation.',
      );
    } finally {
      drainingWrites = false;
    }
  }
  function queueWrite(operation: () => Promise<void>) {
    pendingWrites.push(operation);
    if (!drainingWrites && !writeFailed) saves = drainWrites();
  }
  async function retryPersistence() {
    if (!storageReady) {
      storageInitialization = null;
      await initializeStorage();
    } else if (!drainingWrites) {
      saves = drainWrites();
      await saves;
    }
  }
  async function initializeStorage() {
    if (!native || storageReady) return;
    if (storageInitialization) return storageInitialization;
    storageInitialization = (async () => {
      try {
        await storage.migrate(storedSessions());
        const [savedSessions, savedPreferences] = await Promise.all([
          storage.list(),
          storage.preferences(),
        ]);
        if (destroyed) return;
        sessions.set(savedSessions);
        preferences.set({ contextWindow: 32000, memoryEnabled: false, ...savedPreferences });
        if (savedPreferences.endpoint) endpoint.set(savedPreferences.endpoint);
        if (savedPreferences.model) selectedModel.set(savedPreferences.model);
        storageReady = true;
        persistenceError.set('');
      } catch {
        persistenceError.set(
          'Your saved conversations could not be opened. Existing data has been left in place. Restart Blackwall after checking disk space and folder permissions.',
        );
      }
    })();
    await storageInitialization;
  }
  function savePreferences(next: Preferences) {
    preferences.update((current) => {
      const merged = { ...current, ...next };
      if (native && storageReady) queueWrite(() => storage.savePreferences(next));
      return merged;
    });
  }
  const connectionError = writable('');
  let activeRequest: AbortController | null = null;
  let connectionAttempt = 0;
  let runGeneration = 0;
  let destroyed = false;

  const contextPercent = derived([messages, preferences], ([$messages, $preferences]) => {
    const characters = $messages.reduce((total, message) => total + message.content.length, 0);
    const estimatedTokens = Math.ceil(characters / 4);
    return Math.min(
      100,
      Math.round((estimatedTokens / ($preferences.contextWindow ?? 32000)) * 100),
    );
  });

  function saveCurrentSession(nextMessages = get(messages)): void {
    if (nextMessages.length === 0) return;
    const session: SessionSummary = {
      id: get(activeSessionId),
      title: sessionTitle(nextMessages),
      updatedAt: Date.now(),
      messages: persistedMessages(nextMessages, native),
    };
    sessions.update((current) => {
      const next = [session, ...current.filter((item) => item.id !== session.id)].slice(
        0,
        native ? 500 : MAX_SAVED_SESSIONS,
      );
      if (native) {
        if (storageReady) queueWrite(() => storage.save(session));
      } else writeSessions(next);
      return next;
    });
  }

  async function initialize(candidate?: string): Promise<boolean> {
    await initializeStorage();
    const previousModels = get(models);
    // A cached catalog survives disconnects. Only preserve a currently working,
    // different connection when trying an alternative server fails.
    const preserveConnection =
      get(connectionState) === 'ready' &&
      previousModels.length > 0 &&
      !!candidate &&
      !!get(endpoint) &&
      normalizeEndpoint(candidate) !== normalizeEndpoint(get(endpoint));
    const attempt = ++connectionAttempt;
    connectionState.set('checking');
    connectionError.set('');
    if (!candidate) models.set([]);
    try {
      let configuredEndpoint = candidate ?? get(endpoint).trim();
      if (!configuredEndpoint) {
        configuredEndpoint = (await client.modelEndpoint()).trim();
        if (destroyed || attempt !== connectionAttempt) return false;
        if (configuredEndpoint && !candidate) endpoint.set(configuredEndpoint);
      }

      const catalog = await client.discoverModels(configuredEndpoint || undefined);
      if (destroyed || attempt !== connectionAttempt) return false;
      if (catalog.length === 0) {
        throw new Error('The service did not report any models.');
      }

      models.set(catalog);
      const profileModel = candidate
        ? get(preferences).connections?.find(
            (profile) => profile.endpoint === normalizeEndpoint(candidate),
          )?.model
        : undefined;
      const preferred = profileModel ?? get(selectedModel);
      const choice = catalog.some((model) => model.id === preferred) ? preferred : catalog[0].id;
      selectedModel.set(choice);
      if (candidate) {
        endpoint.set(candidate);
        savePreferences({ endpoint: candidate, model: choice });
      }
      try {
        if (canUseStorage()) {
          window.localStorage.setItem(MODEL_STORAGE_KEY, choice);
          if (candidate) window.localStorage.setItem(ENDPOINT_STORAGE_KEY, candidate);
        }
      } catch {
        /* A working connection remains usable without browser storage. */
      }
      connectionState.set('ready');
      connectionError.set('');
      notice.set('');
      return true;
    } catch (error) {
      if (destroyed || attempt !== connectionAttempt) return false;
      const detail = connectionHelp(error);
      connectionState.set(preserveConnection ? 'ready' : 'offline');
      if (preserveConnection) models.set(previousModels);
      connectionError.set(detail);
      notice.set(detail);
      return false;
    }
  }

  async function configureEndpoint(value: string, name?: string): Promise<boolean> {
    const next = value.trim();
    if (!next) {
      const message = 'Enter the URL of an OpenAI-compatible model endpoint.';
      connectionError.set(message);
      notice.set(message);
      return false;
    }

    if (get(runState) !== 'idle') {
      notice.set('Stop the current response before changing connections.');
      return false;
    }
    try {
      normalizeEndpoint(next);
    } catch (cause) {
      connectionError.set(connectionHelp(cause));
      return false;
    }
    const connected = await initialize(next);
    if (connected && name?.trim()) {
      const normalized = normalizeEndpoint(next);
      const connections = get(preferences).connections ?? [];
      savePreferences({
        connections: [
          { endpoint: normalized, name: name.trim().slice(0, 80), model: get(selectedModel) },
          ...connections.filter((profile) => profile.endpoint !== normalized),
        ].slice(0, 20),
      });
    }
    return connected;
  }

  function chooseModel(modelId: string): void {
    if (!modelId || !get(models).some((model) => model.id === modelId)) return;
    selectedModel.set(modelId);
    const address = get(endpoint);
    savePreferences({
      model: modelId,
      connections: (get(preferences).connections ?? []).map((profile) =>
        address && profile.endpoint === normalizeEndpoint(address)
          ? { ...profile, model: modelId }
          : profile,
      ),
    });
    try {
      if (canUseStorage()) window.localStorage.setItem(MODEL_STORAGE_KEY, modelId);
    } catch {
      /* Keep this session usable. */
    }
  }

  async function send(text: string, pending: PendingAttachment[]): Promise<boolean> {
    const content = text.trim();
    if ((!content && pending.length === 0) || get(runState) !== 'idle') return false;
    if (writeFailed) {
      notice.set('Retry saving or export this conversation before continuing.');
      return false;
    }
    if (!storageReady) {
      persistenceError.set(
        'Your conversation store is unavailable. Restart Blackwall after checking disk space and folder permissions.',
      );
      return false;
    }
    if (!get(selectedModel) || get(connectionState) !== 'ready') {
      notice.set('Connect your model endpoint before sending a message.');
      return false;
    }

    const generation = ++runGeneration;
    approval.set(null);
    runState.set('preparing');
    notice.set('');

    let attachmentPayloads;
    try {
      attachmentPayloads = await Promise.all(pending.map(materializeAttachment));
    } catch (error) {
      if (generation !== runGeneration || destroyed) return false;
      runState.set('error');
      notice.set(
        error instanceof Error ? error.message : 'One of the attachments could not be read.',
      );
      window.setTimeout(() => {
        if (generation === runGeneration) runState.set('idle');
      }, 0);
      return false;
    }

    if (generation !== runGeneration || destroyed) return false;
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
            agentMode: get(agentMode),
            webEnabled: get(webEnabled),
            endpoint: get(endpoint) || undefined,
            model: get(selectedModel),
            messages: asModelMessages(nextMessages.filter((message) => message.id !== assistantId)),
          },
          {
            onEvent(event) {
              if (generation !== runGeneration || controller.signal.aborted || destroyed) return;
              if (event.type === 'subagent_status')
                messages.update((current) =>
                  current.map((message) =>
                    message.id === assistantId
                      ? {
                          ...message,
                          children: [
                            ...(message.children ?? []).filter(
                              (child) => child.id !== event.agentId,
                            ),
                            { id: event.agentId, state: event.state, summary: event.summary ?? '' },
                          ],
                        }
                      : message,
                  ),
                );
              if (event.type === 'approval_request') approval.set(event);
              if (event.type === 'tool_call')
                messages.update((current) =>
                  current.map((message) =>
                    message.id === assistantId
                      ? {
                          ...message,
                          tools: [
                            ...(message.tools ?? []),
                            {
                              id: event.toolCallId,
                              name: event.name,
                              arguments: event.arguments,
                              status: 'running',
                            },
                          ],
                        }
                      : message,
                  ),
                );
              if (event.type === 'tool_result')
                messages.update((current) =>
                  current.map((message) =>
                    message.id === assistantId
                      ? {
                          ...message,
                          tools: message.tools?.map((tool) =>
                            tool.id === event.toolCallId
                              ? {
                                  ...tool,
                                  output: event.output,
                                  status: event.success ? 'complete' : 'error',
                                }
                              : tool,
                          ),
                        }
                      : message,
                  ),
                );
            },
            onDelta(delta) {
              if (generation !== runGeneration || controller.signal.aborted || destroyed) return;
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

        if (generation !== runGeneration || destroyed) return;
        if (
          !get(messages)
            .find((message) => message.id === assistantId)
            ?.content.trim()
        ) {
          throw nativeModelError({ code: 'empty_model_response' });
        }
        messages.update((current) =>
          current.map((message) =>
            message.id === assistantId
              ? {
                  ...message,
                  status: 'complete',
                }
              : message,
          ),
        );
        connectionState.set('ready');
      } catch (error) {
        if (generation !== runGeneration || destroyed) return;
        const stopped = error instanceof DOMException && error.name === 'AbortError';
        messages.update((current) =>
          current.map((message) =>
            message.id === assistantId
              ? {
                  ...message,
                  content: message.content || (stopped ? 'Response stopped.' : ''),
                  status: stopped ? 'stopped' : 'error',
                  tools: message.tools?.map((tool) =>
                    tool.status === 'running'
                      ? { ...tool, status: stopped ? 'stopped' : 'error' }
                      : tool,
                  ),
                  children: message.children?.map((child) =>
                    child.state === 'running'
                      ? { ...child, state: 'interrupted', summary: 'The parent task ended.' }
                      : child,
                  ),
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
          notice.set(error instanceof Error ? error.message : 'The model request failed.');
          if (error instanceof ModelRequestError && error.connectionLost) {
            connectionState.set('offline');
            connectionError.set(error.message);
          }
        }
      } finally {
        if (generation === runGeneration && !destroyed) {
          if (activeRequest === controller) activeRequest = null;
          approval.set(null);
          runState.set('idle');
          saveCurrentSession();
        }
      }
    })();

    return true;
  }

  function stop(): void {
    runGeneration += 1;
    approval.set(null);
    activeRequest?.abort();
    activeRequest = null;
    messages.update((current) =>
      current.map((message) =>
        message.status === 'streaming'
          ? {
              ...message,
              status: 'stopped',
              children: message.children?.map((child) =>
                child.state === 'running'
                  ? { ...child, state: 'interrupted', summary: 'Stopped with the parent task.' }
                  : child,
              ),
              content: message.content || 'Response stopped.',
              tools: message.tools?.map((tool) =>
                tool.status === 'running' ? { ...tool, status: 'stopped' } : tool,
              ),
            }
          : message,
      ),
    );
    runState.set('idle');
    saveCurrentSession();
  }

  async function newChat(): Promise<boolean> {
    const navigation = ++sessionNavigation;
    stop();
    const generation = runGeneration;
    if (native) {
      await saves;
      if (navigation !== sessionNavigation || generation !== runGeneration || destroyed)
        return false;
      if (pendingWrites.length) {
        persistenceError.set(
          'Your conversation has not been saved. Retry saving before starting a new chat. Keep Blackwall open to retry or export this conversation.',
        );
        return false;
      }
    }
    releaseMessagePreviews(get(messages));
    messages.set([]);
    activeSessionId.set(createId('session'));
    notice.set('');
    runState.set('idle');
    return true;
  }

  async function openSession(sessionId: string): Promise<void> {
    if (sessionId === get(activeSessionId)) return;
    stop();
    const navigation = ++sessionNavigation;
    let session = get(sessions).find((item) => item.id === sessionId);
    if (!session) return;
    if (native) {
      try {
        await saves;
        if (pendingWrites.length) {
          persistenceError.set('Retry saving your changes before switching conversations.');
          return;
        }
        session = (await storage.load(sessionId)) ?? undefined;
      } catch {
        persistenceError.set(
          'This conversation could not be opened. Its saved copy has been left in place.',
        );
        return;
      }
      if (navigation !== sessionNavigation || destroyed || !session) return;
    }
    releaseMessagePreviews(get(messages));
    messages.set(
      structuredClone(session.messages).map((message) => ({
        ...message,
        status: message.status === 'streaming' ? 'stopped' : message.status,
        tools: message.tools?.map((tool) =>
          tool.status === 'running' ? { ...tool, status: 'stopped' } : tool,
        ),
        children: message.children?.map((child) =>
          child.state === 'running'
            ? {
                ...child,
                state: 'interrupted',
                summary: 'Interrupted before this conversation was saved.',
              }
            : child,
        ),
        attachments: message.attachments.map((attachment) => ({
          ...attachment,
          previewUrl: attachment.kind === 'image' ? attachment.dataUrl : undefined,
        })),
      })),
    );
    activeSessionId.set(session.id);
    notice.set('');
    runState.set('idle');
  }

  function removeSession(sessionId: string): void {
    sessionNavigation += 1;
    if (sessionId === get(activeSessionId)) {
      stop();
      releaseMessagePreviews(get(messages));
      messages.set([]);
      activeSessionId.set(createId('session'));
      notice.set('');
    }
    sessions.update((current) => {
      const next = current.filter((session) => session.id !== sessionId);
      if (native) {
        if (storageReady) queueWrite(() => storage.remove(sessionId));
      } else writeSessions(next);
      return next;
    });
  }

  function dismissNotice(): void {
    notice.set('');
  }

  async function suspend(): Promise<void> {
    stop();
    connectionAttempt += 1;
    sessionNavigation += 1;
    await saves;
    if (pendingWrites.length)
      throw new Error('Retry saving or export your conversation before locking Blackwall.');
    releaseMessagePreviews(get(messages));
    messages.set([]);
    sessions.set([]);
    approval.set(null);
    workspace.set('');
    agentMode.set(false);
    webEnabled.set(false);
    activeSessionId.set(createId('session'));
    models.set([]);
    connectionState.set('checking');
    storageReady = !native;
    storageInitialization = null;
  }

  function destroy(): void {
    stop();
    destroyed = true;
    connectionAttempt += 1;
    releaseMessagePreviews(get(messages));
  }

  return {
    messages,
    sessions,
    activeSessionId,
    runState,
    agentMode,
    webEnabled,
    workspace,
    approval,
    chooseWorkspace,
    resolveApproval,
    exportConversation,
    connectionState,
    models,
    selectedModel,
    endpoint,
    contextPercent,
    notice,
    persistenceError,
    preferences,
    savePreferences,
    connectionError,
    initialize,
    configureEndpoint,
    forgetConnection: (address: string) =>
      savePreferences({
        connections: (get(preferences).connections ?? []).filter(
          (profile) => profile.endpoint !== address,
        ),
      }),
    chooseModel,
    send,
    stop,
    newChat,
    openSession,
    removeSession,
    dismissNotice,
    destroy,
    suspend,
    retryPersistence,
  };
}

export type ChatController = ReturnType<typeof createChatController>;
