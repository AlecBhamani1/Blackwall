<script lang="ts">
  import Icon from './Icon.svelte';

  export let onFiles: (files: File[]) => void;
  export let busy = false;
  let dragDepth = 0;
  let dragging = false;

  function isFileDrag(event: DragEvent) {
    return Array.from(event.dataTransfer?.types ?? []).includes('Files');
  }

  function dragEnter(event: DragEvent) {
    if (!isFileDrag(event)) return;
    event.preventDefault();
    dragDepth += 1;
    dragging = true;
  }

  function dragOver(event: DragEvent) {
    if (!isFileDrag(event)) return;
    event.preventDefault();
    if (event.dataTransfer) event.dataTransfer.dropEffect = busy ? 'none' : 'copy';
  }

  function dragLeave(event: DragEvent) {
    if (!isFileDrag(event)) return;
    dragDepth = Math.max(0, dragDepth - 1);
    if (dragDepth === 0) dragging = false;
  }

  function drop(event: DragEvent) {
    if (!isFileDrag(event)) return;
    event.preventDefault();
    dragDepth = 0;
    dragging = false;
    if (!busy && event.dataTransfer) onFiles(Array.from(event.dataTransfer.files));
  }
</script>

<section
  class="chat-drop-zone"
  aria-label="Blackwall chat"
  ondragenter={dragEnter}
  ondragover={dragOver}
  ondragleave={dragLeave}
  ondrop={drop}
>
  <slot />
  {#if dragging}
    <div class="drop-prompt" role="status">
      <Icon name="paperclip" size={20} />
      {busy
        ? 'Wait for the response to finish before adding files'
        : 'Drop screenshots or files to attach'}
    </div>
  {/if}
</section>

<style>
  .chat-drop-zone {
    position: relative;
    display: flex;
    min-height: 0;
    flex: 1;
    flex-direction: column;
  }

  .drop-prompt {
    position: absolute;
    z-index: 5;
    inset: 12px;
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 10px;
    border: 1px dashed var(--accent);
    border-radius: var(--radius-lg);
    background: var(--bg-panel);
    color: var(--text-primary);
    pointer-events: none;
  }
</style>
