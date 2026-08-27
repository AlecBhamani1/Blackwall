<script lang="ts">
  import { tick } from 'svelte';
  import { relativeTime } from '../format';
  import type { ConnectionState, SessionSummary } from '../types';
  import Icon from './Icon.svelte';
  import LogoMark from './LogoMark.svelte';
  import SettingsDialog from './SettingsDialog.svelte';

  export let visible = true;
  export let sessions: SessionSummary[] = [];
  export let activeSessionId = '';
  export let connectionState: ConnectionState = 'checking';
  export let selectedModel = '';
  export let onNewChat: () => void;
  export let onOpenSession: (sessionId: string) => void;
  export let onRemoveSession: (sessionId: string) => void;
  export let onClose: () => void;

  let settingsOpen = false;
  let settingsButton: HTMLButtonElement;

  function remove(event: MouseEvent, sessionId: string) {
    event.stopPropagation();
    onRemoveSession(sessionId);
  }

  function closeSettings() {
    settingsOpen = false;
    void tick().then(() => settingsButton?.focus());
  }
</script>

<aside class:visible aria-label="Chat history">
  <div class="brand-row">
    <div class="brand">
      <LogoMark size={27} />
      <span>Blackwall</span>
    </div>
    <button class="icon-button close-sidebar" aria-label="Close sidebar" onclick={onClose}>
      <Icon name="x" size={17} />
    </button>
  </div>

  <button class="new-chat" onclick={onNewChat}>
    <Icon name="new-chat" size={16} />
    <span>New chat</span>
    <kbd>⌘N</kbd>
  </button>

  <section class="history">
    <div class="section-label">Recent</div>
    {#if sessions.length === 0}
      <p class="empty-history">Your local chats will appear here.</p>
    {:else}
      <div class="session-list">
        {#each sessions as session (session.id)}
          <div class:active={session.id === activeSessionId} class="session-row">
            <button class="session-main" onclick={() => onOpenSession(session.id)}>
              <span class="session-title">{session.title}</span>
              <span class="session-time">{relativeTime(session.updatedAt)}</span>
            </button>
            <button
              class="delete-session"
              aria-label={`Delete ${session.title}`}
              onclick={(event) => remove(event, session.id)}
            >
              <Icon name="trash" size={14} />
            </button>
          </div>
        {/each}
      </div>
    {/if}
  </section>

  <div class="sidebar-footer">
    <button
      class="settings-button"
      aria-label="Open settings"
      aria-haspopup="dialog"
      aria-expanded={settingsOpen}
      onclick={() => (settingsOpen = true)}
      bind:this={settingsButton}
    >
      <Icon name="gear" size={17} />
    </button>
    <div class="connection-label">
      <span class:ready={connectionState === 'ready'} class:offline={connectionState === 'offline'} class="dot"></span>
      <span>{connectionState === 'ready' ? 'Model connected' : connectionState === 'checking' ? 'Checking endpoint' : 'Model offline'}</span>
    </div>
  </div>
</aside>

<SettingsDialog open={settingsOpen} model={selectedModel} onClose={closeSettings} />

<style>
  aside {
    position: relative;
    z-index: 20;
    display: flex;
    width: var(--sidebar-width);
    min-width: var(--sidebar-width);
    height: 100%;
    flex-direction: column;
    overflow: hidden;
    border-right: 1px solid var(--border);
    background: var(--bg-panel);
    transition:
      width var(--transition-fast),
      min-width var(--transition-fast),
      transform var(--transition-fast);
  }

  aside:not(.visible) {
    width: 0;
    min-width: 0;
    border-right: 0;
  }

  .brand-row {
    display: flex;
    height: var(--topbar-height);
    align-items: center;
    justify-content: space-between;
    padding: 0 13px 0 15px;
  }

  .brand {
    display: flex;
    align-items: center;
    gap: 9px;
    color: var(--text-primary);
    font-size: 14px;
    font-weight: 650;
    letter-spacing: -0.01em;
  }

  .icon-button {
    display: grid;
    width: 30px;
    height: 30px;
    place-items: center;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-muted);
  }

  .icon-button:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }

  .close-sidebar {
    display: none;
  }

  .new-chat {
    display: flex;
    min-height: 38px;
    align-items: center;
    gap: 9px;
    margin: 8px 10px 18px;
    padding: 0 10px;
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    background: var(--bg-elevated);
    color: var(--text-primary);
    text-align: left;
    transition:
      border-color var(--transition-fast),
      background var(--transition-fast);
  }

  .new-chat:hover {
    border-color: var(--border-strong);
    background: var(--bg-hover);
  }

  .new-chat span {
    flex: 1;
    font-weight: 540;
  }

  kbd {
    color: var(--text-faint);
    font-family: var(--font-ui);
    font-size: 10px;
  }

  .history {
    min-height: 0;
    flex: 1;
    overflow-y: auto;
    padding: 0 8px;
  }

  .section-label {
    padding: 0 8px 7px;
    color: var(--text-faint);
    font-size: 10.5px;
    font-weight: 650;
    letter-spacing: 0.08em;
    text-transform: uppercase;
  }

  .empty-history {
    margin: 4px 8px;
    color: var(--text-faint);
    font-size: 12px;
    line-height: 1.5;
  }

  .session-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .session-row {
    position: relative;
    display: flex;
    align-items: center;
    border-radius: var(--radius-sm);
  }

  .session-row:hover,
  .session-row.active {
    background: var(--bg-hover);
  }

  .session-row.active::before {
    position: absolute;
    left: 0;
    width: 2px;
    height: 16px;
    content: '';
    border-radius: var(--radius-pill);
    background: var(--accent);
  }

  .session-main {
    display: flex;
    min-width: 0;
    min-height: 36px;
    flex: 1;
    align-items: center;
    gap: 8px;
    padding: 0 29px 0 9px;
    background: transparent;
    color: var(--text-muted);
    text-align: left;
  }

  .session-row.active .session-main,
  .session-row:hover .session-main {
    color: var(--text-primary);
  }

  .session-title {
    min-width: 0;
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .session-time {
    color: var(--text-faint);
    font-size: 10.5px;
  }

  .delete-session {
    position: absolute;
    right: 5px;
    display: none;
    width: 25px;
    height: 25px;
    place-items: center;
    border-radius: 5px;
    background: var(--bg-hover);
    color: var(--text-faint);
  }

  .session-row:hover .delete-session,
  .delete-session:focus-visible {
    display: grid;
  }

  .delete-session:hover {
    color: var(--err);
  }

  .sidebar-footer {
    display: flex;
    min-height: 48px;
    align-items: center;
    gap: 8px;
    border-top: 1px solid var(--border);
    padding: 0 10px;
    color: var(--text-faint);
    font-size: 11.5px;
  }

  .settings-button {
    display: grid;
    width: 30px;
    height: 30px;
    flex: 0 0 auto;
    place-items: center;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-muted);
  }

  .settings-button:hover,
  .settings-button[aria-expanded='true'] {
    background: var(--bg-hover);
    color: var(--text-primary);
  }

  .connection-label {
    display: flex;
    min-width: 0;
    align-items: center;
    gap: 7px;
  }

  .connection-label > span:last-child {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--warn);
  }

  .dot.ready {
    background: var(--ok);
  }

  .dot.offline {
    background: var(--err);
  }

  @media (max-width: 760px) {
    aside {
      position: absolute;
      top: 0;
      left: 0;
      width: min(var(--sidebar-width), 86vw);
      min-width: min(var(--sidebar-width), 86vw);
      box-shadow: var(--shadow-menu);
    }

    aside:not(.visible) {
      width: min(var(--sidebar-width), 86vw);
      min-width: min(var(--sidebar-width), 86vw);
      transform: translateX(-102%);
    }

    .close-sidebar {
      display: grid;
    }
  }
</style>
