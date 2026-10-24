import { fireEvent, render } from '@testing-library/svelte';
import { tick } from 'svelte';
import { describe, expect, it, vi } from 'vitest';
import type { ChatMessage } from '../types';
import ChatView from './ChatView.svelte';

function message(content: string, status: ChatMessage['status'] = 'streaming'): ChatMessage {
  return { id: 'response', role: 'assistant', content, status, attachments: [], createdAt: 1 };
}

async function conversation() {
  const view = render(ChatView, { props: { messages: [message('First chunk')] } });
  const scroller = view.container.querySelector<HTMLDivElement>('.chat-scroll')!;
  let height = 1_000;
  let top = 0;
  const scroll = vi.fn((value: number) => {
    top = Math.max(0, Math.min(value, height - 400));
  });
  Object.defineProperties(scroller, {
    clientHeight: { get: () => 400 },
    scrollHeight: { get: () => height },
    scrollTop: { get: () => top, set: scroll },
  });
  await tick();

  return {
    ...view,
    scroller,
    scroll,
    async scrollTo(value: number) {
      scroller.scrollTop = value;
      await fireEvent.scroll(scroller);
      scroll.mockClear();
    },
    async stream(content: string, status: ChatMessage['status'] = 'streaming') {
      height += 100;
      await view.rerender({ messages: [message(content, status)] });
      await tick();
    },
  };
}

describe('ChatView scrolling', () => {
  it('opens at the bottom and keeps following streamed content', async () => {
    const view = await conversation();
    expect(view.scroller.scrollTop).toBe(600);
    await fireEvent.scroll(view.scroller);
    await view.stream('First chunk, then another chunk');
    expect(view.scroller.scrollTop).toBe(700);
    await fireEvent.scroll(view.scroller);
    await view.stream('The completed response', 'complete');
    expect(view.scroller.scrollTop).toBe(800);
  });

  it('preserves a small upward scroll through streaming and completion', async () => {
    const view = await conversation();
    await view.scrollTo(560);
    await view.stream('More streamed content');
    await view.stream('The completed response', 'complete');
    expect(view.scroller.scrollTop).toBe(560);
    expect(view.scroll).not.toHaveBeenCalled();
  });

  it('resumes following only after scrolling back to the bottom', async () => {
    const view = await conversation();
    await view.scrollTo(200);
    await view.stream('More streamed content');
    await view.scrollTo(650);
    await view.stream('Still more streamed content');
    expect(view.scroller.scrollTop).toBe(650);
    expect(view.scroll).not.toHaveBeenCalled();

    await view.scrollTo(800);
    await view.stream('Back to following the response');
    expect(view.scroller.scrollTop).toBe(900);
  });

  it('tolerates fractional pixel rounding at the bottom', async () => {
    const view = await conversation();
    await view.scrollTo(599.5);
    await view.stream('More streamed content');
    expect(view.scroller.scrollTop).toBe(700);
  });

  it('cancels a pending follow when the reader scrolls before the DOM update settles', async () => {
    const view = await conversation();
    const updating = view.rerender({ messages: [message('More streamed content')] });
    view.scroller.scrollTop = 200;
    view.scroller.dispatchEvent(new Event('scroll'));
    view.scroll.mockClear();
    await updating;
    await tick();
    expect(view.scroller.scrollTop).toBe(200);
    expect(view.scroll).not.toHaveBeenCalled();
  });

  it('keeps a recalled message in view while content updates', async () => {
    const view = await conversation();
    const target = view.container.querySelector('#message-response')!;
    const scrollIntoView = vi.fn();
    Object.defineProperty(target, 'scrollIntoView', { value: scrollIntoView });
    await view.rerender({ focusedMessageId: 'response' });
    expect(scrollIntoView).toHaveBeenCalledWith({ block: 'center' });
    await view.scrollTo(200);
    await view.stream('More streamed content');
    expect(view.scroller.scrollTop).toBe(200);
    expect(view.scroll).not.toHaveBeenCalled();
  });
});
