import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import type { ChatMessage } from '../types';
import MessageCell from './MessageCell.svelte';

function message(overrides: Partial<ChatMessage> = {}): ChatMessage {
  return {
    id: 'message-1',
    role: 'assistant',
    content: 'Hello',
    attachments: [],
    status: 'complete',
    createdAt: 1,
    ...overrides,
  };
}

describe('MessageCell', () => {
  it('keeps reasoning collapsed under its duration, apart from the answer', async () => {
    const user = userEvent.setup();
    const { container } = render(MessageCell, {
      props: {
        message: message({
          content: 'The answer.',
          reasoning: 'First I checked the plan.',
          reasoningMs: 2600,
        }),
      },
    });
    const thought = container.querySelector<HTMLDetailsElement>('details.reasoning')!;
    expect(thought.open).toBe(false);
    expect(screen.getByText('Thought for 3s')).toBeInTheDocument();
    expect(container.querySelector('.markdown')?.textContent).not.toContain('checked the plan');
    await user.click(screen.getByText('Thought for 3s'));
    expect(thought.open).toBe(true);
    expect(thought).toHaveTextContent('First I checked the plan.');
  });

  it('shows no reasoning section when the model did not reason', () => {
    const { container } = render(MessageCell, { props: { message: message() } });
    expect(container.querySelector('details.reasoning')).toBeNull();
  });

  it('interleaves tool groups with streamed text and preserves expansion as results arrive', async () => {
    const user = userEvent.setup();
    const writeText = vi.spyOn(navigator.clipboard, 'writeText').mockResolvedValue();
    const intro = 'I will search.\n\n';
    const followup = 'I will read the page.\n\n';
    const tools = [
      {
        id: 'search',
        name: 'web_search',
        arguments: '{}',
        contentOffset: intro.length,
        status: 'running' as const,
      },
      {
        id: 'search-2',
        name: 'web_search',
        arguments: '{}',
        contentOffset: intro.length,
        status: 'complete' as const,
      },
    ];
    const { container, rerender } = render(MessageCell, {
      props: { message: message({ content: intro, status: 'streaming', tools }) },
    });
    const group = container.querySelector<HTMLDetailsElement>('.tool-group')!;
    expect(group.open).toBe(false);
    await user.click(screen.getByText('2 tool calls'));
    await rerender({
      message: message({
        content: intro + followup + 'The answer.',
        tools: [
          { ...tools[0], status: 'complete', output: 'Search results' },
          tools[1],
          {
            id: 'fetch',
            name: 'web_fetch',
            arguments: '{}',
            contentOffset: intro.length + followup.length,
            status: 'complete',
          },
        ],
      }),
    });
    const parts = container.querySelectorAll(
      '.message-body > .markdown, .message-body > .tool-activity',
    );
    expect(Array.from(parts, (part) => part.textContent)).toEqual([
      expect.stringContaining('I will search.'),
      expect.stringContaining('2 tool calls'),
      expect.stringContaining('I will read the page.'),
      expect.stringContaining('1 tool call'),
      expect.stringContaining('The answer.'),
    ]);
    expect(container.querySelector('.tool-group')).toBe(group);
    expect(group.open).toBe(true);
    expect(container.querySelectorAll<HTMLDetailsElement>('.tool-group')[1].open).toBe(false);
    await user.click(screen.getByRole('button', { name: 'Copy response' }));
    expect(writeText).toHaveBeenCalledWith(intro + followup + 'The answer.');
  });

  it('groups legacy calls before the answer and renders tool-only responses', async () => {
    const tools = [{ id: 'read', name: 'read_file', arguments: '{}', status: 'stopped' as const }];
    const { container, rerender } = render(MessageCell, {
      props: { message: message({ tools, historical: true }) },
    });
    expect(container.querySelector('.message-body')?.firstElementChild).toHaveClass(
      'tool-activity',
    );
    expect(screen.getByText('History · 1 stopped')).toBeVisible();
    expect(screen.getByText('Hello')).toBeVisible();
    await rerender({ message: message({ tools, content: '', status: 'stopped' }) });
    expect(screen.getByText('1 tool call')).toBeVisible();
  });

  it('renders model markdown and sanitizes executable markup', () => {
    const { container } = render(MessageCell, {
      props: { message: message({ content: '**Safe** <script>window.bad = true</script>' }) },
    });

    expect(screen.getByText('Safe')).toHaveTextContent('Safe');
    expect(container.querySelector('strong')).toBeInTheDocument();
    expect(container.querySelector('script')).not.toBeInTheDocument();
  });

  it('renders a user file attachment and message text', () => {
    render(MessageCell, {
      props: {
        message: message({
          role: 'user',
          content: 'Review this',
          attachments: [
            {
              id: 'attachment-1',
              name: 'brief.pdf',
              mimeType: 'application/pdf',
              sizeBytes: 2_048,
              kind: 'file',
            },
          ],
        }),
      },
    });

    expect(screen.getByText('Review this')).toBeInTheDocument();
    expect(screen.getByText('brief.pdf')).toBeInTheDocument();
    expect(screen.getByText('2.0 KB')).toBeInTheDocument();
  });

  it('renders completed streamed equations and copies the original LaTeX', async () => {
    const user = userEvent.setup();
    const writeText = vi.spyOn(navigator.clipboard, 'writeText').mockResolvedValue();
    const { container, rerender } = render(MessageCell, {
      props: { message: message({ content: 'Result: $x^2', status: 'streaming' }) },
    });
    expect(container.querySelector('.katex')).toBeNull();
    expect(container).toHaveTextContent('Result: $x^2');

    const content = String.raw`Result: $x^2$ and \[\frac{1}{2}\]`;
    await rerender({ message: message({ content }) });
    expect(container.querySelectorAll('.katex')).toHaveLength(2);
    await user.click(screen.getByRole('button', { name: 'Copy response' }));
    expect(writeText).toHaveBeenCalledWith(content);
  });

  it('copies individual code blocks verbatim without surrounding text or Markdown fences', async () => {
    const user = userEvent.setup();
    const writeText = vi.spyOn(navigator.clipboard, 'writeText').mockResolvedValue();
    const code = 'if a < b && b > 0:\n    print("<script> & done")\n';
    const content = `Example with \`inline code\`:\n\n\`\`\`python\n${code}\`\`\`\n\nOutput:\n\n\`\`\`\nDone\n\`\`\``;
    const { container } = render(MessageCell, { props: { message: message({ content }) } });

    const buttons = screen.getAllByRole('button', { name: 'Copy code' });
    expect(buttons).toHaveLength(2);
    await user.tab();
    expect(buttons[0]).toHaveFocus();
    await user.keyboard('{Enter}');
    expect(writeText).toHaveBeenLastCalledWith(code);
    expect(buttons[0]).toHaveTextContent('Copied!');
    expect(buttons[1]).toHaveTextContent('Copy code');
    expect(container.querySelector('script')).not.toBeInTheDocument();

    await user.click(buttons[1]);
    expect(writeText).toHaveBeenLastCalledWith('Done\n');
    await user.click(screen.getByRole('button', { name: 'Copy response' }));
    expect(writeText).toHaveBeenLastCalledWith(content);
  });

  it('copies updated code while a response streams and supports indented blocks', async () => {
    const user = userEvent.setup();
    const writeText = vi.spyOn(navigator.clipboard, 'writeText').mockResolvedValue();
    const { rerender } = render(MessageCell, {
      props: { message: message({ content: '```python\nprint(', status: 'streaming' }) },
    });
    await user.click(screen.getByRole('button', { name: 'Copy code' }));
    expect(writeText).toHaveBeenLastCalledWith('print(\n');

    await rerender({ message: message({ content: '```python\nprint(1)\n```' }) });
    await user.click(screen.getByRole('button', { name: 'Copy code' }));
    expect(writeText).toHaveBeenLastCalledWith('print(1)\n');

    await rerender({ message: message({ content: '    print(2)' }) });
    await user.click(screen.getByRole('button', { name: 'Copy code' }));
    expect(writeText).toHaveBeenLastCalledWith('print(2)\n');
  });

  it('shows clipboard failure and allows a successful retry', async () => {
    const user = userEvent.setup();
    const writeText = vi
      .spyOn(navigator.clipboard, 'writeText')
      .mockRejectedValueOnce(new Error('Permission denied'))
      .mockResolvedValue();
    render(MessageCell, { props: { message: message({ content: '```\nhello\n```' }) } });

    await user.click(screen.getByRole('button', { name: 'Copy code' }));
    const retry = screen.getByRole('button', { name: 'Copy failed — retry' });
    await user.click(retry);
    expect(writeText).toHaveBeenCalledTimes(2);
    expect(screen.getByRole('button', { name: 'Copied!' })).toBeEnabled();
  });
});
