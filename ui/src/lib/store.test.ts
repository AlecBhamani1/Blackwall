import { get } from 'svelte/store';
import { describe, expect, it, vi } from 'vitest';
import type { LocalModelClient } from './ipc';
import { createChatController } from './store';

function mockClient(): LocalModelClient {
  return {
    modelEndpoint: vi.fn().mockResolvedValue('http://localhost:11434/v1'),
    discoverModels: vi.fn().mockResolvedValue([{ id: 'remote-model:latest', name: 'remote-model:latest' }]),
    streamChat: vi.fn(async (_request, callbacks) => {
      callbacks.onDelta('Blackwall ');
      callbacks.onDelta('is ready.');
      callbacks.onComplete?.();
    }),
  };
}

describe('chat controller', () => {
  it('discovers the configured endpoint and streams a complete turn', async () => {
    const client = mockClient();
    const controller = createChatController(client);

    await controller.initialize();
    expect(get(controller.connectionState)).toBe('ready');
    expect(get(controller.selectedModel)).toBe('remote-model:latest');

    await expect(controller.send('Hello', [])).resolves.toBe(true);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));

    const messages = get(controller.messages);
    expect(messages).toHaveLength(2);
    expect(messages[0]).toMatchObject({ role: 'user', content: 'Hello' });
    expect(messages[1]).toMatchObject({
      role: 'assistant',
      content: 'Blackwall is ready.',
      status: 'complete',
    });
    expect(get(controller.sessions)[0]?.title).toBe('Hello');
  });

  it('persists an endpoint override and uses it for discovery and chat', async () => {
    const client = mockClient();
    const controller = createChatController(client);
    const endpoint = 'http://100.76.24.116:11434';

    await expect(controller.configureEndpoint(endpoint)).resolves.toBe(true);
    expect(client.discoverModels).toHaveBeenCalledWith(endpoint);
    expect(window.localStorage.getItem('blackwall.endpoint.v1')).toBe(endpoint);

    await controller.send('Use the configured host', []);
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    expect(client.streamChat).toHaveBeenCalledWith(
      expect.objectContaining({ endpoint }),
      expect.any(Object),
      expect.any(AbortSignal),
    );
  });

  it('surfaces the endpoint failure detail for troubleshooting', async () => {
    const client = mockClient();
    client.discoverModels = vi.fn().mockRejectedValue(new Error('connection refused at 127.0.0.1:11434'));
    const controller = createChatController(client);

    await expect(controller.initialize()).resolves.toBe(false);
    expect(get(controller.connectionState)).toBe('offline');
    expect(get(controller.connectionError)).toBe('connection refused at 127.0.0.1:11434');
    expect(get(controller.notice)).toBe('connection refused at 127.0.0.1:11434');
  });

  it('does not send whitespace or send while disconnected', async () => {
    const client = mockClient();
    const controller = createChatController(client);

    await expect(controller.send('   ', [])).resolves.toBe(false);
    await expect(controller.send('Hello', [])).resolves.toBe(false);
    expect(client.streamChat).not.toHaveBeenCalled();
  });

  it('marks an interrupted response as stopped', async () => {
    const client = mockClient();
    client.streamChat = vi.fn((_request, _callbacks, signal) =>
      new Promise<void>((_resolve, reject) => {
        signal.addEventListener('abort', () => reject(new DOMException('Stopped', 'AbortError')));
      }),
    );
    const controller = createChatController(client);
    await controller.initialize();
    await controller.send('Keep it short', []);

    controller.stop();
    await vi.waitFor(() => expect(get(controller.runState)).toBe('idle'));
    expect(get(controller.messages).at(-1)).toMatchObject({ status: 'stopped', content: 'Response stopped.' });
  });
});
