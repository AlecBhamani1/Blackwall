<script lang="ts">
  export let dirty = false;
  export let canSave = true;
  export let busy = false;
  export let saveLabel = 'Save';
  export let feedback = '';
  export let error = '';
  export let onSave: () => void;
  export let onCancel: () => void;
</script>

<footer class="settings-footer">
  <span class:error={error} role={error ? 'alert' : 'status'}>{error || feedback}</span>
  <button type="button" class="secondary" disabled={busy || !dirty} onclick={onCancel}
    >Cancel</button
  >
  <button
    type="button"
    class="primary"
    disabled={busy || !dirty || !canSave}
    onclick={onSave}
    >{busy ? 'Saving…' : saveLabel}</button
  >
</footer>

<style>
  .settings-footer {
    position: sticky;
    z-index: 2;
    bottom: 0;
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 24px;
    padding: 12px 0;
    border-top: 1px solid var(--border);
    background: var(--bg-panel);
  }
  span {
    min-width: 0;
    flex: 1;
    color: var(--ok);
    font-size: 12px;
  }
  span.error {
    color: var(--err);
  }
  button {
    padding: 8px 12px;
    border-radius: var(--radius-sm);
  }
  .secondary {
    border: 1px solid var(--border-strong);
    background: var(--bg-elevated);
    color: var(--text-primary);
  }
  .primary {
    background: var(--accent);
    color: var(--text-on-accent);
  }
</style>
