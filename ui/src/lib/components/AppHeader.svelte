<script lang="ts">
  import Icon from './Icon.svelte';

  export let setupMode = false;
  export let sidebarVisible = true;
  export let conversationTitle = 'New chat';
  export let agentMode = false;
  export let workspace = '';
  export let folderPulse = false;
  export let projectDisabled = false;
  export let filesVisible = true;
  export let canExport = false;
  export let onToggleSidebar: () => void;
  export let onChooseWorkspace: () => void;
  export let onFolderPulseEnd: () => void;
  export let onToggleFiles: () => void;
  export let onExport: () => void;
  export let onSearch: () => void;
</script>

<header aria-label="Conversation header">
  <button
    class="icon-button"
    aria-label={sidebarVisible ? 'Hide sidebar' : 'Show sidebar'}
    onclick={onToggleSidebar}
  >
    <Icon name={sidebarVisible ? 'panel-left-open' : 'panel-left'} size={18} />
  </button>
  <strong class="conversation-title">{setupMode ? 'Setup' : conversationTitle}</strong>
  {#if !setupMode && agentMode}
    <button
      class="project-choice"
      class:needs-folder={!workspace}
      class:pulse={folderPulse}
      onanimationend={onFolderPulseEnd}
      disabled={projectDisabled}
      title={workspace || 'Choose the folder Blackwall can work in'}
      onclick={onChooseWorkspace}
      >{workspace ? workspace.split('/').filter(Boolean).at(-1) : 'Choose project folder'}</button
    >
  {/if}
  <span class="spacer"></span>
  {#if !setupMode}
    {#if canExport}<button class="header-action" onclick={onExport}>Export chat</button>{/if}
    <button class="header-action" onclick={onSearch}>Search history</button>
    {#if agentMode}
      <button
        class="icon-button"
        aria-label={filesVisible ? 'Hide files' : 'Show files'}
        title={filesVisible ? 'Hide files' : 'Show files'}
        aria-pressed={filesVisible}
        onclick={onToggleFiles}
      >
        <Icon name={filesVisible ? 'panel-right-open' : 'panel-right'} size={16} />
      </button>
    {/if}
  {/if}
</header>

<style>
  header {
    position: relative;
    z-index: 5;
    display: flex;
    height: var(--topbar-height);
    min-height: var(--topbar-height);
    align-items: center;
    gap: 10px;
    border-bottom: 1px solid var(--border);
    padding: 0 14px;
    background: var(--bg-base);
  }
  .icon-button {
    display: grid;
    width: 32px;
    height: 32px;
    flex: 0 0 auto;
    place-items: center;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-muted);
  }
  .icon-button:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  .conversation-title {
    max-width: 240px;
    overflow: hidden;
    color: var(--text-primary);
    font-size: 12px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .spacer {
    flex: 1;
  }
  .project-choice,
  .header-action {
    max-width: 250px;
    overflow: hidden;
    padding: 4px;
    background: transparent;
    color: var(--text-muted);
    font-size: 11px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .project-choice.needs-folder {
    border-radius: 6px;
    outline: 2px solid var(--accent);
    outline-offset: 1px;
    color: var(--text-primary);
  }
  .project-choice.pulse {
    animation: folder-pulse 700ms ease-out 2;
  }
  @keyframes folder-pulse {
    50% {
      outline-offset: 5px;
      outline-color: transparent;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .project-choice.pulse {
      animation: none;
    }
  }
</style>
