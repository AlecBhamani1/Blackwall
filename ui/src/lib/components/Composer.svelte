<script lang="ts">
  import { onDestroy, onMount, tick } from 'svelte';
  import { prepareAttachments, revokeAttachmentPreview } from '../attachments';
  import type { PendingAttachment } from '../types';
  import AttachmentTray from './AttachmentTray.svelte';
  import Icon from './Icon.svelte';

  export let busy = false;
  export let disabled = false;
  export let notice = '';
  export let onDismissNotice: () => void = () => undefined;
  export let onSend: (text: string, attachments: PendingAttachment[]) => Promise<boolean>;
  export let onStop: () => void;

  let text = '';
  let attachments: PendingAttachment[] = [];
  let fileInput: HTMLInputElement;
  let textarea: HTMLTextAreaElement;
  let dragDepth = 0;
  let dragging = false;
  let attachmentNotice = '';

  $: canSend = !disabled && !busy && Boolean(text.trim() || attachments.length);

  function resizeTextarea() {
    if (!textarea) return;
    textarea.style.height = 'auto';
    textarea.style.height = `${Math.min(textarea.scrollHeight, 180)}px`;
  }

  function addFiles(files: Iterable<File>) {
    const result = prepareAttachments(files, attachments);
    attachments = [...attachments, ...result.accepted];
    attachmentNotice = result.rejected.map((item) => `${item.fileName}: ${item.reason}`).join(' · ');
  }

  function chooseFiles() {
    if (!busy) fileInput.click();
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
    const accepted = await onSend(text, submitted);
    if (!accepted) return;
    text = '';
    attachments = [];
    attachmentNotice = '';
    await tick();
    resizeTextarea();
    textarea?.focus();
  }

  function keydown(event: KeyboardEvent) {
    if (event.key !== 'Enter' || event.shiftKey || event.isComposing) return;
    event.preventDefault();
    void submit();
  }

  function paste(event: ClipboardEvent) {
    const imageFiles = Array.from(event.clipboardData?.items ?? [])
      .filter((item) => item.kind === 'file' && item.type.startsWith('image/'))
      .map((item) => item.getAsFile())
      .filter((file): file is File => file !== null);
    if (imageFiles.length > 0) addFiles(imageFiles);
  }

  function dragEnter(event: DragEvent) {
    if (!event.dataTransfer?.types.includes('Files')) return;
    event.preventDefault();
    dragDepth += 1;
    dragging = true;
  }

  function dragOver(event: DragEvent) {
    if (!event.dataTransfer?.types.includes('Files')) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = 'copy';
  }

  function dragLeave(event: DragEvent) {
    event.preventDefault();
    dragDepth = Math.max(0, dragDepth - 1);
    if (dragDepth === 0) dragging = false;
  }

  function drop(event: DragEvent) {
    event.preventDefault();
    dragDepth = 0;
    dragging = false;
    if (event.dataTransfer?.files) addFiles(event.dataTransfer.files);
  }

  onMount(() => textarea?.focus());
  onDestroy(() => attachments.forEach(revokeAttachmentPreview));
</script>

<div class="composer-region">
  {#if notice || attachmentNotice}
    <div class="notice" role="status">
      <span>{attachmentNotice || notice}</span>
      <button aria-label="Dismiss message" onclick={() => { attachmentNotice = ''; onDismissNotice(); }}>
        <Icon name="x" size={13} />
      </button>
    </div>
  {/if}

  <div
    class:dragging
    class="composer"
    role="group"
    aria-label="Message composer"
    ondragenter={dragEnter}
    ondragover={dragOver}
    ondragleave={dragLeave}
    ondrop={drop}
  >
    {#if dragging}
      <div class="drop-prompt"><Icon name="paperclip" size={17} /> Drop files here</div>
    {/if}

    <AttachmentTray {attachments} removable onRemove={removeAttachment} />

    <textarea
      bind:this={textarea}
      bind:value={text}
      rows="1"
      aria-label="Message Blackwall"
      placeholder="Message Blackwall"
      oninput={resizeTextarea}
      onkeydown={keydown}
      onpaste={paste}
    ></textarea>

    <div class="composer-actions">
      <div class="left-actions">
        <button class="attach" aria-label="Add photos or files" title="Add photos or files" onclick={chooseFiles} disabled={busy}>
          <Icon name="paperclip" size={18} />
        </button>
        <span class="local-hint">Sent only to your model</span>
      </div>

      {#if busy}
        <button class="send active" aria-label="Stop response" title="Stop response" onclick={onStop}>
          <Icon name="stop" size={16} />
        </button>
      {:else}
        <button class="send" class:active={canSend} aria-label="Send message" title="Send message" disabled={!canSend} onclick={submit}>
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

  .composer:focus-within,
  .composer.dragging {
    border-color: var(--accent);
    box-shadow: var(--shadow-composer), 0 0 0 2px var(--focus-ring);
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

  .drop-prompt {
    position: absolute;
    z-index: 5;
    inset: 5px;
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 8px;
    border: 1px dashed var(--accent);
    border-radius: calc(var(--radius-lg) - 3px);
    background: var(--bg-panel);
    color: var(--text-primary);
    font-weight: 550;
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
