<script lang="ts">
  export let size = 28;
</script>

<span class="mark" style:width={`${size}px`} style:height={`${size}px`} aria-hidden="true">
  <span class="wall"></span>
</span>

<style>
  .mark {
    position: relative;
    display: inline-grid;
    flex: 0 0 auto;
    place-items: center;
    overflow: hidden;
    border: 1px solid var(--border-strong);
    border-radius: 8px;
    background: var(--bg-elevated);
  }

  .mark::before,
  .mark::after {
    position: absolute;
    width: 34%;
    height: 42%;
    content: '';
    border: 1.5px solid var(--text-muted);
  }

  .mark::before {
    top: 17%;
    left: 17%;
    border-right: 0;
    border-bottom: 0;
  }

  .mark::after {
    right: 17%;
    bottom: 17%;
    border-top: 0;
    border-left: 0;
  }

  .wall {
    width: 2px;
    height: 52%;
    border-radius: var(--radius-pill);
    background: var(--accent);
  }
</style>
