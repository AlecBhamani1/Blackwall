<script lang="ts">
  import { onDestroy, tick } from 'svelte';
  import { guestShareClient, multiShareClient, type GuestShareClient } from '../ipc';
  import type { ConnectionState, ShareExpiryMinutes, ShareStatus } from '../types';
  import type {
    AppUpdateChannel,
    AppUpdateInfo,
    AppUpdateProgress,
    AppUpdateState,
  } from '../updates';
  import LockSettings from './LockSettings.svelte';
  import CredentialSettings from './CredentialSettings.svelte';
  import TelemetrySettings from './TelemetrySettings.svelte';
  import TrustedProjects from './TrustedProjects.svelte';
  import OwnerRules from './OwnerRules.svelte';
  import SavedConnections from './SavedConnections.svelte';
  import FoundServers from './FoundServers.svelte';
  import { connectionProfiles } from '../connections';
  import PairingPanel from './PairingPanel.svelte';
  import { isPairedEndpoint } from '../pairing';
  import {
    detectedServerLabel,
    isDesktop,
    normalizeEndpoint,
    serviceName,
    setupClient,
    type LocalService,
  } from '../setup';
  import { platform } from '../platform';
  import Icon from './Icon.svelte';
  import MemoryDialog from './MemoryDialog.svelte';
  import SkillsDialog from './SkillsDialog.svelte';
  import McpDialog from './McpDialog.svelte';
  import RunsDialog from './RunsDialog.svelte';
  import UsageDialog from './UsageDialog.svelte';
  import { settingsEntries, settingsGroups, type SettingsPage } from '../settingsNavigation';
  import type { Preferences } from '../persistence';

  export let contextPreferences: Preferences = {};
  export let onPreferences: (preferences: Preferences) => void = () => {};
  export let onLock: () => Promise<void> = async () => {};
  export let onSetup: () => void = () => {};
  export let open = false;
  export let model = '';
  export let endpoint = '';
  export let connectionState: ConnectionState = 'checking';
  export let connectionError = '';
  export let onConfigureEndpoint: (
    endpoint: string,
    name?: string,
  ) => Promise<boolean> = async () => false;
  export let updateChannel: AppUpdateChannel = 'stable';
  export let onUpdateChannelChange: (channel: AppUpdateChannel) => Promise<void> = async () => {};
  export let updateState: AppUpdateState = 'idle';
  export let currentVersion = '';
  export let availableUpdate: AppUpdateInfo | null = null;
  export let updateProgress: AppUpdateProgress = { downloadedBytes: 0 };
  export let updateError = '';
  export let onCheckForUpdates: () => Promise<boolean> = async () => false;
  export let onInstallUpdate: () => Promise<void> = async () => {};
  export let onClose: () => void;
  export let client: GuestShareClient = guestShareClient;
  export let invites = multiShareClient;
  export let detectServer = setupClient.detectServer;
  export let discover = setupClient.discover;
  export let desktop = isDesktop();

  const expiryOptions: Array<{ value: ShareExpiryMinutes; label: string }> = [
    { value: 15, label: '15 min' },
    { value: 60, label: '1 hour' },
    { value: 480, label: '8 hours' },
  ];
  const RELAY_URL_STORAGE_KEY = 'blackwall.relay-url.v1';

  let keysOpen = false;
  let trustedOpen = false;
  let rulesOpen = false;
  let selectedSetting: SettingsPage = 'connection';
  let settingsQuery = '';
  let dialogContent: HTMLElement;

  $: matchingSettings = settingsEntries.filter((entry) => {
    if (entry.desktopOnly && !isDesktop()) return false;
    const text =
      `${entry.title} ${entry.description} ${entry.group} ${entry.keywords}`.toLowerCase();
    return settingsQuery
      .toLowerCase()
      .trim()
      .split(/\s+/)
      .every((word) => text.includes(word));
  });
  $: menuGroups = settingsGroups.map((title) => ({
    title,
    entries: matchingSettings.filter((entry) => entry.group === title),
  }));

  async function openSetting(entry: (typeof settingsEntries)[number]): Promise<void> {
    selectedSetting = entry.id;
    await tick();
    dialogContent.scrollTop = 0;
    dialogContent.focus();
  }
  let expiry: ShareExpiryMinutes = 60;
  let inviteName = '';
  let creatingLink = false;
  let sharedLinks: ShareStatus[] = [];
  const knownLinks = new Map<string, ShareStatus>();
  let status: ShareStatus = { active: false };
  let busy: 'loading' | 'starting' | 'stopping' | '' = '';
  let error = '';
  let copied = false;
  let dialog: HTMLElement;
  let previouslyOpen = false;
  let pollTimer: number | undefined;
  let operationVersion = 0;
  let endpointDraft = endpoint;
  let previousEndpoint = endpoint;
  let endpointBusy = false;
  let endpointFeedback = '';
  let endpointInputError = '';
  let keyEndpoint = '';
  $: activeProfile = (contextPreferences.connections ?? []).find(
    (profile) => profile.endpoint === normalizedOrEmpty(endpoint),
  );
  function normalizedOrEmpty(address: string): string {
    try {
      return address ? normalizeEndpoint(address) : '';
    } catch {
      return '';
    }
  }
  function setTextToolCalls(enabled: boolean): void {
    onPreferences({
      connections: (contextPreferences.connections ?? []).map((profile) => {
        if (profile.endpoint !== activeProfile?.endpoint) return profile;
        const { textToolCalls: _, ...rest } = profile;
        return enabled ? { ...rest, textToolCalls: true } : rest;
      }),
    });
  }
  let foundServers: LocalService[] = [];
  let foundScanning = false;
  let foundFor = false;
  let foundVersion = 0;
  $: foundKey = open && selectedSetting === 'connection' && desktop;
  $: if (foundKey !== foundFor) {
    foundFor = foundKey;
    if (foundKey) void refreshFound();
  }
  async function refreshFound(): Promise<void> {
    const version = ++foundVersion;
    foundScanning = true;
    try {
      const next = await discover();
      if (version === foundVersion) foundServers = next;
    } catch {
      // Discovery is a shortcut; the address field and saved connections still work.
    } finally {
      if (version === foundVersion) foundScanning = false;
    }
  }
  let detectedServer = '';
  let detectedFor = '';
  $: detectKey =
    open && selectedSetting === 'connection' && connectionState === 'ready' && endpoint && model
      ? `${endpoint}\n${model}`
      : '';
  $: if (detectKey !== detectedFor) {
    detectedFor = detectKey;
    detectedServer = '';
    if (detectKey) void showDetectedServer(detectKey, endpoint, model);
  }
  async function showDetectedServer(key: string, address: string, id: string): Promise<void> {
    try {
      const server = await detectServer(address, id);
      if (server && key === detectedFor) detectedServer = detectedServerLabel(server);
    } catch {
      // Detection is informational; the connection itself already succeeded.
    }
  }
  $: modelKeyConnections = connectionProfiles([
    ...(endpoint ? [{ endpoint, name: 'Current model service' }] : []),
    ...(contextPreferences.connections ?? []),
  ]);
  $: if (!modelKeyConnections.some((profile) => profile.endpoint === keyEndpoint)) {
    keyEndpoint = modelKeyConnections[0]?.endpoint ?? '';
  }
  let relayUrlDraft = typeof window === 'undefined' ? '' : readRelayUrl();
  let credentialRelayUrl = relayUrlDraft;
  let relayTokenDraft = '';

  function errorMessage(value: unknown): string {
    return value instanceof Error ? value.message : 'Blackwall could not update guest sharing.';
  }

  async function focusGuestResult(): Promise<void> {
    await tick();
    if (!open || selectedSetting !== 'sharing' || !dialog) return;
    const active = document.activeElement;
    if (active !== document.body && !dialog.contains(active)) return;
    // Creating/revoking a link removes the initiating control from the DOM.
    // Keep keyboard navigation inside Settings after that transition.
    (
      dialog.querySelector<HTMLElement>('[data-guest-error]') ??
      dialog.querySelector<HTMLElement>('#guest-link') ??
      dialog.querySelector<HTMLElement>('.start-button') ??
      dialog
    ).focus();
  }

  function mergeStatus(next: ShareStatus): void {
    const previous = next.id ? knownLinks.get(next.id) : !status.id ? status : undefined;
    status = next.active
      ? {
          ...next,
          shareUrl: next.shareUrl ?? previous?.shareUrl,
          qrDataUrl: next.qrDataUrl ?? previous?.qrDataUrl,
        }
      : { active: false };
    if (status.id) knownLinks.set(status.id, status);
  }
  async function revoke(id: string) {
    if (busy) return;
    busy = 'stopping';
    error = '';
    operationVersion += 1;
    try {
      await invites.revoke(id);
      knownLinks.delete(id);
      if (status.id === id) status = { active: false };
      await refresh(true);
    } catch (cause) {
      error = errorMessage(cause);
    } finally {
      busy = '';
      await focusGuestResult();
    }
  }

  function readRelayUrl(): string {
    try {
      return window.localStorage.getItem(RELAY_URL_STORAGE_KEY) ?? '';
    } catch {
      return '';
    }
  }
  function rememberRelayUrl(): void {
    try {
      const relayUrl = relayUrlDraft.trim();
      credentialRelayUrl = relayUrl;
      if (relayUrl) window.localStorage.setItem(RELAY_URL_STORAGE_KEY, relayUrl);
      else window.localStorage.removeItem(RELAY_URL_STORAGE_KEY);
    } catch {
      /* The connection can still be used for this app session. */
    }
  }

  async function refresh(silent = false): Promise<void> {
    const version = operationVersion;
    if (!silent) busy = 'loading';
    try {
      const [latest, links] = await Promise.all([client.shareStatus(), invites.list()]);
      const next = links.find((link) => link.id === status.id) ?? latest;
      if (version !== operationVersion) return;
      sharedLinks = links;
      mergeStatus(next);
      error = '';
    } catch (cause) {
      if (!silent && version === operationVersion) error = errorMessage(cause);
    } finally {
      if (!silent && version === operationVersion) busy = '';
    }
  }

  async function start(): Promise<void> {
    if (!model || busy) return;
    operationVersion += 1;
    busy = 'starting';
    error = '';
    copied = false;
    try {
      const relayUrl = relayUrlDraft.trim();
      const relayToken = relayTokenDraft.trim();
      rememberRelayUrl();
      mergeStatus(
        await client.startShare({
          model,
          ...(inviteName.trim() ? { name: inviteName.trim() } : {}),
          ...(endpoint ? { endpoint } : {}),
          ...(relayUrl ? { relayUrl } : {}),
          ...(relayToken ? { relayToken } : {}),
          expiresInMinutes: expiry,
        }),
      );
      creatingLink = false;
      inviteName = '';
      relayTokenDraft = '';
      sharedLinks = await invites.list();
    } catch (cause) {
      error = errorMessage(cause);
    } finally {
      busy = '';
      await focusGuestResult();
    }
  }

  async function connectEndpoint(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const candidate = endpointDraft.trim();
    if (!candidate) {
      endpointInputError = 'Enter an endpoint URL before connecting.';
      endpointFeedback = '';
      return;
    }

    await connectSaved(candidate);
  }

  async function connectSaved(candidate: string, name?: string): Promise<void> {
    if (endpointBusy) return;
    endpointBusy = true;
    endpointInputError = '';
    endpointFeedback = '';
    try {
      const connected = await (name
        ? onConfigureEndpoint(candidate, name)
        : onConfigureEndpoint(candidate));
      endpointFeedback = connected ? 'Connected and loaded the available models.' : '';
    } catch (cause) {
      endpointInputError = errorMessage(cause);
    } finally {
      endpointBusy = false;
    }
  }

  async function stop(): Promise<void> {
    if (busy) return;
    operationVersion += 1;
    busy = 'stopping';
    error = '';
    try {
      mergeStatus(await client.stopShare());
      sharedLinks = [];
      knownLinks.clear();
      creatingLink = false;
      copied = false;
    } catch (cause) {
      error = errorMessage(cause);
    } finally {
      busy = '';
      await focusGuestResult();
    }
  }

  async function copyLink(): Promise<void> {
    if (!status.shareUrl) return;
    try {
      await navigator.clipboard.writeText(status.shareUrl);
      copied = true;
      window.setTimeout(() => (copied = false), 2_000);
    } catch {
      error = 'The link could not be copied. Select it and copy it manually.';
    }
  }

  function expiresLabel(value?: number): string {
    if (!value) return 'Temporary access';
    const parsed = new Date(value);
    if (Number.isNaN(parsed.getTime())) return 'Temporary access';
    return `Expires ${new Intl.DateTimeFormat(undefined, {
      hour: 'numeric',
      minute: '2-digit',
    }).format(parsed)}`;
  }

  function downloadPercent(progress: AppUpdateProgress): number {
    if (!progress.totalBytes) return 0;
    return Math.min(100, Math.round((progress.downloadedBytes / progress.totalBytes) * 100));
  }

  function versionLabel(version: string): string {
    return version.startsWith('v') ? version : `v${version}`;
  }

  function handleKeydown(event: KeyboardEvent): void {
    if (open && event.key === 'Escape') {
      event.preventDefault();
      onClose();
    }
  }

  $: if (open !== previouslyOpen) {
    previouslyOpen = open;
    if (open) {
      copied = false;
      selectedSetting = 'connection';
      settingsQuery = '';
      endpointDraft = endpoint;
      previousEndpoint = endpoint;
      endpointFeedback = '';
      endpointInputError = '';
      void tick().then(() => dialog?.focus());
    }
  }

  $: if (open && selectedSetting === 'sharing') {
    void refresh();
    pollTimer = window.setInterval(() => void refresh(true), 10_000);
  } else if (pollTimer !== undefined) {
    window.clearInterval(pollTimer);
    pollTimer = undefined;
  }

  $: if (!endpointBusy && endpoint !== previousEndpoint) {
    previousEndpoint = endpoint;
    endpointDraft = endpoint;
  }

  onDestroy(() => {
    if (pollTimer !== undefined) window.clearInterval(pollTimer);
  });
</script>

<svelte:window onkeydown={handleKeydown} />

{#if open}
  <div class="modal-layer">
    <div
      class="settings-dialog"
      role="region"
      aria-labelledby="settings-title"
      tabindex="-1"
      bind:this={dialog}
    >
      <header>
        <div>
          <h2 id="settings-title">Settings</h2>
          <p>Find preferences, integrations, and app tools.</p>
        </div>
        <button class="icon-button" aria-label="Close settings" onclick={onClose}>
          <Icon name="x" size={18} />
        </button>
      </header>

      <div class="settings-layout">
        <aside class="settings-navigation">
          <div class="menu-controls">
            <label class="settings-search">
              <Icon name="search" size={16} />
              <input
                type="search"
                aria-label="Search settings"
                placeholder="Search settings"
                bind:value={settingsQuery}
              />
            </label>
          </div>
          <nav aria-label="Settings menu">
            {#each menuGroups as group}
              {#if group.entries.length}
                <section class="settings-group" aria-label={group.title}>
                  <h3>{group.title}</h3>
                  {#each group.entries as entry (entry.id)}
                    <button
                      class="preference-button"
                      data-setting-id={entry.id}
                      title={entry.description}
                      aria-current={selectedSetting === entry.id ? 'page' : undefined}
                      onclick={() => openSetting(entry)}
                    >
                      <Icon name={entry.icon} size={19} />
                      <span><strong>{entry.title}</strong></span>
                      <Icon name="chevron-right" size={16} />
                    </button>
                  {/each}
                </section>
              {/if}
            {/each}
            {#if matchingSettings.length === 0}
              <p class="no-settings" role="status">
                No settings match “{settingsQuery}”. Try another search.
              </p>
            {/if}
          </nav>
        </aside>
        <main
          class="dialog-content"
          data-testid="settings-content-pane"
          tabindex="-1"
          bind:this={dialogContent}
        >
        {#if selectedSetting === 'connection'}
          <section class="connection-section" aria-labelledby="connection-heading">
            <div class="section-heading">
              <div>
                <h3 id="connection-heading">Model connection</h3>
                <p>Use any Ollama or OpenAI-compatible endpoint you can reach.</p>
              </div>
              <span
                class:connected={connectionState === 'ready'}
                class:checking={connectionState === 'checking'}
                class="connection-badge"
              >
                <span></span>
                {connectionState === 'ready'
                  ? 'Connected'
                  : connectionState === 'checking'
                    ? 'Checking'
                    : 'Offline'}
              </span>
            </div>

            <div class="connection-start">
              <button class="connect-button setup-button" onclick={onSetup}
                >Set up a connection</button
              >
              {#if desktop}
                <FoundServers
                  services={foundServers}
                  currentEndpoint={endpoint}
                  busy={endpointBusy || connectionState === 'checking'}
                  scanning={foundScanning}
                  onConnect={(service) => connectSaved(service.endpoint, serviceName(service))}
                  onRescan={refreshFound}
                />
              {/if}
            </div>
            <form class="endpoint-form address-form" onsubmit={connectEndpoint}>
              <label for="model-endpoint">Server address</label>
              <div class="address-row">
                <input
                  id="model-endpoint"
                  bind:value={endpointDraft}
                  placeholder="http://localhost:11434/v1"
                  autocomplete="url"
                  autocapitalize="none"
                  spellcheck="false"
                  disabled={endpointBusy}
                />
                <button
                  class="connect-button"
                  disabled={endpointBusy || connectionState === 'checking'}
                  type="submit"
                >
                  {endpointBusy || connectionState === 'checking' ? 'Checking…' : 'Connect'}
                </button>
              </div>
              <p class="endpoint-hint">
                Any Ollama or OpenAI-compatible server, such as <code
                  >http://192.168.1.50:11434</code
                >; Blackwall adds <code>/v1</code> automatically.
              </p>
            </form>
            {#if endpointInputError}
              <p class="connection-error" role="alert">{endpointInputError}</p>
            {:else if endpointFeedback}
              <p class="connection-success" role="status">{endpointFeedback}</p>
            {:else if connectionError}
              <p class="connection-error" role="alert">{connectionError}</p>
            {/if}
            <SavedConnections
              connections={contextPreferences.connections ?? []}
              currentEndpoint={endpoint}
              busy={endpointBusy || connectionState === 'checking'}
              onConnect={connectSaved}
              onForget={(address) =>
                onPreferences({
                  connections: (contextPreferences.connections ?? []).filter(
                    (profile) => profile.endpoint !== address,
                  ),
                })}
            />
            {#if detectedServer}
              <p class="detected-server" role="status">{detectedServer}</p>
            {/if}
            {#if activeProfile}
              <label class="text-tool-calls">
                <input
                  type="checkbox"
                  checked={activeProfile.textToolCalls === true}
                  onchange={(event) => setTextToolCalls(event.currentTarget.checked)}
                />
                <span>
                  <strong>This model writes tool calls as text</strong>
                  <small
                    >For models that reply with &lt;tool_call&gt; blocks instead of structured
                    calls. Blackwall always asks before running one.</small
                  >
                </span>
              </label>
            {/if}
          </section>
        {/if}
        {#if selectedSetting === 'sharing'}
          <section class="share-section">
            <PairingPanel
              mode="host"
              onCreated={rememberRelayUrl}
              {model}
              {endpoint}
              bind:relayUrl={relayUrlDraft}
              bind:relayKey={relayTokenDraft}
            />
          </section>
          <section class="share-section" aria-labelledby="share-heading">
            <div class="section-heading">
              <div>
                <h3 id="share-heading">Share your model</h3>
                <p>Create a temporary browser link for someone you trust.</p>
              </div>
              {#if status.active}
                <span class="live-badge"><span></span>Live</span>
              {/if}
            </div>

            {#if busy === 'loading'}
              <div class="loading-row" role="status">
                <span class="spinner"></span>
                Checking guest access…
              </div>
            {:else if status.active && !creatingLink}
              <div class="active-share">
                <div class="share-summary">
                  <div>
                    <span class="summary-label">Sharing</span>
                    <strong title={status.name || status.model}
                      >{status.name || status.model || model}</strong
                    >{#if status.name}<small>{status.model || model}</small>{/if}
                  </div>
                  <div>
                    <span class="summary-label">Access</span>
                    <strong>{status.networkLabel || 'Hosted relay'}</strong>
                  </div>
                </div>

                {#if status.qrDataUrl && status.shareUrl}
                  <div class="qr-wrap">
                    <img src={status.qrDataUrl} alt="QR code for guest chat link" />
                    <div>
                      <strong>Scan to open</strong>
                      <p>Guests can scan this code with their phone camera.</p>
                    </div>
                  </div>

                  <label class="link-label" for="guest-link">Guest link</label>
                  <div class="link-field">
                    <input
                      id="guest-link"
                      readonly
                      value={status.shareUrl}
                      onclick={(event) => event.currentTarget.select()}
                    />
                    <button class:copied aria-label="Copy guest link" onclick={copyLink}>
                      <Icon name={copied ? 'check' : 'copy'} size={15} />
                      <span aria-live="polite">{copied ? 'Copied' : 'Copy'}</span>
                    </button>
                  </div>
                {:else}
                  <div class="notice warning">
                      This link is active, but its access code is no longer available in this
                      window. Revoke this link below and create a replacement if you need to share
                      it again.
                  </div>
                {/if}

                <div class="share-meta">
                  <span>{expiresLabel(status.expiresAt)}</span>
                  <span
                    >{status.requestCount ?? 0}
                    {(status.requestCount ?? 0) === 1 ? 'request' : 'requests'}</span
                  >
                </div>

                <div class="notice warning">
                    Anyone with this link can send prompts and files to your model until it expires
                    or you stop sharing.
                </div>

                <button
                  class="check-button"
                  disabled={!!busy || sharedLinks.length >= 4}
                  onclick={() => (creatingLink = true)}>Create another guest link</button
                >
                <button class="stop-button" disabled={busy === 'stopping'} onclick={stop}>
                  {busy === 'stopping' ? 'Stopping…' : 'Stop sharing'}
                </button>
              </div>
            {:else}
              <div class="inactive-share">
                <div class="endpoint-form">
                  <label for="invite-name">Name this guest link <span>(optional)</span></label
                  ><input
                    id="invite-name"
                    bind:value={inviteName}
                    maxlength={60}
                    placeholder="For example, Alex’s phone"
                    disabled={busy === 'starting'}
                  />
                </div>
                {#if creatingLink}<button
                    class="check-button"
                    disabled={!!busy}
                    onclick={() => (creatingLink = false)}>Back to active links</button
                  >{/if}
                <div class="relay-form">
                  <label for="relay-url">Hosted relay URL</label>
                  <input
                    id="relay-url"
                    bind:value={relayUrlDraft}
                    placeholder="https://relay.example.com"
                    autocomplete="url"
                    autocapitalize="none"
                    spellcheck="false"
                    disabled={busy === 'starting'}
                    onchange={rememberRelayUrl}
                  />
                  <p class="relay-hint">
                      Saved on this {platform.device}. Change this origin whenever you move the
                      relay to another server.
                  </p>

                  <label for="relay-token">Relay token <span>(optional)</span></label>
                  <input
                    id="relay-token"
                    type="password"
                    bind:value={relayTokenDraft}
                    placeholder="Use the saved relay token when blank"
                    autocomplete="off"
                    autocapitalize="none"
                    spellcheck="false"
                    disabled={busy === 'starting'}
                  />
                  <p class="relay-hint">
                    Saved in {platform.storeName} after the relay accepts it. Never included in guest
                    links.
                  </p>
                </div>

                <div class="model-row">
                  <span>Model</span>
                  <strong>{model || 'No model selected'}</strong>
                </div>

                <fieldset disabled={busy === 'starting'}>
                  <legend>Link expires after</legend>
                  <div class="expiry-options">
                    {#each expiryOptions as option}
                      <label class:checked={expiry === option.value}>
                        <input
                          type="radio"
                          name="share-expiry"
                          value={option.value}
                          bind:group={expiry}
                        />
                        <span>{option.label}</span>
                      </label>
                    {/each}
                  </div>
                </fieldset>

                <div class="notice">
                  Blackwall must stay open and connected to the hosted relay while guests are
                  chatting. No inbound port or VPN is required.
                </div>

                <button
                  class="start-button"
                  disabled={!model || busy === 'starting'}
                  onclick={start}
                >
                  {busy === 'starting' ? 'Creating link…' : 'Create guest link'}
                </button>
              </div>
            {/if}

            {#if sharedLinks.length}
              <div class="guest-list" aria-label="Active guest links">
                <h4>Active links · {sharedLinks.length} of 4</h4>
                {#each sharedLinks as link (link.id)}
                  <div class="guest-row">
                    <button
                      class="guest-select"
                      class:selected={status.id === link.id}
                      disabled={!!busy}
                      onclick={() => {
                        mergeStatus(link);
                        creatingLink = false;
                      }}
                      ><strong>{link.name || 'Guest link'}</strong><small
                        >{link.model} · {expiresLabel(link.expiresAt)} · {link.requestCount ?? 0} requests</small
                      ></button
                    ><button
                      class="guest-revoke"
                      aria-label={`Revoke ${link.name || 'guest link'}`}
                      disabled={!!busy}
                      onclick={() => link.id && revoke(link.id)}>Revoke</button
                    >
                  </div>
                {/each}
                <p>
                  Each link has separate access. Revoke one here, or use Stop sharing to revoke
                  every link.
                </p>
              </div>
            {/if}
            {#if error}
              <p class="error" role="alert" data-guest-error tabindex="-1">{error}</p>
            {/if}

            <p class="privacy-note">
              Guest chats are routed through this Blackwall app to your configured model endpoint.
            </p>
          </section>
        {/if}
          {#if selectedSetting === 'rules'}
          {#if isDesktop()}
            <details class="access-keys" bind:open={keysOpen}>
              <summary>Access keys</summary>
              <p>
                Manage saved keys for your model services and configured relay. Keys are shared by
                connections using the same service origin.
              </p>
              {#if keysOpen}
                {#if modelKeyConnections.length > 1}
                  <label for="key-connection">Model service</label>
                  <select class="key-connection" id="key-connection" bind:value={keyEndpoint}>
                    {#each modelKeyConnections as profile}
                        <option value={profile.endpoint}>{profile.name} · {profile.endpoint}</option
                        >
                    {/each}
                  </select>
                {/if}
                {#if isPairedEndpoint(keyEndpoint)}<p class="notice">
                    This connection uses a paired device key. Manage it under connection setup →
                    paired computers.
                  </p>{:else if keyEndpoint}<CredentialSettings endpoint={keyEndpoint} />{/if}
                {#if credentialRelayUrl}<CredentialSettings
                    kind="relay"
                    endpoint={credentialRelayUrl}
                  />{/if}
                {#if !endpoint && !credentialRelayUrl}<p>
                    Connect a model or configure a relay to manage its access key.
                  </p>{/if}
                <p>
                    Keys supplied through environment variables are managed outside Blackwall and
                    are unaffected by removal here.
                </p>
              {/if}
            </details>
          {/if}
          {#if isDesktop()}
            <details class="access-keys" bind:open={trustedOpen}>
              <summary>Trusted projects</summary>
              {#if trustedOpen}<TrustedProjects />{/if}
            </details>
            <details class="access-keys" bind:open={rulesOpen}>
              <summary>Your rules</summary>
              {#if rulesOpen}<OwnerRules
                  isolation={contextPreferences.commandToolsIsolation ?? 'outside'}
                />{/if}
            </details>
          {/if}
        {/if}
          {#if selectedSetting === 'telemetry'}<TelemetrySettings />{/if}
          {#if selectedSetting === 'lock'}<LockSettings {onLock} />{/if}
          {#if selectedSetting === 'memory'}<MemoryDialog
              embedded
              section="memory"
              {contextPreferences}
              {onPreferences}
              onClose={() => {}}
            />{/if}
          {#if selectedSetting === 'limits'}<MemoryDialog
              embedded
              section="limits"
              {contextPreferences}
              {onPreferences}
              onClose={() => {}}
            />{/if}
          {#if selectedSetting === 'subagents'}<MemoryDialog
              embedded
              section="subagents"
              {contextPreferences}
              {onPreferences}
              onClose={() => {}}
            />{/if}
          {#if selectedSetting === 'instructions'}<MemoryDialog
              embedded
              section="instructions"
              {contextPreferences}
              {onPreferences}
              onClose={() => {}}
            />{/if}
          {#if selectedSetting === 'skills'}<SkillsDialog embedded onClose={() => {}} />{/if}
          {#if selectedSetting === 'mcp'}<McpDialog embedded onClose={() => {}} />{/if}
          {#if selectedSetting === 'runs'}<RunsDialog embedded onClose={() => {}} />{/if}
          {#if selectedSetting === 'usage'}<UsageDialog embedded onClose={() => {}} />{/if}
        {#if selectedSetting === 'updates'}
          <section class="update-section" aria-labelledby="update-heading">
            <div class="section-heading">
              <div>
                <h3 id="update-heading">App updates</h3>
                <p>Choose stable releases or try upcoming features in beta.</p>
              </div>
              {#if currentVersion}
                <span class="version-badge">{versionLabel(currentVersion)}</span>
              {/if}
            </div>

            <label class="channel-label" for="update-channel">Update channel</label>
            <select
              id="update-channel"
              value={updateChannel}
              disabled={updateState === 'downloading' || updateState === 'unsupported'}
              onchange={(event) =>
                onUpdateChannelChange(event.currentTarget.value as AppUpdateChannel)}
            >
              <option value="stable">Stable</option>
              <option value="beta">Beta</option>
            </select>
            <p class="channel-help">
              {#if updateChannel === 'beta'}
                Try features before they reach the stable release. Beta builds may have bugs.
              {:else}
                Get tested releases. Switching from beta offers the latest stable version.
              {/if}
            </p>

            {#if updateState === 'downloading'}
              <div class="update-card" role="status">
                <strong
                  >Installing {availableUpdate
                    ? versionLabel(availableUpdate.version)
                    : 'update'}…</strong
                >
                <p>Blackwall will restart when the signed update is ready.</p>
                <div
                  class:indeterminate={!updateProgress.totalBytes}
                  class="progress-track"
                  aria-label="Update download progress"
                  aria-valuemin="0"
                  aria-valuemax="100"
                  aria-valuenow={downloadPercent(updateProgress)}
                  role="progressbar"
                >
                  <span style={`width: ${downloadPercent(updateProgress)}%`}></span>
                </div>
              </div>
            {:else if availableUpdate}
              <div class="update-card available">
                <div>
                  <span class="update-label">Update available</span>
                  <strong>{versionLabel(availableUpdate.version)}</strong>
                </div>
                {#if availableUpdate.body}
                  <p class="release-notes">{availableUpdate.body}</p>
                {/if}
                {#if updateError}
                  <p class="update-error" role="alert">{updateError}</p>
                {/if}
                <button class="install-button" onclick={onInstallUpdate}
                  >Install update and restart</button
                >
              </div>
            {:else}
              <div class="update-status">
                {#if updateState === 'checking'}
                  <span class="spinner"></span>
                  <span>Checking GitHub for a signed update…</span>
                {:else if updateState === 'current'}
                  <Icon name="check" size={15} />
                  <span
                    >Blackwall is up to date on the {updateChannel === 'beta' ? 'beta' : 'stable'} channel.</span
                  >
                {:else if updateState === 'unsupported'}
                  <span>Updates are available in the installed desktop app.</span>
                {:else if updateState === 'error'}
                  <span class="update-error" role="alert">{updateError}</span>
                {:else}
                  <span>Blackwall checks for updates when it starts.</span>
                {/if}
              </div>
              <button
                class="check-button"
                disabled={updateState === 'checking' || updateState === 'unsupported'}
                onclick={onCheckForUpdates}
              >
                {updateState === 'checking' ? 'Checking…' : 'Check for updates'}
              </button>
            {/if}
          </section>
        {/if}
        </main>
      </div>
    </div>
  </div>
{/if}

<style>
  .key-connection {
    width: 100%;
    margin: 8px 0;
    padding: 8px 30px 8px 8px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background-color: var(--bg-base);
    color: var(--text-primary);
    font: inherit;
  }
  .menu-controls {
    display: flex;
    flex-direction: column;
    gap: 12px;
    margin-bottom: 20px;
  }
  .settings-search {
    display: flex;
    align-items: center;
    gap: 8px;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-sm);
    padding: 0 10px;
    color: var(--text-muted);
  }
  .settings-search:focus-within {
    border-color: var(--accent);
  }
  .settings-search input {
    min-width: 0;
    width: 100%;
    height: 36px;
    border: 0;
    background: transparent;
    color: var(--text-primary);
    outline: none;
    font-size: 13px;
  }
  .settings-group + .settings-group {
    margin-top: 22px;
  }
  .settings-group h3 {
    padding: 0 10px 8px;
    color: var(--text-muted);
    font-size: 12px;
    font-weight: 600;
  }
  .preference-button {
    display: flex;
    width: 100%;
    align-items: center;
    gap: 12px;
    padding: 12px 10px;
    min-height: 44px;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-muted);
    text-align: left;
  }
  .preference-button:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  .preference-button span {
    display: flex;
    flex: 1;
    min-width: 0;
    flex-direction: column;
    gap: 4px;
  }
  .preference-button strong {
    color: var(--text-primary);
    font-size: 14px;
    font-weight: 500;
  }
  .no-settings {
    padding: 16px 10px;
    color: var(--text-muted);
    font-size: 13px;
    line-height: 1.6;
    overflow-wrap: anywhere;
  }

  .access-keys {
    border-bottom: 1px solid var(--border);
    padding-bottom: 20px;
    margin-bottom: 20px;
  }
  .access-keys summary {
    cursor: pointer;
    font-size: 14px;
    font-weight: 600;
  }
  .access-keys > p {
    font-size: 12px;
    line-height: 1.6;
    color: var(--text-muted);
  }

  .guest-list {
    border-top: 1px solid var(--border);
    padding-top: 16px;
  }
  .guest-list h4 {
    margin: 0 0 10px;
    font-size: 12px;
  }
  .guest-list > p {
    font-size: 11px;
    line-height: 1.5;
    color: var(--text-muted);
    margin-top: 10px;
  }
  .guest-row {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 7px;
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 4px;
  }
  .guest-select {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
    background: transparent;
    padding: 9px;
    text-align: left;
  }
  .guest-select strong {
    font-size: 12px;
    overflow-wrap: anywhere;
  }
  .guest-select small {
    font-size: 10px;
    color: var(--text-muted);
  }
  .guest-select.selected strong {
    color: var(--accent);
  }
  .guest-revoke {
    padding: 9px;
    border-radius: 6px;
    background: transparent;
    color: var(--err);
    font-size: 11px;
  }

  .connection-start {
    display: grid;
    gap: 14px;
    margin: 16px 0 22px;
  }
  .setup-button {
    justify-self: start;
    padding: 0 18px;
  }
  .address-form {
    margin-bottom: 22px;
  }
  .address-row {
    display: flex;
    gap: 8px;
  }
  .address-row input {
    flex: 1;
    min-width: 0;
  }
  .address-row .connect-button {
    padding: 0 16px;
  }
  .modal-layer {
    position: fixed;
    z-index: 100;
    inset: 0 0 0 var(--sidebar-width);
    background: var(--bg-panel);
  }

  .settings-dialog {
    position: relative;
    display: flex;
    width: 100%;
    height: 100%;
    flex-direction: column;
    overflow: hidden;
    border-left: 1px solid var(--border);
    background: var(--bg-panel);
  }

  .settings-layout {
    display: grid;
    grid-template-columns: 250px minmax(0, 1fr);
    min-height: 0;
    flex: 1;
  }
  .settings-navigation {
    overflow-y: auto;
    border-right: 1px solid var(--border);
    padding: 20px 14px;
  }

  header {
    display: flex;
    min-height: 72px;
    align-items: center;
    justify-content: space-between;
    border-bottom: 1px solid var(--border);
    padding: 14px 16px 14px 20px;
  }

  h2,
  h3,
  p {
    margin: 0;
  }

  h2 {
    font-size: 16px;
    font-weight: 650;
    letter-spacing: -0.015em;
  }

  header p,
  .section-heading p,
  .qr-wrap p {
    color: var(--text-muted);
    font-size: 12px;
  }

  .icon-button {
    display: grid;
    width: 32px;
    height: 32px;
    place-items: center;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-muted);
  }

  .icon-button:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }

  .dialog-content {
    overflow-y: auto;
    padding: 32px;
    min-height: 0;
  }

  .connection-section,
  .update-section,
  .share-section {
    display: flex;
    flex-direction: column;
    gap: 17px;
  }

  #update-channel {
    width: 100%;
    padding: 10px 30px 10px 12px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background-color: var(--bg-elevated);
    color: var(--text-primary);
    font-size: 13px;
  }

  .channel-label {
    font-size: 12px;
    color: var(--text-secondary);
  }

  .channel-help {
    font-size: 12px;
    color: var(--text-muted);
    line-height: 1.5;
  }

  .connection-section,
  .update-section {
    margin-bottom: 20px;
    border-bottom: 1px solid var(--border);
    padding-bottom: 20px;
  }

  .section-heading {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 16px;
  }

  h3 {
    margin-bottom: 3px;
    font-size: 14px;
    font-weight: 630;
  }

  .live-badge {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    border: 1px solid #4ade8033;
    border-radius: var(--radius-pill);
    padding: 3px 9px;
    background: #4ade800c;
    color: var(--ok);
    font-size: 10.5px;
    font-weight: 650;
    text-transform: uppercase;
  }

  .live-badge span {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--ok);
  }

  .connection-badge {
    display: inline-flex;
    flex: 0 0 auto;
    align-items: center;
    gap: 6px;
    border: 1px solid #f8717133;
    border-radius: var(--radius-pill);
    padding: 3px 9px;
    background: #f871710c;
    color: var(--err);
    font-size: 10.5px;
    font-weight: 650;
    text-transform: uppercase;
  }

  .connection-badge span {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: currentColor;
  }

  .connection-badge.connected {
    border-color: #4ade8033;
    background: #4ade800c;
    color: var(--ok);
  }

  .connection-badge.checking {
    border-color: #fbbf2433;
    background: #fbbf240c;
    color: var(--warn);
  }

  .endpoint-form,
  .relay-form {
    display: flex;
    flex-direction: column;
    gap: 9px;
  }

  .endpoint-form label,
  .relay-form label {
    color: var(--text-muted);
    font-size: 11.5px;
    font-weight: 600;
  }

  .endpoint-form input,
  .relay-form input {
    width: 100%;
    min-height: 40px;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-sm);
    padding: 0 11px;
    background: var(--bg-elevated);
    color: var(--text-primary);
    font-family: var(--font-code);
    font-size: 11.5px;
  }

  .endpoint-form input::placeholder,
  .relay-form input::placeholder {
    color: var(--text-faint);
  }

  .endpoint-form input:disabled,
  .relay-form input:disabled {
    opacity: 0.65;
  }

  .endpoint-hint,
  .relay-hint,
  .connection-error,
  .connection-success {
    font-size: 11.5px;
    line-height: 1.5;
  }

  .endpoint-hint {
    color: var(--text-faint);
  }

  .text-tool-calls {
    display: flex;
    gap: 10px;
    align-items: flex-start;
    margin: 4px 0;
    cursor: pointer;
  }
  .text-tool-calls input {
    margin-top: 3px;
  }
  .text-tool-calls span {
    display: grid;
    gap: 2px;
  }
  .text-tool-calls strong {
    font-size: 12.5px;
    font-weight: 600;
  }
  .text-tool-calls small {
    color: var(--text-muted);
    font-size: 11.5px;
    line-height: 1.5;
  }

  .detected-server {
    margin: 0;
    color: var(--text-muted);
    font-size: 11.5px;
    line-height: 1.5;
  }

  .relay-form {
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 12px 13px;
    background: var(--bg-elevated);
  }

  .relay-form label span,
  .relay-hint {
    color: var(--text-faint);
  }

  .relay-form label span {
    font-weight: 450;
  }

  .endpoint-hint code {
    color: var(--text-muted);
    font-family: var(--font-code);
    font-size: 10.5px;
  }

  .connection-error {
    border: 1px solid #f8717129;
    border-radius: var(--radius-sm);
    padding: 9px 10px;
    background: #f8717108;
    color: var(--err);
  }

  .connection-success {
    color: var(--ok);
  }

  .version-badge {
    flex: 0 0 auto;
    border: 1px solid var(--border);
    border-radius: var(--radius-pill);
    padding: 3px 9px;
    background: var(--bg-elevated);
    color: var(--text-muted);
    font-family: var(--font-code);
    font-size: 10.5px;
  }

  .update-card,
  .update-status {
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 12px 13px;
    background: var(--bg-elevated);
  }

  .update-card {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .update-card.available {
    border-color: #39c5a833;
    background: #39c5a808;
  }

  .update-card > div:first-child {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
  }

  .update-card strong {
    font-size: 12.5px;
  }

  .update-label {
    color: var(--accent);
    font-size: 11.5px;
    font-weight: 620;
  }

  .release-notes,
  .update-card > p,
  .update-status {
    color: var(--text-muted);
    font-size: 11.5px;
    line-height: 1.5;
  }

  .release-notes {
    max-height: 88px;
    overflow-y: auto;
    white-space: pre-line;
  }

  .update-status {
    display: flex;
    min-height: 44px;
    align-items: center;
    gap: 9px;
  }

  .update-status :global(svg) {
    color: var(--ok);
  }

  .update-error {
    color: var(--err);
  }

  .progress-track {
    position: relative;
    height: 5px;
    overflow: hidden;
    border-radius: var(--radius-pill);
    background: var(--border);
  }

  .progress-track span {
    display: block;
    height: 100%;
    border-radius: inherit;
    background: var(--accent);
    transition: width var(--transition-fast);
  }

  .progress-track.indeterminate span {
    width: 35% !important;
    animation: update-progress 1s ease-in-out infinite alternate;
  }

  @keyframes update-progress {
    from {
      transform: translateX(-30%);
    }
    to {
      transform: translateX(215%);
    }
  }

  .loading-row {
    display: flex;
    min-height: 180px;
    align-items: center;
    justify-content: center;
    gap: 9px;
    color: var(--text-muted);
    font-size: 12px;
  }

  .spinner {
    width: 14px;
    height: 14px;
    border: 2px solid var(--border-strong);
    border-top-color: var(--accent);
    border-radius: 50%;
    animation: spin 700ms linear infinite;
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  .inactive-share,
  .active-share {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .model-row,
  .share-summary {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 20px;
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 12px 13px;
    background: var(--bg-elevated);
  }

  .model-row span,
  .summary-label,
  .link-label {
    color: var(--text-faint);
    font-size: 10.5px;
    font-weight: 650;
    letter-spacing: 0.06em;
    text-transform: uppercase;
  }

  .model-row strong {
    overflow: hidden;
    color: var(--text-muted);
    font-size: 12px;
    font-weight: 550;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  fieldset {
    min-width: 0;
    margin: 0;
    border: 0;
    padding: 0;
  }

  legend {
    margin-bottom: 8px;
    color: var(--text-muted);
    font-size: 11.5px;
  }

  .expiry-options {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    gap: 7px;
  }

  .expiry-options label {
    display: grid;
    min-height: 38px;
    place-items: center;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--bg-elevated);
    color: var(--text-muted);
    cursor: pointer;
    font-size: 12px;
    transition: all var(--transition-fast);
  }

  .expiry-options label:hover {
    border-color: var(--border-strong);
    color: var(--text-primary);
  }

  .expiry-options label.checked {
    border-color: var(--accent);
    background: var(--accent-muted);
    color: var(--accent);
  }

  .expiry-options input {
    position: absolute;
    width: 1px;
    height: 1px;
    opacity: 0;
  }

  .notice {
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 10px 11px;
    background: #ffffff03;
    color: var(--text-muted);
    font-size: 11.5px;
    line-height: 1.5;
  }

  .notice.warning {
    border-color: #fbbf2429;
    background: #fbbf2408;
    color: #d6bd7c;
  }

  .connect-button,
  .install-button,
  .start-button,
  .stop-button {
    min-height: 40px;
    border-radius: var(--radius-sm);
    font-weight: 620;
  }

  .connect-button,
  .install-button,
  .start-button {
    background: var(--accent);
    color: var(--text-on-accent);
  }

  .connect-button:hover:not(:disabled),
  .install-button:hover:not(:disabled),
  .start-button:hover:not(:disabled) {
    background: var(--accent-hover);
  }

  .connect-button:disabled,
  .install-button:disabled,
  .start-button:disabled {
    opacity: 0.45;
  }

  .check-button {
    min-height: 38px;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-sm);
    background: var(--bg-elevated);
    color: var(--text-muted);
    font-weight: 590;
  }

  .check-button:hover:not(:disabled) {
    border-color: var(--text-faint);
    color: var(--text-primary);
  }

  .check-button:disabled {
    opacity: 0.5;
  }

  .stop-button {
    border: 1px solid #f8717133;
    background: #f871710c;
    color: var(--err);
  }

  .stop-button:hover:not(:disabled) {
    background: #f8717117;
  }

  .share-summary {
    align-items: stretch;
  }

  .share-summary > div {
    display: flex;
    min-width: 0;
    flex: 1;
    flex-direction: column;
    gap: 2px;
  }

  .share-summary > div + div {
    border-left: 1px solid var(--border);
    padding-left: 14px;
  }

  .share-summary strong {
    overflow: hidden;
    color: var(--text-muted);
    font-size: 11.5px;
    font-weight: 540;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .qr-wrap {
    display: flex;
    align-items: center;
    gap: 14px;
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 13px;
    background: var(--bg-elevated);
  }

  .qr-wrap img {
    width: 112px;
    height: 112px;
    flex: 0 0 auto;
    border: 7px solid white;
    border-radius: 8px;
    background: white;
  }

  .qr-wrap strong {
    display: block;
    margin-bottom: 3px;
    font-size: 13px;
  }

  .link-label {
    margin-bottom: -8px;
  }

  .link-field {
    display: flex;
    min-width: 0;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--bg-base);
  }

  .link-field:focus-within {
    border-color: var(--accent);
    box-shadow: 0 0 0 2px var(--focus-ring);
  }

  .link-field input {
    min-width: 0;
    height: 40px;
    flex: 1;
    border: 0;
    outline: 0;
    padding: 0 10px;
    background: transparent;
    color: var(--text-muted);
    font-family: var(--font-code);
    font-size: 10.5px;
  }

  .link-field button {
    display: flex;
    min-width: 73px;
    align-items: center;
    justify-content: center;
    gap: 6px;
    margin: 4px;
    border-radius: 5px;
    background: var(--bg-hover);
    color: var(--text-muted);
    font-size: 11.5px;
  }

  .link-field button:hover,
  .link-field button.copied {
    color: var(--accent);
  }

  .share-meta {
    display: flex;
    justify-content: space-between;
    color: var(--text-faint);
    font-size: 10.5px;
  }

  .error {
    border-radius: var(--radius-sm);
    padding: 9px 11px;
    background: #f871710d;
    color: var(--err);
    font-size: 11.5px;
  }

  .privacy-note {
    border-top: 1px solid var(--border);
    padding-top: 14px;
    color: var(--text-faint);
    font-size: 10.5px;
    line-height: 1.5;
  }

  @media (max-width: 760px) {
    .modal-layer {
      inset: 0;
    }

    .settings-dialog {
      width: 100%;
      height: 100%;
      border-right: 0;
      border-bottom: 0;
      border-left: 0;
      border-radius: 0;
    }
    .settings-layout {
      grid-template-columns: 190px minmax(0, 1fr);
    }
    .dialog-content {
      padding: 20px;
    }
  }
</style>
