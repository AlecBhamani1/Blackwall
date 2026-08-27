<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import AppHeader from './lib/components/AppHeader.svelte';
  import ChatView from './lib/components/ChatView.svelte';
  import Composer from './lib/components/Composer.svelte';
  import Sidebar from './lib/components/Sidebar.svelte';
  import StatusBar from './lib/components/StatusBar.svelte';
  import { createChatController } from './lib/store';

  const controller = createChatController();
  const {
    messages,
    sessions,
    activeSessionId,
    runState,
    connectionState,
    models,
    selectedModel,
    contextPercent,
    notice,
  } = controller;

  let sidebarVisible = typeof window === 'undefined' ? true : window.innerWidth > 760;
  const showHarnessDemo =
    typeof window !== 'undefined' && new URLSearchParams(window.location.search).has('demo');

  function closeSidebarOnMobile() {
    if (window.innerWidth <= 760) sidebarVisible = false;
  }

  function newChat() {
    controller.newChat();
    closeSidebarOnMobile();
  }

  function openSession(sessionId: string) {
    controller.openSession(sessionId);
    closeSidebarOnMobile();
  }

  function keydown(event: KeyboardEvent) {
    if (!(event.metaKey || event.ctrlKey)) {
      if (event.key === 'Escape' && ($runState === 'streaming' || $runState === 'preparing')) {
        controller.stop();
      }
      return;
    }

    if (event.key.toLowerCase() === 'n') {
      event.preventDefault();
      newChat();
    }
    if (event.key.toLowerCase() === 'b') {
      event.preventDefault();
      sidebarVisible = !sidebarVisible;
    }
  }

  onMount(() => {
    void controller.initialize();
    window.addEventListener('keydown', keydown);
  });

  onDestroy(() => {
    window.removeEventListener('keydown', keydown);
    controller.destroy();
  });
</script>

<div class="app-shell">
  <Sidebar
    visible={sidebarVisible}
    sessions={$sessions}
    activeSessionId={$activeSessionId}
    connectionState={$connectionState}
    selectedModel={$selectedModel}
    onNewChat={newChat}
    onOpenSession={openSession}
    onRemoveSession={controller.removeSession}
    onClose={() => (sidebarVisible = false)}
  />

  {#if sidebarVisible}
    <button class="mobile-scrim" aria-label="Close sidebar" onclick={() => (sidebarVisible = false)}></button>
  {/if}

  <main>
    <AppHeader
      {sidebarVisible}
      models={$models}
      selectedModel={$selectedModel}
      connectionState={$connectionState}
      onToggleSidebar={() => (sidebarVisible = !sidebarVisible)}
      onModelChange={controller.chooseModel}
      onReconnect={controller.initialize}
    />

    <section class="conversation-pane" aria-label="Blackwall chat">
      <ChatView messages={$messages} connectionState={$connectionState} {showHarnessDemo} />
      <Composer
        busy={$runState === 'streaming' || $runState === 'preparing'}
        disabled={$connectionState !== 'ready'}
        notice={$notice}
        onDismissNotice={controller.dismissNotice}
        onSend={controller.send}
        onStop={controller.stop}
      />
    </section>

    <StatusBar
      connectionState={$connectionState}
      runState={$runState}
      model={$selectedModel}
      contextPercent={$contextPercent}
    />
  </main>
</div>

<style>
  .app-shell {
    position: relative;
    display: flex;
    width: 100%;
    height: 100%;
    overflow: hidden;
    background: var(--bg-base);
  }

  main {
    display: flex;
    min-width: 0;
    height: 100%;
    flex: 1;
    flex-direction: column;
  }

  .conversation-pane {
    display: flex;
    min-height: 0;
    flex: 1;
    flex-direction: column;
  }

  .mobile-scrim {
    display: none;
  }

  @media (max-width: 760px) {
    .mobile-scrim {
      position: absolute;
      z-index: 15;
      inset: 0;
      display: block;
      background: var(--bg-scrim);
    }
  }
</style>
