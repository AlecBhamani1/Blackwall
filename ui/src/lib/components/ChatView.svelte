<script lang="ts">
  import { onMount, tick } from 'svelte';
  import type { ChatMessage, ConnectionState } from '../types';
  import DemoConversation from './DemoConversation.svelte';
  import EmptyState from './EmptyState.svelte';
  import MessageCell from './MessageCell.svelte';

  export let messages: ChatMessage[] = [];
  export let connectionState: ConnectionState = 'checking';
  export let showHarnessDemo = false;

  let scroller: HTMLDivElement;
  let followsTail = true;

  function onScroll() {
    if (!scroller) return;
    followsTail = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 120;
  }

  async function followMessages() {
    if (!followsTail || !scroller) return;
    await tick();
    scroller.scrollTop = scroller.scrollHeight;
  }

  $: if (messages) void followMessages();
  onMount(() => void followMessages());
</script>

<div class="chat-scroll" bind:this={scroller} onscroll={onScroll}>
  {#if messages.length === 0 && !showHarnessDemo}
    <EmptyState {connectionState} />
  {:else}
    <div class="transcript" role="log" aria-label="Conversation" aria-live="polite">
      {#if showHarnessDemo && messages.length === 0}
        <DemoConversation />
      {:else}
        {#each messages as message (message.id)}
          <MessageCell {message} />
        {/each}
      {/if}
    </div>
  {/if}
</div>

<style>
  .chat-scroll {
    display: flex;
    min-height: 0;
    flex: 1;
    overflow-y: auto;
    overscroll-behavior: contain;
    scroll-behavior: smooth;
  }

  .transcript {
    display: flex;
    width: min(calc(100% - 36px), var(--transcript-width));
    flex-direction: column;
    gap: 28px;
    margin: 0 auto;
    padding: 34px 0 44px;
  }

  @media (max-width: 560px) {
    .transcript {
      width: calc(100% - 24px);
      gap: 24px;
      padding: 24px 0 32px;
    }
  }
</style>
