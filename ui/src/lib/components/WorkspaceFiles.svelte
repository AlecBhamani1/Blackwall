<script lang="ts">
  import { onDestroy } from 'svelte';
  import {
    workspaceClient,
    type DirectoryListing,
    type FileEntry,
    type FileSearch,
    type FileSearchMode,
  } from '../agent';
  import Icon from './Icon.svelte';

  export let workspace: string;
  export let onClose: () => void;

  let currentWorkspace = '';
  let generation = 0;
  let directories: Record<string, DirectoryListing> = {};
  let expanded = new Set<string>();
  let loading = new Set<string>();
  let errors: Record<string, string> = {};
  let query = '';
  let mode: FileSearchMode = 'names';
  let search: FileSearch | null = null;
  let searching = false;
  let searchError = '';
  let searchVersion = 0;
  let searchTimer: ReturnType<typeof setTimeout>;
  let selected: FileEntry | null = null;
  let preview = '';
  let previewError = '';
  let previewLoading = false;
  let previewVersion = 0;

  function errorMessage(cause: unknown): string {
    return typeof cause === 'string'
      ? cause
      : cause instanceof Error
        ? cause.message
        : 'The project files could not be read. Try refreshing.';
  }

  async function loadDirectory(path: string) {
    const version = generation;
    loading = new Set(loading).add(path);
    errors = { ...errors, [path]: '' };
    try {
      const listing = await workspaceClient.browse(workspace, path);
      if (version === generation) directories = { ...directories, [path]: listing };
    } catch (cause) {
      if (version === generation) errors = { ...errors, [path]: errorMessage(cause) };
    } finally {
      if (version === generation) {
        loading = new Set(loading);
        loading.delete(path);
      }
    }
  }

  function refresh() {
    generation += 1;
    directories = {};
    expanded = new Set();
    loading = new Set();
    errors = {};
    closePreview();
    void loadDirectory('.');
    scheduleSearch(query, mode);
  }

  async function open(entry: FileEntry) {
    if (entry.isDirectory) {
      expanded = new Set(expanded);
      if (expanded.has(entry.path)) expanded.delete(entry.path);
      else {
        expanded.add(entry.path);
        if (!directories[entry.path] && !loading.has(entry.path)) {
          void loadDirectory(entry.path);
        }
      }
      return;
    }
    const version = ++previewVersion;
    const projectVersion = generation;
    selected = entry;
    preview = '';
    previewError = '';
    previewLoading = true;
    try {
      const text = await workspaceClient.read(workspace, entry.path);
      if (version === previewVersion && projectVersion === generation) preview = text;
    } catch (cause) {
      if (version === previewVersion && projectVersion === generation) {
        previewError = errorMessage(cause);
      }
    } finally {
      if (version === previewVersion && projectVersion === generation) previewLoading = false;
    }
  }

  function closePreview() {
    previewVersion += 1;
    selected = null;
    preview = '';
    previewError = '';
    previewLoading = false;
  }

  async function openSearchResult(entry: FileEntry) {
    if (!entry.isDirectory) return open(entry);
    query = '';
    const version = generation;
    let path = '.';
    for (const segment of entry.path.replace(/^\.\//, '').split('/')) {
      path += `/${segment}`;
      expanded = new Set(expanded).add(path);
      if (!directories[path] && !loading.has(path)) await loadDirectory(path);
      if (version !== generation) return;
    }
  }

  function scheduleSearch(value: string, searchMode: FileSearchMode) {
    clearTimeout(searchTimer);
    const version = ++searchVersion;
    const projectVersion = generation;
    const project = workspace;
    search = null;
    searchError = '';
    searching = Boolean(value.trim());
    if (!value.trim()) return;
    searchTimer = setTimeout(async () => {
      try {
        const result = await workspaceClient.search(project, value.trim(), searchMode);
        if (version === searchVersion && projectVersion === generation) search = result;
      } catch (cause) {
        if (version === searchVersion && projectVersion === generation) {
          searchError = errorMessage(cause);
        }
      } finally {
        if (version === searchVersion && projectVersion === generation) searching = false;
      }
    }, 250);
  }

  function visibleRows(
    listings: Record<string, DirectoryListing>,
    openFolders: Set<string>,
    path = '.',
    depth = 0,
  ): { entry: FileEntry; depth: number }[] {
    return (listings[path]?.entries ?? []).flatMap((entry) => [
      { entry, depth },
      ...(entry.isDirectory && openFolders.has(entry.path)
        ? visibleRows(listings, openFolders, entry.path, depth + 1)
        : []),
    ]);
  }

  $: if (workspace !== currentWorkspace) {
    currentWorkspace = workspace;
    query = '';
    refresh();
  }
  $: scheduleSearch(query, mode);
  $: rows = visibleRows(directories, expanded);

  onDestroy(() => {
    generation += 1;
    searchVersion += 1;
    previewVersion += 1;
    clearTimeout(searchTimer);
  });
</script>

<aside class="workspace-files" aria-label="Project files">
  <header>
    <strong title={workspace}
      >{workspace.split('/').filter(Boolean).at(-1) || 'Project files'}</strong
    >
    <button
      class="icon-button"
      aria-label="Collapse all folders"
      onclick={() => (expanded = new Set())}
    >
      <Icon name="menu" size={16} />
    </button>
    <button class="icon-button" aria-label="Refresh project files" onclick={refresh}>
      <Icon name="refresh" size={16} />
    </button>
    <button class="icon-button" aria-label="Hide project files" onclick={onClose}>
      <Icon name="x" size={16} />
    </button>
  </header>

  <div class="search-controls">
    <label class="search-input">
      <Icon name="search" size={16} />
      <input aria-label="Find files" placeholder="Find files" maxlength="256" bind:value={query} />
    </label>
    <div class="search-modes" role="group" aria-label="File search mode">
      <button
        class:active={mode === 'names'}
        aria-pressed={mode === 'names'}
        onclick={() => (mode = 'names')}>Names</button
      >
      <button
        class:active={mode === 'contents'}
        aria-pressed={mode === 'contents'}
        onclick={() => (mode = 'contents')}>Contents</button
      >
    </div>
  </div>

  <div class="file-list" aria-label="Files and folders">
    {#if query.trim()}
      {#if searching}<p class="hint" role="status">Searching files…</p>{/if}
      {#if searchError}<p class="hint error" role="alert">{searchError}</p>{/if}
      {#if search}
        {#each search.matches as entry (entry.path)}
          <button
            class="file-row search-result"
            class:selected={selected?.path === entry.path}
            title={entry.path}
            onclick={() => openSearchResult(entry)}
          >
            <Icon name={entry.isDirectory ? 'folder' : 'file'} size={16} />
            <span
              ><span class="file-name">{entry.path.replace(/^\.\//, '')}</span>
              {#if entry.preview !== null}<small>{entry.line}: {entry.preview}</small>{/if}
            </span>
          </button>
        {/each}
        {#if !search.matches.length}<p class="hint">No matching files.</p>{/if}
        {#if search.truncated}<p class="hint">
            Search reached its limit. Try a more specific query.
          </p>{/if}
      {/if}
    {:else}
      {#if loading.has('.')}<p class="hint" role="status">Loading files…</p>{/if}
      {#if errors['.']}<p class="hint error" role="alert">{errors['.']}</p>{/if}
      {#each rows as { entry, depth } (entry.path)}
        <button
          class="file-row"
          class:selected={selected?.path === entry.path}
          style:padding-left={`${12 + depth * 16}px`}
          title={entry.path}
          aria-expanded={entry.isDirectory ? expanded.has(entry.path) : undefined}
          onclick={() => open(entry)}
        >
          <span class="disclosure"
            >{#if entry.isDirectory}<Icon
                name={expanded.has(entry.path) ? 'chevron-down' : 'chevron-right'}
                size={12}
              />{/if}</span
          >
          <Icon name={entry.isDirectory ? 'folder' : 'file'} size={16} />
          <span class="file-name">{entry.name}</span>
        </button>
        {#if entry.isDirectory && expanded.has(entry.path)}
          {#if loading.has(entry.path)}<p class="hint" role="status">Loading {entry.name}…</p>{/if}
          {#if errors[entry.path]}<p class="hint error" role="alert">
              {errors[entry.path]}
              <button class="retry" onclick={() => loadDirectory(entry.path)}>Retry</button>
            </p>{/if}
          {#if directories[entry.path]?.entries.length === 0}<p class="hint">
              Folder is empty.
            </p>{/if}
          {#if directories[entry.path]?.truncated}<p class="hint">
              Showing up to 500 entries in {entry.name}.
            </p>{/if}
        {/if}
      {/each}
      {#if directories['.']?.entries.length === 0}<p class="hint">
          This project folder is empty.
        </p>{/if}
      {#if directories['.']?.truncated}<p class="hint">
          Showing up to 500 entries. Search to find other files.
        </p>{/if}
    {/if}
  </div>

  {#if selected}
    <section class="file-preview" aria-label="File preview">
      <header>
        <span class="file-name" title={selected.path}>{selected.name}</span>
        <button class="icon-button" aria-label="Close file preview" onclick={closePreview}
          ><Icon name="x" size={16} /></button
        >
      </header>
      {#if previewLoading}<p class="hint" role="status">Loading preview…</p>
      {:else if previewError}<p class="hint error" role="alert">{previewError}</p>
      {:else}<pre><code>{preview || '(Empty file)'}</code></pre>{/if}
    </section>
  {/if}
</aside>

<style>
  .workspace-files {
    display: flex;
    min-height: 0;
    width: 300px;
    flex: 0 0 300px;
    flex-direction: column;
    border-left: 1px solid var(--border);
    background: var(--bg-panel);
  }
  header {
    display: flex;
    min-height: 48px;
    align-items: center;
    gap: 4px;
    padding: 8px 12px;
    border-bottom: 1px solid var(--border);
  }
  header strong,
  header > .file-name {
    min-width: 0;
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 12px;
  }
  .icon-button {
    display: grid;
    width: 28px;
    height: 28px;
    flex-shrink: 0;
    place-items: center;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-muted);
  }
  .icon-button:hover,
  .file-row:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  .search-controls {
    display: grid;
    gap: 8px;
    padding: 12px;
    border-bottom: 1px solid var(--border);
  }
  .search-input {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 7px 10px;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-sm);
    background: var(--bg-elevated);
    color: var(--text-muted);
  }
  input {
    width: 100%;
    min-width: 0;
    border: 0;
    outline: 0;
    background: transparent;
    color: var(--text-primary);
    font-size: 12px;
  }
  .search-input:focus-within {
    outline: 2px solid var(--focus-ring);
  }
  .search-modes {
    display: flex;
    padding: 3px;
    border-radius: var(--radius-sm);
    background: var(--bg-elevated);
  }
  .search-modes button {
    flex: 1;
    padding: 4px;
    border-radius: 4px;
    background: transparent;
    color: var(--text-muted);
    font-size: 12px;
  }
  .search-modes button.active {
    background: var(--bg-base);
    color: var(--text-primary);
  }
  .file-list {
    min-height: 0;
    flex: 1;
    overflow: auto;
    padding: 8px 0;
  }
  .file-row {
    display: flex;
    width: 100%;
    align-items: center;
    gap: 7px;
    padding: 6px 12px;
    background: transparent;
    color: var(--text-muted);
    text-align: left;
    font-size: 12px;
  }
  .file-row.selected {
    background: var(--bg-elevated);
    color: var(--text-primary);
  }
  .file-row :global(svg) {
    flex-shrink: 0;
  }
  .disclosure {
    display: flex;
    width: 12px;
    flex-shrink: 0;
  }
  .file-name {
    display: block;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .search-result > span {
    min-width: 0;
  }
  small {
    display: block;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--text-faint);
    font-size: 11px;
  }
  .hint {
    margin: 8px 16px;
    color: var(--text-muted);
    font-size: 11px;
    overflow-wrap: anywhere;
  }
  .error {
    color: var(--err);
  }
  .retry {
    background: transparent;
    color: var(--text-primary);
    text-decoration: underline;
  }
  .file-preview {
    display: flex;
    min-height: 120px;
    max-height: 45%;
    flex-direction: column;
    border-top: 1px solid var(--border-strong);
  }
  .file-preview header {
    min-height: 38px;
  }
  pre {
    margin: 0;
    padding: 12px;
    overflow: auto;
    font-family: var(--font-code);
    font-size: 11px;
    tab-size: 2;
  }
  @media (max-width: 760px) {
    .workspace-files {
      position: absolute;
      z-index: 10;
      top: 0;
      right: 0;
      bottom: 0;
      width: min(320px, 100%);
      box-shadow: var(--shadow-menu);
    }
  }
</style>
