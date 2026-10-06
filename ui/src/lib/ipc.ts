import {
  contextBudget,
  checkBudget,
  contextReport,
  restoreContext,
  effectiveContext,
  compactContext,
  fingerprint,
} from './context';
import { invoke } from '@tauri-apps/api/core';
import { normalizeEndpoint } from './setup';
import { nativeModelError } from './modelError';
import type {
  AgentEvent,
  AttachmentPayload,
  ChatRequest,
  ModelInfo,
  ModelMessage,
  StreamCallbacks,
  ShareStatus,
  StartShareRequest,
  TokenUsage,
  ContextReport,
  ContextState,
} from './types';

const MODEL_DISCOVERY_TIMEOUT_MS = 4_000;

interface OllamaTag {
  name?: string;
  model?: string;
  size?: number;
}

interface OllamaTagsResponse {
  models?: OllamaTag[];
}

interface OpenAIModel {
  id?: string;
  owned_by?: string;
}

interface OpenAIModelsResponse {
  data?: OpenAIModel[];
}

function isTauriRuntime(): boolean {
  return typeof window !== 'undefined' && Boolean(window.__TAURI_INTERNALS__);
}

function sharingUnavailable(): Error {
  return new Error('Guest sharing requires the Blackwall desktop app.');
}

function normalizeShareStatus(value: unknown): ShareStatus {
  if (!value || typeof value !== 'object') {
    throw new Error('Blackwall returned an invalid sharing status.');
  }

  const record = value as Record<string, unknown>;
  const optionalText = (field: unknown): string | undefined =>
    typeof field === 'string' && field.length > 0 ? field : undefined;
  return {
    active: Boolean(record.active),
    id: optionalText(record.id),
    name: optionalText(record.name),
    shareUrl: optionalText(record.shareUrl),
    qrDataUrl: optionalText(record.qrDataUrl),
    model: optionalText(record.model),
    expiresAt: typeof record.expiresAt === 'number' ? record.expiresAt : undefined,
    networkLabel: optionalText(record.networkLabel),
    requestCount: typeof record.requestCount === 'number' ? record.requestCount : undefined,
  };
}

async function fetchWithTimeout(input: RequestInfo | URL, timeoutMs: number): Promise<Response> {
  const controller = new AbortController();
  const timeout = window.setTimeout(() => controller.abort(), timeoutMs);

  try {
    return await fetch(input, { signal: controller.signal });
  } finally {
    window.clearTimeout(timeout);
  }
}

function normalizeCatalog(value: unknown): ModelInfo[] {
  if (Array.isArray(value)) {
    return value
      .map((model) => {
        if (typeof model === 'string') return { id: model, name: model };
        if (!model || typeof model !== 'object') return null;
        const record = model as Record<string, unknown>;
        const id = String(record.id ?? record.name ?? '');
        if (!id) return null;
        return {
          id,
          name: String(record.name ?? id),
          sizeBytes: typeof record.sizeBytes === 'number' ? record.sizeBytes : undefined,
          ownedBy: typeof record.ownedBy === 'string' ? record.ownedBy : undefined,
        };
      })
      .filter((model): model is ModelInfo => model !== null);
  }

  if (value && typeof value === 'object') {
    const record = value as Record<string, unknown>;
    return normalizeCatalog(record.models ?? record.data ?? []);
  }

  return [];
}

function browserModelRoutes(endpoint?: string): { models: string; tags?: string; chat: string } {
  if (!endpoint?.trim()) {
    return {
      models: '/local-llm/v1/models',
      tags: '/local-llm/api/tags',
      chat: '/local-llm/v1/chat/completions',
    };
  }

  const normalized = normalizeEndpoint(endpoint);
  const url = new URL(normalized);
  const origin = url.origin;
  return {
    models: `${normalized}/models`,
    tags: url.pathname === '/v1' ? `${origin}/api/tags` : undefined,
    chat: `${normalized}/chat/completions`,
  };
}

async function discoverBrowserModels(endpoint?: string): Promise<ModelInfo[]> {
  const routes = browserModelRoutes(endpoint);
  try {
    if (!routes.tags) throw new Error('This endpoint does not expose the Ollama tags route.');
    const tagsResponse = await fetchWithTimeout(routes.tags, MODEL_DISCOVERY_TIMEOUT_MS);
    if (tagsResponse.ok) {
      const payload = (await tagsResponse.json()) as OllamaTagsResponse;
      const models = (payload.models ?? [])
        .map((model): ModelInfo | null => {
          const id = model.model || model.name;
          return id ? { id, name: id, sizeBytes: model.size } : null;
        })
        .filter((model): model is ModelInfo => model !== null)
        .sort(
          (a, b) =>
            (a.sizeBytes ?? Number.MAX_SAFE_INTEGER) - (b.sizeBytes ?? Number.MAX_SAFE_INTEGER),
        );

      if (models.length > 0) return models;
    }
  } catch {
    // Generic OpenAI-compatible endpoints do not expose Ollama's /api/tags route.
  }

  const response = await fetchWithTimeout(routes.models, MODEL_DISCOVERY_TIMEOUT_MS);
  if (!response.ok) {
    throw new Error(`Model endpoint returned ${response.status}.`);
  }

  const payload = (await response.json()) as OpenAIModelsResponse;
  return (payload.data ?? [])
    .filter((model): model is Required<Pick<OpenAIModel, 'id'>> & OpenAIModel => Boolean(model.id))
    .map((model) => ({ id: model.id, name: model.id, ownedBy: model.owned_by }));
}

function attachmentContext(attachments: AttachmentPayload[]): string {
  return attachments
    .filter((attachment) => attachment.kind !== 'image')
    .map((attachment) => {
      if (attachment.textContent !== undefined) {
        return [
          `<attachment name="${attachment.name}" type="${attachment.mimeType}">`,
          attachment.textContent,
          '</attachment>',
        ].join('\n');
      }

      return `[Attached file: ${attachment.name} (${attachment.mimeType}). Binary contents are unavailable to this model.]`;
    })
    .join('\n\n');
}

function toOpenAIMessage(message: ModelMessage): Record<string, unknown> {
  const attachments = message.attachments ?? [];
  if (attachments.length === 0) {
    return { role: message.role, content: message.content };
  }

  const context = attachmentContext(attachments);
  const text = [message.content, context].filter(Boolean).join('\n\n');
  const imageParts = attachments
    .filter((attachment) => attachment.kind === 'image' && attachment.dataUrl)
    .map((attachment) => ({
      type: 'image_url',
      image_url: { url: attachment.dataUrl },
    }));

  if (imageParts.length === 0) {
    return { role: message.role, content: text };
  }

  return {
    role: message.role,
    content: [{ type: 'text', text: text || 'Describe the attached image.' }, ...imageParts],
  };
}

function deltaText(value: unknown): string {
  if (typeof value === 'string') return value;
  if (!Array.isArray(value)) return '';
  return value
    .map((part) => {
      if (typeof part === 'string') return part;
      if (!part || typeof part !== 'object') return '';
      const record = part as Record<string, unknown>;
      return typeof record.text === 'string' ? record.text : '';
    })
    .join('');
}

function parseSseBlock(
  block: string,
  callbacks: StreamCallbacks,
  requestId: string,
  markFinished: () => void,
): boolean {
  for (const line of block.split(/\r?\n/)) {
    if (!line.startsWith('data:')) continue;
    const data = line.slice(5).trim();
    if (!data) continue;
    if (data === '[DONE]') return true;

    const payload = JSON.parse(data) as {
      usage?: { prompt_tokens: number; completion_tokens: number; total_tokens: number };
      choices?: Array<{
        delta?: { content?: unknown };
        message?: { content?: unknown };
        finish_reason?: string;
      }>;
    };
    if (payload.usage)
      callbacks.onEvent?.({
        type: 'turn_complete',
        requestId,
        usage: {
          promptTokens: payload.usage.prompt_tokens,
          completionTokens: payload.usage.completion_tokens,
          totalTokens: payload.usage.total_tokens,
        },
      });
    const choice = payload.choices?.[0];
    if (choice?.finish_reason) markFinished();
    const delta = deltaText(choice?.delta?.content ?? choice?.message?.content);
    if (delta) callbacks.onDelta(delta);
  }

  return false;
}

async function streamBrowserMessages(
  messages: Record<string, unknown>[],
  request: ChatRequest,
  callbacks: StreamCallbacks,
  signal: AbortSignal,
): Promise<void> {
  const response = await fetch(browserModelRoutes(request.endpoint).chat, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      model: request.model,
      messages,
      max_tokens: contextBudget(request).outputTokens,
      stream_options: { include_usage: true },
      stream: true,
    }),
    signal,
  });

  if (!response.ok) {
    let detail = '';
    try {
      const payload = (await response.json()) as { error?: { message?: string } | string };
      detail = typeof payload.error === 'string' ? payload.error : (payload.error?.message ?? '');
    } catch {
      detail = await response.text().catch(() => '');
    }
    throw new Error(detail || `Model endpoint returned ${response.status}.`);
  }

  if (!response.body) throw new Error('Model endpoint returned an empty stream.');

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  let finished = false;
  let finishedResponse = false;
  let received = 0;
  try {
    while (!finished) {
      const { value, done } = await reader.read();
      if (done) break;
      received += value.length;
      if (received > 8 * 1024 * 1024) throw new Error('The model response exceeds its size limit.');
      buffer += decoder.decode(value, { stream: true });

      const blocks = buffer.split(/\r?\n\r?\n/);
      buffer = blocks.pop() ?? '';
      if (new TextEncoder().encode(buffer).length > 1024 * 1024)
        throw new Error('The model event exceeds its size limit.');
      for (const block of blocks) {
        if (
          parseSseBlock(block, callbacks, request.requestId, () => {
            finishedResponse = true;
          })
        ) {
          finished = true;
          break;
        }
      }
    }

    if (buffer.trim() && !finished)
      finished = parseSseBlock(buffer, callbacks, request.requestId, () => {
        finishedResponse = true;
      });
    if (!finished && !finishedResponse)
      throw new Error('The model returned an incomplete response. Previous context retained.');
    callbacks.onComplete?.();
  } finally {
    await reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}

async function streamBrowserChat(
  request: ChatRequest,
  callbacks: StreamCallbacks,
  signal: AbortSignal,
): Promise<void> {
  const budget = contextBudget(request);
  const source = request.messages.map(toOpenAIMessage);
  let state = await restoreContext(source, request.contextState);
  let messages = effectiveContext(state);
  if (request.compact || (budget.autoCompact && estimateForThreshold(messages, budget))) {
    try {
      state = await compactContext(state, budget, async (chunk) => {
        let summary = '';
        await streamBrowserMessages(
          chunk,
          request,
          {
            onDelta(delta) {
              summary += delta;
            },
          },
          signal,
        );
        if (signal.aborted) throw new DOMException('Compaction stopped.', 'AbortError');
        return summary;
      });
      messages = effectiveContext(state);
    } catch (error) {
      if (
        request.compact ||
        !(error instanceof Error) ||
        !error.message.startsWith('There is no older')
      )
        throw error;
    }
  }
  checkBudget(messages, budget);
  const emit = (event: AgentEvent) => {
    if (!signal.aborted) callbacks.onEvent?.(event);
  };
  if (signal.aborted) throw new DOMException('Response stopped.', 'AbortError');
  emit({
    type: 'context_report',
    requestId: request.requestId,
    report: contextReport(messages, budget, state.serverUsage),
  });
  if (request.compact) {
    emit({ type: 'context_updated', requestId: request.requestId, state });
    callbacks.onComplete?.();
    return;
  }
  let answer = '';
  let usage: TokenUsage | undefined;
  await streamBrowserMessages(
    messages,
    request,
    {
      onDelta(delta) {
        answer += delta;
        callbacks.onDelta(delta);
      },
      onEvent(event) {
        if (event.type === 'turn_complete') usage = event.usage;
      },
    },
    signal,
  );
  if (signal.aborted) throw new DOMException('Response stopped.', 'AbortError');
  state.transcript.push({ role: 'assistant', content: answer });
  state.coveredMessages = source.length + 1;
  state.sourceHash = await fingerprint([...source, { role: 'assistant', content: answer }]);
  state.serverUsage = usage;
  emit({
    type: 'context_report',
    requestId: request.requestId,
    report: contextReport(messages, budget, usage),
  });
  emit({ type: 'context_updated', requestId: request.requestId, state });
  emit({ type: 'turn_complete', requestId: request.requestId, usage });
  callbacks.onComplete?.();
}
function estimateForThreshold(
  messages: Record<string, unknown>[],
  budget: ReturnType<typeof contextBudget>,
): boolean {
  return (
    contextReport(messages, budget).estimatedPromptTokens >
    (budget.contextWindow - budget.outputTokens - Math.floor(budget.contextWindow / 20)) * 0.9
  );
}

function normalizeEvent(payload: unknown): AgentEvent | null {
  if (!payload || typeof payload !== 'object') return null;
  const event = payload as Record<string, unknown>;
  const type = String(event.type ?? event.event ?? '');
  const requestId = String(event.requestId ?? event.request_id ?? '');

  if (
    type === 'instructions_loaded' &&
    Array.isArray(event.sources) &&
    event.sources.every((source) => typeof source === 'string') &&
    Array.isArray(event.warnings) &&
    event.warnings.every((warning) => typeof warning === 'string')
  ) {
    return {
      type,
      requestId,
      ...(typeof event.agentId === 'string' ? { agentId: event.agentId } : {}),
      sources: event.sources,
      warnings: event.warnings,
    };
  }
  if (type === 'context_report' && event.report && typeof event.report === 'object')
    return { type, requestId, report: event.report as ContextReport };
  if (type === 'context_updated' && event.state && typeof event.state === 'object')
    return { type, requestId, state: event.state as ContextState };
  if (type === 'assistant_delta') {
    return { type, requestId, delta: String(event.delta ?? event.content ?? '') };
  }
  if (type === 'turn_complete') {
    return {
      type,
      requestId,
      usage: event.usage as TokenUsage | undefined,
      finishReason:
        typeof (event.finishReason ?? event.finish_reason) === 'string'
          ? String(event.finishReason ?? event.finish_reason)
          : undefined,
    };
  }
  if (type === 'error') {
    return {
      type,
      requestId,
      message: String(event.message ?? 'The model request failed.'),
      code: typeof event.code === 'string' ? event.code : undefined,
    };
  }
  if (
    type === 'subagent_status' &&
    typeof event.agentId === 'string' &&
    typeof event.state === 'string'
  )
    return {
      type,
      requestId,
      agentId: event.agentId,
      state: event.state,
      summary: typeof event.summary === 'string' ? event.summary : undefined,
    };
  if (
    type === 'tool_call' &&
    typeof event.toolCallId === 'string' &&
    typeof event.name === 'string' &&
    typeof event.arguments === 'string'
  )
    return {
      type,
      requestId,
      toolCallId: event.toolCallId,
      name: event.name,
      arguments: event.arguments,
    };
  if (
    type === 'tool_result' &&
    typeof event.toolCallId === 'string' &&
    typeof event.output === 'string'
  )
    return {
      type,
      requestId,
      toolCallId: event.toolCallId,
      output: event.output,
      success: event.success === true,
    };
  if (
    type === 'approval_request' &&
    typeof event.approvalId === 'string' &&
    typeof event.detail === 'string' &&
    typeof event.kind === 'string'
  )
    return {
      type,
      requestId,
      approvalId: event.approvalId,
      detail: event.detail,
      kind: event.kind,
    };
  return null;
}

async function streamTauriChat(
  request: ChatRequest,
  callbacks: StreamCallbacks,
  signal: AbortSignal,
): Promise<void> {
  const { listen } = await import('@tauri-apps/api/event');

  let remoteError: Error | undefined;
  let complete = false;
  const unlisten = await listen<unknown>('blackwall://event', ({ payload }) => {
    const event = normalizeEvent(payload);
    if (!event || event.requestId !== request.requestId || signal.aborted) return;

    callbacks.onEvent?.(event);
    if (event.type === 'assistant_delta') callbacks.onDelta(event.delta);
    if (event.type === 'turn_complete') {
      complete = true;
      callbacks.onComplete?.();
    }
    if (event.type === 'error') remoteError = nativeModelError(event);
  });

  const cancel = () => {
    void invoke('cancel_request', { requestId: request.requestId }).catch(() => {});
  };
  signal.addEventListener('abort', cancel, { once: true });
  try {
    if (signal.aborted) throw new DOMException('The request was stopped.', 'AbortError');
    await invoke('stream_chat', {
      request,
      options: {
        enabled: request.agentMode ?? false,
        workspace: request.workspace ?? null,
        webEnabled: request.webEnabled ?? false,
        sessionId: request.sessionId ?? null,
        sourceMessageId: request.sourceMessageId ?? null,
      },
    });
    if (signal.aborted) throw new DOMException('The request was stopped.', 'AbortError');
    if (remoteError) throw remoteError;
    if (!complete) callbacks.onComplete?.();
  } catch (cause) {
    if (signal.aborted) throw new DOMException('The request was stopped.', 'AbortError');
    throw remoteError ?? nativeModelError(cause);
  } finally {
    signal.removeEventListener('abort', cancel);
    unlisten();
  }
}

export const localModelClient = {
  async modelEndpoint(): Promise<string> {
    if (!isTauriRuntime()) return '';
    return invoke<string>('model_endpoint');
  },

  async discoverModels(endpoint?: string): Promise<ModelInfo[]> {
    if (!isTauriRuntime()) return discoverBrowserModels(endpoint);
    return normalizeCatalog(
      await invoke<unknown>('discover_models', { endpoint: endpoint || null }),
    );
  },

  async streamChat(
    request: ChatRequest,
    callbacks: StreamCallbacks,
    signal: AbortSignal,
  ): Promise<void> {
    if (isTauriRuntime()) return streamTauriChat(request, callbacks, signal);
    return streamBrowserChat(request, callbacks, signal);
  },
};

export type LocalModelClient = typeof localModelClient;

export const guestShareClient = {
  async startShare(request: StartShareRequest): Promise<ShareStatus> {
    if (!isTauriRuntime()) throw sharingUnavailable();
    return normalizeShareStatus(
      await invoke<unknown>('start_share', { request, name: request.name }),
    );
  },

  async shareStatus(): Promise<ShareStatus> {
    if (!isTauriRuntime()) throw sharingUnavailable();
    return normalizeShareStatus(await invoke<unknown>('share_status'));
  },

  async stopShare(): Promise<ShareStatus> {
    if (!isTauriRuntime()) throw sharingUnavailable();
    return normalizeShareStatus(await invoke<unknown>('stop_share'));
  },
};

export const multiShareClient = {
  async list(): Promise<ShareStatus[]> {
    if (!isTauriRuntime()) return [];
    const values = await invoke<unknown[]>('list_shares');
    return values.map(normalizeShareStatus);
  },
  async revoke(id: string): Promise<void> {
    if (!isTauriRuntime()) throw sharingUnavailable();
    await invoke('revoke_share', { id });
  },
};
export type GuestShareClient = typeof guestShareClient;
