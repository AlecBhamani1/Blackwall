<script lang="ts">
  import { onDestroy, tick } from 'svelte';
  import {
    setupClient,
    STARTER_MODELS,
    connectionHelp,
    invitationUrl,
    isDesktop,
    type SetupClient,
    type LocalService,
    type DownloadProgress,
  } from '../setup';
  import { persistence, type ConnectionProfile } from '../persistence';
  import LogoMark from './LogoMark.svelte';
  import Icon from './Icon.svelte';
  import PairingPanel from './PairingPanel.svelte';

  export let onConnect: (endpoint: string, name?: string) => Promise<boolean>;
  export let connections: ConnectionProfile[] = [];
  export let onForget: (endpoint: string) => void = () => {};
  export let onDone: () => void = () => {};
  export let onChooseModel: (model: string) => void = () => {};
  export let client: SetupClient = setupClient;
  export let desktop = isDesktop();
  export let currentEndpoint = '';
  export let connectionError = '';

  type Step = 'choose' | 'local' | 'remote' | 'invite' | 'advanced' | 'success';
  let step: Step = 'choose';
  let busy = false;
  let savingKey = false;
  let scanning = false;
  let services: LocalService[] = [];
  let scanned = false;
  // Setup can first appear because the automatic saved-connection check failed.
  let error = connectionError;
  let address = currentEndpoint;
  let accessKey = '';
  let connectionName = '';
  let invitation = '';
  let invitationOpened = false;
  let model = STARTER_MODELS[0].id;
  let progress: DownloadProgress | null = null;
  let download: AbortController | null = null;
  let operation = 0;
  let heading: HTMLHeadingElement;
  let errorMessage: HTMLDivElement | undefined;
  let connectedName = '';

  async function navigate(next: Step) {
    if (busy) return;
    operation += 1;
    scanning = false;
    error = '';
    if (next !== 'advanced') accessKey = '';
    step = next;
    await tick();
    heading?.focus();
    if (next === 'local') void scan();
  }
  async function scan() {
    const version = ++operation;
    scanning = true;
    error = '';
    try {
      const found = await client.discover();
      if (version !== operation) return;
      services = found;
      scanned = true;
    } catch (cause) {
      if (version === operation) error = connectionHelp(cause);
    } finally {
      if (version === operation) scanning = false;
    }
  }
  async function connect(endpoint: string, name: string) {
    if (busy) return;
    busy = true;
    error = '';
    try {
      if (step === 'advanced' && accessKey.trim()) {
        savingKey = true;
        await persistence.saveKey(endpoint, accessKey);
        savingKey = false;
        accessKey = '';
      }
      if (await onConnect(endpoint, name)) {
        connectedName = name;
        step = 'success';
        await tick();
        heading?.focus();
      } else
        error =
          connectionError ||
          'The connection could not be completed. Check that the model service is open and try again.';
    } catch (cause) {
      const detail =
        cause instanceof Error ? cause.message : typeof cause === 'string' ? cause : '';
      error =
        savingKey && /keychain/i.test(detail)
          ? 'Blackwall could not save the access key in Keychain. Respond to any macOS Keychain prompt, unlock your login keychain if needed, then try again.'
          : connectionHelp(cause);
    } finally {
      savingKey = false;
      busy = false;
      if (error) {
        await tick();
        errorMessage?.focus();
      }
    }
  }
  async function startDownload(service: LocalService) {
    busy = true;
    error = '';
    progress = { status: 'Starting download', completed: 0, total: 0 };
    const controller = new AbortController();
    download = controller;
    try {
      await client.download(model, (next) => (progress = next), controller.signal);
      if (controller.signal.aborted) return;
      if (await onConnect(service.endpoint, 'This computer')) {
        onChooseModel(model);
        connectedName = 'This computer';
        step = 'success';
        await tick();
        heading?.focus();
      } else error = 'The model downloaded, but Blackwall could not connect. Try checking again.';
    } catch (cause) {
      error = controller.signal.aborted
        ? 'Download stopped. You can try again whenever you are ready.'
        : typeof cause === 'string'
          ? cause
          : cause instanceof Error
            ? cause.message
            : 'The download could not finish. Check your internet connection and try again.';
    } finally {
      download = null;
      progress = null;
      busy = false;
    }
  }
  async function openDownload() {
    try {
      await client.openLink('download');
    } catch {
      error = 'Your browser could not be opened. Visit ollama.com/download/mac to install Ollama.';
    }
  }
  async function openInvitation(event: SubmitEvent) {
    event.preventDefault();
    error = '';
    busy = true;
    try {
      await client.openLink('invitation', invitationUrl(invitation));
      invitation = '';
      invitationOpened = true;
    } catch (cause) {
      error =
        cause instanceof Error
          ? cause.message
          : 'The invitation could not be opened. Ask the sender for a new link.';
    } finally {
      busy = false;
    }
  }
  function readableBytes(bytes: number): string {
    return `${(bytes / 1_000_000_000).toFixed(1)} GB`;
  }
  $: available = services.filter((service) => service.available);
  $: percent =
    progress && progress.total > 0
      ? Math.min(100, Math.round((progress.completed / progress.total) * 100))
      : undefined;
  onDestroy(() => {
    operation += 1;
    download?.abort();
  });
</script>

<div class="setup-scroll">
  <div class="setup">
    <div class="setup-top">
      <div class="brand"><LogoMark size={26} /><span>Blackwall</span></div>
      <span class="eyebrow">{step === 'success' ? 'Ready to chat' : 'Connection setup'}</span>
    </div>
    {#if step !== 'choose' && step !== 'success'}
      <button class="back" disabled={busy} onclick={() => navigate('choose')}
        ><Icon name="chevron-left" size={14} />All connection options</button
      >
    {/if}

    {#if step === 'choose'}
      <h1 bind:this={heading} tabindex="-1">Your AI. Your choice.</h1>
      <p class="intro">Choose where your model runs.<br />We’ll help you get connected.</p>
      {#if error}
        <div class="error" role="alert" tabindex="-1" bind:this={errorMessage}>{error}</div>
      {/if}
      {#if connections.length}
        <div class="remembered" aria-label="Saved connections">
          <p class="saved-label">Your computers</p>
          {#each connections as profile}
            <div class="saved-row">
              <button
                aria-label={`Connect to ${profile.name}`}
                disabled={busy}
                onclick={() => connect(profile.endpoint, profile.name)}
                ><span class="footer-dot"></span><strong>{profile.name}</strong><span
                  >{busy ? 'Connecting…' : 'Connect'}</span
                ></button
              ><button
                class="forget"
                aria-label={`Forget ${profile.name}`}
                disabled={busy}
                onclick={() => onForget(profile.endpoint)}><Icon name="x" size={14} /></button
              >
            </div>
          {/each}
        </div>
      {/if}
      <div class="choices">
        <button class="choice" onclick={() => navigate('local')}>
          <span class="choice-icon"
            ><svg
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              stroke-width="1.5"
              aria-hidden="true"
              ><rect x="3" y="4" width="18" height="13" rx="2" /><path d="M8 21h8M12 17v4" /></svg
            ></span
          >
          <span
            ><strong>Use this computer <span class="tag">Start here</span></strong><small
              >Run a model locally. Your chats stay on this computer.</small
            ></span
          >
          <Icon name="chevron-right" size={16} />
        </button>
        <button class="choice" onclick={() => navigate('remote')}>
          <span class="choice-icon"
            ><svg
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              stroke-width="1.5"
              aria-hidden="true"
              ><rect x="2" y="4" width="8" height="7" rx="1.5" /><rect
                x="14"
                y="13"
                width="8"
                height="7"
                rx="1.5"
              /><path d="M14 7h4v3M10 17H6v-3" /></svg
            ></span
          >
          <span
            ><strong>Use another computer</strong><small
              >Connect to a model running on a computer you control.</small
            ></span
          >
          <Icon name="chevron-right" size={16} />
        </button>
        <button class="choice" onclick={() => navigate('invite')}>
          <span class="choice-icon"><Icon name="link" size={22} /></span>
          <span
            ><strong>I have an invitation</strong><small
              >Open a shared model in your browser. No installation needed.</small
            ></span
          >
          <Icon name="chevron-right" size={16} />
        </button>
      </div>
      <button class="text-button advanced-link" onclick={() => navigate('advanced')}
        >Advanced connection settings</button
      >
    {:else if step === 'local'}
      <h1 bind:this={heading} tabindex="-1">Let’s find your local model.</h1>
      <p class="intro">Blackwall checks for Ollama and LM Studio on this computer.</p>
      {#if !desktop}
        <div class="info">
          <strong>Open the Blackwall desktop app</strong>
          <p>
            Local discovery and model downloads are available in the installed app. You can still
            connect manually in this browser.
          </p>
        </div>
        <button class="primary" onclick={() => navigate('advanced')}>Connect manually</button>
      {:else if scanning}
        <div class="checking" role="status">
          <span class="spinner"></span>Looking for local models…
        </div>
      {:else if scanned && available.length === 0}
        <div class="info">
          <strong>Set up Ollama to get started</strong>
          <p>
            Ollama runs the model on your Mac. Install it, open it, then return here. If you already
            use LM Studio, start its local server.
          </p>
        </div>
        <div class="actions">
          <button class="primary" onclick={openDownload}
            >Get Ollama <Icon name="external-link" size={14} /></button
          ><button class="secondary" onclick={scan}>I’ve opened it · Check again</button>
        </div>
        <p class="footnote">
          The installer opens on Ollama’s official website. Nothing is installed automatically.
        </p>
      {:else}
        {#each available as service}
          <div class="service">
            <div class="service-heading">
              <strong>{service.name}</strong><span class="found"
                ><span></span>Found on this computer</span
              >
            </div>
            {#if service.models.length > 0}
              <p>
                {service.models.length}
                {service.models.length === 1 ? 'model is' : 'models are'} ready to use.
              </p>
              <div class="model-names">
                {service.models.slice(0, 3).join(' · ')}{service.models.length > 3 ? ' …' : ''}
              </div>
              <button
                class="primary"
                disabled={busy}
                onclick={() => connect(service.endpoint, 'This computer')}
                >{busy ? 'Connecting…' : 'Use these models'}</button
              >
            {:else if service.supportsDownload}
              <p>One more step: download your first model.</p>
              <fieldset disabled={busy}>
                <legend class="sr-only">Starter model</legend>
                {#each STARTER_MODELS as option}
                  <label class:selected={model === option.id} class="model-option"
                    ><input
                      type="radio"
                      bind:group={model}
                      value={option.id}
                      name="starter-model"
                    /><span><strong>{option.name}</strong><small>{option.description}</small></span
                    ><span class="size">{option.size}</span></label
                  >
                {/each}
              </fieldset>
              {#if progress}
                <div class="download-progress" role="status">
                  <div>
                    <span
                      >{progress.total > 0
                        ? 'Downloading model…'
                        : progress.status === 'success'
                          ? 'Connecting…'
                          : 'Preparing your model…'}</span
                    ><span>{percent === undefined ? '' : `${percent}%`}</span>
                  </div>
                  <progress max="100" value={percent} aria-label="Model download progress"
                  ></progress>{#if progress.total > 0}<small
                      >{readableBytes(progress.completed)} of {readableBytes(progress.total)}</small
                    >{/if}
                </div>
                <button class="secondary" onclick={() => download?.abort()}>Stop download</button>
              {:else}
                <button class="primary" disabled={busy} onclick={() => startDownload(service)}
                  >{busy ? 'Connecting…' : 'Download and connect'}</button
                >
                <p class="footnote">
                  Downloads require internet and free disk space. These starter models support text;
                  image chat needs a vision model.
                </p>
              {/if}
            {:else}
              <p>Load a model in LM Studio, start its local server, then check again.</p>
              <button class="secondary" disabled={busy} onclick={scan}>Check again</button>
            {/if}
          </div>
        {/each}
        <button class="text-button" disabled={busy} onclick={scan}>Check for models again</button>
      {/if}
    {:else if step === 'remote'}
      <h1 bind:this={heading} tabindex="-1">Your other computer. Ready when you are.</h1>
      <PairingPanel
        {desktop}
        onConnect={(endpoint, name) => connect(endpoint, name).then(() => step === 'success')}
        {onForget}
      />
      <button class="text-button advanced-link" onclick={() => navigate('advanced')}
        >Connect with a model address instead</button
      >
    {:else if step === 'advanced'}
      <h1 bind:this={heading} tabindex="-1">Connect a model service.</h1>
      <p class="intro">Use the address of an Ollama or OpenAI-compatible service.</p>
      <form
        onsubmit={(event) => {
          event.preventDefault();
          void connect(address, connectionName.trim() || 'Model service');
        }}
      >
        <label for="connection-name">Connection name</label><input
          id="connection-name"
          bind:value={connectionName}
          placeholder="For example, My model service"
          maxlength={80}
          disabled={busy}
        /><label for="setup-address">Model service address</label>
        <input
          id="setup-address"
          bind:value={address}
          placeholder="http://my-computer.local:11434"
          autocomplete="url"
          autocapitalize="none"
          spellcheck="false"
          disabled={busy}
          required
        />
        <p class="footnote">
          Find this in your model service’s server settings. Only reachable computers can connect;
          keep raw model servers off the public internet.
        </p>
        {#if desktop}<details class="key-settings">
            <summary>Access key (if your service requires one)</summary><label for="setup-key"
              >Access key</label
            ><input
              id="setup-key"
              type="password"
              bind:value={accessKey}
              autocomplete="off"
              disabled={busy}
            />
            <p class="footnote">
              Saved securely in macOS Keychain for this service. Leave blank to use an existing key.
            </p>
          </details>{/if}
        <button class="primary" disabled={busy || !address.trim()} type="submit"
          >{savingKey
            ? 'Verifying and saving key…'
            : busy
              ? 'Testing connection…'
              : 'Connect and continue'}</button
        >
        {#if savingKey}
          <p role="status">
            Respond to any macOS Keychain prompt and wait for confirmation before retrying.
          </p>
        {/if}
      </form>
    {:else if step === 'invite'}
      <h1 bind:this={heading} tabindex="-1">You’re invited.</h1>
      <p class="intro">Paste the full link you received. Your shared chat opens in your browser.</p>
      {#if invitationOpened}<div class="info" role="status">
          <strong>Invitation opened</strong>
          <p>Continue in your browser. If the link has expired, ask the sender for a new one.</p>
        </div>{/if}
      <form onsubmit={openInvitation}>
        <label for="setup-invitation">Invitation link</label><input
          id="setup-invitation"
          type="password"
          bind:value={invitation}
          placeholder="Paste your Blackwall invitation"
          autocomplete="off"
          spellcheck="false"
          disabled={busy}
          required
        />
        <p class="footnote">
          The link includes private access. Blackwall does not save it. Only open invitations from
          someone you trust.
        </p>
        <button class="primary" type="submit" disabled={busy || !invitation.trim()}
          >{busy ? 'Opening…' : 'Open invitation'}</button
        >
      </form>
    {:else if step === 'success'}
      <div class="success-mark"><Icon name="check" size={26} /></div>
      <h1 bind:this={heading} tabindex="-1">You’re connected.</h1>
      <p class="intro">
        {connectedName} is ready.<br />Start a conversation, or choose a model from the top bar.
      </p>
      <button class="primary" onclick={onDone}
        >Start chatting <Icon name="chevron-right" size={16} /></button
      >
    {/if}
    {#if error && step !== 'choose'}<div
        class="error"
        role="alert"
        tabindex="-1"
        bind:this={errorMessage}
      >
        {error}
      </div>{/if}
    <div class="setup-footer">
      <span class="footer-dot"></span>No model runs until you choose one.
    </div>
  </div>
</div>

<style>
  .remembered {
    margin: 24px 0;
  }
  .saved-label {
    font-size: 11px;
    color: var(--text-muted);
  }
  .saved-row {
    display: flex;
    gap: 6px;
    border-bottom: 1px solid var(--border);
  }
  .saved-row > button:first-child {
    display: flex;
    min-width: 0;
    align-items: center;
    gap: 12px;
    flex: 1;
    padding: 14px 0;
    background: transparent;
    text-align: left;
  }
  .saved-row strong {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .saved-row span:last-child {
    color: var(--accent);
    font-size: 12px;
  }
  .forget {
    background: transparent;
    color: var(--text-muted);
    padding: 12px;
  }

  .setup-scroll {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    overscroll-behavior: contain;
  }
  .setup {
    width: min(100%, 640px);
    margin: auto;
    padding: 44px 30px 30px;
  }
  .setup-top {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    margin-bottom: 42px;
  }
  .brand {
    display: flex;
    gap: 9px;
    align-items: center;
    font-weight: 650;
  }
  .eyebrow {
    font-size: 11px;
    color: var(--text-muted);
  }
  h1 {
    font-size: clamp(25px, 3vw, 32px);
    font-weight: 620;
    letter-spacing: -0.035em;
    line-height: 1.2;
    margin: 0 0 12px;
    outline: none;
  }
  .intro {
    font-size: 14px;
    color: var(--text-muted);
    line-height: 1.7;
    margin: 0 0 28px;
    max-width: 500px;
  }
  .choices {
    display: grid;
    gap: 10px;
  }
  .choice {
    width: 100%;
    display: flex;
    gap: 16px;
    align-items: center;
    padding: 20px;
    background: var(--bg-panel);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    text-align: left;
    transition:
      border-color var(--transition-fast),
      background var(--transition-fast);
  }
  .choice:hover {
    border-color: var(--border-strong);
    background: var(--bg-elevated);
  }
  .choice > span:nth-child(2) {
    flex: 1;
    min-width: 0;
  }
  .choice strong {
    display: block;
    font-size: 14px;
    font-weight: 600;
  }
  .choice small {
    display: block;
    color: var(--text-muted);
    font-size: 12px;
    line-height: 1.6;
    margin-top: 4px;
  }
  .choice-icon {
    color: var(--text-muted);
    flex: 0 0 28px;
    display: grid;
    place-items: center;
  }
  .choice-icon svg {
    width: 25px;
    height: 25px;
  }
  .tag {
    display: inline-block;
    margin-left: 6px;
    font-size: 10px;
    font-weight: 500;
    color: var(--text-muted);
    border: 1px solid var(--border-strong);
    padding: 1px 6px;
    border-radius: var(--radius-pill);
    vertical-align: 1px;
  }
  .text-button,
  .back {
    padding: 0;
    background: transparent;
    color: var(--text-muted);
    font-size: 12px;
    text-align: left;
  }
  .text-button:hover,
  .back:hover {
    color: var(--text-primary);
  }
  .advanced-link {
    margin-top: 22px;
  }
  .back {
    display: flex;
    align-items: center;
    gap: 5px;
    margin: -18px 0 24px;
  }
  .info,
  .service {
    border: 1px solid var(--border);
    background: var(--bg-panel);
    border-radius: var(--radius-md);
    padding: 20px;
    margin-bottom: 18px;
  }
  .info p,
  .service p {
    color: var(--text-muted);
    font-size: 13px;
    line-height: 1.65;
    margin: 7px 0 12px;
  }
  .info p:last-child {
    margin-bottom: 0;
  }
  .info strong,
  .service strong {
    font-weight: 580;
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
  }
  .primary,
  .secondary {
    min-height: 40px;
    display: inline-flex;
    gap: 9px;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-sm);
    padding: 9px 15px;
    font-size: 12.5px;
    font-weight: 580;
  }
  .primary {
    background: var(--accent);
    color: var(--text-on-accent);
  }
  .primary:hover:not(:disabled) {
    background: var(--accent-hover);
  }
  .secondary {
    background: var(--bg-elevated);
    border: 1px solid var(--border-strong);
  }
  button:disabled {
    opacity: 0.55;
    cursor: wait;
  }
  .footnote {
    color: var(--text-muted);
    font-size: 11.5px;
    line-height: 1.65;
    margin: 12px 0 18px;
  }
  .service-heading {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 12px;
    flex-wrap: wrap;
  }
  .found {
    display: inline-flex;
    gap: 6px;
    align-items: center;
    font-size: 11px;
    color: var(--text-muted);
  }
  .found span {
    background: var(--ok);
    height: 5px;
    width: 5px;
    border-radius: 50%;
  }
  .model-names {
    color: var(--text-muted);
    font-family: var(--font-code);
    font-size: 11px;
    overflow-wrap: anywhere;
    margin-bottom: 18px;
  }
  fieldset {
    border: 0;
    padding: 0;
    margin: 16px 0;
    display: grid;
    gap: 8px;
  }
  .model-option {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 12px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
  }
  .model-option.selected {
    border-color: var(--accent);
  }
  .model-option input {
    accent-color: var(--accent);
  }
  .model-option > span:first-of-type {
    flex: 1;
  }
  .model-option strong {
    display: block;
    font-size: 12px;
  }
  .model-option small {
    display: block;
    color: var(--text-muted);
    font-size: 11px;
    line-height: 1.5;
    margin-top: 4px;
  }
  .size {
    font-size: 11px;
    color: var(--text-muted);
    white-space: nowrap;
  }
  .key-settings {
    margin-bottom: 20px;
  }
  .key-settings summary {
    font-size: 12px;
    color: var(--text-muted);
    cursor: pointer;
    margin-bottom: 12px;
  }
  .key-settings label {
    display: block;
    font-size: 12px;
    margin-bottom: 8px;
  }
  .key-settings input {
    width: 100%;
    background: var(--bg-panel);
    border: 1px solid var(--border-strong);
    padding: 10px;
    border-radius: var(--radius-sm);
  }
  form > label {
    display: block;
    margin-bottom: 8px;
    font-size: 12px;
    font-weight: 550;
  }
  form > input {
    width: 100%;
    min-height: 44px;
    background: var(--bg-panel);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-sm);
    padding: 10px 12px;
    font-size: 13px;
  }
  .checking {
    display: flex;
    align-items: center;
    gap: 12px;
    min-height: 100px;
    color: var(--text-muted);
  }
  .spinner {
    width: 15px;
    height: 15px;
    border: 2px solid var(--border-strong);
    border-top-color: var(--accent);
    border-radius: 50%;
    animation: spin 1s linear infinite;
  }
  .download-progress {
    margin: 18px 0;
  }
  .download-progress > div {
    display: flex;
    justify-content: space-between;
    gap: 8px;
    font-size: 12px;
  }
  progress::-webkit-progress-bar {
    background: var(--border-strong);
    border-radius: 20px;
  }
  progress::-webkit-progress-value {
    background: var(--accent);
    border-radius: 20px;
  }
  progress::-moz-progress-bar {
    background: var(--accent);
    border-radius: 20px;
  }
  progress {
    appearance: none;
    border: 0;
    border-radius: 20px;
    overflow: hidden;
    width: 100%;
    height: 6px;
    accent-color: var(--accent);
    margin: 12px 0 4px;
  }
  .download-progress small {
    color: var(--text-muted);
    font-size: 11px;
  }
  .error {
    border-left: 2px solid var(--err);
    padding: 12px 15px;
    background: var(--bg-panel);
    color: var(--text-primary);
    font-size: 12px;
    line-height: 1.6;
    margin-top: 18px;
  }
  .success-mark {
    display: grid;
    place-items: center;
    width: 48px;
    height: 48px;
    color: var(--ok);
    border: 1px solid var(--border-strong);
    border-radius: 50%;
    margin-bottom: 22px;
  }
  .setup-footer {
    display: flex;
    align-items: center;
    gap: 7px;
    color: var(--text-muted);
    font-size: 10.5px;
    margin-top: 34px;
  }
  .footer-dot {
    width: 4px;
    height: 4px;
    border-radius: 50%;
    background: var(--text-muted);
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .spinner {
      animation: none;
    }
  }
  @media (max-width: 600px) {
    .setup {
      padding: 28px 20px;
    }
    .setup-top {
      margin-bottom: 32px;
    }
    .choice {
      padding: 16px 13px;
      gap: 12px;
    }
    .tag {
      margin-left: 0;
    }
    .model-option {
      flex-wrap: wrap;
    }
    .size {
      margin-left: 24px;
    }
  }
</style>
