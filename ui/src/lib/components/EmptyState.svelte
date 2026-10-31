<script lang="ts">
  import type { ConnectionState } from '../types';
  import LogoMark from './LogoMark.svelte';
  import Icon from './Icon.svelte';

  export let connectionState: ConnectionState = 'checking';
  export let needsFolder = false;
  export let onChooseFolder: () => void = () => {};
</script>

<div class="empty-state">
  <div class="empty-mark"><LogoMark size={40} /></div>
  <h1>What can I help with?</h1>
  <p>
    {#if connectionState === 'ready' && needsFolder}
      Choose a project folder to begin. Agent works only inside the folder you pick.
    {:else if connectionState === 'ready'}
      Chat with your model. Add a photo or file whenever it helps.
    {:else if connectionState === 'checking'}
      Connecting to your model…
    {:else}
      Set up a connection to begin. You can still write your message below.
    {/if}
  </p>
  {#if connectionState === 'ready' && needsFolder}
    <button class="choose-folder" onclick={onChooseFolder}>Choose a project folder</button>
  {/if}
  <div class="local-note">
    <Icon name="link" size={13} />
    Chats go to your selected model
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

  .choose-folder {
    margin-top: 16px;
    padding: 9px 16px;
    border-radius: var(--radius-md);
    background: var(--accent);
    color: var(--text-on-accent);
    font-size: 13px;
    font-weight: 600;
  }

  .choose-folder:hover {
    background: var(--accent-hover);
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
