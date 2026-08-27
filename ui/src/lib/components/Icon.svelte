<script lang="ts">
  export let name:
    | 'arrow-up'
    | 'check'
    | 'chevron-down'
    | 'copy'
    | 'file'
    | 'gear'
    | 'image'
    | 'menu'
    | 'new-chat'
    | 'paperclip'
    | 'refresh'
    | 'stop'
    | 'trash'
    | 'x';
  export let size = 18;
  export let strokeWidth = 1.8;
  export let className = '';
</script>

<svg
  class={className}
  width={size}
  height={size}
  viewBox="0 0 24 24"
  fill="none"
  stroke="currentColor"
  stroke-width={strokeWidth}
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
>
  {#if name === 'arrow-up'}
    <path d="m6 11 6-6 6 6" />
    <path d="M12 19V5" />
  {:else if name === 'check'}
    <path d="m5 12 4 4L19 6" />
  {:else if name === 'chevron-down'}
    <path d="m7 10 5 5 5-5" />
  {:else if name === 'copy'}
    <rect width="14" height="14" x="8" y="8" rx="2" />
    <path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2" />
  {:else if name === 'file'}
    <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8Z" />
    <path d="M14 2v6h6" />
  {:else if name === 'gear'}
    <circle cx="12" cy="12" r="3" />
    <path d="M19.4 15a1.7 1.7 0 0 0 .34 1.88l.06.06-2.83 2.83-.06-.06a1.7 1.7 0 0 0-1.88-.34 1.7 1.7 0 0 0-1.03 1.56V21h-4v-.08A1.7 1.7 0 0 0 9 19.37a1.7 1.7 0 0 0-1.88.34l-.06.06-2.83-2.83.06-.06A1.7 1.7 0 0 0 4.63 15 1.7 1.7 0 0 0 3.08 14H3v-4h.08A1.7 1.7 0 0 0 4.63 9a1.7 1.7 0 0 0-.34-1.88l-.06-.06 2.83-2.83.06.06A1.7 1.7 0 0 0 9 4.63a1.7 1.7 0 0 0 1-1.55V3h4v.08A1.7 1.7 0 0 0 15 4.63a1.7 1.7 0 0 0 1.88-.34l.06-.06 2.83 2.83-.06.06A1.7 1.7 0 0 0 19.37 9 1.7 1.7 0 0 0 20.92 10H21v4h-.08A1.7 1.7 0 0 0 19.4 15Z" />
  {:else if name === 'image'}
    <rect width="18" height="18" x="3" y="3" rx="2" />
    <circle cx="9" cy="9" r="2" />
    <path d="m21 15-3.1-3.1a2 2 0 0 0-2.8 0L6 21" />
  {:else if name === 'menu'}
    <path d="M4 6h16M4 12h16M4 18h16" />
  {:else if name === 'new-chat'}
    <path d="M12 20h9" />
    <path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L8 18l-4 1 1-4Z" />
  {:else if name === 'paperclip'}
    <path d="m21.4 11.6-8.9 8.9a6 6 0 0 1-8.5-8.5l9.5-9.5a4 4 0 0 1 5.7 5.7l-9.5 9.5a2 2 0 0 1-2.9-2.8l8.9-8.9" />
  {:else if name === 'refresh'}
    <path d="M20 6v5h-5" />
    <path d="M4 18v-5h5" />
    <path d="M18.5 9A7 7 0 0 0 6.2 6.2L4 11M20 13l-2.2 4.8A7 7 0 0 1 5.5 15" />
  {:else if name === 'stop'}
    <rect width="10" height="10" x="7" y="7" rx="1" fill="currentColor" stroke="none" />
  {:else if name === 'trash'}
    <path d="M3 6h18" />
    <path d="M8 6V4h8v2M19 6l-1 15H6L5 6M10 11v5M14 11v5" />
  {:else if name === 'x'}
    <path d="M18 6 6 18M6 6l12 12" />
  {/if}
</svg>
