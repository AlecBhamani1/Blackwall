<script lang="ts">
  import type { ChatMessage } from '../types';
  import MessageCell from './MessageCell.svelte';
  import SubagentSection from './SubagentSection.svelte';
  import ToolCell from './ToolCell.svelte';

  const user: ChatMessage = {
    id: 'demo-user',
    role: 'user',
    content: 'Orient me to this project and verify the local harness.',
    attachments: [],
    status: 'complete',
    createdAt: Date.now(),
  };

  const assistant: ChatMessage = {
    id: 'demo-assistant',
    role: 'assistant',
    content: '**I’ll map the repository first**, then confirm the app boundary and local model connection.',
    attachments: [],
    status: 'complete',
    createdAt: Date.now(),
  };
</script>

<div class="demo-flow">
  <MessageCell message={user} />
  <MessageCell message={assistant} />
  <div class="indented"><ToolCell command="rg --files -g '!node_modules'" duration="0.1s" output="README.md\nui/src/App.svelte\nsrc/core/src/lib.rs" /></div>
  <div class="indented"><SubagentSection title="Repository orientation" result="Confirmed: the Svelte view is presentation-only and the Rust core owns model I/O." /></div>
</div>

<style>
  .demo-flow {
    display: flex;
    flex-direction: column;
    gap: 25px;
  }

  .indented {
    margin-left: 35px;
  }

  @media (max-width: 560px) {
    .indented {
      margin-left: 0;
    }
  }
</style>
