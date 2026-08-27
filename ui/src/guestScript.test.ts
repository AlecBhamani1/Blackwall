import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { TextDecoder as NodeTextDecoder, TextEncoder as NodeTextEncoder } from 'node:util';
import { afterEach, describe, expect, it, vi } from 'vitest';

const guestHtml = readFileSync(resolve(process.cwd(), '../src/core/src/share/assets/guest.html'), 'utf8');
const guestScript = readFileSync(resolve(process.cwd(), '../src/core/src/share/assets/guest.js'), 'utf8');
const gatewaySource = readFileSync(resolve(process.cwd(), '../src/core/src/share/gateway.rs'), 'utf8');
const bodyMarkup = guestHtml.match(/<body>([\s\S]*)<\/body>/)?.[1] ?? '';

type FetchInit = { body?: unknown };

function eventStreamResponse() {
  const frame = new NodeTextEncoder().encode(
    'data: {"choices":[{"delta":{"content":"ok"}}]}\n\ndata: [DONE]\n\n',
  );
  let delivered = false;
  return {
    ok: true,
    body: {
      getReader: () => ({
        read: async () => {
          if (delivered) return { value: undefined, done: true };
          delivered = true;
          return { value: frame, done: false };
        },
      }),
    },
  };
}

async function bootGuest(requestLimit: number) {
  document.body.innerHTML = bodyMarkup;
  window.history.replaceState(null, '', '/guest#key=bw1_test-key');
  Object.defineProperty(HTMLElement.prototype, 'scrollIntoView', {
    configurable: true,
    value: vi.fn(),
  });
  vi.stubGlobal('TextEncoder', NodeTextEncoder);
  vi.stubGlobal('TextDecoder', NodeTextDecoder);

  const chatBodies: Uint8Array[] = [];
  const fetchMock = vi.fn(async (input: string, init?: FetchInit) => {
    if (input === '/v1/models') {
      return {
        ok: true,
        json: async () => ({ data: [{ id: 'test-model' }] }),
      };
    }
    if (input === '/v1/chat/completions') {
      chatBodies.push(init?.body as Uint8Array);
      return eventStreamResponse();
    }
    throw new Error(`Unexpected fetch: ${input}`);
  });
  vi.stubGlobal('fetch', fetchMock);

  const source = guestScript.replace(
    'const MAX_REQUEST_BYTES = 32 * 1024 * 1024;',
    `const MAX_REQUEST_BYTES = ${requestLimit};`,
  );
  window.eval(source);
  await vi.waitFor(() => expect(document.querySelector('#status')).toHaveTextContent('test-model'));

  return { chatBodies, fetchMock };
}

function attachImage(size: number, name: string) {
  const input = document.querySelector<HTMLInputElement>('#files');
  const file = new File([new Uint8Array(size)], name, { type: 'image/png' });
  Object.defineProperty(input, 'files', { configurable: true, value: [file] });
  input?.dispatchEvent(new Event('change'));
}

async function submit(message: string) {
  const prompt = document.querySelector<HTMLTextAreaElement>('#prompt');
  const form = document.querySelector<HTMLFormElement>('#composer');
  const send = document.querySelector<HTMLButtonElement>('#send');
  if (!prompt || !form || !send) throw new Error('Guest composer fixture is incomplete.');

  prompt.value = message;
  form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
  expect(send).toBeDisabled();
  await vi.waitFor(() => expect(send).not.toBeDisabled());
}

afterEach(() => {
  document.body.replaceChildren();
});

describe('guest request payload limits', () => {
  it('stays locked to the Rust gateway body limit and sends the measured UTF-8 bytes', async () => {
    expect(guestScript).toContain('const MAX_REQUEST_BYTES = 32 * 1024 * 1024;');
    expect(gatewaySource).toContain('const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;');

    const { chatBodies } = await bootGuest(500);
    attachImage(100, 'first.png');
    await submit('first');
    attachImage(100, 'second.png');
    await submit('second');

    expect(chatBodies).toHaveLength(2);
    for (const body of chatBodies) {
      expect(ArrayBuffer.isView(body)).toBe(true);
      expect(body.byteLength).toBeLessThanOrEqual(500);
    }

    const secondPayload = JSON.parse(new NodeTextDecoder().decode(chatBodies[1]));
    expect(secondPayload.messages).toHaveLength(1);
    expect(secondPayload.messages[0].content[0].text).toBe('second');
    expect(document.querySelector('#conversation')).toHaveTextContent(
      'Earlier messages were left out to keep this request within the 32 MB safety limit.',
    );
  });

  it('rejects an oversized current message before fetch and leaves its attachment removable', async () => {
    const { chatBodies } = await bootGuest(300);
    attachImage(300, 'too-large.png');
    await submit('inspect this');

    expect(chatBodies).toHaveLength(0);
    expect(document.querySelector('#conversation')).toHaveTextContent(
      'This message is too large to send safely. Remove one or more attachments and try again',
    );
    expect(document.querySelectorAll('.attachment')).toHaveLength(1);
    expect(document.querySelector<HTMLTextAreaElement>('#prompt')?.value).toBe('inspect this');
  });
});
