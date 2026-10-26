import type { ChatMessage, ToolEntry } from './types';

type MessagePart =
  { id: string; type: 'text'; content: string } | { id: string; type: 'tools'; tools: ToolEntry[] };

/** Keep calls at their streamed position and group calls with no text between them. */
export function messageTimeline(message: ChatMessage): MessagePart[] {
  const parts: MessagePart[] = [];
  const calls = (message.tools ?? [])
    .map((tool) => ({
      tool,
      // Older transcripts have no call positions; show their activity before the answer.
      offset: Math.min(
        message.content.length,
        Math.max(0, Math.trunc(tool.contentOffset ?? 0) || 0),
      ),
    }))
    .sort((a, b) => a.offset - b.offset);
  let cursor = 0;
  for (const { tool, offset } of calls) {
    if (offset > cursor) {
      parts.push({
        id: `text-${cursor}`,
        type: 'text',
        content: message.content.slice(cursor, offset),
      });
    }
    const last = parts.at(-1);
    if (last?.type === 'tools') last.tools.push(tool);
    else parts.push({ id: `tools-${offset}`, type: 'tools', tools: [tool] });
    cursor = offset;
  }
  if (cursor < message.content.length) {
    parts.push({ id: `text-${cursor}`, type: 'text', content: message.content.slice(cursor) });
  }
  return parts;
}
