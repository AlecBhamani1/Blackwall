import { render, screen } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
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
});
