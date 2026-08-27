<script lang="ts">
  import { renderMarkdown } from '../markdown';
  import type { ChatMessage } from '../types';
  import AttachmentTray from './AttachmentTray.svelte';
  import Icon from './Icon.svelte';
  import LogoMark from './LogoMark.svelte';

  export let message: ChatMessage;
  let copied = false;

  $: rendered = message.role === 'assistant' && message.content ? renderMarkdown(message.content) : '';

  async function copyMessage() {
    if (!message.content || !navigator.clipboard) return;
    await navigator.clipboard.writeText(message.content);
    copied = true;
    window.setTimeout(() => (copied = false), 1_500);
  }
</script>

<article class:user={message.role === 'user'} class:assistant={message.role === 'assistant'}>
  {#if message.role === 'assistant'}
    <div class="assistant-avatar"><LogoMark size={24} /></div>
  {/if}

  <div class="message-body">
    <AttachmentTray attachments={message.attachments} />

    {#if message.role === 'user'}
      {#if message.content}<div class="user-content">{message.content}</div>{/if}
    {:else if message.content}
      <div class="markdown">{@html rendered}</div>
    {/if}

    {#if message.status === 'streaming'}
      <span class="streaming-cursor" aria-label="Blackwall is responding"></span>
    {/if}

    {#if message.error}
      <div class="message-error" role="alert">
        <span>{message.error}</span>
      </div>
    {/if}

    {#if message.role === 'assistant' && message.content && message.status !== 'streaming'}
      <div class="message-actions">
        <button aria-label="Copy response" title="Copy response" onclick={copyMessage}>
          {#if copied}
            <Icon name="check" size={14} />
          {:else}
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" aria-hidden="true">
              <rect width="14" height="14" x="8" y="8" rx="2" />
              <path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2" />
            </svg>
          {/if}
        </button>
      </div>
    {/if}
  </div>
</article>

<style>
  article {
    display: flex;
    width: 100%;
    gap: 11px;
  }

  article.user {
    justify-content: flex-end;
  }

  .assistant-avatar {
    flex: 0 0 auto;
    padding-top: 1px;
  }

  .message-body {
    min-width: 0;
    max-width: 100%;
  }

  .user .message-body {
    max-width: min(82%, 620px);
    border: 1px solid var(--border);
    border-radius: var(--radius-lg) var(--radius-lg) 5px var(--radius-lg);
    padding: 11px 14px;
    background: var(--bg-user-msg);
  }

  .user .message-body :global(.attachment-tray) {
    padding-bottom: 10px;
  }

  .assistant .message-body {
    flex: 1;
    padding-top: 1px;
    color: var(--text-primary);
  }

  .user-content {
    overflow-wrap: anywhere;
    color: var(--text-primary);
    line-height: 1.55;
    white-space: pre-wrap;
  }

  .markdown {
    max-width: 100%;
    overflow-wrap: anywhere;
    color: var(--text-primary);
    font-size: 14px;
    line-height: 1.68;
  }

  .markdown :global(> :first-child) {
    margin-top: 0;
  }

  .markdown :global(> :last-child) {
    margin-bottom: 0;
  }

  .markdown :global(p) {
    margin: 0 0 12px;
  }

  .markdown :global(h1),
  .markdown :global(h2),
  .markdown :global(h3) {
    margin: 20px 0 8px;
    color: var(--text-primary);
    font-weight: 700;
    line-height: 1.3;
  }

  .markdown :global(h1) {
    font-size: 17px;
  }

  .markdown :global(h2) {
    font-size: 15.5px;
  }

  .markdown :global(h3) {
    font-size: 14px;
  }

  .markdown :global(ul),
  .markdown :global(ol) {
    margin: 8px 0 13px;
    padding-left: 23px;
  }

  .markdown :global(li) {
    margin: 4px 0;
  }

  .markdown :global(a) {
    color: var(--accent);
    text-decoration: none;
  }

  .markdown :global(a:hover) {
    text-decoration: underline;
  }

  .markdown :global(code) {
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 1px 4px;
    background: var(--bg-elevated);
    color: var(--text-primary);
    font-family: var(--font-code);
    font-size: 12.5px;
  }

  .markdown :global(pre) {
    max-width: 100%;
    overflow-x: auto;
    margin: 12px 0;
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 13px 14px;
    background: var(--bg-elevated);
  }

  .markdown :global(pre code) {
    border: 0;
    padding: 0;
    background: transparent;
  }

  .markdown :global(blockquote) {
    margin: 12px 0;
    border-left: 2px solid var(--border-strong);
    padding-left: 12px;
    color: var(--text-muted);
  }

  .streaming-cursor {
    display: inline-block;
    width: 7px;
    height: 14px;
    margin-left: 2px;
    border-radius: 1px;
    background: var(--accent);
    vertical-align: -2px;
    animation: cursor-pulse 900ms ease-in-out infinite;
  }

  .message-error {
    margin-top: 9px;
    border-left: 2px solid var(--err);
    padding: 5px 9px;
    color: var(--text-muted);
    font-size: 12px;
  }

  .message-actions {
    height: 27px;
    margin-top: 3px;
    opacity: 0;
    transition: opacity var(--transition-fast);
  }

  article:hover .message-actions,
  .message-actions:focus-within {
    opacity: 1;
  }

  .message-actions button {
    display: grid;
    width: 27px;
    height: 27px;
    place-items: center;
    border-radius: 6px;
    background: transparent;
    color: var(--text-faint);
  }

  .message-actions button:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }

  @keyframes cursor-pulse {
    0%,
    100% {
      opacity: 0.28;
    }
    50% {
      opacity: 1;
    }
  }

  @media (max-width: 560px) {
    .user .message-body {
      max-width: 91%;
    }

    .assistant-avatar {
      display: none;
    }

    .message-actions {
      opacity: 1;
    }
  }
</style>
