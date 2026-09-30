<script lang="ts">
  import { onDestroy, onMount, tick } from 'svelte';
  import { prepareAttachments, revokeAttachmentPreview } from '../attachments';
  import {
    attachmentReferences,
    mentionAtCaret,
    type AttachmentReference,
  } from '../attachmentReferences';
  import type { Attachment, PendingAttachment } from '../types';
  import AttachmentTray from './AttachmentTray.svelte';
  import Icon from './Icon.svelte';

  export let busy = false;
  export let disabled = false;
  export let notice = '';
  export let onDismissNotice: () => void = () => undefined;
  export let onSend: (text: string, attachments: PendingAttachment[]) => Promise<boolean>;
  export let onStop: () => void;
  export let chatAttachments: Attachment[] = [];
  export let sessionId: string | null = null;

  let text = '';
  let attachments: PendingAttachment[] = [];
  let fileInput: HTMLInputElement;
  let textarea: HTMLTextAreaElement;
  let submitting = false;
  let mention: ReturnType<typeof mentionAtCaret> = null;
  let mentionIndex = 0;
  let draftSessionId = sessionId;
  let attachmentNotice = '';

  $: canSend = !disabled && !busy && !submitting && Boolean(text.trim() || attachments.length);
  $: references = attachmentReferences([...chatAttachments, ...attachments]);
  $: suggestions = mention
    ? references.filter(({ label }) => label.toLowerCase().includes(mention!.query.toLowerCase()))
    : [];
  $: if (sessionId !== draftSessionId) {
    attachments.forEach(revokeAttachmentPreview);
    attachments = [];
    text = '';
    mention = null;
    attachmentNotice = '';
    draftSessionId = sessionId;
  }

  function updateMention() {
    mention = mentionAtCaret(text, textarea.selectionStart);
    mentionIndex = 0;
  }

  async function selectReference(reference: AttachmentReference) {
    if (!mention) return;
    const caret = textarea.selectionStart;
    const before = text.slice(0, mention.start);
    const after = text.slice(caret);
    const insertion = `${reference.token} `;
    text = before + insertion + after;
    mention = null;
    await tick();
    textarea.focus();
    textarea.setSelectionRange(before.length + insertion.length, before.length + insertion.length);
    resizeTextarea();
  }

  function input() {
    resizeTextarea();
    updateMention();
  }

  function resizeTextarea() {
    if (!textarea) return;
    textarea.style.height = 'auto';
    textarea.style.height = `${Math.min(textarea.scrollHeight, 180)}px`;
  }

  export function addFiles(files: Iterable<File>) {
    if (busy || submitting) return;
    const result = prepareAttachments(files, attachments);
    attachments = [...attachments, ...result.accepted];
    attachmentNotice = result.rejected
      .map((item) => `${item.fileName}: ${item.reason}`)
      .join(' · ');
    textarea?.focus();
  }

  function chooseFiles() {
    if (!busy && !submitting) fileInput.click();
  }

  function selectedFiles(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    if (input.files) addFiles(input.files);
    input.value = '';
  }

  function removeAttachment(attachmentId: string) {
    const attachment = attachments.find((item) => item.id === attachmentId);
    if (attachment) revokeAttachmentPreview(attachment);
    attachments = attachments.filter((item) => item.id !== attachmentId);
    attachmentNotice = '';
  }

  async function submit() {
    if (!canSend) return;
    const submitted = attachments;
    submitting = true;
    let accepted: boolean;
    try {
      accepted = await onSend(text, submitted);
    } finally {
      submitting = false;
    }
    if (!accepted) return;
    text = '';
    mention = null;
    attachments = [];
    attachmentNotice = '';
    await tick();
    resizeTextarea();
    textarea?.focus();
  }

  function keydown(event: KeyboardEvent) {
    if (event.isComposing) return;
    if (mention && suggestions.length) {
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        event.preventDefault();
        const direction = event.key === 'ArrowDown' ? 1 : -1;
        mentionIndex = (mentionIndex + direction + suggestions.length) % suggestions.length;
        return;
      }
      if ((event.key === 'Enter' && !event.shiftKey) || event.key === 'Tab') {
        event.preventDefault();
        void selectReference(suggestions[mentionIndex] ?? suggestions[0]);
        return;
      }
      if (event.key === 'Escape') {
        event.preventDefault();
        event.stopPropagation();
        mention = null;
        return;
      }
    }
    if (event.key !== 'Enter' || event.shiftKey || event.isComposing) return;
    event.preventDefault();
    void submit();
  }

  function paste(event: ClipboardEvent) {
    const imageFiles = Array.from(event.clipboardData?.items ?? [])
      .filter((item) => item.kind === 'file' && item.type.startsWith('image/'))
      .map((item) => item.getAsFile())
      .filter((file): file is File => file !== null);
    if (imageFiles.length > 0) {
      event.preventDefault();
      addFiles(imageFiles);
    }
  }

  onMount(() => textarea?.focus());
  onDestroy(() => attachments.forEach(revokeAttachmentPreview));
</script>

<div class="composer-region">
  {#if notice || attachmentNotice}
    <div class="notice" role="status">
      <span>{attachmentNotice || notice}</span>
      <button
        aria-label="Dismiss message"
        onclick={() => {
          attachmentNotice = '';
          onDismissNotice();
        }}
      >
        <Icon name="x" size={13} />
      </button>
    </div>
  {/if}

  <div class="composer" role="group" aria-label="Message composer">
    <AttachmentTray {attachments} removable onRemove={removeAttachment} />

    {#if suggestions.length > 0}
      <div
        class="file-suggestions"
        id="file-suggestions"
        role="listbox"
        aria-label="Files in this chat"
      >
        {#each suggestions as reference, index (reference.attachment.id)}
          <button
            id={`file-suggestion-${index}`}
            role="option"
            aria-selected={index === mentionIndex}
            class:highlighted={index === mentionIndex}
            onmousedown={(event) => event.preventDefault()}
            onclick={() => selectReference(reference)}
          >
            <Icon name={reference.attachment.kind === 'image' ? 'image' : 'file'} size={16} />
            <span>{reference.label}</span>
          </button>
        {/each}
      </div>
    {/if}

    <textarea
      bind:this={textarea}
      bind:value={text}
      rows="1"
      aria-label="Message Blackwall"
      placeholder="Message Blackwall · @ to reference a file"
      aria-autocomplete="list"
      aria-controls={suggestions.length ? 'file-suggestions' : undefined}
      aria-activedescendant={suggestions.length ? `file-suggestion-${mentionIndex}` : undefined}
      oninput={input}
      onclick={updateMention}
      onkeyup={(event) => {
        if (['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) updateMention();
      }}
      onkeydown={keydown}
      onpaste={paste}></textarea>

    <div class="composer-actions">
      <div class="left-actions">
        <button
          class="attach"
          aria-label="Add photos or files"
          title="Add photos or files"
          onclick={chooseFiles}
          disabled={busy || submitting}
        >
          <Icon name="paperclip" size={18} />
        </button>
        <span class="local-hint">Sent only to your model</span>
      </div>

      {#if busy}
        <button
          class="send active"
          aria-label="Stop response"
          title="Stop response"
          onclick={onStop}
        >
          <Icon name="stop" size={16} />
        </button>
      {:else}
        <button
          class="send"
          class:active={canSend}
          aria-label="Send message"
          title="Send message"
          disabled={!canSend}
          onclick={submit}
        >
          <Icon name="arrow-up" size={18} strokeWidth={2.1} />
        </button>
      {/if}
    </div>

    <input
      class="sr-only"
      bind:this={fileInput}
      type="file"
      multiple
      accept="image/*,.pdf,.txt,.md,.csv,.json,.doc,.docx,.rtf,.html,.css,.js,.ts,.tsx,.jsx,.py,.rs,.go,.java,.c,.cpp,.h,.zip"
      onchange={selectedFiles}
      tabindex="-1"
    />
  </div>

  <p class="composer-caption">Enter to send <span>·</span> Shift + Enter for a new line</p>
</div>

<style>
  .composer-region {
    width: min(calc(100% - 32px), var(--composer-width));
    margin: 0 auto;
    padding: 10px 0 7px;
  }

  .composer {
    position: relative;
    overflow: hidden;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-lg);
    background: var(--bg-panel);
    box-shadow: var(--shadow-composer);
    transition:
      border-color var(--transition-fast),
      box-shadow var(--transition-fast);
  }

  .composer:focus-within {
    border-color: var(--accent);
    box-shadow:
      var(--shadow-composer),
      0 0 0 2px var(--focus-ring);
  }

  textarea {
    display: block;
    width: 100%;
    min-height: 52px;
    max-height: 180px;
    resize: none;
    overflow-y: auto;
    border: 0;
    padding: 15px 16px 7px;
    background: transparent;
    color: var(--text-primary);
    font-size: 14px;
    line-height: 1.5;
    outline: none;
  }

  textarea::placeholder {
    color: var(--text-faint);
  }

  .composer-actions {
    display: flex;
    height: 42px;
    align-items: center;
    justify-content: space-between;
    padding: 0 9px 8px 10px;
  }

  .left-actions {
    display: flex;
    min-width: 0;
    align-items: center;
    gap: 8px;
  }

  .attach,
  .send {
    display: grid;
    width: 32px;
    height: 32px;
    flex: 0 0 auto;
    place-items: center;
    border-radius: 9px;
    transition:
      color var(--transition-fast),
      background var(--transition-fast),
      transform var(--transition-fast);
  }

  .attach {
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-muted);
  }

  .attach:hover:not(:disabled) {
    border-color: var(--border-strong);
    background: var(--bg-hover);
    color: var(--text-primary);
  }

  .send {
    background: var(--bg-elevated);
    color: var(--text-faint);
  }

  .send.active {
    background: var(--text-primary);
    color: var(--bg-base);
  }

  .send.active:hover {
    transform: translateY(-1px);
  }

  .local-hint {
    overflow: hidden;
    color: var(--text-faint);
    font-size: 10.5px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .composer-caption {
    margin: 6px 0 0;
    color: var(--text-faint);
    font-size: 10.5px;
    text-align: center;
  }

  .composer-caption span {
    padding: 0 3px;
  }

  .notice {
    display: flex;
    max-width: calc(100% - 10px);
    min-height: 31px;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin: 0 auto 8px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 5px 7px 5px 10px;
    background: var(--bg-elevated);
    color: var(--text-muted);
    font-size: 11.5px;
  }

  .notice button {
    display: grid;
    width: 22px;
    height: 22px;
    flex: 0 0 auto;
    place-items: center;
    border-radius: 5px;
    background: transparent;
    color: var(--text-faint);
  }

  .notice button:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }

  .file-suggestions {
    max-height: 180px;
    overflow-y: auto;
    border-bottom: 1px solid var(--border);
    padding: 6px;
  }

  .file-suggestions button {
    display: flex;
    width: 100%;
    align-items: center;
    gap: 8px;
    border-radius: var(--radius-sm);
    padding: 8px 10px;
    background: transparent;
    color: var(--text-muted);
    text-align: left;
    font-size: 12px;
  }

  .file-suggestions button.highlighted,
  .file-suggestions button:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }

  .file-suggestions span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  @media (max-width: 560px) {
    .composer-region {
      width: calc(100% - 18px);
      padding-top: 7px;
    }

    .local-hint,
    .composer-caption {
      display: none;
    }

    textarea {
      min-height: 49px;
      padding-inline: 14px;
    }

    .composer-actions {
      padding-bottom: 7px;
    }
  }
</style>
