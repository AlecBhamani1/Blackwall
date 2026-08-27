<script lang="ts">
  import type { ConnectionState } from '../types';
  import LogoMark from './LogoMark.svelte';

  export let connectionState: ConnectionState = 'checking';
</script>

<div class="empty-state">
  <div class="empty-mark"><LogoMark size={40} /></div>
  <h1>What can I help with?</h1>
  <p>
    {#if connectionState === 'ready'}
      Chat with your model. Add a photo or file whenever it helps.
    {:else if connectionState === 'checking'}
      Connecting to your local model…
    {:else}
      Connect your Ollama host to begin. You can still compose below.
    {/if}
  </p>
  <div class="local-note">
    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
      <rect width="16" height="11" x="4" y="11" rx="2" />
      <path d="M8 11V7a4 4 0 0 1 8 0v4" />
    </svg>
    Private by default · your model endpoint
  </div>
</div>

<style>
  .empty-state {
    display: flex;
    width: min(100%, 540px);
    flex-direction: column;
    align-items: center;
    margin: auto;
    padding: 40px 18px 78px;
    text-align: center;
  }

  .empty-mark {
    margin-bottom: 17px;
  }

  h1 {
    margin: 0;
    color: var(--text-primary);
    font-size: clamp(22px, 2.4vw, 28px);
    font-weight: 630;
    letter-spacing: -0.035em;
    line-height: 1.2;
  }

  p {
    max-width: 430px;
    margin: 10px 0 0;
    color: var(--text-muted);
    font-size: 13px;
  }

  .local-note {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-top: 17px;
    color: var(--text-faint);
    font-size: 10.5px;
  }

  @media (max-height: 700px) {
    .empty-state {
      padding-bottom: 30px;
    }
  }
</style>
