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
