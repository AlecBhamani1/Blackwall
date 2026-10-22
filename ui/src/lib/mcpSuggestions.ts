/**
 * General-purpose MCP servers offered in the Add form. The list ships with the app and is never
 * downloaded, and each command pins an exact package version; raise versions in reviewed changes.
 */
export interface McpSuggestion {
  name: string;
  description: string;
  command: string[];
  /** Added to the end of the command by the owner before saving. */
  needs?: 'folder' | 'url';
}

export const MCP_SUGGESTIONS: McpSuggestion[] = [
  {
    name: 'chrome-devtools',
    description: 'Inspect and drive Chrome: pages, console, network, and performance.',
    command: ['npx', '-y', 'chrome-devtools-mcp@1.10.1'],
  },
  {
    name: 'playwright',
    description: 'Automate a browser with Playwright.',
    command: ['npx', '-y', '@playwright/mcp@0.0.83'],
  },
  {
    name: 'context7',
    description: 'Current documentation for libraries and APIs.',
    command: ['npx', '-y', '@upstash/context7-mcp@4.4.1'],
  },
  {
    name: 'filesystem',
    description: 'Read and edit files in folders you choose.',
    command: ['npx', '-y', '@modelcontextprotocol/server-filesystem@1.0.0'],
    needs: 'folder',
  },
  {
    name: 'sequential-thinking',
    description: 'Plan multi-step work one step at a time.',
    command: ['npx', '-y', '@modelcontextprotocol/server-sequential-thinking@1.0.0'],
  },
  {
    name: 'git',
    description: 'Git history, diffs, and status.',
    command: ['npx', '-y', '@cyanheads/git-mcp-server@2.15.3'],
  },
  {
    name: 'remote',
    description: 'Connect a server that is offered only as a web address.',
    command: ['npx', '-y', 'mcp-remote@0.14.3'],
    needs: 'url',
  },
];
