<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { startPairing, stopPairing } from './lib/pairing';
  import { authClient, authError, authStatus } from './lib/auth';
  import LockScreen from './lib/components/LockScreen.svelte';
  import ApprovalCard from './lib/components/ApprovalCard.svelte';
  import { isDesktop, setupClient, type LocalService } from './lib/setup';
  import ConnectionSetup from './lib/components/ConnectionSetup.svelte';
  import AppHeader from './lib/components/AppHeader.svelte';
  import ChatView from './lib/components/ChatView.svelte';
  import Composer from './lib/components/Composer.svelte';
  import ChatDropZone from './lib/components/ChatDropZone.svelte';
  import RecallPanel from './lib/components/RecallPanel.svelte';
  import WorkspaceFiles from './lib/components/WorkspaceFiles.svelte';
  import ProjectTrust from './lib/components/ProjectTrust.svelte';

  import Sidebar from './lib/components/Sidebar.svelte';
  import { createChatController } from './lib/store';
  import { createAppUpdateController } from './lib/updates';
  import { sandboxClient, type SandboxStatus } from './lib/ipc';

  let composer: Composer;
  const controller = createChatController();
  const updateController = createAppUpdateController();
  const {
    messages,
    visibleSessions,
    sessionLocked,
    activeSessionId,
    runState,
    connectionState,
    models,
    selectedModel,
    endpoint,
    notice,
    connectionError,
    persistenceError,
    agentMode,
    webEnabled,
    executionProfile,
    commandNetwork,
    workspace,
    approval,
    preferences,
    recallText,
    focusedMessageId,
  } = controller;
  const {
    channel: updateChannel,
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
        authInitialized = !status.keyringNeeded;
        if (status.locked) return;
      }
      booted = true;
      startPairing();
      // On a first launch, let the person choose among the servers found here rather than
      // connecting to whichever answered first.
      if (isDesktop() && !showHarnessDemo && !(await controller.hasSavedConnection())) {
        const found = (await setupClient.discover().catch(() => [])).filter(
          (service) => service.available,
        );
        if (found.length) {
          startupServers = found;
          controller.awaitConnectionChoice();
          setupOpen = true;
          return;
        }
      }
      const connected = await controller.initialize();
      if (!connected && !showHarnessDemo) setupOpen = true;
    } catch (cause) {
      lockError = authError(cause);
      if (!booted) {
        // A failed read settles nothing, so show its error rather than an earlier state.
        authStatus.update((status) => ({ ...status, keyringNeeded: false }));
        authInitialized = false;
      }
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
  // Only Linux reports a status. Unknown (other systems, the browser preview, or no answer)
  // leaves every profile on offer; a command that can't be sandboxed still fails closed and says
  // why. Read-only, the safe default, stays selectable even when the sandbox is unavailable.
  // Workspace-write is offered only where its overlay works; the note says why when it isn't.
  let sandbox: SandboxStatus | null = null;
  $: workspaceWrite = sandbox?.workspaceWrite ?? true;
  const SANDBOX_RECHECK_MS = 30_000;
  let sandboxRecheck: ReturnType<typeof setTimeout> | undefined;
  let sandboxRequest = 0;
  /** Ask again; when the sandbox is unavailable (bubblewrap may have failed only for a moment,
   * or been installed since), ask once more about 30 seconds later. Only the latest answer counts. */
  async function checkSandbox(recheck = false) {
    if (!isDesktop()) return;
    const request = ++sandboxRequest;
    const status = await sandboxClient.status();
    if (request !== sandboxRequest) return;
    sandbox = status;
    clearTimeout(sandboxRecheck);
    sandboxRecheck = undefined;
    if (!recheck && status && !status.available) {
      sandboxRecheck = setTimeout(() => {
        if ($agentMode) void checkSandbox(true);
      }, SANDBOX_RECHECK_MS);
    }
  }
  $: if ($agentMode) void checkSandbox();
  function windowFocused() {
    if ($agentMode) void checkSandbox();
  }
  let setupOpen = false;
  let startupServers: LocalService[] | null = null;
  $: if (!setupOpen) startupServers = null;
  let filesVisible = true;
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
    window.addEventListener('focus', windowFocused);
  });

  onDestroy(() => {
    stopPairing();
    window.removeEventListener('keydown', keydown);
    window.removeEventListener('focus', windowFocused);
    clearTimeout(sandboxRecheck);
    controller.destroy();
  });
</script>

{#if !booted}
  <LockScreen
    loading={authBusy}
    error={lockError}
    initialized={authInitialized}
    keyringNeeded={$authStatus.keyringNeeded}
    onUnlock={unlock}
    onRetry={initializeApp}
  />
{:else}
  <div class="app-shell" inert={authBusy} aria-busy={authBusy}>
    <Sidebar
      contextPreferences={$preferences}
      onPreferences={controller.savePreferences}
      onLock={lock}
      visible={sidebarVisible}
      sessions={$visibleSessions}
      agentMode={$agentMode}
      showModes={isDesktop()}
      onModeChange={(mode) => controller.newChat(mode)}
      activeSessionId={$activeSessionId}
      connectionState={$connectionState}
      selectedModel={$selectedModel}
      endpoint={$endpoint}
      connectionError={$connectionError}
      updateChannel={$updateChannel}
      onUpdateChannelChange={updateController.setChannel}
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
          {#if $agentMode}<button
              class="project-choice"
              disabled={$sessionLocked || $runState !== 'idle'}
              title={$workspace || 'Choose the folder Blackwall can work in'}
              onclick={controller.chooseWorkspace}
              >{$workspace
                ? $workspace.split('/').filter(Boolean).at(-1)
                : 'Choose project folder'}</button
            >{/if}<span></span>{#if $agentMode}<label class="web-toggle">
              Commands
              <select
                aria-label="Command execution profile"
                bind:value={$executionProfile}
                disabled={$runState !== 'idle'}
              >
                <option value="read-only">Read-only</option>
                <option value="workspace-write" disabled={!workspaceWrite}>Workspace-write</option>
                <option value="unsandboxed">Unsandboxed (full access)</option>
              </select>
            </label><label class="web-toggle">
              <input
                type="checkbox"
                checked={$commandNetwork || $executionProfile === 'unsandboxed'}
                onchange={(event) => commandNetwork.set(event.currentTarget.checked)}
                disabled={$runState !== 'idle' || $executionProfile === 'unsandboxed'}
              />Command network
            </label>{/if}<label class="web-toggle"
            ><input type="checkbox" bind:checked={$webEnabled} disabled={$runState !== 'idle'} />Web
            access</label
          >{#if $agentMode}<button
              class="export"
              aria-pressed={filesVisible}
              onclick={() => (filesVisible = !filesVisible)}
              >{filesVisible ? 'Hide files' : 'Show files'}</button
            >{/if}{#if $messages.length}<button
              class="export"
              onclick={controller.exportConversation}>Export chat</button
            >{/if}
          <button class="export" onclick={() => controller.showRecall()}>Search history</button>
        </div>
      {/if}
      {#if !setupOpen && isDesktop() && $agentMode && $workspace}
        <ProjectTrust workspace={$workspace} refresh={$runState} />
      {/if}
      {#if !setupOpen && isDesktop() && $agentMode && sandbox?.note && (!sandbox.available || !workspaceWrite)}
        <p class="sandbox-note" role="status">
          {sandbox.note}<button
            class="sandbox-help"
            onclick={() => void setupClient.openLink('sandbox-docs').catch(() => {})}
            >Learn more</button
          >
        </p>
      {/if}
      {#if !setupOpen && isDesktop() && $agentMode && $commandNetwork && $executionProfile !== 'unsandboxed'}
        <p class="sandbox-note" role="note">
          With command network on, sandboxed commands share this computer's network, so they can
          reach services on this computer (localhost) and your local network.
        </p>
      {/if}
      {#if $agentMode && $executionProfile === 'unsandboxed'}
        <div class="storage-error" role="status">
          Unsandboxed commands have full account filesystem and network access. Each command still
          requires approval.
        </div>
      {/if}
      {#if $persistenceError}<div class="storage-error" role="alert">
          <span>{$persistenceError}</span><button onclick={controller.retryPersistence}
            >Retry saving</button
          ><button onclick={controller.exportConversation} disabled={!$messages.length}
            >Export chat</button
          >
        </div>{/if}
      {#if $recallText !== null}
        <RecallPanel
          initialQuery={$recallText}
          search={controller.searchRecall}
          enabled={$preferences.recallEnabled ?? false}
          onEnabled={(recallEnabled) => controller.savePreferences({ recallEnabled })}
          onOpen={controller.openRecallResult}
          onClose={controller.closeRecall}
        />
      {/if}
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
            found={startupServers}
          />
          {#if $connectionState === 'ready'}<button
              class="return-chat"
              onclick={() => (setupOpen = false)}>Return to your conversation</button
            >{/if}
        {:else}
          <div class="conversation-content">
            <ChatDropZone
              busy={$runState !== 'idle'}
              onFiles={(files) => composer?.addFiles(files)}
            >
              <ChatView
                focusedMessageId={$focusedMessageId}
                messages={$messages}
                connectionState={$connectionState}
                {showHarnessDemo}
              />
              {#if $approval}<ApprovalCard
                  approval={$approval}
                  onResolve={controller.resolveApproval}
                />{/if}
              <Composer
                bind:this={composer}
                chatAttachments={$messages.flatMap((message) => message.attachments)}
                sessionId={$activeSessionId}
                busy={$runState === 'streaming' || $runState === 'preparing'}
                disabled={$connectionState !== 'ready'}
                notice={$notice}
                onDismissNotice={controller.dismissNotice}
                onSend={controller.send}
                onStop={controller.stop}
              />
            </ChatDropZone>
            {#if isDesktop() && $agentMode && $workspace && filesVisible}
              <WorkspaceFiles workspace={$workspace} onClose={() => (filesVisible = false)} />
            {/if}
          </div>
        {/if}
      </section>
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

  .conversation-content {
    position: relative;
    display: flex;
    min-width: 0;
    min-height: 0;
    flex: 1;
  }

  .conversation-content :global(.chat-drop-zone) {
    min-width: 0;
  }

  .web-toggle {
    display: flex;
    align-items: center;
    gap: 6px;
    color: var(--text-muted);
    font-size: 11px;
    white-space: nowrap;
  }
  .web-toggle select {
    max-width: 190px;
    padding: 4px 6px;
    border: 1px solid var(--border);
    border-radius: 5px;
    background: var(--bg-base);
    color: var(--text-primary);
    font: inherit;
  }
  .web-toggle input {
    accent-color: var(--accent);
  }
  .project-bar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 12px;
    min-height: 45px;
    padding: 7px 20px;
    border-bottom: 1px solid var(--border);
  }
  .project-bar > span {
    flex: 1;
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
  .sandbox-note {
    margin: 0;
    padding: 6px 20px;
    border-bottom: 1px solid var(--border);
    color: var(--text-muted);
    font-size: 11px;
  }
  .sandbox-help {
    margin-left: 6px;
    padding: 0;
    background: transparent;
    color: var(--text-muted);
    font: inherit;
    text-decoration: underline;
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
