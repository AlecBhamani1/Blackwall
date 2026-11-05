<script lang="ts">
  import { onMount } from 'svelte';
  import { webClient, type WebView } from '../web';
  import { platform } from '../platform';
  export let client = webClient;
  let view: WebView | null = null;
  let provider: 'duckduckgo' | 'brave' = 'duckduckgo';
  let braveFallback = false;
  let rendering = false;
  let key = '';
  let busy = false;
  let error = '';
  let feedback = '';
  async function load(apply = true) {
    view = await client.load();
    if (apply) ({ provider, braveFallback, rendering } = view.settings);
  }
  async function action(work: () => Promise<void>, message: string, reset = false) {
    busy = true;
    error = '';
    feedback = '';
    try {
      await work();
      await load(reset);
      feedback = message;
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause);
    } finally {
      busy = false;
    }
  }
  onMount(() => {
    void action(() => load(), '');
  });
</script>

<section aria-label="Web access settings">
  <h3>Web access</h3>
  <p>
    Search and read public pages in desktop Chat and Agent. Turn on Web access in the composer; your
    host rules and approval choices still apply.
  </p>
  <fieldset disabled={busy || !view}>
    <legend>Search provider</legend>
    <label for="search-provider">Use for searches</label>
    <select id="search-provider" bind:value={provider}>
      <option value="duckduckgo">DuckDuckGo · no key required</option>
      <option value="brave">Brave Web Search API</option>
    </select>
    <p>
      Brave receives your queries and network address. An activated provider plan is required and
      requests may incur charges. DuckDuckGo can stop at human-verification challenges.
    </p>
    {#if provider === 'duckduckgo'}
      <label class="check"
        ><input type="checkbox" bind:checked={braveFallback} />Allow Brave fallback after a
        DuckDuckGo challenge</label
      >
      <p>
        Enabling fallback authorizes sending the query to Brave, subject to your web permissions. A
        challenged DuckDuckGo endpoint is not retried during that run.
      </p>
    {/if}
    <label for="brave-key"
      >Brave access key · {view?.hasKey ? `saved in ${platform.store}` : 'not saved'}</label
    >
    <input
      id="brave-key"
      type="password"
      autocomplete="new-password"
      bind:value={key}
      placeholder="Enter a replacement key"
    />
    <div class="actions">
      <button
        disabled={!key.trim()}
        on:click={() => {
          const value = key;
          key = '';
          void action(() => client.saveKey(value), `Key saved in ${platform.store}.`);
        }}>Save key</button
      >
      <button
        disabled={!view?.hasKey}
        on:click={() => action(() => client.saveKey(''), 'Key removed.')}>Remove key</button
      >
      <button
        disabled={!view?.hasKey}
        on:click={() => action(() => client.check(), 'Brave connection succeeded.')}
        >Check Brave connection</button
      >
    </div>
    <p>
      The connection check sends “Blackwall connection check” to Brave and may count toward your
      plan. Saved keys are never returned to the model or stored in preferences.
    </p>
  </fieldset>
  <fieldset disabled={busy || !view}>
    <legend>Browser rendering</legend>
    <label class="check"
      ><input type="checkbox" bind:checked={rendering} disabled={!view?.browserSupported} />Render
      JavaScript pages when static extraction is empty</label
    >
    <p>
      A fresh isolated Chromium session loads public resources through Blackwall's network checks.
      Each resource host and redirect uses your web permission rules. Browser cookies, login
      sessions, workers, WebSockets, uploads and downloads are disabled.
    </p>
    <p>
      Partial pages can be requested with “Render page” in the composer. Rendering is limited to 30
      seconds, 100 requests, and 48 MiB of decompressed traffic. Page text remains untrusted.
    </p>
    {#if view?.browserSupported}
      <p>
        {view.browserInstalled
          ? 'Browser runtime installed.'
          : 'Browser runtime installation needed.'} Requires Node.js 22+ and npm. Installation downloads
        Playwright and Chromium; use this button again for the app's supported browser revision after
        an update.
      </p>
      <button
        on:click={() =>
          action(
            () => client.install(),
            'Browser installed. Enable rendering and save your settings to use it.',
          )}>{view.browserInstalled ? 'Install / repair browser' : 'Install browser'}</button
      >
    {:else}
      <p>
        Isolated rendering currently requires macOS. Use a public static source on this platform.
      </p>
    {/if}
    <p>
      Human verification or login may still block a page. Opening a source in your browser does not
      share its authenticated content with Blackwall.
    </p>
  </fieldset>
  <button
    disabled={busy || !view}
    on:click={() =>
      action(
        () => client.save({ provider, braveFallback, rendering }),
        'Web settings saved.',
        true,
      )}>Save Web settings</button
  >
  {#if busy}<p role="status">Working… Browser installation can take several minutes.</p>{/if}
  {#if feedback}<p role="status">{feedback}</p>{/if}
  {#if error}<p role="alert">{error}</p>{/if}
</section>

<style>
  section {
    display: grid;
    gap: 14px;
    max-width: 680px;
    color: var(--text-primary);
  }
  h3,
  p {
    margin: 0;
  }
  p {
    color: var(--text-muted);
    font-size: 12px;
    line-height: 1.6;
  }
  fieldset {
    display: grid;
    gap: 12px;
    padding: 16px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
  }
  legend,
  label {
    font-size: 12px;
  }
  legend {
    font-weight: 650;
    padding: 0 5px;
  }
  input,
  select,
  button {
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 8px 10px;
    background: var(--bg-elevated);
    color: var(--text-primary);
    font: inherit;
    font-size: 12px;
  }
  button {
    cursor: pointer;
    width: fit-content;
  }
  button:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }
  .check {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  [role='alert'] {
    color: var(--error);
  }
</style>
