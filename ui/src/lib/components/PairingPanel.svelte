<script lang="ts">
  import { onDestroy, onMount, tick } from 'svelte';
  import { pairingClient, pairingError, type PairingClient, type PairedDevice } from '../pairing';
  import { isDesktop } from '../setup';
  export let mode: 'host' | 'client' = 'client';
  export let endpoint = '';
  export let model = '';
  export let relayUrl = '';
  export let relayKey = '';
  export let onConnect: (endpoint: string, name: string) => Promise<boolean> = async () => false;
  export let onForget: (endpoint: string) => void = () => {};
  export let client: PairingClient = pairingClient;
  export let onCreated: () => void = () => {};
  export let desktop = isDesktop();
  const snapshot = client.snapshot;
  let name = mode === 'host' ? 'My model computer' : 'My laptop';
  let invitation = '';
  let createdInvitation = '';
  let createdExpiresAt = 0;
  let clockNow = Date.now();
  let creating = false;
  let relaySettingsOpen = !relayUrl;
  let nameInput: HTMLInputElement;
  let copyButton: HTMLButtonElement;
  let panel: HTMLElement;
  let previousProgress: string | undefined;
  async function focusResult() {
    await tick();
    if (disposed || !panel) return;
    const active = document.activeElement;
    if (active !== document.body && !panel.contains(active)) return;
    const savedConnection = Array.from(
      panel.querySelectorAll<HTMLElement>('[data-pair-connect]'),
    ).find((button) => button.dataset.pairConnect === progress?.sessionId);
    // Query each priority independently: selector lists otherwise use document order.
    (
      panel.querySelector<HTMLElement>('[role="alert"]') ??
      panel.querySelector<HTMLElement>('[data-pair-result]') ??
      savedConnection ??
      panel.querySelector<HTMLElement>('[data-pair-connect]') ??
      panel.querySelector<HTMLElement>('h3')
    )?.focus();
  }
  function trackProgress(state: string | undefined) {
    const wasWaiting = previousProgress === 'review';
    previousProgress = state;
    if (
      mode === 'client' &&
      wasWaiting &&
      state === 'saved' &&
      panel?.contains(document.activeElement)
    )
      void focusResult();
  }
  async function beginCreation() {
    creating = true;
    await tick();
    nameInput?.focus();
  }
  let busy = false;
  let error = '';
  let notice = '';
  let copied = false;
  let codesMatch = false;
  let candidateId = '';
  let removing: PairedDevice | null = null;
  let disposed = false;
  $: progress = mode === 'host' ? $snapshot.host : $snapshot.client;
  $: trackProgress(progress?.state);
  $: hostComplete =
    mode === 'host' &&
    progress?.state === 'approved' &&
    $snapshot.devices.some(
      (device) => device.id === progress?.sessionId && device.state === 'active',
    );
  $: if (createdInvitation && clockNow >= createdExpiresAt) {
    createdInvitation = '';
    notice = 'The pairing invitation expired. Create a new one.';
  }
  $: saved = $snapshot.devices.filter((device) => device.role === mode);
  $: fingerprint = progress?.candidate
    ? `${progress.candidate.id}:${progress.candidate.hash}:${progress.candidate.salt}`
    : '';
  $: if (fingerprint !== candidateId) {
    candidateId = fingerprint;
    codesMatch = false;
  }
  $: if (progress?.state === 'approved' || progress?.state === 'denied') createdInvitation = '';
  async function act(action: () => Promise<void>) {
    if (busy) return;
    const active = document.activeElement;
    const hadFocus = panel?.contains(active);
    busy = true;
    error = '';
    notice = '';
    try {
      await action();
      if (!disposed) await client.refresh();
    } catch (cause) {
      if (!disposed) {
        // A failed removal can still have committed its pending state.
        await client.refresh().catch(() => {});
        if (!disposed) error = pairingError(cause);
      }
    } finally {
      if (!disposed) busy = false;
    }
    await tick();
    if (hadFocus && (!active?.isConnected || document.activeElement === document.body)) {
      await focusResult();
    }
  }
  async function create() {
    await act(async () => {
      const result = await client.create({ relayUrl, relayKey, name, model, endpoint });
      if (disposed) return;
      createdInvitation = result.invitation;
      createdExpiresAt = result.expiresAt;
      relayKey = '';
      relayUrl = new URL(result.invitation).origin;
      onCreated();
      copied = false;
    });
    await tick();
    if (!disposed && createdInvitation) copyButton?.focus();
  }
  async function join() {
    const value = invitation;
    invitation = '';
    await act(async () => {
      await client.join(value, name);
    });
  }
  async function copy() {
    try {
      await navigator.clipboard.writeText(createdInvitation);
      copied = true;
    } catch {
      error = 'Copy was unavailable. Select the invitation below and copy it manually.';
    }
  }
  async function connect(device: PairedDevice) {
    await act(async () => {
      if (!(await onConnect(device.endpoint, device.name)))
        throw new Error(
          'This computer is offline or its access was removed. Open and unlock Blackwall on the host, then try again.',
        );
    });
  }
  onMount(() => {
    const clock = setInterval(() => {
      clockNow = Date.now();
    }, 1000);
    if (desktop)
      void client.refresh().catch((cause) => {
        if (!disposed) error = pairingError(cause);
      });
    return () => clearInterval(clock);
  });
  onDestroy(() => {
    disposed = true;
    createdInvitation = '';
    invitation = '';
  });
</script>

<section
  bind:this={panel}
  class="pairing"
  aria-label={mode === 'host' ? 'Paired devices' : 'Paired computers'}
  aria-busy={busy}
>
  <div class="heading">
    <span class="eyebrow">{mode === 'host' ? 'YOUR DEVICES' : 'REMEMBERED ACCESS'}</span>
    <h3 tabindex="-1">
      {mode === 'host' ? 'Connect another computer' : 'Pair with your model computer'}
    </h3>
    <p>
      {mode === 'host'
        ? 'Approve a computer once. It can reconnect to this model whenever Blackwall is open, unlocked, and awake.'
        : 'Create a pairing invitation in Blackwall on the computer running your model. Paste it here, then approve the matching code on that computer.'}
    </p>
  </div>
  {#if !desktop}
    <p class="message">Computer pairing is available in the Blackwall desktop app.</p>
  {:else}
    {#if saved.length}
      <div class="devices">
        {#each saved as device (device.id)}
          <div class="device">
            <div class="device-copy">
              <strong>{device.name}</strong><span>{device.model}</span>
              <small
                >{device.state === 'unavailable'
                  ? 'Connection needs attention · check your relay access key'
                  : device.state === 'revoking'
                    ? device.role === 'client'
                      ? 'Removal incomplete · retry removal'
                      : 'Removal pending · retrying automatically'
                    : device.state === 'revoked'
                      ? 'Access removed'
                      : device.state === 'pending'
                        ? progress?.sessionId === device.id && progress?.state === 'approved'
                          ? 'Approval incomplete · finish pairing below'
                          : 'Approval incomplete · remove and pair again'
                        : mode === 'host'
                          ? device.online
                            ? 'Relay connected · keep the model running'
                            : 'Connecting to relay'
                          : 'Saved · host must be open and unlocked'}</small
              >
            </div>
            <div class="row-actions">
              {#if mode === 'host' && device.state === 'unavailable'}<button
                  class="quiet"
                  disabled={busy}
                  onclick={() => act(() => client.retry(device.id))}>Retry connection</button
                >{/if}
              {#if mode === 'client' && device.state === 'active'}<button
                  data-pair-connect={device.id}
                  aria-label={`Connect to ${device.name}`}
                  class="primary compact"
                  disabled={busy}
                  onclick={() => connect(device)}>Connect</button
                >{/if}
              <button
                class="quiet"
                disabled={busy || (device.state === 'revoking' && device.role === 'host')}
                aria-label={`Remove ${device.name}`}
                onclick={() => (removing = device)}>Remove</button
              >
            </div>
          </div>
        {/each}
      </div>
    {/if}
    {#if removing}
      <div class="message removal" role="group" aria-label="Confirm computer removal">
        <strong>Remove {removing.name}?</strong>
        <p>
          {mode === 'host'
            ? 'This stops its access and active requests. Other paired computers and guest links keep working. Pair again to restore access.'
            : 'This removes the saved connection and its key from this Mac. The host can also remove this device from its own list.'}
        </p>
        <div class="actions">
          <button
            class="danger"
            disabled={busy}
            onclick={() =>
              act(async () => {
                const device = removing!;
                await client.remove(device.id);
                onForget(device.endpoint);
                removing = null;
              })}>Remove computer</button
          ><button class="quiet" disabled={busy} onclick={() => (removing = null)}
            >Keep computer</button
          >
        </div>
      </div>
    {/if}
    {#if progress?.state === 'review' || (mode === 'host' && progress?.state === 'approved' && !hostComplete)}
      <div
        class="review"
        data-pair-result
        tabindex="-1"
        role="group"
        aria-label="Pairing confirmation"
      >
        <span class="eyebrow"
          >{mode === 'host' ? 'CHECK THE OTHER SCREEN' : 'WAITING FOR HOST APPROVAL'}</span
        >
        <strong>{mode === 'host' ? progress.candidate?.name : progress.hostName}</strong>
        <code aria-label="Confirmation code">{progress.confirmation}</code>
        <p>
          Both computers must show this code. Pairing allows model chat, including attachments. It
          does not grant access to the host’s files, tools, or saved chats.
        </p>
        {#if mode === 'host'}
          <label class="check"
            ><input type="checkbox" bind:checked={codesMatch} disabled={busy} />These codes match on
            both computers</label
          >
          <div class="actions">
            <button
              class="primary"
              disabled={busy || !codesMatch}
              onclick={() =>
                act(async () => {
                  await client.approve(progress!.candidate!);
                  createdInvitation = '';
                })}>{progress.state === 'approved' ? 'Finish pairing' : 'Approve computer'}</button
            >
            {#if progress.state !== 'approved'}<button
                class="quiet"
                disabled={busy}
                onclick={() => act(() => client.cancel(mode))}>Deny request</button
              >{/if}
          </div>
        {:else}<p class="waiting">
            {$snapshot.warnings.length
              ? 'Pairing needs attention. Resolve the message below, then check again.'
              : 'Approve this computer on the host to finish pairing.'}
          </p>{/if}
      </div>
    {:else if hostComplete}
      <p class="message" role="status">Computer approved. Finish connecting on the other screen.</p>
    {:else if progress?.state === 'saved'}
      <p class="message" role="status">
        Computer paired. Use its saved connection above to start chatting.
      </p>
    {:else if progress?.state === 'denied'}
      <p class="message">
        This pairing request was declined. Create a new invitation to try again.
      </p>
    {/if}
    {#if mode === 'host' && progress?.state === 'waiting' && !createdInvitation}
      <p class="message" role="status">
        Waiting for your other computer. The invitation key is shown only when created. To copy a
        new invitation, cancel this one and create another.
      </p>
    {/if}
    {#if mode === 'client' && !progress && (error || $snapshot.warnings.length)}
      <button class="quiet" disabled={busy} onclick={() => act(() => client.cancel(mode))}
        >Cancel current pairing</button
      >
    {/if}
    {#if createdInvitation}
      <div class="invitation">
        <label for={`pair-invite-${mode}`}>One-time pairing invitation</label>
        <textarea
          id={`pair-invite-${mode}`}
          readonly
          value={createdInvitation}
          rows="3"
          spellcheck="false"></textarea>
        <p>
          Expires in {Math.min(5, Math.max(1, Math.ceil((createdExpiresAt - clockNow) / 60000)))} minutes.
          Send it to your other computer through a trusted channel. You still approve access here.
        </p>
        <div class="actions">
          <button bind:this={copyButton} class="primary" disabled={busy} onclick={copy}
            >{copied ? 'Copied' : 'Copy invitation'}</button
          ><button
            class="quiet"
            disabled={busy}
            onclick={() =>
              act(async () => {
                await client.cancel(mode);
                createdInvitation = '';
              })}>Cancel invitation</button
          >
        </div>
      </div>
    {:else if !progress || ['denied', 'saved'].includes(progress.state)}
      {#if mode === 'host' && !creating}<button
          class="primary"
          disabled={busy || !model || !endpoint}
          onclick={beginCreation}>Pair another computer</button
        >
      {:else}
        <form
          onsubmit={(event) => {
            event.preventDefault();
            if (mode === 'host') void create();
            else void join();
          }}
        >
          <label for={`pair-name-${mode}`}
            >{mode === 'host' ? 'Name this model computer' : 'Name this computer'}</label
          >
          <input
            id={`pair-name-${mode}`}
            bind:this={nameInput}
            bind:value={name}
            maxlength="60"
            required
            disabled={busy}
            autocomplete="off"
          />
          {#if mode === 'host'}
            <p class="model">Sharing model: <strong>{model || 'Choose a model first'}</strong></p>
            <details bind:open={relaySettingsOpen}>
              <summary>Advanced relay settings</summary>
              <label for="pair-relay">Relay address</label><input
                id="pair-relay"
                bind:value={relayUrl}
                placeholder="https://relay.example.com"
                required
                disabled={busy}
              />
              <label for="pair-relay-key"
                >Relay access key <span>(leave blank to reuse saved key)</span></label
              ><input
                id="pair-relay-key"
                type="password"
                bind:value={relayKey}
                disabled={busy}
                autocomplete="off"
              />
              <p>Use your operated relay. A default Blackwall relay is not available yet.</p>
            </details>
          {:else}<label for="pair-paste">Pairing invitation</label><textarea
              id="pair-paste"
              bind:value={invitation}
              placeholder="Paste the complete invitation"
              required
              rows="3"
              disabled={busy}
              spellcheck="false"></textarea>{/if}
          <button
            class="primary"
            type="submit"
            disabled={busy || (mode === 'host' && (!model || !endpoint))}
            >{busy
              ? 'Connecting…'
              : mode === 'host'
                ? 'Create pairing invitation'
                : 'Request pairing'}</button
          >
        </form>
      {/if}
    {/if}
    {#if progress && !['saved', 'denied'].includes(progress.state) && !createdInvitation}<button
        data-pair-result={hostComplete ? '' : undefined}
        class="quiet"
        disabled={busy}
        onclick={() =>
          act(async () => {
            await client.cancel(mode);
            createdInvitation = '';
          })}>{progress.state === 'approved' ? 'Done' : 'Cancel pairing'}</button
      >{/if}
    {#each $snapshot.warnings as warning}<p class="message warning" role="status">
        {warning}
      </p>{/each}
    {#if error}<p class="message error" role="alert" tabindex="-1">{error}</p>{/if}
    {#if error || $snapshot.warnings.length}
      <button class="quiet" disabled={busy} onclick={() => act(() => client.refresh())}
        >Check again</button
      >{/if}
    {#if notice}<p class="message" role="status">{notice}</p>{/if}
  {/if}
</section>

<style>
  .pairing {
    display: grid;
    gap: 16px;
    color: var(--text-primary);
    min-width: 0;
  }
  .heading {
    display: grid;
    gap: 7px;
  }
  h3 {
    margin: 0;
    font-size: 17px;
    font-weight: 600;
    letter-spacing: -0.3px;
  }
  p {
    margin: 0;
    color: var(--text-muted);
    font-size: 12px;
    line-height: 1.65;
    overflow-wrap: anywhere;
  }
  .eyebrow {
    color: var(--text-muted);
    font-size: 9px;
    font-weight: 600;
    letter-spacing: 1.3px;
  }
  form,
  .invitation,
  .review,
  .devices {
    display: grid;
    gap: 12px;
    min-width: 0;
  }
  label {
    font-size: 12px;
    font-weight: 500;
  }
  label span {
    font-size: 10px;
    color: var(--text-muted);
  }
  input:not([type='checkbox']),
  textarea {
    box-sizing: border-box;
    width: 100%;
    min-width: 0;
    padding: 11px 12px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-base);
    color: var(--text-primary);
    font: inherit;
    font-size: 12px;
  }
  textarea {
    resize: vertical;
    word-break: break-all;
  }
  input:focus-visible,
  textarea:focus-visible,
  button:focus-visible,
  summary:focus-visible {
    outline: 2px solid var(--text-muted);
    outline-offset: 3px;
  }
  button {
    font: inherit;
    font-size: 11px;
    border-radius: 7px;
    padding: 10px 13px;
    border: 1px solid var(--border);
    cursor: pointer;
  }
  button:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .primary {
    background: var(--text-primary);
    color: var(--bg-base);
    font-weight: 600;
    justify-self: start;
  }
  .quiet {
    background: transparent;
    color: var(--text-muted);
    justify-self: start;
  }
  .danger {
    background: transparent;
    color: var(--err);
  }
  .actions,
  .row-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }
  .device {
    display: flex;
    gap: 12px;
    align-items: center;
    justify-content: space-between;
    border: 1px solid var(--border);
    padding: 13px;
    border-radius: 9px;
  }
  .device-copy {
    min-width: 0;
    display: grid;
    gap: 5px;
    overflow-wrap: anywhere;
  }
  .device-copy strong {
    font-size: 12px;
  }
  .device-copy span,
  .device-copy small {
    color: var(--text-muted);
    font-size: 10px;
  }
  .review,
  .message {
    border: 1px solid var(--border);
    border-radius: 9px;
    background: var(--bg-panel);
    padding: 16px;
  }
  .review code {
    font-size: 25px;
    letter-spacing: 2px;
    padding: 5px 0;
    font-variant-numeric: tabular-nums;
  }
  .check {
    display: flex;
    gap: 8px;
    align-items: flex-start;
    line-height: 1.5;
  }
  .check input {
    margin-top: 2px;
    accent-color: var(--text-primary);
  }
  .error {
    color: var(--err);
  }
  .removal {
    display: grid;
    gap: 12px;
  }
  details {
    display: grid;
    padding: 12px;
    border: 1px solid var(--border);
    border-radius: 8px;
  }
  details label {
    display: block;
    margin: 14px 0 8px;
  }
  details p {
    margin-top: 10px;
  }
  summary {
    cursor: pointer;
    font-size: 11px;
    color: var(--text-muted);
  }
  @media (max-width: 480px) {
    .device {
      align-items: stretch;
      flex-direction: column;
    }
    .review code {
      font-size: 21px;
    }
  }
</style>
