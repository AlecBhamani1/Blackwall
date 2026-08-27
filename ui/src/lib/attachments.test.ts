import { describe, expect, it } from 'vitest';
import {
  MAX_ATTACHMENT_COUNT,
  MAX_ATTACHMENT_SIZE_BYTES,
  attachmentKind,
  materializeAttachment,
  prepareAttachments,
} from './attachments';

describe('attachments', () => {
  it('classifies images, text-like source files, and binary files', () => {
    expect(attachmentKind(new File(['x'], 'photo.png', { type: 'image/png' }))).toBe('image');
    expect(attachmentKind(new File(['const x = 1'], 'index.ts'))).toBe('text');
    expect(attachmentKind(new File(['x'], 'report.pdf', { type: 'application/pdf' }))).toBe('file');
  });

  it('accepts supported files and rejects duplicates deterministically', () => {
    const photo = new File(['pixels'], 'photo.png', { type: 'image/png', lastModified: 42 });
    const first = prepareAttachments([photo]);
    const second = prepareAttachments([photo], first.accepted);

    expect(first.accepted).toHaveLength(1);
    expect(first.accepted[0]).toMatchObject({ name: 'photo.png', kind: 'image' });
    expect(second.accepted).toHaveLength(0);
    expect(second.rejected[0]?.reason).toBe('Already attached');
  });

  it('enforces per-file and per-message count limits', () => {
    const oversized = new File([new Uint8Array(MAX_ATTACHMENT_SIZE_BYTES + 1)], 'large.bin');
    expect(prepareAttachments([oversized]).rejected[0]?.reason).toContain('15 MB');

    const files = Array.from({ length: MAX_ATTACHMENT_COUNT + 1 }, (_, index) =>
      new File(['x'], `file-${index}.txt`, { lastModified: index }),
    );
    const result = prepareAttachments(files);
    expect(result.accepted).toHaveLength(MAX_ATTACHMENT_COUNT);
    expect(result.rejected).toHaveLength(1);
  });

  it('materializes text attachments without base64 inflation', async () => {
    const [pending] = prepareAttachments([new File(['hello Blackwall'], 'notes.md', { type: 'text/markdown' })])
      .accepted;
    await expect(materializeAttachment(pending)).resolves.toMatchObject({
      name: 'notes.md',
      kind: 'text',
      textContent: 'hello Blackwall',
    });
  });
});
