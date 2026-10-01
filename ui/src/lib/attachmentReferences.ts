import type { Attachment } from './types';

export interface AttachmentReference {
  attachment: Attachment;
  label: string;
  token: string;
}

// Stable IDs deduplicate files reused in later turns. Distinct files with the same
// name get distinct, readable tokens, including names containing whitespace.
export function attachmentReferences(attachments: readonly Attachment[]): AttachmentReference[] {
  const seenIds = new Set<string>();
  const usedLabels = new Set<string>();
  const names = new Set(attachments.map((attachment) => attachment.name));
  return attachments.flatMap((attachment) => {
    if (seenIds.has(attachment.id)) return [];
    seenIds.add(attachment.id);
    let label = attachment.name;
    let occurrence = 2;
    while (usedLabels.has(label) || (label !== attachment.name && names.has(label))) {
      label = `${attachment.name} (${occurrence++})`;
    }
    usedLabels.add(label);
    const token = `@${/[\s"\\@]/.test(label) ? JSON.stringify(label) : label}`;
    return [{ attachment, label, token }];
  });
}

export function referencedAttachments(
  text: string,
  references: readonly AttachmentReference[],
): Attachment[] {
  return references
    .filter(({ token }) => {
      const escaped = token.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
      return new RegExp(`(?:^|\\s)${escaped}(?=$|\\s|[,.!?;:](?:$|\\s))`).test(text);
    })
    .map(({ attachment }) => attachment);
}

export function mentionAtCaret(
  text: string,
  caret: number,
): { start: number; query: string } | null {
  const match = /(?:^|\s)@(?:"((?:[^"\\]|\\.)*)|([^\s"@]*))$/.exec(text.slice(0, caret));
  if (!match) return null;
  const query = match[1] ?? match[2];
  return { start: caret - query.length - (match[1] === undefined ? 1 : 2), query };
}
