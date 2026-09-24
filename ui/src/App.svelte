<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { startPairing, stopPairing } from './lib/pairing';
  import { authClient, authError, authStatus } from './lib/auth';
  import LockScreen from './lib/components/LockScreen.svelte';
  import ApprovalCard from './lib/components/ApprovalCard.svelte';
  import { isDesktop } from './lib/setup';
  import ConnectionSetup from './lib/components/ConnectionSetup.svelte';
  import AppHeader from './lib/components/AppHeader.svelte';
  import ChatView from './lib/components/ChatView.svelte';
  import Composer from './lib/components/Composer.svelte';
  import Sidebar from './lib/components/Sidebar.svelte';
  import StatusBar from './lib/components/StatusBar.svelte';
  import { createChatController } from './lib/store';
  import { createAppUpdateController } from './lib/updates';

  const controller = createChatController();
  const updateController = createAppUpdateController();
  const {
    messages,
    sessions,
    activeSessionId,
    runState,
    connectionState,
    models,
    selectedModel,
    endpoint,
    contextPercent,
    notice,
    connectionError,
    persistenceError,
    agentMode,
    webEnabled,
    workspace,
    approval,
    preferences,
  } = controller;
  const {
    state: updateState,
    currentVersion,
    update: availableUpdate,
    progress: updateProgress,
    error: updateError,
  } = updateController;

  let booted = !isDesktop();
  let authInitialized = false;
  let authBusy = false;
  let lockError = '';
  async function initializeApp() {
    authBusy = true;
    lockError = '';
    try {
      if (isDesktop()) {
        const status = await authClient.status();
        authStatus.set(status);
        authInitialized = true;
        if (status.locked) return;
      }
      booted = true;
      startPairing();
      const connected = await controller.initialize();
      if (!connected && !showHarnessDemo) setupOpen = true;
    } catch (cause) {
      lockError = authError(cause);
    } finally {
      authBusy = false;
    }
  }
  async function unlock(passphrase: string) {
    authBusy = true;
    lockError = '';
    try {
      authStatus.set(await authClient.unlock(passphrase));
      await initializeApp();
    } catch (cause) {
      lockError = authError(cause);
    } finally {
      authBusy = false;
    }
  }
  async function lock() {
    if (authBusy) return;
    authBusy = true;
    try {
      await controller.suspend();
      booted = false;
      stopPairing();
      authStatus.set(await authClient.lock());
    } catch (cause) {
      const message = authError(cause);
      if (!booted) await initializeApp();
      controller.notice.set(message);
      throw new Error(message);
    } finally {
      authBusy = false;
    }
  }
  let setupOpen = false;
  let sidebarVisible = typeof window === 'undefined' ? true : window.innerWidth > 760;
  const showHarnessDemo =
    typeof window !== 'undefined' && new URLSearchParams(window.location.search).has('demo');

  function closeSidebarOnMobile() {
    if (window.innerWidth <= 760) sidebarVisible = false;
  }

  async function newChat() {
    if (await controller.newChat()) closeSidebarOnMobile();
  }

  function openSession(sessionId: string) {
    controller.openSession(sessionId);
    closeSidebarOnMobile();
  }

  function keydown(event: KeyboardEvent) {
    if (!booted || authBusy) return;
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
    void initializeApp();
    void updateController.initialize();
    window.addEventListener('keydown', keydown);
  });

  onDestroy(() => {
    stopPairing();
    window.removeEventListener('keydown', keydown);
    controller.destroy();
  });
</script>

{#if !booted}
  <LockScreen
    loading={authBusy}
    error={lockError}
    initialized={authInitialized}
    onUnlock={unlock}
    onRetry={initializeApp}
  />
{:else}
  <div class="app-shell" inert={authBusy} aria-busy={authBusy}>
    <Sidebar
      onPreferences={(patch) =>
        controller.preferences.update((current) => ({ ...current, ...patch }))}
      onLock={lock}
      visible={sidebarVisible}
      sessions={$sessions}
      activeSessionId={$activeSessionId}
      connectionState={$connectionState}
      selectedModel={$selectedModel}
      endpoint={$endpoint}
      connectionError={$connectionError}
      updateState={$updateState}
      currentVersion={$currentVersion}
      availableUpdate={$availableUpdate}
      updateProgress={$updateProgress}
      updateError={$updateError}
      onNewChat={newChat}
      onOpenSession={openSession}
      onRemoveSession={controller.removeSession}
      onSetup={() => {
        setupOpen = true;
        closeSidebarOnMobile();
      }}
      onConfigureEndpoint={controller.configureEndpoint}
      onCheckForUpdates={updateController.checkForUpdates}
      onInstallUpdate={updateController.installUpdate}
      onClose={() => (sidebarVisible = false)}
    />

    {#if sidebarVisible}
      <button
        class="mobile-scrim"
        aria-label="Close sidebar"
        onclick={() => (sidebarVisible = false)}
      ></button>
    {/if}

    <main>
      <AppHeader
        setupMode={setupOpen}
        {sidebarVisible}
        models={$models}
        selectedModel={$selectedModel}
        connectionState={$connectionState}
        onToggleSidebar={() => (sidebarVisible = !sidebarVisible)}
        onModelChange={controller.chooseModel}
        onReconnect={() => (setupOpen = true)}
      />

      {#if !setupOpen && isDesktop()}
        <div class="project-bar">
          <div class="mode-switch" aria-label="Conversation mode">
            <button
              class:active={!$agentMode}
              disabled={$runState !== 'idle'}
              onclick={() => agentMode.set(false)}>Chat</button
            ><button
              class:active={$agentMode}
              disabled={$runState !== 'idle'}
              onclick={() => ($workspace ? agentMode.set(true) : controller.chooseWorkspace())}
              >Agent</button
            >
          </div>
          <button
            class="project-choice"
            disabled={$runState !== 'idle'}
            title={$workspace || 'Choose the folder Blackwall can work in'}
            onclick={controller.chooseWorkspace}
            >{$workspace
              ? $workspace.split('/').filter(Boolean).at(-1)
              : 'Choose project folder'}</button
          ><span></span>{#if $agentMode}<label class="web-toggle"
              ><input
                type="checkbox"
                bind:checked={$webEnabled}
                disabled={$runState !== 'idle'}
              />Web access</label
            >{/if}{#if $messages.length}<button
              class="export"
              onclick={controller.exportConversation}>Export chat</button
            >{/if}
        </div>
      {/if}
      {#if $persistenceError}<div class="storage-error" role="alert">
          <span>{$persistenceError}</span><button onclick={controller.retryPersistence}
            >Retry saving</button
          ><button onclick={controller.exportConversation} disabled={!$messages.length}
            >Export chat</button
          >
        </div>{/if}
      <section class="conversation-pane" aria-label="Blackwall chat">
        {#if setupOpen}
          <ConnectionSetup
            connections={$preferences.connections ?? []}
            onForget={controller.forgetConnection}
            onConnect={controller.configureEndpoint}
            onChooseModel={controller.chooseModel}
            onDone={() => (setupOpen = false)}
            currentEndpoint={$endpoint}
            connectionError={$connectionError}
          />
          {#if $connectionState === 'ready'}<button
              class="return-chat"
              onclick={() => (setupOpen = false)}>Return to your conversation</button
            >{/if}
        {:else}
          <ChatView messages={$messages} connectionState={$connectionState} {showHarnessDemo} />
          {#if $approval}<ApprovalCard
              approval={$approval}
              onResolve={controller.resolveApproval}
            />{/if}
          <Composer
            busy={$runState === 'streaming' || $runState === 'preparing'}
            disabled={$connectionState !== 'ready'}
            notice={$notice}
            onDismissNotice={controller.dismissNotice}
            onSend={controller.send}
            onStop={controller.stop}
          />
        {/if}
      </section>

      {#if !setupOpen}
        <StatusBar
          mode={$agentMode ? 'Agent mode' : 'Chat mode'}
          connectionState={$connectionState}
          runState={$runState}
          model={$selectedModel}
          contextPercent={$contextPercent}
        />
      {/if}
    </main>
  </div>
{/if}

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

  .web-toggle {
    display: flex;
    align-items: center;
    gap: 6px;
    color: var(--text-muted);
    font-size: 11px;
    white-space: nowrap;
  }
  .web-toggle input {
    accent-color: var(--accent);
  }
  .project-bar {
    display: flex;
    align-items: center;
    gap: 12px;
    min-height: 45px;
    padding: 7px 20px;
    border-bottom: 1px solid var(--border);
  }
  .project-bar > span {
    flex: 1;
  }
  .mode-switch {
    display: flex;
    padding: 3px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
  }
  .mode-switch button {
    border-radius: 4px;
    background: transparent;
    color: var(--text-muted);
    padding: 3px 10px;
    font-size: 11px;
  }
  .mode-switch button.active {
    background: var(--bg-elevated);
    color: var(--text-primary);
  }
  .project-choice,
  .export {
    font-size: 11px;
    background: transparent;
    color: var(--text-muted);
    padding: 4px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 250px;
  }
  .storage-error button {
    flex: 0 0 auto;
    padding: 6px 10px;
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    background: var(--bg-elevated);
  }
  .storage-error {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
    padding: 12px 20px;
    border-bottom: 1px solid var(--warn);
    background: var(--bg-panel);
    color: var(--text-primary);
    font-size: 12px;
  }
  .return-chat {
    padding: 12px;
    background: transparent;
    color: var(--text-muted);
    font-size: 12px;
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
