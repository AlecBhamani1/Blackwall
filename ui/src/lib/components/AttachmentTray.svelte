<script lang="ts">
  import { formatBytes } from '../format';
  import type { Attachment } from '../types';
  import Icon from './Icon.svelte';

  export let attachments: Attachment[] = [];
  export let removable = false;
  export let onRemove: (attachmentId: string) => void = () => undefined;
</script>

{#if attachments.length > 0}
  <div class:compact={!removable} class="attachment-tray" aria-label="Attachments">
    {#each attachments as attachment (attachment.id)}
      <div class:image-card={attachment.kind === 'image' && (attachment.previewUrl || attachment.dataUrl)} class="attachment">
        {#if attachment.kind === 'image' && (attachment.previewUrl || attachment.dataUrl)}
          <img src={attachment.previewUrl || attachment.dataUrl} alt={`Preview of ${attachment.name}`} />
          <div class="image-label" title={attachment.name}>{attachment.name}</div>
        {:else}
          <div class="file-icon">
            <Icon name={attachment.kind === 'image' ? 'image' : 'file'} size={18} />
          </div>
          <div class="file-meta">
            <span class="file-name" title={attachment.name}>{attachment.name}</span>
            <span class="file-size">{formatBytes(attachment.sizeBytes)}</span>
          </div>
        {/if}
        {#if removable}
          <button class="remove" aria-label={`Remove ${attachment.name}`} onclick={() => onRemove(attachment.id)}>
            <Icon name="x" size={12} strokeWidth={2.1} />
          </button>
        {/if}
      </div>
    {/each}
  </div>
{/if}

<style>
  .attachment-tray {
    display: flex;
    gap: 8px;
    overflow-x: auto;
    padding: 10px 11px 3px;
    scrollbar-width: thin;
  }

  .attachment-tray.compact {
    flex-wrap: wrap;
    overflow: visible;
    padding: 0 0 9px;
  }

  .attachment {
    position: relative;
    display: flex;
    width: 190px;
    height: 54px;
    flex: 0 0 auto;
    align-items: center;
    gap: 9px;
    overflow: hidden;
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    background: var(--bg-elevated);
  }

  .attachment.image-card {
    width: 82px;
    height: 70px;
  }

  .file-icon {
    display: grid;
    width: 34px;
    height: 34px;
    flex: 0 0 auto;
    place-items: center;
    margin-left: 9px;
    border-radius: var(--radius-sm);
    background: var(--bg-user-msg);
    color: var(--text-muted);
  }

  .file-meta {
    display: flex;
    min-width: 0;
    flex: 1;
    flex-direction: column;
    padding-right: 9px;
  }

  .file-name,
  .file-size {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .file-name {
    color: var(--text-primary);
    font-size: 11.5px;
    font-weight: 550;
  }

  .file-size {
    color: var(--text-faint);
    font-size: 10.5px;
  }

  img {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }

  .image-label {
    position: absolute;
    right: 0;
    bottom: 0;
    left: 0;
    overflow: hidden;
    padding: 9px 6px 4px;
    background: linear-gradient(transparent, var(--image-scrim));
    color: var(--text-primary);
    font-size: 9.5px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .remove {
    position: absolute;
    top: 4px;
    right: 4px;
    display: grid;
    width: 19px;
    height: 19px;
    place-items: center;
    border: 1px solid var(--border-strong);
    border-radius: 50%;
    background: var(--bg-panel);
    color: var(--text-muted);
    opacity: 0;
    transition: opacity var(--transition-fast);
  }

  .attachment:hover .remove,
  .remove:focus-visible {
    opacity: 1;
  }

  .remove:hover {
    color: var(--text-primary);
  }

  @media (max-width: 560px) {
    .attachment {
      width: 160px;
    }

    .remove {
      opacity: 1;
    }
  }
</style>
