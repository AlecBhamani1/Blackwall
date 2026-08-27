import type {
  AgentEvent,
  AttachmentPayload,
  ChatRequest,
  ModelInfo,
  ModelMessage,
  StreamCallbacks,
  ShareStatus,
  StartShareRequest,
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

function normalizeBrowserEndpoint(endpoint: string): string {
  const trimmed = endpoint.trim();
  const candidate = trimmed.includes('://') ? trimmed : `http://${trimmed}`;
  const url = new URL(candidate);
  url.search = '';
  url.hash = '';
  if (url.pathname === '' || url.pathname === '/') url.pathname = '/v1';
  return url.toString().replace(/\/$/, '');
}

function browserModelRoutes(endpoint?: string): { models: string; tags?: string; chat: string } {
  if (!endpoint?.trim()) {
    return {
      models: '/local-llm/v1/models',
      tags: '/local-llm/api/tags',
      chat: '/local-llm/v1/chat/completions',
    };
  }

  const normalized = normalizeBrowserEndpoint(endpoint);
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
        .sort((a, b) => (a.sizeBytes ?? Number.MAX_SAFE_INTEGER) - (b.sizeBytes ?? Number.MAX_SAFE_INTEGER));

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

function parseSseBlock(block: string, callbacks: StreamCallbacks): boolean {
  for (const line of block.split(/\r?\n/)) {
    if (!line.startsWith('data:')) continue;
    const data = line.slice(5).trim();
    if (!data) continue;
    if (data === '[DONE]') return true;

    const payload = JSON.parse(data) as {
      choices?: Array<{ delta?: { content?: unknown }; message?: { content?: unknown } }>;
    };
    const choice = payload.choices?.[0];
    const delta = deltaText(choice?.delta?.content ?? choice?.message?.content);
    if (delta) callbacks.onDelta(delta);
  }

  return false;
}

async function streamBrowserChat(
  request: ChatRequest,
  callbacks: StreamCallbacks,
  signal: AbortSignal,
): Promise<void> {
  const response = await fetch(browserModelRoutes(request.endpoint).chat, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      model: request.model,
      messages: request.messages.map(toOpenAIMessage),
      stream: true,
    }),
    signal,
  });

  if (!response.ok) {
    let detail = '';
    try {
      const payload = (await response.json()) as { error?: { message?: string } | string };
      detail = typeof payload.error === 'string' ? payload.error : payload.error?.message ?? '';
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

  while (!finished) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });

    const blocks = buffer.split(/\r?\n\r?\n/);
    buffer = blocks.pop() ?? '';
    for (const block of blocks) {
      if (parseSseBlock(block, callbacks)) {
        finished = true;
        break;
      }
    }
  }

  if (buffer.trim() && !finished) parseSseBlock(buffer, callbacks);
  callbacks.onComplete?.();
}

function normalizeEvent(payload: unknown): AgentEvent | null {
  if (!payload || typeof payload !== 'object') return null;
  const event = payload as Record<string, unknown>;
  const type = String(event.type ?? event.event ?? '');
  const requestId = String(event.requestId ?? event.request_id ?? '');

  if (type === 'assistant_delta') {
    return { type, requestId, delta: String(event.delta ?? event.content ?? '') };
  }
  if (type === 'turn_complete') {
    return {
      type,
      requestId,
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
  return null;
}

async function streamTauriChat(
  request: ChatRequest,
  callbacks: StreamCallbacks,
  signal: AbortSignal,
): Promise<void> {
  const [{ invoke }, { listen }] = await Promise.all([
    import('@tauri-apps/api/core'),
    import('@tauri-apps/api/event'),
  ]);

  let remoteError: Error | undefined;
  let complete = false;
  const unlisten = await listen<unknown>('blackwall://event', ({ payload }) => {
    const event = normalizeEvent(payload);
    if (!event || event.requestId !== request.requestId || signal.aborted) return;

    if (event.type === 'assistant_delta') callbacks.onDelta(event.delta);
    if (event.type === 'turn_complete') {
      complete = true;
      callbacks.onComplete?.();
    }
    if (event.type === 'error') remoteError = new Error(event.message);
  });

  try {
    if (signal.aborted) throw new DOMException('The request was stopped.', 'AbortError');
    await invoke('stream_chat', { request });
    if (signal.aborted) throw new DOMException('The request was stopped.', 'AbortError');
    if (remoteError) throw remoteError;
    if (!complete) callbacks.onComplete?.();
  } finally {
    unlisten();
  }
}

export const localModelClient = {
  async modelEndpoint(): Promise<string> {
    if (!isTauriRuntime()) return '';
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke<string>('model_endpoint');
  },

  async discoverModels(endpoint?: string): Promise<ModelInfo[]> {
    if (!isTauriRuntime()) return discoverBrowserModels(endpoint);
    const { invoke } = await import('@tauri-apps/api/core');
    return normalizeCatalog(await invoke<unknown>('discover_models', { endpoint: endpoint || null }));
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
    const { invoke } = await import('@tauri-apps/api/core');
    return normalizeShareStatus(await invoke<unknown>('start_share', { request }));
  },

  async shareStatus(): Promise<ShareStatus> {
    if (!isTauriRuntime()) throw sharingUnavailable();
    const { invoke } = await import('@tauri-apps/api/core');
    return normalizeShareStatus(await invoke<unknown>('share_status'));
  },

  async stopShare(): Promise<ShareStatus> {
    if (!isTauriRuntime()) throw sharingUnavailable();
    const { invoke } = await import('@tauri-apps/api/core');
    return normalizeShareStatus(await invoke<unknown>('stop_share'));
  },
};

export type GuestShareClient = typeof guestShareClient;
