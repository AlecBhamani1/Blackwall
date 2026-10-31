<script lang="ts">
  import { onDestroy, onMount, tick } from 'svelte';
  import { prepareAttachments, revokeAttachmentPreview } from '../attachments';
  import {
    attachmentReferences,
    mentionAtCaret,
    type AttachmentReference,
  } from '../attachmentReferences';
  import { persistence } from '../persistence';
  import { modelLabel } from '../format';
  import type { Attachment, ConnectionState, ModelInfo, PendingAttachment } from '../types';
  import AttachmentTray from './AttachmentTray.svelte';
  import Icon from './Icon.svelte';

  type ExecutionProfile = 'read-only' | 'workspace-write' | 'unsandboxed';

  export let busy = false;
  export let disabled = false;
  export let notice = '';
  export let onDismissNotice: () => void = () => undefined;
  export let onSend: (text: string, attachments: PendingAttachment[]) => Promise<boolean>;
  export let onStop: () => void;
  export let chatAttachments: Attachment[] = [];
  export let sessionId: string | null = null;
  export let models: ModelInfo[] = [];
  export let selectedModel = '';
  export let connectionState: ConnectionState = 'checking';
  export let agentMode = false;
  export let executionProfile: ExecutionProfile = 'read-only';
  export let commandNetwork = false;
  export let webEnabled = false;
  export let workspaceWrite = true;
  export let controlsDisabled = false;
  export let onModelChange: (modelId: string) => void = () => undefined;
  export let onExecutionProfileChange: (profile: ExecutionProfile) => void = () => undefined;
  export let onCommandNetworkChange: (enabled: boolean) => void = () => undefined;
  export let onWebEnabledChange: (enabled: boolean) => void = () => undefined;
  export let onProfileFocusChange: (focused: boolean) => void = () => undefined;
  export let onReconnect: () => void = () => undefined;

  let text = '';
  let attachments: PendingAttachment[] = [];
  let fileInput: HTMLInputElement;
  let textarea: HTMLTextAreaElement;
  let submitting = false;
  let mention: ReturnType<typeof mentionAtCaret> = null;
  let mentionIndex = 0;
  let draftSessionId = sessionId;
  let attachmentNotice = '';
  // Typing / at the start of a message lists the commands and saved skills it can run.
  const BUILT_IN_COMMANDS = [
    { name: 'compact', hint: 'Summarize older context' },
    { name: 'context', hint: 'Show the context budget' },
    { name: 'init', hint: 'Propose an AGENTS.md for this project' },
    { name: 'learn', hint: 'Propose a skill from this conversation' },
    { name: 'recall', hint: 'Search saved messages' },
    { name: 'review', hint: 'Review Git changes' },
  ];
  let commands = BUILT_IN_COMMANDS;
  let slash: string | null = null;
  let slashIndex = 0;
  let commandList: HTMLDivElement | undefined;

  $: canSend = !disabled && !busy && !submitting && Boolean(text.trim() || attachments.length);
  $: references = attachmentReferences([...chatAttachments, ...attachments]);
  $: suggestions = mention
    ? references.filter(({ label }) => label.toLowerCase().includes(mention!.query.toLowerCase()))
    : [];
  $: commandSuggestions =
    slash === null ? [] : commands.filter(({ name }) => name.startsWith(slash!));
  $: if (slashIndex >= commandSuggestions.length) slashIndex = 0;
  $: scrollCommandIntoView(slashIndex, commandSuggestions);
  $: if (sessionId !== draftSessionId) {
    attachments.forEach(revokeAttachmentPreview);
    attachments = [];
    text = '';
    mention = null;
    slash = null;
    slashIndex = 0;
    attachmentNotice = '';
    draftSessionId = sessionId;
  }

  function updateMention() {
    mention = mentionAtCaret(text, textarea.selectionStart);
    mentionIndex = 0;
    const typed = /^\/([a-z0-9-]*)(?=\s|$)/.exec(text);
    const caret = textarea.selectionStart;
    const inCommand = typed && caret > 0 && caret <= typed[0].length;
    if (inCommand && slash === null) void loadSkills();
    slash = inCommand ? text.slice(1, caret) : null;
    slashIndex = 0;
  }

  async function scrollCommandIntoView(index: number, matches: typeof commands) {
    if (!matches[index]) return;
    await tick();
    commandList
      ?.querySelector<HTMLElement>(`#command-suggestion-${index}`)
      ?.scrollIntoView?.({ block: 'nearest' });
  }

  async function loadSkills() {
    try {
      const { skills } = await persistence.skills();
      const saved = skills
        .filter((skill) => skill.enabled)
        .map((skill) => ({ name: skill.name, hint: skill.description }));
      commands = [...BUILT_IN_COMMANDS, ...saved].sort((a, b) => a.name.localeCompare(b.name));
    } catch {
      commands = BUILT_IN_COMMANDS;
    }
  }

  async function selectCommand(name: string) {
    const command = /^\/[a-z0-9-]*/.exec(text);
    if (!command) return;
    const after = text.slice(command[0].length);
    const hasSeparator = /^\s/.test(after);
    const insertion = `/${name}${hasSeparator ? '' : ' '}`;
    text = insertion + after;
    slash = null;
    await tick();
    textarea.focus();
    const caret = insertion.length + (hasSeparator ? 1 : 0);
    textarea.setSelectionRange(caret, caret);
    resizeTextarea();
  }

  async function selectReference(reference: AttachmentReference) {
    if (!mention) return;
    const caret = textarea.selectionStart;
    const before = text.slice(0, mention.start);
    const after = text.slice(caret);
    const insertion = `${reference.token} `;
    text = before + insertion + after;
    mention = null;
    await tick();
    textarea.focus();
    textarea.setSelectionRange(before.length + insertion.length, before.length + insertion.length);
    resizeTextarea();
  }

  function input() {
    resizeTextarea();
    updateMention();
  }

  function resizeTextarea() {
    if (!textarea) return;
    textarea.style.height = 'auto';
    textarea.style.height = `${Math.min(textarea.scrollHeight, 180)}px`;
  }

  export function addFiles(files: Iterable<File>) {
    if (busy || submitting) return;
    const result = prepareAttachments(files, attachments);
    attachments = [...attachments, ...result.accepted];
    attachmentNotice = result.rejected
      .map((item) => `${item.fileName}: ${item.reason}`)
      .join(' · ');
    textarea?.focus();
  }

  function chooseFiles() {
    if (!busy && !submitting) fileInput.click();
  }

  function selectedFiles(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    if (input.files) addFiles(input.files);
    input.value = '';
  }

  function removeAttachment(attachmentId: string) {
    const attachment = attachments.find((item) => item.id === attachmentId);
    if (attachment) revokeAttachmentPreview(attachment);
    attachments = attachments.filter((item) => item.id !== attachmentId);
    attachmentNotice = '';
  }

  async function submit() {
    if (!canSend) return;
    const submitted = attachments;
    submitting = true;
    let accepted: boolean;
    try {
      accepted = await onSend(text, submitted);
    } finally {
      submitting = false;
    }
    if (!accepted) return;
    text = '';
    mention = null;
    slash = null;
    attachments = [];
    attachmentNotice = '';
    await tick();
    resizeTextarea();
    textarea?.focus();
  }

  function keydown(event: KeyboardEvent) {
    if (event.isComposing) return;
    if (slash !== null && event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      slash = null;
      return;
    }
    if (commandSuggestions.length) {
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        event.preventDefault();
        const direction = event.key === 'ArrowDown' ? 1 : -1;
        slashIndex =
          (slashIndex + direction + commandSuggestions.length) % commandSuggestions.length;
        return;
      }
      if ((event.key === 'Enter' && !event.shiftKey) || event.key === 'Tab') {
        event.preventDefault();
        void selectCommand((commandSuggestions[slashIndex] ?? commandSuggestions[0]).name);
        return;
      }
    }
    if (mention && suggestions.length) {
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        event.preventDefault();
        const direction = event.key === 'ArrowDown' ? 1 : -1;
        mentionIndex = (mentionIndex + direction + suggestions.length) % suggestions.length;
        return;
      }
      if ((event.key === 'Enter' && !event.shiftKey) || event.key === 'Tab') {
        event.preventDefault();
        void selectReference(suggestions[mentionIndex] ?? suggestions[0]);
        return;
      }
      if (event.key === 'Escape') {
        event.preventDefault();
        event.stopPropagation();
        mention = null;
        return;
      }
    }
    if (event.key !== 'Enter' || event.shiftKey || event.isComposing) return;
    event.preventDefault();
    void submit();
  }

  function paste(event: ClipboardEvent) {
    const imageFiles = Array.from(event.clipboardData?.items ?? [])
      .filter((item) => item.kind === 'file' && item.type.startsWith('image/'))
      .map((item) => item.getAsFile())
      .filter((file): file is File => file !== null);
    if (imageFiles.length > 0) {
      event.preventDefault();
      addFiles(imageFiles);
    }
  }

  onMount(() => textarea?.focus());
  onDestroy(() => attachments.forEach(revokeAttachmentPreview));
</script>

<div class="composer-region">
  {#if notice || attachmentNotice}
    <div class="notice" role="status">
      <span>{attachmentNotice || notice}</span>
      <button
        aria-label="Dismiss message"
        onclick={() => {
          attachmentNotice = '';
          onDismissNotice();
        }}
      >
        <Icon name="x" size={13} />
      </button>
    </div>
  {/if}

  <div class="composer-container">
    {#if slash !== null}
      <div class="command-menu">
        <div class="command-heading">Commands</div>
        <div
          class="file-suggestions command-suggestions"
          id="command-suggestions"
          role="listbox"
          aria-label="Commands"
          bind:this={commandList}
        >
          {#each commandSuggestions as command, index (command.name)}
            <button
              id={`command-suggestion-${index}`}
              role="option"
              aria-selected={index === slashIndex}
              class:highlighted={index === slashIndex}
              onmousedown={(event) => event.preventDefault()}
              onclick={() => selectCommand(command.name)}
            >
              <span class="command-name">/{command.name}</span>
              <span class="command-hint">{command.hint}</span>
            </button>
          {/each}
        </div>
        {#if commandSuggestions.length === 0}
          <p class="command-empty" role="status">No matching commands</p>
        {/if}
        <div class="command-help">↑ ↓ to navigate · Enter or Tab to insert · Esc to close</div>
      </div>
    {/if}

    <div class="composer" role="group" aria-label="Message composer">
      <AttachmentTray {attachments} removable onRemove={removeAttachment} />

      {#if suggestions.length > 0}
        <div
          class="file-suggestions"
          id="file-suggestions"
          role="listbox"
          aria-label="Files in this chat"
        >
          {#each suggestions as reference, index (reference.attachment.id)}
            <button
              id={`file-suggestion-${index}`}
              role="option"
              aria-selected={index === mentionIndex}
              class:highlighted={index === mentionIndex}
              onmousedown={(event) => event.preventDefault()}
              onclick={() => selectReference(reference)}
            >
              <Icon name={reference.attachment.kind === 'image' ? 'image' : 'file'} size={16} />
              <span>{reference.label}</span>
            </button>
          {/each}
        </div>
      {/if}

      <textarea
        bind:this={textarea}
        bind:value={text}
        rows="1"
        aria-label="Message Blackwall"
        placeholder="Message Blackwall · @ to reference a file · / for commands"
        aria-autocomplete="list"
        aria-controls={slash !== null
          ? 'command-suggestions'
          : suggestions.length
            ? 'file-suggestions'
            : undefined}
        aria-activedescendant={commandSuggestions.length
          ? `command-suggestion-${slashIndex}`
          : suggestions.length
            ? `file-suggestion-${mentionIndex}`
            : undefined}
        oninput={input}
        onclick={updateMention}
        onkeyup={(event) => {
          if (['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) updateMention();
        }}
        onkeydown={keydown}
        onpaste={paste}></textarea>

      <div class="composer-actions">
        <div class="left-actions">
          <div class="model-control">
            <span class:ready={connectionState === 'ready'} class:offline={connectionState === 'offline'} class="state-dot" aria-hidden="true"></span>
            {#if models.length > 0}
              <label class="sr-only" for="model-select">Model</label>
              <select id="model-select" value={selectedModel} onchange={(event) => onModelChange(event.currentTarget.value)}>
                {#each models as model (model.id)}<option value={model.id}>{modelLabel(model.name)}</option>{/each}
              </select>
            {:else}<span class="model-placeholder">{connectionState === 'checking' ? 'Finding your model…' : 'Model unavailable'}</span>{/if}
            {#if connectionState === 'offline'}<button class="reconnect" onclick={onReconnect}><Icon name="refresh" size={14} />Reconnect</button>{/if}
          </div>
          {#if agentMode}
            <label class="compact-control">Commands
              <select aria-label="Command execution profile" value={executionProfile} disabled={controlsDisabled} onfocus={() => onProfileFocusChange(true)} onblur={() => onProfileFocusChange(false)} onchange={(event) => onExecutionProfileChange(event.currentTarget.value as ExecutionProfile)}>
                <option value="read-only">Read-only</option>
                <option value="workspace-write" disabled={!workspaceWrite}>Workspace-write</option>
                <option value="unsandboxed">Unsandboxed (full access)</option>
              </select>
            </label>
            <label class="compact-control"><input type="checkbox" checked={commandNetwork || executionProfile === 'unsandboxed'} onchange={(event) => onCommandNetworkChange(event.currentTarget.checked)} disabled={controlsDisabled || executionProfile === 'unsandboxed'} />Command network</label>
          {/if}
          <label class="compact-control"><input type="checkbox" checked={webEnabled} onchange={(event) => onWebEnabledChange(event.currentTarget.checked)} disabled={controlsDisabled} />Web access</label>
          <button
            class="attach"
            aria-label="Add photos or files"
            title="Add photos or files"
            onclick={chooseFiles}
            disabled={busy || submitting}
          >
            <Icon name="paperclip" size={18} />
          </button>
        </div>

        {#if busy}
          <button
            class="send active"
            aria-label="Stop response"
            title="Stop response"
            onclick={onStop}
          >
            <Icon name="stop" size={16} />
          </button>
        {:else}
          <button
            class="send"
            class:active={canSend}
            aria-label="Send message"
            title="Send message"
            disabled={!canSend}
            onclick={submit}
          >
            <Icon name="arrow-up" size={18} strokeWidth={2.1} />
          </button>
        {/if}
      </div>

      <input
        class="sr-only"
        bind:this={fileInput}
        type="file"
        multiple
        accept="image/*,.pdf,.txt,.md,.csv,.json,.doc,.docx,.rtf,.html,.css,.js,.ts,.tsx,.jsx,.py,.rs,.go,.java,.c,.cpp,.h,.zip"
        onchange={selectedFiles}
        tabindex="-1"
      />
    </div>
  </div>

  <p class="composer-caption">Enter to send <span>·</span> Shift + Enter for a new line</p>
</div>

<style>
  .composer-region {
    width: min(calc(100% - 32px), var(--composer-width));
    margin: 0 auto;
    padding: 10px 0 7px;
  }

  .composer-container {
    position: relative;
  }

  .command-menu {
    position: absolute;
    z-index: 10;
    bottom: calc(100% + 8px);
    width: 100%;
    overflow: hidden;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-lg);
    background: var(--bg-panel);
    box-shadow: var(--shadow-composer);
  }

  .command-heading,
  .command-help,
  .command-empty {
    padding: 8px 14px;
    color: var(--text-muted);
    font-size: 11px;
  }

  .command-heading {
    border-bottom: 1px solid var(--border);
    font-weight: 600;
  }

  .command-help {
    border-top: 1px solid var(--border);
    color: var(--text-faint);
  }

  .command-empty {
    margin: 0;
  }

  .file-suggestions.command-suggestions {
    max-height: min(280px, 35vh);
    border-bottom: 0;
  }

  .command-suggestions .command-name {
    flex-shrink: 0;
    max-width: 55%;
    color: var(--text-primary);
  }

  .composer {
    position: relative;
    overflow: hidden;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-lg);
    background: var(--bg-panel);
    box-shadow: var(--shadow-composer);
    transition:
      border-color var(--transition-fast),
      box-shadow var(--transition-fast);
  }

  .composer:focus-within {
    border-color: var(--accent);
    box-shadow:
      var(--shadow-composer),
      0 0 0 2px var(--focus-ring);
  }

  textarea {
    display: block;
    width: 100%;
    min-height: 52px;
    max-height: 180px;
    resize: none;
    overflow-y: auto;
    border: 0;
    padding: 15px 16px 7px;
    background: transparent;
    color: var(--text-primary);
    font-size: 14px;
    line-height: 1.5;
    outline: none;
  }

  textarea::placeholder {
    color: var(--text-faint);
  }

  .composer-actions {
    display: flex;
    min-height: 42px;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    padding: 0 9px 8px 10px;
  }

  .left-actions {
    display: flex;
    min-width: 0;
    flex: 1;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
  }

  .model-control,
  .compact-control {
    display: flex;
    height: 28px;
    align-items: center;
    gap: 5px;
    color: var(--text-muted);
    font-size: 10.5px;
    white-space: nowrap;
  }

  .model-control select,
  .compact-control select {
    max-width: 150px;
    border: 1px solid var(--border);
    border-radius: 5px;
    padding: 3px 5px;
    background: var(--bg-base);
    color: var(--text-primary);
    font: inherit;
  }

  .compact-control input { margin: 0; accent-color: var(--accent); }
  .state-dot { width: 6px; height: 6px; flex: 0 0 auto; border-radius: 50%; background: var(--warn); }
  .state-dot.ready { background: var(--ok); }
  .state-dot.offline { background: var(--err); }
  .model-placeholder { font-size: 10.5px; }
  .reconnect { display: flex; align-items: center; gap: 4px; padding: 3px 5px; background: transparent; color: var(--text-muted); font: inherit; }

  .attach,
  .send {
    display: grid;
    width: 32px;
    height: 32px;
    flex: 0 0 auto;
    place-items: center;
    border-radius: 9px;
    transition:
      color var(--transition-fast),
      background var(--transition-fast),
      transform var(--transition-fast);
  }

  .attach {
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-muted);
  }

  .attach:hover:not(:disabled) {
    border-color: var(--border-strong);
    background: var(--bg-hover);
    color: var(--text-primary);
  }

  .send {
    background: var(--bg-elevated);
    color: var(--text-faint);
  }

  .send.active {
    background: var(--text-primary);
    color: var(--bg-base);
  }

  .send.active:hover {
    transform: translateY(-1px);
  }

  .composer-caption {
    margin: 6px 0 0;
    color: var(--text-faint);
    font-size: 10.5px;
    text-align: center;
  }

  .composer-caption span {
    padding: 0 3px;
  }

  .notice {
    display: flex;
    max-width: calc(100% - 10px);
    min-height: 31px;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin: 0 auto 8px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 5px 7px 5px 10px;
    background: var(--bg-elevated);
    color: var(--text-muted);
    font-size: 11.5px;
  }

  .notice button {
    display: grid;
    width: 22px;
    height: 22px;
    flex: 0 0 auto;
    place-items: center;
    border-radius: 5px;
    background: transparent;
    color: var(--text-faint);
  }

  .notice button:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }

  .file-suggestions {
    max-height: 180px;
    overflow-y: auto;
    border-bottom: 1px solid var(--border);
    padding: 6px;
  }

  .file-suggestions button {
    display: flex;
    width: 100%;
    align-items: center;
    gap: 8px;
    border-radius: var(--radius-sm);
    padding: 8px 10px;
    background: transparent;
    color: var(--text-muted);
    text-align: left;
    font-size: 12px;
  }

  .file-suggestions button.highlighted,
  .file-suggestions button:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }

  .file-suggestions span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .file-suggestions .command-hint {
    color: var(--text-muted);
    font-size: 12px;
    margin-left: auto;
  }

  @media (max-width: 560px) {
    .composer-region {
      width: calc(100% - 18px);
      padding-top: 7px;
    }

    .composer-caption {
      display: none;
    }

    textarea {
      min-height: 49px;
      padding-inline: 14px;
    }

    .composer-actions {
      padding-bottom: 7px;
    }
  }
</style>
