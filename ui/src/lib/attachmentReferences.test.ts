import { describe, expect, it } from 'vitest';
import {
  attachmentReferences,
  mentionAtCaret,
  referencedAttachments,
} from './attachmentReferences';
import type { Attachment } from './types';

function file(id: string, name: string): Attachment {
  return { id, name, kind: 'text', mimeType: 'text/plain', sizeBytes: 5, textContent: id };
}

describe('attachment references', () => {
  it('deduplicates reused files by identity and disambiguates colliding names', () => {
    const first = file('first', 'notes.md');
    const references = attachmentReferences([
      first,
      file('second', 'notes.md'),
      file('third', 'notes.md (2)'),
      first,
      file('fourth', 'notes.md'),
    ]);
    expect(references.map(({ token }) => token)).toEqual([
      '@notes.md',
      '@"notes.md (3)"',
      '@"notes.md (2)"',
      '@"notes.md (4)"',
    ]);
    expect(referencedAttachments('Compare @notes.md and @"notes.md (3)".', references)).toEqual([
      first,
      file('second', 'notes.md'),
    ]);
  });

  it('quotes screenshot names and treats regex characters literally', () => {
    const screenshot = file('image', 'Screenshot 2026-09-30 at 10.00.00.png');
    const source = file('source', 'a+[b].ts');
    const references = attachmentReferences([screenshot, source]);
    expect(referencedAttachments(`Read ${references[0].token} and @a+[b].ts!`, references)).toEqual(
      [screenshot, source],
    );
    expect(referencedAttachments('@aaab.ts', references)).toEqual([]);
  });

  it('ignores email addresses, filename prefixes, and deleted references', () => {
    const references = attachmentReferences([file('one', 'notes.md')]);
    expect(referencedAttachments('user@notes.md @notes.md.bak @notes.m', references)).toEqual([]);
    expect(referencedAttachments('Read @notes.md, then @notes.md', references)).toHaveLength(1);
  });

  it('finds a mention at the caret, including quoted names and mid-message edits', () => {
    expect(mentionAtCaret('Compare @screen with this', 15)).toEqual({ start: 8, query: 'screen' });
    expect(mentionAtCaret('Read @"Screen shot', 18)).toEqual({ start: 5, query: 'Screen shot' });
    expect(mentionAtCaret('hello@example.com', 17)).toBeNull();
    expect(mentionAtCaret('Read @notes.md ', 15)).toBeNull();
  });
});
