import { createId } from './id';
import type {
  AttachmentKind,
  AttachmentPayload,
  AttachmentRejection,
  PendingAttachment,
} from './types';

export const MAX_ATTACHMENT_COUNT = 8;
export const MAX_ATTACHMENT_SIZE_BYTES = 15 * 1024 * 1024;
export const MAX_ATTACHMENT_TOTAL_BYTES = 30 * 1024 * 1024;
const MAX_TEXT_CONTENT_BYTES = 512 * 1024;

const TEXT_EXTENSIONS = new Set([
  'c',
  'cc',
  'conf',
  'cpp',
  'css',
  'csv',
  'go',
  'h',
  'hpp',
  'html',
  'ini',
  'java',
  'js',
  'json',
  'jsx',
  'log',
  'md',
  'py',
  'rb',
  'rs',
  'sh',
  'sql',
  'svelte',
  'toml',
  'ts',
  'tsx',
  'txt',
  'xml',
  'yaml',
  'yml',
]);

function extensionOf(fileName: string): string {
  return fileName.includes('.') ? fileName.split('.').at(-1)?.toLowerCase() || '' : '';
}

export function attachmentKind(file: File): AttachmentKind {
  if (file.type.startsWith('image/')) return 'image';
  if (file.type.startsWith('text/') || TEXT_EXTENSIONS.has(extensionOf(file.name))) return 'text';
  return 'file';
}

function fingerprint(file: File): string {
  return `${file.name}:${file.size}:${file.lastModified}:${file.type}`;
}

function makePreviewUrl(file: File): string | undefined {
  if (!file.type.startsWith('image/') || typeof URL.createObjectURL !== 'function') return undefined;
  return URL.createObjectURL(file);
}

export function prepareAttachments(
  files: Iterable<File>,
  existing: readonly PendingAttachment[] = [],
): { accepted: PendingAttachment[]; rejected: AttachmentRejection[] } {
  const accepted: PendingAttachment[] = [];
  const rejected: AttachmentRejection[] = [];
  const knownFingerprints = new Set(existing.map((attachment) => attachment.fingerprint));
  let totalBytes = existing.reduce((total, attachment) => total + attachment.sizeBytes, 0);

  for (const file of files) {
    const fileFingerprint = fingerprint(file);

    if (knownFingerprints.has(fileFingerprint)) {
      rejected.push({ fileName: file.name, reason: 'Already attached' });
      continue;
    }

    if (existing.length + accepted.length >= MAX_ATTACHMENT_COUNT) {
      rejected.push({ fileName: file.name, reason: `Up to ${MAX_ATTACHMENT_COUNT} files per message` });
      continue;
    }

    if (file.size > MAX_ATTACHMENT_SIZE_BYTES) {
      rejected.push({ fileName: file.name, reason: 'File is larger than 15 MB' });
      continue;
    }

    if (totalBytes + file.size > MAX_ATTACHMENT_TOTAL_BYTES) {
      rejected.push({ fileName: file.name, reason: 'Attachments exceed the 30 MB message limit' });
      continue;
    }

    const pending: PendingAttachment = {
      id: createId('attachment'),
      name: file.name,
      mimeType: file.type || 'application/octet-stream',
      sizeBytes: file.size,
      kind: attachmentKind(file),
      previewUrl: makePreviewUrl(file),
      file,
      fingerprint: fileFingerprint,
    };

    accepted.push(pending);
    knownFingerprints.add(fileFingerprint);
    totalBytes += file.size;
  }

  return { accepted, rejected };
}

function readAsDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(new Error(`Could not read ${file.name}`));
    reader.onload = () => resolve(String(reader.result));
    reader.readAsDataURL(file);
  });
}

export async function materializeAttachment(attachment: PendingAttachment): Promise<AttachmentPayload> {
  const payload: AttachmentPayload = {
    id: attachment.id,
    name: attachment.name,
    mimeType: attachment.mimeType,
    sizeBytes: attachment.sizeBytes,
    kind: attachment.kind,
  };

  if (attachment.kind === 'text' && attachment.sizeBytes <= MAX_TEXT_CONTENT_BYTES) {
    payload.textContent = await attachment.file.text();
  } else {
    payload.dataUrl = await readAsDataUrl(attachment.file);
  }

  return payload;
}

export function revokeAttachmentPreview(attachment: Pick<PendingAttachment, 'previewUrl'>): void {
  if (attachment.previewUrl?.startsWith('blob:') && typeof URL.revokeObjectURL === 'function') {
    URL.revokeObjectURL(attachment.previewUrl);
  }
}
